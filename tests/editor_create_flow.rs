//! Creating stops and routes, merging stops, bulk import and station proposals
//! (docs/gtfs-editor.md sections 5 and 6), end to end against a real Postgres
//! holding the editor schema (db/gtfs_editor/0001..0006).
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. Each test uses its own feed and accounts and removes its rows
//! afterwards; the timing test copies chennai_bus into its own feed and never
//! writes to chennai_bus. See scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use std::time::Instant;

const AUD: &str = "gtfs.editor-create-test.local";
const BASE: &str = "/internal/gtfs-editor";
const EMPTY_ROWS_HASH: &str = "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945";

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
            "DELETE" => test::TestRequest::delete(),
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

fn clear_feed(feed: &str) -> Vec<String> {
    vec![
        format!("DELETE FROM gtfs_station_proposal WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{feed}'"),
        format!("UPDATE gtfs_stop SET parent_station = NULL WHERE gtfs_id = '{feed}' AND parent_station IS NOT NULL"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{feed}'"),
    ]
}

fn reset_accounts(emails: &[&str]) -> Vec<String> {
    let list = emails
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        format!(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, totp_last_step = NULL, \
             status = 'active' WHERE email IN ({list})"
        ),
        format!(
            "DELETE FROM gtfs_editor_session WHERE user_id IN (SELECT user_id FROM gtfs_editor_user WHERE email IN ({list}))"
        ),
    ]
}

fn state(pool: &PgPool, signer: &TestSigner, admin: &str) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-create-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    use base64::Engine;
    let st = EditorState::build(
        pool.clone(),
        EditorSettings {
            totp_issuer: "GTFS Editor".into(),
            jwks_url: format!("file://{}", jwks.display()),
            audience: AUD.into(),
            bootstrap_admins: vec![admin.to_string()],
            totp_key_b64: base64::engine::general_purpose::STANDARD
                .encode(crypto::random_bytes(32)),
            session_hours: 1,
            ui_dir: dir.join("no-ui"),
            osrm_url: None,
            webhook_policy: Default::default(),
        },
    )
    .unwrap();
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

async fn scalar_text(pool: &PgPool, sql: impl AsRef<str>) -> Option<String> {
    let sql = sql.as_ref();
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<Option<String>, _>(0)
}

fn errors(v: &Value) -> Vec<Value> {
    v["validation"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| x["level"] == "error")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn has_code(list: &Value, code: &str) -> bool {
    list.as_array()
        .is_some_and(|a| a.iter().any(|x| x["code"] == code))
}

// ---------------------------------------------------------------- the flow

const FEED: &str = "editor_create_test_feed";
const ADMIN: &str = "admin@editor-create-test.invalid";
const EDITOR: &str = "editor@editor-create-test.invalid";
const APPROVER: &str = "approver@editor-create-test.invalid";

fn seed() -> Vec<String> {
    let mut s = clear_feed(FEED);
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{FEED}', 'Editor create test feed')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', 'S' || i, 'S' || i, 'STOP ' || i, 13.0 + i * 0.001, 80.2 FROM generate_series(1, 8) i"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
         ('{FEED}', 'D1', 'D1', 'DUP STOP', 13.05, 80.25), ('{FEED}', 'D2', 'D2', 'DUP STOP', 13.05, 80.25), \
         ('{FEED}', 'M1', 'M1', 'MANALI RD.JN', 13.16136, 80.26891), ('{FEED}', 'M2', 'M2', 'MANALI RD.JN', 13.16136, 80.26891), \
         ('{FEED}', 'N1', 'N1', 'OLD NAME', 13.06, 80.26), ('{FEED}', 'N2', 'N2', 'NEW NAME', 13.0601, 80.2601), \
         ('{FEED}', 'O1', 'O1', 'SPELLING A', 13.08, 80.28), ('{FEED}', 'O2', 'O2', 'SPELLING B', 13.08, 80.28), \
         ('{FEED}', 'K1', 'K1', 'STALE', 13.07, 80.27), ('{FEED}', 'K2', 'K2', 'STALE', 13.07, 80.27), \
         ('{FEED}', 'P1a', 'P1a', 'PLACE ONE', 13.1, 80.3), ('{FEED}', 'P1b', 'P1b', 'PLACE ONE', 13.1003, 80.3), \
         ('{FEED}', 'P2a', 'P2a', 'PLACE TWO', 13.2, 80.3), ('{FEED}', 'P2b', 'P2b', 'PLACE TWO', 13.2003, 80.3), \
         ('{FEED}', 'P3a', 'P3a', 'PLACE THREE', 13.3, 80.3), ('{FEED}', 'P3b', 'P3b', 'PLACE THREE', 13.3003, 80.3), \
         ('{FEED}', 'P4', 'P4', 'PLACE ONE', 13.1006, 80.3), \
         ('{FEED}', 'SDEL', 'SDEL', 'ONLY ON RDEL', 13.5, 80.5)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id) VALUES \
         ('{FEED}', 'R1', 'T1', 'STOP 1 To STOP 5', 'TESTAG'), ('{FEED}', 'RA', 'TA', 'A', 'TESTAG'), \
         ('{FEED}', 'RB', 'TB', 'B', 'TESTAG'), ('{FEED}', 'R56D', '56D', 'MANALI', 'TESTAG'), \
         ('{FEED}', 'RN', 'TN', 'N', 'TESTAG'), ('{FEED}', 'RO', 'TO', 'O', 'TESTAG'), \
         ('{FEED}', 'RK', 'TK', 'K', 'TESTAG'), ('{FEED}', 'RDEL', 'TDEL', 'DEL', 'TESTAG')"
    ));
    let rows = [
        ("R1", 1, "S1", "NEW STOP", 1, "STOP 1"),
        ("R1", 2, "S2", "INTERMEDIATE STOP", 1, "STOP 1"),
        ("R1", 3, "S3", "NEW STOP", 2, "STOP 3"),
        ("R1", 4, "S4", "INTERMEDIATE STOP", 2, "STOP 3"),
        ("R1", 5, "S5", "NEW STOP", 3, "STOP 5"),
        ("RA", 1, "S1", "NEW STOP", 1, "STOP 1"),
        ("RA", 2, "D1", "INTERMEDIATE STOP", 1, "STOP 1"),
        ("RA", 3, "S3", "NEW STOP", 2, "STOP 3"),
        ("RB", 1, "D2", "NEW STOP", 1, "DUP"),
        ("RB", 2, "S4", "INTERMEDIATE STOP", 1, "DUP"),
        ("RB", 3, "S5", "NEW STOP", 2, "STOP 5"),
        ("R56D", 1, "M1", "NEW STOP", 1, "MANALI"),
        ("R56D", 2, "M2", "NEW STOP", 2, "MANALI RD.JN"),
        ("R56D", 3, "S6", "NEW STOP", 3, "STOP 6"),
        ("RN", 1, "N1", "NEW STOP", 1, "OLD NAME"),
        ("RN", 2, "S7", "NEW STOP", 2, "STOP 7"),
        ("RO", 1, "O1", "NEW STOP", 1, "SPELLING A"),
        ("RO", 2, "S2", "NEW STOP", 2, "STOP 2"),
        ("RK", 1, "K1", "NEW STOP", 1, "STALE"),
        ("RK", 2, "S8", "NEW STOP", 2, "STOP 8"),
        ("RDEL", 1, "S6", "NEW STOP", 1, "STOP 6"),
        ("RDEL", 2, "SDEL", "NEW STOP", 2, "ONLY ON RDEL"),
    ];
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES {}",
        rows.iter()
            .map(|(r, q, st, t, n, name)| format!("('{FEED}', '{r}', {q}, '{st}', '{t}', {n}, '{name}', '7')"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let member = |id: &str, lat: f64, label: &str| json!({"stop_id": id, "name": "x", "lat": lat, "lon": 80.3, "platform_code": label, "route_count": 1});
    for (station, name, lat, members) in [
        (
            "stn_ta",
            "PLACE ONE",
            13.10015,
            vec![
                member("P1a", 13.1, "Towards X"),
                member("P1b", 13.1003, "Towards Y"),
            ],
        ),
        (
            "stn_tb",
            "PLACE TWO",
            13.20015,
            vec![
                member("P2a", 13.2, "Towards X"),
                member("P2b", 13.2003, "Towards Y"),
            ],
        ),
        (
            "stn_tc",
            "PLACE THREE",
            13.30015,
            vec![
                member("P3a", 13.3, "Towards X"),
                member("P3b", 13.3003, "Towards Y"),
            ],
        ),
        (
            "stn_td",
            "PLACE ONE AGAIN",
            13.1003,
            vec![
                member("P1a", 13.1, "Towards X"),
                member("P4", 13.1006, "Towards Z"),
            ],
        ),
    ] {
        s.push(format!(
            "INSERT INTO gtfs_station_proposal (gtfs_id, batch, station_id, name, lat, lon, members, spread_m) \
             VALUES ('{FEED}', 'test-batch', '{station}', '{name}', {lat}, 80.3, '{}'::jsonb, 33)",
            Value::Array(members)
        ));
    }
    s.extend(reset_accounts(&[ADMIN, EDITOR, APPROVER]));
    s
}

async fn proposal_id(pool: &PgPool, station: &str) -> i64 {
    scalar_i64(
        pool,
        &format!(
            "SELECT proposal_id FROM gtfs_station_proposal WHERE gtfs_id = '{FEED}' AND station_id = '{station}'"
        ),
    )
    .await
}

#[actix_web::test]
async fn create_merge_bulk_and_proposals() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("create-test-key");
    let (st, dir) = state(&pool, &signer, ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

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

    let new_set = |c: &Caller, title: &str| {
        c.req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": title}))
    };
    let feed_version = || {
        scalar_i64(
            &pool,
            format!("SELECT version FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
        )
    };

    // ======================================================== create
    let version0 = feed_version().await;
    let (s, set, _) = call!(&app, new_set(&editor_c, "create a stop and a route"));
    assert_eq!(s, 201, "{set}");
    let c1 = set["change_set_id"].as_str().unwrap().to_string();
    let add = |c: &Caller, set: &str, change: Value| {
        c.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(change)
    };

    // a stop without an id gets one minted
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c1,
            json!({"entity": "stop", "op": "create", "after": {
        "name": "NEW KERB", "lat": 13.0095, "lon": 80.2005, "platform_code": "Towards STOP 3"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let created = b["changes"].as_array().unwrap().last().unwrap().clone();
    let minted = created["entity_key"].as_str().unwrap().to_string();
    assert!(minted.starts_with("ed_") && minted.len() == 13, "{minted}");
    assert!(minted[3..]
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(created["after"]["stop_id"], minted.as_str());
    assert_eq!(b["change_id"], created["change_id"]);

    // a route, its entity_key taken from after; defaults written into the change
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c1,
            json!({"entity": "route", "op": "create", "after": {
        "route_id": "NR1", "short_name": "N1", "long_name": "STOP 1 To STOP 3", "color": "#0a7e3c"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let route_change = b["changes"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(route_change["entity_key"], "NR1");
    assert_eq!(route_change["after"]["route_type"], 3);
    assert_eq!(route_change["after"]["agency_id"], "TESTAG");

    // the new route previews with no rows: its stop list's base is the hash of []
    let (s, preview, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{c1}/preview/routes/NR1"))
    );
    assert_eq!(s, 200, "{preview}");
    assert_eq!(preview["rows"], json!([]));
    assert_eq!(preview["rows_hash"], EMPTY_ROWS_HASH);
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c1,
            json!({"entity": "route_stops", "op": "replace", "entity_key": "NR1", "after": {
        "base_rows_hash": EMPTY_ROWS_HASH,
        "rows": [
            {"stop_id": "S1", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "STOP 1"},
            {"stop_id": minted, "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "STOP 1"},
            {"stop_id": "S3", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "STOP 3"}]}})
        )
    );
    assert_eq!(s, 201, "{b}");
    assert_eq!(
        b["changes"].as_array().unwrap().last().unwrap()["before"],
        json!([])
    );
    // the new stop counts as existing for a later edit
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c1,
            json!({"entity": "stop", "op": "update", "entity_key": minted, "after": {"name": "NEW KERB RENAMED"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    assert_eq!(
        b["changes"].as_array().unwrap().last().unwrap()["before"]["name"],
        "NEW KERB"
    );
    assert!(errors(&b).is_empty(), "{b}");
    assert_eq!(b["conflicts"], json!([]), "{b}");
    assert_eq!(b["can_submit"], true, "{b}");
    assert_eq!(b["stop_names"][&minted], "NEW KERB", "{b}");
    let (s, preview, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{c1}/preview/routes/NR1"))
    );
    assert_eq!(s, 200, "{preview}");
    assert_eq!(preview["stop_count"], 3);
    assert_eq!(preview["rows"][1]["stop_id"], minted.as_str());
    assert_eq!(preview["rows"][1]["stop_name"], "NEW KERB RENAMED");
    assert_eq!(preview["short_name"], "N1");
    assert!(errors(&preview).is_empty(), "{preview}");

    // refusals, in a draft of their own
    let (_, c0, _) = call!(&app, new_set(&editor_c, "refusals"));
    let c0 = c0["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c0,
            json!({"entity": "route", "op": "create", "after": {"route_id": "R1", "short_name": "T1"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    assert!(has_code(&b["validation"], "route_exists"), "{b}");
    assert_eq!(b["conflicts"][0]["reason"], "exists", "{b}");
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c0,
            json!({"entity": "stop", "op": "create", "after": {"stop_id": "A:B", "name": "x", "lat": 13.0, "lon": 80.0}})
        )
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c0,
            json!({"entity": "route_stops", "op": "replace", "entity_key": "NOPE", "after": {"base_rows_hash": EMPTY_ROWS_HASH, "rows": []}})
        )
    );
    assert_eq!((s, code_of(&b)), (404, "entity_not_found"), "{b}");
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c0,
            json!({"entity": "route", "op": "update", "entity_key": "R1", "after": {"color": "#112233"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c0,
            json!({"entity": "route", "op": "delete", "entity_key": "R1"})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "route_has_pending_changes"), "{b}");
    // the dashboard's shape: a blank or null entity_key also means "mint an id"
    for key in [json!(""), Value::Null] {
        let (s, b, _) = call!(
            &app,
            add(
                &editor_c,
                &c0,
                json!({"entity": "stop", "op": "create", "entity_key": key, "after": {
            "name": "BLANK KEY KERB", "lat": 13.0096, "lon": 80.2006}})
            )
        );
        assert_eq!(s, 201, "{key}: {b}");
        let made = b["changes"].as_array().unwrap().last().unwrap().clone();
        let id = made["entity_key"].as_str().unwrap();
        assert!(id.starts_with("ed_") && id.len() == 13, "{made}");
        assert_eq!(made["after"]["stop_id"], id, "{made}");
    }
    // a station groups at least two stops, and the refusal says so
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c0,
            json!({"entity": "station", "op": "create", "entity_key": "stn_one", "after": {
        "station_id": "stn_one", "name": "ONE STOP", "lat": 13.0, "lon": 80.2, "members": [{"stop_id": "S1"}]}})
        )
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
    assert_eq!(b["error"]["details"]["code"], "too_few_members", "{b}");
    assert_eq!(
        b["error"]["message"],
        "station/create: a station groups at least two stops, and stn_one would have only one"
    );
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{c0}/discard"))
    );
    assert_eq!(s, 200);

    // submit, a different person approves and commits
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{c1}/submit"))
    );
    assert_eq!((s, b["status"].clone()), (200, json!("submitted")), "{b}");
    let (s, b, _) = call!(
        &app,
        approver
            .req("POST", &format!("/change-sets/{c1}/approve"))
            .set_json(json!({"comment": "ok"}))
    );
    assert_eq!((s, b["status"].clone()), (200, json!("approved")), "{b}");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{c1}/commit"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["feed_version"].as_i64().unwrap(), version0 + 1);
    assert_eq!(feed_version().await, version0 + 1);
    let route = sqlx::query(&format!(
        "SELECT short_name, long_name, route_type, agency_id, color, deleted FROM gtfs_route WHERE gtfs_id = '{FEED}' AND route_id = 'NR1'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        route.get::<Option<String>, _>("short_name").as_deref(),
        Some("N1")
    );
    assert_eq!(route.get::<i16, _>("route_type"), 3);
    assert_eq!(
        route.get::<Option<String>, _>("agency_id").as_deref(),
        Some("TESTAG")
    );
    assert_eq!(
        route.get::<Option<String>, _>("color").as_deref(),
        Some("#0A7E3C")
    );
    let order: Vec<String> = sqlx::query(&format!(
        "SELECT stop_id FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'NR1' ORDER BY sequence"
    ))
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get("stop_id"))
    .collect();
    assert_eq!(
        order,
        vec!["S1".to_string(), minted.clone(), "S3".to_string()]
    );
    let stop = sqlx::query(&format!(
        "SELECT name, stop_code, platform_code, lat, deleted FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = '{minted}'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stop.get::<String, _>("name"), "NEW KERB RENAMED");
    assert_eq!(
        stop.get::<Option<String>, _>("stop_code").as_deref(),
        Some(minted.as_str())
    );
    assert_eq!(
        stop.get::<Option<String>, _>("platform_code").as_deref(),
        Some("Towards STOP 3")
    );

    // route delete: a soft delete, gone from the route list
    let (_, c2, _) = call!(&app, new_set(&editor_c, "delete a route"));
    let c2 = c2["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c2,
            json!({"entity": "route", "op": "delete", "entity_key": "RDEL"})
        )
    );
    assert_eq!(s, 201, "{b}");
    assert!(errors(&b).is_empty(), "{b}");
    assert_eq!(b["changes"][0]["before"]["route_id"], "RDEL");
    for (who, action) in [
        (&editor_c, "submit"),
        (&approver, "approve"),
        (&approver, "commit"),
    ] {
        let (s, b, _) = call!(
            &app,
            who.req("POST", &format!("/change-sets/{c2}/{action}"))
        );
        assert_eq!(s, 200, "{action}: {b}");
    }
    assert_eq!(
        scalar_text(&pool, &format!("SELECT deleted::text FROM gtfs_route WHERE gtfs_id = '{FEED}' AND route_id = 'RDEL'")).await.as_deref(),
        Some("true")
    );
    assert_eq!(scalar_i64(&pool, &format!("SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'RDEL'")).await, 2);
    let (_, listed, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes?q=RDEL"))
    );
    assert_eq!(listed["items"], json!([]), "{listed}");
    // a stop only a deleted route calls at can be deleted; one a live route
    // still calls at cannot, and the deleted route is not counted
    let (_, c3, _) = call!(&app, new_set(&editor_c, "delete stops"));
    let c3 = c3["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c3,
            json!({"entity": "stop", "op": "delete", "entity_key": "SDEL"})
        )
    );
    assert_eq!(s, 201, "{b}");
    assert!(errors(&b).is_empty(), "{b}");
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c3,
            json!({"entity": "stop", "op": "delete", "entity_key": "S6"})
        )
    );
    assert_eq!(s, 201, "{b}");
    let in_use = b["validation"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["code"] == "stop_in_use")
        .cloned();
    let in_use = in_use.unwrap_or_else(|| panic!("{b}"));
    assert!(
        in_use["message"]
            .as_str()
            .unwrap()
            .contains("1 route(s) (R56D)"),
        "{in_use}"
    );
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{c3}/discard"))
    );
    assert_eq!(s, 200);

    // ======================================================== merge
    let stop_version = |id: &'static str| {
        scalar_i64(&pool, format!("SELECT row_version::int8 FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = '{id}'"))
    };
    let (_, g1, _) = call!(&app, new_set(&editor_c, "merge duplicates"));
    let g1 = g1["change_set_id"].as_str().unwrap().to_string();
    let (d1_version, d2_version) = (stop_version("D1").await, stop_version("D2").await);
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &g1,
            json!({"entity": "stop", "op": "merge", "entity_key": "D1", "after": {"into_stop_id": "D2"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let merge = b["changes"][0].clone();
    assert_eq!(merge["base_row_version"].as_i64(), Some(d1_version));
    assert_eq!(
        merge["after"]["into_row_version"].as_i64(),
        Some(d2_version)
    );
    assert_eq!(merge["before"]["from"]["stop_id"], "D1");
    assert_eq!(merge["before"]["into"]["stop_id"], "D2");
    assert_eq!(
        merge["before"]["affected"],
        json!([{"route_id": "RA", "short_name": "TA", "sequences": [2]}])
    );
    assert!(errors(&b).is_empty(), "{b}");
    // previews show the kept id
    let (_, preview, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{g1}/preview/routes/RA"))
    );
    assert_eq!(preview["rows"][1]["stop_id"], "D2", "{preview}");
    // a later stop list that still uses the merged stop is refused
    let (_, ra, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/RA"))
    );
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &g1,
            json!({"entity": "route_stops", "op": "replace", "entity_key": "RA", "after": {
        "base_rows_hash": ra["rows_hash"],
        "rows": [
            {"stop_id": "S1", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "STOP 1"},
            {"stop_id": "D1", "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "STOP 1"},
            {"stop_id": "S3", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "STOP 3"}]}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let stale_list = b["change_id"].as_i64().unwrap();
    assert!(
        b["validation"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["code"] == "stop_merged_away" && v["change_id"] == stale_list),
        "{b}"
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req("DELETE", &format!("/change-sets/{g1}/changes/{stale_list}"))
    );
    assert_eq!(s, 200, "{b}");
    // different names: keep the merged stop's name, or keep the route's spelling
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &g1,
            json!({"entity": "stop", "op": "merge", "entity_key": "N1", "after": {"into_stop_id": "N2", "keep_name": "from"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    assert!(has_code(&b["validation"], "merge_names_differ"), "{b}");
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &g1,
            json!({"entity": "stop", "op": "merge", "entity_key": "O1", "after": {"into_stop_id": "O2"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    assert!(errors(&b).is_empty(), "{b}");
    for (who, action) in [
        (&editor_c, "submit"),
        (&approver, "approve"),
        (&approver, "commit"),
    ] {
        let (s, b, _) = call!(
            &app,
            who.req("POST", &format!("/change-sets/{g1}/{action}"))
        );
        assert_eq!(s, 200, "{action}: {b}");
    }
    let row = |route: &str, seq: i32| {
        format!("SELECT stop_id, stop_name_override, provenance->>'merged_from' AS merged_from FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = '{route}' AND sequence = {seq}")
    };
    let ra2 = sqlx::query(&row("RA", 2)).fetch_one(&pool).await.unwrap();
    assert_eq!(
        ra2.get::<Option<String>, _>("stop_id").as_deref(),
        Some("D2")
    );
    assert_eq!(
        ra2.get::<Option<String>, _>("merged_from").as_deref(),
        Some("D1")
    );
    assert_eq!(ra2.get::<Option<String>, _>("stop_name_override"), None);
    let d1 = sqlx::query(&format!("SELECT deleted, provenance->>'merged_into' AS into FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'D1'")).fetch_one(&pool).await.unwrap();
    assert!(d1.get::<bool, _>("deleted"));
    assert_eq!(d1.get::<Option<String>, _>("into").as_deref(), Some("D2"));
    assert_eq!(scalar_i64(&pool, &format!("SELECT count(*) FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'D2' AND NOT deleted")).await, 1);
    assert_eq!(
        scalar_i64(
            &pool,
            &format!(
                "SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'D1'"
            )
        )
        .await,
        0
    );
    // keep_name "from": the kept stop is renamed, the moved row needs no spelling
    assert_eq!(
        scalar_text(
            &pool,
            &format!("SELECT name FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'N2'")
        )
        .await
        .as_deref(),
        Some("OLD NAME")
    );
    let rn1 = sqlx::query(&row("RN", 1)).fetch_one(&pool).await.unwrap();
    assert_eq!(
        rn1.get::<Option<String>, _>("stop_id").as_deref(),
        Some("N2")
    );
    assert_eq!(rn1.get::<Option<String>, _>("stop_name_override"), None);
    // keep_name "into": the moved row keeps the route's own spelling
    let ro1 = sqlx::query(&row("RO", 1)).fetch_one(&pool).await.unwrap();
    assert_eq!(
        ro1.get::<Option<String>, _>("stop_id").as_deref(),
        Some("O2")
    );
    assert_eq!(
        ro1.get::<Option<String>, _>("stop_name_override")
            .as_deref(),
        Some("SPELLING A")
    );
    assert_eq!(
        scalar_text(
            &pool,
            &format!("SELECT name FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'O2'")
        )
        .await
        .as_deref(),
        Some("SPELLING B")
    );
    let merges = scalar_i64(&pool, &format!("SELECT count(*) FROM gtfs_audit_log WHERE change_set_id = '{g1}' AND action = 'stop_merged' AND (detail->>'rows')::int = 1 AND (detail->>'routes')::int = 1")).await;
    assert_eq!(merges, 3);

    // back to back on 56D: refused
    let (_, g2, _) = call!(&app, new_set(&editor_c, "merge manali"));
    let g2 = g2["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &g2,
            json!({"entity": "stop", "op": "merge", "entity_key": "M1", "after": {"into_stop_id": "M2"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let repeat = b["validation"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["code"] == "merge_would_repeat_stop")
        .cloned();
    let repeat = repeat.unwrap_or_else(|| panic!("{b}"));
    assert_eq!(repeat["level"], "error");
    let message = repeat["message"].as_str().unwrap();
    assert!(
        message.contains("R56D") && message.contains("sequences 1 and 2"),
        "{message}"
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{g2}/submit"))
    );
    assert_eq!((s, code_of(&b)), (400, "validation_failed"), "{b}");
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{g2}/discard"))
    );
    assert_eq!(s, 200);

    // a stale kept stop: 409 at commit
    let (_, g3, _) = call!(&app, new_set(&editor_c, "merge stale"));
    let g3 = g3["change_set_id"].as_str().unwrap().to_string();
    let k2_version = stop_version("K2").await;
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &g3,
            json!({"entity": "stop", "op": "merge", "entity_key": "K1", "after": {"into_stop_id": "K2", "into_row_version": k2_version}})
        )
    );
    assert_eq!(s, 201, "{b}");
    for (who, action) in [(&editor_c, "submit"), (&approver, "approve")] {
        let (s, b, _) = call!(
            &app,
            who.req("POST", &format!("/change-sets/{g3}/{action}"))
        );
        assert_eq!(s, 200, "{action}: {b}");
    }
    let (_, g4, _) = call!(&app, new_set(&editor_c, "rename the kept stop"));
    let g4 = g4["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &g4,
            json!({"entity": "stop", "op": "update", "entity_key": "K2", "after": {"name": "STALE RENAMED"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    for (who, action) in [
        (&editor_c, "submit"),
        (&approver, "approve"),
        (&approver, "commit"),
    ] {
        let (s, b, _) = call!(
            &app,
            who.req("POST", &format!("/change-sets/{g4}/{action}"))
        );
        assert_eq!(s, 200, "{action}: {b}");
    }
    let before_stale = feed_version().await;
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{g3}/commit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    let conflict = &b["error"]["details"]["conflicts"][0];
    assert_eq!(
        (conflict["entity_key"].clone(), conflict["reason"].clone()),
        (json!("K2"), json!("changed")),
        "{b}"
    );
    assert_eq!(conflict["expected"].as_i64(), Some(k2_version));
    assert_eq!(feed_version().await, before_stale);
    assert_eq!(
        scalar_i64(
            &pool,
            &format!(
                "SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'K1'"
            )
        )
        .await,
        1
    );

    // ======================================================== bulk
    let (_, b1, _) = call!(&app, new_set(&editor_c, "bulk import"));
    let b1 = b1["change_set_id"].as_str().unwrap().to_string();
    let bulk = |c: &Caller, kind: &str, rows: Value, dry_run: bool| {
        c.req("POST", &format!("/change-sets/{b1}/bulk"))
            .set_json(json!({"kind": kind, "rows": rows, "dry_run": dry_run}))
    };
    let mixed = json!([
        {"action": "add", "name": "BULK A", "lat": "13.4", "lon": "80.1"},
        {"action": "add", "stop_id": "BULK1", "name": "BULK B", "lat": 13.41, "lon": 80.11, "platform_code": ""},
        {"action": "add", "stop_id": "S1", "name": "TAKEN", "lat": 13.42, "lon": 80.12},
        {"action": "add", "name": "BAD LAT", "lat": "north", "lon": 80.1},
        {"action": "add", "stop_id": "BULK2", "name": "TWICE", "lat": 13.43, "lon": 80.13},
        {"action": "add", "stop_id": "BULK2", "name": "TWICE AGAIN", "lat": 13.44, "lon": 80.14},
        {"action": "add", "name": "OFF THE MAP", "lat": 95, "lon": 80},
        {"action": "add", "name": "NOTES", "lat": 13.45, "lon": 80.15, "notes": "x"},
    ]);
    let (s, out, _) = call!(&app, bulk(&editor_c, "stops", mixed.clone(), true));
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["dry_run"], true);
    assert_eq!(
        out["summary"],
        json!({"rows": 8, "ok": 2, "warnings": 0, "errors": 6, "changes": 6}),
        "{out}"
    );
    let status: Vec<&str> = out["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        status,
        vec!["ok", "ok", "error", "error", "error", "error", "error", "error"]
    );
    let first_code = |i: usize| {
        out["rows"][i]["messages"][0]["code"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(
        (1..8).map(first_code).collect::<Vec<_>>(),
        vec![
            "",
            "stop_exists",
            "invalid_row",
            "duplicate_in_upload",
            "duplicate_in_upload",
            "invalid_position",
            "invalid_row"
        ]
    );
    assert_eq!(out["rows"][0]["row"], 1);
    assert_eq!(
        out["rows"][0]["change"],
        json!({"entity": "stop", "op": "create", "entity_key": null})
    );
    assert_eq!(out["rows"][1]["change"]["entity_key"], "BULK1");
    assert!(out["rows"][3]["change"].is_null());
    assert_eq!(out["changes_preview"].as_array().unwrap().len(), 6);
    let change_count = |v: &Value| v["change_count"].as_i64().unwrap();
    let (_, set_now, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{b1}")));
    assert_eq!(change_count(&set_now), 0);
    let (s, refused, _) = call!(&app, bulk(&editor_c, "stops", mixed, false));
    assert_eq!(
        (s, code_of(&refused)),
        (400, "bulk_has_errors"),
        "{refused}"
    );
    assert_eq!(refused["error"]["details"]["summary"]["errors"], 6);
    let (_, set_now, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{b1}")));
    assert_eq!(change_count(&set_now), 0);
    let (s, out, _) = call!(
        &app,
        bulk(
            &editor_c,
            "stops",
            json!([
                {"action": "add", "name": "BULK A", "lat": "13.4", "lon": "80.1"},
                {"action": "add", "stop_id": "BULK1", "name": "BULK B", "lat": 13.41, "lon": 80.11, "platform_code": ""},
                {"action": "add", "stop_id": "BULK2", "name": "BULK C", "lat": 13.43, "lon": 80.13, "platform_code": "Towards BULK B"},
            ]),
            false
        )
    );
    assert_eq!(s, 200, "{out}");
    assert_eq!(
        out["summary"],
        json!({"rows": 3, "ok": 3, "warnings": 0, "errors": 0, "changes": 3})
    );
    let bulk_minted = out["rows"][0]["change"]["entity_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(bulk_minted.starts_with("ed_") && bulk_minted.len() == 13);
    assert!(out["rows"][0]["change"]["change_id"].is_i64());
    assert_eq!(change_count(&out["change_set"]), 3);
    assert_eq!(out["change_set"]["can_submit"], true, "{out}");
    // routes, then their stop lists referencing the stops and routes just imported
    let (s, out, _) = call!(
        &app,
        bulk(
            &editor_c,
            "routes",
            json!([
                {"action": "add", "route_id": "BR1", "short_name": "B1", "long_name": "BULK A To BULK C", "color": "#123abc"},
            ]),
            false
        )
    );
    assert_eq!(s, 200, "{out}");
    let (s, out, _) = call!(
        &app,
        bulk(
            &editor_c,
            "routes",
            json!([{"action": "add", "route_id": "BR1", "short_name": "again"}, {"action": "add", "route_id": "R1", "short_name": "T1"}]),
            true
        )
    );
    assert_eq!(s, 200, "{out}");
    assert_eq!(
        out["rows"][0]["messages"][0]["code"], "route_exists",
        "{out}"
    );
    assert_eq!(
        out["rows"][1]["messages"][0]["code"], "route_exists",
        "{out}"
    );
    let fare_error = json!([
        {"action": "add", "route_id": "BR1", "sequence": 3, "stop_id": "BULK2", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "BULK C"},
        {"action": "add", "route_id": "BR1", "sequence": "1", "stop_id": bulk_minted, "stop_type": "NEW STOP", "stage_no": "1", "stage_name": "BULK A"},
        {"action": "add", "route_id": "BR1", "sequence": 2, "stop_id": "BULK1", "stop_type": "INTERMEDIATE STOP", "stage_no": 2, "stage_name": "BULK C"},
    ]);
    let (s, refused, _) = call!(
        &app,
        bulk(&editor_c, "route_stops", fare_error.clone(), false)
    );
    assert_eq!(
        (s, code_of(&refused)),
        (400, "bulk_has_errors"),
        "{refused}"
    );
    let rows = &refused["error"]["details"]["rows"];
    assert_eq!(rows[2]["status"], "error", "{refused}");
    assert_eq!(
        rows[2]["messages"][0]["code"], "fare_stage_mismatch",
        "{refused}"
    );
    assert!(
        rows[2]["messages"][0]["message"]
            .as_str()
            .unwrap()
            .starts_with("sequence 2 (BULK1)"),
        "{refused}"
    );
    assert_eq!(
        (rows[0]["status"].clone(), rows[1]["status"].clone()),
        (json!("ok"), json!("ok"))
    );
    assert_eq!(
        rows[0]["change"],
        json!({"entity": "route_stops", "op": "replace", "entity_key": "BR1"})
    );
    let (_, set_now, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{b1}")));
    assert_eq!(change_count(&set_now), 4);
    // more route-list problems in one preview
    let (s, out, _) = call!(
        &app,
        bulk(
            &editor_c,
            "route_stops",
            json!([
                {"action": "add", "route_id": "BR1", "sequence": 1, "stop_id": "BULK1", "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "BULK A"},
                {"action": "add", "route_id": "BR1", "sequence": 1, "stop_id": "BULK2", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "BULK A"},
                {"action": "add", "route_id": "GHOST", "sequence": 1, "stop_id": "S1", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "STOP 1"},
                {"action": "update", "route_id": "R1", "sequence": 1, "stop_id": "NOSUCH", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "STOP 1"},
                {"action": "update", "route_id": "R1", "sequence": 2, "stop_id": "S5", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "STOP 5"},
            ]),
            true
        )
    );
    assert_eq!(s, 200, "{out}");
    let codes = |i: usize| {
        out["rows"][i]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["code"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert!(
        codes(0).contains(&"duplicate_in_upload".to_string())
            && codes(0).contains(&"first_stop_not_stage".to_string()),
        "{out}"
    );
    assert_eq!(codes(2), vec!["route_not_found"], "{out}");
    assert_eq!(codes(3), vec!["unknown_stop"], "{out}");
    let (s, out, _) = call!(
        &app,
        bulk(
            &editor_c,
            "route_stops",
            json!([
                {"action": "add", "route_id": "BR1", "sequence": 3, "stop_id": "BULK2", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "BULK C"},
                {"action": "add", "route_id": "BR1", "sequence": 1, "stop_id": bulk_minted, "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "BULK A"},
                {"action": "add", "route_id": "BR1", "sequence": 2, "stop_id": "BULK1", "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "BULK A"},
            ]),
            false
        )
    );
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["summary"]["errors"], 0, "{out}");
    assert_eq!(out["summary"]["changes"], 1);
    assert_eq!(change_count(&out["change_set"]), 5);
    assert!(errors(&out["change_set"]).is_empty(), "{out}");
    let audit_bulk = scalar_i64(&pool, &format!("SELECT count(*) FROM gtfs_audit_log WHERE change_set_id = '{b1}' AND action = 'bulk_imported'")).await;
    assert_eq!(audit_bulk, 3);
    for (who, action) in [
        (&editor_c, "submit"),
        (&approver, "approve"),
        (&approver, "commit"),
    ] {
        let (s, b, _) = call!(
            &app,
            who.req("POST", &format!("/change-sets/{b1}/{action}"))
        );
        assert_eq!(s, 200, "{action}: {b}");
    }
    let br1: Vec<String> = sqlx::query(&format!("SELECT stop_id FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'BR1' ORDER BY sequence"))
        .fetch_all(&pool).await.unwrap().iter().map(|r| r.get("stop_id")).collect();
    assert_eq!(
        br1,
        vec![bulk_minted.clone(), "BULK1".into(), "BULK2".into()]
    );
    assert_eq!(scalar_text(&pool, &format!("SELECT platform_code FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'BULK2'")).await.as_deref(), Some("Towards BULK B"));

    // ================================================ bulk: update and delete
    // a stop and a route to change and remove, live before the upload that does it
    let (_, extra, _) = call!(&app, new_set(&editor_c, "bulk extras"));
    let extra = extra["change_set_id"].as_str().unwrap().to_string();
    for (kind, rows) in [
        (
            "stops",
            json!([{"action": "add", "stop_id": "BULK3", "name": "BULK D", "lat": 13.46, "lon": 80.16}]),
        ),
        (
            "routes",
            json!([{"action": "add", "route_id": "BR2", "short_name": "B2"}]),
        ),
    ] {
        let (s, out, _) = call!(
            &app,
            editor_c
                .req("POST", &format!("/change-sets/{extra}/bulk"))
                .set_json(json!({"kind": kind, "rows": rows, "dry_run": false}))
        );
        assert_eq!(s, 200, "{kind}: {out}");
    }
    for (who, action) in [
        (&editor_c, "submit"),
        (&approver, "approve"),
        (&approver, "commit"),
    ] {
        let (s, b, _) = call!(
            &app,
            who.req("POST", &format!("/change-sets/{extra}/{action}"))
        );
        assert_eq!(s, 200, "{action}: {b}");
    }
    let (_, b2, _) = call!(&app, new_set(&editor_c, "bulk update and delete"));
    let b2 = b2["change_set_id"].as_str().unwrap().to_string();
    let bulk2 = |c: &Caller, kind: &str, rows: Value, dry_run: bool| {
        c.req("POST", &format!("/change-sets/{b2}/bulk"))
            .set_json(json!({"kind": kind, "rows": rows, "dry_run": dry_run}))
    };
    // every row says what it does: a blank action is refused, whatever else is right
    let (s, out, _) = call!(
        &app,
        bulk2(
            &editor_c,
            "stops",
            json!([{"stop_id": "BULK1", "name": "NO ACTION"}]),
            true
        )
    );
    assert_eq!(s, 200, "{out}");
    assert_eq!(
        out["rows"][0]["messages"][0]["code"], "invalid_row",
        "{out}"
    );
    assert!(
        out["rows"][0]["messages"][0]["message"]
            .as_str()
            .unwrap()
            .contains("action is required"),
        "{out}"
    );
    // add on an id that is there, update and delete on one that is not
    let (s, out, _) = call!(
        &app,
        bulk2(
            &editor_c,
            "stops",
            json!([
                {"action": "add", "stop_id": "BULK1", "name": "AGAIN", "lat": 13.4, "lon": 80.1},
                {"action": "update", "stop_id": "NOSUCH1", "name": "GHOST"},
                {"action": "delete", "stop_id": "NOSUCH2"},
                {"action": "update", "stop_id": "BULK2"},
                {"action": "delete", "stop_id": "BULK3", "name": "WITH A NAME"},
                {"action": "sideways", "stop_id": "S1"},
            ]),
            true
        )
    );
    assert_eq!(s, 200, "{out}");
    let code = |i: usize| {
        out["rows"][i]["messages"][0]["code"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(
        (0..6).map(code).collect::<Vec<_>>(),
        vec![
            "stop_exists",
            "stop_not_found",
            "stop_not_found",
            "nothing_to_update",
            "invalid_row",
            "invalid_row"
        ],
        "{out}"
    );
    // the add row is planned before its id is checked, as any errored row may be;
    // the upload is refused as a whole, so nothing of it reaches the draft
    assert_eq!(out["summary"]["changes"], 1, "{out}");
    assert_eq!(out["summary"]["errors"], 6, "{out}");
    // the real thing: move and rename one stop, relabel another, delete a third,
    // change a route's name and colour, and delete a route that has no stop list
    let (s, out, _) = call!(
        &app,
        bulk2(
            &editor_c,
            "stops",
            json!([
                {"action": "update", "stop_id": "BULK1", "name": "BULK B MOVED", "lat": 13.5, "lon": 80.2},
                {"action": "update", "stop_id": "BULK2", "platform_code": "Towards BULK A"},
                {"action": "delete", "stop_id": "BULK3"},
            ]),
            false
        )
    );
    assert_eq!(s, 200, "{out}");
    assert_eq!(
        out["summary"],
        json!({"rows": 3, "ok": 3, "warnings": 0, "errors": 0, "changes": 3}),
        "{out}"
    );
    let ops: Vec<String> = out["changes_preview"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{}/{}",
                c["entity"].as_str().unwrap(),
                c["op"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(ops, vec!["stop/update", "stop/update", "stop/delete"]);
    let (s, out, _) = call!(
        &app,
        bulk2(
            &editor_c,
            "routes",
            json!([
                {"action": "update", "route_id": "BR1", "long_name": "BULK A To BULK C, renamed", "color": "#abcdef"},
                {"action": "delete", "route_id": "BR2"},
            ]),
            false
        )
    );
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["summary"]["errors"], 0, "{out}");
    // a route's stop list is one action, and it must match what the route has
    let (s, out, _) = call!(
        &app,
        bulk2(
            &editor_c,
            "route_stops",
            json!([
                {"action": "add", "route_id": "BR1", "sequence": 1, "stop_id": "BULK1", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "A"},
                {"action": "update", "route_id": "R1", "sequence": 1, "stop_id": "S1", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "STOP 1"},
                {"action": "add", "route_id": "R1", "sequence": 2, "stop_id": "S5", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "STOP 5"},
            ]),
            true
        )
    );
    assert_eq!(s, 200, "{out}");
    // BR1 has the list the earlier import gave it; R1's rows disagree with each other
    assert_eq!(
        out["rows"][0]["messages"][0]["code"], "route_stops_exist",
        "{out}"
    );
    assert_eq!(
        out["rows"][1]["messages"][0]["code"], "mixed_action",
        "{out}"
    );
    assert_eq!(
        out["rows"][2]["messages"][0]["code"], "mixed_action",
        "{out}"
    );

    for (who, action) in [
        (&editor_c, "submit"),
        (&approver, "approve"),
        (&approver, "commit"),
    ] {
        let (s, b, _) = call!(
            &app,
            who.req("POST", &format!("/change-sets/{b2}/{action}"))
        );
        assert_eq!(s, 200, "{action}: {b}");
    }
    assert_eq!(
        scalar_text(
            &pool,
            &format!("SELECT name FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'BULK1'")
        )
        .await
        .as_deref(),
        Some("BULK B MOVED")
    );
    assert_eq!(
        scalar_text(
            &pool,
            &format!(
                "SELECT lat::text FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'BULK1'"
            )
        )
        .await
        .as_deref(),
        Some("13.5")
    );
    assert_eq!(
        scalar_text(&pool, &format!("SELECT platform_code FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'BULK2'")).await.as_deref(),
        Some("Towards BULK A")
    );
    assert_eq!(
        scalar_i64(&pool, &format!("SELECT count(*) FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'BULK3' AND deleted")).await,
        1
    );
    assert_eq!(
        scalar_text(
            &pool,
            &format!(
                "SELECT long_name FROM gtfs_route WHERE gtfs_id = '{FEED}' AND route_id = 'BR1'"
            )
        )
        .await
        .as_deref(),
        Some("BULK A To BULK C, renamed")
    );
    assert_eq!(
        scalar_i64(&pool, &format!("SELECT count(*) FROM gtfs_route WHERE gtfs_id = '{FEED}' AND route_id = 'BR2' AND deleted")).await,
        1
    );

    // ======================================================== station proposals
    let (pa, pb, pc, pd) = (
        proposal_id(&pool, "stn_ta").await,
        proposal_id(&pool, "stn_tb").await,
        proposal_id(&pool, "stn_tc").await,
        proposal_id(&pool, "stn_td").await,
    );
    let (s, list, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/station-proposals"))
    );
    assert_eq!(s, 200, "{list}");
    assert_eq!(list["items"].as_array().unwrap().len(), 4);
    assert_eq!(list["items"][0]["members"][0]["stop_id"], "P1a");
    let (_, summary, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/station-proposals/summary"))
    );
    assert_eq!(
        summary,
        json!({"pending": 4, "approved": 0, "rejected": 0, "committed": 0})
    );
    let (_, found, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/station-proposals?q=P3a"))
    );
    assert_eq!(
        found["items"][0]["proposal_id"].as_i64(),
        Some(pc),
        "{found}"
    );
    let (_, found, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/feeds/{FEED}/station-proposals?q=place%20two")
        )
    );
    assert_eq!(
        found["items"][0]["proposal_id"].as_i64(),
        Some(pb),
        "{found}"
    );
    let (_, found, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/feeds/{FEED}/station-proposals?bbox=13.25,80.2,13.35,80.4&limit=1")
        )
    );
    assert_eq!(found["items"].as_array().unwrap().len(), 1, "{found}");
    assert_eq!(found["items"][0]["proposal_id"].as_i64(), Some(pc));
    assert!(found["next_cursor"].is_null());
    let (s, b, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/feeds/{FEED}/station-proposals?status=pending,bogus")
        )
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_status"));
    let (s, detail, _) = call!(
        &app,
        editor_c.req("GET", &format!("/station-proposals/{pa}"))
    );
    assert_eq!(s, 200, "{detail}");
    assert_eq!(detail["problems"], json!([]), "{detail}");
    assert_eq!(detail["members"][0]["current"]["name"], "PLACE ONE");
    assert_eq!(
        detail["members"][0]["current"]["parent_station"],
        Value::Null
    );
    let (s, b, _) = call!(&app, editor_c.req("GET", "/station-proposals/999999999"));
    assert_eq!((s, code_of(&b)), (404, "proposal_not_found"));

    // approve with edits into a draft, then discard the draft: pending again
    let (_, p1, _) = call!(&app, new_set(&editor_c, "stations 1"));
    let p1 = p1["change_set_id"].as_str().unwrap().to_string();
    let approve = |c: &Caller, id: i64, body: Value| {
        c.req("POST", &format!("/station-proposals/{id}/approve"))
            .set_json(body)
    };
    // dropping all but one stop leaves no station: refused, with the reason
    let (s, b, _) = call!(
        &app,
        approve(
            &editor_c,
            pa,
            json!({"change_set_id": p1, "members": [{"stop_id": "P1a"}]})
        )
    );
    assert_eq!((s, code_of(&b)), (400, "proposal_has_problems"), "{b}");
    assert_eq!(
        b["error"]["details"]["problems"][0]["code"], "too_few_members",
        "{b}"
    );
    assert_eq!(
        b["error"]["details"]["problems"][0]["message"],
        "a station groups at least two stops, and stn_ta would have only one"
    );
    let (s, b, _) = call!(
        &app,
        approve(
            &editor_c,
            pa,
            json!({"change_set_id": p1, "name": "Place One (edited)", "lat": 13.1001, "lon": 80.3001,
        "members": [{"stop_id": "P1a", "platform_code": "Towards Edited"}, {"stop_id": "P1b"}]})
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["status"], "approved");
    assert_eq!(b["change_set_id"], p1.as_str());
    assert_eq!(b["change_set_title"], "stations 1");
    assert_eq!(b["reviewed_by_email"], EDITOR);
    let approved_change = b["change_id"].as_i64().unwrap();
    let (_, set_now, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{p1}")));
    let change = &set_now["changes"][0];
    assert_eq!(
        (
            change["entity"].clone(),
            change["op"].clone(),
            change["change_id"].as_i64()
        ),
        (json!("station"), json!("create"), Some(approved_change))
    );
    assert_eq!(change["after"]["proposal_id"].as_i64(), Some(pa));
    assert_eq!(change["after"]["name"], "Place One (edited)");
    // a member sent without platform_code keeps the proposal's label
    assert_eq!(
        change["after"]["members"],
        json!([{"stop_id": "P1a", "platform_code": "Towards Edited"}, {"stop_id": "P1b", "platform_code": "Towards Y"}])
    );
    assert!(errors(&set_now).is_empty(), "{set_now}");
    let (s, b, _) = call!(&app, approve(&editor_c, pa, json!({"change_set_id": p1})));
    assert_eq!((s, code_of(&b)), (409, "proposal_not_pending"), "{b}");
    let (s, b, _) = call!(
        &app,
        approve(
            &editor_c,
            pb,
            json!({"change_set_id": p1, "members": [{"stop_id": "P2a"}, {"stop_id": "S1"}]})
        )
    );
    assert_eq!((s, code_of(&b)), (400, "member_not_in_proposal"), "{b}");
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{p1}/discard"))
    );
    assert_eq!(s, 200, "{b}");
    let (_, b, _) = call!(
        &app,
        editor_c.req("GET", &format!("/station-proposals/{pa}"))
    );
    assert_eq!(
        (
            b["status"].clone(),
            b["change_set_id"].clone(),
            b["change_id"].clone(),
            b["reviewed_by_email"].clone()
        ),
        (json!("pending"), Value::Null, Value::Null, Value::Null),
        "{b}"
    );

    // approve again, relabel one platform, and release it
    let (_, p2, _) = call!(&app, new_set(&editor_c, "stations 2"));
    let p2 = p2["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        approve(
            &editor_c,
            pa,
            json!({"change_set_id": p2,
        "members": [{"stop_id": "P1a"}, {"stop_id": "P1b", "platform_code": "Towards Kerb B"}]})
        )
    );
    assert_eq!(s, 200, "{b}");
    let version_before = feed_version().await;
    for (who, action) in [
        (&editor_c, "submit"),
        (&approver, "approve"),
        (&approver, "commit"),
    ] {
        let (s, b, _) = call!(
            &app,
            who.req("POST", &format!("/change-sets/{p2}/{action}"))
        );
        assert_eq!(s, 200, "{action}: {b}");
    }
    assert_eq!(feed_version().await, version_before + 1);
    let (_, b, _) = call!(
        &app,
        editor_c.req("GET", &format!("/station-proposals/{pa}"))
    );
    assert_eq!(b["status"], "committed", "{b}");
    assert_eq!(b["problems"], json!([]), "{b}");
    assert_eq!(b["members"][0]["current"]["parent_station"], "stn_ta");
    let station = sqlx::query(&format!("SELECT location_type, name, deleted FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'stn_ta'"))
        .fetch_one(&pool).await.unwrap();
    assert_eq!(station.get::<i16, _>("location_type"), 1);
    assert_eq!(station.get::<String, _>("name"), "PLACE ONE");
    let platforms: Vec<(String, Option<String>, Option<String>)> = sqlx::query(&format!(
        "SELECT stop_id, parent_station, platform_code FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id IN ('P1a', 'P1b') ORDER BY stop_id"
    ))
    .fetch_all(&pool).await.unwrap().iter()
    .map(|r| (r.get("stop_id"), r.get("parent_station"), r.get("platform_code")))
    .collect();
    assert_eq!(
        platforms,
        vec![
            (
                "P1a".into(),
                Some("stn_ta".into()),
                Some("Towards X".into())
            ),
            (
                "P1b".into(),
                Some("stn_ta".into()),
                Some("Towards Kerb B".into())
            ),
        ]
    );

    // removing the change from its draft: pending again
    let (_, p3, _) = call!(&app, new_set(&editor_c, "stations 3"));
    let p3 = p3["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(&app, approve(&editor_c, pb, json!({"change_set_id": p3})));
    assert_eq!(s, 200, "{b}");
    let cid = b["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        editor_c.req("DELETE", &format!("/change-sets/{p3}/changes/{cid}"))
    );
    assert_eq!(s, 200, "{b}");
    let (_, b, _) = call!(
        &app,
        editor_c.req("GET", &format!("/station-proposals/{pb}"))
    );
    assert_eq!(
        (b["status"].clone(), b["change_id"].clone()),
        (json!("pending"), Value::Null),
        "{b}"
    );

    // reject needs a note; reopen
    let reject = |c: &Caller, id: i64, body: Value| {
        c.req("POST", &format!("/station-proposals/{id}/reject"))
            .set_json(body)
    };
    let (s, b, _) = call!(&app, reject(&editor_c, pb, json!({"note": "  "})));
    assert_eq!((s, code_of(&b)), (400, "note_required"), "{b}");
    let (s, b, _) = call!(
        &app,
        reject(
            &editor_c,
            pb,
            json!({"note": "the two kerbs are different junctions"})
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (b["status"].clone(), b["review_note"].clone()),
        (
            json!("rejected"),
            json!("the two kerbs are different junctions")
        )
    );
    let (s, b, _) = call!(&app, reject(&editor_c, pb, json!({"note": "again"})));
    assert_eq!((s, code_of(&b)), (409, "proposal_not_pending"), "{b}");
    let (s, b, _) = call!(&app, approve(&editor_c, pb, json!({"change_set_id": p3})));
    assert_eq!((s, code_of(&b)), (409, "proposal_not_pending"), "{b}");
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/station-proposals/{pb}/reopen"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (b["status"].clone(), b["review_note"].clone()),
        (json!("pending"), Value::Null)
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/station-proposals/{pb}/reopen"))
    );
    assert_eq!((s, code_of(&b)), (409, "proposal_not_rejected"), "{b}");

    // bulk approve: problems are skipped, not fatal
    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/station-proposals/approve"))
            .set_json(json!({"change_set_id": p3, "proposal_ids": [pb, pc, pd, pa, 999999999]}))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (b["approved"].clone(), b["skipped"].clone()),
        (json!(2), json!(3)),
        "{b}"
    );
    let results = b["results"].as_array().unwrap();
    assert_eq!(results[0]["ok"], true);
    assert!(results[0]["change_id"].is_i64() && results[1]["change_id"].is_i64());
    assert_eq!(results[2]["ok"], false);
    assert_eq!(
        results[2]["problems"][0]["code"], "member_has_parent",
        "{b}"
    );
    assert_eq!(results[2]["problems"][0]["stop_id"], "P1a", "{b}");
    assert_eq!(
        results[3]["problems"][0]["code"], "proposal_not_pending",
        "{b}"
    );
    assert_eq!(
        results[4]["problems"][0]["code"], "proposal_not_found",
        "{b}"
    );
    let (_, detail_d, _) = call!(
        &app,
        editor_c.req("GET", &format!("/station-proposals/{pd}"))
    );
    assert!(
        has_code(&detail_d["problems"], "member_has_parent"),
        "{detail_d}"
    );
    let (_, summary, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/station-proposals/summary"))
    );
    assert_eq!(
        summary,
        json!({"pending": 1, "approved": 2, "rejected": 0, "committed": 1})
    );
    let (_, set_now, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{p3}")));
    assert_eq!(change_count(&set_now), 2);
    assert!(errors(&set_now).is_empty(), "{set_now}");
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{p3}/discard"))
    );
    assert_eq!(s, 200);
    let (_, summary, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/station-proposals/summary"))
    );
    assert_eq!(
        summary,
        json!({"pending": 3, "approved": 0, "rejected": 0, "committed": 1})
    );

    // every transition is in the audit log
    let actions: Vec<String> = sqlx::query(&format!(
        "SELECT DISTINCT action FROM gtfs_audit_log WHERE gtfs_id = '{FEED}' AND action LIKE 'station_proposal_%'"
    ))
    .fetch_all(&pool).await.unwrap().iter().map(|r| r.get("action")).collect();
    for want in [
        "station_proposal_approved",
        "station_proposal_returned",
        "station_proposal_committed",
        "station_proposal_rejected",
        "station_proposal_reopened",
    ] {
        assert!(
            actions.iter().any(|a| a == want),
            "{want} missing from {actions:?}"
        );
    }
    // a viewer-level read of the audit trail shows them per draft
    let (_, audit, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/audit?change_set={p3}"))
    );
    assert!(
        audit["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["action"] == "station_proposal_returned"
                && a["detail"]["reason"] == "change_set_discarded"),
        "{audit}"
    );

    exec(&pool, &clear_feed(FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}

// ---------------------------------------------------------------- timings

const PERF_FEED: &str = "editor_bulk_perf_feed";
const PERF_ADMIN: &str = "admin@editor-perf-test.invalid";

/// Bulk dry runs at the documented sizes against a copy of chennai_bus (the
/// source feed is only read). Prints the timings.
#[actix_web::test]
async fn bulk_dry_run_timings() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let source = scalar_i64(
        &pool,
        "SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = 'chennai_bus'",
    )
    .await;
    if source < 5000 {
        eprintln!("chennai_bus has {source} route rows; skipping the bulk timings");
        return;
    }
    let mut setup = clear_feed(PERF_FEED);
    setup.extend([
        format!("INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{PERF_FEED}', 'Bulk timing copy of chennai_bus')"),
        format!(
            "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, location_type, platform_code, cluster_id, deleted) \
             SELECT '{PERF_FEED}', stop_id, stop_code, name, lat, lon, location_type, platform_code, cluster_id, deleted \
             FROM gtfs_stop WHERE gtfs_id = 'chennai_bus'"
        ),
        format!(
            "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type, agency_id, deleted) \
             SELECT '{PERF_FEED}', route_id, short_name, long_name, route_type, agency_id, deleted FROM gtfs_route WHERE gtfs_id = 'chennai_bus'"
        ),
        format!(
            "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, marker_id, marker_name, \
                                          marker_lat, marker_lon, stop_name_override, provider_id) \
             SELECT '{PERF_FEED}', route_id, sequence, stop_id, stop_type, stage_no, stage_name, marker_id, marker_name, \
                    marker_lat, marker_lon, stop_name_override, provider_id FROM gtfs_route_stop WHERE gtfs_id = 'chennai_bus'"
        ),
    ]);
    setup.extend(reset_accounts(&[PERF_ADMIN]));
    let copy = Instant::now();
    exec(&pool, &setup).await;
    eprintln!(
        "[timing] copied chennai_bus into {PERF_FEED} in {:?}",
        copy.elapsed()
    );

    let signer = TestSigner::generate("perf-test-key");
    let (st, dir) = state(&pool, &signer, PERF_ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;
    let mut admin = Caller {
        signer: &signer,
        email: PERF_ADMIN.into(),
        session: None,
    };
    let (s, b, _) = call!(&app, admin.req("POST", "/auth/totp/enroll"));
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
    let (s, set, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{PERF_FEED}/change-sets"))
            .set_json(json!({"title": "timings"}))
    );
    assert_eq!(s, 201, "{set}");
    let set_id = set["change_set_id"].as_str().unwrap().to_string();

    // 2,000 new stops, ids minted
    let stops: Vec<Value> = (0..2000)
        .map(|i| json!({"action": "add", "name": format!("PERF STOP {i}"), "lat": format!("{:.6}", 12.9 + (i as f64) * 0.0001), "lon": "80.2", "platform_code": "Towards somewhere"}))
        .collect();
    // 5,000 rows: whole stop lists of real routes, re-uploaded
    let rows = sqlx::query(&format!(
        "SELECT route_id, sequence, stop_id, stop_type, stage_no, stage_name FROM gtfs_route_stop \
         WHERE gtfs_id = '{PERF_FEED}' AND stop_type <> 'ROUTE CORRECTION' ORDER BY route_id, sequence"
    ))
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut route_stops: Vec<Value> = Vec::with_capacity(5000);
    let mut routes = 0;
    let mut i = 0;
    while i < rows.len() {
        let route: String = rows[i].get("route_id");
        let end = (i..rows.len())
            .find(|k| rows[*k].get::<String, _>("route_id") != route)
            .unwrap_or(rows.len());
        if route_stops.len() + (end - i) > 5000 {
            break;
        }
        for r in &rows[i..end] {
            route_stops.push(json!({
                "action": "update", "route_id": route, "sequence": r.get::<i32, _>("sequence"), "stop_id": r.get::<String, _>("stop_id"),
                "stop_type": r.get::<String, _>("stop_type"), "stage_no": r.get::<i32, _>("stage_no"),
                "stage_name": r.get::<String, _>("stage_name"),
            }));
        }
        routes += 1;
        i = end;
    }
    // top up to exactly 5,000 with the next route's first rows
    let mut k = i;
    while route_stops.len() < 5000 && k < rows.len() {
        let r = &rows[k];
        route_stops.push(json!({
            "action": "update", "route_id": r.get::<String, _>("route_id"), "sequence": r.get::<i32, _>("sequence"), "stop_id": r.get::<String, _>("stop_id"),
            "stop_type": r.get::<String, _>("stop_type"), "stage_no": r.get::<i32, _>("stage_no"), "stage_name": r.get::<String, _>("stage_name"),
        }));
        k += 1;
    }
    assert_eq!(route_stops.len(), 5000);

    let bulk = |kind: &str, rows: &Vec<Value>, dry_run: bool| {
        admin
            .req("POST", &format!("/change-sets/{set_id}/bulk"))
            .set_json(json!({"kind": kind, "rows": rows, "dry_run": dry_run}))
    };
    for round in 1..=3 {
        let t = Instant::now();
        let (s, out, _) = call!(&app, bulk("stops", &stops, true));
        let took = t.elapsed();
        assert_eq!(s, 200, "{out}");
        assert_eq!(out["summary"]["ok"], 2000, "{}", out["summary"]);
        eprintln!(
            "[timing] stops dry run, 2000 rows, round {round}: {took:?} ({})",
            out["summary"]
        );
        let t = Instant::now();
        let (s, out, _) = call!(&app, bulk("route_stops", &route_stops, true));
        let took = t.elapsed();
        assert_eq!(s, 200, "{}", out["error"]);
        let mut by_code = std::collections::BTreeMap::<String, usize>::new();
        for m in out["rows"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|r| r["messages"].as_array().unwrap())
        {
            *by_code
                .entry(format!(
                    "{}:{}",
                    m["level"].as_str().unwrap(),
                    m["code"].as_str().unwrap()
                ))
                .or_default() += 1;
        }
        eprintln!(
            "[timing] route_stops dry run, 5000 rows over {} routes, round {round}: {took:?} ({}, messages {by_code:?})",
            out["summary"]["changes"], out["summary"]
        );
        // whole routes, plus the start of one more when they do not fill 5,000
        let partial = if k > i { 1 } else { 0 };
        assert_eq!(
            out["summary"]["changes"].as_i64(),
            Some(routes as i64 + partial)
        );
    }
    let t = Instant::now();
    let (s, out, _) = call!(&app, bulk("stops", &stops, false));
    assert_eq!(s, 200, "{}", out["error"]);
    eprintln!(
        "[timing] stops apply, 2000 rows, including the set detail it returns: {:?}",
        t.elapsed()
    );
    assert_eq!(out["change_set"]["change_count"], 2000);

    exec(&pool, &clear_feed(PERF_FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}
