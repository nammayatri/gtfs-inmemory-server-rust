//! A feed whose trips come from the editor's tables (docs/gtfs-editor.md
//! section 16.6), served end to end by GIMS against a real Postgres:
//!
//! - every stop order with a trip is a pattern, in the order of its first trip,
//!   with the preprocessor's public id and that trip's times; a stop order no
//!   trip runs is not served, nor a route with no trips at all;
//! - `tripCount` is every trip of the route, `/example-trip` its first trip,
//!   the route-stop mapping its longest stop order;
//! - `/trip/{id}` is computed from the trip - reference time plus its profile,
//!   or the default timing - with stops numbered 1 to n, for an id with a `/`
//!   in it too, and follows a committed edit on the next reload, uncached;
//! - the feed needs no preprocessed data at all.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. Uses its own feeds and preprocessed data in a temp dir, and removes
//! its rows afterwards; never touches chennai_bus. See
//! scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::crypto;
use gtfs_routes_service::environment::{AppConfig, AppState, OtpConfig, OtpInstance};
use gtfs_routes_service::handlers::routes::create_routes;
use gtfs_routes_service::models::{
    GTFSStop, LatLong, NandiPatternDetails, NandiRoutesRes, NandiStop, NandiTrip,
};
use gtfs_routes_service::services::gtfs_db_source::GtfsDbSource;
use gtfs_routes_service::services::gtfs_timing;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The feed whose trips are in the tables: a metro line with a short turn.
const FEED: &str = "gims_db_trips_test_metro";
/// A preprocessed feed, so GIMS has preprocessed data to boot from at all.
const OTHER: &str = "gims_db_trips_test_other";
/// A trip id as bhubaneswar and sambalpur spell theirs.
const SLASHED: &str = "22B/1-1-OR_trip_1";

async fn local_pool() -> Option<PgPool> {
    let Ok(url) = std::env::var("EDITOR_TEST_DATABASE_URL") else {
        eprintln!("EDITOR_TEST_DATABASE_URL not set; skipping");
        return None;
    };
    assert!(
        url.contains("@127.0.0.1") || url.contains("@localhost"),
        "this test only runs against a local database"
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

fn clear_feed() -> Vec<String> {
    vec![
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_trip WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_service WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
    ]
}

/// R1 calls at S1..S4 (pattern 1, with a profile) and turns back at S3
/// (pattern 2, whose S2 row is a HIDDEN STOP and whose S3 row carries a headsign
/// of its own); R2 has no trips. The first trip by sort key runs the short turn.
fn seed() -> Vec<String> {
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, agency_name, data_source, trips_source, \
                                headsign_source, default_run_s, default_dwell_s) \
         VALUES ('{FEED}', 'GIMS DB trips test metro', 'METROAG', 'db', 'db', 'none', 100, 20)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
         ('{FEED}', 'S1', 'C1', 'ONE', 13.00, 80.2), ('{FEED}', 'S2', 'C2', 'TWO', 13.01, 80.2), \
         ('{FEED}', 'S3', 'C3', 'THREE', 13.02, 80.2), ('{FEED}', 'S4', 'C4', 'FOUR', 13.03, 80.2), \
         ('{FEED}', 'S5', 'C5', 'FIVE', 13.04, 80.2)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type) VALUES \
         ('{FEED}', 'R1', 'BLUE', 'ONE To FOUR', 1), ('{FEED}', 'R2', 'GREEN', 'FOUR To FIVE', 1)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_pattern (gtfs_id, route_id, pattern_key, name) VALUES ('{FEED}', 'R1', 2, 'short turn')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, pattern_key, sequence, stop_id, stop_type, stop_headsign) VALUES \
         ('{FEED}', 'R1', 1, 1, 'S1', 'NEW STOP', NULL), ('{FEED}', 'R1', 1, 2, 'S2', 'NEW STOP', NULL), \
         ('{FEED}', 'R1', 1, 3, 'S3', 'NEW STOP', NULL), ('{FEED}', 'R1', 1, 4, 'S4', 'NEW STOP', NULL), \
         ('{FEED}', 'R1', 2, 1, 'S1', 'NEW STOP', NULL), ('{FEED}', 'R1', 2, 2, 'S2', 'HIDDEN STOP', NULL), \
         ('{FEED}', 'R1', 2, 3, 'S3', 'NEW STOP', 'Towards THREE'), \
         ('{FEED}', 'R2', 1, 1, 'S4', 'NEW STOP', NULL), ('{FEED}', 'R2', 1, 2, 'S5', 'NEW STOP', NULL)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_timing_profile (gtfs_id, route_id, pattern_key, profile_key, arrival_s, departure_s, source) \
         VALUES ('{FEED}', 'R1', 1, 1, '{{0, 200, 400, 700}}', '{{30, 230, 430, 700}}', 'import')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_service (gtfs_id, service_id, monday, tuesday, wednesday, thursday, friday, saturday, sunday) \
         VALUES ('{FEED}', 'WK', true, true, true, true, true, false, false)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_trip (gtfs_id, trip_id, route_id, pattern_key, profile_key, service_id, direction_id, \
                                ref_s, sort_key, source) VALUES \
         ('{FEED}', 'T-TURN', 'R1', 2, NULL, 'WK', 1, 25200, 1, 'import'), \
         ('{FEED}', 'T-PEAK', 'R1', 1, 1, 'WK', 0, 28800, 2, 'import'), \
         ('{FEED}', '{SLASHED}', 'R1', 1, NULL, 'WK', 0, 86100, 3, 'import'), \
         ('{FEED}', 'T-LATE', 'R1', 1, NULL, 'WK', NULL, 90000, 4, 'import')"
    ));
    s
}

// ---------------------------------------------------------------- preprocessed fixture

/// Preprocessed data for OTHER only: the trips feed needs none.
fn write_preprocessed(dir: &Path) {
    let route = NandiRoutesRes {
        id: format!("{OTHER}:X"),
        short_name: Some("X".into()),
        long_name: Some("X".into()),
        mode: "BUS".into(),
        agency_name: None,
        color: None,
        trip_count: Some(1),
        stop_count: Some(2),
        start_point: Some(LatLong {
            lat: 13.0,
            lon: 80.0,
        }),
        end_point: Some(LatLong {
            lat: 13.1,
            lon: 80.0,
        }),
        service_tier_type: None,
        encoded_polyline: None,
    };
    let stop = |i: i32| NandiStop {
        id: format!("{OTHER}:X{i}"),
        code: format!("X{i}"),
        name: format!("X{i}"),
        lat: 13.0 + i as f64 * 0.1,
        lon: 80.0,
        arrival_time: Some(i * 100),
        departure_time: Some(i * 100),
        stop_sequence: Some(i + 1),
        platform_code: None,
        headsign: None,
        stage_number: None,
        is_stage_stop: None,
    };
    let gtfs_stop = |i: i32| GTFSStop {
        id: format!("{OTHER}:X{i}"),
        code: format!("X{i}"),
        name: format!("X{i}"),
        lat: 13.0 + i as f64 * 0.1,
        lon: 80.0,
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
    let pattern = NandiPatternDetails {
        id: format!("{OTHER}:X:0"),
        desc: None,
        route_id: format!("{OTHER}:X"),
        stops: (0..2).map(stop).collect(),
        trips: vec![NandiTrip {
            id: "x1".into(),
            direction: Some(0),
        }],
    };
    let files = [
        (
            "routes.json",
            serde_json::to_vec(&HashMap::from([(OTHER, vec![route])])).unwrap(),
        ),
        (
            "stops.json",
            serde_json::to_vec(&HashMap::from([(OTHER, vec![gtfs_stop(0), gtfs_stop(1)])]))
                .unwrap(),
        ),
        (
            "patterns.json",
            serde_json::to_vec(&HashMap::from([(OTHER, vec![pattern])])).unwrap(),
        ),
    ];
    let mut manifest_files = serde_json::Map::new();
    for (name, bytes) in &files {
        std::fs::write(dir.join(name), bytes).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        manifest_files.insert(
            (*name).to_string(),
            json!({"sha256": format!("{:x}", hasher.finalize()), "count": 1}),
        );
    }
    std::fs::write(
        dir.join("metadata.json"),
        serde_json::to_vec(&json!({
            "version": "db-trips-test", "gtfs_feeds": [OTHER], "files": manifest_files,
        }))
        .unwrap(),
    )
    .unwrap();
}

/// A GIMS that serves exactly this test's feeds: preprocessed data from `dir`
/// (the other feed), the trips feed from the local editor DB, no OTP calls and
/// no vehicle DB.
fn app_config(dir: &Path, db_url: &str) -> AppConfig {
    let instance = OtpInstance {
        url: "http://127.0.0.1:1/nandi".into(),
        identifier: OTHER.into(),
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
        gtfs_db_feeds: vec![FEED.to_string()],
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
    let dir = std::env::temp_dir().join(format!("gims-db-trips-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[actix_web::test]
async fn a_feed_serves_its_trips_from_the_tables() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let url = std::env::var("EDITOR_TEST_DATABASE_URL").unwrap();
    exec(&pool, &seed()).await;

    // ---- the loader, directly: patterns in the order of their first trip
    let db = GtfsDbSource::new(pool.clone(), vec![]);
    let feed = db.load_feed(FEED, &HashMap::new()).await.unwrap();
    let ids: Vec<String> = feed.patterns.iter().map(|p| p.id.clone()).collect();
    assert_eq!(
        ids,
        vec![
            gtfs_timing::pattern_id(FEED, "R1", &["S1", "S3"]),
            gtfs_timing::pattern_id(FEED, "R1", &["S1", "S2", "S3", "S4"]),
        ],
        "the short turn runs first; its hidden stop is not served"
    );
    let trips: Vec<Vec<String>> = feed
        .patterns
        .iter()
        .map(|p| p.trips.iter().map(|t| t.id.clone()).collect())
        .collect();
    assert_eq!(
        trips,
        vec![
            vec!["T-TURN".to_string()],
            vec!["T-PEAK".into(), SLASHED.into(), "T-LATE".into()]
        ]
    );
    // pattern 1's times are its first trip's, T-PEAK's profile from 08:00
    let times: Vec<(i32, i32)> = feed.patterns[1]
        .stops
        .iter()
        .map(|s| (s.arrival_time.unwrap(), s.departure_time.unwrap()))
        .collect();
    assert_eq!(
        times,
        vec![
            (28800, 28830),
            (29000, 29030),
            (29200, 29230),
            (29500, 29500)
        ]
    );
    // R2 has no trips: no route, no pattern
    assert_eq!(feed.routes.len(), 1);
    assert_eq!(feed.routes[0].trip_count, Some(4));
    assert_eq!(feed.routes[0].stop_count, Some(4));
    assert_eq!(feed.trips.as_ref().unwrap().len(), 4);

    // ---- through GIMS
    let dir = temp_dir();
    write_preprocessed(&dir);
    let state = AppState::new(app_config(&dir, &url)).await.unwrap();
    let api = test::init_service(
        App::new()
            .app_data(actix_web::web::Data::new(state.clone()))
            .configure(create_routes),
    )
    .await;
    let get = |path: String| {
        let req = test::TestRequest::get().uri(&path);
        async {
            let resp = test::call_service(&api, req.to_request()).await;
            let status = resp.status().as_u16();
            let body = test::read_body(resp).await;
            (
                status,
                serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
            )
        }
    };

    let (s, routes) = get(format!("/routes/{FEED}")).await;
    assert_eq!(s, 200, "{routes}");
    let listed: Vec<(&str, i64)> = routes
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].as_str().unwrap(), r["tripCount"].as_i64().unwrap()))
        .collect();
    assert_eq!(listed, vec![("R1", 4)]);

    // the example trip is the first trip, on the short turn, on the default
    // timing (100 s runs, 20 s dwell), with the row's own headsign
    let (s, example) = get(format!("/example-trip/{FEED}/R1")).await;
    assert_eq!(s, 200, "{example}");
    assert_eq!(example["tripId"], "T-TURN", "{example}");
    let stops = example["stops"].as_array().unwrap();
    assert_eq!(
        stops
            .iter()
            .map(|s| (
                s["stopCode"].as_str().unwrap(),
                s["scheduledArrival"].as_i64().unwrap(),
                s["scheduledDeparture"].as_i64().unwrap(),
                s["stopPosition"].as_i64().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![("C1", 25200, 25220, 1), ("C3", 25320, 25320, 2)]
    );
    assert_eq!(stops[1]["headsign"], "Towards THREE");

    // the route-stop mapping is its longest stop order
    let (s, mapping) = get(format!("/route-stop-mapping/{FEED}/route/R1")).await;
    assert_eq!(s, 200, "{mapping}");
    assert_eq!(mapping.as_array().unwrap().len(), 4);

    // /trip: computed from the trip, stops numbered from 1
    let (s, trip) = get(format!("/trip/T-PEAK?gtfs_id={FEED}")).await;
    assert_eq!(s, 200, "{trip}");
    assert_eq!(
        (
            &trip["routeId"],
            &trip["routeName"],
            &trip["direction"],
            &trip["source"]
        ),
        (
            &json!(format!("{FEED}:R1")),
            &json!("ONE To FOUR"),
            &json!(0),
            &json!("db")
        )
    );
    let schedule: Vec<(i64, i64, i64)> = trip["schedule"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["sequence"].as_i64().unwrap(),
                s["arrivalTime"].as_i64().unwrap(),
                s["departureTime"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        schedule,
        vec![
            (1, 28800, 28830),
            (2, 29000, 29030),
            (3, 29200, 29230),
            (4, 29500, 29500)
        ]
    );
    assert_eq!(trip["stops"][0]["stopId"], format!("{FEED}:S1"));
    assert_eq!(trip["stops"][0]["stopCode"], "C1");
    // an id with a slash in it, percent-encoded in the path; one past midnight
    let (s, trip) = get(format!(
        "/trip/{}?gtfs_id={FEED}",
        urlencoding::encode(SLASHED)
    ))
    .await;
    assert_eq!(s, 200, "{trip}");
    assert_eq!(trip["tripId"], SLASHED);
    assert_eq!(trip["schedule"][3]["arrivalTime"], 86100 + 3 * 120);
    let (s, _) = get(format!("/trip/NOPE?gtfs_id={FEED}")).await;
    assert_eq!(s, 404);

    // a committed edit (the version moves) is served on the next reload
    exec(
        &pool,
        &[
            format!("UPDATE gtfs_trip SET ref_s = 30600 WHERE gtfs_id = '{FEED}' AND trip_id = 'T-PEAK'"),
            format!("UPDATE gtfs_feed SET version = version + 1 WHERE gtfs_id = '{FEED}'"),
        ],
    )
    .await;
    state.gtfs_service.reload_db_feed(FEED).await.unwrap();
    let (_, trip) = get(format!("/trip/T-PEAK?gtfs_id={FEED}")).await;
    assert_eq!(trip["schedule"][0]["arrivalTime"], 30600, "{trip}");

    exec(&pool, &clear_feed()).await;
    let _ = std::fs::remove_dir_all(&dir);
}
