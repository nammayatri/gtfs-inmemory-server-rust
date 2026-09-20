//! GTFS feeds whose metadata lives in the internal DB (`gtfs_*` tables, see
//! `docs/gtfs-editor.md` and `db/gtfs_editor/`).
//!
//! A DB feed is the preprocessed feed with its **metadata** replaced: stops,
//! route names, colours, polylines and each route's stop order come from the
//! tables an editor changes, while what the editor does not own - which trips a
//! route runs and when its example trip starts - is carried over from the
//! preprocessed patterns (a [`TripOverlay`] per route).
//!
//! The output is exactly what `gtfs_preprocessor.py` would have produced from a
//! GTFS built out of these tables by `generate_trips_from_db.py`, so everything
//! downstream of `fetch_and_process_data` is unchanged:
//!
//! - **stops**: stops a served route row references, plus stations
//!   (`location_type = 1`), with `clusterId` carried in `infoJson`.
//! - **patterns**: one per route that has trips. Stops are the route's served
//!   rows (NEW STOP / INTERMEDIATE STOP) in sequence order. Times follow the
//!   generator: `arrival = start + 135·i`, `departure = arrival + 15`, the last
//!   stop departing when it arrives. The headsign is the fare stage number, or
//!   `{'fareStageNumber': 'N', 'isStageStop': true}` on a NEW STOP.
//! - **routes**: routes with a pattern.

use crate::editor::validation::{
    is_stage_boundary, INTERMEDIATE_STOP, NEW_STOP, SERVED_STOP_TYPES,
};
use crate::models::{GTFSStop, LatLong, NandiPatternDetails, NandiRoutesRes, NandiStop, NandiTrip};
use crate::tools::error::{AppError, AppResult};
use sqlx::postgres::PgPool;
use sqlx::Row;
use std::collections::{BTreeMap, HashMap, HashSet};
use tracing::info;

/// Seconds between consecutive stops in the generator's synthetic schedule.
pub const STOP_INTERVAL_SECONDS: i32 = 135;
/// Dwell at each stop but the last.
pub const DWELL_SECONDS: i32 = 15;

/// What a DB feed borrows from the preprocessed data for one route.
#[derive(Debug, Clone)]
pub struct TripOverlay {
    pub pattern_id: String,
    pub desc: Option<String>,
    /// The route's trips. After a snapshot boot only the example trip is known,
    /// so `trip_count` is authoritative, not `trips.len()`.
    pub trips: Vec<NandiTrip>,
    pub trip_count: i32,
    /// Arrival at the first stop of the example trip, seconds since midnight.
    pub start_seconds: i32,
}

/// One DB feed, ready for the normal build.
#[derive(Debug, Default)]
pub struct DbFeed {
    pub gtfs_id: String,
    /// `gtfs_feed.version`, read before any row.
    pub version: i64,
    pub routes: Vec<NandiRoutesRes>,
    pub patterns: Vec<NandiPatternDetails>,
    pub stops: Vec<GTFSStop>,
    /// route code -> polyline (None clears whatever another source set).
    pub polylines: HashMap<String, Option<String>>,
    /// Merged-away stop code -> the code of the stop that survived the merge,
    /// from [`resolve_merge_chains`]. Empty for a feed that has had no merges.
    pub aliases: HashMap<String, String>,
}

/// A `gtfs_stop` row the editor merged away: deleted, with its provenance
/// naming the stop it was merged into (the merge's step 4, see
/// `docs/gtfs-editor.md` "Merging duplicate stops").
#[derive(Debug, Clone)]
pub struct MergedAwayStop {
    pub stop_id: String,
    /// The feed's public code for it, when the row carries one.
    pub stop_code: Option<String>,
    pub merged_into: String,
}

/// The live stops a merge chain can end at: each live `stop_id` with the public
/// code the feed serves it under, plus every one of those codes, so an alias is
/// never built for a key a live stop already answers to.
#[derive(Debug, Default)]
pub struct LiveStopCodes {
    code_by_id: HashMap<String, String>,
    codes: HashSet<String>,
}

impl LiveStopCodes {
    /// From `(stop_id, public code)` pairs - the loader's non-deleted rows.
    pub fn from_pairs<I: IntoIterator<Item = (String, String)>>(pairs: I) -> Self {
        let code_by_id: HashMap<String, String> = pairs.into_iter().collect();
        let codes = code_by_id.values().cloned().collect();
        Self { code_by_id, codes }
    }

    /// True when a caller holding `key` is holding a live stop, by either
    /// spelling - such a key must never be aliased away.
    fn is_live(&self, key: &str) -> bool {
        self.code_by_id.contains_key(key) || self.codes.contains(key)
    }
}

/// Resolve every merged-away stop to the public code of the live stop at the end
/// of its merge chain: A merged into B and B later into C gives `A -> C`, so a
/// caller holding any id the editor has ever retired lands on what survives
/// today, however many merges ago that was.
///
/// Both spellings of a retired stop are keys - its `stop_id` and its
/// `stop_code` - because a caller holds whichever the feed served it.
///
/// Three chains produce no alias at all, so the ids in them 404 exactly as they
/// do today:
///
/// - a **cycle** (A into B, B back into A - which the editor's own validation
///   refuses, but a hand-written row could not be trusted to),
/// - a **dangling** end: the chain's last target is deleted without a
///   `merged_into` of its own, so nothing survives to answer for it,
/// - a key that is **still live** under either spelling: a live stop always wins
///   over an alias, so a reused id can never be shadowed by a retired one.
pub fn resolve_merge_chains(
    merged: &[MergedAwayStop],
    live: &LiveStopCodes,
) -> HashMap<String, String> {
    let next: HashMap<&str, &str> = merged
        .iter()
        .map(|m| (m.stop_id.as_str(), m.merged_into.as_str()))
        .collect();

    let mut aliases = HashMap::new();
    for m in merged {
        // Walk to the first live stop. `seen` makes a cycle terminate, and a
        // target that is neither live nor itself merged away ends the walk
        // with nothing.
        let mut seen: HashSet<&str> = HashSet::from([m.stop_id.as_str()]);
        let mut target = m.merged_into.as_str();
        let survivor = loop {
            if let Some(code) = live.code_by_id.get(target) {
                break Some(code.clone());
            }
            if !seen.insert(target) {
                break None; // cycle
            }
            match next.get(target) {
                Some(t) => target = t,
                None => break None, // deleted with no merged_into, or no such row
            }
        };
        let Some(survivor) = survivor else { continue };

        for key in [Some(&m.stop_id), m.stop_code.as_ref()]
            .into_iter()
            .flatten()
        {
            if live.is_live(key) || *key == survivor {
                continue;
            }
            aliases.insert(key.clone(), survivor.clone());
        }
    }
    aliases
}

/// Trip overlays for `gtfs_id`, from preprocessed patterns (JSON boot).
/// A route with several patterns keeps its longest, which is the one the
/// route-stop mapping is built from.
pub fn overlays_from_patterns(
    patterns: &[NandiPatternDetails],
    gtfs_id: &str,
) -> HashMap<String, TripOverlay> {
    let mut by_route: HashMap<String, (usize, TripOverlay)> = HashMap::new();
    let mut counts: HashMap<String, i32> = HashMap::new();
    for p in patterns {
        let Some((g, route)) = p.route_id.split_once(':') else {
            continue;
        };
        if g != gtfs_id {
            continue;
        }
        *counts.entry(route.to_string()).or_insert(0) += p.trips.len() as i32;
        let start = p.stops.first().and_then(|s| s.arrival_time).unwrap_or(0);
        let candidate = TripOverlay {
            pattern_id: p.id.clone(),
            desc: p.desc.clone(),
            trips: p.trips.clone(),
            trip_count: 0,
            start_seconds: start,
        };
        match by_route.get(route) {
            Some((len, _)) if *len >= p.stops.len() => {}
            _ => {
                by_route.insert(route.to_string(), (p.stops.len(), candidate));
            }
        }
    }
    by_route
        .into_iter()
        .map(|(route, (_, mut o))| {
            o.trip_count = counts.get(&route).copied().unwrap_or(o.trips.len() as i32);
            (route, o)
        })
        .collect()
}

/// The generator's `stop_headsign`.
pub fn headsign(stage_no: i32, stop_type: &str) -> String {
    if is_stage_boundary(stop_type) {
        format!("{{'fareStageNumber': '{}', 'isStageStop': true}}", stage_no)
    } else {
        stage_no.to_string()
    }
}

pub fn parse_headsign_stage(
    headsign: &str,
    feed_uses_fare_stages: bool,
) -> (Option<i32>, Option<&'static str>) {
    let raw = headsign.trim();
    if raw.is_empty() {
        return (None, None);
    }
    if !raw.starts_with('{') {
        if !feed_uses_fare_stages {
            return (None, None);
        }
        return match parse_stage_no(raw) {
            Some(stage_no) => (Some(stage_no), Some(INTERMEDIATE_STOP)),
            None => (None, None),
        };
    }
    let stage_raw = dict_value(raw, "fareStageNumber");
    let flag_raw = dict_value(raw, "isStageStop");
    if stage_raw.is_none() && flag_raw.is_none() {
        return (None, None);
    }
    let stage_no = stage_raw.and_then(parse_stage_no);
    if stage_raw.is_some() && stage_no.is_none() {
        return (None, None);
    }
    let stop_type = match flag_raw {
        Some(v) if v.eq_ignore_ascii_case("true") => Some(NEW_STOP),
        Some(v) if v.eq_ignore_ascii_case("false") => Some(INTERMEDIATE_STOP),
        _ => None,
    };
    (stage_no, stop_type)
}

pub fn is_fare_stage_dict(headsign: &str) -> bool {
    let raw = headsign.trim();
    raw.starts_with('{')
        && (dict_value(raw, "fareStageNumber").is_some()
            || dict_value(raw, "isStageStop").is_some())
}

fn parse_stage_no(raw: &str) -> Option<i32> {
    let v = raw.trim();
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse::<i32>().ok()
}

fn dict_value<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    for quote in ['\'', '"'] {
        let needle = format!("{quote}{key}{quote}");
        let mut from = 0;
        while let Some(rel) = raw[from..].find(&needle) {
            let after = from + rel + needle.len();
            if let Some(value) = value_after_colon(&raw[after..]) {
                return Some(value);
            }
            from += rel + 1;
        }
    }
    None
}

fn value_after_colon(rest: &str) -> Option<&str> {
    let value = rest.trim_start().strip_prefix(':')?.trim_start();
    let quoted = value.starts_with('\'') || value.starts_with('"');
    let value = value.trim_start_matches(['\'', '"']);
    let end = value.find([',', '}', '\'', '"']).unwrap_or(value.len());
    let out = value[..end].trim();
    if out.is_empty() || (!quoted && (out == "None" || out == "null" || out == "nil")) {
        return None;
    }
    Some(out)
}

/// GTFS route_type -> the mode string GIMS stores (preprocessor's ROUTE_TYPE_MAP
/// followed by cast_vehicle_type).
pub fn route_mode(route_type: i16) -> String {
    let mode = match route_type {
        0 => "TRAM",
        1 => "SUBWAY",
        2 => "RAIL",
        4 => "FERRY",
        5 => "CABLE_TRAM",
        6 => "AERIAL_LIFT",
        7 => "FUNICULAR",
        11 => "TROLLEYBUS",
        12 => "MONORAIL",
        _ => "BUS",
    };
    crate::models::cast_vehicle_type(mode)
}

/// Merges `gtfs_feed` rows (as `live_feeds` fetched them) with the static
/// fallback list into the live decision `live_feeds` returns. Pure and
/// synchronous so the precedence rule - a feed's own row always wins over the
/// fallback list, even a row saying `preprocessed` - can be unit tested
/// without a database. See `live_feeds`'s doc comment for the exact rule.
fn merge_live_feeds(
    rows: &[(String, String, i64)], // (gtfs_id, data_source, version)
    fallback: &[String],
) -> HashMap<String, Option<i64>> {
    let known: HashMap<&str, (&str, i64)> = rows
        .iter()
        .map(|(id, src, v)| (id.as_str(), (src.as_str(), *v)))
        .collect();
    let mut out: HashMap<String, Option<i64>> = HashMap::new();
    for (id, (src, v)) in &known {
        if *src == "db" {
            out.insert((*id).to_string(), Some(*v));
        }
    }
    for id in fallback {
        if !known.contains_key(id.as_str()) {
            out.entry(id.clone()).or_insert(None);
        }
    }
    out
}

/// What the poll loop should do this cycle for one feed. Returned only for a
/// feed that needs something done; a feed that is up to date, or that has
/// never been touched (not live and not loaded), is simply absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedAction {
    /// Not loaded as a DB feed yet, or its live version moved: (re)load it
    /// from the DB. Covers both "just flipped to `data_source = 'db'` for the
    /// first time" and "a committed edit bumped its version" - `reload_db_feed`
    /// handles both the same way.
    LoadOrReload,
    /// Loaded as a DB feed but no longer in `live`: someone flipped it back
    /// to `data_source = 'preprocessed'`. Revert it to preprocessed data.
    Revert,
}

/// Diffs a poll's `live_feeds` result against what GIMS currently has loaded
/// as DB feeds (`GTFSData::db_feed_versions`), and says what to do with each
/// feed that needs anything done. Pure and synchronous - the actual loading/
/// reverting is async I/O the caller (`GTFSService::start_db_version_polling`)
/// does after consulting this.
///
/// A feed absent from both `live` and `loaded_versions` - one that has never
/// been flipped to `db`, e.g. Kolkata, Sambalpur, Bhubaneswar, Bangalore
/// metro today - is absent from the result too: completely untouched.
pub fn plan_feed_actions(
    live: &HashMap<String, Option<i64>>,
    loaded_versions: &HashMap<String, i64>,
) -> Vec<(String, FeedAction)> {
    let mut out = Vec::new();
    for (gtfs_id, latest) in live {
        let up_to_date = matches!(
            (loaded_versions.get(gtfs_id), latest),
            (Some(loaded), Some(v)) if loaded == v
        );
        if !up_to_date {
            out.push((gtfs_id.clone(), FeedAction::LoadOrReload));
        }
    }
    for gtfs_id in loaded_versions.keys() {
        if !live.contains_key(gtfs_id) {
            out.push((gtfs_id.clone(), FeedAction::Revert));
        }
    }
    out
}

struct StopRow {
    stop_id: String,
    stop_code: Option<String>,
    name: String,
    lat: f64,
    lon: f64,
    location_type: i16,
    parent_station: Option<String>,
    platform_code: Option<String>,
    cluster_id: Option<String>,
    description: Option<String>,
}

pub struct GtfsDbSource {
    pool: PgPool,
    /// The static `gtfs_db_feeds` config list. Since `gtfs_feed.data_source`
    /// became the live, per-feed source of truth (docs/gtfs-editor.md "Feed
    /// data source"), this is only a boot-time fallback: it is what
    /// `overlay_db_feeds`/`overlay_db_feeds_on_snapshot` load as DB feeds
    /// before the first poll runs, and what `live_feeds` treats as `db` mode
    /// for a feed that has no `gtfs_feed` row yet. A feed with a row is
    /// governed entirely by that row, even one saying `preprocessed` - the
    /// row always wins over this list.
    feeds: Vec<String>,
}

impl GtfsDbSource {
    pub fn new(pool: PgPool, feeds: Vec<String>) -> Self {
        Self { pool, feeds }
    }

    /// The static fallback list (see the field doc). Boot-time overlay code
    /// still uses this directly; the poll loop uses it only as `live_feeds`'s
    /// `fallback` argument.
    pub fn feeds(&self) -> &[String] {
        &self.feeds
    }

    /// The internal-DB pool, for the cache-state heartbeat and the webhook
    /// dispatcher that ride along with the version poll (`services::webhook`).
    /// They read and write their own tables and never the feed's.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The live, per-feed `data_source` decision the poll loop reconciles
    /// against each cycle: every feed with `data_source = 'db'` right now,
    /// plus every `fallback` feed with no `gtfs_feed` row at all. Returns
    /// `gtfs_id -> Some(version)` for a feed with a row, or `gtfs_id -> None`
    /// for a fallback feed with no row yet (nothing to load until one
    /// exists - same failure mode as before this feature existed).
    ///
    /// One query does both jobs: which feeds are DB-mode, and their current
    /// version, for the union of "has a 'db' row" and "named in `fallback`"
    /// (so a `preprocessed` row for a fallback feed is fetched too, and wins
    /// over the fallback list rather than being invisible to it).
    pub async fn live_feeds(&self, fallback: &[String]) -> AppResult<HashMap<String, Option<i64>>> {
        let rows = sqlx::query(
            "SELECT gtfs_id, data_source, version FROM gtfs_feed \
             WHERE data_source = 'db' OR gtfs_id = ANY($1)",
        )
        .bind(fallback)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Internal(format!("gtfs_feed live_feeds: {}", e)))?;
        let parsed = rows
            .iter()
            .map(|r| {
                Ok((
                    r.try_get::<String, _>("gtfs_id").map_err(db_err)?,
                    r.try_get::<String, _>("data_source").map_err(db_err)?,
                    r.try_get::<i64, _>("version").map_err(db_err)?,
                ))
            })
            .collect::<AppResult<Vec<(String, String, i64)>>>()?;
        Ok(merge_live_feeds(&parsed, fallback))
    }

    /// Build one feed from the tables. The version is read first: an edit that
    /// commits while the rows are being read bumps it past what this returns,
    /// so the next poll reloads again rather than missing the edit.
    pub async fn load_feed(
        &self,
        gtfs_id: &str,
        overlays: &HashMap<String, TripOverlay>,
    ) -> AppResult<DbFeed> {
        let started = std::time::Instant::now();
        let feed = sqlx::query("SELECT version, agency_name FROM gtfs_feed WHERE gtfs_id = $1")
            .bind(gtfs_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?
            .ok_or_else(|| AppError::Internal(format!("gtfs_feed has no row for {}", gtfs_id)))?;
        let version: i64 = feed.try_get("version").map_err(db_err)?;
        let agency_name: Option<String> = feed.try_get("agency_name").map_err(db_err)?;

        let route_rows = sqlx::query(
            "SELECT route_id, short_name, long_name, route_type, color, encoded_polyline
             FROM gtfs_route WHERE gtfs_id = $1 AND NOT deleted",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        let served = sqlx::query(
            "SELECT rs.route_id, rs.sequence, rs.stop_id, rs.stop_type, rs.stage_no
             FROM gtfs_route_stop rs
             JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted
             WHERE rs.gtfs_id = $1 AND rs.stop_type = ANY($2)
             ORDER BY rs.route_id, rs.sequence",
        )
        .bind(gtfs_id)
        .bind(&SERVED_STOP_TYPES[..])
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        let stop_rows = sqlx::query(
            "SELECT stop_id, stop_code, name, lat, lon, location_type, parent_station,
                    platform_code, cluster_id, description
             FROM gtfs_stop WHERE gtfs_id = $1 AND NOT deleted",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        // Merged-away stops, for the alias map. A second, cheap query rather
        // than widening the one above: the stop query above must keep returning
        // live rows only (everything downstream of it builds the served feed),
        // and these rows are read for their provenance alone.
        let alias_started = std::time::Instant::now();
        let merged_rows = sqlx::query(
            "SELECT stop_id, stop_code, provenance->>'merged_into' AS merged_into
             FROM gtfs_stop
             WHERE gtfs_id = $1 AND deleted AND provenance->>'merged_into' IS NOT NULL",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let alias_query_ms = alias_started.elapsed().as_millis() as u64;

        let mut stops: HashMap<String, StopRow> = HashMap::with_capacity(stop_rows.len());
        for r in &stop_rows {
            let s = StopRow {
                stop_id: r.try_get("stop_id").map_err(db_err)?,
                stop_code: r.try_get("stop_code").map_err(db_err)?,
                name: r.try_get("name").map_err(db_err)?,
                lat: r.try_get("lat").map_err(db_err)?,
                lon: r.try_get("lon").map_err(db_err)?,
                location_type: r.try_get("location_type").map_err(db_err)?,
                parent_station: r.try_get("parent_station").map_err(db_err)?,
                platform_code: r.try_get("platform_code").map_err(db_err)?,
                cluster_id: r.try_get("cluster_id").map_err(db_err)?,
                description: r.try_get("description").map_err(db_err)?,
            };
            stops.insert(s.stop_id.clone(), s);
        }

        // route_id -> ordered (stop_id, stop_type, stage_no); BTreeMap keeps the
        // output order independent of hashing.
        let mut rows_by_route: BTreeMap<String, Vec<(String, String, i32)>> = BTreeMap::new();
        for r in &served {
            let route_id: String = r.try_get("route_id").map_err(db_err)?;
            let stop_id: Option<String> = r.try_get("stop_id").map_err(db_err)?;
            let Some(stop_id) = stop_id else { continue };
            rows_by_route.entry(route_id).or_default().push((
                stop_id,
                r.try_get("stop_type").map_err(db_err)?,
                r.try_get("stage_no").map_err(db_err)?,
            ));
        }

        // The alias map: every stop the editor has merged away, pointed at what
        // survives its chain today. Built from the live rows just read, so it
        // can never name a stop this feed does not serve.
        let mut merged = Vec::with_capacity(merged_rows.len());
        for r in &merged_rows {
            let merged_into: String = r.try_get("merged_into").map_err(db_err)?;
            if merged_into.trim().is_empty() {
                continue;
            }
            merged.push(MergedAwayStop {
                stop_id: r.try_get("stop_id").map_err(db_err)?,
                stop_code: r.try_get("stop_code").map_err(db_err)?,
                merged_into,
            });
        }
        let live = LiveStopCodes::from_pairs(stops.values().map(|s| {
            (
                s.stop_id.clone(),
                s.stop_code.clone().unwrap_or_else(|| s.stop_id.clone()),
            )
        }));
        let aliases = resolve_merge_chains(&merged, &live);
        let merged_away = merged.len();

        let prefixed = |id: &str| format!("{}:{}", gtfs_id, id);

        let mut out = DbFeed {
            gtfs_id: gtfs_id.to_string(),
            version,
            aliases,
            ..Default::default()
        };

        // Stops: everything a served row references (whether or not its route
        // runs trips - the generator writes those too), plus stations.
        let mut referenced: HashSet<&str> = HashSet::new();
        for rows in rows_by_route.values() {
            for (stop_id, _, _) in rows {
                referenced.insert(stop_id.as_str());
            }
        }
        let mut stop_ids: Vec<&String> = stops
            .values()
            .filter(|s| s.location_type == 1 || referenced.contains(s.stop_id.as_str()))
            .map(|s| &s.stop_id)
            .collect();
        stop_ids.sort();
        for id in stop_ids {
            let s = &stops[id];
            out.stops.push(GTFSStop {
                id: prefixed(&s.stop_id),
                code: s.stop_code.clone().unwrap_or_else(|| s.stop_id.clone()),
                name: s.name.clone(),
                lat: s.lat,
                lon: s.lon,
                station_id: s.parent_station.as_deref().map(prefixed),
                cluster: None,
                hindi_name: None,
                regional_name: None,
                info_json: s
                    .cluster_id
                    .as_ref()
                    .map(|c| serde_json::json!({ "clusterId": c }).to_string()),
                cluster_id: None,
                location_type: s.location_type.to_string(),
                platform_code: s.platform_code.clone(),
                description: s.description.clone().filter(|d| !d.trim().is_empty()),
            });
        }

        let mut dropped_no_trips = 0usize;
        for r in &route_rows {
            let route_id: String = r.try_get("route_id").map_err(db_err)?;
            let polyline: Option<String> = r.try_get("encoded_polyline").map_err(db_err)?;
            out.polylines.insert(route_id.clone(), polyline);

            let Some(overlay) = overlays.get(&route_id) else {
                dropped_no_trips += 1;
                continue;
            };
            let Some(rows) = rows_by_route.get(&route_id) else {
                continue;
            };
            let last = rows.len().saturating_sub(1);
            let mut pattern_stops = Vec::with_capacity(rows.len());
            let mut codes: HashSet<&str> = HashSet::new();
            for (i, (stop_id, stop_type, stage_no)) in rows.iter().enumerate() {
                let Some(s) = stops.get(stop_id) else {
                    return Err(AppError::Internal(format!(
                        "{} route {} uses stop {} that is missing or deleted",
                        gtfs_id, route_id, stop_id
                    )));
                };
                let arrival = overlay.start_seconds + STOP_INTERVAL_SECONDS * i as i32;
                let departure = if i == last {
                    arrival
                } else {
                    arrival + DWELL_SECONDS
                };
                codes.insert(s.stop_code.as_deref().unwrap_or(&s.stop_id));
                pattern_stops.push(NandiStop {
                    id: prefixed(&s.stop_id),
                    code: s.stop_code.clone().unwrap_or_else(|| s.stop_id.clone()),
                    name: s.name.clone(),
                    lat: s.lat,
                    lon: s.lon,
                    arrival_time: Some(arrival),
                    departure_time: Some(departure),
                    stop_sequence: Some(i as i32 + 1),
                    platform_code: None,
                    headsign: Some(headsign(*stage_no, stop_type)),
                    stage_number: Some(*stage_no),
                    stop_type: Some(stop_type.clone()),
                });
            }
            let (Some(first), Some(end)) = (pattern_stops.first(), pattern_stops.last()) else {
                continue;
            };
            let route_type: i16 = r.try_get("route_type").map_err(db_err)?;
            out.routes.push(NandiRoutesRes {
                id: prefixed(&route_id),
                short_name: r.try_get("short_name").map_err(db_err)?,
                long_name: r.try_get("long_name").map_err(db_err)?,
                mode: route_mode(route_type),
                agency_name: agency_name.clone(),
                color: r.try_get("color").map_err(db_err)?,
                trip_count: Some(overlay.trip_count),
                stop_count: Some(codes.len() as i32),
                start_point: Some(LatLong {
                    lat: first.lat,
                    lon: first.lon,
                }),
                end_point: Some(LatLong {
                    lat: end.lat,
                    lon: end.lon,
                }),
                service_tier_type: None,
                encoded_polyline: None,
            });
            out.patterns.push(NandiPatternDetails {
                id: overlay.pattern_id.clone(),
                desc: overlay.desc.clone(),
                route_id: prefixed(&route_id),
                stops: pattern_stops,
                trips: overlay.trips.clone(),
            });
        }

        info!(
            gtfs_id,
            version,
            routes = out.routes.len(),
            stops = out.stops.len(),
            routes_without_trips = dropped_no_trips,
            merged_away_stops = merged_away,
            stop_aliases = out.aliases.len(),
            alias_query_ms,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Loaded DB feed"
        );
        Ok(out)
    }
}

fn db_err(e: sqlx::Error) -> AppError {
    AppError::Internal(format!("gtfs DB source: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headsign_matches_the_generator() {
        assert_eq!(
            headsign(3, "NEW STOP"),
            "{'fareStageNumber': '3', 'isStageStop': true}"
        );
        assert_eq!(headsign(3, "INTERMEDIATE STOP"), "3");
    }

    #[test]
    fn parse_headsign_stage_inverts_the_generator() {
        for stage_no in [1, 3, 12, 47] {
            assert_eq!(
                parse_headsign_stage(&headsign(stage_no, "NEW STOP"), true),
                (Some(stage_no), Some(NEW_STOP))
            );
            assert_eq!(
                parse_headsign_stage(&headsign(stage_no, "INTERMEDIATE STOP"), true),
                (Some(stage_no), Some(INTERMEDIATE_STOP))
            );
        }
    }

    #[test]
    fn parse_headsign_stage_tolerates_the_dialect() {
        assert_eq!(
            parse_headsign_stage("{'isStageStop': True, 'fareStageNumber': '7'}", true),
            (Some(7), Some(NEW_STOP))
        );
        assert_eq!(
            parse_headsign_stage("{\"fareStageNumber\":\"7\",\"isStageStop\":false}", true),
            (Some(7), Some(INTERMEDIATE_STOP))
        );
    }

    #[test]
    fn a_missing_or_unreadable_flag_is_unknown_not_a_boundary() {
        assert_eq!(
            parse_headsign_stage("{'fareStageNumber': 7}", true),
            (Some(7), None)
        );
        assert_eq!(
            parse_headsign_stage("{'fareStageNumber': '7', 'isStageStop': maybe}", true),
            (Some(7), None)
        );
        assert_eq!(
            parse_headsign_stage("{'isStageStop': true}", true),
            (None, Some(NEW_STOP))
        );
    }

    #[test]
    fn a_value_that_says_nothing_is_not_read_as_a_value() {
        for raw in [
            "{'fareStageNumber': None, 'isStageStop': None}",
            "{\"fareStageNumber\": null, \"isStageStop\": null}",
        ] {
            assert_eq!(parse_headsign_stage(raw, true), (None, None), "{raw}");
        }
        assert_eq!(
            parse_headsign_stage("{'fareStageNumber': 'None', 'isStageStop': true}", true),
            (None, None)
        );
    }

    #[test]
    fn a_stage_number_that_does_not_fit_is_not_published_as_a_stage() {
        for raw in [
            "{'fareStageNumber': '99999999999', 'isStageStop': true}",
            "{'fareStageNumber': '-4', 'isStageStop': true}",
            "{'fareStageNumber': '+7', 'isStageStop': true}",
            "{'fareStageNumber': '3.5', 'isStageStop': true}",
        ] {
            assert_eq!(parse_headsign_stage(raw, true), (None, None), "{raw}");
        }
    }

    #[test]
    fn a_key_named_inside_an_earlier_value_does_not_shadow_the_real_one() {
        assert_eq!(
            parse_headsign_stage(
                "{'note': 'see fareStageNumber below', 'fareStageNumber': '5'}",
                true
            ),
            (Some(5), None)
        );
        assert_eq!(
            parse_headsign_stage(
                "{'stopName': 'fareStageNumber Rd', 'fareStageNumber': '5'}",
                true
            ),
            (Some(5), None)
        );
        assert_eq!(
            parse_headsign_stage(
                "{'stopName': 'fareStageNumber', 'fareStageNumber': '5', 'isStageStop': true}",
                true
            ),
            (Some(5), Some(NEW_STOP))
        );
        assert_eq!(
            parse_headsign_stage(
                "{\"stopName\": \"isStageStop\", \"fareStageNumber\": \"5\", \"isStageStop\": false}",
                true
            ),
            (Some(5), Some(INTERMEDIATE_STOP))
        );
    }

    #[test]
    fn a_bare_number_is_a_stage_only_on_a_feed_that_uses_fare_stages() {
        assert_eq!(
            parse_headsign_stage("3", true),
            (Some(3), Some(INTERMEDIATE_STOP))
        );
        for raw in ["3", "500", "-4", "+7"] {
            assert_eq!(parse_headsign_stage(raw, false), (None, None), "{raw}");
        }
        assert_eq!(parse_headsign_stage("-4", true), (None, None));
        assert_eq!(parse_headsign_stage("+7", true), (None, None));
    }

    #[test]
    fn is_fare_stage_dict_recognises_only_the_generators_shape() {
        assert!(is_fare_stage_dict(
            "{'fareStageNumber': '1', 'isStageStop': true}"
        ));
        assert!(is_fare_stage_dict("{'isStageStop': false}"));
        assert!(!is_fare_stage_dict("3"));
        assert!(!is_fare_stage_dict("Towards Broadway"));
        assert!(!is_fare_stage_dict("{'headsign': 'Broadway'}"));
    }

    #[test]
    fn parse_headsign_stage_reports_nothing_for_a_headsign_that_is_not_a_stage() {
        assert_eq!(
            parse_headsign_stage("Towards Parrys Corner", true),
            (None, None)
        );
        assert_eq!(parse_headsign_stage("", true), (None, None));
        assert_eq!(parse_headsign_stage("  ", true), (None, None));
        assert_eq!(
            parse_headsign_stage("{'headsign': 'Broadway'}", true),
            (None, None)
        );
    }

    /// Parity: a feed reverted from `data_source = db` to preprocessed data
    /// (`FeedAction::Revert`, or a deploy with `gtfs_db_feeds` empty) must
    /// report the same stage. The DB path reads the columns; the preprocessed
    /// path parses the headsign built from those same columns.
    #[test]
    fn the_db_and_preprocessed_paths_report_the_same_stage() {
        for stage_no in [1, 2, 7, 23, 104] {
            for stop_type in SERVED_STOP_TYPES {
                let from_columns = (Some(stage_no), Some(stop_type));
                let from_headsign = parse_headsign_stage(&headsign(stage_no, stop_type), true);
                assert_eq!(
                    from_columns, from_headsign,
                    "stage {stage_no} on {stop_type}"
                );
            }
        }
    }

    #[test]
    fn served_stop_types_are_the_ones_the_headsign_can_carry() {
        assert_eq!(SERVED_STOP_TYPES, [NEW_STOP, INTERMEDIATE_STOP]);
    }

    #[test]
    fn route_type_maps_like_the_preprocessor() {
        assert_eq!(route_mode(3), "BUS");
        assert_eq!(route_mode(2), "METRO");
        assert_eq!(route_mode(99), "BUS");
    }

    #[test]
    fn overlay_keeps_longest_pattern_and_counts_all_trips() {
        let stop = |i: i32| NandiStop {
            id: format!("g:s{}", i),
            code: format!("s{}", i),
            name: "n".into(),
            lat: 0.0,
            lon: 0.0,
            arrival_time: Some(100 + i),
            departure_time: None,
            stop_sequence: Some(i),
            platform_code: None,
            headsign: None,
            stage_number: None,
            stop_type: None,
        };
        let pat = |id: &str, n: i32, trips: usize| NandiPatternDetails {
            id: id.into(),
            desc: None,
            route_id: "g:r1".into(),
            stops: (0..n).map(stop).collect(),
            trips: (0..trips)
                .map(|t| NandiTrip {
                    id: format!("{}-{}", id, t),
                    direction: Some(0),
                })
                .collect(),
        };
        let o = overlays_from_patterns(&[pat("short", 2, 3), pat("long", 5, 1)], "g");
        let r = &o["r1"];
        assert_eq!(r.pattern_id, "long");
        assert_eq!(r.trip_count, 4);
        assert_eq!(r.start_seconds, 100);
    }

    // ---------------------------------------------------------------- merged stop aliases

    fn merged(stop_id: &str, into: &str) -> MergedAwayStop {
        MergedAwayStop {
            stop_id: stop_id.into(),
            stop_code: None,
            merged_into: into.into(),
        }
    }

    /// Live stops whose code is their id, the usual case.
    fn live(ids: &[&str]) -> LiveStopCodes {
        LiveStopCodes::from_pairs(ids.iter().map(|i| (i.to_string(), i.to_string())))
    }

    #[test]
    fn alias_points_a_merged_stop_at_the_survivor() {
        let a = resolve_merge_chains(&[merged("A", "B")], &live(&["B"]));
        assert_eq!(a.get("A"), Some(&"B".to_string()));
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn alias_follows_a_chain_of_merges_to_the_end() {
        // A -> B -> C -> D, only D live: everything answers as D.
        let a = resolve_merge_chains(
            &[merged("A", "B"), merged("B", "C"), merged("C", "D")],
            &live(&["D"]),
        );
        assert_eq!(a.get("A"), Some(&"D".to_string()));
        assert_eq!(a.get("B"), Some(&"D".to_string()));
        assert_eq!(a.get("C"), Some(&"D".to_string()));
    }

    #[test]
    fn alias_chain_order_in_the_rows_does_not_matter() {
        // The same chain, rows in the reverse order: a later merge must not
        // depend on having seen the earlier one first.
        let a = resolve_merge_chains(
            &[merged("C", "D"), merged("B", "C"), merged("A", "B")],
            &live(&["D"]),
        );
        assert_eq!(a.get("A"), Some(&"D".to_string()));
    }

    #[test]
    fn alias_guards_against_a_cycle() {
        // A -> B -> A with neither live: no alias, and no hang.
        let a = resolve_merge_chains(&[merged("A", "B"), merged("B", "A")], &live(&["C"]));
        assert!(a.is_empty(), "{a:?}");
    }

    #[test]
    fn alias_guards_against_a_stop_merged_into_itself() {
        let a = resolve_merge_chains(&[merged("A", "A")], &live(&["B"]));
        assert!(a.is_empty(), "{a:?}");
    }

    #[test]
    fn alias_guards_against_a_cycle_that_a_live_chain_hangs_off() {
        // X -> A -> B -> A: X is as unanswerable as the cycle it runs into.
        let a = resolve_merge_chains(
            &[merged("X", "A"), merged("A", "B"), merged("B", "A")],
            &live(&["L"]),
        );
        assert!(a.is_empty(), "{a:?}");
    }

    #[test]
    fn alias_is_dropped_when_the_target_is_deleted_with_no_merged_into() {
        // B is deleted (absent from the live set) and was not merged into
        // anything: nothing survives to answer for A.
        let a = resolve_merge_chains(&[merged("A", "B")], &live(&["C"]));
        assert!(a.is_empty(), "{a:?}");
    }

    #[test]
    fn alias_never_shadows_a_live_stop() {
        // A stop id that is live again (recreated under the same id) keeps
        // answering for itself, whatever an old deleted row says.
        let a = resolve_merge_chains(&[merged("A", "B")], &live(&["A", "B"]));
        assert!(a.is_empty(), "{a:?}");
    }

    #[test]
    fn alias_covers_both_spellings_of_a_retired_stop() {
        let m = MergedAwayStop {
            stop_id: "old_id".into(),
            stop_code: Some("OLD".into()),
            merged_into: "new_id".into(),
        };
        let live = LiveStopCodes::from_pairs([("new_id".to_string(), "NEW".to_string())]);
        let a = resolve_merge_chains(&[m], &live);
        // Both the id and the code answer, and both give the survivor's code -
        // never its id, which is not what the feed serves.
        assert_eq!(a.get("old_id"), Some(&"NEW".to_string()));
        assert_eq!(a.get("OLD"), Some(&"NEW".to_string()));
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn alias_does_not_shadow_a_live_code_belonging_to_another_stop() {
        // A retired stop whose stop_code is now served by a different live
        // stop: that code must still reach the live stop.
        let m = MergedAwayStop {
            stop_id: "X".into(),
            stop_code: Some("SHARED".into()),
            merged_into: "S".into(),
        };
        let live = LiveStopCodes::from_pairs([
            ("S".to_string(), "S".to_string()),
            ("other".to_string(), "SHARED".to_string()),
        ]);
        let a = resolve_merge_chains(&[m], &live);
        assert_eq!(a.get("X"), Some(&"S".to_string()));
        assert_eq!(a.get("SHARED"), None);
    }

    // ---------------------------------------------------------------- live feed precedence

    #[test]
    fn merge_live_feeds_picks_up_a_db_row_even_outside_the_fallback_list() {
        let rows = vec![("chennai_bus".to_string(), "db".to_string(), 7i64)];
        let out = merge_live_feeds(&rows, &[]);
        assert_eq!(out.get("chennai_bus"), Some(&Some(7)));
    }

    #[test]
    fn merge_live_feeds_a_preprocessed_row_wins_over_the_fallback_list() {
        // A feed named in the static fallback list, but whose row explicitly
        // says 'preprocessed' (someone reverted it): the row wins, so it must
        // NOT appear in the live set at all.
        let rows = vec![("chennai_bus".to_string(), "preprocessed".to_string(), 9i64)];
        let out = merge_live_feeds(&rows, &["chennai_bus".to_string()]);
        assert_eq!(out.get("chennai_bus"), None);
    }

    #[test]
    fn merge_live_feeds_fallback_feed_with_no_row_is_db_mode_with_unknown_version() {
        let out = merge_live_feeds(&[], &["chennai_bus".to_string()]);
        assert_eq!(out.get("chennai_bus"), Some(&None));
    }

    #[test]
    fn merge_live_feeds_untouched_feed_is_absent() {
        let rows = vec![("chennai_bus".to_string(), "db".to_string(), 1i64)];
        let out = merge_live_feeds(&rows, &["chennai_bus".to_string()]);
        assert_eq!(out.get("kolkata_bus"), None);
        assert_eq!(out.len(), 1);
    }

    // ---------------------------------------------------------------- poll loop plan

    #[test]
    fn plan_loads_a_feed_freshly_flipped_to_db() {
        let mut live = HashMap::new();
        live.insert("chennai_bus".to_string(), Some(5));
        let loaded = HashMap::new(); // GIMS has never loaded it as a DB feed
        let plan = plan_feed_actions(&live, &loaded);
        assert_eq!(
            plan,
            vec![("chennai_bus".to_string(), FeedAction::LoadOrReload)]
        );
    }

    #[test]
    fn plan_skips_a_feed_whose_version_has_not_moved() {
        let mut live = HashMap::new();
        live.insert("chennai_bus".to_string(), Some(5));
        let mut loaded = HashMap::new();
        loaded.insert("chennai_bus".to_string(), 5);
        assert!(plan_feed_actions(&live, &loaded).is_empty());
    }

    #[test]
    fn plan_reloads_a_feed_whose_version_moved() {
        let mut live = HashMap::new();
        live.insert("chennai_bus".to_string(), Some(6));
        let mut loaded = HashMap::new();
        loaded.insert("chennai_bus".to_string(), 5);
        assert_eq!(
            plan_feed_actions(&live, &loaded),
            vec![("chennai_bus".to_string(), FeedAction::LoadOrReload)]
        );
    }

    #[test]
    fn plan_reverts_a_feed_flipped_back_to_static() {
        // Was loaded as a DB feed; the poll's live query no longer has it
        // (data_source flipped back to 'preprocessed').
        let live = HashMap::new();
        let mut loaded = HashMap::new();
        loaded.insert("chennai_bus".to_string(), 5);
        assert_eq!(
            plan_feed_actions(&live, &loaded),
            vec![("chennai_bus".to_string(), FeedAction::Revert)]
        );
    }

    #[test]
    fn plan_leaves_a_feed_with_no_row_and_never_loaded_untouched() {
        let live = HashMap::new();
        let loaded = HashMap::new();
        assert!(plan_feed_actions(&live, &loaded).is_empty());
    }

    #[test]
    fn plan_round_trips_static_to_db_and_back() {
        // db -> loaded -> flipped back to static -> flipped to db again.
        let mut loaded = HashMap::new();

        // 1) first seen live: load it.
        let mut live = HashMap::new();
        live.insert("chennai_bus".to_string(), Some(1));
        assert_eq!(
            plan_feed_actions(&live, &loaded),
            vec![("chennai_bus".to_string(), FeedAction::LoadOrReload)]
        );
        loaded.insert("chennai_bus".to_string(), 1); // simulate the load succeeding

        // 2) flipped back to static: revert.
        live.clear();
        assert_eq!(
            plan_feed_actions(&live, &loaded),
            vec![("chennai_bus".to_string(), FeedAction::Revert)]
        );
        loaded.remove("chennai_bus"); // simulate the revert succeeding

        // 3) flipped to db again: load it again.
        live.insert("chennai_bus".to_string(), Some(2));
        assert_eq!(
            plan_feed_actions(&live, &loaded),
            vec![("chennai_bus".to_string(), FeedAction::LoadOrReload)]
        );
    }
}
