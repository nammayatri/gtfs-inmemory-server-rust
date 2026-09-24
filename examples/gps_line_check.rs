//! Real-data check of the GTFS editor's map line from GPS (docs/gtfs-editor.md
//! section 17): runs the very pipeline the endpoint runs, for a few routes,
//! against the real ClickHouse, and writes what it found as GeoJSON to look at.
//!
//! READ-ONLY and small on purpose. The cluster is production and shared, and
//! the credential in nandi's `.env` can write: every statement goes through
//! `services::clickhouse_reader` (readonly=2 on every request, bounded SELECTs
//! only, one at a time, a gap between them). On some network paths answers
//! above ~300 rows stall, so answers stay at 100 rows.
//!
//!   cargo run --example gps_line_check -- \
//!       --env /path/to/nandi/gtfs-v3/.env \
//!       --db postgres://postgres@127.0.0.1:55433/gims_gps_test \
//!       --osrm http://127.0.0.1:5055 --out /tmp/gps 115 117
//!
//! It reads as the endpoint does - a 14-day lookback, today first, stopping at
//! `enough_bus_days`, inside the same time budget - and prints each statement's
//! time. `--days N` and `--enough N` narrow it.
//!
//! The credentials are read from the `.env` file into this process only; they
//! are never printed. `--schema` prints the table's column names and types.

use gtfs_routes_service::editor::{gps_line, service as svc};
use gtfs_routes_service::services::clickhouse_reader::{
    ClickHouseError, ClickHouseReader, ClickHouseSettings, RowSource, DEFAULT_MIN_GAP,
};
use gtfs_routes_service::services::osrm;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Args {
    env: String,
    db: String,
    osrm: Option<String>,
    out: String,
    feed: String,
    days: u32,
    /// `enough_bus_days`; the deployment's default when not given.
    enough: Option<u32>,
    schema: bool,
    /// Re-snap the GPS path saved in an earlier output, without ClickHouse.
    rematch: Option<String>,
    /// Run the suggestion through the stops for every route of the feed
    /// (local Postgres and local OSRM only) and count why it fails.
    sweep: bool,
    routes: Vec<String>,
}

fn args() -> Args {
    let mut a = Args {
        env: String::new(),
        db: "postgres://postgres@127.0.0.1:55433/gims_gps_test".into(),
        osrm: None,
        out: ".".into(),
        feed: "chennai_bus".into(),
        days: 14,
        enough: None,
        schema: false,
        rematch: None,
        sweep: false,
        routes: vec![],
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--env" => a.env = it.next().expect("--env FILE"),
            "--db" => a.db = it.next().expect("--db URL"),
            "--osrm" => a.osrm = it.next(),
            "--out" => a.out = it.next().expect("--out DIR"),
            "--feed" => a.feed = it.next().expect("--feed ID"),
            "--days" => a.days = it.next().expect("--days N").parse().expect("a number"),
            "--enough" => {
                a.enough = Some(it.next().expect("--enough N").parse().expect("a number"))
            }
            "--schema" => a.schema = true,
            "--rematch" => a.rematch = it.next(),
            "--sweep" => a.sweep = true,
            other => a.routes.push(other.to_string()),
        }
    }
    assert!(
        a.days <= 14,
        "keep the real-data check small: --days 14 at most"
    );
    assert!(
        a.enough.unwrap_or(12) <= 12,
        "keep the real-data check small: --enough 12 at most"
    );
    assert!(
        a.db.contains("@127.0.0.1") || a.db.contains("@localhost"),
        "only a local Postgres"
    );
    a
}

fn env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_default()
        .trim()
        .trim_matches(|c| c == '\'' || c == '"')
        .to_string()
}

/// The HTTP endpoint for the .env's ClickHouse, the way nandi's ch_client.py
/// works it out: CLICKHOUSE_PORT names the native port, HTTP is beside it.
fn clickhouse_settings() -> ClickHouseSettings {
    let host = env("CLICKHOUSE_HOST");
    let user = env("CLICKHOUSE_USER");
    assert!(
        !host.is_empty() && !user.is_empty(),
        "CLICKHOUSE_HOST and CLICKHOUSE_USER are not set"
    );
    let configured = env("CLICKHOUSE_PORT");
    let port: u16 = match configured.as_str() {
        "9000" => 8123,
        "9440" => 8443,
        "" => 8123,
        p => p.parse().expect("CLICKHOUSE_PORT is a number"),
    };
    let secure = matches!(
        env("CLICKHOUSE_SECURE").to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    );
    let scheme = if secure && (port == 8443 || port == 443) {
        "https"
    } else {
        "http"
    };
    ClickHouseSettings {
        url: format!("{scheme}://{host}:{port}"),
        user,
        password: Some(env("CLICKHOUSE_PASSWORD")).filter(|p| !p.is_empty()),
        query_timeout: Duration::from_secs(60),
        min_gap: DEFAULT_MIN_GAP,
    }
}

fn line_feature(points: &[(f64, f64)], props: Value) -> Value {
    json!({
        "type": "Feature",
        "properties": props,
        "geometry": {"type": "LineString", "coordinates": points.iter().map(|(la, lo)| json!([lo, la])).collect::<Vec<_>>()},
    })
}

fn point_feature(lat: f64, lon: f64, props: Value) -> Value {
    json!({"type": "Feature", "properties": props, "geometry": {"type": "Point", "coordinates": [lon, lat]}})
}

fn near_line(line: &[(f64, f64)], lat: f64, lon: f64) -> f64 {
    let pl = osrm::Planar::around(lat, lon);
    let p = pl.xy((lat, lon));
    line.windows(2)
        .map(|w| osrm::seg_dist(p, pl.xy(w[0]), pl.xy(w[1])).0)
        .fold(f64::MAX, f64::min)
}

/// The reader, timing each statement: (what, seconds, rows, timed out).
struct Timed {
    inner: ClickHouseReader,
    log: Mutex<Vec<(String, f64, usize, bool)>>,
}

#[async_trait::async_trait]
impl RowSource for Timed {
    async fn rows(&self, sql: &str, limit: Duration) -> Result<Vec<Vec<String>>, ClickHouseError> {
        let t = Instant::now();
        let out = self.inner.query_within(sql, limit).await;
        let what = if sql.contains("arrayStringConcat") {
            "tracks"
        } else {
            "bus-days"
        };
        let span = sql
            .split("toDateTime(")
            .skip(1)
            .take(2)
            .map(|p| p.split(')').next().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("..");
        self.log.lock().unwrap().push((
            format!("{what} {span} (limit {} s)", limit.as_secs()),
            t.elapsed().as_secs_f64(),
            out.as_ref().map(|r| r.len()).unwrap_or(0),
            matches!(out, Err(ClickHouseError::Timeout(_))),
        ));
        out
    }

    fn sent(&self) -> u64 {
        self.inner.queries()
    }
}

#[tokio::main]
async fn main() {
    let a = args();
    if let Some(file) = &a.rematch {
        let fc: Value =
            serde_json::from_str(&std::fs::read_to_string(file).expect("read")).expect("json");
        let raw: Vec<(f64, f64)> = fc["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["properties"]["name"] == "gps path (before OSRM)")
            .expect("a gps path")["geometry"]["coordinates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c[1].as_f64().unwrap(), c[0].as_f64().unwrap()))
            .collect();
        let m = osrm::match_path(
            &reqwest::Client::new(),
            a.osrm.as_deref(),
            &raw,
            &osrm::MatchOptions::default(),
            Duration::from_secs(60),
        )
        .await;
        println!(
            "rematch: {} -> {} points, {:.2} km -> {:.2} km, {:?} share {:.3}, {} chunks ({} failed), {} detours skipped, error {:?}",
            raw.len(),
            m.points.len(),
            osrm::length_m(&raw) / 1000.0,
            osrm::length_m(&m.points) / 1000.0,
            m.quality,
            m.matched_share,
            m.chunks,
            m.chunks_failed,
            m.detours,
            m.error
        );
        let out = format!("{file}.rematch.geojson");
        std::fs::write(
            &out,
            serde_json::to_string(&json!({"type": "FeatureCollection", "features": [
                line_feature(&m.points, json!({"name": "gps line (snapped)"})),
                line_feature(&raw, json!({"name": "gps path (before OSRM)"})),
            ]}))
            .unwrap(),
        )
        .unwrap();
        println!("wrote {out}");
        return;
    }
    if a.sweep {
        let base = a.osrm.clone().expect("--sweep needs --osrm");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&a.db)
            .await
            .expect("the local Postgres");
        let ids: Vec<(String,)> = sqlx::query_as(
            "SELECT route_id FROM gtfs_route WHERE gtfs_id = $1 AND NOT deleted ORDER BY route_id",
        )
        .bind(&a.feed)
        .fetch_all(&pool)
        .await
        .expect("routes");
        let http = reqwest::Client::new();
        let mut tally: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for (route_id,) in &ids {
            let mut conn = pool.acquire().await.expect("a connection");
            let detail = svc::route_detail(&mut conn, &a.feed, route_id)
                .await
                .expect("route");
            drop(conn);
            let wps = svc::polyline_waypoint_rows(&detail);
            let pts: Vec<(f64, f64)> = wps.iter().map(|w| (w.lat, w.lon)).collect();
            let key = match osrm::route_through(&http, &base, &pts, Duration::from_secs(25)).await {
                Ok(_) => "ok".to_string(),
                Err(f) => {
                    let at = |i: Option<usize>| {
                        i.and_then(|i| wps.get(i)).map(|w| {
                            format!(
                                "{} {}",
                                w.id.clone().unwrap_or_default(),
                                w.name.clone().unwrap_or_default()
                            )
                        })
                    };
                    let example = format!(
                        "{route_id} {}: leg {:?} {:?} -> {:?}; waypoint {:?} {:?}; {}",
                        detail["short_name"].as_str().unwrap_or(""),
                        f.leg,
                        at(f.leg),
                        at(f.leg.map(|l| l + 1)),
                        f.waypoint,
                        at(f.waypoint),
                        f.message
                    );
                    tally
                        .entry(format!("{:?}", f.reason))
                        .or_default()
                        .push(example);
                    continue;
                }
            };
            tally.entry(key).or_default().push(route_id.clone());
        }
        println!(
            "stops-only suggestion over {} routes of {}:",
            ids.len(),
            a.feed
        );
        for (k, v) in &tally {
            println!("  {k}: {}", v.len());
            if k != "ok" {
                for e in v.iter().take(6) {
                    println!("     {e}");
                }
            }
        }
        return;
    }
    if !a.env.is_empty() {
        dotenv::from_path(&a.env).expect("the .env file can be read");
    }
    let ch = clickhouse_settings();
    let (user, password) = (ch.user.clone(), ch.password.clone().unwrap_or_default());
    // anything printed goes through this first
    let scrub = |s: String| -> String {
        let mut s = s;
        if !password.is_empty() {
            s = s.replace(&password, "<redacted>");
        }
        s.replace(&user, "<user>")
    };

    if a.schema {
        let reader = ClickHouseReader::new(ch.clone()).expect("reader");
        let rows = reader
            .query("SELECT name, type FROM system.columns WHERE database = 'atlas_kafka' AND table = 'amnex_direct_data' ORDER BY position LIMIT 100")
            .await;
        match rows {
            Ok(rows) => rows
                .iter()
                .for_each(|r| println!("  {:<24} {}", r[0], r.get(1).cloned().unwrap_or_default())),
            Err(e) => println!("schema: {}", scrub(e.to_string())),
        }
    }

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&a.db)
        .await
        .expect("the local Postgres");
    let http = reqwest::Client::new();
    std::fs::create_dir_all(&a.out).expect("out dir");
    // the deployment's defaults, but for the lookback and a page of 100 rows
    let settings = gps_line::settings_from_config(
        &gtfs_routes_service::environment::GtfsGpsConfig {
            url: ch.url.clone(),
            user: ch.user.clone(),
            table: None,
            days: Some(a.days),
            enough_bus_days: a.enough,
            feeds: Some(vec![a.feed.clone()]),
            max_bus_days: None,
            page_rows: Some(100),
            timeout_seconds: None,
        },
        ch.password.clone(),
    );
    let read_budget = settings.timeout - settings.osrm_reserve;
    let timed = Arc::new(Timed {
        inner: ClickHouseReader::new(settings.clickhouse.clone()).expect("reader"),
        log: Mutex::new(vec![]),
    });
    let gps = gps_line::GpsLine::with_source(settings, timed.clone()).expect("gps settings");

    for route_id in &a.routes {
        let mut conn = pool.acquire().await.expect("a connection");
        let detail = match svc::route_detail(&mut conn, &a.feed, route_id).await {
            Ok(d) => d,
            Err(e) => {
                println!("route {route_id}: {e}");
                continue;
            }
        };
        drop(conn);
        let short = detail["short_name"].as_str().unwrap_or("").to_string();
        let long = detail["long_name"].as_str().unwrap_or("").to_string();
        let stops = gps_line::served_stops(&detail);
        println!(
            "\n== route {route_id} {short} \"{long}\": {} served stops",
            stops.len()
        );

        // what the old suggestion through the stops says now
        let waypoints = svc::polyline_waypoint_rows(&detail);
        let points: Vec<(f64, f64)> = waypoints.iter().map(|w| (w.lat, w.lon)).collect();
        let mut stops_only: Option<Vec<(f64, f64)>> = None;
        if let Some(base) = &a.osrm {
            match osrm::route_through(&http, base, &points, Duration::from_secs(25)).await {
                Ok(line) => {
                    println!(
                        "   stops-only OSRM: ok, {:.1} km through {} waypoints",
                        osrm::length_m(&line.points) / 1000.0,
                        points.len()
                    );
                    stops_only = Some(line.points);
                }
                Err(f) => {
                    let name = |i: Option<usize>| {
                        i.and_then(|i| waypoints.get(i))
                            .map(|w| {
                                format!(
                                    "{} ({}, row {})",
                                    w.name.clone().unwrap_or_default(),
                                    w.id.clone().unwrap_or_default(),
                                    w.sequence.unwrap_or(-1)
                                )
                            })
                            .unwrap_or_default()
                    };
                    println!(
                        "   stops-only OSRM: FAILED reason={:?} code={:?} leg={:?} from={} to={} waypoint={} msg={}",
                        f.reason,
                        f.osrm_code,
                        f.leg,
                        name(f.leg),
                        name(f.leg.map(|l| l + 1)),
                        name(f.waypoint),
                        f.message
                    );
                }
            }
        }

        let q = gps_line::RouteQuery {
            gtfs_id: &a.feed,
            route_id,
            short_name: &short,
            stops: &stops,
        };
        let t = Instant::now();
        timed.log.lock().unwrap().clear();
        let got = gps.gps_path(&q, read_budget).await;
        let read_s = t.elapsed().as_secs_f64();
        for (what, secs, rows, timed_out) in timed.log.lock().unwrap().iter() {
            println!(
                "     {what}: {secs:.2} s, {rows} rows{}",
                if *timed_out {
                    ", STOPPED AT ITS LIMIT"
                } else {
                    ""
                }
            );
        }
        let (raw, evidence) = match got {
            Ok(x) => x,
            Err(gps_line::GpsFailure::NotEnoughRuns(ev)) => {
                println!(
                    "   GPS: not enough runs in {read_s:.1} s: days read {} (stopped: {}), bus-days {}, runs seen {} used {}",
                    ev["days_read"], ev["stopped"], ev["bus_days"], ev["runs_seen"], ev["runs_used"]
                );
                continue;
            }
            Err(e) => {
                println!("   GPS: {}", scrub(format!("{e:?}")));
                continue;
            }
        };
        let t = Instant::now();
        let answer = gps
            .finish(
                a.osrm.as_deref(),
                &q,
                &raw,
                evidence,
                Duration::from_secs(60),
            )
            .await;
        let ev = &answer["evidence"];
        let snapped = osrm::decode_polyline(answer["encoded_polyline"].as_str().unwrap_or(""))
            .unwrap_or_default();
        println!(
            "   GPS: days read {} of {} (stopped: {}), {} bus-days, {} pings ({} points) in {} queries, {read_s:.1} s; runs seen {} used {} by {} buses",
            ev["days_read"], ev["days"], ev["stopped"], ev["bus_days"], ev["pings"], ev["points_read"], ev["queries"], ev["runs_seen"], ev["runs_used"], ev["buses"]
        );
        println!(
            "   line: {} points, {:.1} km, stop coverage {}, matched {} (share {}), OSRM {:.1} s{}",
            ev["points"],
            ev["length_m"].as_f64().unwrap_or(0.0) / 1000.0,
            ev["stop_coverage"],
            ev["matched"],
            ev["matched_share"],
            t.elapsed().as_secs_f64(),
            ev.get("osrm_error")
                .map(|e| format!(", osrm_error {e}"))
                .unwrap_or_default()
        );
        let raw_cov = stops
            .iter()
            .filter(|s| near_line(&raw, s.lat, s.lon) <= 30.0)
            .count() as f64
            / stops.len() as f64;
        println!(
            "   before snapping: {} points, {:.1} km, stop coverage {:.3}",
            raw.len(),
            osrm::length_m(&raw) / 1000.0,
            raw_cov
        );

        let mut features = vec![
            line_feature(
                &snapped,
                json!({"name": "gps line (snapped)", "route_id": route_id, "short_name": short, "evidence": ev, "stroke": "#0b6660", "stroke-width": 4}),
            ),
            line_feature(
                &raw,
                json!({"name": "gps path (before OSRM)", "stroke": "#d97706", "stroke-width": 2}),
            ),
        ];
        if let Some(line) = &stops_only {
            features.push(line_feature(
                line,
                json!({"name": "stops-only OSRM", "stroke": "#7c3aed", "stroke-width": 2}),
            ));
        }
        if let Some(existing) = detail["encoded_polyline"]
            .as_str()
            .and_then(osrm::decode_polyline)
        {
            features.push(line_feature(&existing, json!({"name": format!("current line ({})", detail["polyline_source"]), "stroke": "#6b7280"})));
        }
        for (i, s) in stops.iter().enumerate() {
            let d = near_line(&snapped, s.lat, s.lon);
            features.push(point_feature(
                s.lat,
                s.lon,
                json!({
                    "stop_id": s.stop_id, "order": i + 1, "distance_to_line_m": d.round(),
                    "marker-color": if d <= 30.0 { "#16a34a" } else { "#dc2626" },
                }),
            ));
        }
        let path = format!("{}/gps_line_{route_id}.geojson", a.out);
        std::fs::write(
            &path,
            serde_json::to_string(&json!({"type": "FeatureCollection", "features": features}))
                .unwrap(),
        )
        .expect("write");
        println!("   wrote {path}");
    }
    println!("\nClickHouse statements sent: {}", gps.queries());
}
