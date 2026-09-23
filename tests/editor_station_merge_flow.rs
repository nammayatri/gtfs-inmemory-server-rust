//! Merging two stations (docs/gtfs-editor.md section 5, "Merging duplicate
//! stations"), end to end against a real Postgres holding the editor schema: a
//! draft carrying one `station`/`merge`, submitted, approved by someone else
//! and committed, then the database and the public API asked what it did.
//!
//! Why the public API is in here at all: a station merge is only finished when
//! the retired station code keeps answering. That is three separate mechanisms
//! composing - the merge's `merged_into` provenance, the loader's alias map (PR
//! #203) and the station expansion (PR #206) - and each was written without the
//! others in view. The three "composition" blocks at the end are the test of
//! that, in the one order that works: alias first, expansion second.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local. Uses its own feed, its own accounts and its own preprocessed
//! data in a temp dir, and removes its rows afterwards; never touches
//! chennai_bus. See scripts/editor_flow_test.sh.

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
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const AUD: &str = "gtfs.station-merge-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "gims_station_merge_test_feed";
const ADMIN: &str = "admin@station-merge-test.invalid";
const EDITOR: &str = "editor@station-merge-test.invalid";
const APPROVER: &str = "approver@station-merge-test.invalid";

// ---------------------------------------------------------------- harness

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
            "PUT" => test::TestRequest::put(),
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

fn code_of(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

fn detail_code(v: &Value) -> &str {
    v["error"]["details"]["code"].as_str().unwrap_or("")
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
        "the station merge test only runs against a local database"
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

async fn one<T>(pool: &PgPool, sql: &str) -> T
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
{
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .try_get(0)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
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
        // gtfs_audit_log is append-only and is never cleared; the assertions
        // below read it by this run's own change set id.
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

/// Two stations a few metres apart that are really one place (STN_GONE merges
/// into STN_KEEP), a station far away, a station with nothing under it, and two
/// ordinary stops that are not stations at all - the `not_a_station` cases.
///
/// Every platform is on a different route, so what the surviving station
/// expands to after the merge is visible. GP_OLD is a platform of STN_GONE that
/// is merged away into G1 *before* its station is merged: the third composition
/// case, a code retired twice over.
fn seed() -> Vec<String> {
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, agency_name, data_source) \
         VALUES ('{FEED}', 'GIMS station merge test feed', 'MERGEAG', 'db')"
    ));
    // Stations first: a platform's parent_station references them.
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, location_type) VALUES \
         ('{FEED}', 'STN_KEEP', 'STN_KEEP', 'ADYAR DEPOT', 13.0060, 80.2570, 1), \
         ('{FEED}', 'STN_GONE', 'SG', 'ADYAR DEPOT SOUTH', 13.0063, 80.2573, 1), \
         ('{FEED}', 'STN_FAR', 'STN_FAR', 'ADYAR DEPOT', 13.0500, 80.3000, 1), \
         ('{FEED}', 'STN_EMPTY', 'STN_EMPTY', 'NOBODY HOME', 13.0300, 80.2800, 1)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop \
           (gtfs_id, stop_id, stop_code, name, lat, lon, location_type, parent_station, platform_code, cluster_id) VALUES \
         ('{FEED}', 'K1', 'K1', 'ADYAR DEPOT', 13.0060, 80.2570, 0, 'STN_KEEP', 'Towards Guindy', 'cA'), \
         ('{FEED}', 'G1', 'G1', 'ADYAR DEPOT SOUTH', 13.0063, 80.2573, 0, 'STN_GONE', 'Towards Besant Nagar', 'cA'), \
         ('{FEED}', 'G2', 'G2', 'ADYAR DEPOT SOUTH', 13.0064, 80.2574, 0, 'STN_GONE', 'Towards Thiruvanmiyur', 'cB'), \
         ('{FEED}', 'GP_OLD', 'GPO', 'ADYAR DEPOT SOUTH', 13.0063, 80.2573, 0, 'STN_GONE', 'Towards Besant Nagar', 'cA'), \
         ('{FEED}', 'F1', 'F1', 'ADYAR DEPOT', 13.0500, 80.3000, 0, 'STN_FAR', NULL, 'cX'), \
         ('{FEED}', 'F2', 'F2', 'ADYAR DEPOT', 13.0501, 80.3001, 0, 'STN_FAR', NULL, 'cX')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, location_type, cluster_id) VALUES \
         ('{FEED}', 'PLAIN', 'PLAIN', 'INDIRA NAGAR', 13.0100, 80.2600, 0, 'cP'), \
         ('{FEED}', 'FAR', 'FAR', 'THIRUVANMIYUR', 13.0200, 80.2700, 0, 'cF')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name) VALUES \
         ('{FEED}', 'R1', '21G', 'ADYAR DEPOT To THIRUVANMIYUR'), \
         ('{FEED}', 'R2', '23C', 'ADYAR DEPOT To THIRUVANMIYUR'), \
         ('{FEED}', 'R3', '5C', 'ADYAR DEPOT To THIRUVANMIYUR'), \
         ('{FEED}', 'R4', '19B', 'ADYAR DEPOT To THIRUVANMIYUR')"
    ));
    let rows = [
        ("R1", 1, "K1", "NEW STOP", 1, "ADYAR DEPOT"),
        ("R1", 2, "PLAIN", "INTERMEDIATE STOP", 1, "ADYAR DEPOT"),
        ("R1", 3, "FAR", "NEW STOP", 2, "THIRUVANMIYUR"),
        ("R2", 1, "G1", "NEW STOP", 1, "ADYAR DEPOT SOUTH"),
        ("R2", 2, "FAR", "NEW STOP", 2, "THIRUVANMIYUR"),
        ("R3", 1, "G2", "NEW STOP", 1, "ADYAR DEPOT SOUTH"),
        ("R3", 2, "FAR", "NEW STOP", 2, "THIRUVANMIYUR"),
        ("R4", 1, "GP_OLD", "NEW STOP", 1, "ADYAR DEPOT SOUTH"),
        ("R4", 2, "FAR", "NEW STOP", 2, "THIRUVANMIYUR"),
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
    // a suggestion still waiting for review that names the station going away
    s.push(format!(
        "INSERT INTO gtfs_station_proposal (gtfs_id, batch, station_id, name, lat, lon, members, spread_m) \
         VALUES ('{FEED}', 'station-merge-test', 'STN_GONE', 'ADYAR DEPOT SOUTH', 13.0063, 80.2573, \
                 '[{{\"stop_id\": \"G1\"}}, {{\"stop_id\": \"G2\"}}]'::jsonb, 40)"
    ));
    s.extend(reset_accounts());
    s
}

// ---------------------------------------------------------------- preprocessed fixture

/// The preprocessed data a DB feed takes its trips from: one pattern with one
/// trip per route. Without a pattern carrying trips a route is dropped and the
/// feed has no route-stop mappings at all.
fn write_preprocessed(dir: &Path) {
    let route_ids = ["R1", "R2", "R3", "R4"];
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
        stage_number: None,
        is_stage_stop: None,
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
                agency_name: Some("MERGEAG".into()),
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
        ("routes.json", serde_json::to_vec(&routes).unwrap(), 4),
        ("stops.json", serde_json::to_vec(&stops).unwrap(), 1),
        ("patterns.json", serde_json::to_vec(&patterns).unwrap(), 4),
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
            "version": "station-merge-test",
            "gtfs_feeds": [FEED],
            "files": manifest_files,
        }))
        .unwrap(),
    )
    .unwrap();
}

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
        is_master: false,
        gtfs_webhook_allowed_hosts: vec![],
        gtfs_pod_id: None,
        gtfs_gps: None,
        gtfs_gps_clickhouse_password: None,
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
    let dir = std::env::temp_dir().join(format!("gims-station-merge-{}", crypto::random_token()));
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

/// The findings a draft reports for one change, as `(code, level)` sorted.
fn findings(set: &Value, change_id: i64) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = set["validation"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter(|v| v["change_id"] == change_id)
        .map(|v| {
            (
                v["code"].as_str().unwrap_or("").to_string(),
                v["level"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------- the flow

#[actix_web::test]
async fn two_stations_merge_into_one() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let url = std::env::var("EDITOR_TEST_DATABASE_URL").unwrap();
    exec(&pool, &seed()).await;

    let dir = temp_dir();
    write_preprocessed(&dir);

    let signer = TestSigner::generate("station-merge-test-key");
    let st = editor_state(&pool, &signer, &dir);
    let app =
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

    // ================================================= accounts
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
    let (s, b, _) = call!(&app, enrol(&admin));
    assert_eq!(s, 200, "{b}");
    let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let (s, b, cookie) = call!(
        &app,
        admin
            .req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now())}))
    );
    assert_eq!(s, 200, "{b}");
    admin.session = cookie;
    for (email, role) in [(EDITOR, "editor"), (APPROVER, "approver")] {
        let (s, b, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    }
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
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
            &app,
            admin
                .req("PATCH", &format!("/users/{id}"))
                .set_json(json!({"role": role, "status": "active"}))
        );
        assert_eq!(s, 200, "{b}");
    }
    for c in [&mut editor_c, &mut approver] {
        let (s, b, _) = call!(&app, enrol(c));
        assert_eq!(s, 200, "{b}");
        let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
        let (s, b, cookie) = call!(
            &app,
            c.req("POST", "/auth/totp/confirm")
                .set_json(json!({"code": crypto::totp_now(&secret, now())}))
        );
        assert_eq!(s, 200, "{b}");
        c.session = cookie;
    }

    // a draft of `title`, and a helper to add one change to it
    macro_rules! draft {
        ($title:expr) => {{
            let (s, set, _) = call!(
                &app,
                editor_c
                    .req("POST", &format!("/feeds/{FEED}/change-sets"))
                    .set_json(json!({"title": $title}))
            );
            assert_eq!(s, 201, "{set}");
            set["change_set_id"].as_str().unwrap().to_string()
        }};
    }
    macro_rules! add {
        ($id:expr, $change:expr) => {{
            call!(
                &app,
                editor_c
                    .req("POST", &format!("/change-sets/{}/changes", $id))
                    .set_json($change)
            )
        }};
    }
    macro_rules! station_merge {
        ($id:expr, $from:expr, $into:expr) => {
            add!(
                $id,
                json!({"entity": "station", "op": "merge", "entity_key": $from,
                       "after": {"into_station_id": $into}})
            )
        };
    }
    macro_rules! release {
        ($id:expr) => {{
            for (who, step) in [(&editor_c, "submit"), (&approver, "approve")] {
                let (s, b, _) = call!(
                    &app,
                    who.req("POST", &format!("/change-sets/{}/{step}", $id))
                        .set_json(json!({}))
                );
                assert_eq!(s, 200, "{step}: {b}");
            }
            let (s, b, _) = call!(
                &app,
                approver.req("POST", &format!("/change-sets/{}/commit", $id))
            );
            (s, b)
        }};
    }

    // ================================================= what is refused
    // `stop`/`merge` must keep refusing a station exactly as before: this whole
    // change type exists so that refusal never has to be loosened.
    let refused = draft!("what a station merge refuses");
    let (s, b, _) = add!(
        refused,
        json!({"entity": "stop", "op": "merge", "entity_key": "STN_GONE",
               "after": {"into_stop_id": "STN_KEEP"}})
    );
    assert_eq!((s, code_of(&b)), (400, "stop_is_station"), "{b}");
    let (s, b, _) = add!(
        refused,
        json!({"entity": "stop", "op": "merge", "entity_key": "G1",
               "after": {"into_stop_id": "STN_KEEP"}})
    );
    assert_eq!((s, code_of(&b)), (400, "stop_is_station"), "{b}");

    // and `station`/`merge` refuses anything that is not two live stations
    let (s, b, _) = station_merge!(refused, "PLAIN", "STN_KEEP");
    assert_eq!((s, code_of(&b)), (400, "not_a_station"), "{b}");
    assert!(
        b["error"]["message"].as_str().unwrap().contains("PLAIN"),
        "{b}"
    );
    let (s, b, _) = station_merge!(refused, "STN_GONE", "K1");
    assert_eq!((s, code_of(&b)), (400, "not_a_station"), "{b}");
    assert!(
        b["error"]["message"].as_str().unwrap().contains("K1"),
        "{b}"
    );
    let (s, b, _) = station_merge!(refused, "STN_GONE", "STN_GONE");
    assert_eq!((s, detail_code(&b)), (400, "merge_same_station"), "{b}");
    let (s, b, _) = station_merge!(refused, "STN_GONE", "NOSUCHSTATION");
    assert_eq!((s, code_of(&b)), (404, "entity_not_found"), "{b}");
    let (s, b, _) = station_merge!(refused, "NOSUCHSTATION", "STN_KEEP");
    assert_eq!((s, code_of(&b)), (404, "entity_not_found"), "{b}");
    // the payload's own shape
    for bad in [
        json!({"into_stop_id": "STN_KEEP"}),
        json!({"into_station_id": "STN_KEEP", "keep_name": "either"}),
        json!({"into_station_id": "STN_KEEP", "into_row_version": 0}),
    ] {
        let (s, b, _) = add!(
            refused,
            json!({"entity": "station", "op": "merge", "entity_key": "STN_GONE", "after": bad})
        );
        assert_eq!(s, 400, "{bad}: {b}");
    }
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{refused}/discard"))
    );
    assert_eq!(s, 200);

    // ================================================= warnings, not refusals
    let warned = draft!("station merges worth a second look");
    // far apart (the two points are ~5.5 km apart, well past the 500 m a
    // station is grouped within) and, since STN_FAR has the same name, no
    // names-differ warning to go with it
    let (s, b, _) = station_merge!(warned, "STN_FAR", "STN_KEEP");
    assert_eq!(s, 201, "{b}");
    let far_change = b["change_id"].as_i64().unwrap();
    assert_eq!(
        findings(&b, far_change),
        vec![("station_merge_far_apart".to_string(), "warning".to_string())],
        "{b}"
    );
    // a station with no live platforms: the merge is really just a delete
    let (s, b, _) = station_merge!(warned, "STN_EMPTY", "STN_KEEP");
    assert_eq!(s, 201, "{b}");
    let empty_change = b["change_id"].as_i64().unwrap();
    let seen: Vec<String> = findings(&b, empty_change)
        .into_iter()
        .map(|(c, _)| c)
        .collect();
    assert!(
        seen.contains(&"station_merge_no_platforms".to_string())
            && seen.contains(&"station_merge_names_differ".to_string()),
        "{seen:?}"
    );
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{warned}/discard"))
    );
    assert_eq!(s, 200);

    // ================================================= a platform merged away first
    // GP_OLD is merged into G1 while both are still platforms of STN_GONE. Its
    // code has to keep answering after its station is merged too - a code
    // retired twice over.
    let first = draft!("merge GP_OLD into G1");
    let (s, b, _) = add!(
        first,
        json!({"entity": "stop", "op": "merge", "entity_key": "GP_OLD",
               "after": {"into_stop_id": "G1"}})
    );
    assert_eq!(s, 201, "{b}");
    let (s, b) = release!(first);
    assert_eq!(s, 200, "commit: {b}");

    // ================================================= the conflict case
    // The station that stays moves on under the editor's feet between the
    // change being made and the commit.
    let stale = draft!("a station merge that goes stale");
    let (s, b, _) = station_merge!(stale, "STN_GONE", "STN_KEEP");
    assert_eq!(s, 201, "{b}");
    for (who, step) in [(&editor_c, "submit"), (&approver, "approve")] {
        let (s, b, _) = call!(
            &app,
            who.req("POST", &format!("/change-sets/{stale}/{step}"))
                .set_json(json!({}))
        );
        assert_eq!(s, 200, "{step}: {b}");
    }
    // the station that stays is edited by someone else between the approval and
    // the commit: it is the commit's own check that has to catch this
    exec(
        &pool,
        &[format!(
            "UPDATE gtfs_stop SET description = 'someone else was here' \
             WHERE gtfs_id = '{FEED}' AND stop_id = 'STN_KEEP'"
        )],
    )
    .await;
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{stale}/commit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    let conflicts = b["error"]["details"]["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{b}");
    assert_eq!(conflicts[0]["entity"], "station", "{b}");
    assert_eq!(conflicts[0]["entity_key"], "STN_KEEP", "{b}");
    assert_eq!(conflicts[0]["reason"], "changed", "{b}");
    assert!(
        conflicts[0]["message"]
            .as_str()
            .unwrap()
            .contains("kept by the merge of STN_GONE"),
        "the conflict says which side of the merge it is about: {b}"
    );
    // nothing was applied
    let still: i64 = one(
        &pool,
        &format!(
            "SELECT count(*) FROM gtfs_stop \
             WHERE gtfs_id = '{FEED}' AND stop_id = 'STN_GONE' AND NOT deleted"
        ),
    )
    .await;
    assert_eq!(still, 1, "the conflicted commit applied nothing");
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{stale}/discard"))
    );
    assert_eq!(s, 200);

    // ================================================= the merge itself
    let set_id = draft!("merge ADYAR DEPOT SOUTH into ADYAR DEPOT");
    let (s, b, _) = station_merge!(set_id, "STN_GONE", "STN_KEEP");
    assert_eq!(s, 201, "{b}");
    let change_id = b["change_id"].as_i64().unwrap();

    // `before` is the read shape the dashboard draws the screen from
    let (s, set, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{set_id}")));
    assert_eq!(s, 200, "{set}");
    let ch = set["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["change_id"] == change_id)
        .unwrap()
        .clone();
    assert_eq!(ch["before"]["from"]["stop_id"], "STN_GONE", "{ch}");
    assert_eq!(ch["before"]["from"]["location_type"], 1, "{ch}");
    assert_eq!(ch["before"]["into"]["stop_id"], "STN_KEEP", "{ch}");
    assert_eq!(ch["before"]["into"]["location_type"], 1, "{ch}");
    // GP_OLD was merged away in the earlier draft, so it is not a live platform
    // any more and does not move
    let moving: Vec<String> = ch["before"]["moving_platforms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["stop_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(moving, vec!["G1", "G2"], "{ch}");
    let g1 = &ch["before"]["moving_platforms"][0];
    assert_eq!(g1["platform_code"], "Towards Besant Nagar", "{ch}");
    // G1 now carries R4's row as well, from the platform merge above
    assert_eq!(g1["route_count"], 2, "{ch}");
    assert_eq!(
        ch["before"]["moving_platforms"][1]["route_count"], 1,
        "{ch}"
    );
    // the kept station's version is filled in from the live row
    assert!(ch["after"]["into_row_version"].is_i64(), "{ch}");
    assert!(ch["base_row_version"].is_i64(), "{ch}");

    // the warnings the reviewer sees: the names differ, and a suggestion
    // waiting for review names one of these stations
    let seen: Vec<String> = findings(&set, change_id)
        .into_iter()
        .map(|(c, _)| c)
        .collect();
    assert!(
        seen.contains(&"station_merge_names_differ".to_string())
            && seen.contains(&"station_merge_pending_proposal".to_string()),
        "{seen:?}"
    );
    assert!(
        !seen.contains(&"station_merge_far_apart".to_string()),
        "{seen:?}"
    );
    assert_eq!(set["can_submit"], true, "warnings never block: {set}");

    let (s, b) = release!(set_id);
    assert_eq!(s, 200, "commit: {b}");

    // ================================================= what the commit did
    let parents: String = one(
        &pool,
        &format!(
            "SELECT string_agg(stop_id || '->' || coalesce(parent_station, 'none'), ',' ORDER BY stop_id) \
             FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id IN ('K1', 'G1', 'G2') AND NOT deleted"
        ),
    )
    .await;
    assert_eq!(parents, "G1->STN_KEEP,G2->STN_KEEP,K1->STN_KEEP");
    // each platform kept its own label
    let labels: String = one(
        &pool,
        &format!(
            "SELECT string_agg(coalesce(platform_code, 'none'), ',' ORDER BY stop_id) \
             FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id IN ('G1', 'G2')"
        ),
    )
    .await;
    assert_eq!(labels, "Towards Besant Nagar,Towards Thiruvanmiyur");
    // the station that went: soft-deleted, out of any station, `merged_into`
    let gone: String = one(
        &pool,
        &format!(
            "SELECT deleted || '|' || coalesce(parent_station, 'null') || '|' \
                    || coalesce(provenance->>'merged_into', 'null') \
             FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'STN_GONE'"
        ),
    )
    .await;
    assert_eq!(gone, "true|null|STN_KEEP");
    // the kept station kept its own name and point: keep_name / keep_position
    // default to "into"
    let kept: String = one(
        &pool,
        &format!(
            "SELECT name || '|' || lat || '|' || lon FROM gtfs_stop \
             WHERE gtfs_id = '{FEED}' AND stop_id = 'STN_KEEP'"
        ),
    )
    .await;
    assert_eq!(kept, "ADYAR DEPOT|13.006|80.257");
    // route rows never name a station, so nothing in gtfs_route_stop moved
    let rows: i64 = one(
        &pool,
        &format!(
            "SELECT count(*) FROM gtfs_route_stop \
             WHERE gtfs_id = '{FEED}' AND stop_id IN ('STN_GONE', 'STN_KEEP')"
        ),
    )
    .await;
    assert_eq!(rows, 0);

    // the audit row
    let (s, audit, _) = call!(
        &app,
        approver.req(
            "GET",
            &format!("/feeds/{FEED}/audit?change_set={set_id}&limit=50")
        )
    );
    assert_eq!(s, 200, "{audit}");
    let merged = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["action"] == "station_merged")
        .unwrap_or_else(|| panic!("no station_merged audit row in {audit}"));
    assert_eq!(merged["detail"]["from"], "STN_GONE", "{merged}");
    assert_eq!(merged["detail"]["into"], "STN_KEEP", "{merged}");
    assert_eq!(merged["detail"]["platforms_moved"], 2, "{merged}");
    assert_eq!(merged["detail"]["keep_name"], "into", "{merged}");
    assert_eq!(merged["detail"]["keep_position"], "into", "{merged}");

    // ================================================= composing with the reads
    gtfs.reload_db_feed(FEED).await.unwrap();

    // 1. the retired station code still answers for the station that survived,
    //    through the alias map (PR #203).
    let (s, b, alias, expanded) = api!(&api, &format!("/stop/{FEED}/STN_GONE"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(alias.as_deref(), Some("STN_GONE=STN_KEEP"), "{b}");
    assert_eq!(expanded, None, "/stop never expands, {b}");
    assert_eq!(b["stopCode"], "STN_KEEP", "{b}");
    assert_eq!(
        b["locationType"], "1",
        "the survivor is still a station, {b}"
    );
    // both spellings of the retired row are alias keys
    let (s, b, alias, _) = api!(&api, &format!("/stop/{FEED}/SG"));
    assert_eq!((s, alias.as_deref()), (200, Some("SG=STN_KEEP")), "{b}");

    // 2. and it expands to the SURVIVOR's platforms: alias first, station map
    //    second (PR #206). The other order cannot work - an alias always names
    //    a live stop, and only a live stop can be a station.
    let (s, b, alias, expanded) = api!(&api, &format!("/route-stop-mapping/{FEED}/stop/STN_GONE"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(alias.as_deref(), Some("STN_GONE=STN_KEEP"), "{b}");
    assert_eq!(expanded.as_deref(), Some("STN_KEEP=3"), "{b}");
    let mut got = codes(&b);
    got.sort();
    got.dedup();
    assert_eq!(got, vec!["G1", "G2", "K1"], "{b}");
    assert_eq!(
        routes_of(&b),
        vec!["R1", "R2", "R3", "R4"],
        "every route of every platform now under STN_KEEP, {b}"
    );
    // the station itself lists the same three platforms
    let (_, b, _, _) = api!(&api, &format!("/station-children/{FEED}/STN_KEEP"));
    assert_eq!(strings(&b), vec!["G1", "G2", "K1"], "{b}");

    // 3. a platform merged away BEFORE its station was merged still resolves -
    //    and as the platform that survived it, not as the whole station, which
    //    would widen the answer behind the caller's back.
    let (s, b, alias, expanded) = api!(&api, &format!("/route-stop-mapping/{FEED}/stop/GP_OLD"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(alias.as_deref(), Some("GP_OLD=G1"), "{b}");
    assert_eq!(expanded, None, "a platform does not expand, {b}");
    for code in codes(&b) {
        assert_eq!(code, "G1", "{b}");
    }
    assert_eq!(
        routes_of(&b),
        vec!["R2", "R4"],
        "G1's own rows, R4's among them from the platform merge, {b}"
    );
    let (s, b, alias, _) = api!(&api, &format!("/stop/{FEED}/GPO"));
    assert_eq!((s, alias.as_deref()), (200, Some("GPO=G1")), "{b}");
    assert_eq!(b["locationType"], "0", "{b}");

    exec(&pool, &clear_feed()).await;
    let _ = std::fs::remove_dir_all(&dir);
}
