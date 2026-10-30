//! `/stops/{gtfs_id}/fuzzy/{query}` over the public API, on a preprocessed feed
//! loaded with no database. The endpoint matches the consumer app's stop search
//! (services::stop_fuzzy, unit-tested there against the app's own output); here
//! the assertions are that the endpoint uses it: best match first, typos found,
//! and a limit keeping the best rather than an arbitrary subset.

use actix_web::{test, App};
use gtfs_routes_service::environment::{AppConfig, AppState, OtpConfig, OtpInstance};
use gtfs_routes_service::handlers::routes::create_routes;
use gtfs_routes_service::models::{
    GTFSStop, LatLong, NandiPatternDetails, NandiRoutesRes, NandiStop, NandiTrip,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const FEED: &str = "gims_stop_fuzzy_test_feed";

/// (code, name)
type StopDef = (&'static str, &'static str);

/// One route through every stop, in this order.
const STOPS: [StopDef; 6] = [
    ("LITTLE_MOUNT", "LITTLE MOUNT GUINDY"),
    ("GUINDY_TERM", "GUINDY BUS TERMINUS"),
    ("GUINDY_N", "GUINDY"),
    ("GUINDY_S", "GUINDY"),
    ("SAIDAPET", "SAIDAPET"),
    ("TAMBARAM", "TAMBARAM"),
];

fn write_preprocessed(dir: &Path) {
    let routes: HashMap<&str, Vec<NandiRoutesRes>> = HashMap::from([(
        FEED,
        vec![NandiRoutesRes {
            id: format!("{FEED}:21G"),
            short_name: Some("21G".into()),
            long_name: Some("LITTLE MOUNT GUINDY To TAMBARAM".into()),
            mode: "BUS".into(),
            agency_name: Some("FUZZYAG".into()),
            color: None,
            trip_count: Some(1),
            stop_count: Some(STOPS.len() as i32),
            start_point: Some(LatLong {
                lat: 13.0,
                lon: 80.2,
            }),
            end_point: None,
            service_tier_type: None,
            encoded_polyline: None,
            route_tag: None,
            is_active: true,
        }],
    )]);
    let stops: HashMap<&str, Vec<GTFSStop>> = HashMap::from([(
        FEED,
        STOPS
            .iter()
            .enumerate()
            .map(|(i, (code, name))| GTFSStop {
                id: format!("{FEED}:{code}"),
                code: (*code).into(),
                name: (*name).into(),
                lat: 13.0 + i as f64 * 0.001,
                lon: 80.2,
                station_id: None,
                location_type: "0".into(),
                platform_code: None,
                cluster: None,
                hindi_name: None,
                regional_name: None,
                info_json: None,
                cluster_id: None,
                description: None,
            })
            .collect(),
    )]);
    let patterns: HashMap<&str, Vec<NandiPatternDetails>> = HashMap::from([(
        FEED,
        vec![NandiPatternDetails {
            id: format!("{FEED}:21G:0"),
            desc: Some("Pattern for route 21G".into()),
            route_id: format!("{FEED}:21G"),
            stops: STOPS
                .iter()
                .enumerate()
                .map(|(i, (code, name))| NandiStop {
                    id: format!("{FEED}:{code}"),
                    code: (*code).to_string(),
                    name: (*name).to_string(),
                    lat: 13.0 + i as f64 * 0.001,
                    lon: 80.2,
                    arrival_time: Some(21600 + i as i32 * 300),
                    departure_time: Some(21600 + i as i32 * 300 + 15),
                    stop_sequence: Some(i as i32 + 1),
                    platform_code: None,
                    headsign: None,
                    stage_number: None,
                    is_stage_stop: None,
                    unserviceable: None,
                })
                .collect(),
            trips: vec![NandiTrip {
                id: format!("{FEED}:21G:t1"),
                direction: Some(0),
            }],
        }],
    )]);

    let files = [
        ("routes.json", serde_json::to_vec(&routes).unwrap(), 1),
        ("stops.json", serde_json::to_vec(&stops).unwrap(), 1),
        ("patterns.json", serde_json::to_vec(&patterns).unwrap(), 1),
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
            "version": "stop-fuzzy-test",
            "gtfs_feeds": [FEED],
            "files": manifest_files,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn app_config(dir: &Path) -> AppConfig {
    let instance = OtpInstance {
        url: "http://127.0.0.1:1/nandi".into(),
        identifier: FEED.into(),
    };
    AppConfig {
        logger_cfg: shared::tools::logger::LoggerConfig {
            level: shared::tools::logger::LogLevel::WARN,
            log_to_file: false,
        },
        database_url: None,
        internal_database_url: None,
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
        is_master: false,
        gtfs_webhook_allowed_hosts: vec![],
        gtfs_pod_id: None,
        gtfs_gps: None,
        gtfs_gps_clickhouse_password: None,
    }
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gims-stop-fuzzy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn codes(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s["stopCode"].as_str().unwrap().to_string())
        .collect()
}

#[actix_web::test]
async fn stop_fuzzy_ranks_like_the_app_and_finds_typos() {
    let dir = temp_dir();
    write_preprocessed(&dir);
    let state = AppState::new(app_config(&dir)).await.unwrap();
    let api = test::init_service(
        App::new()
            .app_data(actix_web::web::Data::new(state.clone()))
            .configure(create_routes),
    )
    .await;
    let get = |path: String| test::TestRequest::get().uri(&path).to_request();

    // Exact names (both kerbs), then the name that starts with the query, then
    // the one that only contains it.
    let resp = test::call_service(&api, get(format!("/stops/{FEED}/fuzzy/guindy"))).await;
    assert_eq!(resp.status(), 200);
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(
        codes(&body),
        vec!["GUINDY_N", "GUINDY_S", "GUINDY_TERM", "LITTLE_MOUNT"]
    );

    // A typo still finds the stop.
    let resp = test::call_service(&api, get(format!("/stops/{FEED}/fuzzy/saidapt"))).await;
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(codes(&body), vec!["SAIDAPET"]);

    // A limit keeps the best matches.
    let resp = test::call_service(&api, get(format!("/stops/{FEED}/fuzzy/guindy?limit=2"))).await;
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(codes(&body), vec!["GUINDY_N", "GUINDY_S"]);

    let _ = std::fs::remove_dir_all(&dir);
}
