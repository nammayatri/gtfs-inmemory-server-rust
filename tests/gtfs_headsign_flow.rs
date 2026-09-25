//! A DB feed's stop headsign (docs/gtfs-editor.md section 1, "The headsign"),
//! end to end against a real Postgres holding the editor schema.
//!
//! The loader used to synthesise every headsign from MTC's fare stages. Every
//! feed but chennai_bus serves `headsign: null` today, so moving one into these
//! tables would have turned that null into a stage number for a feed that has no
//! stages - the rider app would start showing a "fare stage 1" on a metro. So
//! this test asserts the three cases that decision comes down to:
//!
//! - a feed with no fare stages serves **null**, with `stage_no` / `stage_name`
//!   left to the defaults migration 0017 gives them,
//! - a row's own `stop_headsign` is served as-is, whatever the feed falls back to,
//! - a `fare_stage` feed still serves the generator's exact spelling.
//!
//! And then the one that matters most: **chennai_bus is unchanged**. Its real
//! rows are read (read-only) straight through the loader and every headsign is
//! compared with what the pre-0017 code would have produced from the same row.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. Uses its own feeds and its own preprocessed data in a temp dir, and
//! removes its rows afterwards; never writes chennai_bus. See
//! scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::crypto;
use gtfs_routes_service::environment::{AppConfig, AppState, OtpConfig, OtpInstance};
use gtfs_routes_service::handlers::routes::create_routes;
use gtfs_routes_service::models::{
    GTFSStop, LatLong, NandiPatternDetails, NandiRoutesRes, NandiStop, NandiTrip,
};
use gtfs_routes_service::services::gtfs_db_source::{
    fare_stage_headsign, GtfsDbSource, TripOverlay,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A feed with no fare stages - what every metro and suburban feed is.
const METRO: &str = "gims_headsign_test_metro";
/// A feed that keeps MTC's fare-stage headsigns, as chennai_bus does.
const BUS: &str = "gims_headsign_test_bus";
const ROUTE: &str = "R1";
/// The one row that carries a headsign of its own.
const OWN_HEADSIGN: &str = "Towards Airport";

async fn local_pool() -> Option<PgPool> {
    let Ok(url) = std::env::var("EDITOR_TEST_DATABASE_URL") else {
        eprintln!("EDITOR_TEST_DATABASE_URL not set; skipping");
        return None;
    };
    assert!(
        url.contains("@127.0.0.1") || url.contains("@localhost"),
        "the headsign test only runs against a local database"
    );
    Some(
        PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .unwrap(),
    )
}

async fn exec(pool: &PgPool, stmts: &[String]) {
    for s in stmts {
        sqlx::query(s)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{s}: {e}"));
    }
}

fn clear_feeds() -> Vec<String> {
    [METRO, BUS]
        .iter()
        .flat_map(|f| {
            [
                format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{f}'"),
                format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{f}'"),
                format!("DELETE FROM gtfs_route WHERE gtfs_id = '{f}'"),
                format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{f}'"),
                format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{f}'"),
            ]
        })
        .collect()
}

/// Both feeds, identical but for how they answer a row with no headsign of its
/// own. The metro feed's rows are inserted **without** `stage_no` / `stage_name`,
/// which is the point of 0017's defaults: a feed with no fare stages should not
/// have to invent them to have a route.
fn seed() -> Vec<String> {
    let mut s = clear_feeds();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, agency_name, data_source, headsign_source) \
         VALUES ('{METRO}', 'GIMS headsign test metro', 'METROAG', 'db', 'none'), \
                ('{BUS}', 'GIMS headsign test bus', 'BUSAG', 'db', 'fare_stage')"
    ));
    for feed in [METRO, BUS] {
        s.push(format!(
            "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
             ('{feed}', 'S1', 'S1', 'AIRPORT', 13.0000, 80.1700), \
             ('{feed}', 'S2', 'S2', 'GUINDY', 13.0100, 80.2100), \
             ('{feed}', 'S3', 'S3', 'CENTRAL', 13.0800, 80.2750)"
        ));
        s.push(format!(
            "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type) \
             VALUES ('{feed}', '{ROUTE}', 'BLUE', 'AIRPORT To CENTRAL', 1)"
        ));
    }
    // No stage_no / stage_name: the columns take migration 0017's defaults.
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stop_headsign) VALUES \
         ('{METRO}', '{ROUTE}', 1, 'S1', 'NEW STOP', NULL), \
         ('{METRO}', '{ROUTE}', 2, 'S2', 'NEW STOP', '{OWN_HEADSIGN}'), \
         ('{METRO}', '{ROUTE}', 3, 'S3', 'NEW STOP', '   ')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop \
         (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name) VALUES \
         ('{BUS}', '{ROUTE}', 1, 'S1', 'NEW STOP', 1, 'AIRPORT'), \
         ('{BUS}', '{ROUTE}', 2, 'S2', 'INTERMEDIATE STOP', 1, 'AIRPORT'), \
         ('{BUS}', '{ROUTE}', 3, 'S3', 'NEW STOP', 2, 'CENTRAL')"
    ));
    s
}

// ---------------------------------------------------------------- preprocessed fixture

/// The preprocessed data both feeds take their trips from. Their stop lists are
/// irrelevant - the DB feed replaces them - but without a pattern per route a DB
/// feed has no trips, and so no stops to carry a headsign at all.
fn write_preprocessed(dir: &Path) {
    let stop = |feed: &str, seq: i32| NandiStop {
        id: format!("{feed}:S{}", seq + 1),
        code: format!("S{}", seq + 1),
        name: "n".into(),
        lat: 13.0,
        lon: 80.2,
        arrival_time: Some(21600 + seq * 135),
        departure_time: Some(21600 + seq * 135 + 15),
        stop_sequence: Some(seq + 1),
        platform_code: None,
        // Null, exactly as the preprocessed data has it for every feed that is
        // not chennai_bus: if the DB feed served a headsign here the test would
        // be reading the fixture back rather than the loader's decision.
        headsign: None,
        stage_number: None,
        is_stage_stop: None,
    };
    let route = |feed: &str| NandiRoutesRes {
        id: format!("{feed}:{ROUTE}"),
        short_name: Some("BLUE".into()),
        long_name: Some("AIRPORT To CENTRAL".into()),
        mode: "METRO".into(),
        agency_name: Some("AG".into()),
        color: None,
        trip_count: Some(1),
        stop_count: Some(3),
        start_point: Some(LatLong {
            lat: 13.0,
            lon: 80.17,
        }),
        end_point: Some(LatLong {
            lat: 13.08,
            lon: 80.275,
        }),
        service_tier_type: None,
        encoded_polyline: None,
    };
    let gtfs_stop = |feed: &str| GTFSStop {
        id: format!("{feed}:S1"),
        code: "S1".into(),
        name: "AIRPORT".into(),
        lat: 13.0,
        lon: 80.17,
        station_id: None,
        location_type: "0".into(),
        platform_code: None,
        cluster: None,
        hindi_name: None,
        regional_name: None,
        info_json: None,
        cluster_id: None,
        description: None,
    };
    let pattern = |feed: &str| NandiPatternDetails {
        id: format!("{feed}:{ROUTE}:0"),
        desc: Some(format!("Pattern for route {ROUTE}")),
        route_id: format!("{feed}:{ROUTE}"),
        stops: (0..3).map(|i| stop(feed, i)).collect(),
        trips: vec![NandiTrip {
            id: format!("{feed}:t1"),
            direction: Some(0),
        }],
    };

    let routes: HashMap<&str, Vec<NandiRoutesRes>> =
        [METRO, BUS].iter().map(|f| (*f, vec![route(f)])).collect();
    let stops: HashMap<&str, Vec<GTFSStop>> = [METRO, BUS]
        .iter()
        .map(|f| (*f, vec![gtfs_stop(f)]))
        .collect();
    let patterns: HashMap<&str, Vec<NandiPatternDetails>> = [METRO, BUS]
        .iter()
        .map(|f| (*f, vec![pattern(f)]))
        .collect();

    let files = [
        ("routes.json", serde_json::to_vec(&routes).unwrap(), 2),
        ("stops.json", serde_json::to_vec(&stops).unwrap(), 2),
        ("patterns.json", serde_json::to_vec(&patterns).unwrap(), 2),
    ];
    let mut manifest_files = serde_json::Map::new();
    for (name, bytes, count) in &files {
        std::fs::write(dir.join(name), bytes).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        manifest_files.insert(
            (*name).to_string(),
            json!({"sha256": format!("{:x}", hasher.finalize()), "count": count}),
        );
    }
    std::fs::write(
        dir.join("metadata.json"),
        serde_json::to_vec(&json!({
            "version": "headsign-test",
            "gtfs_feeds": [METRO, BUS],
            "files": manifest_files,
        }))
        .unwrap(),
    )
    .unwrap();
}

/// A GIMS that serves exactly this test's feeds: preprocessed data from `dir`,
/// both feeds from the local editor DB, no OTP calls and no vehicle DB.
fn app_config(dir: &Path, db_url: &str) -> AppConfig {
    let instance = OtpInstance {
        url: "http://127.0.0.1:1/nandi".into(),
        identifier: METRO.into(),
    };
    AppConfig {
        logger_cfg: shared::tools::logger::LoggerConfig {
            level: shared::tools::logger::LogLevel::WARN,
            log_to_file: false,
        },
        database_url: None,
        internal_database_url: Some(db_url.to_string()),
        db_max_connections: 2,
        db_min_connections: 0,
        db_acquire_timeout: 5,
        db_idle_timeout: 600,
        db_max_lifetime: 3600,
        cache_duration: 3600,
        otp_instances: OtpConfig {
            city_based_instances: vec![],
            gtfs_id_based_instances: vec![instance.clone()],
            default_instance: instance,
        },
        polling_enabled: false,
        polling_interval: 3600,
        process_batch_size: 100,
        port: 0,
        gc_interval: 300,
        max_retries: 1,
        retry_delay: 1,
        rate_limit_delay: 0.1,
        cpu_threshold: 80.0,
        connection_limit: 4,
        http_pool_idle_timeout: 90,
        http_tcp_keepalive: 7200,
        dns_ttl: 300,
        memory_threshold: 1073741824,
        ignored_trip_ids: vec![],
        bhubaneswar_cache_update_interval: 3600,
        bhubaneswar_external_auth: None,
        use_preprocessed_data: true,
        preprocessed_data_dir: dir.display().to_string(),
        phone_number_hash_key: "test".into(),
        enable_schedule_reconciliation: false,
        osrtc_base_url: None,
        osrtc_username: None,
        osrtc_secret_key: None,
        osrtc_station_refresh_interval_hours: 1,
        osrtc_feed_key: None,
        osrm_url: None,
        gtfs_db_feeds: vec![METRO.to_string(), BUS.to_string()],
        gtfs_version_poll_seconds: 3600,
        gtfs_editor_enabled: false,
        gtfs_editor_pomerium_jwks_url: None,
        gtfs_editor_audience: None,
        gtfs_editor_bootstrap_admins: vec![],
        gtfs_editor_totp_key: None,
        gtfs_editor_session_hours: None,
        gtfs_editor_ui_dir: None,
        gtfs_webhooks_enabled: false,
        gtfs_webhook_allowed_hosts: vec![],
        gtfs_pod_id: None,
        gtfs_gps: None,
        is_master: false,
        gtfs_gps_clickhouse_password: None,
    }
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gims-headsign-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every stop's headsign in an `/example-trip` body, in sequence order.
fn headsigns(v: &Value) -> Vec<Value> {
    let mut stops = v["stops"].as_array().cloned().unwrap_or_default();
    stops.sort_by_key(|s| s["stopPosition"].as_i64().unwrap_or(0));
    stops.into_iter().map(|s| s["headsign"].clone()).collect()
}

// ---------------------------------------------------------------- the flow

#[actix_web::test]
async fn a_feed_with_no_fare_stages_serves_no_headsign() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let url = std::env::var("EDITOR_TEST_DATABASE_URL").unwrap();
    exec(&pool, &seed()).await;

    let dir = temp_dir();
    write_preprocessed(&dir);

    let state = AppState::new(app_config(&dir, &url)).await.unwrap();
    let api = test::init_service(
        App::new()
            .app_data(actix_web::web::Data::new(state.clone()))
            .configure(create_routes),
    )
    .await;

    let trip = |feed: &str| {
        let req = test::TestRequest::get().uri(&format!("/example-trip/{feed}/{ROUTE}"));
        async {
            let resp = test::call_service(&api, req.to_request()).await;
            let status = resp.status().as_u16();
            let body = test::read_body(resp).await;
            let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            (status, json)
        }
    };

    // The metro feed: null where the row says nothing, the row's own text where
    // it does, and null again for a row whose headsign is only whitespace.
    let (status, body) = trip(METRO).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        headsigns(&body),
        vec![Value::Null, json!(OWN_HEADSIGN), Value::Null],
        "{body}"
    );

    // The bus feed: the generator's fare-stage spelling, byte for byte.
    let (status, body) = trip(BUS).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        headsigns(&body),
        vec![
            json!("{'fareStageNumber': '1', 'isStageStop': true}"),
            json!("1"),
            json!("{'fareStageNumber': '2', 'isStageStop': true}"),
        ],
        "{body}"
    );

    exec(&pool, &clear_feeds()).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// chennai_bus, read-only: every headsign the loader builds from its real rows is
/// the one the pre-0017 code built from the same row, and its times are the
/// generator's spacing whether or not the overlay carries a timetable.
///
/// Both are the no-change claim this whole migration rests on, asserted against
/// the actual 100k rows rather than a fixture: `headsign_source = 'fare_stage'`
/// with no row carrying a headsign of its own gives back the old synthesis, and
/// chennai_bus's preprocessed times *are* `start + 135·i` to the second, so
/// keeping a route's real timetable gives back the same numbers as computing it.
#[actix_web::test]
async fn chennai_bus_is_served_exactly_as_it_was() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let rows = sqlx::query(
        "SELECT rs.route_id, rs.sequence, rs.stop_type, rs.stage_no, rs.stop_headsign,
                coalesce(s.stop_code, s.stop_id) AS code
           FROM gtfs_route_stop rs
           JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted
           JOIN gtfs_stop s ON s.gtfs_id = rs.gtfs_id AND s.stop_id = rs.stop_id AND NOT s.deleted
          WHERE rs.gtfs_id = 'chennai_bus' AND rs.pattern_key = 1
            AND rs.stop_type IN ('NEW STOP', 'INTERMEDIATE STOP')
          ORDER BY rs.route_id, rs.sequence",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    if rows.is_empty() {
        eprintln!("no chennai_bus rows in this database; skipping");
        return;
    }

    // What the pre-0017 loader would have produced, per route, in order.
    let mut expected: HashMap<String, Vec<String>> = HashMap::new();
    let mut codes: HashMap<String, Vec<String>> = HashMap::new();
    for r in &rows {
        let route_id: String = r.get("route_id");
        assert!(
            r.get::<Option<String>, _>("stop_headsign").is_none(),
            "a chennai_bus row carries its own headsign; this feed is supposed to \
             have none, so the no-change claim below no longer holds"
        );
        expected
            .entry(route_id.clone())
            .or_default()
            .push(fare_stage_headsign(
                r.get::<i32, _>("stage_no"),
                r.get::<&str, _>("stop_type"),
            ));
        codes
            .entry(route_id)
            .or_default()
            .push(r.get::<String, _>("code"));
    }

    const START: i32 = 21600;
    // One overlay per route, so load_feed emits a pattern for every one of them
    // (it drops routes with no trips). `with_schedule` is what the preprocessed
    // data actually holds for chennai_bus - the generator's spacing - and the
    // bare one is what a route the editor has since changed falls back to.
    let overlay = |route_id: &String, schedule: Vec<(String, i32, i32)>| TripOverlay {
        pattern_id: format!("chennai_bus:{route_id}:0"),
        desc: None,
        trips: vec![NandiTrip {
            id: format!("chennai_bus:{route_id}:t"),
            direction: Some(0),
        }],
        trip_count: 1,
        start_seconds: START,
        schedule,
    };
    let bare: HashMap<String, TripOverlay> = expected
        .keys()
        .map(|r| (r.clone(), overlay(r, vec![])))
        .collect();
    let with_schedule: HashMap<String, TripOverlay> = codes
        .iter()
        .map(|(route_id, route_codes)| {
            let last = route_codes.len() - 1;
            let schedule = route_codes
                .iter()
                .enumerate()
                .map(|(i, code)| {
                    let arrival = START + 135 * i as i32;
                    (
                        code.clone(),
                        arrival,
                        if i == last { arrival } else { arrival + 15 },
                    )
                })
                .collect();
            (route_id.clone(), overlay(route_id, schedule))
        })
        .collect();

    let db = GtfsDbSource::new(pool.clone(), vec![]);
    let feed = db.load_feed("chennai_bus", &bare).await.unwrap();
    let kept = db.load_feed("chennai_bus", &with_schedule).await.unwrap();
    assert!(!feed.patterns.is_empty());

    // As JSON, which is both comparable and the thing callers actually receive.
    let by_route = |f: &gtfs_routes_service::services::gtfs_db_source::DbFeed| {
        f.patterns
            .iter()
            .map(|p| (p.route_id.clone(), serde_json::to_value(&p.stops).unwrap()))
            .collect::<HashMap<_, Value>>()
    };
    let (bare_stops, kept_stops) = (by_route(&feed), by_route(&kept));
    assert_eq!(bare_stops.len(), kept_stops.len());

    for (route_key, stops) in &bare_stops {
        let route_id = route_key.trim_start_matches("chennai_bus:");
        let stops = stops.as_array().unwrap();
        let want = &expected[route_id];
        let got: Vec<String> = stops
            .iter()
            .map(|s| s["headsign"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(&got, want, "route {route_id}");

        // The generator's spacing, and the same times either way.
        let last = stops.len() - 1;
        for (i, s) in stops.iter().enumerate() {
            let arrival = START + 135 * i as i32;
            assert_eq!(
                s["arrivalTime"],
                json!(arrival),
                "route {route_id} stop {i}"
            );
            assert_eq!(
                s["departureTime"],
                json!(if i == last { arrival } else { arrival + 15 }),
                "route {route_id} stop {i}"
            );
        }
        assert_eq!(
            bare_stops[route_key], kept_stops[route_key],
            "route {route_id}"
        );
    }
}
