//! Merged-away stop ids keep answering (docs/gtfs-editor.md section 1), end to
//! end: a real editor commit merges one stop into another in a real Postgres
//! holding the editor schema, the feed reloads, and the GIMS public API is asked
//! for the retired id.
//!
//! The whole point is the incident this came from: the editor merged
//! `2a25e7a0ed` into `bd9b3af7c9` (Adyar Depot), the DB loader stopped emitting
//! the merged-away stop, and every caller still holding the old id - an OTP leg
//! handed to the rider app, a saved stop, a deep link - got 404 `Stop not found`.
//! So the assertions here are about the *old* id: `/stop/{g}/{old}` and
//! `/route-stop-mapping/{g}/stop/{old}` answer 200 with the surviving stop, the
//! response says which stop that is (`stopCode`, and the `X-Stop-Alias` header),
//! a chain of merges resolves to the end, and an id that was never a stop still
//! 404s.
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

const AUD: &str = "gtfs.stop-alias-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "gims_stop_alias_test_feed";
const ADMIN: &str = "admin@stop-alias-test.invalid";
const EDITOR: &str = "editor@stop-alias-test.invalid";
const APPROVER: &str = "approver@stop-alias-test.invalid";

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

/// A public-API GET: `(status, body, the X-Stop-Alias header if any)`.
macro_rules! api {
    ($app:expr, $path:expr) => {{
        let req = test::TestRequest::get().uri($path).to_request();
        let resp = test::call_service($app, req).await;
        let status = resp.status().as_u16();
        let alias = resp
            .headers()
            .get("x-stop-alias")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = test::read_body(resp).await;
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, json, alias)
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
        "the stop alias test only runs against a local database"
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

/// Four stops and one route. OLD is the stop that gets merged away; MID is what
/// it is merged into and is itself merged into KEEP later, so OLD ends up two
/// merges from what survives. Only OLD, VIA and FAR are on the route at the
/// start, so neither merge makes a route call the same stop twice.
fn seed() -> Vec<String> {
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, agency_name, data_source) \
         VALUES ('{FEED}', 'GIMS stop alias test feed', 'ALIASAG', 'db')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
         ('{FEED}', 'OLD', 'OLD', 'ADYAR DEPOT', 13.0060, 80.2570), \
         ('{FEED}', 'MID', 'MID', 'ADYAR DEPOT', 13.0061, 80.2571), \
         ('{FEED}', 'KEEP', 'KEEP', 'ADYAR DEPOT TERMINUS', 13.0062, 80.2572), \
         ('{FEED}', 'VIA', 'VIA', 'INDIRA NAGAR', 13.0100, 80.2600), \
         ('{FEED}', 'FAR', 'FAR', 'THIRUVANMIYUR', 13.0200, 80.2700)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name) \
         VALUES ('{FEED}', 'R1', '21G', 'ADYAR DEPOT To THIRUVANMIYUR')"
    ));
    let rows = [
        ("R1", 1, "OLD", "NEW STOP", 1, "ADYAR DEPOT"),
        ("R1", 2, "VIA", "INTERMEDIATE STOP", 1, "ADYAR DEPOT"),
        ("R1", 3, "FAR", "NEW STOP", 2, "THIRUVANMIYUR"),
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

/// The preprocessed data a DB feed takes its trips from, written to `dir`: one
/// route with one pattern and one trip, matching the seeded route. Its stop list
/// is irrelevant to this test - the DB feed replaces it - but its trip and start
/// time are what the loader overlays, so without it the feed has no patterns and
/// no route-stop mappings at all.
fn write_preprocessed(dir: &Path) {
    let stop = |id: &str, seq: i32| NandiStop {
        id: format!("{FEED}:{id}"),
        code: id.to_string(),
        name: id.to_string(),
        lat: 13.0,
        lon: 80.2,
        arrival_time: Some(21600 + seq * 135),
        departure_time: Some(21600 + seq * 135 + 15),
        stop_sequence: Some(seq + 1),
        platform_code: None,
        headsign: None,
        stage_number: None,
        stop_type: None,
    };
    let routes: HashMap<&str, Vec<NandiRoutesRes>> = HashMap::from([(
        FEED,
        vec![NandiRoutesRes {
            id: format!("{FEED}:R1"),
            short_name: Some("21G".into()),
            long_name: Some("ADYAR DEPOT To THIRUVANMIYUR".into()),
            mode: "BUS".into(),
            agency_name: Some("ALIASAG".into()),
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
        }],
    )]);
    let stops: HashMap<&str, Vec<GTFSStop>> = HashMap::from([(
        FEED,
        vec![GTFSStop {
            id: format!("{FEED}:VIA"),
            code: "VIA".into(),
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
        vec![NandiPatternDetails {
            id: format!("{FEED}:R1:0"),
            desc: Some("Pattern for route R1".into()),
            route_id: format!("{FEED}:R1"),
            stops: (0..3).map(|i| stop("VIA", i)).collect(),
            trips: vec![NandiTrip {
                id: format!("{FEED}:t1"),
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
            "version": "stop-alias-test",
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
    let dir = std::env::temp_dir().join(format!("gims-stop-alias-{}", crypto::random_token()));
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

// ---------------------------------------------------------------- the flow

#[actix_web::test]
async fn a_merged_away_stop_id_keeps_answering() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let url = std::env::var("EDITOR_TEST_DATABASE_URL").unwrap();
    exec(&pool, &seed()).await;

    let dir = temp_dir();
    write_preprocessed(&dir);

    // ---- the editor, and the GIMS public API over the same database
    let signer = TestSigner::generate("stop-alias-test-key");
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

    // The feed is DB-backed and serving the seeded stops.
    let (s, b, alias) = api!(&api, &format!("/stop/{FEED}/OLD"));
    assert_eq!((s, alias.clone()), (200, None), "{b}");
    assert_eq!(b["stopCode"], "OLD");
    let (s, b, _) = api!(&api, &format!("/route-stop-mapping/{FEED}/stop/OLD"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(codes(&b), vec!["OLD"], "{b}");

    // ---- accounts
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

    // submit (editor), approve and commit (approver) - the real commit path
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

    // ======================================================== one merge
    merge!("OLD", "MID");
    gtfs.reload_db_feed(FEED).await.unwrap();

    // The incident, fixed: the retired id answers, with the surviving stop.
    let (s, b, alias) = api!(&api, &format!("/stop/{FEED}/OLD"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b["stopCode"], "MID",
        "the survivor's code, so callers learn it"
    );
    assert_eq!(alias.as_deref(), Some("OLD=MID"), "the redirect is visible");

    // ... and so does the route-stop mapping, with the survivor's rows: the
    // route now calls MID where it called OLD.
    let (s, b, alias) = api!(&api, &format!("/route-stop-mapping/{FEED}/stop/OLD"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(codes(&b), vec!["MID"], "{b}");
    assert_eq!(b[0]["routeCode"], "R1");
    assert_eq!(b[0]["sequenceNum"], 1);
    assert_eq!(alias.as_deref(), Some("OLD=MID"));

    // The survivor answers for itself, with no header: an ordinary read.
    let (s, b, alias) = api!(&api, &format!("/stop/{FEED}/MID"));
    assert_eq!((s, alias), (200, None), "{b}");
    assert_eq!(b["stopCode"], "MID");

    // A stop that was never merged is untouched, header and all.
    let (s, b, alias) = api!(&api, &format!("/stop/{FEED}/VIA"));
    assert_eq!((s, alias), (200, None), "{b}");
    assert_eq!(b["stopCode"], "VIA");

    // An id that was never a stop still 404s, exactly as before.
    for path in [
        format!("/stop/{FEED}/NOSUCHSTOP"),
        format!("/route-stop-mapping/{FEED}/stop/NOSUCHSTOP"),
    ] {
        let (s, _, alias) = api!(&api, &path);
        assert_eq!((s, alias), (404, None), "{path}");
    }

    // The bulk read redirects the same way.
    let req = test::TestRequest::post()
        .uri("/getAllRouteStopMappingsByStopCodes")
        .set_json(json!({"gtfsId": FEED, "stopCodes": ["OLD"]}))
        .to_request();
    let b: Value = test::call_and_read_body_json(&api, req).await;
    assert_eq!(codes(&b), vec!["MID"], "{b}");

    // ======================================================== a chain of merges
    merge!("MID", "KEEP");
    gtfs.reload_db_feed(FEED).await.unwrap();

    // Both retired ids resolve to the end of the chain, not one step along it.
    for old in ["OLD", "MID"] {
        let (s, b, alias) = api!(&api, &format!("/stop/{FEED}/{old}"));
        assert_eq!(s, 200, "{old}: {b}");
        assert_eq!(b["stopCode"], "KEEP", "{old}: {b}");
        assert_eq!(alias.as_deref(), Some(&*format!("{old}=KEEP")));
        let (s, b, _) = api!(&api, &format!("/route-stop-mapping/{FEED}/stop/{old}"));
        assert_eq!(s, 200, "{old}: {b}");
        assert_eq!(codes(&b), vec!["KEEP"], "{old}: {b}");
    }

    // ======================================================== reverted to preprocessed data
    // A feed that leaves DB mode has no merges to know about: the retired ids
    // 404 again, which is what its preprocessed data says.
    gtfs.revert_db_feed_to_preprocessed(FEED).await.unwrap();
    let (s, _, alias) = api!(&api, &format!("/stop/{FEED}/OLD"));
    assert_eq!((s, alias), (404, None));

    exec(&pool, &clear_feed()).await;
    let _ = std::fs::remove_dir_all(&dir);
}
