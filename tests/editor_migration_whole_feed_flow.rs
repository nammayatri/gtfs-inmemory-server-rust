//! A full migration carries every row of a feed (docs/gtfs-editor.md section
//! 23), end to end against two real Postgres databases holding the editor
//! schema: one is master's, the other prod's, each with its own editor.
//!
//! The feed serves its routes' own stop lists (no stages), as shillong's do.
//! Master changes a route's stop list, adds a second stop order to it, makes a
//! route with its stops, tags a route, makes a stop a depot with a geofence,
//! renames the agency and adds a fare; its calendar has no exceptions, so its
//! zip has no `calendar_dates.txt`, and it has no trips at all. Prod changes a
//! stop list of its own and adds a geofence and a fare of its own. The whole
//! feed moves in steps - stops, routes and geofences; the stop lists; the GTFS
//! files - each a prod draft a second person commits, until prod has all of
//! master's and still has its own.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` (master) and
//! `EDITOR_TEST_PROD_DATABASE_URL` (prod) are both set, local, and different.
//! Uses its own feed and accounts and removes its rows afterwards. See
//! scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::io::Read;
use std::sync::Arc;

const AUD: &str = "gtfs.editor-migration-whole-test.local";
const BASE: &str = "/internal/gtfs-editor";
const EMPTY_HASH: &str = "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945";

const FEED: &str = "editor_migration_whole_test_feed";
const ADMIN: &str = "admin@editor-migration-whole-test.invalid";
const APPROVER: &str = "approver@editor-migration-whole-test.invalid";

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

macro_rules! call_bytes {
    ($app:expr, $req:expr) => {{
        let resp = test::call_service($app, $req.to_request()).await;
        let status = resp.status().as_u16();
        let body = test::read_body(resp).await.to_vec();
        (status, body)
    }};
}

fn now() -> u64 {
    chrono::Utc::now().timestamp() as u64
}

async fn pool_of(var: &str) -> Option<PgPool> {
    let Ok(url) = std::env::var(var) else {
        eprintln!("{var} not set; skipping");
        return None;
    };
    assert!(
        url.contains("@127.0.0.1") || url.contains("@localhost"),
        "the editor tests only run against a local database"
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
    let mut s = vec![
        format!("DELETE FROM gtfs_migration_import WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_geofence WHERE gtfs_id = '{FEED}'"),
    ];
    for t in editor::feed_io::feed_data_tables() {
        s.push(format!("DELETE FROM {t} WHERE gtfs_id = '{FEED}'"));
    }
    s.push(format!(
        "DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{FEED}'"
    ));
    s.push(format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"));
    s
}

/// The same feed on both sides: stops, two routes with their own stop lists,
/// an agency and a weekday calendar with no exceptions; no stages, no trips.
fn seed() -> Vec<String> {
    let list = [ADMIN, APPROVER]
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, use_stages) \
         VALUES ('{FEED}', 'Editor whole-feed migration test feed', false)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', c, c, 'STOP ' || c, 25.5 + ascii(c) * 0.001, 91.8 \
         FROM unnest(ARRAY['A','B','C','D','E','F','G','H']) c"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id, active) VALUES \
         ('{FEED}', 'R1', '1', 'A To E', 'AG', true), ('{FEED}', 'R2', '2', 'A To E short', 'AG', true)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES \
         ('{FEED}', 'R1', 1, 'A', 'NEW STOP', 1, 'ALPHA', '11'), ('{FEED}', 'R1', 2, 'B', 'INTERMEDIATE STOP', 1, 'ALPHA', '11'), \
         ('{FEED}', 'R1', 3, 'C', 'NEW STOP', 2, 'BETA', '11'), ('{FEED}', 'R1', 4, 'E', 'NEW STOP', 3, 'GAMMA', '11'), \
         ('{FEED}', 'R2', 1, 'A', 'NEW STOP', 1, 'ALPHA', '22'), ('{FEED}', 'R2', 2, 'B', 'INTERMEDIATE STOP', 1, 'ALPHA', '22'), \
         ('{FEED}', 'R2', 3, 'E', 'NEW STOP', 2, 'GAMMA', '22')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_agency (gtfs_id, agency_id, agency_name, agency_url, agency_timezone) \
         VALUES ('{FEED}', 'AG', 'Shared Cabs', 'https://cabs.example', 'Asia/Kolkata')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_service (gtfs_id, service_id, monday, tuesday, wednesday, thursday, friday, \
                                   saturday, sunday, start_date, end_date) \
         VALUES ('{FEED}', 'WK', true, true, true, true, true, false, false, '2026-01-01', '2026-12-31')"
    ));
    s.push(format!(
        "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, totp_last_step = NULL, \
         status = 'active' WHERE email IN ({list})"
    ));
    s.push(format!(
        "DELETE FROM gtfs_editor_session WHERE user_id IN \
         (SELECT user_id FROM gtfs_editor_user WHERE email IN ({list}))"
    ));
    s
}

fn state(pool: &PgPool, signer: &TestSigner, is_master: bool) -> (EditorState, std::path::PathBuf) {
    let dir =
        std::env::temp_dir().join(format!("editor-migration-whole-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    use base64::Engine;
    let mut st = EditorState::build(
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
    .unwrap();
    st.is_master = is_master;
    (st, dir)
}

async fn scalar_i64(pool: &PgPool, sql: impl AsRef<str>) -> i64 {
    let sql = sql.as_ref();
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<i64, _>(0)
}

async fn scalar_text(pool: &PgPool, sql: impl AsRef<str>) -> String {
    let sql = sql.as_ref();
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<String, _>(0)
}

/// One stop order of a route as it is served: `stop/type/stage`, in order.
async fn served(pool: &PgPool, route: &str, pattern: i16) -> Vec<String> {
    sqlx::query(&format!(
        "SELECT coalesce(stop_id, marker_id) || '/' || stop_type || '/' || stage_no AS r \
         FROM gtfs_route_stop_effective WHERE gtfs_id = '{FEED}' AND route_id = '{route}' \
           AND pattern_key = {pattern} ORDER BY sequence"
    ))
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<String, _>("r"))
    .collect()
}

fn row(stop: &str, kind: &str, no: i32, name: &str) -> Value {
    json!({"stop_id": stop, "stop_type": kind, "stage_no": no, "stage_name": name})
}

/// A full migration report's step by its name.
fn step<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["steps"]
        .as_array()
        .and_then(|a| a.iter().find(|s| s["step"] == name))
        .unwrap_or_else(|| panic!("no step {name}: {report}"))
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- the flow

#[actix_web::test]
async fn the_whole_feed_moves_stop_lists_geofences_and_gtfs_files_too() {
    let (Some(mpool), Some(ppool)) = (
        pool_of("EDITOR_TEST_DATABASE_URL").await,
        pool_of("EDITOR_TEST_PROD_DATABASE_URL").await,
    ) else {
        return;
    };
    assert_ne!(
        std::env::var("EDITOR_TEST_DATABASE_URL").unwrap(),
        std::env::var("EDITOR_TEST_PROD_DATABASE_URL").unwrap(),
        "master and prod are two databases"
    );
    exec(&mpool, &seed()).await;
    exec(&ppool, &seed()).await;
    let signer = TestSigner::generate("migration-whole-test-key");
    let (mst, mdir) = state(&mpool, &signer, true);
    let (pst, pdir) = state(&ppool, &signer, false);
    let master =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(mst)))))
            .await;
    let prod =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(pst)))))
            .await;

    // ---- an admin and an approver on each side
    let new_caller = |email: &str| Caller {
        signer: &signer,
        email: email.into(),
        session: None,
    };
    let (mut m_admin, mut m_ap) = (new_caller(ADMIN), new_caller(APPROVER));
    let (mut p_admin, mut p_ap) = (new_caller(ADMIN), new_caller(APPROVER));
    macro_rules! sign_in {
        ($app:expr, $who:expr) => {{
            let (s, b, _) = call!($app, $who.req("POST", "/auth/totp/enroll"));
            assert_eq!(s, 200, "{b}");
            let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
            let (s, b, cookie) = call!(
                $app,
                $who.req("POST", "/auth/totp/confirm")
                    .set_json(json!({"code": crypto::totp_now(&secret, now())}))
            );
            assert_eq!(s, 200, "{b}");
            $who.session = cookie;
        }};
    }
    macro_rules! approver {
        ($app:expr, $admin:expr, $ap:expr) => {{
            let (s, b, _) = call!(
                $app,
                $admin
                    .req("POST", "/users")
                    .set_json(json!({"email": APPROVER, "role": "approver"}))
            );
            assert!(
                s == 201 || b["error"]["code"] == "user_exists",
                "{s} {b}"
            );
            let (_, users, _) = call!($app, $admin.req("GET", "/users"));
            let id = users["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|u| u["email"] == APPROVER)
                .unwrap()["user_id"]
                .as_str()
                .unwrap()
                .to_string();
            let (s, b, _) = call!(
                $app,
                $admin
                    .req("PATCH", &format!("/users/{id}"))
                    .set_json(json!({"role": "approver", "status": "active"}))
            );
            assert_eq!(s, 200, "{b}");
            let (s, b, _) = call!(
                $app,
                $admin
                    .req("PUT", &format!("/users/{id}/feeds/{FEED}"))
                    .set_json(json!({"role": "approver"}))
            );
            assert_eq!(s, 200, "{b}");
            sign_in!($app, $ap);
        }};
    }
    sign_in!(&master, m_admin);
    approver!(&master, m_admin, m_ap);
    sign_in!(&prod, p_admin);
    approver!(&prod, p_admin, p_ap);

    // a draft by the admin, submitted, then approved and committed by the approver
    macro_rules! draft {
        ($app:expr, $admin:expr, $title:expr, $changes:expr) => {{
            let (s, b, _) = call!(
                $app,
                $admin
                    .req("POST", &format!("/feeds/{FEED}/change-sets"))
                    .set_json(json!({"title": $title}))
            );
            assert_eq!(s, 201, "{b}");
            let id = b["change_set_id"].as_str().unwrap().to_string();
            for c in $changes {
                let (s, b, _) = call!(
                    $app,
                    $admin
                        .req("POST", &format!("/change-sets/{id}/changes"))
                        .set_json(c.clone())
                );
                assert_eq!(s, 201, "{c}: {b}");
            }
            id
        }};
    }
    macro_rules! ship {
        ($app:expr, $admin:expr, $ap:expr, $id:expr) => {{
            let id: &str = &$id;
            let (s, b, _) = call!(
                $app,
                $admin.req("POST", &format!("/change-sets/{id}/submit"))
            );
            assert_eq!(s, 200, "submit: {b}");
            let (s, b, _) = call!($app, $ap.req("POST", &format!("/change-sets/{id}/approve")));
            assert_eq!(s, 200, "approve: {b}");
            let (s, b, _) = call!($app, $ap.req("POST", &format!("/change-sets/{id}/commit")));
            assert_eq!(s, 200, "commit: {b}");
        }};
    }
    // ======================================================== master's work
    let (_, r1, _) = call!(
        &master,
        m_admin.req("GET", &format!("/feeds/{FEED}/routes/R1"))
    );
    let m1 = draft!(
        &master,
        m_admin,
        "shillong's routes, fences and fares",
        [
            json!({"entity": "route_stops", "op": "replace", "entity_key": "R1", "after": {
                "rows": [row("A", "NEW STOP", 1, "ALPHA"), row("B", "INTERMEDIATE STOP", 1, "ALPHA"),
                         row("C", "NEW STOP", 2, "BETA"), row("D", "INTERMEDIATE STOP", 2, "BETA"),
                         row("E", "NEW STOP", 3, "GAMMA")],
                "base_rows_hash": r1["rows_hash"]}}),
            json!({"entity": "route_stops", "op": "replace", "entity_key": "R1", "after": {
                "pattern_key": 2,
                "rows": [row("A", "NEW STOP", 1, "ALPHA"), row("B", "INTERMEDIATE STOP", 1, "ALPHA"),
                         row("C", "NEW STOP", 2, "BETA")],
                "base_rows_hash": EMPTY_HASH}}),
            json!({"entity": "route", "op": "create", "entity_key": "R4", "after": {
                "route_id": "R4", "short_name": "4", "long_name": "A To F"}}),
            json!({"entity": "route_stops", "op": "replace", "entity_key": "R4", "after": {
                "rows": [row("A", "NEW STOP", 1, "ALPHA"), row("F", "NEW STOP", 2, "ZETA")],
                "base_rows_hash": EMPTY_HASH}}),
            json!({"entity": "route", "op": "update", "entity_key": "R2",
                   "after": {"route_tag": "METRO_FEEDER"}}),
            json!({"entity": "stop", "op": "update", "entity_key": "B", "after": {"is_depot": true}}),
            json!({"entity": "geofence", "op": "create", "entity_key": "B", "after": {
                "stop_id": "B", "lat": 25.566, "lon": 91.8, "shape": "circle", "radius_m": 150}}),
            json!({"entity": "agency", "op": "update", "entity_key": "AG",
                   "after": {"agency_name": "Shillong Shared Cabs"}}),
            json!({"entity": "fare_attribute", "op": "create", "entity_key": "F1", "after": {
                "fare_id": "F1", "price": 10, "currency_type": "INR", "payment_method": 0,
                "transfers": 0}}),
        ]
    );
    ship!(&master, m_admin, m_ap, m1);

    // ======================================================== prod's own edits
    let since = scalar_i64(
        &ppool,
        format!("SELECT version FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
    )
    .await;
    let (_, r2, _) = call!(
        &prod,
        p_admin.req("GET", &format!("/feeds/{FEED}/routes/R2"))
    );
    let p1 = draft!(
        &prod,
        p_admin,
        "prod's own edits",
        [
            json!({"entity": "route_stops", "op": "replace", "entity_key": "R2", "after": {
                "rows": [row("A", "NEW STOP", 1, "ALPHA"), row("E", "NEW STOP", 2, "GAMMA")],
                "base_rows_hash": r2["rows_hash"]}}),
            json!({"entity": "geofence", "op": "create", "entity_key": "C", "after": {
                "stop_id": "C", "lat": 25.567, "lon": 91.8, "shape": "circle", "radius_m": 90}}),
            json!({"entity": "fare_attribute", "op": "create", "entity_key": "PF", "after": {
                "fare_id": "PF", "price": 5, "currency_type": "INR", "payment_method": 0,
                "transfers": 0}}),
        ]
    );
    ship!(&prod, p_admin, p_ap, p1);
    let r2_prod = served(&ppool, "R2", 1).await;

    // ======================================================== the whole feed, by default
    let (s, file) = call_bytes!(
        &master,
        m_admin.req("GET", &format!("/feeds/{FEED}/migration"))
    );
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&file));
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(file.clone())).unwrap();
    let mut manifest = String::new();
    zip.by_name("manifest.json")
        .unwrap()
        .read_to_string(&mut manifest)
        .unwrap();
    let manifest: Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(
        manifest["scopes"],
        json!(["stops", "routes", "stages", "timetables", "records"]),
        "{manifest}"
    );
    for f in [
        "stops.json",
        "geofences.json",
        "routes.json",
        "route_stops.json",
        "stages.json",
        "route_stages.json",
        "variants.json",
        "gtfs.zip",
    ] {
        assert!(manifest["files"].get(f).is_some(), "{f}: {manifest}");
    }
    // master's calendar has no exceptions: its zip has no calendar_dates.txt,
    // and no trips
    let mut gtfs = Vec::new();
    zip.by_name("gtfs.zip")
        .unwrap()
        .read_to_end(&mut gtfs)
        .unwrap();
    let gtfs = zip::ZipArchive::new(std::io::Cursor::new(gtfs)).unwrap();
    let names: Vec<&str> = gtfs.file_names().collect();
    assert!(names.contains(&"calendar.txt"), "{names:?}");
    assert!(!names.contains(&"calendar_dates.txt"), "{names:?}");
    assert!(!names.contains(&"trips.txt"), "{names:?}");

    let import = |q: &str| {
        p_admin
            .req("POST", &format!("/feeds/{FEED}/migrations?{q}"))
            .set_payload(file.clone())
    };

    // a dry run: every step planned, prod's own edits kept
    let (s, b, _) = call!(
        &prod,
        import(&format!("dry_run=true&since_version={since}"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["step"], "stops_routes", "{b}");
    let first = &step(&b, "stops_routes")["planned"];
    assert_eq!(first["stop/update"], 1, "{b}");
    assert_eq!(first["geofence/create"], 1, "{b}");
    assert_eq!(first["route/create"], 1, "{b}");
    // R2's tag, and what R4's create does not take
    assert_eq!(first["route/update"], 2, "{b}");
    assert!(
        first.get("geofence/delete").is_none(),
        "prod's own fence stays: {b}"
    );
    assert_eq!(
        step(&b, "route_stops")["planned"]["route_stops/replace"],
        3,
        "R1, R1's second stop order and R4; R2 is prod's own: {b}"
    );
    let conflicts = ids(&b["conflicts"]);
    assert!(conflicts.contains(&"route_list:R2".to_string()), "{b}");
    assert!(conflicts.contains(&"geofence:C".to_string()), "{b}");
    assert!(
        !b["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["code"] == "not_in_file"),
        "the file carries everything: {b}"
    );

    // read the file in until prod has everything, a step at a time
    let mut drafted = Vec::new();
    let mut last = Value::Null;
    for round in 0..8 {
        let q = if round == 0 {
            format!("dry_run=false&since_version={since}")
        } else {
            "dry_run=false".to_string()
        };
        let (s, b, _) = call!(&prod, import(&q));
        assert_eq!(s, 200, "{b}");
        assert!(b["dropped"].as_array().unwrap().is_empty(), "{b}");
        let sets: Vec<String> = b["change_sets"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["change_set_id"].as_str().map(str::to_string))
            .collect();
        if sets.is_empty() {
            last = b;
            break;
        }
        drafted.push(b["step"].as_str().unwrap_or("").to_string());
        if b["step"] == "timetables" {
            let conflicts = ids(&b["conflicts"]);
            assert!(
                conflicts.contains(&"record:fare_attribute:PF".to_string()),
                "prod's own fare is kept: {b}"
            );
        }
        for set in sets {
            ship!(&prod, p_admin, p_ap, set);
        }
    }
    assert_eq!(
        last["next"], "prod has everything this file brings",
        "{last}"
    );
    assert_eq!(
        drafted,
        ["stops_routes", "route_stops", "timetables"],
        "every step with something to do, in order"
    );

    // ======================================================== prod has master's, and its own
    assert_eq!(served(&ppool, "R1", 1).await, served(&mpool, "R1", 1).await);
    assert_eq!(served(&ppool, "R1", 1).await.len(), 5);
    assert_eq!(served(&ppool, "R1", 2).await, served(&mpool, "R1", 2).await);
    assert_eq!(served(&ppool, "R4", 1).await, served(&mpool, "R4", 1).await);
    assert_eq!(
        served(&ppool, "R2", 1).await,
        r2_prod,
        "prod's own stop list"
    );
    assert_eq!(
        scalar_text(
            &ppool,
            format!(
                "SELECT route_tag FROM gtfs_route WHERE gtfs_id = '{FEED}' AND route_id = 'R2'"
            )
        )
        .await,
        "METRO_FEEDER"
    );
    assert_eq!(
        scalar_i64(
            &ppool,
            format!(
                "SELECT count(*) FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'B' AND is_depot"
            )
        )
        .await,
        1
    );
    assert_eq!(
        scalar_i64(
            &ppool,
            format!(
                "SELECT radius_m::bigint FROM gtfs_geofence WHERE gtfs_id = '{FEED}' AND stop_id = 'B'"
            )
        )
        .await,
        150
    );
    assert_eq!(
        scalar_i64(
            &ppool,
            format!(
                "SELECT count(*) FROM gtfs_geofence WHERE gtfs_id = '{FEED}' AND stop_id = 'C'"
            )
        )
        .await,
        1,
        "prod's own geofence"
    );
    assert_eq!(
        scalar_text(
            &ppool,
            format!(
                "SELECT agency_name FROM gtfs_agency WHERE gtfs_id = '{FEED}' AND agency_id = 'AG'"
            )
        )
        .await,
        "Shillong Shared Cabs"
    );
    assert_eq!(
        scalar_text(
            &ppool,
            format!(
                "SELECT string_agg(fare_id, ',' ORDER BY fare_id) FROM gtfs_fare_attribute \
                 WHERE gtfs_id = '{FEED}'"
            )
        )
        .await,
        "F1,PF",
        "master's fare, and prod's own"
    );
    assert_eq!(
        scalar_i64(
            &ppool,
            format!(
                "SELECT count(*) FROM gtfs_service WHERE gtfs_id = '{FEED}' AND service_id = 'WK'"
            )
        )
        .await,
        1
    );

    // ---- tidy up
    exec(&mpool, &clear_feed()).await;
    exec(&ppool, &clear_feed()).await;
    let _ = std::fs::remove_dir_all(mdir);
    let _ = std::fs::remove_dir_all(pdir);
}
