//! Service Hopper — precomputed metro interchange index.
//!
//! Answers "how do I get from stop A to stop B" for a metro feed, as an ordered
//! list of minimum-degree paths, each split into seated rides (legs). Paths
//! are sorted by total stops by the preprocessor; degree is the number of legs.
//!
//! # Where the answers come from
//!
//! This module no longer plans anything. The graph is built in Nandi by
//! `scripts/gtfs-preprocessor/metro_hopper.py`, which reads the GTFS zip
//! directly — including `calendar.txt` and `calendar_dates.txt`, which this
//! service never had — and emits `metro_hops.json` alongside the other
//! preprocessed artifacts. See
//! `nandi/docs/specs/2026-08-29-metro-hopper-planner.md`.
//!
//! That move exists because the previous in-service builder had no notion of
//! *when* a route runs. It happily routed `St.Thomas Mount → Airport` as one
//! seated ride on route `81407` — a marathon special whose service window
//! expired in January. The planner filters routes by service calendar and emits
//! one graph per day type, so a journey is only offered on days it exists.
//!
//! # What is left here
//!
//! Loading, day-type selection, and a hash-map lookup of all stored paths for
//! an ordered station pair. `edges` is a list of seated rides, one per distinct
//! station sequence, so two rides between the same stations over different
//! stops (Alandur→Central on Blue and on Green) are two edges. A path is a list
//! of edge indices; an absent pair has no path. Route code lists are stored
//! once per ride and shared by every journey that uses it.
//!
//! # Directionality
//!
//! The graph is directed and `A→B` is stored independently of `B→A`. Legs are
//! never produced by reversing a stored path, because the reverse is not always
//! the same journey — a corridor can be served one way by a through-service and
//! the other way only via an interchange. The `routeCode` on a leg is the one
//! that genuinely runs that direction, which is what makes it safe to forward
//! to CMRL.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{Datelike, FixedOffset, NaiveDate, Utc, Weekday};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

/// Dense station id. A metro network of 65k stations is not a thing we need to
/// plan for; `MAX_STATIONS` keeps us far below the limit regardless.
pub type StopId = u16;

/// Index into one day type's ride list. Delhi has ~8,400 rides per day type.
pub type EdgeId = u32;

/// Refuse to load above this many stations. Guards the all-pairs materialisation:
/// a bus feed would be catastrophic here (chennai_bus has 9,776 stops → 95.6M
/// pairs). Metro feeds are two orders of magnitude below this.
pub const MAX_STATIONS: usize = 1_000;

/// Filename of the planner artifact within the preprocessed data directory.
pub const ARTIFACT_FILE: &str = "metro_hops.json";

/// Warn when the artifact is older than this. The planner runs with the rest of
/// the preprocessor, so anything beyond a week means the pipeline is dead and
/// we are serving journeys for a timetable nobody is running. A silently stale
/// answer is the exact failure mode the planner spec exists to remove, so it
/// must be loud.
pub const STALE_AFTER_DAYS: i64 = 7;

/// IST. The feed's service calendar is in local time, so the day type for a
/// query must be too — deriving it from UTC would flip over 5.5 hours early and
/// serve Sunday's reduced graph through Saturday evening.
fn ist() -> FixedOffset {
    FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("+05:30 is a valid offset")
}

/// Today in IST.
pub fn today_ist() -> NaiveDate {
    Utc::now().with_timezone(&ist()).date_naive()
}

/// How a rider gets from one leg onto the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InterchangeType {
    /// The next leg is on a different line.
    LineChange,
    /// The next leg is on the same line — a change of train, not of line.
    /// Happens when a corridor is served only by non-overlapping short-turns.
    SameLineChange,
}

/// One seated ride on a single route.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HopLeg {
    pub src_stop_code: String,
    pub src_stop_name: String,
    /// Platform to board at, e.g. "1" — the one serving this direction.
    /// Absent when the feed carries no platform data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_platform: Option<String>,
    pub dest_stop_code: String,
    pub dest_stop_name: String,
    /// Platform you arrive on. At an interchange this differs from the next
    /// leg's `src_platform`, and that difference is the change to make.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dest_platform: Option<String>,
    /// The route that actually runs this leg in this direction, on this day
    /// type. Safe to send on to CMRL.
    pub route_code: String,
    /// Line short name, e.g. "Blue". Display only — never used for routing.
    pub line_name: String,
    /// Stops traversed on this leg.
    pub total_no_of_stops: u16,
    /// Other routes serving this exact leg with the same stop count, on the
    /// same line. Maps onto `RouteDetails.alternateRouteIds` in the backend,
    /// which uses it to track live vehicles across every serving route.
    pub alternate_route_codes: Vec<String>,
    /// How the rider joined this leg. `None` on the first leg.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interchange_type: Option<InterchangeType>,
}

/// Outcome of a hopper query.
#[derive(Debug, Clone)]
pub enum HopLookup {
    /// The stop code is not in this feed.
    UnknownStop(String),
    /// Source and destination are the same station.
    SameStop,
    /// Both stops exist but no service connects them on this day type.
    NoPath,
    Found {
        legs: Vec<Vec<HopLeg>>,
    },
}

// ── Artifact ───────────────────────────────────────────────────────────────

/// `metro_hops.json` — a map of `gtfs_id` → feed, written by the Nandi planner.
pub type Artifact = HashMap<String, ArtifactFeed>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactFeed {
    /// RFC 3339, UTC. Drives the staleness warning — the only artifact field
    /// this service computes anything from.
    ///
    /// The planner also writes `feedVersion`, `minDailyTrips` and
    /// `excludedRoutes`. They are deliberately **not** deserialised here: this
    /// service looks journeys up, it does not reason about how they were
    /// planned. Echoing them into a log would add nothing that
    /// `jq '.<feed>' metro_hops.json` does not already give you, straight from
    /// the artifact that is actually loaded. Serde ignores unknown keys, so
    /// they stay in the file and stay inspectable.
    pub generated_at: String,
    /// Index position is the dense `StopId` used by `edges` and `chains` keys.
    pub stops: Vec<ArtifactStop>,
    /// `calendar_dates.txt` overrides: `"YYYYMMDD"` → day type. Consulted
    /// before the weekday, so a public holiday running a Sunday timetable
    /// resolves correctly.
    #[serde(default)]
    pub calendar_dates: HashMap<String, String>,
    /// Keyed `"WD"` / `"SAT"` / `"SUN"`.
    pub day_types: HashMap<String, ArtifactDayType>,
}

#[derive(Debug, Deserialize)]
pub struct ArtifactStop {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct ArtifactDayType {
    /// Representative service date the graph was planned for, `YYYYMMDD`.
    pub date: String,
    /// Routes that survived the calendar filter on that date.
    #[serde(default)]
    pub routes: u32,
    /// Every distinct seated ride. Position is the `EdgeId` used in `chains`.
    pub edges: Vec<ArtifactEdge>,
    /// `"src:dest"` → all minimum-degree paths, each a list of `EdgeId`s,
    /// sorted by total stops. Absence means no path.
    pub chains: HashMap<String, Vec<Vec<EdgeId>>>,
}

/// One seated ride. Field names are abbreviated because this is the bulk of
/// the artifact — ~1,100 edges per day type, three day types. The planner also
/// writes `stops`, the full station sequence; it is not needed to serve a
/// lookup, so it is left undeserialised.
#[derive(Debug, Deserialize)]
pub struct ArtifactEdge {
    /// Boarding station.
    pub src: StopId,
    /// Alighting station.
    pub dst: StopId,
    /// Stops traversed.
    pub n: u16,
    /// Line short name.
    pub line: String,
    /// Primary route code.
    pub route: String,
    /// Other routes serving this leg identically.
    #[serde(default)]
    pub alt: Vec<String>,
    /// Boarding platform.
    #[serde(default)]
    pub sp: Option<String>,
    /// Alighting platform.
    #[serde(default)]
    pub dp: Option<String>,
}

/// A seated ride, interned. One entry of the planner's edge table.
#[derive(Debug, Clone)]
struct DirectLeg {
    src: StopId,
    dest: StopId,
    num_stops: u16,
    line: Arc<str>,
    primary_route: Arc<str>,
    alt_routes: Vec<Arc<str>>,
    /// Platforms for `primary_route` specifically. Alternates are same-line and
    /// same-direction by construction, so they share these.
    src_platform: Option<Arc<str>>,
    dest_platform: Option<Arc<str>>,
}

/// Why an index could not be loaded.
#[derive(Debug)]
pub enum HopperLoadError {
    /// Fewer than two stations — nothing to route between.
    TooFewStations(usize),
    /// Above `MAX_STATIONS`; almost certainly a non-metro feed.
    TooManyStations(usize),
    /// The artifact declares no day types.
    NoDayTypes,
    /// A `"src:dest"` key was malformed or referenced a station out of range,
    /// or its paths did not join `src` to `dest` through known edges.
    BadKey(String),
    /// An edge referenced a station out of range or went nowhere.
    BadEdge(usize),
}

impl std::fmt::Display for HopperLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooFewStations(n) => write!(f, "only {} station(s); need at least 2", n),
            Self::TooManyStations(n) => write!(
                f,
                "{} stations exceeds the {} limit — refusing to materialise {} pairs",
                n,
                MAX_STATIONS,
                n.saturating_mul(*n)
            ),
            Self::NoDayTypes => write!(f, "artifact declares no day types"),
            Self::BadKey(k) => write!(f, "malformed or out-of-range pair key: {:?}", k),
            Self::BadEdge(i) => write!(f, "edge {} has an out-of-range or looping station", i),
        }
    }
}

/// Interning pool for the strings that repeat heavily across an edge table:
/// ~1,100 Chennai edges share 36 route codes and 3 line names.
#[derive(Default)]
struct Pool(HashMap<String, Arc<str>>);

impl Pool {
    fn intern(&mut self, s: &str) -> Arc<str> {
        if let Some(a) = self.0.get(s) {
            return a.clone();
        }
        let a: Arc<str> = Arc::from(s);
        self.0.insert(s.to_string(), a.clone());
        a
    }
}

/// The precomputed all-pairs journey index for one feed on one day type.
pub struct HopperIndex {
    gtfs_id: String,
    day_type: String,
    /// Representative service date this graph was planned for, `YYYYMMDD`.
    service_date: String,
    n: usize,
    ids: HashMap<Arc<str>, StopId>,
    codes: Vec<Arc<str>>,
    names: Vec<Arc<str>>,
    edges: Vec<DirectLeg>,
    /// Ordered station pair → minimum-degree paths as edge ids, sorted by
    /// total stops.
    chains: HashMap<(StopId, StopId), Vec<Vec<EdgeId>>>,
}

impl HopperIndex {
    pub fn gtfs_id(&self) -> &str {
        &self.gtfs_id
    }

    pub fn day_type(&self) -> &str {
        &self.day_type
    }

    pub fn service_date(&self) -> &str {
        &self.service_date
    }

    pub fn station_count(&self) -> usize {
        self.n
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Number of ordered pairs with a journey stored.
    pub fn journey_count(&self) -> usize {
        self.chains.len()
    }

    /// Number of paths stored across all ordered pairs.
    pub fn path_count(&self) -> usize {
        self.chains.values().map(Vec::len).sum()
    }

    /// Approximate heap footprint of the chain map, excluding allocator and
    /// hash-table control overhead, for startup logging.
    pub fn chain_bytes(&self) -> usize {
        self.chains.capacity() * std::mem::size_of::<((StopId, StopId), Vec<Vec<EdgeId>>)>()
            + self
                .chains
                .values()
                .map(|paths| {
                    paths.capacity() * std::mem::size_of::<Vec<StopId>>()
                        + paths
                            .iter()
                            .map(|chain| chain.capacity() * std::mem::size_of::<EdgeId>())
                            .sum::<usize>()
                })
                .sum::<usize>()
    }

    /// Every stop code in the index, for diagnostics.
    pub fn stop_codes(&self) -> impl Iterator<Item = &str> {
        self.codes.iter().map(|c| c.as_ref())
    }

    // ── Load ───────────────────────────────────────────────────────────

    /// Load one day type's edge and chain maps.
    ///
    /// The station list is shared across day types, so it is passed in already
    /// interned rather than rebuilt three times.
    fn from_day_type(
        gtfs_id: &str,
        day_type: &str,
        dt: &ArtifactDayType,
        codes: &[Arc<str>],
        names: &[Arc<str>],
        ids: &HashMap<Arc<str>, StopId>,
    ) -> Result<Self, HopperLoadError> {
        let n = codes.len();

        let parse_pair = |key: &str| -> Result<(StopId, StopId), HopperLoadError> {
            let bad = || HopperLoadError::BadKey(key.to_string());
            let (a, b) = key.split_once(':').ok_or_else(bad)?;
            let a: usize = a.parse().map_err(|_| bad())?;
            let b: usize = b.parse().map_err(|_| bad())?;
            if a >= n || b >= n {
                return Err(bad());
            }
            Ok((a as StopId, b as StopId))
        };

        let mut pool = Pool::default();
        let mut edges: Vec<DirectLeg> = Vec::with_capacity(dt.edges.len());
        for (i, e) in dt.edges.iter().enumerate() {
            if e.src as usize >= n || e.dst as usize >= n || e.src == e.dst {
                return Err(HopperLoadError::BadEdge(i));
            }
            edges.push(DirectLeg {
                src: e.src,
                dest: e.dst,
                num_stops: e.n,
                line: pool.intern(&e.line),
                primary_route: pool.intern(&e.route),
                alt_routes: e.alt.iter().map(|r| pool.intern(r)).collect(),
                src_platform: e.sp.as_deref().map(|p| pool.intern(p)),
                dest_platform: e.dp.as_deref().map(|p| pool.intern(p)),
            });
        }

        // Validate every path once here so `lookup` can index without checks:
        // non-empty, known edges only, and legs that join src to dest.
        let joins = |src: StopId, dest: StopId, path: &[EdgeId]| -> bool {
            let mut at = src;
            for &e in path {
                match edges.get(e as usize) {
                    Some(edge) if edge.src == at => at = edge.dest,
                    _ => return false,
                }
            }
            !path.is_empty() && at == dest
        };
        let mut chains = HashMap::with_capacity(dt.chains.len());
        for (key, paths) in &dt.chains {
            let (src, dest) = parse_pair(key)?;
            if src == dest {
                continue;
            }
            if paths.is_empty() || !paths.iter().all(|p| joins(src, dest, p)) {
                return Err(HopperLoadError::BadKey(key.to_string()));
            }
            chains.insert((src, dest), paths.clone());
        }

        Ok(Self {
            gtfs_id: gtfs_id.to_string(),
            day_type: day_type.to_string(),
            service_date: dt.date.clone(),
            n,
            ids: ids.clone(),
            codes: codes.to_vec(),
            names: names.to_vec(),
            edges,
            chains,
        })
    }

    // ── Query ──────────────────────────────────────────────────────────

    /// Reconstruct all stored paths between two stop codes in planner order.
    pub fn lookup(&self, from: &str, to: &str) -> HopLookup {
        let Some(&src) = self.ids.get(from) else {
            return HopLookup::UnknownStop(from.to_string());
        };
        let Some(&dest) = self.ids.get(to) else {
            return HopLookup::UnknownStop(to.to_string());
        };
        if src == dest {
            return HopLookup::SameStop;
        }

        let Some(chains) = self.chains.get(&(src, dest)) else {
            return HopLookup::NoPath;
        };

        let mut paths = Vec::with_capacity(chains.len());
        for chain in chains {
            let mut legs = Vec::with_capacity(chain.len());
            let mut prev_line: Option<Arc<str>> = None;
            for &e in chain {
                let edge = &self.edges[e as usize];
                let (a, b) = (edge.src, edge.dest);
                let interchange_type = prev_line.as_ref().map(|prev| {
                    if *prev == edge.line {
                        InterchangeType::SameLineChange
                    } else {
                        InterchangeType::LineChange
                    }
                });
                prev_line = Some(edge.line.clone());
                legs.push(HopLeg {
                    src_stop_code: self.codes[a as usize].to_string(),
                    src_stop_name: self.names[a as usize].to_string(),
                    src_platform: edge.src_platform.as_deref().map(str::to_string),
                    dest_stop_code: self.codes[b as usize].to_string(),
                    dest_stop_name: self.names[b as usize].to_string(),
                    dest_platform: edge.dest_platform.as_deref().map(str::to_string),
                    route_code: edge.primary_route.to_string(),
                    line_name: edge.line.to_string(),
                    total_no_of_stops: edge.num_stops,
                    alternate_route_codes: edge.alt_routes.iter().map(|r| r.to_string()).collect(),
                    interchange_type,
                });
            }
            paths.push(legs);
        }
        HopLookup::Found { legs: paths }
    }
}

/// Every day type's index for one feed, plus the calendar that selects between
/// them.
pub struct FeedHoppers {
    gtfs_id: String,
    generated_at: String,
    calendar_dates: HashMap<String, String>,
    by_day_type: HashMap<String, Arc<HopperIndex>>,
}

impl FeedHoppers {
    pub fn gtfs_id(&self) -> &str {
        &self.gtfs_id
    }

    pub fn generated_at(&self) -> &str {
        &self.generated_at
    }

    pub fn day_types(&self) -> impl Iterator<Item = &str> {
        self.by_day_type.keys().map(|k| k.as_str())
    }

    /// Which day type applies on `date`.
    ///
    /// `calendar_dates.txt` wins over the weekday, so a holiday running a
    /// Sunday timetable resolves to `SUN` even on a Tuesday.
    pub fn day_type_for(&self, date: NaiveDate) -> &'static str {
        let normalise = |s: &str| match s {
            "SUN" => "SUN",
            "SAT" => "SAT",
            _ => "WD",
        };
        if let Some(dt) = self.calendar_dates.get(&date.format("%Y%m%d").to_string()) {
            return normalise(dt);
        }
        match date.weekday() {
            Weekday::Sun => "SUN",
            Weekday::Sat => "SAT",
            _ => "WD",
        }
    }

    /// The index to answer a query made on `date`.
    ///
    /// Falls back to `WD`, then to any present day type, rather than failing: a
    /// partial artifact should degrade to a slightly wrong timetable, not to a
    /// feed that appears to have no metro at all. The fallback is warned about
    /// because it means the planner produced an incomplete artifact.
    pub fn index_for(&self, date: NaiveDate) -> Option<&Arc<HopperIndex>> {
        let want = self.day_type_for(date);
        if let Some(idx) = self.by_day_type.get(want) {
            return Some(idx);
        }
        warn!(
            gtfs_id = %self.gtfs_id,
            "service hopper: no {} graph in artifact; falling back", want
        );
        self.by_day_type
            .get("WD")
            .or_else(|| self.by_day_type.values().next())
    }

    /// Build from one feed's artifact entry.
    pub fn from_artifact(gtfs_id: &str, feed: &ArtifactFeed) -> Result<Self, HopperLoadError> {
        let n = feed.stops.len();
        if n < 2 {
            return Err(HopperLoadError::TooFewStations(n));
        }
        if n > MAX_STATIONS {
            return Err(HopperLoadError::TooManyStations(n));
        }
        if feed.day_types.is_empty() {
            return Err(HopperLoadError::NoDayTypes);
        }

        let codes: Vec<Arc<str>> = feed
            .stops
            .iter()
            .map(|s| Arc::from(s.code.as_str()))
            .collect();
        let names: Vec<Arc<str>> = feed
            .stops
            .iter()
            .map(|s| Arc::from(s.name.as_str()))
            .collect();
        let ids: HashMap<Arc<str>, StopId> = codes
            .iter()
            .enumerate()
            .map(|(i, c)| (c.clone(), i as StopId))
            .collect();
        if ids.len() != n {
            warn!(
                gtfs_id = %gtfs_id,
                "service hopper: artifact lists {} stops but only {} distinct codes — \
                 duplicate codes make the later entry unreachable",
                n,
                ids.len()
            );
        }

        let mut by_day_type = HashMap::with_capacity(feed.day_types.len());
        for (day_type, dt) in &feed.day_types {
            let index = HopperIndex::from_day_type(gtfs_id, day_type, dt, &codes, &names, &ids)?;
            by_day_type.insert(day_type.clone(), Arc::new(index));
        }

        Ok(Self {
            gtfs_id: gtfs_id.to_string(),
            generated_at: feed.generated_at.clone(),
            calendar_dates: feed.calendar_dates.clone(),
            by_day_type,
        })
    }

    /// Days since the artifact was generated, if `generated_at` parses.
    fn age_days(&self) -> Option<i64> {
        chrono::DateTime::parse_from_rfc3339(&self.generated_at)
            .ok()
            .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_days())
    }
}

/// Load every feed's hopper indexes from `<dir>/metro_hops.json`.
///
/// A missing or unreadable artifact yields an empty map: the hop endpoint 404s
/// and every other API is unaffected. It is logged at `error` rather than
/// `info` because in any environment using preprocessed data the file should be
/// there, and its absence means the planner did not run.
pub fn load_all(preprocessed_data_dir: &str) -> HashMap<String, Arc<FeedHoppers>> {
    let path = Path::new(preprocessed_data_dir).join(ARTIFACT_FILE);

    if !path.exists() {
        error!(
            "No {} in {} — the metro hop endpoint will 404. Run the Nandi GTFS preprocessor.",
            ARTIFACT_FILE, preprocessed_data_dir
        );
        return HashMap::new();
    }

    let json = match std::fs::read_to_string(&path) {
        Ok(j) => j,
        Err(e) => {
            error!("Failed to read {}: {}", path.display(), e);
            return HashMap::new();
        }
    };

    let artifact: Artifact = match serde_json::from_str(&json) {
        Ok(a) => a,
        Err(e) => {
            error!("Failed to deserialize {}: {}", path.display(), e);
            return HashMap::new();
        }
    };

    let mut out = HashMap::new();
    for (gtfs_id, feed) in &artifact {
        match FeedHoppers::from_artifact(gtfs_id, feed) {
            Ok(hoppers) => {
                let mut day_types: Vec<&str> = hoppers.day_types().collect();
                day_types.sort_unstable();
                let stats: Vec<String> = day_types
                    .iter()
                    .filter_map(|dt| hoppers.by_day_type.get(*dt))
                    .map(|i| {
                        format!(
                            "{} {}: {} edges, {} pairs, {} paths",
                            i.day_type(),
                            i.service_date(),
                            i.edge_count(),
                            i.journey_count(),
                            i.path_count()
                        )
                    })
                    .collect();
                let bytes: usize = hoppers.by_day_type.values().map(|i| i.chain_bytes()).sum();
                let stations = hoppers
                    .by_day_type
                    .values()
                    .next()
                    .map(|i| i.station_count())
                    .unwrap_or(0);
                info!(
                    "service hopper [{}]: {} stations, {} KB, generated {} — {}",
                    gtfs_id,
                    stations,
                    bytes / 1024,
                    hoppers.generated_at(),
                    stats.join(" | ")
                );
                match hoppers.age_days() {
                    Some(days) if days > STALE_AFTER_DAYS => warn!(
                        "service hopper [{}]: artifact is {} days old (generated {}) — the GTFS \
                         preprocessor may have stopped running, and journeys are being served \
                         against a timetable that may no longer apply",
                        gtfs_id, days, hoppers.generated_at
                    ),
                    None => warn!(
                        "service hopper [{}]: unparseable generatedAt {:?}; staleness unchecked",
                        gtfs_id, hoppers.generated_at
                    ),
                    _ => {}
                }
                out.insert(gtfs_id.clone(), Arc::new(hoppers));
            }
            Err(e) => warn!("service hopper [{}]: not loaded — {}", gtfs_id, e),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A three-station toy artifact: A—B on Blue, B—C on Green, with a
    /// through-service A—C only on weekdays.
    fn fixture() -> &'static str {
        r#"{
          "toy": {
            "generatedAt": "2026-08-29T04:00:00Z",
            "feedVersion": "abc123",
            "minDailyTrips": 1,
            "excludedRoutes": ["r9"],
            "stops": [
              {"code": "A", "name": "Alpha"},
              {"code": "B", "name": "Bravo"},
              {"code": "C", "name": "Charlie"}
            ],
            "calendarDates": {"20260101": "SUN"},
            "dayTypes": {
              "WD": {
                "date": "20260831",
                "routes": 2,
                "edges": [
                  {"src": 0, "dst": 1, "n": 1, "line": "Blue",  "route": "r1", "alt": ["r1b"], "sp": "1", "dp": "2", "stops": [0, 1]},
                  {"src": 1, "dst": 2, "n": 1, "line": "Green", "route": "r2", "alt": [],      "sp": "3", "dp": "4", "stops": [1, 2]},
                  {"src": 0, "dst": 2, "n": 2, "line": "Blue",  "route": "r3", "alt": [],      "sp": "1", "dp": "5", "stops": [0, 1, 2]}
                ],
                "chains": {"0:1": [[0]], "1:2": [[1]], "0:2": [[2]]}
              },
              "SUN": {
                "date": "20260830",
                "routes": 2,
                "edges": [
                  {"src": 0, "dst": 1, "n": 1, "line": "Blue",  "route": "r1", "alt": [], "sp": "1", "dp": "2"},
                  {"src": 1, "dst": 2, "n": 1, "line": "Green", "route": "r2", "alt": [], "sp": "3", "dp": "4"}
                ],
                "chains": {"0:1": [[0]], "1:2": [[1]], "0:2": [[0, 1]]}
              }
            }
          }
        }"#
    }

    fn load() -> Arc<FeedHoppers> {
        let artifact: Artifact = serde_json::from_str(fixture()).expect("fixture parses");
        Arc::new(FeedHoppers::from_artifact("toy", &artifact["toy"]).expect("fixture loads"))
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn legs(idx: &HopperIndex, from: &str, to: &str) -> Vec<HopLeg> {
        match idx.lookup(from, to) {
            HopLookup::Found { mut legs } => legs.remove(0),
            other => panic!("expected a journey {}→{}, got {:?}", from, to, other),
        }
    }

    /// The fixture carries `feedVersion`, `minDailyTrips` and `excludedRoutes`,
    /// which this service deliberately does not deserialise. Loading must
    /// ignore them rather than fail — that is what keeps the planner free to
    /// add provenance fields without a coordinated GIMS release.
    #[test]
    fn planner_only_fields_are_ignored_not_rejected() {
        let f = load();
        let mut dts: Vec<&str> = f.day_types().collect();
        dts.sort_unstable();
        assert_eq!(dts, vec!["SUN", "WD"]);
    }

    #[test]
    fn weekday_selects_the_matching_graph() {
        let f = load();
        // 2026-08-31 Monday, 2026-08-29 Saturday, 2026-08-30 Sunday.
        assert_eq!(f.day_type_for(day(2026, 8, 31)), "WD");
        assert_eq!(f.day_type_for(day(2026, 8, 29)), "SAT");
        assert_eq!(f.day_type_for(day(2026, 8, 30)), "SUN");
    }

    #[test]
    fn calendar_date_overrides_the_weekday() {
        let f = load();
        // 2026-01-01 is a Thursday, but the feed declares it a Sunday service.
        assert_eq!(day(2026, 1, 1).weekday(), Weekday::Thu);
        assert_eq!(f.day_type_for(day(2026, 1, 1)), "SUN");
    }

    #[test]
    fn a_missing_day_type_falls_back_rather_than_vanishing() {
        let f = load();
        // The fixture has no SAT graph; Saturday must still answer.
        assert_eq!(f.day_type_for(day(2026, 8, 29)), "SAT");
        assert_eq!(
            f.index_for(day(2026, 8, 29))
                .expect("falls back")
                .day_type(),
            "WD"
        );
    }

    #[test]
    fn through_service_on_a_weekday_is_one_leg() {
        let f = load();
        let idx = f.index_for(day(2026, 8, 31)).unwrap();
        let l = legs(idx, "A", "C");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].route_code, "r3");
        assert_eq!(l[0].total_no_of_stops, 2);
        assert!(l[0].interchange_type.is_none());
    }

    /// The whole reason the planner moved to Nandi: the same pair is one leg on
    /// a weekday and two on a Sunday, because the through-service does not run.
    #[test]
    fn the_same_pair_splits_when_the_through_service_does_not_run() {
        let f = load();
        let sun = f.index_for(day(2026, 8, 30)).unwrap();
        assert_eq!(sun.day_type(), "SUN");
        let l = legs(sun, "A", "C");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].dest_stop_code, "B");
        assert_eq!(l[1].src_stop_code, "B");
        assert_eq!(l[1].interchange_type, Some(InterchangeType::LineChange));
    }

    #[test]
    fn stop_counts_are_available_on_each_leg() {
        let f = load();
        let sun = f.index_for(day(2026, 8, 30)).unwrap();
        match sun.lookup("A", "C") {
            HopLookup::Found { legs } => assert_eq!(
                legs[0]
                    .iter()
                    .map(|leg| leg.total_no_of_stops as u32)
                    .sum::<u32>(),
                2
            ),
            other => panic!("expected a journey, got {:?}", other),
        }
    }

    #[test]
    fn platforms_survive_the_round_trip_and_differ_at_the_interchange() {
        let f = load();
        let sun = f.index_for(day(2026, 8, 30)).unwrap();
        let l = legs(sun, "A", "C");
        assert_eq!(l[0].src_platform.as_deref(), Some("1"));
        // Arrive on 2, leave from 3 — that difference is the change to make.
        assert_eq!(l[0].dest_platform.as_deref(), Some("2"));
        assert_eq!(l[1].src_platform.as_deref(), Some("3"));
        assert_eq!(l[1].dest_platform.as_deref(), Some("4"));
    }

    #[test]
    fn alternates_come_through_unmodified() {
        let f = load();
        let wd = f.index_for(day(2026, 8, 31)).unwrap();
        assert_eq!(legs(wd, "A", "B")[0].alternate_route_codes, vec!["r1b"]);
    }

    #[test]
    fn stop_names_come_from_the_artifact() {
        let f = load();
        let wd = f.index_for(day(2026, 8, 31)).unwrap();
        let l = legs(wd, "A", "B");
        assert_eq!(l[0].src_stop_name, "Alpha");
        assert_eq!(l[0].dest_stop_name, "Bravo");
    }

    #[test]
    fn unknown_stop_is_distinguishable_from_no_path() {
        let f = load();
        let wd = f.index_for(day(2026, 8, 31)).unwrap();
        assert!(matches!(wd.lookup("A", "Z"), HopLookup::UnknownStop(c) if c == "Z"));
        // C→A is never emitted: the graph is directed and the planner found no
        // northbound service.
        assert!(matches!(wd.lookup("C", "A"), HopLookup::NoPath));
    }

    #[test]
    fn same_stop_is_not_a_journey() {
        let f = load();
        let wd = f.index_for(day(2026, 8, 31)).unwrap();
        assert!(matches!(wd.lookup("A", "A"), HopLookup::SameStop));
    }

    #[test]
    fn reachability_is_not_inferred_from_an_empty_chain() {
        let f = load();
        let wd = f.index_for(day(2026, 8, 31)).unwrap();
        // "0:2" has one direct path; "2:0" has no map entry.
        assert!(matches!(wd.lookup("A", "C"), HopLookup::Found { .. }));
        assert!(matches!(wd.lookup("C", "A"), HopLookup::NoPath));
        assert_eq!(wd.journey_count(), 3);
    }

    #[test]
    fn a_bus_sized_artifact_is_refused() {
        let stops: Vec<String> = (0..MAX_STATIONS + 1)
            .map(|i| format!(r#"{{"code":"S{}","name":"S{}"}}"#, i, i))
            .collect();
        let json = format!(
            r#"{{"big":{{"generatedAt":"2026-08-29T04:00:00Z","feedVersion":null,
                "minDailyTrips":1,"stops":[{}],"calendarDates":{{}},
                "dayTypes":{{"WD":{{"date":"20260831","routes":0,"edges":[],"chains":{{}}}}}}}}}}"#,
            stops.join(",")
        );
        let artifact: Artifact = serde_json::from_str(&json).unwrap();
        match FeedHoppers::from_artifact("big", &artifact["big"]) {
            Err(HopperLoadError::TooManyStations(n)) => assert_eq!(n, MAX_STATIONS + 1),
            Err(e) => panic!("wrong error: {}", e),
            Ok(_) => panic!("a {}-station feed must be refused", MAX_STATIONS + 1),
        }
    }

    fn two_station_artifact(edge: &str, chains: &str) -> Artifact {
        let json = format!(
            r#"{{"bad":{{"generatedAt":"2026-08-29T04:00:00Z","feedVersion":null,
              "minDailyTrips":1,
              "stops":[{{"code":"A","name":"A"}},{{"code":"B","name":"B"}}],
              "calendarDates":{{}},
              "dayTypes":{{"WD":{{"date":"20260831","routes":1,
                "edges":[{}],"chains":{}}}}}}}}}"#,
            edge, chains
        );
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn an_out_of_range_edge_is_rejected_not_ignored() {
        for edge in [
            r#"{"src":0,"dst":9,"n":1,"line":"Blue","route":"r1"}"#,
            r#"{"src":1,"dst":1,"n":1,"line":"Blue","route":"r1"}"#,
        ] {
            let artifact = two_station_artifact(edge, "{}");
            assert!(matches!(
                FeedHoppers::from_artifact("bad", &artifact["bad"]),
                Err(HopperLoadError::BadEdge(0))
            ));
        }
    }

    #[test]
    fn a_malformed_pair_key_is_rejected_not_ignored() {
        let edge = r#"{"src":0,"dst":1,"n":1,"line":"Blue","route":"r1"}"#;
        for chains in [r#"{"0:9":[[0]]}"#, r#"{"0-1":[[0]]}"#] {
            let artifact = two_station_artifact(edge, chains);
            assert!(matches!(
                FeedHoppers::from_artifact("bad", &artifact["bad"]),
                Err(HopperLoadError::BadKey(_))
            ));
        }
    }

    #[test]
    fn two_direct_rides_between_one_pair_are_both_served() {
        let mut artifact: Artifact = serde_json::from_str(fixture()).unwrap();
        let feed = artifact.get_mut("toy").unwrap();
        let dt = feed.day_types.get_mut("WD").unwrap();
        dt.edges.push(ArtifactEdge {
            src: 0,
            dst: 2,
            n: 3,
            line: "Green".into(),
            route: "r4".into(),
            alt: vec![],
            sp: None,
            dp: None,
        });
        dt.chains.insert("0:2".into(), vec![vec![2], vec![3]]);
        let hoppers = FeedHoppers::from_artifact("toy", feed).unwrap();
        let idx = hoppers.index_for(day(2026, 8, 31)).unwrap();
        let HopLookup::Found { legs: paths } = idx.lookup("A", "C") else {
            panic!("expected paths");
        };
        let summary: Vec<(usize, &str, u16)> = paths
            .iter()
            .map(|legs| {
                (
                    legs.len(),
                    legs[0].line_name.as_str(),
                    legs[0].total_no_of_stops,
                )
            })
            .collect();
        assert_eq!(summary, vec![(1, "Blue", 2), (1, "Green", 3)]);
    }

    #[test]
    fn multiple_paths_preserve_planner_order_and_reset_interchange_type() {
        let mut artifact: Artifact = serde_json::from_str(fixture()).unwrap();
        let feed = artifact.get_mut("toy").unwrap();
        feed.stops.push(ArtifactStop {
            code: "D".into(),
            name: "Delta".into(),
        });
        let dt = feed.day_types.get_mut("SUN").unwrap();
        for (src, dst, stops) in [(0, 3, 2), (3, 2, 3)] {
            dt.edges.push(ArtifactEdge {
                src,
                dst,
                n: stops,
                line: "Blue".into(),
                route: format!("{}:{}", src, dst),
                alt: vec![],
                sp: None,
                dp: None,
            });
        }
        dt.chains.insert("0:2".into(), vec![vec![0, 1], vec![2, 3]]);
        let hoppers = FeedHoppers::from_artifact("toy", feed).unwrap();
        let idx = hoppers.index_for(day(2026, 8, 30)).unwrap();
        let HopLookup::Found { legs: paths } = idx.lookup("A", "C") else {
            panic!("expected paths");
        };
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0][0].dest_stop_code, "B");
        assert_eq!(paths[1][0].dest_stop_code, "D");
        assert!(paths
            .iter()
            .all(|legs| legs.len() == 2 && legs[0].interchange_type.is_none()));
        assert_eq!(
            paths[1][1].interchange_type,
            Some(InterchangeType::SameLineChange)
        );
        assert_eq!(idx.path_count(), 4);
        let json = serde_json::to_value(&paths).unwrap();
        assert_eq!(json[1][0]["totalNoOfStops"], 2);
        assert!(json[1][0].get("numStops").is_none());
    }

    #[test]
    fn paths_that_are_empty_unknown_or_do_not_join_are_rejected() {
        for paths in [
            vec![],
            vec![vec![]],
            vec![vec![9]],
            vec![vec![0]],
            vec![vec![1, 0]],
        ] {
            let mut artifact: Artifact = serde_json::from_str(fixture()).unwrap();
            let feed = artifact.get_mut("toy").unwrap();
            feed.day_types
                .get_mut("SUN")
                .unwrap()
                .chains
                .insert("0:2".into(), paths);
            assert!(matches!(
                FeedHoppers::from_artifact("toy", feed),
                Err(HopperLoadError::BadKey(_))
            ));
        }
    }

    /// Exercise the artifact the preprocessor actually wrote, when one is
    /// present. Skips silently otherwise so the suite stays runnable anywhere.
    #[test]
    fn real_artifact_loads_if_present() {
        let Ok(dir) = std::env::var("HOPPER_ARTIFACT_DIR") else {
            return;
        };
        let loaded = load_all(&dir);
        assert!(!loaded.is_empty(), "no feeds loaded from {}", dir);

        for (gtfs_id, feed) in &loaded {
            let mut dts: Vec<&str> = feed.day_types().collect();
            dts.sort_unstable();
            assert_eq!(
                dts,
                vec!["SAT", "SUN", "WD"],
                "{} misses a day type",
                gtfs_id
            );

            for dt in dts {
                let idx = &feed.by_day_type[dt];
                assert!(idx.station_count() >= 2);

                // Every stored journey must rebuild into legs that actually
                // chain: leg N's destination is leg N+1's origin, and the ends
                // are the requested pair.
                let codes: Vec<String> = idx.stop_codes().map(str::to_string).collect();
                let mut found = 0usize;
                for a in &codes {
                    for b in &codes {
                        if a == b {
                            continue;
                        }
                        let HopLookup::Found { legs: paths } = idx.lookup(a, b) else {
                            continue;
                        };
                        found += 1;
                        assert!(!paths.is_empty());
                        if gtfs_id == "chennai_metro" && a == "SMM|0233" && b == "SAP|0133" {
                            assert_eq!(
                                paths.len(),
                                1,
                                "Airport transfer detours must be deduplicated"
                            );
                            assert_eq!(paths[0].len(), 2);
                            assert_eq!(paths[0][0].dest_stop_code, "SAL|0231");
                        }
                        let degree = paths[0].len();
                        let mut previous_total = 0;
                        if gtfs_id == "chennai_metro" && a == "SAL|0231" && b == "SCC|0201" {
                            let summary: Vec<(usize, &str, u16)> = paths
                                .iter()
                                .map(|legs| {
                                    (
                                        legs.len(),
                                        legs[0].line_name.as_str(),
                                        legs[0].total_no_of_stops,
                                    )
                                })
                                .collect();
                            assert_eq!(summary, vec![(1, "Blue", 10), (1, "Green", 15)]);
                        }
                        let mut rides = std::collections::HashSet::new();
                        for legs in paths {
                            assert!(rides.insert(
                                legs.iter()
                                    .map(|leg| {
                                        (
                                            leg.src_stop_code.clone(),
                                            leg.dest_stop_code.clone(),
                                            leg.route_code.clone(),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            ));
                            assert_eq!(legs.len(), degree);
                            assert!(!legs.is_empty());
                            assert_eq!(&legs[0].src_stop_code, a);
                            assert_eq!(&legs[legs.len() - 1].dest_stop_code, b);
                            let total = legs
                                .iter()
                                .map(|leg| leg.total_no_of_stops as u32)
                                .sum::<u32>();
                            assert!(total >= previous_total);
                            previous_total = total;
                            for w in legs.windows(2) {
                                assert_eq!(w[0].dest_stop_code, w[1].src_stop_code);
                            }
                            assert!(legs[0].interchange_type.is_none());
                            assert!(legs[1..].iter().all(|l| l.interchange_type.is_some()));
                        }
                    }
                }
                assert_eq!(found, idx.journey_count(), "{} {}", gtfs_id, dt);
            }
        }
    }
}
