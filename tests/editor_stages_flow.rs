//! Stages (docs/gtfs-editor.md section 18), end to end against a real Postgres
//! holding the editor schema (db/gtfs_editor/0001..0025): stages are created
//! and given to routes in a draft, a route's stop list is rebuilt from them at
//! commit, one stage edit changes every route that uses it, and the rules that
//! keep the flattened `gtfs_route_stop` rows and the stages the same.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. Uses its own feed and accounts and removes its rows afterwards.
//! See scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

const AUD: &str = "gtfs.editor-stages-test.local";
const BASE: &str = "/internal/gtfs-editor";
const EMPTY_HASH: &str = "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945";

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
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route_stage WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_stage_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_stage WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{feed}'"),
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
    let dir = std::env::temp_dir().join(format!("editor-stages-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    use base64::Engine;
    let st = EditorState::build(
        pool.clone(),
        EditorSettings {
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

/// A route's served stop list as `stop_id/stop_type/stage_no/stage_name`.
async fn route_rows(pool: &PgPool, route: &str) -> Vec<String> {
    sqlx::query(&format!(
        "SELECT coalesce(stop_id, marker_id) || '/' || stop_type || '/' || stage_no || '/' || stage_name AS r \
         FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = '{route}' ORDER BY sequence"
    ))
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<String, _>("r"))
    .collect()
}

fn findings(v: &Value, level: &str) -> Vec<Value> {
    v["validation"]
        .as_array()
        .map(|a| a.iter().filter(|x| x["level"] == level).cloned().collect())
        .unwrap_or_default()
}

fn has_code(list: &[Value], code: &str) -> bool {
    list.iter().any(|x| x["code"] == code)
}

// ---------------------------------------------------------------- the flow

const FEED: &str = "editor_stages_test_feed";
const ADMIN: &str = "admin@editor-stages-test.invalid";
const EDITOR: &str = "editor@editor-stages-test.invalid";
const APPROVER: &str = "approver@editor-stages-test.invalid";

fn seed() -> Vec<String> {
    let mut s = clear_feed(FEED);
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{FEED}', 'Editor stages test feed')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', c, c, 'STOP ' || c, 13.0 + ascii(c) * 0.001, 80.2 \
         FROM unnest(ARRAY['A','B','C','D','E','F','G','H','X']) c"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id) VALUES \
         ('{FEED}', 'R1', '1', 'A To E', 'AG'), ('{FEED}', 'R2', '2', 'A To E short', 'AG'), \
         ('{FEED}', 'R3', '3', 'legacy', 'AG')"
    ));
    // R3 is not built from stages: its rows are edited directly, as before
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES \
         ('{FEED}', 'R3', 1, 'G', 'NEW STOP', 1, 'G STAGE', '33'), ('{FEED}', 'R3', 2, 'H', 'NEW STOP', 2, 'H STAGE', '33'), \
         ('{FEED}', 'R1', 1, 'G', 'NEW STOP', 1, 'OLD', '11'), ('{FEED}', 'R1', 2, 'H', 'NEW STOP', 2, 'OLD 2', '11')"
    ));
    s.extend(reset_accounts(&[ADMIN, EDITOR, APPROVER]));
    s
}

fn stops(list: &[(&str, &str)]) -> Value {
    json!(list
        .iter()
        .map(|(id, t)| json!({"stop_id": id, "stop_type": t}))
        .collect::<Vec<_>>())
}

#[actix_web::test]
async fn stages_build_routes_and_one_edit_changes_them_all() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("stages-test-key");
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
    let mut ed = Caller {
        signer: &signer,
        email: EDITOR.into(),
        session: None,
    };
    let mut ap = Caller {
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
    for c in [&mut ed, &mut ap] {
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

    let new_set = |title: &str| {
        ed.req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": title}))
    };
    let add = |set: &str, change: Value| {
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(change)
    };
    let feed_version = || {
        scalar_i64(
            &pool,
            format!("SELECT version FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
        )
    };
    macro_rules! draft {
        ($title:expr) => {{
            let (s, b, _) = call!(&app, new_set($title));
            assert_eq!(s, 201, "{b}");
            b["change_set_id"].as_str().unwrap().to_string()
        }};
    }
    macro_rules! added {
        ($set:expr, $change:expr) => {{
            let (s, b, _) = call!(&app, add(&$set, $change));
            assert_eq!(s, 201, "{b}");
            b
        }};
    }
    // submit (editor), approve and commit (approver): a draft goes live
    macro_rules! ship {
        ($set:expr) => {{
            let (s, b, _) = call!(
                &app,
                ed.req("POST", &format!("/change-sets/{}/submit", $set))
            );
            assert_eq!(s, 200, "submit: {b}");
            let (s, b, _) = call!(
                &app,
                ap.req("POST", &format!("/change-sets/{}/approve", $set))
            );
            assert_eq!(s, 200, "approve: {b}");
            let (s, b, _) = call!(
                &app,
                ap.req("POST", &format!("/change-sets/{}/commit", $set))
            );
            (s, b)
        }};
    }

    // ======================================================== build routes from stages
    let v0 = feed_version().await;
    let d1 = draft!("stages for routes 1 and 2");
    // a stage without an id gets one minted
    let b = added!(
        d1,
        json!({"entity": "stage", "op": "create", "after": {
            "name": "ALPHA", "description": "towards E", "direction": "up",
            "rows": stops(&[("A", "NEW STOP"), ("B", "INTERMEDIATE STOP")])}})
    );
    let alpha = b["changes"].as_array().unwrap().last().unwrap()["entity_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(alpha.starts_with("stg_") && alpha.len() == 14, "{alpha}");
    added!(
        d1,
        json!({"entity": "stage", "op": "create", "entity_key": "stg_beta", "after": {
            "name": "BETA", "rows": stops(&[("C", "NEW STOP"), ("D", "INTERMEDIATE STOP")])}})
    );
    added!(
        d1,
        json!({"entity": "stage", "op": "create", "entity_key": "stg_gamma", "after": {
            "name": "GAMMA", "rows": stops(&[("E", "NEW STOP")])}})
    );
    // the other direction of ALPHA's corridor: one name, the opposite stops
    added!(
        d1,
        json!({"entity": "stage", "op": "create", "entity_key": "stg_alpha_down", "after": {
            "name": "ALPHA", "direction": "down",
            "rows": stops(&[("B", "NEW STOP"), ("A", "INTERMEDIATE STOP")])}})
    );
    let (s, b, _) = call!(
        &app,
        add(
            &d1,
            json!({"entity": "stage", "op": "create", "entity_key": "stg_sideways", "after": {
                "name": "SIDEWAYS", "direction": "left",
                "rows": stops(&[("A", "NEW STOP"), ("B", "INTERMEDIATE STOP")])}})
        )
    );
    assert_eq!(s, 400, "a direction is up or down: {b}");
    assert_eq!(b["error"]["details"]["code"], "invalid_payload", "{b}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("direction"),
        "{b}"
    );
    added!(
        d1,
        json!({"entity": "stage", "op": "create", "entity_key": "stg_unused", "after": {
            "name": "UNUSED", "rows": stops(&[("H", "NEW STOP")])}})
    );
    // R1 had rows written directly; giving it stages replaces them
    added!(
        d1,
        json!({"entity": "route_stages", "op": "replace", "entity_key": "R1", "after": {
            "stages": [{"stage_id": alpha}, {"stage_id": "stg_beta"}, {"stage_id": "stg_gamma"}],
            "base_stages_hash": EMPTY_HASH}})
    );
    let b = added!(
        d1,
        json!({"entity": "route_stages", "op": "replace", "entity_key": "R2", "after": {
            "stages": [{"stage_id": alpha}, {"stage_id": "stg_gamma", "stage_no": 5}],
            "base_stages_hash": EMPTY_HASH}})
    );
    assert!(findings(&b, "error").is_empty(), "{b}");

    // a route uses a stage once
    let (s, b, _) = call!(
        &app,
        add(
            &d1,
            json!({"entity": "route_stages", "op": "replace", "entity_key": "R2", "after": {
                "stages": [{"stage_id": alpha}, {"stage_id": "stg_gamma"}, {"stage_id": alpha}],
                "base_stages_hash": EMPTY_HASH}})
        )
    );
    assert_eq!(
        (s, b["error"]["details"]["code"].as_str()),
        (400, Some("stage_repeated")),
        "{b}"
    );

    // the draft's preview shows the route built from its stages
    let (s, p, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/change-sets/{d1}/preview/routes/R1/stages")
        )
    );
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["has_stages"], true);
    assert_eq!(p["in_sync"], true);
    assert_eq!(p["stages"].as_array().unwrap().len(), 3);
    assert_eq!(p["stages"][0]["route_count"], 2);
    let (s, p, _) = call!(
        &app,
        ed.req("GET", &format!("/change-sets/{d1}/preview/routes/R1"))
    );
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["rows"].as_array().unwrap().len(), 5);
    // nothing is live before commit
    assert!(
        call!(
            &app,
            ed.req("GET", &format!("/feeds/{FEED}/stages/{alpha}"))
        )
        .0 == 404
    );

    let (s, b) = ship!(d1);
    assert_eq!(s, 200, "commit: {b}");
    assert_eq!(feed_version().await, v0 + 1);
    assert_eq!(
        route_rows(&pool, "R1").await,
        [
            "A/NEW STOP/1/ALPHA",
            "B/INTERMEDIATE STOP/1/ALPHA",
            "C/NEW STOP/2/BETA",
            "D/INTERMEDIATE STOP/2/BETA",
            "E/NEW STOP/3/GAMMA"
        ]
    );
    // a skipped fare stage number is the route's to keep
    assert_eq!(
        route_rows(&pool, "R2").await,
        [
            "A/NEW STOP/1/ALPHA",
            "B/INTERMEDIATE STOP/1/ALPHA",
            "E/NEW STOP/5/GAMMA"
        ]
    );
    // the route keeps its provider id
    assert_eq!(
        scalar_i64(
            &pool,
            format!("SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'R1' AND provider_id = '11'")
        )
        .await,
        5
    );

    // ---- reads
    let (s, list, _) = call!(&app, ed.req("GET", &format!("/feeds/{FEED}/stages?q=alp")));
    assert_eq!(s, 200, "{list}");
    // both directions of ALPHA match the name, so the one this route uses is
    // found by its id, not by being first
    let up = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["stage_id"] == alpha.as_str())
        .unwrap_or_else(|| panic!("{list}"));
    assert_eq!(up["route_count"], 2);
    assert_eq!(up["stop_count"], 2);
    assert_eq!(up["direction"], "up");
    assert_eq!(up["first_stop"]["stop_id"], "A");
    assert_eq!(up["last_stop"]["stop_id"], "B");
    // two stages named ALPHA, one each way: the search tells them apart by
    // direction, and each says where it runs from and to
    let (_, both, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{FEED}/stages?q=ALPHA"))
    );
    let names: Vec<(&str, &str, &str, &str)> = both["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["stage_id"].as_str().unwrap_or(""),
                i["direction"].as_str().unwrap_or("either"),
                i["first_stop"]["stop_id"].as_str().unwrap_or(""),
                i["last_stop"]["stop_id"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(names.len(), 2, "one name, two directions: {both}");
    assert!(
        names.contains(&(alpha.as_str(), "up", "A", "B"))
            && names.contains(&("stg_alpha_down", "down", "B", "A")),
        "each says which way it runs, and from which stop to which: {names:?}"
    );
    let (_, down, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/feeds/{FEED}/stages?q=ALPHA&direction=down")
        )
    );
    assert_eq!(down["items"].as_array().unwrap().len(), 1, "{down}");
    assert_eq!(down["items"][0]["stage_id"], "stg_alpha_down", "{down}");
    let (_, list, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{FEED}/stages?stop_id=E"))
    );
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    let (_, list, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{FEED}/stages?unused=true"))
    );
    // no route runs the down direction of ALPHA yet, so it is unused too
    let unused: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["stage_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(unused, vec!["stg_alpha_down", "stg_unused"], "{list}");
    let (s, stage, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{FEED}/stages/{alpha}"))
    );
    assert_eq!(s, 200, "{stage}");
    assert_eq!(stage["description"], "towards E");
    assert_eq!(stage["rows"][1]["stop_name"], "STOP B");
    let routes: Vec<&str> = stage["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["route_id"].as_str().unwrap())
        .collect();
    assert_eq!(routes, ["R1", "R2"]);
    // each says which of the route's lists it is on: none of these is a
    // temporary route, and every one of them is what its route runs
    assert!(
        stage["routes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["variant_id"].is_null() && r["running"] == true),
        "the normal list, running: {stage}"
    );
    assert_eq!(stage["direction"], "up", "{stage}");
    let (s, r1, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{FEED}/routes/R1/stages"))
    );
    assert_eq!(s, 200, "{r1}");
    assert_eq!(r1["in_sync"], true);
    let r1_hash = r1["stages_hash"].as_str().unwrap().to_string();
    let (_, r3, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{FEED}/routes/R3/stages"))
    );
    assert_eq!(r3["has_stages"], false);
    assert_eq!(r3["stages_hash"], EMPTY_HASH);

    // ======================================================== a route's stops change only through its stages
    let d2 = draft!("try to edit R1's stops directly");
    let (_, detail, _) = call!(&app, ed.req("GET", &format!("/feeds/{FEED}/routes/R1")));
    let b = added!(
        d2,
        json!({"entity": "route_stops", "op": "replace", "entity_key": "R1", "after": {
            "rows": [{"stop_id": "A", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "ALPHA"},
                     {"stop_id": "E", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "GAMMA"}],
            "base_rows_hash": detail["rows_hash"]}})
    );
    assert!(has_code(&findings(&b, "error"), "route_has_stages"), "{b}");
    // a route without stages is still edited as before
    let (_, detail, _) = call!(&app, ed.req("GET", &format!("/feeds/{FEED}/routes/R3")));
    let b = added!(
        d2,
        json!({"entity": "route_stops", "op": "replace", "entity_key": "R3", "after": {
            "rows": [{"stop_id": "G", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "G STAGE"},
                     {"stop_id": "F", "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "G STAGE"},
                     {"stop_id": "H", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "H STAGE"}],
            "base_rows_hash": detail["rows_hash"]}})
    );
    let errs = findings(&b, "error");
    assert_eq!(errs.len(), 1, "{b}");
    let (s, _, _) = call!(&app, ed.req("POST", &format!("/change-sets/{d2}/discard")));
    assert_eq!(s, 200);

    // ======================================================== one stage edit changes every route using it
    let d3 = draft!("add F to ALPHA");
    let b = added!(
        d3,
        json!({"entity": "stage", "op": "update", "entity_key": alpha, "after": {
            "rows": stops(&[("A", "NEW STOP"), ("B", "INTERMEDIATE STOP"), ("F", "INTERMEDIATE STOP")])}})
    );
    assert!(findings(&b, "error").is_empty(), "{b}");
    let told = findings(&b, "warning");
    let changes_routes = told
        .iter()
        .find(|w| w["code"] == "stage_changes_routes")
        .unwrap_or_else(|| panic!("{b}"));
    let msg = changes_routes["message"].as_str().unwrap();
    assert!(
        msg.contains("2 routes") && msg.contains("1 (R1)") && msg.contains("2 (R2)"),
        "{msg}"
    );
    // the change's before is the stage as it was, with the routes it reaches
    let change = b["changes"].as_array().unwrap().last().unwrap();
    assert_eq!(change["before"]["rows"].as_array().unwrap().len(), 2);
    assert_eq!(change["before"]["routes"].as_array().unwrap().len(), 2);
    let (s, p, _) = call!(
        &app,
        ed.req("GET", &format!("/change-sets/{d3}/preview/stages/{alpha}"))
    );
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["rows"].as_array().unwrap().len(), 3);
    let (s, b) = ship!(d3);
    assert_eq!(s, 200, "commit: {b}");
    assert_eq!(route_rows(&pool, "R1").await.len(), 6);
    assert_eq!(
        route_rows(&pool, "R2").await,
        [
            "A/NEW STOP/1/ALPHA",
            "B/INTERMEDIATE STOP/1/ALPHA",
            "F/INTERMEDIATE STOP/1/ALPHA",
            "E/NEW STOP/5/GAMMA"
        ]
    );
    // the route's stage list did not move, so a draft based on it still applies
    let (_, r1, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{FEED}/routes/R1/stages"))
    );
    assert_eq!(r1["stages_hash"], r1_hash.as_str());
    assert_eq!(r1["in_sync"], true);

    // ======================================================== a stage edit that breaks a route's fares is blocked
    let d4 = draft!("GAMMA starts with an intermediate stop");
    let b = added!(
        d4,
        json!({"entity": "stage", "op": "update", "entity_key": "stg_gamma", "after": {
            "rows": stops(&[("E", "INTERMEDIATE STOP")])}})
    );
    let errs = findings(&b, "error");
    assert!(
        errs.iter().any(|e| e["code"] == "fare_stage_mismatch"
            && e["message"].as_str().unwrap().starts_with("route 1 (R1): ")),
        "{b}"
    );
    let (s, b, _) = call!(&app, ed.req("POST", &format!("/change-sets/{d4}/submit")));
    assert_eq!((s, code_of(&b)), (400, "validation_failed"), "{b}");
    call!(&app, ed.req("POST", &format!("/change-sets/{d4}/discard")));

    // ======================================================== two drafts changing one stage: the second conflicts
    let d5 = draft!("rename BETA");
    let d6 = draft!("rename BETA again");
    for (d, name) in [(&d5, "BETA ONE"), (&d6, "BETA TWO")] {
        added!(
            d,
            json!({"entity": "stage", "op": "update", "entity_key": "stg_beta", "after": {"name": name}})
        );
    }
    let (s, b) = ship!(d5);
    assert_eq!(s, 200, "{b}");
    assert!(route_rows(&pool, "R1")
        .await
        .contains(&"C/NEW STOP/2/BETA ONE".to_string()));
    let (s, b, _) = call!(&app, ed.req("POST", &format!("/change-sets/{d6}/submit")));
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    call!(&app, ed.req("POST", &format!("/change-sets/{d6}/discard")));

    // ======================================================== deleting stages and stops they hold
    let d7 = draft!("delete stages");
    let b = added!(
        d7,
        json!({"entity": "stage", "op": "delete", "entity_key": alpha, "after": null})
    );
    assert!(has_code(&findings(&b, "error"), "stage_in_use"), "{b}");
    call!(&app, ed.req("POST", &format!("/change-sets/{d7}/discard")));
    // a stop only an unused stage holds cannot be deleted either
    let d8 = draft!("delete H");
    let b = added!(
        d8,
        json!({"entity": "stop", "op": "delete", "entity_key": "H", "after": null})
    );
    // H is still on the legacy route R3 as well
    assert!(has_code(&findings(&b, "error"), "stop_in_use"), "{b}");
    call!(&app, ed.req("POST", &format!("/change-sets/{d8}/discard")));
    exec(
        &pool,
        &[format!(
            "DELETE FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'R3' AND stop_id = 'H'"
        )],
    )
    .await;
    let d9 = draft!("delete H again");
    let b = added!(
        d9,
        json!({"entity": "stop", "op": "delete", "entity_key": "H", "after": null})
    );
    assert!(has_code(&findings(&b, "error"), "stop_in_stage"), "{b}");
    call!(
        &app,
        ed.req(
            "DELETE",
            &format!("/change-sets/{d9}/changes/{}", b["change_id"])
        )
    );
    added!(
        d9,
        json!({"entity": "stage", "op": "delete", "entity_key": "stg_unused", "after": null})
    );
    let b = added!(
        d9,
        json!({"entity": "stop", "op": "delete", "entity_key": "H", "after": null})
    );
    assert!(findings(&b, "error").is_empty(), "{b}");
    let (s, b) = ship!(d9);
    assert_eq!(s, 200, "{b}");
    let (_, list, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{FEED}/stages?unused=true"))
    );
    // only the down direction of ALPHA, which no route runs in this test
    let left: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["stage_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(left, vec!["stg_alpha_down"], "{list}");

    // ======================================================== a stop merge moves the stages too
    let d10 = draft!("merge B into X");
    let b = added!(
        d10,
        json!({"entity": "stop", "op": "merge", "entity_key": "B", "after": {"into_stop_id": "X"}})
    );
    assert!(findings(&b, "error").is_empty(), "{b}");
    let alpha_version = scalar_i64(
        &pool,
        format!("SELECT row_version::bigint FROM gtfs_stage WHERE gtfs_id = '{FEED}' AND stage_id = '{alpha}'"),
    )
    .await;
    let (s, b) = ship!(d10);
    assert_eq!(s, 200, "{b}");
    // both directions of ALPHA called at B, so both now call at X
    assert_eq!(
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_stage_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'X'"
            )
        )
        .await,
        2
    );
    assert!(
        scalar_i64(
            &pool,
            format!("SELECT row_version::bigint FROM gtfs_stage WHERE gtfs_id = '{FEED}' AND stage_id = '{alpha}'"),
        )
        .await
            > alpha_version
    );
    for r in ["R1", "R2"] {
        let (_, rs, _) = call!(
            &app,
            ed.req("GET", &format!("/feeds/{FEED}/routes/{r}/stages"))
        );
        assert_eq!(rs["in_sync"], true, "{r}: {rs}");
    }

    // ======================================================== a route changed outside its stages is not overwritten
    exec(
        &pool,
        &[format!(
            "UPDATE gtfs_route_stop SET stage_name = 'HAND EDIT' WHERE gtfs_id = '{FEED}' AND route_id = 'R2' AND stop_id = 'E'"
        )],
    )
    .await;
    let (_, rs, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{FEED}/routes/R2/stages"))
    );
    assert_eq!(rs["in_sync"], false);
    let d11 = draft!("rename GAMMA");
    let b = added!(
        d11,
        json!({"entity": "stage", "op": "update", "entity_key": "stg_gamma", "after": {"name": "GAMMA 2"}})
    );
    let errs = findings(&b, "error");
    assert!(
        has_code(&errs, "route_out_of_sync")
            && errs[0]["message"].as_str().unwrap().contains("2 (R2)"),
        "{b}"
    );
    // giving the route its stages again puts it back, with a warning that it replaces the hand edit
    let b = added!(
        d11,
        json!({"entity": "route_stages", "op": "replace", "entity_key": "R2", "after": {
            "stages": [{"stage_id": alpha}, {"stage_id": "stg_gamma", "stage_no": 5}],
            "base_stages_hash": rs["stages_hash"]}})
    );
    assert!(
        has_code(&findings(&b, "warning"), "route_out_of_sync"),
        "{b}"
    );
    call!(&app, ed.req("POST", &format!("/change-sets/{d11}/discard")));

    exec(&pool, &clear_feed(FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}
