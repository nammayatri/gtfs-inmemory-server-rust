//! Trips, timing profiles, patterns and service calendars as editor data
//! (docs/gtfs-editor.md section 16): their reads, the shape checks run when a
//! change is added, and what applying each change does inside
//! [`super::service::evaluate`].
//!
//! A route's trips are replaced as one list (`route_trips/replace`), as its stop
//! list is, and a conflict is a moved list hash. A trip stores a pattern, a
//! timing profile or the pattern's default timing, a service and a reference
//! time, nothing per stop: its stop times are `services::gtfs_timing`'s.
//!
//! When a stop list changes under a pattern that has stored profiles, every one
//! of them is carried over in the same change ([`carry_over_profiles`]), so no
//! trip runs to offsets that no longer line up with its stops.

use super::crypto::sha256_hex;
use super::error::{EditorError, EditorResult};
use super::service::{fail, field, json_col, load_patterns_rows, ApplyError, FIRST_PATTERN};
use super::validation::{check_entity_id, Finding, Level, RouteRow};
use crate::services::gtfs_timing::{self, Offsets, MAX_TIME_S};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, Row};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Longest trip id; GTFS feeds carry ids like `22B/1-1-OR-PKPT-V7_trip_1`.
pub const TRIP_ID_MAX_CHARS: usize = 128;
/// Longest pattern name or profile / service label.
pub const LABEL_MAX_CHARS: usize = 120;
/// Where a trip came from. A trip already on a route keeps its own.
pub const TRIP_SOURCES: [&str; 3] = ["import", "mtc", "editor"];
/// What a change may say a profile came from; `interpolated` is written only by
/// a carry-over (16.4).
pub const PROFILE_SOURCES: [&str; 3] = ["import", "mtc_running_time", "manual"];
pub const DAYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// The hash of nothing: no trips, no profile, no rows (section 5's constant).
pub fn empty_hash() -> String {
    sha256_hex(b"[]")
}

// ---------------------------------------------------------------- stored rows

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredFrequency {
    pub start_s: i32,
    pub end_s: i32,
    pub headway_s: i32,
    /// None: the feed left it blank (0023), which reads as 0. A stored 0 hashes
    /// exactly as it did before the column could be blank.
    pub exact_times: Option<i16>,
}

/// A `gtfs_trip` row with its frequency windows, in the field order the trips
/// hash is taken in.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredTrip {
    pub trip_id: String,
    pub pattern_key: i16,
    pub profile_key: Option<i32>,
    pub service_id: String,
    pub direction_id: Option<i16>,
    pub ref_s: i32,
    pub headsign: Option<String>,
    pub short_name: Option<String>,
    pub block_id: Option<String>,
    pub shape_id: Option<String>,
    pub wheelchair_accessible: Option<i16>,
    pub bikes_allowed: Option<i16>,
    pub sort_key: i32,
    pub source: String,
    pub source_ref: Option<Value>,
    pub frequencies: Vec<StoredFrequency>,
    /// GTFS `cars_allowed` (0023): left out of the hash when unset, so the
    /// `base_trips_hash` of every draft made before it existed stays valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cars_allowed: Option<i16>,
}

/// Fingerprint of a route's whole trip list, in `sort_key` order; a
/// `route_trips` replace carries the one it was based on (`base_trips_hash`).
pub fn trips_hash(trips: &[StoredTrip]) -> String {
    sha256_hex(
        serde_json::to_string(trips)
            .expect("trips serialize")
            .as_bytes(),
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredProfile {
    pub pattern_key: i16,
    pub profile_key: i32,
    pub offsets: Offsets,
    pub label: Option<String>,
    pub source: String,
    pub row_version: i32,
}

/// Fingerprint of one profile (its offsets and label); a `timing_profile`
/// replace carries the one it was based on. No profile hashes as nothing.
pub fn profile_hash(p: Option<&StoredProfile>) -> String {
    match p {
        None => empty_hash(),
        Some(p) => sha256_hex(
            json!({
                "arrival_s": p.offsets.arrival,
                "departure_s": p.offsets.departure,
                "label": p.label,
            })
            .to_string()
            .as_bytes(),
        ),
    }
}

fn profile_json(p: &StoredProfile) -> Value {
    json!({
        "pattern_key": p.pattern_key,
        "profile_key": p.profile_key,
        "label": p.label,
        "source": p.source,
        "arrival_s": p.offsets.arrival,
        "departure_s": p.offsets.departure,
        "row_version": p.row_version,
        "hash": profile_hash(Some(p)),
    })
}

// ---------------------------------------------------------------- reads

pub async fn load_route_trips(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
) -> EditorResult<Vec<StoredTrip>> {
    Ok(load_routes_trips(conn, g, &[route_id.to_string()])
        .await?
        .remove(route_id)
        .unwrap_or_default())
}

/// [`load_route_trips`] for many routes in two queries. Routes without trips
/// are absent.
pub async fn load_routes_trips(
    conn: &mut PgConnection,
    g: &str,
    route_ids: &[String],
) -> EditorResult<HashMap<String, Vec<StoredTrip>>> {
    let mut frequencies: HashMap<String, Vec<StoredFrequency>> = HashMap::new();
    for r in sqlx::query(
        "SELECT f.trip_id, f.start_s, f.end_s, f.headway_s, f.exact_times FROM gtfs_frequency f \
         JOIN gtfs_trip t ON t.gtfs_id = f.gtfs_id AND t.trip_id = f.trip_id \
         WHERE t.gtfs_id = $1 AND t.route_id = ANY($2) ORDER BY f.trip_id, f.start_s",
    )
    .bind(g)
    .bind(route_ids)
    .fetch_all(&mut *conn)
    .await?
    {
        frequencies
            .entry(r.try_get("trip_id")?)
            .or_default()
            .push(StoredFrequency {
                start_s: r.try_get("start_s")?,
                end_s: r.try_get("end_s")?,
                headway_s: r.try_get("headway_s")?,
                exact_times: r.try_get("exact_times")?,
            });
    }
    let rows = sqlx::query(
        "SELECT route_id, trip_id, pattern_key, profile_key, service_id, direction_id, ref_s, headsign, \
                short_name, block_id, shape_id, wheelchair_accessible, bikes_allowed, cars_allowed, \
                sort_key, source, \
                source_ref::text AS source_ref \
         FROM gtfs_trip WHERE gtfs_id = $1 AND route_id = ANY($2) ORDER BY route_id, sort_key, trip_id",
    )
    .bind(g)
    .bind(route_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: HashMap<String, Vec<StoredTrip>> = HashMap::new();
    for r in &rows {
        let trip_id: String = r.try_get("trip_id")?;
        let source_ref = json_col(r, "source_ref")?;
        out.entry(r.try_get("route_id")?)
            .or_default()
            .push(StoredTrip {
                frequencies: frequencies.remove(&trip_id).unwrap_or_default(),
                trip_id,
                pattern_key: r.try_get("pattern_key")?,
                profile_key: r.try_get("profile_key")?,
                service_id: r.try_get("service_id")?,
                direction_id: r.try_get("direction_id")?,
                ref_s: r.try_get("ref_s")?,
                headsign: r.try_get("headsign")?,
                short_name: r.try_get("short_name")?,
                block_id: r.try_get("block_id")?,
                shape_id: r.try_get("shape_id")?,
                wheelchair_accessible: r.try_get("wheelchair_accessible")?,
                bikes_allowed: r.try_get("bikes_allowed")?,
                cars_allowed: r.try_get("cars_allowed")?,
                sort_key: r.try_get("sort_key")?,
                source: r.try_get("source")?,
                source_ref: (!source_ref.is_null()).then_some(source_ref),
            });
    }
    Ok(out)
}

/// The profiles of a route, or of one of its patterns, in key order.
pub async fn load_profiles(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    pattern_key: Option<i16>,
) -> EditorResult<Vec<StoredProfile>> {
    sqlx::query(
        "SELECT pattern_key, profile_key, arrival_s, departure_s, label, source, row_version \
         FROM gtfs_timing_profile \
         WHERE gtfs_id = $1 AND route_id = $2 AND ($3::int2 IS NULL OR pattern_key = $3) \
         ORDER BY pattern_key, profile_key",
    )
    .bind(g)
    .bind(route_id)
    .bind(pattern_key)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<StoredProfile, sqlx::Error> {
        Ok(StoredProfile {
            pattern_key: r.try_get("pattern_key")?,
            profile_key: r.try_get("profile_key")?,
            offsets: Offsets {
                arrival: r.try_get("arrival_s")?,
                departure: r.try_get("departure_s")?,
            },
            label: r.try_get("label")?,
            source: r.try_get("source")?,
            row_version: r.try_get("row_version")?,
        })
    })
    .collect::<Result<Vec<_>, _>>()
    .map_err(Into::into)
}

#[derive(Debug, Clone)]
pub struct PatternRow {
    pub pattern_key: i16,
    pub name: Option<String>,
    pub direction_id: Option<i16>,
    pub row_version: i32,
}

pub async fn load_patterns(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
) -> EditorResult<Vec<PatternRow>> {
    sqlx::query(
        "SELECT pattern_key, name, direction_id, row_version FROM gtfs_pattern \
         WHERE gtfs_id = $1 AND route_id = $2 ORDER BY pattern_key",
    )
    .bind(g)
    .bind(route_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<PatternRow, sqlx::Error> {
        Ok(PatternRow {
            pattern_key: r.try_get("pattern_key")?,
            name: r.try_get("name")?,
            direction_id: r.try_get("direction_id")?,
            row_version: r.try_get("row_version")?,
        })
    })
    .collect::<Result<Vec<_>, _>>()
    .map_err(Into::into)
}

/// The stop ids a pattern serves, in order: rows that are not ROUTE
/// CORRECTION, JUMP STOP or HIDDEN STOP.
pub fn served_ids(rows: &[RouteRow]) -> Vec<String> {
    rows.iter()
        .filter(|r| r.is_served())
        .filter_map(|r| r.stop_id.clone())
        .collect()
}

/// What a route detail says about its timetable (section 16): its stop orders
/// with their public ids and hashes, its profiles, and its trips' count and hash.
pub async fn route_timetable(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
) -> EditorResult<Value> {
    let patterns = load_patterns(conn, g, route_id).await?;
    let keys: Vec<(String, i16)> = patterns
        .iter()
        .map(|p| (route_id.to_string(), p.pattern_key))
        .collect();
    let mut rows = load_patterns_rows(conn, g, &keys).await?;
    let profiles = load_profiles(conn, g, route_id, None).await?;
    let trips = load_route_trips(conn, g, route_id).await?;
    let mut trip_counts: HashMap<i16, usize> = HashMap::new();
    for t in &trips {
        *trip_counts.entry(t.pattern_key).or_default() += 1;
    }
    let patterns: Vec<Value> = patterns
        .iter()
        .map(|p| {
            let rows = rows
                .remove(&(route_id.to_string(), p.pattern_key))
                .unwrap_or_default();
            let served = served_ids(&rows);
            json!({
                "pattern_key": p.pattern_key,
                "pattern_id": gtfs_timing::pattern_id(g, route_id, &served),
                "name": p.name,
                "direction_id": p.direction_id,
                "row_version": p.row_version,
                "stop_count": served.len(),
                "rows_hash": super::service::rows_hash(&rows),
                "trip_count": trip_counts.get(&p.pattern_key).copied().unwrap_or(0),
            })
        })
        .collect();
    Ok(json!({
        "patterns": patterns,
        "profiles": profiles.iter().map(profile_json).collect::<Vec<_>>(),
        "trip_count": trips.len(),
        "trips_hash": trips_hash(&trips),
    }))
}

/// First arrival offset of each profile, by `(pattern, profile)`: what turns a
/// trip's reference time into the start time a person reads.
pub fn first_arrivals(profiles: &[StoredProfile]) -> HashMap<(i16, i32), i32> {
    profiles
        .iter()
        .map(|p| {
            (
                (p.pattern_key, p.profile_key),
                p.offsets.arrival.first().copied().unwrap_or(0),
            )
        })
        .collect()
}

/// A trip in the shape a `route_trips` change sends it, plus what the change
/// does not set (`source`, `sort_key`, `ref_s`) - so a list read here can be
/// sent back as it is.
pub fn trip_json(t: &StoredTrip, first_arrival: i32) -> Value {
    json!({
        "trip_id": t.trip_id,
        "pattern_key": t.pattern_key,
        "profile_key": t.profile_key,
        "service_id": t.service_id,
        "direction_id": t.direction_id,
        "start_time": gtfs_timing::format_time(t.ref_s + first_arrival),
        "headsign": t.headsign,
        "short_name": t.short_name,
        "block_id": t.block_id,
        "shape_id": t.shape_id,
        "wheelchair_accessible": t.wheelchair_accessible,
        "bikes_allowed": t.bikes_allowed,
        "cars_allowed": t.cars_allowed,
        "frequencies": t.frequencies.iter().map(|f| json!({
            "start_time": gtfs_timing::format_time(f.start_s),
            "end_time": gtfs_timing::format_time(f.end_s),
            "headway_s": f.headway_s,
            "exact_times": f.exact_times,
        })).collect::<Vec<_>>(),
        "source_ref": t.source_ref,
        "source": t.source,
        "sort_key": t.sort_key,
        "ref_s": t.ref_s,
    })
}

/// A route's trips in read shape.
pub async fn route_trips_json(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
) -> EditorResult<Vec<Value>> {
    let trips = load_route_trips(conn, g, route_id).await?;
    let firsts = first_arrivals(&load_profiles(conn, g, route_id, None).await?);
    Ok(trips
        .iter()
        .map(|t| {
            let first = t
                .profile_key
                .and_then(|p| firsts.get(&(t.pattern_key, p)).copied())
                .unwrap_or(0);
            trip_json(t, first)
        })
        .collect())
}

/// `GET /feeds/{g}/routes/{route_id}/trips`: `{route_id, trips_hash,
/// trip_count, items}`.
pub async fn route_trips_detail(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
) -> EditorResult<Value> {
    let exists = sqlx::query("SELECT 1 FROM gtfs_route WHERE gtfs_id = $1 AND route_id = $2")
        .bind(g)
        .bind(route_id)
        .fetch_optional(&mut *conn)
        .await?
        .is_some();
    if !exists {
        return Err(EditorError::not_found(
            "route_not_found",
            format!("no route {route_id}"),
        ));
    }
    let items = route_trips_json(conn, g, route_id).await?;
    let hash = trips_hash(&load_route_trips(conn, g, route_id).await?);
    Ok(json!({
        "route_id": route_id, "trips_hash": hash, "trip_count": items.len(), "items": items,
    }))
}

/// A pattern in read shape, as a `pattern` change snapshots it.
pub async fn pattern_json(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    pattern_key: i16,
) -> EditorResult<Option<(Value, i32)>> {
    let Some(p) = load_patterns(conn, g, route_id)
        .await?
        .into_iter()
        .find(|p| p.pattern_key == pattern_key)
    else {
        return Ok(None);
    };
    let trips: i64 = sqlx::query(
        "SELECT count(*) AS n FROM gtfs_trip WHERE gtfs_id = $1 AND route_id = $2 AND pattern_key = $3",
    )
    .bind(g)
    .bind(route_id)
    .bind(pattern_key)
    .fetch_one(&mut *conn)
    .await?
    .try_get("n")?;
    Ok(Some((
        json!({
            "pattern_key": p.pattern_key, "name": p.name, "direction_id": p.direction_id,
            "row_version": p.row_version, "trip_count": trips,
        }),
        p.row_version,
    )))
}

/// One profile in read shape, with its row version.
pub async fn profile_read(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    pattern_key: i16,
    profile_key: i32,
) -> EditorResult<Option<(Value, i32)>> {
    Ok(load_profiles(conn, g, route_id, Some(pattern_key))
        .await?
        .into_iter()
        .find(|p| p.profile_key == profile_key)
        .map(|p| (profile_json(&p), p.row_version)))
}

/// The next profile key of a pattern: past every live one and every one the
/// draft's earlier changes name.
pub async fn next_profile_key(
    conn: &mut PgConnection,
    change_set_id: uuid::Uuid,
    g: &str,
    route_id: &str,
    pattern_key: i16,
) -> EditorResult<i32> {
    let n: i32 = sqlx::query(
        "SELECT greatest( \
            (SELECT coalesce(max(profile_key), 0) FROM gtfs_timing_profile \
              WHERE gtfs_id = $1 AND route_id = $2 AND pattern_key = $3), \
            (SELECT coalesce(max((after->>'profile_key')::int), 0) FROM gtfs_change \
              WHERE change_set_id = $4 AND entity = 'timing_profile' AND entity_key = $2 \
                AND (after->>'pattern_key')::int = $3 AND after->>'profile_key' ~ '^[0-9]+$')) AS n",
    )
    .bind(g)
    .bind(route_id)
    .bind(pattern_key)
    .bind(change_set_id)
    .fetch_one(&mut *conn)
    .await?
    .try_get("n")?;
    Ok(n + 1)
}

/// `n` trip ids for new trips on `route_id`: `{route_id}-ed-{8 hex}`, unused in
/// the feed.
pub async fn mint_trip_ids(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    n: usize,
) -> EditorResult<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(n);
    let mut seen: HashSet<String> = HashSet::new();
    for _ in 0..8 {
        let need = n - out.len();
        if need == 0 {
            break;
        }
        let candidates: Vec<String> = (0..need)
            .map(|_| {
                format!(
                    "{route_id}-ed-{}",
                    hex::encode(super::crypto::random_bytes(4))
                )
            })
            .filter(|c| seen.insert(c.clone()))
            .collect();
        let used: HashSet<String> =
            sqlx::query("SELECT trip_id FROM gtfs_trip WHERE gtfs_id = $1 AND trip_id = ANY($2)")
                .bind(g)
                .bind(&candidates)
                .fetch_all(&mut *conn)
                .await?
                .iter()
                .map(|r| r.try_get("trip_id"))
                .collect::<Result<_, _>>()?;
        out.extend(candidates.into_iter().filter(|c| !used.contains(c)));
    }
    if out.len() < n {
        return Err(EditorError::internal("could not mint unused trip ids"));
    }
    Ok(out)
}

// ---------------------------------------------------------------- services

fn date_text(d: Option<NaiveDate>) -> Value {
    d.map(|d| json!(d.format("%Y-%m-%d").to_string()))
        .unwrap_or(Value::Null)
}

/// An ISO date, `YYYY-MM-DD`.
pub fn parse_date(text: &str) -> Option<NaiveDate> {
    let t = text.trim();
    (t.len() == 10)
        .then(|| NaiveDate::parse_from_str(t, "%Y-%m-%d").ok())
        .flatten()
}

const SERVICE_COLS: &str = "s.service_id, s.monday, s.tuesday, s.wednesday, s.thursday, s.friday, \
     s.saturday, s.sunday, s.start_date, s.end_date, s.label, s.row_version, \
     (SELECT count(*) FROM gtfs_trip t WHERE t.gtfs_id = s.gtfs_id AND t.service_id = s.service_id) \
        AS trip_count";

async fn services_json(
    conn: &mut PgConnection,
    g: &str,
    only: Option<&str>,
) -> EditorResult<Vec<Value>> {
    let mut dates: HashMap<String, Vec<Value>> = HashMap::new();
    for r in sqlx::query(
        "SELECT service_id, service_date, exception_type FROM gtfs_service_date \
         WHERE gtfs_id = $1 AND ($2::text IS NULL OR service_id = $2) ORDER BY service_id, service_date",
    )
    .bind(g)
    .bind(only)
    .fetch_all(&mut *conn)
    .await?
    {
        dates.entry(r.try_get("service_id")?).or_default().push(json!({
            "date": date_text(r.try_get("service_date")?),
            "exception_type": r.try_get::<i16, _>("exception_type")?,
        }));
    }
    sqlx::query(&format!(
        "SELECT {SERVICE_COLS} FROM gtfs_service s \
         WHERE s.gtfs_id = $1 AND ($2::text IS NULL OR s.service_id = $2) ORDER BY s.service_id"
    ))
    .bind(g)
    .bind(only)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        let id: String = r.try_get("service_id")?;
        let mut days = Map::new();
        for d in DAYS {
            days.insert(d.to_string(), json!(r.try_get::<bool, _>(d)?));
        }
        Ok(json!({
            "service_id": id,
            "days": days,
            "start_date": date_text(r.try_get("start_date")?),
            "end_date": date_text(r.try_get("end_date")?),
            "label": r.try_get::<Option<String>, _>("label")?,
            "dates": dates.remove(&id).unwrap_or_default(),
            "row_version": r.try_get::<i32, _>("row_version")?,
            "trip_count": r.try_get::<i64, _>("trip_count")?,
        }))
    })
    .collect::<Result<Vec<_>, _>>()
    .map_err(Into::into)
}

/// One service in read shape: `{service_id, days, start_date, end_date, label,
/// dates, row_version, trip_count}`.
pub async fn service_json(
    conn: &mut PgConnection,
    g: &str,
    service_id: &str,
) -> EditorResult<Option<Value>> {
    Ok(services_json(conn, g, Some(service_id)).await?.pop())
}

/// `GET /feeds/{g}/services`.
pub async fn list_services(conn: &mut PgConnection, g: &str) -> EditorResult<Value> {
    Ok(json!({"items": services_json(conn, g, None).await?, "next_cursor": null}))
}

// ---------------------------------------------------------------- shape checks

fn invalid(key: &str, message: impl Into<String>) -> Finding {
    Finding::error("invalid_payload", key, message)
}

fn object<'a>(after: &'a Value, what: &str) -> Result<&'a Map<String, Value>, Finding> {
    after
        .as_object()
        .ok_or_else(|| invalid(what, format!("{what}: `after` must be an object")))
}

fn only(m: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), Finding> {
    match m.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(invalid(
            k,
            format!(
                "{what}: field {k:?} cannot be set (allowed: {})",
                allowed.join(", ")
            ),
        )),
        None => Ok(()),
    }
}

/// A required pattern key: a whole number from 1.
pub fn pattern_key_of(m: &Map<String, Value>, what: &str) -> Result<i16, Finding> {
    m.get("pattern_key")
        .and_then(Value::as_i64)
        .filter(|k| (1..=i16::MAX as i64).contains(k))
        .map(|k| k as i16)
        .ok_or_else(|| {
            invalid(
                "pattern_key",
                format!("{what}: pattern_key is a whole number from 1"),
            )
        })
}

/// An optional profile key: absent or null, or a whole number from 1.
pub fn profile_key_of(m: &Map<String, Value>, what: &str) -> Result<Option<i32>, Finding> {
    match m.get("profile_key") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .filter(|k| (1..=i32::MAX as i64).contains(k))
            .map(|k| Some(k as i32))
            .ok_or_else(|| {
                invalid(
                    "profile_key",
                    format!("{what}: profile_key is a whole number from 1"),
                )
            }),
    }
}

fn label_ok(m: &Map<String, Value>, key: &str, what: &str) -> Result<(), Finding> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(s)) if s.trim().chars().count() <= LABEL_MAX_CHARS => Ok(()),
        Some(Value::String(_)) => Err(invalid(
            key,
            format!("{what}: {key} is longer than {LABEL_MAX_CHARS} characters"),
        )),
        Some(_) => Err(invalid(key, format!("{what}: {key} must be text or null"))),
    }
}

fn small_int_in(
    v: Option<&Value>,
    key: &str,
    range: std::ops::RangeInclusive<i64>,
    what: &str,
) -> Result<(), Finding> {
    match v {
        None | Some(Value::Null) => Ok(()),
        Some(v) if v.as_i64().is_some_and(|n| range.contains(&n)) => Ok(()),
        Some(_) => Err(invalid(
            key,
            format!(
                "{what}: {key} is one of {}",
                range.map(|n| n.to_string()).collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}

/// `pattern/update` `{pattern_key, name?, direction_id?}`, `pattern/delete`
/// `{pattern_key}`.
pub fn check_pattern_payload(op: &str, after: &Value) -> Result<(), Finding> {
    let what = format!("pattern/{op}");
    let what = what.as_str();
    let m = object(after, what)?;
    match op {
        "update" => {
            only(m, &["pattern_key", "name", "direction_id"], what)?;
            pattern_key_of(m, what)?;
            if !m.contains_key("name") && !m.contains_key("direction_id") {
                return Err(invalid("", format!("{what}: nothing to change")));
            }
            label_ok(m, "name", what)?;
            small_int_in(m.get("direction_id"), "direction_id", 0..=1, what)
        }
        _ => {
            only(m, &["pattern_key"], what)?;
            pattern_key_of(m, what).map(|_| ())
        }
    }
}

fn offsets_of(m: &Map<String, Value>, key: &str, what: &str) -> Result<Vec<i32>, Finding> {
    let bad = || {
        invalid(
            key,
            format!("{what}: {key} is a list of whole seconds within ±47:59:59"),
        )
    };
    m.get(key)
        .and_then(Value::as_array)
        .ok_or_else(bad)?
        .iter()
        .map(|v| {
            v.as_i64()
                .filter(|n| n.abs() <= MAX_TIME_S as i64)
                .map(|n| n as i32)
                .ok_or_else(bad)
        })
        .collect()
}

/// The offsets a `timing_profile/replace` sends, once its shape is checked.
pub fn profile_offsets(after: &Value) -> Option<Offsets> {
    let m = after.as_object()?;
    Some(Offsets {
        arrival: offsets_of(m, "arrival_s", "").ok()?,
        departure: offsets_of(m, "departure_s", "").ok()?,
    })
}

/// `timing_profile/replace` `{pattern_key, profile_key?, arrival_s, departure_s,
/// label?, base_hash, source?}` and `timing_profile/delete` `{pattern_key,
/// profile_key}`. The number of offsets against the pattern's stops is checked
/// when the change applies; that they never go backwards, here.
pub fn check_timing_profile_payload(op: &str, after: &Value) -> Result<(), Finding> {
    let what = format!("timing_profile/{op}");
    let what = what.as_str();
    let m = object(after, what)?;
    if op == "delete" {
        only(m, &["pattern_key", "profile_key"], what)?;
        pattern_key_of(m, what)?;
        return match profile_key_of(m, what)? {
            Some(_) => Ok(()),
            None => Err(invalid(
                "profile_key",
                format!("{what}: profile_key is required"),
            )),
        };
    }
    only(
        m,
        &[
            "pattern_key",
            "profile_key",
            "arrival_s",
            "departure_s",
            "label",
            "base_hash",
            "source",
        ],
        what,
    )?;
    pattern_key_of(m, what)?;
    let key = profile_key_of(m, what)?;
    match m.get("base_hash") {
        Some(Value::String(h)) if !h.trim().is_empty() => {}
        None | Some(Value::Null) if key.is_none() => {}
        _ => {
            return Err(invalid(
                "base_hash",
                format!("{what}: base_hash is required, the profile's `hash` as read (a new profile's is that of nothing)"),
            ))
        }
    }
    let offsets = Offsets {
        arrival: offsets_of(m, "arrival_s", what)?,
        departure: offsets_of(m, "departure_s", what)?,
    };
    if offsets.arrival.len() != offsets.departure.len() {
        return Err(Finding::error(
            "profile_length_mismatch",
            "",
            format!(
                "{what}: {} arrivals and {} departures; a profile has one of each per stop",
                offsets.arrival.len(),
                offsets.departure.len()
            ),
        ));
    }
    if offsets.len() < 2 {
        return Err(Finding::error(
            "profile_length_mismatch",
            "",
            format!("{what}: a profile times at least two stops"),
        ));
    }
    if let Some((i, why)) = gtfs_timing::goes_backwards(&offsets) {
        return Err(Finding::error(
            "timing_goes_backwards",
            format!("{i}"),
            format!("{what}: stop {} {why}", i + 1),
        ));
    }
    label_ok(m, "label", what)?;
    match m.get("source") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(s)) if PROFILE_SOURCES.contains(&s.as_str()) => Ok(()),
        Some(_) => Err(invalid(
            "source",
            format!("{what}: source is {}", PROFILE_SOURCES.join(", ")),
        )),
    }
}

/// A trip as a `route_trips/replace` change sends it (16.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TripSpec {
    #[serde(default)]
    pub trip_id: Option<String>,
    pub pattern_key: i16,
    #[serde(default)]
    pub profile_key: Option<i32>,
    pub service_id: String,
    #[serde(default)]
    pub direction_id: Option<i16>,
    pub start_time: String,
    #[serde(default)]
    pub headsign: Option<String>,
    #[serde(default)]
    pub short_name: Option<String>,
    #[serde(default)]
    pub block_id: Option<String>,
    /// GTFS `shape_id`, carried as the feed has it (no shapes table yet).
    #[serde(default)]
    pub shape_id: Option<String>,
    #[serde(default)]
    pub wheelchair_accessible: Option<i16>,
    #[serde(default)]
    pub bikes_allowed: Option<i16>,
    #[serde(default)]
    pub cars_allowed: Option<i16>,
    #[serde(default)]
    pub frequencies: Option<Vec<FrequencySpec>>,
    #[serde(default)]
    pub source_ref: Option<Value>,
    /// A new trip's origin (default `editor`); a trip already on the route keeps
    /// its own.
    #[serde(default)]
    pub source: Option<String>,
    /// Read-only: what the trips read carries beside the change's own fields,
    /// accepted so a list read from the API can be sent back as it is.
    #[serde(default)]
    pub sort_key: Option<Value>,
    #[serde(default)]
    pub ref_s: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrequencySpec {
    pub start_time: String,
    pub end_time: String,
    pub headway_s: i32,
    #[serde(default)]
    pub exact_times: Option<i16>,
}

impl TripSpec {
    pub fn id(&self) -> Option<&str> {
        self.trip_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

/// A trip id: 1-128 characters, no `:` (GIMS splits ids on it), no control
/// characters, nothing around it.
pub fn check_trip_id(id: &str) -> Result<(), Finding> {
    let ok = !id.is_empty()
        && id.trim() == id
        && id.chars().count() <= TRIP_ID_MAX_CHARS
        && !id.contains(':')
        && !id.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(invalid(
            id,
            format!("trip_id {id:?} must be 1-{TRIP_ID_MAX_CHARS} characters without ':' or spaces around it"),
        ))
    }
}

/// The trips of a `route_trips/replace`, or why they cannot be read.
pub fn trip_specs(after: &Value) -> Result<Vec<TripSpec>, String> {
    serde_json::from_value(after.get("trips").cloned().unwrap_or(Value::Null))
        .map_err(|e| format!("trips are not valid: {e}"))
}

/// `route_trips/replace` `{trips: [...], base_trips_hash}`: every trip's own
/// shape. What it names (pattern, profile, service, id taken) is checked when
/// the change applies.
pub fn check_route_trips_payload(after: &Value) -> Result<(), Finding> {
    let what = "route_trips/replace";
    let m = object(after, what)?;
    only(m, &["trips", "base_trips_hash"], what)?;
    match m.get("base_trips_hash").and_then(Value::as_str) {
        Some(h) if !h.trim().is_empty() => {}
        _ => {
            return Err(invalid(
                "base_trips_hash",
                format!("{what}: base_trips_hash is required"),
            ))
        }
    }
    let trips = trip_specs(after).map_err(|e| invalid("trips", format!("{what}: {e}")))?;
    for (i, t) in trips.iter().enumerate() {
        let at =
            |key: &str, message: String| invalid(key, format!("{what}: trip {}: {message}", i + 1));
        if let Some(id) = &t.trip_id {
            if !id.trim().is_empty() {
                check_trip_id(id).map_err(|f| at("trip_id", f.message))?;
            }
        }
        if t.pattern_key < 1 {
            return Err(at(
                "pattern_key",
                "pattern_key is a whole number from 1".into(),
            ));
        }
        if t.profile_key.is_some_and(|p| p < 1) {
            return Err(at(
                "profile_key",
                "profile_key is a whole number from 1".into(),
            ));
        }
        if t.service_id.trim().is_empty() {
            return Err(at("service_id", "service_id is required".into()));
        }
        for (key, v, max) in [
            ("direction_id", t.direction_id, 1),
            ("wheelchair_accessible", t.wheelchair_accessible, 2),
            ("bikes_allowed", t.bikes_allowed, 2),
            ("cars_allowed", t.cars_allowed, 2),
        ] {
            if v.is_some_and(|v| !(0..=max).contains(&v)) {
                return Err(at(key, format!("{key} is from 0 to {max}")));
            }
        }
        if gtfs_timing::parse_time(&t.start_time).is_none() {
            return Err(Finding::error(
                "invalid_time",
                format!("{i}"),
                format!(
                    "{what}: trip {}: start_time {:?} is not a time from 00:00:00 to 47:59:59",
                    i + 1,
                    t.start_time
                ),
            ));
        }
        if let Some(s) = &t.source {
            if !TRIP_SOURCES.contains(&s.as_str()) {
                return Err(at(
                    "source",
                    format!("source is {}", TRIP_SOURCES.join(", ")),
                ));
            }
        }
        if t.source_ref
            .as_ref()
            .is_some_and(|v| !v.is_object() && !v.is_null())
        {
            return Err(at("source_ref", "source_ref is an object".into()));
        }
        for label in [&t.headsign, &t.short_name, &t.block_id, &t.shape_id]
            .into_iter()
            .flatten()
        {
            if label.chars().count() > LABEL_MAX_CHARS * 2 {
                return Err(at(
                    "",
                    format!("a text is longer than {} characters", LABEL_MAX_CHARS * 2),
                ));
            }
        }
        for f in t.frequencies.iter().flatten() {
            let (Some(_), Some(_)) = (
                gtfs_timing::parse_time(&f.start_time),
                gtfs_timing::parse_window_end(&f.end_time),
            ) else {
                return Err(Finding::error(
                    "invalid_time",
                    format!("{i}"),
                    format!(
                        "{what}: trip {}: a frequency window lies within 00:00:00-48:00:00",
                        i + 1
                    ),
                ));
            };
            if f.headway_s <= 0 {
                return Err(at(
                    "headway_s",
                    "headway_s is a positive number of seconds".into(),
                ));
            }
            if f.exact_times.is_some_and(|e| !(0..=1).contains(&e)) {
                return Err(at("exact_times", "exact_times is 0 or 1".into()));
            }
        }
    }
    Ok(())
}

/// A service's days as sent: each day given `true` or `false`. `None` when
/// `days` is not sent.
fn days_of(m: &Map<String, Value>, what: &str) -> Result<Option<Map<String, Value>>, Finding> {
    match m.get("days") {
        None => Ok(None),
        Some(Value::Object(d)) => {
            if let Some(k) = d.keys().find(|k| !DAYS.contains(&k.as_str())) {
                return Err(invalid(
                    "days",
                    format!("{what}: {k:?} is not a day of the week"),
                ));
            }
            if d.values().any(|v| !v.is_boolean()) {
                return Err(invalid(
                    "days",
                    format!("{what}: each day is true or false"),
                ));
            }
            Ok(Some(d.clone()))
        }
        Some(_) => Err(invalid(
            "days",
            format!("{what}: days is {{monday: true, ...}}"),
        )),
    }
}

/// A service's added and removed dates as sent, in date order.
pub fn service_dates(
    m: &Map<String, Value>,
    what: &str,
) -> Result<Option<Vec<(NaiveDate, i16)>>, Finding> {
    let Some(list) = m.get("dates") else {
        return Ok(None);
    };
    let bad = || {
        invalid(
            "dates",
            format!("{what}: dates is a list of {{date: \"YYYY-MM-DD\", exception_type: 1 | 2}}"),
        )
    };
    let mut out = BTreeMap::new();
    for d in list.as_array().ok_or_else(bad)? {
        let o = d.as_object().ok_or_else(bad)?;
        only(o, &["date", "exception_type"], what)?;
        let date = o
            .get("date")
            .and_then(Value::as_str)
            .and_then(parse_date)
            .ok_or_else(bad)?;
        let kind = o
            .get("exception_type")
            .and_then(Value::as_i64)
            .filter(|k| *k == 1 || *k == 2)
            .ok_or_else(bad)? as i16;
        if out.insert(date, kind).is_some() {
            return Err(invalid("dates", format!("{what}: {date} is listed twice")));
        }
    }
    Ok(Some(out.into_iter().collect()))
}

/// `start_date` / `end_date`: sent together, both dates or both null, the end
/// on or after the start. `None` when neither is sent.
pub fn service_range(
    m: &Map<String, Value>,
    what: &str,
) -> Result<Option<Option<(NaiveDate, NaiveDate)>>, Finding> {
    let (s, e) = (m.get("start_date"), m.get("end_date"));
    if s.is_none() && e.is_none() {
        return Ok(None);
    }
    let bad = |why: &str| Finding::error("invalid_dates", "", format!("{what}: {why}"));
    let date = |v: Option<&Value>| -> Result<Option<NaiveDate>, Finding> {
        match v {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_str()
                .and_then(parse_date)
                .map(Some)
                .ok_or_else(|| bad("a date is YYYY-MM-DD")),
        }
    };
    match (date(s)?, date(e)?) {
        (None, None) => Ok(Some(None)),
        (Some(a), Some(b)) if b >= a => Ok(Some(Some((a, b)))),
        (Some(_), Some(_)) => Err(bad("end_date is before start_date")),
        _ => Err(bad(
            "start_date and end_date are given together, or neither",
        )),
    }
}

/// `service/create` `{service_id, days, start_date?, end_date?, label?,
/// dates?}`, `service/update` (the same, all optional; `dates` replaces the
/// list) and `service/delete` (`null`). A service that runs on no day and no
/// added date is allowed - a feed's stub trips hang off one - and the apply
/// says so with a warning.
pub fn check_service_payload(op: &str, key: &str, after: &Value) -> Result<(), Finding> {
    let what = format!("service/{op}");
    let what = what.as_str();
    if op == "delete" {
        return if after.is_null() {
            Ok(())
        } else {
            Err(invalid("", format!("{what}: `after` must be null")))
        };
    }
    let m = object(after, what)?;
    let create = op == "create";
    if create {
        only(
            m,
            &[
                "service_id",
                "days",
                "start_date",
                "end_date",
                "label",
                "dates",
            ],
            what,
        )?;
        let id = m
            .get("service_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        check_entity_id("service_id", id)?;
        if id != key {
            return Err(invalid(
                id,
                format!("{what}: entity_key must equal service_id"),
            ));
        }
    } else {
        only(
            m,
            &["days", "start_date", "end_date", "label", "dates"],
            what,
        )?;
        if m.is_empty() {
            return Err(invalid("", format!("{what}: nothing to change")));
        }
    }
    days_of(m, what)?;
    service_range(m, what)?;
    service_dates(m, what)?;
    label_ok(m, "label", what)
}

// ---------------------------------------------------------------- checking trips

/// What a route's trips are checked against: the route's patterns and profiles
/// and the feed's services once the draft applies, and the trip ids other routes
/// hold.
pub struct TripRules<'a> {
    pub route_id: &'a str,
    /// Existing patterns of the route.
    pub patterns: &'a HashSet<i16>,
    /// First arrival offset of each existing profile, by `(pattern, profile)`.
    pub profiles: &'a HashMap<(i16, i32), i32>,
    pub service_exists: &'a dyn Fn(&str) -> bool,
    /// Trip id -> the other route that holds it.
    pub taken: &'a HashMap<String, String>,
}

/// A trip that passed: its reference time and frequency windows.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedTrip {
    pub ref_s: i32,
    pub frequencies: Vec<StoredFrequency>,
}

/// The rules of 16.4 for a route's whole trip list. Findings about one trip
/// carry its index ([`Finding::row`]); a trip with an error has no
/// [`CheckedTrip`]. Keys name the trip by id (or position, for one without), so
/// a problem the live list already had can be told apart from a new one.
pub fn check_trips(
    trips: &[TripSpec],
    rules: &TripRules,
    label: &dyn Fn(usize) -> String,
) -> (Vec<Finding>, Vec<Option<CheckedTrip>>) {
    let mut findings = Vec::new();
    let mut out = Vec::with_capacity(trips.len());
    let mut ids: HashMap<&str, usize> = HashMap::new();
    let mut departures: HashMap<(i16, &str, Option<i16>, i32), usize> = HashMap::new();
    for (i, t) in trips.iter().enumerate() {
        let name = label(i);
        let key = t
            .id()
            .map(str::to_string)
            .unwrap_or_else(|| format!("#{}", i + 1));
        let before = findings.len();
        let mut err = |code: &str, message: String| {
            findings.push(
                Finding::error(code, format!("{key}|{code}"), format!("{name}: {message}")).at(i),
            );
        };
        if let Some(id) = t.id() {
            if let Some(other) = rules.taken.get(id) {
                err(
                    "trip_id_taken",
                    format!("trip id {id} is route {other}'s; a trip id is unique in the feed"),
                );
            } else if let Some(first) = ids.insert(id, i) {
                err(
                    "trip_id_taken",
                    format!("trip id {id} is also {}", label(first)),
                );
            }
        }
        if !rules.patterns.contains(&t.pattern_key) {
            err(
                "pattern_not_found",
                format!("route {} has no pattern {}", rules.route_id, t.pattern_key),
            );
        }
        let first = match t.profile_key {
            None => Some(0),
            Some(p) => {
                let found = rules.profiles.get(&(t.pattern_key, p)).copied();
                if found.is_none() && rules.patterns.contains(&t.pattern_key) {
                    err(
                        "profile_not_found",
                        format!(
                            "pattern {} of route {} has no timing profile {p}",
                            t.pattern_key, rules.route_id
                        ),
                    );
                }
                found
            }
        };
        if !(rules.service_exists)(t.service_id.trim()) {
            err(
                "service_not_found",
                format!("no service {}", t.service_id.trim()),
            );
        }
        let start = gtfs_timing::parse_time(&t.start_time);
        let ref_s = match (start, first) {
            (Some(s), Some(f)) => {
                let r = s - f;
                if !(0..=MAX_TIME_S).contains(&r) {
                    err(
                        "invalid_time",
                        format!(
                            "starting at {} its profile would put the trip's reference time outside 00:00:00-47:59:59",
                            t.start_time.trim()
                        ),
                    );
                    None
                } else {
                    Some(r)
                }
            }
            (None, _) => {
                err(
                    "invalid_time",
                    format!(
                        "start_time {:?} is not a time from 00:00:00 to 47:59:59",
                        t.start_time
                    ),
                );
                None
            }
            _ => None,
        };
        let mut windows: Vec<StoredFrequency> = Vec::new();
        for f in t.frequencies.iter().flatten() {
            match (
                gtfs_timing::parse_time(&f.start_time),
                gtfs_timing::parse_window_end(&f.end_time),
            ) {
                (Some(a), Some(b)) if a < b && f.headway_s > 0 => windows.push(StoredFrequency {
                    start_s: a,
                    end_s: b,
                    headway_s: f.headway_s,
                    exact_times: f.exact_times,
                }),
                _ => err(
                    "invalid_time",
                    format!(
                        "frequency window {}-{} is not a window within 00:00:00-48:00:00 with a positive headway",
                        f.start_time, f.end_time
                    ),
                ),
            }
        }
        windows.sort_by_key(|w| w.start_s);
        if windows.windows(2).any(|w| w[1].start_s < w[0].end_s) {
            err(
                "frequency_overlap",
                "its frequency windows overlap".to_string(),
            );
        }
        if let (Some(s), true) = (start, windows.is_empty()) {
            let slot = (t.pattern_key, t.service_id.trim(), t.direction_id, s);
            if let Some(other) = departures.insert(slot, i) {
                findings.push(
                    Finding::warning(
                        "duplicate_departure",
                        format!("{key}|duplicate_departure"),
                        format!(
                            "{name} leaves at the same time as {} (same pattern, service and direction)",
                            label(other)
                        ),
                    )
                    .at(i),
                );
            }
        }
        let failed = findings[before..].iter().any(|f| f.level == Level::Error);
        out.push(match (failed, ref_s) {
            (false, Some(ref_s)) => Some(CheckedTrip {
                ref_s,
                frequencies: windows,
            }),
            _ => None,
        });
    }
    (findings, out)
}

// ---------------------------------------------------------------- apply

/// The route a change edits: it exists and is not deleted, locked for the
/// change.
async fn live_route(conn: &mut PgConnection, g: &str, route_id: &str) -> Result<(), ApplyError> {
    let deleted: bool = sqlx::query(
        "SELECT deleted FROM gtfs_route WHERE gtfs_id = $1 AND route_id = $2 FOR UPDATE",
    )
    .bind(g)
    .bind(route_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| fail("route_not_found", format!("no route {route_id}")))?
    .try_get("deleted")?;
    if deleted {
        return Err(fail(
            "route_deleted",
            format!("route {route_id} is deleted"),
        ));
    }
    Ok(())
}

async fn pattern_exists(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    pattern_key: i16,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "SELECT 1 FROM gtfs_pattern WHERE gtfs_id = $1 AND route_id = $2 AND pattern_key = $3 FOR UPDATE",
    )
    .bind(g)
    .bind(route_id)
    .bind(pattern_key)
    .fetch_optional(&mut *conn)
    .await?
    .is_some())
}

fn no_pattern(route_id: &str, pattern_key: i16) -> ApplyError {
    fail(
        "pattern_not_found",
        format!("route {route_id} has no pattern {pattern_key}"),
    )
}

pub(super) async fn pattern_update(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    live_route(conn, g, route_id).await?;
    let m = after.as_object().expect("payload checked");
    let key = pattern_key_of(m, "").expect("payload checked");
    if !pattern_exists(conn, g, route_id, key).await? {
        return Err(no_pattern(route_id, key));
    }
    let (has_name, name) = field(m, "name");
    let has_direction = m.contains_key("direction_id");
    sqlx::query(
        "UPDATE gtfs_pattern SET \
            name = CASE WHEN $4 THEN $5 ELSE name END, \
            direction_id = CASE WHEN $6 THEN $7 ELSE direction_id END, \
            updated_by = $8 \
         WHERE gtfs_id = $1 AND route_id = $2 AND pattern_key = $3",
    )
    .bind(g)
    .bind(route_id)
    .bind(key)
    .bind(has_name)
    .bind(name.map(str::trim).filter(|n| !n.is_empty()))
    .bind(has_direction)
    .bind(
        m.get("direction_id")
            .and_then(Value::as_i64)
            .map(|d| d as i16),
    )
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(vec![])
}

/// Trips on a pattern, or on one of its profiles when `profile_key` is given.
async fn trips_on(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    pattern_key: i16,
    profile_key: Option<i32>,
) -> Result<i64, sqlx::Error> {
    sqlx::query(
        "SELECT count(*) AS n FROM gtfs_trip WHERE gtfs_id = $1 AND route_id = $2 AND pattern_key = $3 \
           AND ($4::int4 IS NULL OR profile_key = $4)",
    )
    .bind(g)
    .bind(route_id)
    .bind(pattern_key)
    .bind(profile_key)
    .fetch_one(&mut *conn)
    .await?
    .try_get("n")
}

/// A pattern's stop rows and profiles go with it (the tables cascade).
pub(super) async fn pattern_delete(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    after: &Value,
) -> Result<Vec<Finding>, ApplyError> {
    let key =
        pattern_key_of(after.as_object().expect("payload checked"), "").expect("payload checked");
    if key == FIRST_PATTERN {
        return Err(fail(
            "pattern_one",
            format!("pattern 1 is route {route_id}'s stop list; it goes only with the route"),
        ));
    }
    live_route(conn, g, route_id).await?;
    if !pattern_exists(conn, g, route_id, key).await? {
        return Err(no_pattern(route_id, key));
    }
    let n = trips_on(conn, g, route_id, key, None).await?;
    if n > 0 {
        return Err(fail(
            "pattern_in_use",
            format!("{n} trip(s) of route {route_id} run pattern {key}; move or remove them first"),
        ));
    }
    sqlx::query(
        "DELETE FROM gtfs_pattern WHERE gtfs_id = $1 AND route_id = $2 AND pattern_key = $3",
    )
    .bind(g)
    .bind(route_id)
    .bind(key)
    .execute(&mut *conn)
    .await?;
    Ok(vec![])
}

/// The served stops of a pattern as it is now, with their positions.
async fn served_stops(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    pattern_key: i16,
) -> Result<Vec<(String, (f64, f64))>, sqlx::Error> {
    sqlx::query(
        "SELECT rs.stop_id, s.lat, s.lon FROM gtfs_route_stop rs \
         JOIN gtfs_stop s ON s.gtfs_id = rs.gtfs_id AND s.stop_id = rs.stop_id \
         WHERE rs.gtfs_id = $1 AND rs.route_id = $2 AND rs.pattern_key = $3 \
           AND rs.stop_type NOT IN ('ROUTE CORRECTION', 'JUMP STOP', 'HIDDEN STOP') \
         ORDER BY rs.sequence",
    )
    .bind(g)
    .bind(route_id)
    .bind(pattern_key)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(String, (f64, f64)), sqlx::Error> {
        Ok((
            r.try_get("stop_id")?,
            (r.try_get("lat")?, r.try_get("lon")?),
        ))
    })
    .collect()
}

/// A warning naming every implausible hop of a profile.
pub fn implausible_warning(
    what: &str,
    key: &str,
    offsets: &Offsets,
    positions: &[(f64, f64)],
) -> Option<Finding> {
    let hops = gtfs_timing::implausible_hops(offsets, positions);
    if hops.is_empty() {
        return None;
    }
    let listed = hops
        .iter()
        .take(5)
        .map(|(i, kmh)| {
            if kmh.is_finite() {
                format!("stop {} to {} at {kmh:.0} km/h", i + 1, i + 2)
            } else {
                format!("stop {} to {} in no time", i + 1, i + 2)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(Finding::warning(
        "timing_implausible",
        key,
        format!(
            "{what}: {} hop(s) outside {:.0}-{:.0} km/h in a straight line ({listed}{})",
            hops.len(),
            gtfs_timing::IMPLAUSIBLE_SLOW_KMH,
            gtfs_timing::IMPLAUSIBLE_FAST_KMH,
            if hops.len() > 5 { ", ..." } else { "" }
        ),
    ))
}

pub(super) async fn timing_profile_replace(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    live_route(conn, g, route_id).await?;
    let m = after.as_object().expect("payload checked");
    let pattern = pattern_key_of(m, "").expect("payload checked");
    if !pattern_exists(conn, g, route_id, pattern).await? {
        return Err(no_pattern(route_id, pattern));
    }
    let offsets = profile_offsets(after).expect("payload checked");
    let stops = served_stops(conn, g, route_id, pattern).await?;
    if offsets.len() != stops.len() {
        return Err(fail(
            "profile_length_mismatch",
            format!(
                "the profile times {} stops and pattern {pattern} of route {route_id} serves {}; one offset per served stop",
                offsets.len(),
                stops.len()
            ),
        ));
    }
    // minted when the change was added; a probe of one without takes the next
    let profile = match profile_key_of(m, "").expect("payload checked") {
        Some(p) => p,
        None => {
            let n: i32 = sqlx::query(
                "SELECT coalesce(max(profile_key), 0) + 1 AS n FROM gtfs_timing_profile \
                 WHERE gtfs_id = $1 AND route_id = $2 AND pattern_key = $3",
            )
            .bind(g)
            .bind(route_id)
            .bind(pattern)
            .fetch_one(&mut *conn)
            .await?
            .try_get("n")?;
            n
        }
    };
    let positions: Vec<(f64, f64)> = stops.iter().map(|(_, p)| *p).collect();
    let warnings: Vec<Finding> = implausible_warning(
        &format!("profile {profile} of pattern {pattern}"),
        &format!("{pattern}|{profile}"),
        &offsets,
        &positions,
    )
    .into_iter()
    .collect();
    let source = m.get("source").and_then(Value::as_str).unwrap_or("manual");
    let label = m
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|l| !l.is_empty());
    sqlx::query(
        "INSERT INTO gtfs_timing_profile (gtfs_id, route_id, pattern_key, profile_key, arrival_s, \
                                          departure_s, label, source, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (gtfs_id, route_id, pattern_key, profile_key) DO UPDATE SET \
            arrival_s = EXCLUDED.arrival_s, departure_s = EXCLUDED.departure_s, \
            label = EXCLUDED.label, source = EXCLUDED.source, updated_by = EXCLUDED.updated_by",
    )
    .bind(g)
    .bind(route_id)
    .bind(pattern)
    .bind(profile)
    .bind(&offsets.arrival)
    .bind(&offsets.departure)
    .bind(label)
    .bind(source)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(warnings)
}

pub(super) async fn timing_profile_delete(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    after: &Value,
) -> Result<Vec<Finding>, ApplyError> {
    live_route(conn, g, route_id).await?;
    let m = after.as_object().expect("payload checked");
    let pattern = pattern_key_of(m, "").expect("payload checked");
    let profile = profile_key_of(m, "")
        .expect("payload checked")
        .expect("payload checked");
    let found = sqlx::query(
        "SELECT 1 FROM gtfs_timing_profile \
         WHERE gtfs_id = $1 AND route_id = $2 AND pattern_key = $3 AND profile_key = $4 FOR UPDATE",
    )
    .bind(g)
    .bind(route_id)
    .bind(pattern)
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?;
    if found.is_none() {
        return Err(fail(
            "profile_not_found",
            format!("pattern {pattern} of route {route_id} has no timing profile {profile}"),
        ));
    }
    let n = trips_on(conn, g, route_id, pattern, Some(profile)).await?;
    if n > 0 {
        return Err(fail(
            "profile_in_use",
            format!("{n} trip(s) run to profile {profile} of pattern {pattern}; move them first"),
        ));
    }
    sqlx::query(
        "DELETE FROM gtfs_timing_profile \
         WHERE gtfs_id = $1 AND route_id = $2 AND pattern_key = $3 AND profile_key = $4",
    )
    .bind(g)
    .bind(route_id)
    .bind(pattern)
    .bind(profile)
    .execute(&mut *conn)
    .await?;
    Ok(vec![])
}

/// A live trip list as the specs a change would send, for checking it with the
/// same rules: only what [`check_trips`] reads is carried.
pub fn specs_of(live: &[StoredTrip], firsts: &HashMap<(i16, i32), i32>) -> Vec<TripSpec> {
    live.iter()
        .map(|t| TripSpec {
            trip_id: Some(t.trip_id.clone()),
            pattern_key: t.pattern_key,
            profile_key: t.profile_key,
            service_id: t.service_id.clone(),
            direction_id: t.direction_id,
            start_time: gtfs_timing::format_time(
                t.ref_s
                    + t.profile_key
                        .and_then(|p| firsts.get(&(t.pattern_key, p)).copied())
                        .unwrap_or(0),
            ),
            headsign: None,
            short_name: None,
            block_id: None,
            shape_id: None,
            wheelchair_accessible: None,
            bikes_allowed: None,
            cars_allowed: None,
            frequencies: None,
            source_ref: None,
            source: None,
            sort_key: None,
            ref_s: None,
        })
        .collect()
}

/// Replace a route's whole trip list. A trip sent back with its id keeps its
/// `sort_key` and `source`; a new one is sorted after the route's last and is
/// the change's `source` (default `editor`). Only problems the edit introduces
/// block it: one the live list already had is a warning.
pub(super) async fn route_trips_replace(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    live_route(conn, g, route_id).await?;
    let trips = trip_specs(after).map_err(|e| fail("invalid_payload", e))?;
    if let Some(i) = trips.iter().position(|t| t.id().is_none()) {
        // ids are minted when the change is added
        return Err(fail(
            "invalid_payload",
            format!("trip {} has no trip_id", i + 1),
        ));
    }
    let patterns: HashSet<i16> = load_patterns(conn, g, route_id)
        .await?
        .iter()
        .map(|p| p.pattern_key)
        .collect();
    let profiles = first_arrivals(&load_profiles(conn, g, route_id, None).await?);
    let wanted: Vec<String> = trips
        .iter()
        .map(|t| t.service_id.trim().to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let services: HashSet<String> = sqlx::query(
        "SELECT service_id FROM gtfs_service WHERE gtfs_id = $1 AND service_id = ANY($2)",
    )
    .bind(g)
    .bind(&wanted)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| r.try_get("service_id"))
    .collect::<Result<_, _>>()?;
    let ids: Vec<String> = trips
        .iter()
        .filter_map(|t| t.id().map(str::to_string))
        .collect();
    let taken: HashMap<String, String> = sqlx::query(
        "SELECT trip_id, route_id FROM gtfs_trip WHERE gtfs_id = $1 AND trip_id = ANY($2) AND route_id <> $3",
    )
    .bind(g)
    .bind(&ids)
    .bind(route_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(String, String), sqlx::Error> {
        Ok((r.try_get("trip_id")?, r.try_get("route_id")?))
    })
    .collect::<Result<_, _>>()?;
    let rules = TripRules {
        route_id,
        patterns: &patterns,
        profiles: &profiles,
        service_exists: &|s: &str| services.contains(s),
        taken: &taken,
    };
    let label = |i: usize| match trips[i].id() {
        Some(id) => format!("trip {} ({id})", i + 1),
        None => format!("trip {}", i + 1),
    };
    let (findings, checked) = check_trips(&trips, &rules, &label);

    // the live list, checked the same way, for what was already there
    let live = load_route_trips(conn, g, route_id).await?;
    let live_specs = specs_of(&live, &profiles);
    let live_label = |i: usize| format!("trip {} ({})", i + 1, live[i].trip_id);
    let (live_findings, _) = check_trips(&live_specs, &rules, &live_label);
    let findings = super::validation::grade_against_live(findings, &live_findings);
    if findings.iter().any(|f| f.level == Level::Error) {
        return Err(ApplyError::Findings(findings));
    }

    let findings = [findings, trip_references(conn, g, &trips, &live).await?].concat();
    if findings.iter().any(|f| f.level == Level::Error) {
        return Err(ApplyError::Findings(findings));
    }

    let kept: HashMap<&str, &StoredTrip> = live.iter().map(|t| (t.trip_id.as_str(), t)).collect();
    let mut next_sort = live.iter().map(|t| t.sort_key).max().unwrap_or(0);
    let text = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut rows: Vec<StoredTrip> = Vec::with_capacity(trips.len());
    for (t, c) in trips.iter().zip(checked) {
        let c = c.expect("no errors");
        let id = t.id().expect("ids checked").to_string();
        let (sort_key, source, stored_ref) = match kept.get(id.as_str()) {
            Some(live) => (live.sort_key, live.source.clone(), live.source_ref.clone()),
            None => {
                next_sort += 1;
                let source = t.source.clone().unwrap_or_else(|| "editor".into());
                (next_sort, source, None)
            }
        };
        rows.push(StoredTrip {
            trip_id: id,
            pattern_key: t.pattern_key,
            profile_key: t.profile_key,
            service_id: t.service_id.trim().to_string(),
            direction_id: t.direction_id,
            ref_s: c.ref_s,
            headsign: text(&t.headsign),
            short_name: text(&t.short_name),
            block_id: text(&t.block_id),
            shape_id: text(&t.shape_id),
            wheelchair_accessible: t.wheelchair_accessible,
            bikes_allowed: t.bikes_allowed,
            cars_allowed: t.cars_allowed,
            sort_key,
            source,
            source_ref: t.source_ref.clone().filter(|v| !v.is_null()).or(stored_ref),
            frequencies: c.frequencies,
        });
    }
    let col = |f: &dyn Fn(&StoredTrip) -> Option<String>| rows.iter().map(f).collect::<Vec<_>>();
    let small = |f: &dyn Fn(&StoredTrip) -> Option<i16>| rows.iter().map(f).collect::<Vec<_>>();
    sqlx::query("DELETE FROM gtfs_trip WHERE gtfs_id = $1 AND route_id = $2")
        .bind(g)
        .bind(route_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO gtfs_trip (gtfs_id, trip_id, route_id, pattern_key, profile_key, service_id, \
                                direction_id, ref_s, headsign, short_name, block_id, \
                                wheelchair_accessible, bikes_allowed, sort_key, source, source_ref, \
                                updated_by, shape_id, cars_allowed) \
         SELECT $1, u.id, $2, u.pattern, u.profile, u.service, u.direction, u.ref, u.headsign, u.short, \
                u.block, u.wheel, u.bikes, u.sort, u.source, u.sref::jsonb, $3, u.shape, u.cars \
         FROM UNNEST($4::text[], $5::int2[], $6::int4[], $7::text[], $8::int2[], $9::int4[], \
                     $10::text[], $11::text[], $12::text[], $13::int2[], $14::int2[], $15::int4[], \
                     $16::text[], $17::text[], $18::text[], $19::int2[]) \
              AS u(id, pattern, profile, service, direction, ref, headsign, short, block, wheel, bikes, \
                   sort, source, sref, shape, cars)",
    )
    .bind(g)
    .bind(route_id)
    .bind(actor)
    .bind(rows.iter().map(|t| t.trip_id.clone()).collect::<Vec<_>>())
    .bind(rows.iter().map(|t| t.pattern_key).collect::<Vec<_>>())
    .bind(rows.iter().map(|t| t.profile_key).collect::<Vec<_>>())
    .bind(rows.iter().map(|t| t.service_id.clone()).collect::<Vec<_>>())
    .bind(small(&|t| t.direction_id))
    .bind(rows.iter().map(|t| t.ref_s).collect::<Vec<_>>())
    .bind(col(&|t| t.headsign.clone()))
    .bind(col(&|t| t.short_name.clone()))
    .bind(col(&|t| t.block_id.clone()))
    .bind(small(&|t| t.wheelchair_accessible))
    .bind(small(&|t| t.bikes_allowed))
    .bind(rows.iter().map(|t| t.sort_key).collect::<Vec<_>>())
    .bind(rows.iter().map(|t| t.source.clone()).collect::<Vec<_>>())
    .bind(col(&|t| t.source_ref.as_ref().map(Value::to_string)))
    .bind(col(&|t| t.shape_id.clone()))
    .bind(small(&|t| t.cars_allowed))
    .execute(&mut *conn)
    .await?;
    let windows: Vec<(&str, &StoredFrequency)> = rows
        .iter()
        .flat_map(|t| t.frequencies.iter().map(move |f| (t.trip_id.as_str(), f)))
        .collect();
    if !windows.is_empty() {
        sqlx::query(
            "INSERT INTO gtfs_frequency (gtfs_id, trip_id, start_s, end_s, headway_s, exact_times) \
             SELECT $1, u.trip, u.a, u.b, u.h, u.e \
             FROM UNNEST($2::text[], $3::int4[], $4::int4[], $5::int4[], $6::int2[]) AS u(trip, a, b, h, e)",
        )
        .bind(g)
        .bind(windows.iter().map(|(id, _)| id.to_string()).collect::<Vec<_>>())
        .bind(windows.iter().map(|(_, f)| f.start_s).collect::<Vec<_>>())
        .bind(windows.iter().map(|(_, f)| f.end_s).collect::<Vec<_>>())
        .bind(windows.iter().map(|(_, f)| f.headway_s).collect::<Vec<_>>())
        .bind(windows.iter().map(|(_, f)| f.exact_times).collect::<Vec<_>>())
        .execute(&mut *conn)
        .await?;
    }
    Ok(findings)
}

/// What a route's new trip list does to what names its trips (section 18): in
/// a feed that keeps its shapes, a shape a trip takes must be one the feed has
/// - an error when the edit gives it, a warning when the live trip already had
/// it - and a trip a transfer or an attribution names cannot be dropped from
/// the route. A feed with no shapes in the tables (chennai_bus, whose shapes
/// are drawn elsewhere) is not checked, as a feed without agencies is not.
async fn trip_references(
    conn: &mut PgConnection,
    g: &str,
    trips: &[TripSpec],
    live: &[StoredTrip],
) -> Result<Vec<Finding>, ApplyError> {
    let mut out = Vec::new();
    let live_shapes: HashSet<(&str, &str)> = live
        .iter()
        .filter_map(|t| Some((t.trip_id.as_str(), t.shape_id.as_deref()?)))
        .collect();
    let named: Vec<String> = trips
        .iter()
        .filter_map(|t| t.shape_id.as_deref().map(str::trim))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let keeps_shapes = !named.is_empty()
        && sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM gtfs_shape WHERE gtfs_id = $1)",
        )
        .bind(g)
        .fetch_one(&mut *conn)
        .await?;
    if keeps_shapes {
        let found: HashSet<String> = sqlx::query_scalar(
            "SELECT shape_id FROM gtfs_shape WHERE gtfs_id = $1 AND shape_id = ANY($2)",
        )
        .bind(g)
        .bind(&named)
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .collect();
        for t in trips {
            let Some(shape) = t
                .shape_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            if found.contains(shape) {
                continue;
            }
            let id = t.id().unwrap_or("");
            let message = format!("trip {id} takes shape {shape}, which the feed does not have");
            out.push(if live_shapes.contains(&(id, shape)) {
                Finding::warning(
                    "reference_not_found",
                    id,
                    format!("{message} (already present before this edit)"),
                )
            } else {
                Finding::error("reference_not_found", id, message)
            });
        }
    }
    let staying: HashSet<&str> = trips.iter().filter_map(TripSpec::id).collect();
    for t in live
        .iter()
        .filter(|t| !staying.contains(t.trip_id.as_str()))
    {
        let users = super::records::used_by(conn, g, "trips.txt", "trip_id", &t.trip_id).await?;
        if !users.is_empty() {
            out.push(Finding::error(
                "trip_in_use",
                t.trip_id.as_str(),
                format!(
                    "trip {} is named by {}; change or remove those before dropping it",
                    t.trip_id,
                    super::records::say_users(&users)
                ),
            ));
        }
    }
    Ok(out)
}

/// Carry every stored profile of a pattern over onto its new stop list, inside
/// the change that altered it; `old_ids` are the pattern's served stops before.
/// A warning `timing_interpolated` says how many profiles and stops had to be
/// estimated. Trips on the default timing need nothing: the formula follows
/// the list.
pub(super) async fn carry_over_profiles(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    pattern_key: i16,
    old_ids: &[String],
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let profiles = load_profiles(conn, g, route_id, Some(pattern_key)).await?;
    if profiles.is_empty() {
        return Ok(vec![]);
    }
    let stops = served_stops(conn, g, route_id, pattern_key).await?;
    let new_ids: Vec<&str> = stops.iter().map(|(id, _)| id.as_str()).collect();
    // a stop list too short to time is an error of its own, which blocks the
    // draft; the profiles are left as they are until it is fixed
    if new_ids.len() < 2
        || new_ids
            .iter()
            .copied()
            .eq(old_ids.iter().map(String::as_str))
    {
        return Ok(vec![]);
    }
    let positions: Vec<(f64, f64)> = stops.iter().map(|(_, p)| *p).collect();
    let (mut carried, mut estimated_profiles, mut estimated_stops) = (0, 0, 0);
    for p in &profiles {
        let Some(c) = gtfs_timing::carry_over(old_ids, &p.offsets, &new_ids, &positions) else {
            return Err(fail(
                "profile_length_mismatch",
                format!(
                    "profile {} of pattern {pattern_key} of route {route_id} does not time the stops the pattern served; fix it before changing the stop list",
                    p.profile_key
                ),
            ));
        };
        sqlx::query(
            "UPDATE gtfs_timing_profile SET arrival_s = $5, departure_s = $6, \
                source = CASE WHEN $7 THEN 'interpolated' ELSE source END, updated_by = $8 \
             WHERE gtfs_id = $1 AND route_id = $2 AND pattern_key = $3 AND profile_key = $4",
        )
        .bind(g)
        .bind(route_id)
        .bind(pattern_key)
        .bind(p.profile_key)
        .bind(&c.offsets.arrival)
        .bind(&c.offsets.departure)
        .bind(c.estimated > 0)
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        carried += 1;
        if c.estimated > 0 {
            estimated_profiles += 1;
            estimated_stops += c.estimated;
        }
    }
    if estimated_stops == 0 {
        return Ok(vec![]);
    }
    Ok(vec![Finding::warning(
        "timing_interpolated",
        format!("{route_id}|{pattern_key}"),
        format!(
            "pattern {pattern_key} of route {route_id}: {carried} timing profile(s) carried over to the new stop list; in {estimated_profiles} of them {estimated_stops} stop time(s) are estimated - check the times in the preview"
        ),
    )])
}

fn days_values(m: &Map<String, Value>) -> Option<Vec<bool>> {
    let d = m.get("days")?.as_object()?;
    Some(
        DAYS.iter()
            .map(|k| d.get(*k).and_then(Value::as_bool).unwrap_or(false))
            .collect(),
    )
}

async fn write_service_dates(
    conn: &mut PgConnection,
    g: &str,
    service_id: &str,
    dates: &[(NaiveDate, i16)],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM gtfs_service_date WHERE gtfs_id = $1 AND service_id = $2")
        .bind(g)
        .bind(service_id)
        .execute(&mut *conn)
        .await?;
    if dates.is_empty() {
        return Ok(());
    }
    let (days, kinds): (Vec<NaiveDate>, Vec<i16>) = dates.iter().cloned().unzip();
    sqlx::query(
        "INSERT INTO gtfs_service_date (gtfs_id, service_id, service_date, exception_type) \
         SELECT $1, $2, u.d, u.k FROM UNNEST($3::date[], $4::int2[]) AS u(d, k)",
    )
    .bind(g)
    .bind(service_id)
    .bind(&days)
    .bind(&kinds)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub(super) async fn service_create(
    conn: &mut PgConnection,
    g: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let m = after.as_object().expect("payload checked");
    let id = m["service_id"].as_str().expect("payload checked").trim();
    let days = days_values(m).unwrap_or_else(|| vec![false; 7]);
    let range = service_range(m, "").expect("payload checked").flatten();
    let created = sqlx::query(
        "INSERT INTO gtfs_service (gtfs_id, service_id, monday, tuesday, wednesday, thursday, friday, \
                                   saturday, sunday, start_date, end_date, label, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
         ON CONFLICT DO NOTHING RETURNING service_id",
    )
    .bind(g)
    .bind(id)
    .bind(days[0])
    .bind(days[1])
    .bind(days[2])
    .bind(days[3])
    .bind(days[4])
    .bind(days[5])
    .bind(days[6])
    .bind(range.map(|r| r.0))
    .bind(range.map(|r| r.1))
    .bind(
        m.get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|l| !l.is_empty()),
    )
    .bind(actor)
    .fetch_optional(&mut *conn)
    .await?;
    if created.is_none() {
        return Err(fail(
            "service_exists",
            format!("service {id} already exists"),
        ));
    }
    if let Some(dates) = service_dates(m, "").expect("payload checked") {
        write_service_dates(conn, g, id, &dates).await?;
    }
    never_runs(conn, g, id).await
}

/// `service_never_runs` when a service runs on no day of the week and no added
/// date once the change applies: allowed (chennai_bus's stub trips hang off
/// one), but worth a reviewer's look.
async fn never_runs(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let runs: bool = sqlx::query(
        "SELECT (monday OR tuesday OR wednesday OR thursday OR friday OR saturday OR sunday \
                 OR EXISTS (SELECT 1 FROM gtfs_service_date d WHERE d.gtfs_id = s.gtfs_id \
                              AND d.service_id = s.service_id AND d.exception_type = 1)) AS runs \
         FROM gtfs_service s WHERE s.gtfs_id = $1 AND s.service_id = $2",
    )
    .bind(g)
    .bind(id)
    .fetch_one(&mut *conn)
    .await?
    .try_get("runs")?;
    Ok(if runs {
        vec![]
    } else {
        vec![Finding::warning(
            "service_never_runs",
            id,
            format!(
                "service {id} runs on no day of the week and no added date; its trips never run"
            ),
        )]
    })
}

pub(super) async fn service_update(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let m = after.as_object().expect("payload checked");
    let live = sqlx::query(
        "SELECT monday, tuesday, wednesday, thursday, friday, saturday, sunday \
         FROM gtfs_service WHERE gtfs_id = $1 AND service_id = $2 FOR UPDATE",
    )
    .bind(g)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| fail("service_not_found", format!("no service {id}")))?;
    // days sent are set, days not sent keep theirs
    let sent = m.get("days").and_then(Value::as_object);
    let mut days = Vec::with_capacity(7);
    for d in DAYS {
        days.push(match sent.and_then(|s| s.get(d)).and_then(Value::as_bool) {
            Some(v) => v,
            None => live.try_get::<bool, _>(d)?,
        });
    }
    let range = service_range(m, "").expect("payload checked");
    let (has_label, label) = field(m, "label");
    sqlx::query(
        "UPDATE gtfs_service SET monday = $3, tuesday = $4, wednesday = $5, thursday = $6, \
            friday = $7, saturday = $8, sunday = $9, \
            start_date = CASE WHEN $10 THEN $11 ELSE start_date END, \
            end_date = CASE WHEN $10 THEN $12 ELSE end_date END, \
            label = CASE WHEN $13 THEN $14 ELSE label END, updated_by = $15 \
         WHERE gtfs_id = $1 AND service_id = $2",
    )
    .bind(g)
    .bind(id)
    .bind(days[0])
    .bind(days[1])
    .bind(days[2])
    .bind(days[3])
    .bind(days[4])
    .bind(days[5])
    .bind(days[6])
    .bind(range.is_some())
    .bind(range.flatten().map(|r| r.0))
    .bind(range.flatten().map(|r| r.1))
    .bind(has_label)
    .bind(label.map(str::trim).filter(|l| !l.is_empty()))
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    if let Some(dates) = service_dates(m, "").expect("payload checked") {
        write_service_dates(conn, g, id, &dates).await?;
    }
    never_runs(conn, g, id).await
}

pub(super) async fn service_delete(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let found =
        sqlx::query("SELECT 1 FROM gtfs_service WHERE gtfs_id = $1 AND service_id = $2 FOR UPDATE")
            .bind(g)
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    if found.is_none() {
        return Err(fail("service_not_found", format!("no service {id}")));
    }
    let n: i64 =
        sqlx::query("SELECT count(*) AS n FROM gtfs_trip WHERE gtfs_id = $1 AND service_id = $2")
            .bind(g)
            .bind(id)
            .fetch_one(&mut *conn)
            .await?
            .try_get("n")?;
    if n > 0 {
        return Err(fail(
            "service_in_use",
            format!("{n} trip(s) run on service {id}; move them first"),
        ));
    }
    // timeframes and booking rules name a service too (section 18)
    let users: Vec<_> = super::records::used_by(conn, g, "calendar.txt", "service_id", id)
        .await?
        .into_iter()
        .filter(|(file, _, _)| file != "trips.txt")
        .collect();
    if !users.is_empty() {
        return Err(fail(
            "service_in_use",
            format!(
                "service {id} is named by {}; change or remove those first",
                super::records::say_users(&users)
            ),
        ));
    }
    sqlx::query("DELETE FROM gtfs_service WHERE gtfs_id = $1 AND service_id = $2")
        .bind(g)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(vec![])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trip(id: Option<&str>, pattern: i16, service: &str, start: &str) -> TripSpec {
        serde_json::from_value(json!({
            "trip_id": id, "pattern_key": pattern, "service_id": service, "start_time": start,
        }))
        .unwrap()
    }

    fn codes(f: &[Finding], level: Level) -> Vec<&str> {
        f.iter()
            .filter(|x| x.level == level)
            .map(|x| x.code.as_str())
            .collect()
    }

    #[test]
    fn trips_are_checked_against_what_the_route_has() {
        let patterns: HashSet<i16> = [1, 2].into_iter().collect();
        let profiles: HashMap<(i16, i32), i32> = [((2, 1), 60)].into_iter().collect();
        let taken: HashMap<String, String> = [("OTHER-1".to_string(), "R9".to_string())]
            .into_iter()
            .collect();
        let rules = TripRules {
            route_id: "R1",
            patterns: &patterns,
            profiles: &profiles,
            service_exists: &|s: &str| s == "WK",
            taken: &taken,
        };
        let label = |i: usize| format!("trip {}", i + 1);
        let mut on_profile = trip(Some("T2"), 2, "WK", "06:01:00");
        on_profile.profile_key = Some(1);
        let trips = vec![
            trip(Some("T1"), 1, "WK", "6:00"),
            on_profile,
            trip(Some("T3"), 3, "WK", "06:00:00"),
            trip(Some("OTHER-1"), 1, "SUN", "06:00:00"),
            trip(Some("T1"), 1, "WK", "07:00:00"),
            trip(Some("T6"), 1, "WK", "06:00:00"),
        ];
        let (f, ok) = check_trips(&trips, &rules, &label);
        // T2's reference time is its start less its profile's first arrival
        assert_eq!(ok[1].as_ref().unwrap().ref_s, 6 * 3600 + 60 - 60);
        assert_eq!(ok[0].as_ref().unwrap().ref_s, 6 * 3600);
        assert_eq!(
            codes(&f, Level::Error),
            vec![
                "pattern_not_found",
                "trip_id_taken",
                "service_not_found",
                "trip_id_taken"
            ]
        );
        assert!(ok[2].is_none() && ok[3].is_none() && ok[4].is_none());
        // two trips of a pattern, service and direction at one time: a warning
        assert_eq!(codes(&f, Level::Warning), vec!["duplicate_departure"]);
        assert_eq!(
            f.iter()
                .find(|x| x.code == "duplicate_departure")
                .unwrap()
                .row,
            Some(5)
        );
    }

    #[test]
    fn a_profile_must_exist_and_keep_the_reference_time_in_the_day() {
        let patterns: HashSet<i16> = [1].into_iter().collect();
        let profiles: HashMap<(i16, i32), i32> = [((1, 1), 600)].into_iter().collect();
        let taken = HashMap::new();
        let rules = TripRules {
            route_id: "R1",
            patterns: &patterns,
            profiles: &profiles,
            service_exists: &|_: &str| true,
            taken: &taken,
        };
        let label = |i: usize| format!("trip {}", i + 1);
        let mut a = trip(Some("A"), 1, "WK", "00:05:00");
        a.profile_key = Some(1);
        let mut b = trip(Some("B"), 1, "WK", "06:00:00");
        b.profile_key = Some(2);
        let (f, _) = check_trips(&[a, b], &rules, &label);
        assert_eq!(
            codes(&f, Level::Error),
            vec!["invalid_time", "profile_not_found"]
        );
    }

    #[test]
    fn frequency_windows_may_not_overlap() {
        let patterns: HashSet<i16> = [1].into_iter().collect();
        let (profiles, taken) = (HashMap::new(), HashMap::new());
        let rules = TripRules {
            route_id: "R1",
            patterns: &patterns,
            profiles: &profiles,
            service_exists: &|_: &str| true,
            taken: &taken,
        };
        let spec = |windows: Value| -> TripSpec {
            serde_json::from_value(json!({
                "trip_id": "F", "pattern_key": 1, "service_id": "WK", "start_time": "06:00:00",
                "frequencies": windows,
            }))
            .unwrap()
        };
        let label = |i: usize| format!("trip {}", i + 1);
        let fine = spec(json!([
            {"start_time": "06:00:00", "end_time": "09:00:00", "headway_s": 300},
            {"start_time": "09:00:00", "end_time": "25:00:00", "headway_s": 600, "exact_times": 1}]));
        let (f, ok) = check_trips(&[fine], &rules, &label);
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(ok[0].as_ref().unwrap().frequencies.len(), 2);
        let overlapping = spec(json!([
            {"start_time": "06:00:00", "end_time": "09:00:00", "headway_s": 300},
            {"start_time": "08:00:00", "end_time": "10:00:00", "headway_s": 600}]));
        let (f, _) = check_trips(&[overlapping], &rules, &label);
        assert_eq!(codes(&f, Level::Error), vec!["frequency_overlap"]);
    }

    #[test]
    fn change_shapes() {
        let trips =
            |t: Value| check_route_trips_payload(&json!({"base_trips_hash": "x", "trips": t}));
        assert!(
            trips(json!([{"pattern_key": 1, "service_id": "WK", "start_time": "05:10"}])).is_ok()
        );
        assert_eq!(
            trips(json!([{"pattern_key": 1, "service_id": "WK", "start_time": "5.10"}]))
                .unwrap_err()
                .code,
            "invalid_time"
        );
        assert_eq!(
            trips(
                json!([{"pattern_key": 1, "service_id": "WK", "start_time": "05:10", "colour": 1}])
            )
            .unwrap_err()
            .code,
            "invalid_payload"
        );
        assert!(check_trip_id("22B/1-1-OR-PKPT-V7_trip_1").is_ok());
        assert!(check_trip_id("g:t").is_err() && check_trip_id(" t").is_err());

        let profile = |a: Value| check_timing_profile_payload("replace", &a);
        assert!(profile(
            json!({"pattern_key": 1, "arrival_s": [0, 100], "departure_s": [10, 100]})
        )
        .is_ok());
        assert_eq!(
            profile(json!({"pattern_key": 1, "profile_key": 2, "arrival_s": [0, 100], "departure_s": [10, 100]}))
                .unwrap_err()
                .key,
            "base_hash"
        );
        assert_eq!(
            profile(json!({"pattern_key": 1, "arrival_s": [0, 5], "departure_s": [10, 5]}))
                .unwrap_err()
                .code,
            "timing_goes_backwards"
        );
        assert_eq!(
            profile(json!({"pattern_key": 1, "arrival_s": [0, 5, 9], "departure_s": [1, 6]}))
                .unwrap_err()
                .code,
            "profile_length_mismatch"
        );

        let service = |op: &str, a: Value| check_service_payload(op, "WK", &a);
        assert!(service(
            "create",
            json!({"service_id": "WK", "days": {"monday": true}})
        )
        .is_ok());
        assert!(service(
            "create",
            json!({"service_id": "WK", "dates": [{"date": "2026-09-26", "exception_type": 1}]})
        )
        .is_ok());
        // a service that never runs is a warning when it applies, not a shape
        assert!(service(
            "create",
            json!({"service_id": "WK", "days": {"monday": false}})
        )
        .is_ok());
        assert_eq!(
            service(
                "create",
                json!({"service_id": "WK", "dates": [{"date": "20260926", "exception_type": 1}]})
            )
            .unwrap_err()
            .code,
            "invalid_payload"
        );
        assert_eq!(
            service(
                "create",
                json!({"service_id": "WK", "days": {"monday": true}, "start_date": "2026-09-30", "end_date": "2026-09-01"})
            )
            .unwrap_err()
            .code,
            "invalid_dates"
        );
        assert_eq!(
            service("update", json!({"start_date": "2026-09-01"}))
                .unwrap_err()
                .code,
            "invalid_dates"
        );
        assert!(service("update", json!({"days": {"sunday": false}})).is_ok());
        assert!(service("delete", Value::Null).is_ok());

        assert!(
            check_pattern_payload("update", &json!({"pattern_key": 2, "name": "short turn"}))
                .is_ok()
        );
        assert!(check_pattern_payload("update", &json!({"pattern_key": 2})).is_err());
        assert!(check_pattern_payload("delete", &json!({"pattern_key": 0})).is_err());
    }

    #[test]
    fn the_hash_of_nothing_is_one_constant() {
        assert_eq!(
            empty_hash(),
            "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
        );
        assert_eq!(trips_hash(&[]), empty_hash());
        assert_eq!(profile_hash(None), empty_hash());
    }
}
