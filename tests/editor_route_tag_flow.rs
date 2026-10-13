//! A route's fare tag (docs/gtfs-editor.md section 18.18), end to end against
//! a real Postgres holding the editor schema: a draft sets `route_tag`
//! through `route/update`, is reviewed, committed by someone else, and the
//! tag shows up in GIMS's own serving API as `routeTag` once the feed
//! reloads - then a second draft clears it, and a blank value normalises to
//! cleared the same way null does.
//!
//! This is the gap the Kolkata metro feeder rollout needs closed: a route
//! built straight into `gtfs_route` (no `route_internal` row) previously had
//! no way to carry a fare tag at all - `gtfs_db_source.rs` always answered
//! `routeTag: null`.
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
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const AUD: &str = "gtfs.route-tag-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "gims_route_tag_test_feed";
/// A preprocessed feed, so GIMS has preprocessed data to boot from at all.
const OTHER: &str = "gims_route_tag_test_other";
const ADMIN: &str = "admin@route-tag-test.invalid";
const EDITOR: &str = "editor@route-tag-test.invalid";
const APPROVER: &str = "approver@route-tag-test.invalid";

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
        "the route tag test only runs against a local database"
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
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{FEED}'"),
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

/// One route (R1), untagged, with trips in the tables - no preprocessed
/// overlay needed, so the fixture is just this feed's own rows.
fn seed() -> Vec<String> {
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, agency_name, data_source, trips_source, \
                                headsign_source, default_run_s, default_dwell_s) \
         VALUES ('{FEED}', 'GIMS route tag test', 'FEEDERAG', 'db', 'db', 'none', 100, 20)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
         ('{FEED}', 'S1', 'C1', 'METRO GATE', 13.00, 80.2), \
         ('{FEED}', 'S2', 'C2', 'FEEDER END', 13.01, 80.2)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type) VALUES \
         ('{FEED}', 'R1', 'MF1', 'METRO GATE To FEEDER END', 3)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, pattern_key, sequence, stop_id, stop_type) VALUES \
         ('{FEED}', 'R1', 1, 1, 'S1', 'NEW STOP'), ('{FEED}', 'R1', 1, 2, 'S2', 'NEW STOP')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_service (gtfs_id, service_id, monday, tuesday, wednesday, thursday, friday, saturday, sunday) \
         VALUES ('{FEED}', 'WK', true, true, true, true, true, true, true)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_trip (gtfs_id, trip_id, route_id, pattern_key, profile_key, service_id, direction_id, \
                                ref_s, sort_key, source) VALUES \
         ('{FEED}', 'T1', 'R1', 1, NULL, 'WK', 0, 28800, 1, 'import')"
    ));
    s.extend(reset_accounts());
    s
}

// ---------------------------------------------------------------- preprocessed fixture

/// `FEED` needs no preprocessed data of its own (its trips are in the
/// tables), but a zero-record manifest file is refused (`meta.count <= 0`),
/// which sends GIMS to the OTP fallback and a connection-refused panic in a
/// test. So `OTHER` carries one real route, exactly as
/// `gtfs_route_inactive_flow.rs`/`editor_station_merge_flow.rs` do.
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
            "version": "route-tag-test", "gtfs_feeds": [OTHER], "files": manifest_files,
        }))
        .unwrap(),
    )
    .unwrap();
}

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
    let dir = std::env::temp_dir().join(format!("gims-route-tag-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[actix_web::test]
async fn a_route_tag_set_through_a_draft_reaches_the_public_api() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let url = std::env::var("EDITOR_TEST_DATABASE_URL").unwrap();
    exec(&pool, &seed()).await;

    let dir = temp_dir();
    write_preprocessed(&dir);

    let signer = TestSigner::generate("route-tag-test-key");
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
    let get_api = |path: String| {
        let req = test::TestRequest::get().uri(&path).to_request();
        async {
            let resp = test::call_service(&api, req).await;
            let status = resp.status().as_u16();
            let body = test::read_body(resp).await;
            (
                status,
                serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null),
            )
        }
    };

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
        let (s, b, _) = call!(
            &app,
            admin
                .req("PUT", &format!("/users/{id}/feeds/{FEED}"))
                .set_json(json!({"role": role}))
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
            assert_eq!(s, 200, "commit: {b}");
        }};
    }

    // ================================================= refused payloads
    let refused = draft!("bad route_tag payloads");
    for bad in [json!(123), json!(true), json!(["x"])] {
        let (s, b, _) = add!(
            refused,
            json!({"entity": "route", "op": "update", "entity_key": "R1",
                   "after": {"route_tag": bad}})
        );
        assert_eq!(s, 400, "{bad}: {b}");
    }
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{refused}/discard"))
    );
    assert_eq!(s, 200);

    // ================================================= before: no tag at all
    gtfs.reload_db_feed(FEED).await.unwrap();
    let (s, route) = get_api(format!("/route/{FEED}/R1")).await;
    assert_eq!(s, 200, "{route}");
    assert!(
        route.get("routeTag").is_none(),
        "an untagged route carries no routeTag key: {route}"
    );

    // ================================================= set it
    let set_id = draft!("tag the feeder route");
    let (s, b, _) = add!(
        set_id,
        json!({"entity": "route", "op": "update", "entity_key": "R1",
               "after": {"route_tag": "METRO_FEEDER"}})
    );
    assert_eq!(s, 201, "{b}");
    release!(set_id);
    gtfs.reload_db_feed(FEED).await.unwrap();

    let (s, route) = get_api(format!("/route/{FEED}/R1")).await;
    assert_eq!(s, 200, "{route}");
    assert_eq!(route["routeTag"], "METRO_FEEDER", "{route}");
    let (s, routes) = get_api(format!("/routes/{FEED}")).await;
    assert_eq!(s, 200, "{routes}");
    let r1 = routes
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "R1")
        .unwrap();
    assert_eq!(r1["routeTag"], "METRO_FEEDER", "{routes}");

    // ================================================= a blank value clears it, same as null
    let blank_id = draft!("blank the feeder route's tag");
    let (s, b, _) = add!(
        blank_id,
        json!({"entity": "route", "op": "update", "entity_key": "R1",
               "after": {"route_tag": "   "}})
    );
    assert_eq!(s, 201, "{b}");
    release!(blank_id);
    gtfs.reload_db_feed(FEED).await.unwrap();
    let (s, route) = get_api(format!("/route/{FEED}/R1")).await;
    assert_eq!(s, 200, "{route}");
    assert!(
        route.get("routeTag").is_none(),
        "a blank tag normalises to cleared: {route}"
    );

    // ================================================= set again, then clear with null
    let set_again = draft!("tag it again");
    let (s, b, _) = add!(
        set_again,
        json!({"entity": "route", "op": "update", "entity_key": "R1",
               "after": {"route_tag": "METRO_FEEDER"}})
    );
    assert_eq!(s, 201, "{b}");
    release!(set_again);

    let clear_id = draft!("clear the feeder route's tag");
    let (s, b, _) = add!(
        clear_id,
        json!({"entity": "route", "op": "update", "entity_key": "R1",
               "after": {"route_tag": null}})
    );
    assert_eq!(s, 201, "{b}");
    release!(clear_id);
    gtfs.reload_db_feed(FEED).await.unwrap();
    let (s, route) = get_api(format!("/route/{FEED}/R1")).await;
    assert_eq!(s, 200, "{route}");
    assert!(
        route.get("routeTag").is_none(),
        "null clears the tag: {route}"
    );

    exec(&pool, &clear_feed()).await;
    let _ = std::fs::remove_dir_all(&dir);
}
