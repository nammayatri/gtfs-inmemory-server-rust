//! A station stop code answers everywhere a stop code does
//! (docs/gtfs-editor.md section 1), end to end: a DB feed whose stops are
//! grouped under stations is loaded from a real Postgres holding the editor
//! schema, and the GIMS public API is asked for a station code on every
//! stop-keyed endpoint.
//!
//! The bug this comes from: the rider app held a station code, asked for its
//! routes, and got them - but asking the cluster endpoints with the same code
//! got an empty list, so the screen showed routes with no ETAs and no vehicles
//! and said nothing about why. A code that names a place must answer for that
//! place on every endpoint, or a caller cannot tell "no service" from "wrong
//! kind of code".
//!
//! So the assertions here are about one station code against each endpoint in
//! turn, against the platform codes beneath it, and against the merge aliases of
//! the sibling commit - a station merged into another station, and a platform
//! merged away under a station - which have to compose in both orders.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. Uses its own feed, its own accounts and its own preprocessed data
//! in a temp dir, and removes its rows afterwards; never touches chennai_bus.
//! See scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::environment::{AppConfig, AppState, OtpConfig, OtpInstance};
use gtfs_routes_service::handlers::routes::create_routes;
use gtfs_routes_service::models::{
    GTFSStop, LatLong, NandiPatternDetails, NandiRoutesRes, NandiStop, NandiTrip,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const AUD: &str = "gtfs.station-code-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "gims_station_code_test_feed";
const ADMIN: &str = "admin@station-code-test.invalid";
const EDITOR: &str = "editor@station-code-test.invalid";
const APPROVER: &str = "approver@station-code-test.invalid";

// ---------------------------------------------------------------- editor harness

struct Caller<'a> {
    signer: &'a TestSigner,
    email: String,
    session: Option<String>,
}

impl Caller<'_> {
    fn req(&self, method: &str, path: &str) -> test::TestRequest {
        let r = match method {
            "GET" => test::TestRequest::get(),
            "POST" => test::TestRequest::post(),
            "PATCH" => test::TestRequest::patch(),
            _ => unreachable!(),
        }
        .uri(&format!("{BASE}{path}"))
        .insert_header((
            "x-pomerium-jwt-assertion",
            self.signer.token_for(&self.email, AUD, 300),
        ));
        let r = if method != "GET" {
            r.insert_header(("x-requested-with", "gtfs-editor"))
        } else {
            r
        };
        match &self.session {
            Some(tok) => r.insert_header(("cookie", format!("gtfs_editor_session={tok}"))),
            None => r,
        }
    }
}

fn session_from(resp: &actix_web::dev::ServiceResponse) -> Option<String> {
    resp.headers()
        .get_all("set-cookie")
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| c.strip_prefix("gtfs_editor_session="))
        .map(|c| c.split(';').next().unwrap_or("").to_string())
}

macro_rules! call {
    ($app:expr, $req:expr) => {{
        let resp = test::call_service($app, $req.to_request()).await;
        let status = resp.status().as_u16();
        let cookie = session_from(&resp);
        let body = test::read_body(resp).await;
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, json, cookie)
    }};
}

/// A public-API GET: `(status, body, X-Stop-Alias, X-Stop-Expanded)`.
macro_rules! api {
    ($app:expr, $path:expr) => {{
        let req = test::TestRequest::get().uri($path).to_request();
        let resp = test::call_service($app, req).await;
        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let alias = header("x-stop-alias");
        let expanded = header("x-stop-expanded");
        let body = test::read_body(resp).await;
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, json, alias, expanded)
    }};
}

/// A public-API POST (the bulk reads): `(status, body)`.
macro_rules! post_api {
    ($app:expr, $path:expr, $payload:expr) => {{
        let req = test::TestRequest::post()
            .uri($path)
            .set_json($payload)
            .to_request();
        let resp = test::call_service($app, req).await;
        let status = resp.status().as_u16();
        let body = test::read_body(resp).await;
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, json)
    }};
}

fn code_of(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

fn now() -> u64 {
    chrono::Utc::now().timestamp() as u64
}

async fn local_pool() -> Option<PgPool> {
    let Ok(url) = std::env::var("EDITOR_TEST_DATABASE_URL") else {
        eprintln!("EDITOR_TEST_DATABASE_URL not set; skipping");
        return None;
    };
    assert!(
        url.contains("@127.0.0.1") || url.contains("@localhost"),
        "the station code test only runs against a local database"
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

/// The feed's rows, and nothing else's. `parent_station` is cleared first so the
/// stop deletes cannot trip the self-reference.
fn clear_feed() -> Vec<String> {
    vec![
        format!("DELETE FROM gtfs_station_proposal WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!(
            "UPDATE gtfs_stop SET parent_station = NULL \
             WHERE gtfs_id = '{FEED}' AND parent_station IS NOT NULL"
        ),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
    ]
}

fn reset_accounts() -> Vec<String> {
    let list = [ADMIN, EDITOR, APPROVER]
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        format!(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, \
             totp_last_step = NULL, status = 'active' WHERE email IN ({list})"
        ),
        format!(
            "DELETE FROM gtfs_editor_session WHERE user_id IN \
             (SELECT user_id FROM gtfs_editor_user WHERE email IN ({list}))"
        ),
    ]
}

/// One station over three platforms, a second station with nothing under it, a
/// station that gets merged into the first, and two ordinary stops.
///
/// Each platform is on a different route, so the union a station code has to
/// produce is visible: R1 calls P1, R3 calls P2, R2 calls P3, and all three end
/// at FAR. P1 and P2 share an H3 cluster, P3 is in another, so the cluster
/// endpoints have two clusters to widen a single station code into.
fn seed() -> Vec<String> {
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, agency_name, data_source) \
         VALUES ('{FEED}', 'GIMS station code test feed', 'STATIONAG', 'db')"
    ));
    // Stations first: a platform's parent_station references them.
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, location_type) VALUES \
         ('{FEED}', 'STN_A', 'STN_A', 'ADYAR DEPOT', 13.0060, 80.2570, 1), \
         ('{FEED}', 'STN_OLD', 'STN_OLD', 'ADYAR DEPOT', 13.0059, 80.2569, 1), \
         ('{FEED}', 'STN_EMPTY', 'STN_EMPTY', 'NOBODY HOME', 13.0300, 80.2800, 1)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop \
           (gtfs_id, stop_id, stop_code, name, lat, lon, location_type, parent_station, cluster_id) VALUES \
         ('{FEED}', 'P1', 'P1', 'ADYAR DEPOT', 13.0060, 80.2570, 0, 'STN_A', 'cA'), \
         ('{FEED}', 'P2', 'P2', 'ADYAR DEPOT', 13.0061, 80.2571, 0, 'STN_A', 'cA'), \
         ('{FEED}', 'P3', 'P3', 'ADYAR DEPOT', 13.0062, 80.2572, 0, 'STN_A', 'cB'), \
         ('{FEED}', 'PLAIN', 'PLAIN', 'INDIRA NAGAR', 13.0100, 80.2600, 0, NULL, 'cP'), \
         ('{FEED}', 'FAR', 'FAR', 'THIRUVANMIYUR', 13.0200, 80.2700, 0, NULL, 'cF')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name) VALUES \
         ('{FEED}', 'R1', '21G', 'ADYAR DEPOT To THIRUVANMIYUR'), \
         ('{FEED}', 'R2', '23C', 'ADYAR DEPOT To THIRUVANMIYUR'), \
         ('{FEED}', 'R3', '5C', 'ADYAR DEPOT To THIRUVANMIYUR')"
    ));
    let rows = [
        ("R1", 1, "P1", "NEW STOP", 1, "ADYAR DEPOT"),
        ("R1", 2, "PLAIN", "INTERMEDIATE STOP", 1, "ADYAR DEPOT"),
        ("R1", 3, "FAR", "NEW STOP", 2, "THIRUVANMIYUR"),
        ("R2", 1, "P3", "NEW STOP", 1, "ADYAR DEPOT"),
        ("R2", 2, "FAR", "NEW STOP", 2, "THIRUVANMIYUR"),
        ("R3", 1, "P2", "NEW STOP", 1, "ADYAR DEPOT"),
        ("R3", 2, "FAR", "NEW STOP", 2, "THIRUVANMIYUR"),
    ];
    s.push(format!(
        "INSERT INTO gtfs_route_stop \
         (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES {}",
        rows.iter()
            .map(|(r, q, st, t, n, name)| format!(
                "('{FEED}', '{r}', {q}, '{st}', '{t}', {n}, '{name}', '7')"
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    s.extend(reset_accounts());
    s
}

// ---------------------------------------------------------------- preprocessed fixture

/// The preprocessed data a DB feed takes its trips from: one pattern with one
/// trip per route. Its stops are irrelevant - the DB feed replaces them - but
/// without a pattern carrying trips a route is dropped and the feed has no
/// route-stop mappings at all.
///
/// It carries no station row and no `stationId`, which is also the fixture the
/// revert-to-preprocessed assertion at the end of the flow needs.
fn write_preprocessed(dir: &Path) {
    let route_ids = ["R1", "R2", "R3"];
    let stop = |seq: i32| NandiStop {
        id: format!("{FEED}:PLAIN"),
        code: "PLAIN".to_string(),
        name: "INDIRA NAGAR".to_string(),
        lat: 13.01,
        lon: 80.26,
        arrival_time: Some(21600 + seq * 135),
        departure_time: Some(21600 + seq * 135 + 15),
        stop_sequence: Some(seq + 1),
        platform_code: None,
        headsign: None,
    };
    let routes: HashMap<&str, Vec<NandiRoutesRes>> = HashMap::from([(
        FEED,
        route_ids
            .iter()
            .map(|r| NandiRoutesRes {
                id: format!("{FEED}:{r}"),
                short_name: Some((*r).to_string()),
                long_name: Some("ADYAR DEPOT To THIRUVANMIYUR".into()),
                mode: "BUS".into(),
                agency_name: Some("STATIONAG".into()),
                color: None,
                trip_count: Some(1),
                stop_count: Some(3),
                start_point: Some(LatLong {
                    lat: 13.006,
                    lon: 80.257,
                }),
                end_point: Some(LatLong {
                    lat: 13.02,
                    lon: 80.27,
                }),
                service_tier_type: None,
                encoded_polyline: None,
            })
            .collect(),
    )]);
    let stops: HashMap<&str, Vec<GTFSStop>> = HashMap::from([(
        FEED,
        vec![GTFSStop {
            id: format!("{FEED}:PLAIN"),
            code: "PLAIN".into(),
            name: "INDIRA NAGAR".into(),
            lat: 13.01,
            lon: 80.26,
            station_id: None,
            location_type: "0".into(),
            platform_code: None,
            cluster: None,
            hindi_name: None,
            regional_name: None,
            info_json: None,
            cluster_id: None,
            description: None,
        }],
    )]);
    let patterns: HashMap<&str, Vec<NandiPatternDetails>> = HashMap::from([(
        FEED,
        route_ids
            .iter()
            .map(|r| NandiPatternDetails {
                id: format!("{FEED}:{r}:0"),
                desc: Some(format!("Pattern for route {r}")),
                route_id: format!("{FEED}:{r}"),
                stops: (0..3).map(stop).collect(),
                trips: vec![NandiTrip {
                    id: format!("{FEED}:{r}:t1"),
                    direction: Some(0),
                }],
            })
            .collect(),
    )]);

    let files = [
        ("routes.json", serde_json::to_vec(&routes).unwrap(), 3),
        ("stops.json", serde_json::to_vec(&stops).unwrap(), 1),
        ("patterns.json", serde_json::to_vec(&patterns).unwrap(), 3),
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
            "version": "station-code-test",
            "gtfs_feeds": [FEED],
            "files": manifest_files,
        }))
        .unwrap(),
    )
    .unwrap();
}

/// A GIMS config that serves exactly this test's feed: preprocessed data from
/// `dir`, the feed itself from the local editor DB, no OTP calls and no vehicle
/// DB (mock readers).
fn app_config(dir: &Path, db_url: &str) -> AppConfig {
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
    }
}

fn editor_state(pool: &PgPool, signer: &TestSigner, dir: &Path) -> EditorState {
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    use base64::Engine;
    EditorState::build(
        pool.clone(),
        EditorSettings {
            jwks_url: format!("file://{}", jwks.display()),
            audience: AUD.into(),
            bootstrap_admins: vec![ADMIN.to_string()],
            totp_key_b64: base64::engine::general_purpose::STANDARD
                .encode(crypto::random_bytes(32)),
            session_hours: 1,
            ui_dir: dir.join("no-ui"),
            osrm_url: None,
            webhook_policy: Default::default(),
        },
    )
    .unwrap()
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gims-station-code-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every stop code in a route-stop-mapping response, in order.
fn codes(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|m| m["stopCode"].as_str().map(str::to_string))
        .collect()
}

/// Every route code in a route-stop-mapping response, sorted and deduplicated.
fn routes_of(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = v
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|m| m["routeCode"].as_str().map(str::to_string))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

// ---------------------------------------------------------------- the flow

#[actix_web::test]
async fn a_station_code_answers_everywhere_a_stop_code_does() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let url = std::env::var("EDITOR_TEST_DATABASE_URL").unwrap();
    exec(&pool, &seed()).await;

    let dir = temp_dir();
    write_preprocessed(&dir);

    let signer = TestSigner::generate("station-code-test-key");
    let st = editor_state(&pool, &signer, &dir);
    let editor_app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    let state = AppState::new(app_config(&dir, &url)).await.unwrap();
    let gtfs = state.gtfs_service.clone();
    let api = test::init_service(
        App::new()
            .app_data(actix_web::web::Data::new(state.clone()))
            .configure(create_routes),
    )
    .await;

    // ================================================= the stop itself
    // A station code must keep naming the station. Substituting a platform here
    // would silently change what a saved stop, a deep link or a map pin means.
    let (s, b, alias, expanded) = api!(&api, &format!("/stop/{FEED}/STN_A"));
    assert_eq!((s, alias, expanded), (200, None, None), "{b}");
    assert_eq!(b["stopCode"], "STN_A", "{b}");
    assert_eq!(b["locationType"], "1", "{b}");

    let (s, b) = post_api!(
        &api,
        "/getAllStopsByIds",
        &json!({"gtfsId": FEED, "stopIds": ["STN_A", "P1"]})
    );
    assert_eq!(s, 200, "{b}");
    // `getAllStopsByIds` returns stops, which spell the code `code`.
    let stop_codes: Vec<String> = b
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["code"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(stop_codes, vec!["STN_A", "P1"], "{b}");

    // ================================================= route-stop mappings
    // The union of the platforms, each row keeping its own platform code - the
    // code a bus actually calls at, which is what an ETA or a vehicle join is
    // keyed on. The station itself never appears as a stopCode: nothing calls
    // there.
    for query in ["", "?direction=UP", "?allowClusters=true"] {
        let (s, b, alias, expanded) = api!(
            &api,
            &format!("/route-stop-mapping/{FEED}/stop/STN_A{query}")
        );
        assert_eq!(s, 200, "{query}: {b}");
        assert_eq!(alias, None, "{query}");
        assert_eq!(
            expanded.as_deref(),
            Some("STN_A=3"),
            "{query}: the expansion is visible in the header"
        );
        assert_eq!(
            routes_of(&b),
            vec!["R1", "R2", "R3"],
            "{query}: every route serving any platform, {b}"
        );
        for code in codes(&b) {
            assert!(
                ["P1", "P2", "P3"].contains(&code.as_str()),
                "{query}: rows carry the real platform code, got {code}"
            );
        }
    }

    // Platform order is the sorted platform order, and it is the same on every
    // load - the map behind it is a sorted Vec, not a set.
    let (_, b, _, _) = api!(&api, &format!("/route-stop-mapping/{FEED}/stop/STN_A"));
    assert_eq!(codes(&b), vec!["P1", "P2", "P3"], "{b}");

    // A platform answers for itself, with no header: an ordinary read, exactly
    // as before stations were understood anywhere.
    let (s, b, alias, expanded) = api!(&api, &format!("/route-stop-mapping/{FEED}/stop/P1"));
    assert_eq!((s, alias, expanded), (200, None, None), "{b}");
    assert_eq!(codes(&b), vec!["P1"], "{b}");
    assert_eq!(routes_of(&b), vec!["R1"], "{b}");

    // The bulk read expands the same way, and asking for a station and one of
    // its platforms together does not return that platform twice.
    let (s, b) = post_api!(
        &api,
        "/getAllRouteStopMappingsByStopCodes",
        &json!({"gtfsId": FEED, "stopCodes": ["STN_A"]})
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(codes(&b), vec!["P1", "P2", "P3"], "{b}");
    let (_, both) = post_api!(
        &api,
        "/getAllRouteStopMappingsByStopCodes",
        &json!({"gtfsId": FEED, "stopCodes": ["STN_A", "P1"]})
    );
    assert_eq!(codes(&both), codes(&b), "de-duplicated, {both}");

    // ================================================= station children
    let (s, b, _, _) = api!(&api, &format!("/station-children/{FEED}/STN_A"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(strings(&b), vec!["P1", "P2", "P3"], "sorted, {b}");

    // ================================================= the cluster reads
    // This is the endpoint the production bug was on: a station code used to
    // walk from the station itself, which no trip calls at, and answer [].
    let (s, b, _, expanded) = api!(&api, &format!("/cluster/{FEED}/destinations/STN_A"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(expanded.as_deref(), Some("STN_A=3"));
    assert_eq!(strings(&b), vec!["FAR", "PLAIN"], "{b}");

    // A platform of it sees only what its own cluster reaches; the station sees
    // the union, and neither counts the place it started from.
    let (_, b, _, _) = api!(&api, &format!("/cluster/{FEED}/destinations/P3"));
    assert_eq!(strings(&b), vec!["FAR"], "{b}");

    let (s, b, _, expanded) = api!(&api, &format!("/cluster/{FEED}/routes/STN_A/FAR"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(expanded.as_deref(), Some("STN_A=3"));
    let mut between: Vec<String> = b
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["routeCode"].as_str().unwrap().to_string())
        .collect();
    between.sort();
    assert_eq!(between, vec!["R1", "R2", "R3"], "{b}");

    // ================================================= the edges
    // A station with no platforms is not expanded: turning its 404 into an empty
    // 200 would tell a caller "no service here" when the truth is "nothing is
    // under this station".
    let (s, _, _, expanded) = api!(&api, &format!("/route-stop-mapping/{FEED}/stop/STN_EMPTY"));
    assert_eq!((s, expanded), (404, None));
    let (s, b, _, _) = api!(&api, &format!("/stop/{FEED}/STN_EMPTY"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["stopCode"], "STN_EMPTY");

    // An unknown code still 404s, with no header, exactly as before.
    for path in [
        format!("/stop/{FEED}/NOSUCHSTOP"),
        format!("/route-stop-mapping/{FEED}/stop/NOSUCHSTOP"),
    ] {
        let (s, _, alias, expanded) = api!(&api, &path);
        assert_eq!((s, alias, expanded), (404, None, None), "{path}");
    }

    // ================================================= accounts, for the merges
    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    let mut editor_c = Caller {
        signer: &signer,
        email: EDITOR.into(),
        session: None,
    };
    let mut approver = Caller {
        signer: &signer,
        email: APPROVER.into(),
        session: None,
    };
    let enrol = |c: &Caller| c.req("POST", "/auth/totp/enroll");
    let (s, b, _) = call!(&editor_app, enrol(&admin));
    assert_eq!(s, 200, "{b}");
    let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let (s, b, cookie) = call!(
        &editor_app,
        admin
            .req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now())}))
    );
    assert_eq!(s, 200, "{b}");
    admin.session = cookie;
    for (email, role) in [(EDITOR, "editor"), (APPROVER, "approver")] {
        let (s, b, _) = call!(
            &editor_app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    }
    let (_, users, _) = call!(&editor_app, admin.req("GET", "/users"));
    for (email, role) in [(EDITOR, "editor"), (APPROVER, "approver")] {
        let id = users["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["email"] == email)
            .unwrap()["user_id"]
            .as_str()
            .unwrap()
            .to_string();
        let (s, b, _) = call!(
            &editor_app,
            admin
                .req("PATCH", &format!("/users/{id}"))
                .set_json(json!({"role": role, "status": "active"}))
        );
        assert_eq!(s, 200, "{b}");
    }
    for c in [&mut editor_c, &mut approver] {
        let (s, b, _) = call!(&editor_app, enrol(c));
        assert_eq!(s, 200, "{b}");
        let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
        let (s, b, cookie) = call!(
            &editor_app,
            c.req("POST", "/auth/totp/confirm")
                .set_json(json!({"code": crypto::totp_now(&secret, now())}))
        );
        assert_eq!(s, 200, "{b}");
        c.session = cookie;
    }

    macro_rules! merge {
        ($from:expr, $into:expr) => {{
            let (s, set, _) = call!(
                &editor_app,
                editor_c
                    .req("POST", &format!("/feeds/{FEED}/change-sets"))
                    .set_json(json!({"title": format!("merge {} into {}", $from, $into)}))
            );
            assert_eq!(s, 201, "{set}");
            let id = set["change_set_id"].as_str().unwrap().to_string();
            let (s, b, _) = call!(
                &editor_app,
                editor_c
                    .req("POST", &format!("/change-sets/{id}/changes"))
                    .set_json(json!({"entity": "stop", "op": "merge",
                                     "entity_key": $from, "after": {"into_stop_id": $into}}))
            );
            assert_eq!(s, 201, "{b}");
            for (who, step) in [(&editor_c, "submit"), (&approver, "approve")] {
                let (s, b, _) = call!(
                    &editor_app,
                    who.req("POST", &format!("/change-sets/{id}/{step}"))
                        .set_json(json!({}))
                );
                assert_eq!(s, 200, "{step}: {b}");
            }
            let (s, b, _) = call!(
                &editor_app,
                approver.req("POST", &format!("/change-sets/{id}/commit"))
            );
            assert_eq!(s, 200, "commit: {b}");
        }};
    }

    // ============================== merge then station: a retired station code
    // The caller holds a station code that has since been retired in favour of
    // another station. The alias resolves it, and what it resolves to is a
    // station, so it expands - both maps, in that order, on one request.
    //
    // The editor refuses `stop/merge` on a station (`stop_is_station`: "only
    // stops are merged"), so this row is written the way the loader would find
    // it - deleted, with `merged_into` provenance - rather than through the
    // change-set path the platform merge below uses. The alias map is built from
    // exactly these rows, whatever wrote them.
    exec(
        &pool,
        &[format!(
            "UPDATE gtfs_stop SET deleted = true, \
               provenance = coalesce(provenance, '{{}}'::jsonb) \
                            || jsonb_build_object('merged_into', 'STN_A') \
             WHERE gtfs_id = '{FEED}' AND stop_id = 'STN_OLD'"
        )],
    )
    .await;
    gtfs.reload_db_feed(FEED).await.unwrap();

    let (s, b, alias, expanded) = api!(&api, &format!("/route-stop-mapping/{FEED}/stop/STN_OLD"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(alias.as_deref(), Some("STN_OLD=STN_A"));
    assert_eq!(expanded.as_deref(), Some("STN_A=3"));
    assert_eq!(codes(&b), vec!["P1", "P2", "P3"], "{b}");

    // ... and the stop read still hands back a station, the surviving one.
    let (s, b, alias, _) = api!(&api, &format!("/stop/{FEED}/STN_OLD"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(alias.as_deref(), Some("STN_OLD=STN_A"));
    assert_eq!(b["stopCode"], "STN_A", "{b}");
    assert_eq!(b["locationType"], "1", "{b}");

    // ============================== station then merge: a retired platform code
    // The caller holds a platform code that was merged into another platform of
    // the same station. It must answer as that platform - not as the station,
    // which would quietly widen the answer to every route the station sees.
    merge!("P2", "P1");
    gtfs.reload_db_feed(FEED).await.unwrap();

    let (s, b, alias, expanded) = api!(&api, &format!("/route-stop-mapping/{FEED}/stop/P2"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(alias.as_deref(), Some("P2=P1"));
    assert_eq!(expanded, None, "a platform does not expand");
    assert_eq!(
        routes_of(&b),
        vec!["R1", "R3"],
        "P1's own rows, including the route the merge moved onto it, {b}"
    );
    for code in codes(&b) {
        assert_eq!(code, "P1", "{b}");
    }

    // The station lost a platform on the same commit, and says so.
    let (_, b, _, _) = api!(&api, &format!("/station-children/{FEED}/STN_A"));
    assert_eq!(strings(&b), vec!["P1", "P3"], "{b}");
    let (s, b, _, expanded) = api!(&api, &format!("/route-stop-mapping/{FEED}/stop/STN_A"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(expanded.as_deref(), Some("STN_A=2"));
    assert_eq!(
        routes_of(&b),
        vec!["R1", "R2", "R3"],
        "the same routes, now reached through two platforms, {b}"
    );

    // ============================== reverted to preprocessed data
    // A feed that leaves DB mode has no station rows at all, so nothing expands
    // and the station codes are simply unknown - the behaviour it had before.
    gtfs.revert_db_feed_to_preprocessed(FEED).await.unwrap();
    for path in [
        format!("/stop/{FEED}/STN_A"),
        format!("/route-stop-mapping/{FEED}/stop/STN_A"),
    ] {
        let (s, _, alias, expanded) = api!(&api, &path);
        assert_eq!((s, alias, expanded), (404, None, None), "{path}");
    }
    let (s, b, _, _) = api!(&api, &format!("/station-children/{FEED}/STN_A"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(strings(&b), Vec::<String>::new(), "{b}");

    exec(&pool, &clear_feed()).await;
    let _ = std::fs::remove_dir_all(&dir);
}
