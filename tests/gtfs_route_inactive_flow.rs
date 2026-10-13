//! An inactive route (docs/gtfs-editor.md section 18.17), served end to end by
//! GIMS against a real Postgres:
//!
//! - it is in no list: not in `/routes`, route search, the routes at a stop or
//!   the bulk lookups by stop, the example-trip map or `/cached-data`; a stop
//!   only it serves is not in `/stops`;
//! - it answers when asked for by its id: `/route`, `/getRoutesByIds`, its
//!   route-stop mapping and the bulk lookup by route code, its example trip,
//!   its trips; with `isActive: false`, which an active route never carries;
//!   and the stop only it serves answers `/stop`;
//! - making it active again lists it on the next reload, and moves the feed's
//!   version.
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
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// The feed with an inactive route, served from the tables.
const FEED: &str = "gims_route_inactive_test_bus";
/// A preprocessed feed, so GIMS has preprocessed data to boot from at all.
const OTHER: &str = "gims_route_inactive_test_other";

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

/// BLUE (R1) calls at S1..S3 and is active; GREEN (R2) calls at S2..S4 and is
/// inactive, so S4 is served by no active route. Each runs one trip.
fn seed() -> Vec<String> {
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, agency_name, data_source, trips_source, \
                                headsign_source, default_run_s, default_dwell_s) \
         VALUES ('{FEED}', 'GIMS inactive route test', 'BUSAG', 'db', 'db', 'none', 100, 20)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
         ('{FEED}', 'S1', 'C1', 'ONE', 13.00, 80.2), ('{FEED}', 'S2', 'C2', 'TWO', 13.01, 80.2), \
         ('{FEED}', 'S3', 'C3', 'THREE', 13.02, 80.2), ('{FEED}', 'S4', 'C4', 'FOUR', 13.03, 80.2)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type, active) VALUES \
         ('{FEED}', 'R1', 'BLUE', 'ONE To THREE', 3, true), \
         ('{FEED}', 'R2', 'GREEN', 'TWO To FOUR', 3, false)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, pattern_key, sequence, stop_id, stop_type) VALUES \
         ('{FEED}', 'R1', 1, 1, 'S1', 'NEW STOP'), ('{FEED}', 'R1', 1, 2, 'S2', 'NEW STOP'), \
         ('{FEED}', 'R1', 1, 3, 'S3', 'NEW STOP'), \
         ('{FEED}', 'R2', 1, 1, 'S2', 'NEW STOP'), ('{FEED}', 'R2', 1, 2, 'S3', 'NEW STOP'), \
         ('{FEED}', 'R2', 1, 3, 'S4', 'NEW STOP')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_service (gtfs_id, service_id, monday, tuesday, wednesday, thursday, friday, saturday, sunday) \
         VALUES ('{FEED}', 'WK', true, true, true, true, true, true, true)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_trip (gtfs_id, trip_id, route_id, pattern_key, profile_key, service_id, direction_id, \
                                ref_s, sort_key, source) VALUES \
         ('{FEED}', 'T-BLUE', 'R1', 1, NULL, 'WK', 0, 28800, 1, 'import'), \
         ('{FEED}', 'T-GREEN', 'R2', 1, NULL, 'WK', 0, 30600, 2, 'import')"
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
            "version": "route-inactive-test", "gtfs_feeds": [OTHER], "files": manifest_files,
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
    let dir = std::env::temp_dir().join(format!("gims-route-inactive-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every `routeCode` anywhere in a response.
fn route_codes(v: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn walk(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::Object(o) => {
                if let Some(Value::String(c)) = o.get("routeCode") {
                    out.insert(c.clone());
                }
                o.values().for_each(|v| walk(v, out));
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(v, &mut out);
    out
}

fn set(codes: &[&str]) -> BTreeSet<String> {
    codes.iter().map(|c| c.to_string()).collect()
}

#[actix_web::test]
async fn an_inactive_route_answers_by_its_id_and_is_in_no_list() {
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
    let call = |req: test::TestRequest| async {
        let resp = test::call_service(&api, req.to_request()).await;
        let status = resp.status().as_u16();
        let body = test::read_body(resp).await;
        (
            status,
            serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
        )
    };
    let get = |path: String| call(test::TestRequest::get().uri(&path));
    let post = |path: &str, body: Value| call(test::TestRequest::post().uri(path).set_json(body));
    let ids = |v: &Value| -> BTreeSet<String> {
        v.as_array()
            .unwrap_or_else(|| panic!("not a list: {v}"))
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };

    // ---- in no list
    let (s, routes) = get(format!("/routes/{FEED}")).await;
    assert_eq!(s, 200, "{routes}");
    assert_eq!(ids(&routes), set(&["R1"]));
    assert!(
        routes[0].get("isActive").is_none(),
        "an active route reads as it always has: {routes}"
    );
    let (s, found) = get(format!("/routes/{FEED}/fuzzy/GREEN")).await;
    assert_eq!(s, 200, "{found}");
    assert_eq!(found, json!([]));
    let (_, at_stop) = get(format!("/route-stop-mapping/{FEED}/stop/C2")).await;
    assert_eq!(route_codes(&at_stop), set(&["R1"]), "{at_stop}");
    let (_, at_stop) = get(format!("/route-stop-mapping/{FEED}/stop/C4")).await;
    assert!(route_codes(&at_stop).is_empty(), "{at_stop}");
    let (s, by_stops) = post(
        "/getAllRouteStopMappingsByStopCodes",
        json!({"gtfsId": FEED, "stopCodes": ["C2", "C3", "C4"]}),
    )
    .await;
    assert_eq!(s, 200, "{by_stops}");
    assert_eq!(route_codes(&by_stops), set(&["R1"]));
    let (s, stops) = get(format!("/stops/{FEED}")).await;
    assert_eq!(s, 200, "{stops}");
    let codes: BTreeSet<String> = stops
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["stopCode"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        codes,
        set(&["C1", "C2", "C3"]),
        "C4 is served by no active route"
    );
    let (_, trip_map) = get("/example-trip-map".to_string()).await;
    let feed_trips = trip_map[FEED].as_object().unwrap();
    assert!(
        feed_trips.contains_key("R1") && !feed_trips.contains_key("R2"),
        "{trip_map}"
    );
    let (_, dump) = get("/cached-data".to_string()).await;
    let route_data = &dump["route_data_by_gtfs"][FEED];
    assert_eq!(route_codes(&route_data["mappings"]), set(&["R1"]));
    assert!(route_data["by_route"].get("R2").is_none(), "{route_data}");
    assert!(route_data["by_stop"].get("C4").is_none(), "{route_data}");

    // ---- answered by its id
    let (s, route) = get(format!("/route/{FEED}/R2")).await;
    assert_eq!(s, 200, "{route}");
    assert_eq!(
        (&route["shortName"], &route["isActive"]),
        (&json!("GREEN"), &json!(false))
    );
    let (s, by_ids) = post(&format!("/getRoutesByIds/{FEED}"), json!(["R1", "R2"])).await;
    assert_eq!(s, 200, "{by_ids}");
    assert_eq!(ids(&by_ids), set(&["R1", "R2"]));
    let (s, by_ids) = post(
        "/getAllRoutesByIds",
        json!({"gtfsId": FEED, "routeIds": ["R2"]}),
    )
    .await;
    assert_eq!(s, 200, "{by_ids}");
    assert_eq!(ids(&by_ids), set(&["R2"]));
    let (s, mapping) = get(format!("/route-stop-mapping/{FEED}/route/R2")).await;
    assert_eq!(s, 200, "{mapping}");
    assert_eq!(mapping.as_array().unwrap().len(), 3);
    let (s, by_route) = post(
        "/getAllRouteStopMappingsByRouteCodes",
        json!({"gtfsId": FEED, "routeCodes": ["R2"]}),
    )
    .await;
    assert_eq!(s, 200, "{by_route}");
    assert_eq!(route_codes(&by_route), set(&["R2"]));
    let (s, example) = get(format!("/example-trip/{FEED}/R2")).await;
    assert_eq!(s, 200, "{example}");
    assert_eq!(example["tripId"], "T-GREEN");
    let (s, trip) = get(format!("/trip/T-GREEN?gtfs_id={FEED}")).await;
    assert_eq!(s, 200, "{trip}");
    let (s, stop) = get(format!("/stop/{FEED}/C4")).await;
    assert_eq!(s, 200, "{stop}");
    assert_eq!(stop["stopCode"], "C4", "{stop}");

    // ---- active again: listed on the next reload, and the version moves
    let (_, before) = get(format!("/version/{FEED}")).await;
    exec(
        &pool,
        &[
            format!(
                "UPDATE gtfs_route SET active = true WHERE gtfs_id = '{FEED}' AND route_id = 'R2'"
            ),
            format!("UPDATE gtfs_feed SET version = version + 1 WHERE gtfs_id = '{FEED}'"),
        ],
    )
    .await;
    state.gtfs_service.reload_db_feed(FEED).await.unwrap();
    let (_, routes) = get(format!("/routes/{FEED}")).await;
    assert_eq!(ids(&routes), set(&["R1", "R2"]));
    assert!(routes
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r.get("isActive").is_none()));
    let (_, at_stop) = get(format!("/route-stop-mapping/{FEED}/stop/C4")).await;
    assert_eq!(route_codes(&at_stop), set(&["R2"]), "{at_stop}");
    let (_, after) = get(format!("/version/{FEED}")).await;
    assert_ne!(before, after);

    exec(&pool, &clear_feed()).await;
    let _ = std::fs::remove_dir_all(&dir);
}
