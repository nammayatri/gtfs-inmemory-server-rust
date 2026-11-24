//! A stop a route's map line passes by (docs/gtfs-editor.md section 27), end
//! to end against a real Postgres holding the editor schema
//! (db/gtfs_editor/0001..0037), with two pods on the same feed:
//!
//! - nothing is flagged until a pod's version poll has measured the feed; a
//!   pod that finds another one measuring measures nothing;
//! - the pod that measured serves the flags in the same poll, and the other
//!   picks up what it stored on its next poll, without measuring again; only
//!   the bus route whose line passes the stop by flags it, the stop-wide answer
//!   does not, and `/version` moves;
//! - each feed version is measured once: a second poll stores nothing;
//! - a stop moved onto the line keeps its flag through the reload of that
//!   commit (the stored flags were measured before it), and loses it on the
//!   poll that measures the new version; a pod still on the old version never
//!   measures over that;
//! - none of it moves `gtfs_feed.version`.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. Uses its own feeds and preprocessed data in a temp dir, and removes
//! its rows afterwards; never touches chennai_bus.

use actix_web::{test, App};
use gtfs_routes_service::editor::crypto;
use gtfs_routes_service::environment::{AppConfig, AppState, OtpConfig, OtpInstance};
use gtfs_routes_service::handlers::routes::create_routes;
use gtfs_routes_service::models::{
    GTFSStop, LatLong, NandiPatternDetails, NandiRoutesRes, NandiStop, NandiTrip,
};
use gtfs_routes_service::services::off_route_line::StoreOutcome;
use gtfs_routes_service::services::osrm::encode_polyline;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// The DB feed whose lines are measured.
const FEED: &str = "gims_off_route_line_test_bus";
/// A preprocessed feed, so GIMS has preprocessed data to boot from at all.
const OTHER: &str = "gims_off_route_line_test_other";

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
        // the off-route-line rows go with the feed (ON DELETE CASCADE)
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_trip WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_service WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
    ]
}

/// An east-west road 2.2 km long. BUS (R1) runs along it, calling at S1, S2 and
/// S3, but S2 is on a service road 300 m north of it: the line passes it by.
/// METRO (R2, a subway) has the same line and stops, and flags nothing. NOLINE
/// (R3) is a bus with no line, and flags nothing either.
fn seed() -> Vec<String> {
    let road = encode_polyline(&[(13.0, 80.20), (13.0, 80.21), (13.0, 80.22)]);
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, agency_name, data_source, trips_source, \
                                headsign_source, default_run_s, default_dwell_s) \
         VALUES ('{FEED}', 'GIMS off-route-line test', 'BUSAG', 'db', 'db', 'none', 100, 20)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
         ('{FEED}', 'S1', 'C1', 'ONE', 13.0, 80.20), ('{FEED}', 'S2', 'C2', 'SERVICE ROAD', 13.0027, 80.21), \
         ('{FEED}', 'S3', 'C3', 'THREE', 13.0, 80.22)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type, encoded_polyline) VALUES \
         ('{FEED}', 'R1', 'BUS', 'ONE To THREE', 3, '{road}'), \
         ('{FEED}', 'R2', 'METRO', 'ONE To THREE', 1, '{road}'), \
         ('{FEED}', 'R3', 'NOLINE', 'ONE To THREE', 3, NULL)"
    ));
    let mut calls = Vec::new();
    for route in ["R1", "R2", "R3"] {
        for (i, stop) in ["S1", "S2", "S3"].iter().enumerate() {
            calls.push(format!(
                "('{FEED}', '{route}', 1, {}, '{stop}', 'NEW STOP')",
                i + 1
            ));
        }
    }
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, pattern_key, sequence, stop_id, stop_type) VALUES {}",
        calls.join(", ")
    ));
    s.push(format!(
        "INSERT INTO gtfs_service (gtfs_id, service_id, monday, tuesday, wednesday, thursday, friday, saturday, sunday) \
         VALUES ('{FEED}', 'WK', true, true, true, true, true, true, true)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_trip (gtfs_id, trip_id, route_id, pattern_key, profile_key, service_id, direction_id, \
                                ref_s, sort_key, source) VALUES \
         ('{FEED}', 'T1', 'R1', 1, NULL, 'WK', 0, 28800, 1, 'import'), \
         ('{FEED}', 'T2', 'R2', 1, NULL, 'WK', 0, 28800, 2, 'import'), \
         ('{FEED}', 'T3', 'R3', 1, NULL, 'WK', 0, 28800, 3, 'import')"
    ));
    s
}

// ---------------------------------------------------------------- preprocessed fixture

/// Preprocessed data for OTHER only: the DB feed needs none.
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
        route_tag: None,
        is_active: true,
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
        unserviceable: None,
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
            "version": "off-route-line-test", "gtfs_feeds": [OTHER], "files": manifest_files,
        }))
        .unwrap(),
    )
    .unwrap();
}

/// A GIMS that serves exactly this test's feeds: preprocessed data from `dir`
/// (the other feed), the DB feed from the local editor DB, no OTP calls and no
/// vehicle DB.
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
        repeater_lookahead_days: 7,
        repeater_tick_interval_secs: 300,
        repeater_min_run_interval_secs: 3600,
        repeater_stuck_lock_timeout_secs: 600,
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
        gtfs_gps_polyline_sync: None,
    }
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gims-off-route-line-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Per route, the stop codes it flags, from `/route-stop-mapping/{feed}/route/{code}`.
fn flagged(mappings: &Value) -> Vec<String> {
    mappings
        .as_array()
        .unwrap_or_else(|| panic!("not a list: {mappings}"))
        .iter()
        .filter(|m| m.get("offRouteLine") == Some(&json!(true)))
        .map(|m| m["stopCode"].as_str().unwrap().to_string())
        .collect()
}

async fn stored_rows(pool: &PgPool) -> Vec<(String, String, f64)> {
    sqlx::query(
        "SELECT route_id, stop_code, distance_m FROM gtfs_off_route_line \
         WHERE gtfs_id = $1 ORDER BY route_id, stop_code",
    )
    .bind(FEED)
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| {
        (
            r.get("route_id"),
            r.get("stop_code"),
            r.get::<f64, _>("distance_m"),
        )
    })
    .collect()
}

async fn feed_version(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT version FROM gtfs_feed WHERE gtfs_id = $1")
        .bind(FEED)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn outcome(map: &BTreeMap<String, StoreOutcome>) -> StoreOutcome {
    *map.get(FEED)
        .unwrap_or_else(|| panic!("{FEED} not measured: {map:?}"))
}

/// `GET path` on `api`, which must answer 200 with JSON.
macro_rules! get {
    ($api:expr, $path:expr) => {{
        let path: String = $path;
        let resp = test::call_service($api, test::TestRequest::get().uri(&path).to_request()).await;
        let status = resp.status().as_u16();
        let body = test::read_body(resp).await;
        let v = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
        assert_eq!(status, 200, "{path}: {v}");
        v
    }};
}

async fn run_row(pool: &PgPool) -> (i64, i64) {
    let r = sqlx::query(
        "SELECT feed_version, revision FROM gtfs_off_route_line_run WHERE gtfs_id = $1",
    )
    .bind(FEED)
    .fetch_one(pool)
    .await
    .unwrap();
    (r.get("feed_version"), r.get("revision"))
}

#[actix_web::test]
async fn a_stop_the_line_passes_by_is_flagged_once_a_poll_has_measured_it() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let url = std::env::var("EDITOR_TEST_DATABASE_URL").unwrap();
    exec(&pool, &seed()).await;

    let dir = temp_dir();
    write_preprocessed(&dir);
    // two pods on the feed, both booted before anything was measured; the test
    // runs each one's version poll (its sync_off_route_line) itself
    let state = AppState::new(app_config(&dir, &url)).await.unwrap();
    let gims = state.gtfs_service.clone();
    let api = test::init_service(
        App::new()
            .app_data(actix_web::web::Data::new(state.clone()))
            .configure(create_routes),
    )
    .await;
    let state_b = AppState::new(app_config(&dir, &url)).await.unwrap();
    let gims_b = state_b.gtfs_service.clone();
    let api_b = test::init_service(
        App::new()
            .app_data(actix_web::web::Data::new(state_b.clone()))
            .configure(create_routes),
    )
    .await;
    let route = |code: &str| {
        let path = format!("/route-stop-mapping/{FEED}/route/{code}");
        let api = &api;
        async move { get!(api, path) }
    };
    let route_b = |code: &str| {
        let path = format!("/route-stop-mapping/{FEED}/route/{code}");
        let api = &api_b;
        async move { get!(api, path) }
    };
    let version_before = feed_version(&pool).await;

    // ---- never measured: nothing flagged
    let r1 = route("R1").await;
    assert_eq!(r1.as_array().unwrap().len(), 3, "{r1}");
    assert!(flagged(&r1).is_empty(), "{r1}");
    let hash_unmeasured = get!(&api, format!("/version/{FEED}"));

    // ---- another pod measuring (the test holds the feed's lock): this poll
    // measures nothing, stores nothing, and comes back to it next poll
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('off-route-line:' || $1))")
        .bind(FEED)
        .execute(&mut *held)
        .await
        .unwrap();
    assert_eq!(
        outcome(&gims.sync_off_route_line().await),
        StoreOutcome::Busy
    );
    held.rollback().await.unwrap();
    assert!(stored_rows(&pool).await.is_empty());
    assert!(flagged(&route("R1").await).is_empty());

    // ---- pod A's poll measures, stores, and serves it in the same poll
    let first = gims.sync_off_route_line().await;
    assert_eq!(
        outcome(&first),
        StoreOutcome::Stored {
            changed: true,
            revision: 1
        }
    );
    let rows = stored_rows(&pool).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((rows[0].0.as_str(), rows[0].1.as_str()), ("R1", "C2"));
    assert!((rows[0].2 - 298.5).abs() < 1.0, "{rows:?}");
    assert_eq!(flagged(&route("R1").await), vec!["C2"]);
    assert!(
        flagged(&route("R2").await).is_empty(),
        "a subway flags nothing"
    );
    assert!(
        flagged(&route("R3").await).is_empty(),
        "nor a bus with no line"
    );
    let at_stop = get!(&api, format!("/route-stop-mapping/{FEED}/stop/C2"));
    let by_route: HashMap<String, Option<bool>> = at_stop
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["routeCode"].as_str().unwrap().to_string(),
                m.get("offRouteLine").and_then(Value::as_bool),
            )
        })
        .collect();
    assert_eq!(by_route.get("R1"), Some(&Some(true)), "{at_stop}");
    assert_eq!(by_route.get("R2"), Some(&None), "{at_stop}");
    let stop = get!(&api, format!("/stop/{FEED}/C2"));
    assert!(
        stop.get("offRouteLine").is_none(),
        "not the stop's own: {stop}"
    );
    let hash_flagged = get!(&api, format!("/version/{FEED}"));
    assert_ne!(
        hash_unmeasured, hash_flagged,
        "a client caching by it reads again"
    );

    // ---- pod B: nothing until its poll, which picks up what A stored and
    // measures nothing
    assert!(
        flagged(&route_b("R1").await).is_empty(),
        "stored, not yet picked up"
    );
    let on_b = gims_b.sync_off_route_line().await;
    assert_eq!(outcome(&on_b), StoreOutcome::UpToDate);
    assert_eq!(flagged(&route_b("R1").await), vec!["C2"]);
    assert_eq!(
        get!(&api_b, format!("/version/{FEED}")),
        hash_flagged,
        "both pods answer the same hash"
    );

    // ---- one measurement a version: the next poll stores nothing and changes
    // nothing
    let second = gims.sync_off_route_line().await;
    assert_eq!(outcome(&second), StoreOutcome::UpToDate);
    assert_eq!(get!(&api, format!("/version/{FEED}")), hash_flagged);
    assert_eq!(run_row(&pool).await, (version_before, 1));
    assert_eq!(
        feed_version(&pool).await,
        version_before,
        "measuring never bumps it"
    );

    // ---- the stop moved onto the road in a commit
    exec(
        &pool,
        &[
            format!(
                "UPDATE gtfs_stop SET lat = 13.0001 WHERE gtfs_id = '{FEED}' AND stop_id = 'S2'"
            ),
            format!("UPDATE gtfs_feed SET version = version + 1 WHERE gtfs_id = '{FEED}'"),
        ],
    )
    .await;
    // what pod A's poll does: reload the version that moved, then sync
    gims.reload_db_feed(FEED).await.unwrap();
    assert_eq!(
        flagged(&route("R1").await),
        vec!["C2"],
        "the reload serves what was stored, measured before the move"
    );
    let third = gims.sync_off_route_line().await;
    assert_eq!(
        outcome(&third),
        StoreOutcome::Stored {
            changed: true,
            revision: 2
        }
    );
    assert!(stored_rows(&pool).await.is_empty());
    assert!(flagged(&route("R1").await).is_empty());
    assert_eq!(run_row(&pool).await, (version_before + 1, 2));

    // ---- pod B, still on the old version: never measures over the new one's
    // flags, serves them, and still serves them once it has reloaded
    let behind = gims_b.sync_off_route_line().await;
    assert_eq!(outcome(&behind), StoreOutcome::UpToDate);
    assert_eq!(run_row(&pool).await, (version_before + 1, 2));
    assert!(flagged(&route_b("R1").await).is_empty());
    gims_b.reload_db_feed(FEED).await.unwrap();
    assert!(flagged(&route_b("R1").await).is_empty());
    assert_eq!(
        get!(&api_b, format!("/version/{FEED}")),
        get!(&api, format!("/version/{FEED}"))
    );
    assert_eq!(
        feed_version(&pool).await,
        version_before + 1,
        "only the commit moved it"
    );

    exec(&pool, &clear_feed()).await;
    let gone: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gtfs_off_route_line_run WHERE gtfs_id = $1")
            .bind(FEED)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(gone, 0, "the flags go with the feed");
    let _ = std::fs::remove_dir_all(&dir);
}
