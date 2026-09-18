//! What a route has actually been running: its recent trips, or its last known
//! trip when it has not run lately. See `docs/gtfs-editor.md` section 13.
//!
//! # Why the editor needs this
//!
//! Everything else the dashboard shows about a route is what the feed *says*:
//! its stops, their order, its polyline. None of it says whether buses are on
//! the road. An operator deciding whether a route's stop list is worth fixing,
//! or whether a route should exist at all, is asking a different question -
//! when did this last run, and how often - and the answer is in the operational
//! database, beside the waybills.
//!
//! # Where the answer comes from
//!
//! A waybill is a duty a crew actually signed for, so a waybill joined to its
//! scheduled trip is a trip that ran. `bus_schedule_trip_detail` holds the
//! ordinary trips and `bus_schedule_trip_flexi` the flexible ones; both carry
//! `route_number_id`, which is the same id as `gtfs_route.route_id` - no
//! mapping table is needed.
//!
//! Dead trips (a bus moving to or from the depot) are excluded: they are not
//! service, and counting them would say a route ran when nobody could board.
//!
//! # Recent, else last known
//!
//! Inside the window (45 days by default) the trips themselves are returned.
//! A route with nothing in the window falls back to one row - the last trip it
//! ever ran - so a dormant route reads as "last ran on <date>" rather than as
//! an empty answer indistinguishable from a broken query. A route that has
//! never run at all says so.
//!
//! # This is the operational database
//!
//! It is a different database from the editor's, shared with everything else
//! that serves riders, and it is read here on an operator's page view. So every
//! query is bounded three ways - one route, a date window, a row limit - and
//! each route's answer is cached briefly, because a person reloading a route
//! page must not become load on it.

use super::error::{EditorError, EditorResult};
use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, Utc};
use serde_json::{json, Value};
use sqlx::postgres::PgPool;
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// The default window. 45 days is what the nightly GTFS build already treats as
/// "recent" when it decides whether a route's schedule is live
/// (`query_tummoc_db_for_generating_schedule_info_tummoc_db_latest.sql` in
/// nandi), so the dashboard and the feed agree on the word.
pub const DEFAULT_DAYS: i64 = 45;
pub const MAX_DAYS: i64 = 180;
pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 500;

/// Long enough that reloading a route page is free, short enough that an
/// operator watching a route come back on the road sees it within minutes.
const CACHE_TTL: Duration = Duration::from_secs(180);

/// India Standard Time: `waybills.duty_date` is a local calendar day written as
/// text, so "today" and "45 days ago" have to be worked out in Chennai's day,
/// not the server's.
fn ist() -> FixedOffset {
    FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("IST offset is valid")
}

/// `duty_date` is `YYYY-MM-DD` text, so the window bound is text too and the
/// comparison stays on the column's own index.
pub fn window_start(now: DateTime<Utc>, days: i64) -> String {
    (now.with_timezone(&ist()) - ChronoDuration::days(days.max(0)))
        .format("%Y-%m-%d")
        .to_string()
}

pub fn clamp_days(days: Option<i64>) -> i64 {
    days.unwrap_or(DEFAULT_DAYS).clamp(1, MAX_DAYS)
}

pub fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// Trips that ran, newest first. `$1` route id, `$2` the window's first day,
/// `$3` the row cap.
///
/// The two trip tables are unioned rather than joined: a waybill points at one
/// or the other, and `is_flexi` on the waybill is not always set, so asking
/// both and letting the join decide is what `get_routes_served_today` does too.
const RECENT_SQL: &str = r#"
    SELECT duty_date, start_time, end_time, vehicle_no, schedule_no,
           schedule_trip_id, trip_type, is_flexi
      FROM (
        SELECT w.duty_date, bstd.start_time, bstd.end_time, w.vehicle_no,
               w.schedule_no, w.schedule_trip_id::text AS schedule_trip_id,
               bstd.trip_type, false AS is_flexi
          FROM waybills w
          JOIN bus_schedule_trip_detail bstd
            ON w.schedule_trip_id::bigint = bstd.schedule_trip_id
         WHERE bstd.route_number_id::text = $1
           AND w.deleted = false
           AND bstd.trip_type != 'dead-trip'
           AND w.duty_date >= $2
        UNION ALL
        SELECT w.duty_date, bstf.start_time, bstf.end_time, w.vehicle_no,
               w.schedule_no, w.schedule_trip_id::text AS schedule_trip_id,
               bstf.trip_type, true AS is_flexi
          FROM waybills w
          JOIN bus_schedule_trip_flexi bstf
            ON w.schedule_trip_id::bigint = bstf.schedule_trip_id
         WHERE bstf.route_number_id::text = $1
           AND w.deleted = false
           AND bstf.trip_type != 'dead-trip'
           AND w.duty_date >= $2
      ) t
     ORDER BY duty_date DESC, start_time DESC
     LIMIT $3
"#;

/// The one last trip a route ever ran, for a route with nothing in the window.
/// Same shape as `RECENT_SQL` so both fill the same row reader.
const LAST_KNOWN_SQL: &str = r#"
    SELECT duty_date, start_time, end_time, vehicle_no, schedule_no,
           schedule_trip_id, trip_type, is_flexi
      FROM (
        SELECT w.duty_date, bstd.start_time, bstd.end_time, w.vehicle_no,
               w.schedule_no, w.schedule_trip_id::text AS schedule_trip_id,
               bstd.trip_type, false AS is_flexi
          FROM waybills w
          JOIN bus_schedule_trip_detail bstd
            ON w.schedule_trip_id::bigint = bstd.schedule_trip_id
         WHERE bstd.route_number_id::text = $1
           AND w.deleted = false
           AND bstd.trip_type != 'dead-trip'
        UNION ALL
        SELECT w.duty_date, bstf.start_time, bstf.end_time, w.vehicle_no,
               w.schedule_no, w.schedule_trip_id::text AS schedule_trip_id,
               bstf.trip_type, true AS is_flexi
          FROM waybills w
          JOIN bus_schedule_trip_flexi bstf
            ON w.schedule_trip_id::bigint = bstf.schedule_trip_id
         WHERE bstf.route_number_id::text = $1
           AND w.deleted = false
           AND bstf.trip_type != 'dead-trip'
      ) t
     ORDER BY duty_date DESC, start_time DESC
     LIMIT 1
"#;

fn trip_json(r: &sqlx::postgres::PgRow) -> Result<Value, sqlx::Error> {
    Ok(json!({
        "duty_date": r.try_get::<Option<String>, _>("duty_date")?,
        "start_time": r.try_get::<Option<String>, _>("start_time")?,
        "end_time": r.try_get::<Option<String>, _>("end_time")?,
        "vehicle_no": r.try_get::<Option<String>, _>("vehicle_no")?,
        "schedule_no": r.try_get::<Option<String>, _>("schedule_no")?,
        "schedule_trip_id": r.try_get::<Option<String>, _>("schedule_trip_id")?,
        "trip_type": r.try_get::<Option<String>, _>("trip_type")?,
        "is_flexi": r.try_get::<Option<bool>, _>("is_flexi")?.unwrap_or(false),
    }))
}

/// Counts a reader can take in at a glance: how many trips, over how many days,
/// and the busiest day. Pure, so the shaping is tested without a database.
pub fn summarise(trips: &[Value]) -> Value {
    let mut per_day: HashMap<&str, i64> = HashMap::new();
    let mut vehicles: HashMap<&str, ()> = HashMap::new();
    for t in trips {
        if let Some(d) = t["duty_date"].as_str() {
            *per_day.entry(d).or_insert(0) += 1;
        }
        if let Some(v) = t["vehicle_no"].as_str().filter(|v| !v.is_empty()) {
            vehicles.insert(v, ());
        }
    }
    let busiest = per_day.iter().max_by_key(|(d, n)| (**n, **d));
    json!({
        "trips": trips.len(),
        "days_operated": per_day.len(),
        "vehicles": vehicles.len(),
        "busiest_day": busiest.map(|(d, n)| json!({"duty_date": d, "trips": n})),
    })
}

struct Cached {
    value: Value,
    at: SystemTime,
}

/// Per-route answers, held briefly so a page reload is not a query.
#[derive(Default)]
pub struct TripsCache {
    entries: RwLock<HashMap<String, Cached>>,
}

impl TripsCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    async fn get(&self, key: &str) -> Option<Value> {
        let e = self.entries.read().await;
        e.get(key)
            .filter(|c| c.at.elapsed().unwrap_or(CACHE_TTL) < CACHE_TTL)
            .map(|c| c.value.clone())
    }

    async fn put(&self, key: String, value: Value) {
        let mut e = self.entries.write().await;
        // the feed has thousands of routes and this is a page-view cache, so
        // drop everything rather than grow without bound
        if e.len() > 2000 {
            e.clear();
        }
        e.insert(
            key,
            Cached {
                value,
                at: SystemTime::now(),
            },
        );
    }
}

/// Recent trips for one route, or its last known trip.
///
/// `pool` is the operational database. When the deployment has none configured
/// the answer says so rather than failing, because a GIMS without it is a
/// perfectly good editor - it just cannot see the road.
pub async fn for_route(
    pool: Option<&PgPool>,
    cache: &TripsCache,
    route_id: &str,
    days: i64,
    limit: i64,
) -> EditorResult<Value> {
    let Some(pool) = pool else {
        return Ok(json!({
            "route_id": route_id, "days": days, "source": "unavailable",
            "message": "this deployment has no operational database configured, \
                        so what a route has been running is not known here",
            "trips": [], "summary": summarise(&[]),
        }));
    };
    let key = format!("{route_id}|{days}|{limit}");
    if let Some(hit) = cache.get(&key).await {
        debug!(route_id, "route trips cache hit");
        return Ok(hit);
    }

    let from = window_start(Utc::now(), days);
    let rows = sqlx::query(RECENT_SQL)
        .bind(route_id)
        .bind(&from)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(|e| {
            warn!(route_id, "recent trips query failed: {e}");
            EditorError::internal("could not read what this route has been running")
        })?;
    let trips = rows
        .iter()
        .map(trip_json)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| EditorError::internal(format!("trip row: {e}")))?;

    let out = if !trips.is_empty() {
        json!({
            "route_id": route_id, "days": days, "window_from": from,
            "source": "operated", "trips": trips, "summary": summarise(&trips),
            "last_trip": trips.first(),
        })
    } else {
        // nothing in the window: the last trip it ever ran, so a dormant route
        // reads as a date rather than as an empty answer
        let last = sqlx::query(LAST_KNOWN_SQL)
            .bind(route_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| {
                warn!(route_id, "last known trip query failed: {e}");
                EditorError::internal("could not read what this route has been running")
            })?
            .map(|r| trip_json(&r))
            .transpose()
            .map_err(|e| EditorError::internal(format!("trip row: {e}")))?;
        match last {
            Some(t) => json!({
                "route_id": route_id, "days": days, "window_from": from,
                "source": "last_known", "trips": [], "summary": summarise(&[]),
                "last_trip": t,
            }),
            None => json!({
                "route_id": route_id, "days": days, "window_from": from,
                "source": "never", "trips": [], "summary": summarise(&[]),
                "last_trip": Value::Null,
            }),
        }
    };
    cache.put(key, out.clone()).await;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_counted_in_chennais_day() {
        // 2026-09-18 00:30 UTC is already 06:00 on the 18th in Chennai, so a
        // one-day window starts on the 17th, not the 16th
        let now = DateTime::parse_from_rfc3339("2026-09-18T00:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(window_start(now, 1), "2026-09-17");
        assert_eq!(window_start(now, 45), "2026-08-04");
        assert_eq!(window_start(now, 0), "2026-09-18");
    }

    #[test]
    fn a_late_evening_utc_time_is_already_tomorrow_in_chennai() {
        let now = DateTime::parse_from_rfc3339("2026-09-18T19:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // 00:30 on the 19th in IST
        assert_eq!(window_start(now, 0), "2026-09-19");
    }

    #[test]
    fn the_window_and_the_row_cap_are_bounded() {
        assert_eq!(clamp_days(None), DEFAULT_DAYS);
        assert_eq!(clamp_days(Some(0)), 1);
        assert_eq!(clamp_days(Some(-5)), 1);
        assert_eq!(clamp_days(Some(10_000)), MAX_DAYS);
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(10_000)), MAX_LIMIT);
    }

    fn trip(date: &str, vehicle: &str) -> Value {
        json!({"duty_date": date, "vehicle_no": vehicle, "start_time": "06:00"})
    }

    #[test]
    fn the_summary_counts_days_and_vehicles_not_rows() {
        let trips = vec![
            trip("2026-09-18", "TN01A1"),
            trip("2026-09-18", "TN01A2"),
            trip("2026-09-17", "TN01A1"),
        ];
        let s = summarise(&trips);
        assert_eq!(s["trips"], json!(3));
        assert_eq!(s["days_operated"], json!(2));
        assert_eq!(s["vehicles"], json!(2));
        assert_eq!(s["busiest_day"]["duty_date"], json!("2026-09-18"));
        assert_eq!(s["busiest_day"]["trips"], json!(2));
    }

    #[test]
    fn an_empty_summary_says_nothing_ran() {
        let s = summarise(&[]);
        assert_eq!(s["trips"], json!(0));
        assert_eq!(s["days_operated"], json!(0));
        assert_eq!(s["busiest_day"], Value::Null);
    }

    #[test]
    fn a_trip_with_no_vehicle_is_not_counted_as_one() {
        let trips = vec![
            json!({"duty_date": "2026-09-18", "vehicle_no": null}),
            json!({"duty_date": "2026-09-18", "vehicle_no": ""}),
        ];
        assert_eq!(summarise(&trips)["vehicles"], json!(0));
        assert_eq!(summarise(&trips)["trips"], json!(2));
    }

    /// The two queries must stay the same shape: the same reader fills both.
    #[test]
    fn both_queries_select_the_same_columns() {
        let cols = |sql: &str| {
            let head = sql.split("FROM").next().unwrap();
            head.replace("SELECT", "")
                .split(',')
                .map(|c| c.trim().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(cols(RECENT_SQL), cols(LAST_KNOWN_SQL));
    }

    /// Dead trips are depot moves, not service; counting them would say a route
    /// ran when nobody could board it.
    #[test]
    fn dead_trips_are_excluded_from_both_queries() {
        for sql in [RECENT_SQL, LAST_KNOWN_SQL] {
            assert_eq!(sql.matches("trip_type != 'dead-trip'").count(), 2, "{sql}");
            assert_eq!(sql.matches("w.deleted = false").count(), 2);
        }
    }

    /// Every read of the operational database must name one route, and the
    /// recent one must also bound the days and the rows.
    #[test]
    fn every_query_is_bounded() {
        assert!(RECENT_SQL.contains("route_number_id::text = $1"));
        assert!(RECENT_SQL.contains("w.duty_date >= $2"));
        assert!(RECENT_SQL.contains("LIMIT $3"));
        assert!(LAST_KNOWN_SQL.contains("route_number_id::text = $1"));
        assert!(LAST_KNOWN_SQL.contains("LIMIT 1"));
    }
}
