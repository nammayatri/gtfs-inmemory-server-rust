//! Temporary routes (docs/gtfs-editor.md section 19), end to end against a real
//! Postgres holding the editor schema (db/gtfs_editor/0001..0025): a route is
//! diverted onto a different list of stages through a draft, its
//! `gtfs_route_stop` rows follow the list it is wearing, and one click puts it
//! back on its normal route with the rows it had before.
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

const AUD: &str = "gtfs.editor-variants-test.local";
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
         FROM gtfs_route_stop_effective WHERE gtfs_id = '{FEED}' AND route_id = '{route}' ORDER BY sequence"
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

const FEED: &str = "editor_variants_test_feed";
const ADMIN: &str = "admin@editor-variants-test.invalid";
const EDITOR: &str = "editor@editor-variants-test.invalid";
const APPROVER: &str = "approver@editor-variants-test.invalid";

fn seed() -> Vec<String> {
    let mut s = clear_feed(FEED);
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, use_stages) VALUES ('{FEED}', 'Editor variants test feed', true)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', c, c, 'STOP ' || c, 13.0 + ascii(c) * 0.001, 80.2 \
         FROM unnest(ARRAY['A','B','C','D','E','F']) c"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id) VALUES \
         ('{FEED}', 'R1', '21G', 'A To D', 'AG')"
    ));
    // R1 runs A B | C D, two fare stages. On a feed served from its stages
    // that is what its stages say and nothing else (migration 0027); the rows
    // in gtfs_route_stop are not read as stops, only for the provider id the
    // route's rows must keep.
    s.push(format!(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, direction, name) VALUES \
         ('{FEED}', 'stg_a', '', 'A STAGE'), ('{FEED}', 'stg_c', '', 'C STAGE')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, direction, position, stop_id, stop_type) VALUES \
         ('{FEED}', 'stg_a', '', 1, 'A', 'NEW STOP'), ('{FEED}', 'stg_a', '', 2, 'B', 'INTERMEDIATE STOP'), \
         ('{FEED}', 'stg_c', '', 1, 'C', 'NEW STOP'), ('{FEED}', 'stg_c', '', 2, 'D', 'INTERMEDIATE STOP')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stage (gtfs_id, route_id, position, stage_id, direction, stage_no) VALUES \
         ('{FEED}', 'R1', 1, 'stg_a', '', 1), ('{FEED}', 'R1', 2, 'stg_c', '', 2)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES \
         ('{FEED}', 'R1', 1, 'A', 'NEW STOP', 1, 'A STAGE', '21'), \
         ('{FEED}', 'R1', 2, 'B', 'INTERMEDIATE STOP', 1, 'A STAGE', '21'), \
         ('{FEED}', 'R1', 3, 'C', 'NEW STOP', 2, 'C STAGE', '21'), \
         ('{FEED}', 'R1', 4, 'D', 'INTERMEDIATE STOP', 2, 'C STAGE', '21')"
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
async fn a_route_is_diverted_and_put_back() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("variants-test-key");
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

    // the rows R1 serves today, before anything is diverted
    let rows_of = |route: &str| {
        let route = route.to_string();
        let pool = pool.clone();
        async move {
            sqlx::query(&format!(
                "SELECT sequence, stop_id, stop_type, stage_no, stage_name, provider_id \
                 FROM gtfs_route_stop_effective WHERE gtfs_id = '{FEED}' AND route_id = '{route}' ORDER BY sequence"
            ))
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "{}:{}:{}:{}:{}",
                    r.get::<i32, _>("sequence"),
                    r.get::<Option<String>, _>("stop_id").unwrap_or_default(),
                    r.get::<String, _>("stop_type"),
                    r.get::<i32, _>("stage_no"),
                    r.get::<Option<String>, _>("provider_id").unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>()
        }
    };
    let normal = rows_of("R1").await;
    assert_eq!(
        normal,
        vec![
            "1:A:NEW STOP:1:21",
            "2:B:INTERMEDIATE STOP:1:21",
            "3:C:NEW STOP:2:21",
            "4:D:INTERMEDIATE STOP:2:21"
        ],
        "the fixture is a route of two stages"
    );

    // ======================================================== a diversion
    let d1 = draft!("divert 21G around the bridge");
    // the stage the diversion runs through instead of C and D
    let b = added!(
        d1,
        json!({"entity": "stage", "op": "create", "after": {
            "name": "E STAGE", "rows": stops(&[("E", "NEW STOP"), ("F", "INTERMEDIATE STOP")])}})
    );
    let e_stage = b["changes"].as_array().unwrap().last().unwrap()["entity_key"]
        .as_str()
        .unwrap()
        .to_string();
    // the diversion runs through E alone
    let b = added!(
        d1,
        json!({"entity": "route_variant", "op": "create", "entity_key": "R1", "after": {
            "variant_id": "mandaveli_1", "stages": [{"stage_id": e_stage, "stage_no": 1}]}})
    );
    let findings: Vec<String> = b["validation"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["code"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        !findings.iter().any(|c| c == "route_built_from_stages"),
        "its normal list is already stages; nothing is converted: {b}"
    );
    // creating a diversion does not change what the route serves
    let (s, b) = ship!(d1);
    assert_eq!(s, 200, "commit: {b}");
    assert_eq!(rows_of("R1").await, normal, "the route still runs normally");
    let stage_count: i64 = scalar_i64(
        &pool,
        format!("SELECT count(*) FROM gtfs_route_stage WHERE gtfs_id = '{FEED}' AND route_id = 'R1' AND variant_id IS NULL"),
    )
    .await;
    assert_eq!(stage_count, 2, "its normal list is its two stages still");

    // ======================================================== wearing it
    let hash_of = |variant: Option<&str>| {
        let variant = variant.map(str::to_string);
        let pool = pool.clone();
        async move {
            let mut conn = pool.acquire().await.unwrap();
            gtfs_routes_service::editor::stages::live_links_hash(
                &mut conn,
                FEED,
                "R1",
                variant.as_deref(),
            )
            .await
            .unwrap()
        }
    };
    let d2 = draft!("run the diversion");
    let b = added!(
        d2,
        json!({"entity": "route_variant", "op": "activate", "entity_key": "R1", "after": {
            "variant_id": "mandaveli_1", "base_stages_hash": hash_of(Some("mandaveli_1")).await}})
    );
    let codes: Vec<String> = b["validation"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["code"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(codes.iter().any(|c| c == "route_diverted"), "{b}");
    let (s, b) = ship!(d2);
    assert_eq!(s, 200, "commit: {b}");
    let diverted = rows_of("R1").await;
    assert_eq!(
        diverted,
        vec!["1:E:NEW STOP:1:21", "2:F:INTERMEDIATE STOP:1:21"],
        "it serves the diversion, keeping the route's provider id"
    );
    let worn: Option<String> = sqlx::query(&format!(
        "SELECT active_variant_id FROM gtfs_route WHERE gtfs_id = '{FEED}' AND route_id = 'R1'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap()
    .get("active_variant_id");
    assert_eq!(worn.as_deref(), Some("mandaveli_1"));

    // ======================================================== what the dashboard reads
    let (s, b, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{FEED}/routes/R1/variants"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["diverted"], true, "{b}");
    assert_eq!(b["active_variant_id"], "mandaveli_1", "{b}");
    assert_eq!(
        b["has_normal_list"], true,
        "it has a normal route to go back to: {b}"
    );
    let v = &b["variants"][0];
    assert_eq!(v["variant_id"], "mandaveli_1");
    assert_eq!(v["active"], true);
    assert_eq!(v["stage_count"], 1, "{b}");
    let (s, b, _) = call!(&app, ed.req("GET", &format!("/feeds/{FEED}/diversions")));
    assert_eq!(s, 200, "{b}");
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "one route is diverted: {b}");
    assert_eq!(items[0]["route_id"], "R1");
    assert_eq!(items[0]["short_name"], "21G");
    assert_eq!(items[0]["variant_id"], "mandaveli_1");
    assert!(items[0]["since"].is_string(), "since when: {b}");

    // ======================================================== it cannot be deleted while worn
    let d3 = draft!("try to delete the running diversion");
    let (s, b, _) = call!(
        &app,
        add(
            &d3,
            json!({"entity": "route_variant", "op": "delete", "entity_key": "R1",
                   "after": {"variant_id": "mandaveli_1"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    assert!(
        b["validation"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["code"] == "variant_active"),
        "a diversion the route is running cannot be deleted: {b}"
    );
    // nor can the route's normal list be deleted at all
    let (s, b, _) = call!(
        &app,
        add(
            &d3,
            json!({"entity": "route_variant", "op": "delete", "entity_key": "R1", "after": {}})
        )
    );
    assert_eq!(s, 400, "the normal route is not deletable: {b}");
    assert_eq!(
        b["error"]["details"]["code"], "variant_is_main",
        "and it says so plainly: {b}"
    );
    call!(&app, ed.req("POST", &format!("/change-sets/{d3}/discard")));

    // ======================================================== one click back
    let d4 = draft!("back to the normal route");
    let b = added!(
        d4,
        json!({"entity": "route_variant", "op": "activate", "entity_key": "R1", "after": {
            "variant_id": null, "base_stages_hash": hash_of(None).await}})
    );
    assert!(
        b["validation"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["code"] == "route_back_to_normal"),
        "{b}"
    );
    let (s, b) = ship!(d4);
    assert_eq!(s, 200, "commit: {b}");
    assert_eq!(
        rows_of("R1").await,
        normal,
        "every row is what it was before the diversion, provider id included"
    );

    let (_, b, _) = call!(&app, ed.req("GET", &format!("/feeds/{FEED}/diversions")));
    assert!(
        b["items"].as_array().unwrap().is_empty(),
        "nothing is diverted once it is back to normal: {b}"
    );

    // ======================================================== and now it can be deleted
    let d5 = draft!("the bridge is open again");
    let b = added!(
        d5,
        json!({"entity": "route_variant", "op": "delete", "entity_key": "R1",
               "after": {"variant_id": "mandaveli_1"}})
    );
    assert!(
        !b["validation"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["level"] == "error"),
        "{b}"
    );
    let (s, b) = ship!(d5);
    assert_eq!(s, 200, "commit: {b}");
    let links: i64 = scalar_i64(
        &pool,
        format!("SELECT count(*) FROM gtfs_route_stage WHERE gtfs_id = '{FEED}' AND route_id = 'R1' AND variant_id IS NOT NULL"),
    )
    .await;
    assert_eq!(
        links, 0,
        "the diversion is gone: its links were the whole of it"
    );
    assert_eq!(rows_of("R1").await, normal, "the route is untouched");

    // ======================================================== added and run in one draft
    // What the dashboard's + then "Run this one" does: the draft writes the
    // list and then names it. The hash such a change carries is the draft's own
    // list, which is not in the live rows yet, so it is not a stale read of
    // them: the draft submits and commits like any other.
    let d6 = draft!("divert 21G again, in one go");
    let b = added!(
        d6,
        json!({"entity": "stage", "op": "create", "after": {
            "name": "F STAGE", "rows": stops(&[("E", "NEW STOP"), ("F", "INTERMEDIATE STOP")])}})
    );
    let f_stage = b["changes"].as_array().unwrap().last().unwrap()["entity_key"]
        .as_str()
        .unwrap()
        .to_string();
    added!(
        d6,
        json!({"entity": "route_variant", "op": "create", "entity_key": "R1", "after": {
            "variant_id": "mandaveli_2", "stages": [{"stage_id": f_stage, "stage_no": 1}]}})
    );
    // the hash the dashboard reads from its own draft's preview, not from live
    let (s, prev, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/change-sets/{d6}/preview/routes/R1/variants")
        )
    );
    assert_eq!(s, 200, "{prev}");
    let draft_hash = prev["variants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["variant_id"] == "mandaveli_2")
        .unwrap_or_else(|| panic!("the draft's own diversion is in its preview: {prev}"))
        ["stages_hash"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(
        draft_hash, EMPTY_HASH,
        "the preview's list is the draft's stages: {prev}"
    );
    added!(
        d6,
        json!({"entity": "route_variant", "op": "activate", "entity_key": "R1", "after": {
            "variant_id": "mandaveli_2", "base_stages_hash": draft_hash}})
    );
    let (s, b, _) = call!(&app, ed.req("GET", &format!("/change-sets/{d6}")));
    assert_eq!(s, 200, "{b}");
    assert!(
        b["conflicts"].as_array().map_or(true, |a| a.is_empty()),
        "a draft based on its own list conflicts with nothing: {b}"
    );
    assert_eq!(b["can_submit"], true, "so the draft can be submitted: {b}");
    let (s, b) = ship!(d6);
    assert_eq!(s, 200, "commit: {b}");
    assert_eq!(
        rows_of("R1").await,
        vec!["1:E:NEW STOP:1:21", "2:F:INTERMEDIATE STOP:1:21"],
        "it serves the diversion it added and ran in one draft"
    );

    // and back again, in a draft that changes the normal list first: the same
    // rule covers the list the draft rewrote
    let d7 = draft!("back to normal, tidying the normal list");
    added!(
        d7,
        json!({"entity": "route_variant", "op": "activate", "entity_key": "R1", "after": {
            "variant_id": null, "base_stages_hash": hash_of(None).await}})
    );
    let (s, b) = ship!(d7);
    assert_eq!(s, 200, "commit: {b}");
    assert_eq!(rows_of("R1").await, normal, "back on its normal route");

    exec(&pool, &clear_feed(FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}
