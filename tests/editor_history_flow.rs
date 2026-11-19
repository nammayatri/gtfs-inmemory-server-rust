//! A node's versions and rolling it back (docs/gtfs-editor.md section 26), end
//! to end against a real Postgres holding the editor schema.
//!
//! A stop is renamed and moved in one committed draft, renamed again in a
//! second; its history lists both, newest first, with the stop each left and
//! the stop before its first edit. A viewer may read it but not roll back. An
//! editor rolls it back to the first version: a new draft holding one
//! `stop/update` with only the name (the position did not change since), which
//! goes live only through approval by someone else and commit - and is then the
//! newest version. A second rollback of the same node into that draft is
//! refused, as is one to what the stop already has (leaving no empty draft
//! behind), and one to before its first edit restores name and position
//! together. A route's stop list is cut short and put back from its history,
//! with the base it replaces. A rollback overtaken by another commit of the
//! node conflicts at commit like any other change. A write that went round the
//! drafts shows as a change the history does not see. Every node kind's read
//! runs, stage and stage list and record included.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local (see scripts/editor_flow_test.sh). Uses its own feed and
//! accounts and removes them afterwards; never touches chennai_bus.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

const AUD: &str = "gtfs.editor-history-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_history_test_feed";
const ADMIN: &str = "admin@editor-history-test.invalid";
const VIEWER: &str = "viewer@editor-history-test.invalid";
const EDITOR: &str = "editor@editor-history-test.invalid";
const APPROVER: &str = "approver@editor-history-test.invalid";

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
        "this test only runs against a local database"
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

fn clear() -> Vec<String> {
    let accounts = format!("'{ADMIN}', '{VIEWER}', '{EDITOR}', '{APPROVER}'");
    vec![
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
        format!(
            "DELETE FROM gtfs_editor_session WHERE user_id IN \
             (SELECT user_id FROM gtfs_editor_user WHERE email IN ({accounts}))"
        ),
        format!(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, \
             totp_last_step = NULL, status = 'active' WHERE email IN ({accounts})"
        ),
    ]
}

/// Three stops and one route through them, two fare stages.
fn seed() -> Vec<String> {
    let mut s = clear();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, data_source) \
         VALUES ('{FEED}', 'History test feed', 'db')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, name, lat, lon) VALUES \
         ('{FEED}', 'S1', 'ALPHA', 13.0000, 80.2000), \
         ('{FEED}', 'S2', 'BETA', 13.0050, 80.2000), \
         ('{FEED}', 'S3', 'GAMMA', 13.0100, 80.2000)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type) VALUES \
         ('{FEED}', 'R1', '1A', 'ALPHA To GAMMA', 3)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, pattern_key, sequence, stop_id, stop_type, \
                                      stage_no, stage_name) VALUES \
         ('{FEED}', 'R1', 1, 1, 'S1', 'NEW STOP', 1, 'ALPHA'), \
         ('{FEED}', 'R1', 1, 2, 'S2', 'INTERMEDIATE STOP', 1, 'ALPHA'), \
         ('{FEED}', 'R1', 1, 3, 'S3', 'NEW STOP', 2, 'GAMMA')"
    ));
    s
}

fn state(pool: &PgPool, signer: &TestSigner) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-history-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    use base64::Engine;
    let st = EditorState::build(
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
    (st, dir)
}

async fn sign_in(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    c: &mut Caller<'_>,
) {
    let (s, b, _) = call!(app, c.req("POST", "/auth/totp/enroll"));
    assert_eq!(s, 200, "{b}");
    let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let (s, b, cookie) = call!(
        app,
        c.req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now())}))
    );
    assert_eq!(s, 200, "{b}");
    c.session = cookie;
}

async fn change_set_count(pool: &PgPool) -> i64 {
    sqlx::query("SELECT count(*) FROM gtfs_change_set WHERE gtfs_id = $1")
        .bind(FEED)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<i64, _>(0)
}

#[actix_web::test]
async fn a_node_goes_back_to_a_version_through_a_draft() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;

    let signer = TestSigner::generate("editor-history-test-key");
    let (st, dir) = state(&pool, &signer);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    sign_in(&app, &mut admin).await;
    let others = [
        (VIEWER, "viewer"),
        (EDITOR, "editor"),
        (APPROVER, "approver"),
    ];
    for (email, role) in others {
        let (s, b, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    }
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    for (email, role) in others {
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
    let mut viewer = Caller {
        signer: &signer,
        email: VIEWER.into(),
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
    for c in [&mut viewer, &mut editor_c, &mut approver] {
        sign_in(&app, c).await;
    }

    // a draft of the editor's with `changes`, submitted, approved and committed
    // by the approver: its feed version
    macro_rules! commit_set {
        ($id:expr) => {{
            let id: String = $id;
            let (s, b, _) = call!(
                &app,
                editor_c.req("POST", &format!("/change-sets/{id}/submit"))
            );
            assert_eq!(s, 200, "submit {b}");
            let (s, b, _) = call!(
                &app,
                approver
                    .req("POST", &format!("/change-sets/{id}/approve"))
                    .set_json(json!({}))
            );
            assert_eq!(s, 200, "approve {b}");
            let (s, b, _) = call!(
                &app,
                approver.req("POST", &format!("/change-sets/{id}/commit"))
            );
            (s, b)
        }};
    }
    macro_rules! commit {
        ($title:expr, $changes:expr) => {{
            let (s, b, _) = call!(
                &app,
                editor_c
                    .req("POST", &format!("/feeds/{FEED}/change-sets"))
                    .set_json(json!({"title": $title}))
            );
            assert_eq!(s, 201, "{b}");
            let id = b["change_set_id"].as_str().unwrap().to_string();
            for c in $changes {
                let (s, b, _) = call!(
                    &app,
                    editor_c
                        .req("POST", &format!("/change-sets/{id}/changes"))
                        .set_json(c)
                );
                assert_eq!(s, 201, "{b}");
            }
            let (s, b) = commit_set!(id.clone());
            assert_eq!(s, 200, "commit {b}");
            id
        }};
    }
    macro_rules! history {
        ($who:expr, $entity:expr, $key:expr) => {{
            let (s, b, _) = call!(
                &app,
                $who.req(
                    "GET",
                    &format!("/feeds/{FEED}/history/{}/{}", $entity, $key)
                )
            );
            assert_eq!(s, 200, "{b}");
            b
        }};
    }
    let stop = |c: &Caller, id: &str| c.req("GET", &format!("/feeds/{FEED}/stops/{id}"));

    // ---- a stop's versions
    commit!(
        "rename and move S1",
        [json!({"entity": "stop", "op": "update", "entity_key": "S1",
                "after": {"name": "ALPHA ONE", "lat": 13.0004, "lon": 80.2001}})]
    );
    commit!(
        "rename S1 again",
        [json!({"entity": "stop", "op": "update", "entity_key": "S1",
                "after": {"name": "ALPHA TWO"}})]
    );
    let h = history!(viewer, "stop", "S1");
    assert_eq!(
        h["node"],
        json!({"entity": "stop", "entity_key": "S1"}),
        "{h}"
    );
    assert_eq!(h["exists"], true);
    let v = h["versions"].as_array().unwrap();
    assert_eq!(v.len(), 2, "{h}");
    assert!(
        v[0]["feed_version"].as_i64() > v[1]["feed_version"].as_i64(),
        "newest first: {h}"
    );
    assert_eq!(v[0]["state"]["name"], "ALPHA TWO");
    assert_eq!(v[0]["is_live"], true);
    assert_eq!(v[1]["state"]["name"], "ALPHA ONE");
    assert_eq!(v[1]["state"]["lat"], json!(13.0004));
    assert_eq!(v[1]["is_live"], false);
    for x in v {
        assert_eq!(x["restorable"], true, "{x}");
        assert_eq!(x["direct"], true, "{x}");
        assert_eq!(x["unrecorded_change_after"], false, "{x}");
    }
    assert_eq!(h["original"]["state"]["name"], "ALPHA");
    assert_eq!(h["original"]["is_live"], false);
    let (first, first_version) = (
        v[1]["change_id"].as_i64().unwrap(),
        v[1]["feed_version"].as_i64().unwrap(),
    );
    let latest = v[0]["change_id"].as_i64().unwrap();

    // a viewer reads it and cannot roll it back
    let rollback = |c: &Caller, entity: &str, key: &str, body: Value| {
        c.req(
            "POST",
            &format!("/feeds/{FEED}/history/{entity}/{key}/rollback"),
        )
        .set_json(body)
    };
    let (s, b, _) = call!(
        &app,
        rollback(&viewer, "stop", "S1", json!({"change_id": first}))
    );
    assert_eq!((s, code_of(&b)), (403, "role_required"), "{b}");

    // the editor rolls it back to its first version: a new draft, one change,
    // the name only - the position is what that version left
    let (s, b, _) = call!(
        &app,
        rollback(&editor_c, "stop", "S1", json!({"change_id": first}))
    );
    assert_eq!(s, 201, "{b}");
    assert_eq!(b["created_set"], true);
    assert_eq!(b["fields"], json!(["name"]), "{b}");
    assert_eq!(b["to_feed_version"], json!(first_version));
    let draft = b["change_set_id"].as_str().unwrap().to_string();
    let (s, set, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{draft}")));
    assert_eq!(s, 200, "{set}");
    assert_eq!(
        set["title"],
        format!("Roll back stop S1 to v{first_version}")
    );
    assert_eq!(set["status"], "draft");
    let changes = set["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "{set}");
    assert_eq!(
        (
            &changes[0]["entity"],
            &changes[0]["op"],
            &changes[0]["after"]
        ),
        (
            &json!("stop"),
            &json!("update"),
            &json!({"name": "ALPHA ONE"})
        ),
        "{set}"
    );
    assert_eq!(
        changes[0]["before"]["name"], "ALPHA TWO",
        "based on the live stop"
    );
    // nothing is live yet
    let (_, s1, _) = call!(&app, stop(&viewer, "S1"));
    assert_eq!(s1["name"], "ALPHA TWO");
    // the node once per draft
    let (s, b, _) = call!(
        &app,
        rollback(
            &editor_c,
            "stop",
            "S1",
            json!({"change_id": first, "change_set_id": draft})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "node_in_draft"), "{b}");
    // it is audited as a rollback
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gtfs_audit_log WHERE gtfs_id = $1 AND action = 'node_rolled_back' \
         AND change_set_id = $2::uuid AND detail -> 'node' ->> 'entity_key' = 'S1' \
         AND (detail ->> 'to_change_id')::bigint = $3",
    )
    .bind(FEED)
    .bind(&draft)
    .bind(first)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // it goes live through review, and is then the newest version
    let (s, b) = commit_set!(draft.clone());
    assert_eq!(s, 200, "{b}");
    let (_, s1, _) = call!(&app, stop(&viewer, "S1"));
    assert_eq!(
        (&s1["name"], &s1["lat"]),
        (&json!("ALPHA ONE"), &json!(13.0004))
    );
    let h = history!(viewer, "stop", "S1");
    let v = h["versions"].as_array().unwrap();
    assert_eq!(v.len(), 3, "{h}");
    assert_eq!(v[0]["change_set_id"], json!(draft));
    assert_eq!(v[0]["is_live"], true);
    assert_eq!(
        v[2]["is_live"], true,
        "the first version's stop is the live one again"
    );

    // to what the stop already has: refused, and no empty draft is left
    let sets = change_set_count(&pool).await;
    let newest = v[0]["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        rollback(&editor_c, "stop", "S1", json!({"change_id": newest}))
    );
    assert_eq!((s, code_of(&b)), (409, "nothing_to_roll_back"), "{b}");
    assert_eq!(change_set_count(&pool).await, sets);

    // before its first edit: name and position, the position whole
    let (s, b, _) = call!(
        &app,
        rollback(
            &editor_c,
            "stop",
            "S1",
            json!({"change_id": first, "side": "before"})
        )
    );
    assert_eq!(s, 201, "{b}");
    let mut fields: Vec<String> = serde_json::from_value(b["fields"].clone()).unwrap();
    fields.sort();
    assert_eq!(fields, ["lat", "lon", "name"], "{b}");
    let before_draft = b["change_set_id"].as_str().unwrap().to_string();
    let (_, set, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{before_draft}"))
    );
    assert_eq!(
        set["changes"][0]["after"],
        json!({"name": "ALPHA", "lat": 13.0, "lon": 80.2}),
        "{set}"
    );
    assert!(
        set["title"].as_str().unwrap().contains("to before v"),
        "{set}"
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{before_draft}/discard"))
    );
    assert_eq!(s, 200, "{b}");

    // a version that is not this node's, an unknown node kind, a bad side
    let (s, b, _) = call!(
        &app,
        rollback(&editor_c, "stop", "S2", json!({"change_id": latest}))
    );
    assert_eq!((s, code_of(&b)), (404, "version_not_found"), "{b}");
    let (s, b, _) = call!(
        &app,
        viewer.req("GET", &format!("/feeds/{FEED}/history/trip/T1"))
    );
    assert_eq!((s, code_of(&b)), (400, "not_a_node"), "{b}");
    let (s, b, _) = call!(
        &app,
        rollback(
            &editor_c,
            "stop",
            "S1",
            json!({"change_id": first, "side": "sideways"})
        )
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_side"), "{b}");

    // ---- a route's stop list: cut short, then put back from its history
    let (_, r1, _) = call!(&app, viewer.req("GET", &format!("/feeds/{FEED}/routes/R1")));
    let hash = r1["rows_hash"].as_str().unwrap().to_string();
    commit!(
        "drop BETA from 1A",
        [
            json!({"entity": "route_stops", "op": "replace", "entity_key": "R1",
                "after": {"base_rows_hash": hash, "rows": [
                    {"stop_id": "S1", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "ALPHA"},
                    {"stop_id": "S3", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "GAMMA"}]}})
        ]
    );
    let h = history!(viewer, "route_stops", "R1");
    assert_eq!(h["node"]["pattern_key"], 1, "{h}");
    let v = h["versions"].as_array().unwrap();
    assert_eq!(v.len(), 1, "{h}");
    assert_eq!(v[0]["state"]["rows"].as_array().unwrap().len(), 2);
    assert_eq!(h["original"]["state"]["rows"].as_array().unwrap().len(), 3);
    let cut = v[0]["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        rollback(
            &editor_c,
            "route_stops",
            "R1",
            json!({"change_id": cut, "side": "before"})
        )
    );
    assert_eq!(s, 201, "{b}");
    assert_eq!(b["fields"], json!(["rows"]), "{b}");
    let list_draft = b["change_set_id"].as_str().unwrap().to_string();
    let (_, set, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{list_draft}"))
    );
    let after = &set["changes"][0]["after"];
    assert_eq!(after["rows"].as_array().unwrap().len(), 3, "{set}");
    assert_eq!(after["rows"][1]["stop_id"], "S2");
    assert_eq!(after["rows"][1]["stop_type"], "INTERMEDIATE STOP");
    let (_, r1, _) = call!(&app, viewer.req("GET", &format!("/feeds/{FEED}/routes/R1")));
    assert_eq!(
        after["base_rows_hash"], r1["rows_hash"],
        "based on the live list"
    );
    assert_eq!(set["can_submit"], true, "{set}");
    let (s, b) = commit_set!(list_draft.clone());
    assert_eq!(s, 200, "{b}");
    let (_, r1, _) = call!(&app, viewer.req("GET", &format!("/feeds/{FEED}/routes/R1")));
    let served: Vec<&str> = r1["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["stop_id"].as_str().unwrap())
        .collect();
    assert_eq!(served, ["S1", "S2", "S3"]);

    // ---- a rollback overtaken by another commit of the node conflicts
    commit!(
        "rename S3",
        [json!({"entity": "stop", "op": "update", "entity_key": "S3",
                "after": {"name": "GAMMA X"}})]
    );
    let h = history!(viewer, "stop", "S3");
    let renamed = h["versions"][0]["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        rollback(
            &editor_c,
            "stop",
            "S3",
            json!({"change_id": renamed, "side": "before",
                                                  "title": "GAMMA back"})
        )
    );
    assert_eq!(s, 201, "{b}");
    let late = b["change_set_id"].as_str().unwrap().to_string();
    let (_, set, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{late}")));
    assert_eq!(set["title"], "GAMMA back");
    commit!(
        "rename S3 meanwhile",
        [json!({"entity": "stop", "op": "update", "entity_key": "S3",
                "after": {"name": "GAMMA Y"}})]
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{late}/submit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    let (_, s3, _) = call!(&app, stop(&viewer, "S3"));
    assert_eq!(s3["name"], "GAMMA Y");

    // ---- a write round the drafts is a change the history does not see
    commit!(
        "rename S2",
        [json!({"entity": "stop", "op": "update", "entity_key": "S2",
                "after": {"name": "BETA ONE"}})]
    );
    exec(
        &pool,
        &[format!(
            "UPDATE gtfs_stop SET name = 'BETA BY SCRIPT' WHERE gtfs_id = '{FEED}' AND stop_id = 'S2'"
        )],
    )
    .await;
    let h = history!(viewer, "stop", "S2");
    let v = &h["versions"][0];
    assert_eq!(v["unrecorded_change_after"], true, "{h}");
    assert_eq!(v["is_live"], false);
    assert_eq!(h["live"]["name"], "BETA BY SCRIPT");

    // ---- every kind of node reads, with or without versions
    let h = history!(viewer, "route", "R1");
    assert_eq!(h["versions"], json!([]));
    assert_eq!(h["exists"], true);
    let h = history!(viewer, "route_stages", "R1");
    assert_eq!(h["exists"], true);
    assert_eq!(h["live"]["stages"], json!([]));
    let h = history!(viewer, "stage", "NO_SUCH_STAGE");
    assert_eq!(
        (h["exists"].clone(), h["versions"].clone()),
        (json!(false), json!([]))
    );
    let h = history!(viewer, "station", "NO_SUCH_STATION");
    assert_eq!(h["exists"], false);
    let h = history!(viewer, "agency", "NO_SUCH_AGENCY");
    assert_eq!(h["exists"], false);
    let (s, b, _) = call!(
        &app,
        viewer.req(
            "GET",
            &format!("/feeds/{FEED}/history/route_stops/R1?pattern_key=2")
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["node"]["pattern_key"], 2);
    assert_eq!(b["versions"], json!([]));

    exec(&pool, &clear()).await;
    let _ = std::fs::remove_dir_all(dir);
}
