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

const FEED: &str = "gims_fare_stage_test_feed";

const STOPS: [(&str, &str); 4] = [
    ("A1", "{'fareStageNumber': '1', 'isStageStop': true}"),
    ("B1", "{'fareStageNumber': '2', 'isStageStop': true}"),
    ("B2", "2"),
    ("NOSTAGE", "Towards Broadway"),
];

const R2_STOPS: [(&str, &str); 2] = [
    ("B1", "{'fareStageNumber': '9', 'isStageStop': true}"),
    ("B2", "9"),
];

fn write_preprocessed(dir: &Path) {
    let stop_of = |code: &str, headsign: &str, i: usize| {
        let code = code.to_string();
        NandiStop {
            id: format!("{FEED}:{code}"),
            name: code.clone(),
            code,
            lat: 13.0 + i as f64 / 1000.0,
            lon: 80.2,
            arrival_time: Some(21600 + i as i32 * 135),
            departure_time: Some(21600 + i as i32 * 135 + 15),
            stop_sequence: Some(i as i32 + 1),
            platform_code: None,
            headsign: Some(headsign.to_string()),
            stage_number: None,
            is_stage_stop: None,
        }
    };
    let stop = |i: usize| {
        let (code, headsign) = STOPS[i];
        stop_of(code, headsign, i)
    };
    let routes: HashMap<&str, Vec<NandiRoutesRes>> = HashMap::from([(
        FEED,
        vec![
            NandiRoutesRes {
                id: format!("{FEED}:R1"),
                short_name: Some("21G".into()),
                long_name: Some("A1 To NOSTAGE".into()),
                mode: "BUS".into(),
                agency_name: Some("STAGEAG".into()),
                color: None,
                trip_count: Some(1),
                stop_count: Some(STOPS.len() as i32),
                start_point: Some(LatLong {
                    lat: 13.0,
                    lon: 80.2,
                }),
                end_point: Some(LatLong {
                    lat: 13.003,
                    lon: 80.2,
                }),
                service_tier_type: None,
                encoded_polyline: None,
            },
            NandiRoutesRes {
                id: format!("{FEED}:R2"),
                short_name: Some("70X".into()),
                long_name: Some("B1 To B2".into()),
                mode: "BUS".into(),
                agency_name: Some("STAGEAG".into()),
                color: None,
                trip_count: Some(1),
                stop_count: Some(R2_STOPS.len() as i32),
                start_point: Some(LatLong {
                    lat: 13.001,
                    lon: 80.2,
                }),
                end_point: Some(LatLong {
                    lat: 13.002,
                    lon: 80.2,
                }),
                service_tier_type: None,
                encoded_polyline: None,
            },
        ],
    )]);
    let stops: HashMap<&str, Vec<GTFSStop>> = HashMap::from([(
        FEED,
        STOPS
            .iter()
            .enumerate()
            .map(|(i, (code, _))| GTFSStop {
                id: format!("{FEED}:{code}"),
                code: (*code).into(),
                name: (*code).into(),
                lat: 13.0 + i as f64 / 1000.0,
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
        vec![
            NandiPatternDetails {
                id: format!("{FEED}:R1:0"),
                desc: Some("Pattern for route R1".into()),
                route_id: format!("{FEED}:R1"),
                stops: (0..STOPS.len()).map(stop).collect(),
                trips: vec![NandiTrip {
                    id: format!("{FEED}:t1"),
                    direction: Some(0),
                }],
            },
            NandiPatternDetails {
                id: format!("{FEED}:R2:0"),
                desc: Some("Pattern for route R2".into()),
                route_id: format!("{FEED}:R2"),
                stops: R2_STOPS
                    .iter()
                    .enumerate()
                    .map(|(i, (code, hs))| stop_of(code, hs, i))
                    .collect(),
                trips: vec![NandiTrip {
                    id: format!("{FEED}:t2"),
                    direction: Some(0),
                }],
            },
        ],
    )]);

    let files = [
        ("routes.json", serde_json::to_vec(&routes).unwrap(), 1),
        (
            "stops.json",
            serde_json::to_vec(&stops).unwrap(),
            STOPS.len(),
        ),
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
            "version": "fare-stage-test",
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
        gtfs_db_feeds: vec![],
        gtfs_version_poll_seconds: 3600,
        repeater_lookahead_days: 7,
        repeater_tick_interval_secs: 300,
        repeater_min_run_interval_secs: 3600,
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
        gtfs_gps_clickhouse_password: None,
    }
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gims-fare-stage-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn stages(v: &Value) -> HashMap<String, (Value, Value)> {
    v.as_array()
        .expect("mapping response is an array")
        .iter()
        .map(|m| {
            (
                m["stopCode"].as_str().unwrap_or_default().to_string(),
                (m["stageNumber"].clone(), m["isStageStop"].clone()),
            )
        })
        .collect()
}

#[actix_web::test]
async fn route_stop_mapping_carries_the_fare_stage() {
    let dir = temp_dir();
    write_preprocessed(&dir);

    let state = AppState::new(app_config(&dir)).await.unwrap();
    let api = test::init_service(
        App::new()
            .app_data(actix_web::web::Data::new(state.clone()))
            .configure(create_routes),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/route-stop-mapping/{FEED}/route/R1"))
        .to_request();
    let resp = test::call_service(&api, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let by_stop = stages(&body);
    assert_eq!(by_stop.len(), STOPS.len(), "{body}");

    assert_eq!(by_stop["A1"], (json!(1), json!(true)), "{body}");
    assert_eq!(by_stop["B1"], (json!(2), json!(true)), "{body}");
    assert_eq!(by_stop["B2"], (json!(2), json!(false)), "{body}");
    assert_eq!(by_stop["NOSTAGE"], (json!(null), json!(null)), "{body}");

    let req = test::TestRequest::get()
        .uri(&format!("/route-stop-mapping/{FEED}/stop/B2"))
        .to_request();
    let resp = test::call_service(&api, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 2, "{body}");
    for row in rows {
        let expected = match row["routeCode"].as_str().unwrap() {
            "R1" => 2,
            "R2" => 9,
            other => panic!("unexpected route {other}: {body}"),
        };
        assert_eq!(row["stageNumber"], json!(expected), "{body}");
        assert_eq!(row["isStageStop"], json!(false), "{body}");
    }

    let req = test::TestRequest::get()
        .uri(&format!("/stop/{FEED}/B1"))
        .to_request();
    let resp = test::call_service(&api, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(body["stopCode"], "B1", "{body}");
    let obj = body.as_object().unwrap();
    assert!(!obj.contains_key("stageNumber"), "{body}");
    assert!(!obj.contains_key("isStageStop"), "{body}");

    let req = test::TestRequest::get()
        .uri(&format!("/route-stop-mapping/{FEED}/route/R2"))
        .to_request();
    let resp = test::call_service(&api, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let r2 = stages(&body);
    assert_eq!(r2["B1"], (json!(9), json!(true)), "{body}");
    assert_eq!(r2["B2"], (json!(9), json!(false)), "{body}");

    let req = test::TestRequest::get()
        .uri(&format!("/route-stop-mapping/{FEED}/stop/B1"))
        .to_request();
    let resp = test::call_service(&api, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 2, "{body}");
    for row in rows {
        let expected = match row["routeCode"].as_str().unwrap() {
            "R1" => 2,
            "R2" => 9,
            other => panic!("unexpected route {other}: {body}"),
        };
        assert_eq!(row["stageNumber"], json!(expected), "{body}");
        assert_eq!(row["isStageStop"], json!(true), "{body}");
    }

    let req = test::TestRequest::get()
        .uri(&format!("/stops/{FEED}"))
        .to_request();
    let resp = test::call_service(&api, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), STOPS.len(), "{body}");
    for row in rows {
        let code = row["stopCode"].as_str().unwrap();
        let obj = row.as_object().unwrap();
        if code == "NOSTAGE" {
            assert!(!obj.contains_key("stageNumber"), "{body}");
            continue;
        }
        let expected = match (code, row["routeCode"].as_str().unwrap()) {
            ("A1", "R1") => 1,
            ("B1", "R1") => 2,
            ("B1", "R2") => 9,
            ("B2", "R1") => 2,
            ("B2", "R2") => 9,
            (c, r) => panic!("unexpected {c} on {r}: {body}"),
        };
        assert_eq!(row["stageNumber"], json!(expected), "{body}");
    }

    std::fs::remove_dir_all(&dir).ok();
}
