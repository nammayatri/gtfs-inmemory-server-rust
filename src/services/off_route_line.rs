//! A stop a route's map line passes by: "the bus may not stop here"
//! (docs/gtfs-editor.md section 27).
//!
//! Measured once per feed version, stored, and read by every pod that serves:
//!
//! ```text
//! each pod's version poll, after it has loaded the versions that moved
//!   runs()                      one read: every feed's last run
//!   a DB feed it serves on a version no run covers
//!     -> claim(): the feed's advisory lock - one pod measures, the rest skip
//!     -> measure() over the lines and stops this pod serves
//!     -> Claim::store(): gtfs_off_route_line rows, gtfs_off_route_line_run.revision
//!        moved only when the set of flagged stops changed
//!   a DB feed whose stored revision is not the one it serves
//!     -> read_stored() + apply(), patched in place
//! a reload of a DB feed         -> read_stored() + apply()
//! ```
//!
//! Nothing here bumps `gtfs_feed.version`: the flags are derived from what was
//! committed, not an edit, so storing them must not reload every pod's feed or
//! fire the release webhooks.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sqlx::{PgPool, Row};

use crate::models::{GTFSRouteData, NandiRoutesRes, OFF_ROUTE_LINE_M};
use crate::services::osrm::{self, seg_dist, Planar};

/// One stop a route's line passes more than [`OFF_ROUTE_LINE_M`] from.
#[derive(Debug, Clone, PartialEq)]
pub struct OffLineStop {
    pub route_code: String,
    pub stop_code: String,
    /// How far the line passes from the stop, in metres.
    pub distance_m: f64,
}

/// What one feed's measurement found.
#[derive(Debug, Default)]
pub struct Measured {
    pub stops: Vec<OffLineStop>,
    /// Bus routes with a line, every one of which was measured.
    pub routes_checked: usize,
}

/// The flagged stops of one feed: route code -> stop codes.
pub type OffLineSet = HashMap<String, HashSet<String>>;

/// A route's line flattened to metres once, then asked about each of its stops.
struct RouteLine {
    plane: Planar,
    xy: Vec<(f64, f64)>,
    /// The segment the last stop was near. A route's stops come in order along
    /// its line, so the next one is usually a few segments on: looking from
    /// here first makes a route's stops cost about one pass of its line.
    hint: usize,
}

impl RouteLine {
    /// `None` for a line of fewer than two points, which passes nothing.
    fn new(line: &[(f64, f64)]) -> Option<Self> {
        let &(lat, lon) = line.first()?;
        if line.len() < 2 {
            return None;
        }
        let plane = Planar::around(lat, lon);
        Some(Self {
            xy: line.iter().map(|&p| plane.xy(p)).collect(),
            plane,
            hint: 0,
        })
    }

    /// `None` when (lat, lon) is within `tol` metres of the line; otherwise how
    /// far the line passes from it. Proving a stop off takes every segment, so
    /// its distance costs nothing more.
    fn distance_beyond(&mut self, lat: f64, lon: f64, tol: f64) -> Option<f64> {
        let p = self.plane.xy((lat, lon));
        let segments = self.xy.len() - 1;
        let mut nearest = f64::INFINITY;
        for k in (self.hint..segments).chain(0..self.hint) {
            let d = seg_dist(p, self.xy[k], self.xy[k + 1]).0;
            if d <= tol {
                self.hint = k;
                return None;
            }
            nearest = nearest.min(d);
        }
        Some(nearest)
    }
}

/// Every bus route's stops of one feed that its map line passes more than
/// [`OFF_ROUTE_LINE_M`] from, routes in code order and each route's stops in
/// its order. A route with no line, or a line of one point, flags nothing:
/// nothing says the bus goes elsewhere. Nor does a rail route: its line is
/// drawn by hand, and a station's point can be a building well off the track.
pub fn measure(route_data: &GTFSRouteData, routes: &HashMap<String, NandiRoutesRes>) -> Measured {
    let mut out = Measured::default();
    let mut codes: Vec<&String> = route_data.by_route.keys().collect();
    codes.sort();
    for route_code in codes {
        let Some(mut line) = routes
            .get(route_code)
            .filter(|r| r.mode == "BUS")
            .and_then(|r| r.encoded_polyline.as_deref())
            .and_then(osrm::decode_polyline)
            .and_then(|l| RouteLine::new(&l))
        else {
            continue;
        };
        out.routes_checked += 1;
        let mut seen: HashSet<&str> = HashSet::new();
        for &i in &route_data.by_route[route_code] {
            let Some(m) = route_data.mappings.get(i) else {
                continue;
            };
            let (lat, lon) = (m.stop_point.lat, m.stop_point.lon);
            if let Some(d) = line.distance_beyond(lat, lon, OFF_ROUTE_LINE_M) {
                if seen.insert(&m.stop_code) {
                    out.stops.push(OffLineStop {
                        route_code: route_code.clone(),
                        stop_code: m.stop_code.to_string(),
                        distance_m: d,
                    });
                }
            }
        }
    }
    out
}

/// Flag the mappings of one feed that `off` names and clear the flag on every
/// other. A mapping already right is left as it is, Arc and all.
pub fn apply(route_data: &mut GTFSRouteData, off: &OffLineSet) {
    for m in route_data.mappings.iter_mut() {
        let flagged = off
            .get(&*m.route_code)
            .is_some_and(|stops| stops.contains(&*m.stop_code))
            .then_some(true);
        if m.off_route_line != flagged {
            let mut marked = (**m).clone();
            marked.off_route_line = flagged;
            *m = Arc::new(marked);
        }
    }
}

// ---------------------------------------------------------------- storage

/// When a feed was last measured, and the revision of what was stored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LastRun {
    pub feed_version: i64,
    pub threshold_m: f64,
    pub revision: i64,
}

impl LastRun {
    /// Whether a pod serving `feed_version` has nothing to measure: the flags
    /// were measured on that version or a later one, at today's threshold. A pod
    /// still on an older version never measures over a newer one's flags.
    pub fn covers(&self, feed_version: i64) -> bool {
        self.feed_version >= feed_version && self.threshold_m == OFF_ROUTE_LINE_M
    }
}

/// The flags a feed's pods serve, and the revision they carry.
#[derive(Debug, Default, PartialEq)]
pub struct Stored {
    pub revision: i64,
    pub off: OffLineSet,
}

/// What one version poll did about one feed's flags.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StoreOutcome {
    /// Measured and stored; `revision` moved only when the flagged stops did.
    Stored { changed: bool, revision: i64 },
    /// This version, or a later one, was measured already.
    UpToDate,
    /// Another pod is measuring this feed right now.
    Busy,
}

/// Every measured feed's last run: one small read, every version poll.
pub async fn runs(pool: &PgPool) -> Result<HashMap<String, LastRun>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT gtfs_id, feed_version, threshold_m, revision FROM gtfs_off_route_line_run",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("gtfs_id")?,
                LastRun {
                    feed_version: r.try_get("feed_version")?,
                    threshold_m: r.try_get("threshold_m")?,
                    revision: r.try_get("revision")?,
                },
            ))
        })
        .collect()
}

/// `gtfs_id`'s stored flags and their revision, in one statement so the two
/// agree; `None` when the feed was never measured.
pub async fn read_stored(pool: &PgPool, gtfs_id: &str) -> Result<Option<Stored>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT r.revision, s.route_id, s.stop_code \
         FROM gtfs_off_route_line_run r \
         LEFT JOIN gtfs_off_route_line s ON s.gtfs_id = r.gtfs_id \
         WHERE r.gtfs_id = $1",
    )
    .bind(gtfs_id)
    .fetch_all(pool)
    .await?;
    let mut out: Option<Stored> = None;
    for r in &rows {
        let stored = out.get_or_insert_with(Stored::default);
        stored.revision = r.try_get("revision")?;
        let route: Option<String> = r.try_get("route_id")?;
        let stop: Option<String> = r.try_get("stop_code")?;
        if let (Some(route), Some(stop)) = (route, stop) {
            stored.off.entry(route).or_default().insert(stop);
        }
    }
    Ok(out)
}

/// The right to measure `feed_version` of a feed and store what it finds: an
/// open transaction holding the feed's advisory lock. Every pod reloads a new
/// version within a poll of the others, so the lock is taken before measuring,
/// not after - one pod spends the CPU, the rest find it [`Claimed::Busy`] and
/// pick up what it stores on a later poll. Dropped without [`Claim::store`],
/// it rolls back and stores nothing.
pub struct Claim {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
    gtfs_id: String,
    feed_version: i64,
    /// the run this one replaces; `None` the first time the feed is measured
    last: Option<LastRun>,
}

pub enum Claimed {
    /// boxed: an open transaction is large, and this is made once per feed version
    Yours(Box<Claim>),
    /// measured on this version or a later one already
    UpToDate,
    /// another pod holds the feed's lock
    Busy,
}

/// Claim `feed_version` of `gtfs_id` for measuring, unless another pod holds it
/// or a measurement of this version or a later one is already stored.
pub async fn claim(
    pool: &PgPool,
    gtfs_id: &str,
    feed_version: i64,
) -> Result<Claimed, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let locked: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtext('off-route-line:' || $1))")
            .bind(gtfs_id)
            .fetch_one(&mut *tx)
            .await?;
    if !locked {
        return Ok(Claimed::Busy);
    }
    // read again under the lock: the poll's read may predate another pod's store
    let last = sqlx::query(
        "SELECT feed_version, threshold_m, revision FROM gtfs_off_route_line_run \
         WHERE gtfs_id = $1",
    )
    .bind(gtfs_id)
    .fetch_optional(&mut *tx)
    .await?
    .map(|r| -> Result<LastRun, sqlx::Error> {
        Ok(LastRun {
            feed_version: r.try_get("feed_version")?,
            threshold_m: r.try_get("threshold_m")?,
            revision: r.try_get("revision")?,
        })
    })
    .transpose()?;
    if last.is_some_and(|l| l.covers(feed_version)) {
        return Ok(Claimed::UpToDate);
    }
    Ok(Claimed::Yours(Box::new(Claim {
        tx,
        gtfs_id: gtfs_id.to_string(),
        feed_version,
        last,
    })))
}

impl Claim {
    /// Store `measured` as the feed's flags and release the lock. Returns
    /// whether the set of flagged stops changed, and the revision it now has.
    pub async fn store(
        mut self,
        measured: &Measured,
        took_ms: i64,
        pod_id: &str,
    ) -> Result<(bool, i64), sqlx::Error> {
        let tx = &mut self.tx;
        let before: HashSet<(String, String)> =
            sqlx::query("SELECT route_id, stop_code FROM gtfs_off_route_line WHERE gtfs_id = $1")
                .bind(&self.gtfs_id)
                .fetch_all(&mut **tx)
                .await?
                .iter()
                .map(|r| Ok((r.try_get("route_id")?, r.try_get("stop_code")?)))
                .collect::<Result<_, sqlx::Error>>()?;
        let after: HashSet<(String, String)> = measured
            .stops
            .iter()
            .map(|s| (s.route_code.clone(), s.stop_code.clone()))
            .collect();
        let changed = self.last.is_none() || before != after;

        // the run row first: the stop rows reference it
        let revision: i64 = sqlx::query_scalar(
            "INSERT INTO gtfs_off_route_line_run AS r \
                (gtfs_id, feed_version, threshold_m, revision, routes_checked, stops_flagged, \
                 took_ms, refreshed_at, refreshed_by) \
             VALUES ($1, $2, $3, 1, $4, $5, $6, now(), $7) \
             ON CONFLICT (gtfs_id) DO UPDATE SET \
                feed_version = EXCLUDED.feed_version, threshold_m = EXCLUDED.threshold_m, \
                revision = r.revision + CASE WHEN $8 THEN 1 ELSE 0 END, \
                routes_checked = EXCLUDED.routes_checked, stops_flagged = EXCLUDED.stops_flagged, \
                took_ms = EXCLUDED.took_ms, refreshed_at = now(), refreshed_by = EXCLUDED.refreshed_by \
             RETURNING revision",
        )
        .bind(&self.gtfs_id)
        .bind(self.feed_version)
        .bind(OFF_ROUTE_LINE_M)
        .bind(measured.routes_checked as i32)
        .bind(measured.stops.len() as i32)
        .bind(took_ms)
        .bind(pod_id)
        .bind(changed)
        .fetch_one(&mut **tx)
        .await?;
        // the distances are rewritten even when the set is the same: a stop moved
        // but still off is still worth reading right
        sqlx::query("DELETE FROM gtfs_off_route_line WHERE gtfs_id = $1")
            .bind(&self.gtfs_id)
            .execute(&mut **tx)
            .await?;
        let routes: Vec<&str> = measured
            .stops
            .iter()
            .map(|s| s.route_code.as_str())
            .collect();
        let stops: Vec<&str> = measured
            .stops
            .iter()
            .map(|s| s.stop_code.as_str())
            .collect();
        let distances: Vec<f64> = measured.stops.iter().map(|s| s.distance_m).collect();
        sqlx::query(
            "INSERT INTO gtfs_off_route_line (gtfs_id, route_id, stop_code, distance_m) \
             SELECT $1, * FROM UNNEST($2::text[], $3::text[], $4::float8[])",
        )
        .bind(&self.gtfs_id)
        .bind(&routes)
        .bind(&stops)
        .bind(&distances)
        .execute(&mut **tx)
        .await?;
        self.tx.commit().await?;
        Ok((changed, revision))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{LatLong, RouteStopMapping};

    fn stop_on(route: &str, stop: &str, seq: i32, lat: f64, lon: f64) -> Arc<RouteStopMapping> {
        Arc::new(RouteStopMapping {
            estimated_travel_time_from_previous_stop: None,
            provider_code: Arc::from("GTFS"),
            route_code: Arc::from(route),
            sequence_num: seq,
            stop_code: Arc::from(stop),
            stop_name: Arc::from(stop),
            stop_point: LatLong { lat, lon },
            vehicle_type: Arc::from("BUS"),
            geo_json: None,
            gates: None,
            hindi_name: None,
            regional_name: None,
            platform: None,
            parent_stop_code: None,
            location_type: "0".to_string(),
            cluster_id: None,
            stage_number: None,
            is_stage_stop: None,
            unserviceable: None,
            off_route_line: None,
        })
    }

    fn route_with_line(code: &str, line: Option<&[(f64, f64)]>) -> NandiRoutesRes {
        NandiRoutesRes {
            id: code.to_string(),
            short_name: None,
            long_name: None,
            mode: "BUS".to_string(),
            agency_name: None,
            color: None,
            trip_count: None,
            stop_count: None,
            start_point: None,
            end_point: None,
            service_tier_type: None,
            encoded_polyline: line.map(osrm::encode_polyline),
            route_tag: None,
            is_active: true,
        }
    }

    /// Route "R" over `stops` in that order, its line `line`.
    fn one_route(
        stops: &[(&str, f64, f64)],
        line: Option<&[(f64, f64)]>,
    ) -> (GTFSRouteData, HashMap<String, NandiRoutesRes>) {
        let mut rd = GTFSRouteData::default();
        for (seq, &(code, lat, lon)) in stops.iter().enumerate() {
            let i = rd.mappings.len();
            rd.mappings
                .push(stop_on("R", code, seq as i32 + 1, lat, lon));
            rd.by_route.entry("R".into()).or_default().push(i);
            rd.by_stop.entry(code.into()).or_default().push(i);
        }
        let routes = [("R".to_string(), route_with_line("R", line))]
            .into_iter()
            .collect();
        (rd, routes)
    }

    fn codes(m: &Measured) -> Vec<&str> {
        m.stops.iter().map(|s| s.stop_code.as_str()).collect()
    }

    fn flagged(rd: &GTFSRouteData) -> Vec<String> {
        rd.mappings
            .iter()
            .filter(|m| m.off_route_line == Some(true))
            .map(|m| m.stop_code.to_string())
            .collect()
    }

    fn set_of(m: &Measured) -> OffLineSet {
        let mut off = OffLineSet::new();
        for s in &m.stops {
            off.entry(s.route_code.clone())
                .or_default()
                .insert(s.stop_code.clone());
        }
        off
    }

    // An east-west road 2.2 km long; 0.0001 deg of latitude is about 11 m.
    const ROAD: [(f64, f64); 3] = [(13.0, 80.20), (13.0, 80.21), (13.0, 80.22)];

    #[test]
    fn a_stop_the_line_passes_far_from_is_off_it_and_says_how_far() {
        let (rd, routes) = one_route(
            &[
                ("on_start", 13.0, 80.20),
                ("kerb_33m", 13.0003, 80.205),
                ("service_road_90m", 13.0008, 80.208),
                ("flyover_skip_300m", 13.0027, 80.21),
                ("on_end", 13.0, 80.22),
                ("past_the_end_1km", 13.0, 80.23),
            ],
            Some(&ROAD),
        );
        let m = measure(&rd, &routes);
        assert_eq!(codes(&m), vec!["flyover_skip_300m", "past_the_end_1km"]);
        assert_eq!(m.routes_checked, 1);
        // 0.0027 deg of latitude, and 0.01 deg of longitude at 13 N
        assert!((m.stops[0].distance_m - 298.5).abs() < 1.0, "{:?}", m.stops);
        assert!(
            (m.stops[1].distance_m - 1084.7).abs() < 1.0,
            "{:?}",
            m.stops
        );
    }

    #[test]
    fn a_rail_route_flags_nothing() {
        let (rd, mut routes) = one_route(&[("far", 13.0027, 80.21)], Some(&ROAD));
        routes.get_mut("R").unwrap().mode = "METRO".into();
        let m = measure(&rd, &routes);
        assert!(m.stops.is_empty());
        assert_eq!(m.routes_checked, 0);
    }

    #[test]
    fn stops_out_of_order_along_the_line_are_still_found_on_it() {
        // a loop, or stops listed against the line's direction: the hint wraps
        let (rd, routes) = one_route(
            &[
                ("end", 13.0, 80.22),
                ("start", 13.0, 80.20),
                ("middle", 13.0, 80.21),
            ],
            Some(&ROAD),
        );
        assert!(measure(&rd, &routes).stops.is_empty());
    }

    #[test]
    fn a_route_without_a_line_flags_nothing() {
        let stops = [("on", 13.0, 80.20), ("far", 13.0027, 80.21)];
        let (rd, routes) = one_route(&stops, Some(&ROAD));
        assert_eq!(codes(&measure(&rd, &routes)), vec!["far"]);
        // the line cleared in the editor: nothing says the bus goes elsewhere
        let (rd, no_line) = one_route(&stops, None);
        let m = measure(&rd, &no_line);
        assert!(m.stops.is_empty());
        assert_eq!(m.routes_checked, 0);
        // and a line of one point is no line either
        let (rd, one_point) = one_route(&stops, Some(&ROAD[..1]));
        assert!(measure(&rd, &one_point).stops.is_empty());
    }

    #[test]
    fn a_stop_a_route_calls_at_twice_is_listed_once() {
        let (rd, routes) = one_route(
            &[
                ("far", 13.0027, 80.21),
                ("on", 13.0, 80.21),
                ("far", 13.0027, 80.21),
            ],
            Some(&ROAD),
        );
        assert_eq!(codes(&measure(&rd, &routes)), vec!["far"]);
    }

    #[test]
    fn apply_flags_what_is_named_and_clears_the_rest() {
        let stops = [("on", 13.0, 80.20), ("far", 13.0027, 80.21)];
        let (mut rd, routes) = one_route(&stops, Some(&ROAD));
        let off = set_of(&measure(&rd, &routes));
        apply(&mut rd, &off);
        assert_eq!(flagged(&rd), vec!["far"]);

        // the same flags again leave every mapping as it was
        let before: Vec<_> = rd.mappings.iter().map(Arc::as_ptr).collect();
        apply(&mut rd, &off);
        let after: Vec<_> = rd.mappings.iter().map(Arc::as_ptr).collect();
        assert_eq!(before, after);

        // and flags measured with the line gone clear it
        apply(&mut rd, &OffLineSet::new());
        assert!(flagged(&rd).is_empty());
    }

    #[test]
    fn apply_is_per_route() {
        // the same stop off one route's line and on another's
        let (mut rd, _) = one_route(&[("S", 13.0027, 80.21)], Some(&ROAD));
        let i = rd.mappings.len();
        rd.mappings.push(stop_on("Q", "S", 1, 13.0027, 80.21));
        rd.by_route.entry("Q".into()).or_default().push(i);
        let off: OffLineSet = [("R".to_string(), ["S".to_string()].into_iter().collect())]
            .into_iter()
            .collect();
        apply(&mut rd, &off);
        let by_route: Vec<(&str, Option<bool>)> = rd
            .mappings
            .iter()
            .map(|m| (&*m.route_code, m.off_route_line))
            .collect();
        assert_eq!(by_route, vec![("R", Some(true)), ("Q", None)]);
    }

    #[test]
    fn a_run_covers_its_version_and_later_ones_measured_at_this_threshold() {
        let run = LastRun {
            feed_version: 7,
            threshold_m: OFF_ROUTE_LINE_M,
            revision: 1,
        };
        assert!(run.covers(7));
        assert!(
            run.covers(6),
            "a pod behind never measures over newer flags"
        );
        assert!(!run.covers(8));
        let other_threshold = LastRun {
            threshold_m: OFF_ROUTE_LINE_M + 50.0,
            ..run
        };
        assert!(!other_threshold.covers(7), "a new threshold measures again");
    }
}
