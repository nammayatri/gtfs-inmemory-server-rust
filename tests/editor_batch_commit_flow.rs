//! Several drafts put through in one call (docs/gtfs-editor.md section 3,
//! "Several drafts at once"): `POST /feeds/{g}/change-sets/commit` commits them
//! in one transaction as **one** feed version, so the pods reload once and every
//! webhook (the Jenkins build) fires once; `.../approve` approves them in one
//! call. A draft that fails is reported and the rest go through.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. It uses its own feed and accounts and removes its rows afterwards.
//! See scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

const FEED: &str = "editor_batch_test_feed";
const AUD: &str = "gtfs.editor-batch-test.local";
const ADMIN: &str = "admin@editor-batch-test.invalid";
const EDITOR: &str = "editor@editor-batch-test.invalid";
const APPROVER: &str = "approver@editor-batch-test.invalid";
const BASE: &str = "/internal/gtfs-editor";

// ---------------------------------------------------------------- harness

struct Caller<'a> {
    signer: &'a TestSigner,
    email: &'static str,
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
            self.signer.token_for(self.email, AUD, 300),
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
            .max_connections(6)
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
        format!("DELETE FROM gtfs_position_review WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_station_proposal WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("UPDATE gtfs_stop SET parent_station = NULL WHERE gtfs_id = '{FEED}' AND parent_station IS NOT NULL"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
    ]
}

/// The feed: six stops S1..S6 and nothing else.
fn seed() -> Vec<String> {
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, headsign_source) VALUES ('{FEED}', 'Editor batch test feed', 'fare_stage')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', 'S' || i, 'S' || i, 'STOP ' || i, 13.0 + i * 0.001, 80.2 FROM generate_series(1, 6) i"
    ));
    let list = [ADMIN, EDITOR, APPROVER]
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(", ");
    s.push(format!(
        "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, totp_last_step = NULL, \
         status = 'active' WHERE email IN ({list})"
    ));
    s.push(format!(
        "DELETE FROM gtfs_editor_session WHERE user_id IN (SELECT user_id FROM gtfs_editor_user WHERE email IN ({list}))"
    ));
    s
}

fn state(pool: &PgPool, signer: &TestSigner) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-batch-{}", crypto::random_token()));
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

async fn feed_version(pool: &PgPool) -> i64 {
    sqlx::query("SELECT version FROM gtfs_feed WHERE gtfs_id = $1")
        .bind(FEED)
        .fetch_one(pool)
        .await
        .unwrap()
        .get("version")
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------- the flow

#[actix_web::test]
async fn several_drafts_go_live_as_one_feed_version() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("batch-test-key");
    let (st, dir) = state(&pool, &signer);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    // ---- accounts
    let mut admin = Caller {
        signer: &signer,
        email: ADMIN,
        session: None,
    };
    let mut editor_c = Caller {
        signer: &signer,
        email: EDITOR,
        session: None,
    };
    let mut approver = Caller {
        signer: &signer,
        email: APPROVER,
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
        // since 0018 a member works on a feed only through a grant on it
        let (s, b, _) = call!(
            &app,
            admin
                .req("PUT", &format!("/users/{id}/feeds/{FEED}"))
                .set_json(json!({"role": role}))
        );
        assert_eq!(s, 200, "{b}");
    }
    for c in [&mut editor_c, &mut approver] {
        let (s, b, _) = call!(&app, c.req("POST", "/auth/totp/enroll"));
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
    let editor_c = editor_c;
    let approver = approver;

    // a draft by `who` renaming `stop`, submitted
    macro_rules! submitted {
        ($who:expr, $title:expr, $stop:expr, $name:expr) => {{
            let (s, cs, _) = call!(
                &app,
                $who.req("POST", &format!("/feeds/{FEED}/change-sets"))
                    .set_json(json!({"title": $title}))
            );
            assert_eq!(s, 201, "{cs}");
            let id = cs["change_set_id"].as_str().unwrap().to_string();
            let (s, b, _) = call!(
                &app,
                $who.req("POST", &format!("/change-sets/{id}/changes")).set_json(
                    json!({"entity": "stop", "op": "update", "entity_key": $stop, "after": {"name": $name}})
                )
            );
            assert_eq!(s, 201, "{b}");
            let (s, b, _) = call!(&app, $who.req("POST", &format!("/change-sets/{id}/submit")));
            assert_eq!(s, 200, "{b}");
            id
        }};
    }

    // ---- approve and commit in one call: three drafts, one of which is
    // overtaken by the first, go live as one version
    let a = submitted!(editor_c, "rename S1", "S1", "STOP ONE A");
    let b = submitted!(editor_c, "rename S2", "S2", "STOP TWO");
    let c = submitted!(editor_c, "rename S1 again", "S1", "STOP ONE C");
    let d = submitted!(editor_c, "rename S3", "S3", "STOP THREE");
    let before = feed_version(&pool).await;

    // an editor may not put drafts live
    let (s, body, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets/commit"))
            .set_json(json!({"change_set_ids": [a, b], "approve": true}))
    );
    assert_eq!(s, 403, "{body}");

    let (s, out, _) = call!(
        &app,
        approver
            .req("POST", &format!("/feeds/{FEED}/change-sets/commit"))
            .set_json(json!({"change_set_ids": [a, b, c, d], "approve": true}))
    );
    assert_eq!(s, 200, "{out}");
    assert_eq!(
        ids(&out["committed"]),
        vec![a.clone(), b.clone(), d.clone()],
        "{out}"
    );
    let failed = out["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{out}");
    assert_eq!(failed[0]["change_set_id"], c.as_str());
    assert_eq!(failed[0]["stage"], "commit");
    assert_eq!(failed[0]["approved"], true);
    assert_eq!(failed[0]["error"]["code"], "change_set_conflicts", "{out}");
    // one version for all three
    assert_eq!(out["feed_version"], before + 1, "{out}");
    assert_eq!(feed_version(&pool).await, before + 1);
    for id in [&a, &b, &d] {
        let (_, cs, _) = call!(&app, approver.req("GET", &format!("/change-sets/{id}")));
        assert_eq!(cs["status"], "committed", "{cs}");
        assert_eq!(cs["committed_version"], before + 1, "{cs}");
    }
    // the overtaken one was approved but not committed, and waits under Approved
    let (_, cs, _) = call!(&app, approver.req("GET", &format!("/change-sets/{c}")));
    assert_eq!(cs["status"], "approved", "{cs}");
    let names: Vec<(String, String)> = sqlx::query(
        "SELECT stop_id, name FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id IN ('S1', 'S2', 'S3') ORDER BY stop_id",
    )
    .bind(FEED)
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get("stop_id"), r.get("name")))
    .collect();
    assert_eq!(
        names,
        vec![
            ("S1".into(), "STOP ONE A".into()),
            ("S2".into(), "STOP TWO".into()),
            ("S3".into(), "STOP THREE".into()),
        ]
    );

    // ---- approve in one call, then commit in another: a draft the approver
    // submitted is refused (maker-checker) and the rest are approved
    let e = submitted!(editor_c, "rename S4", "S4", "STOP FOUR");
    let f = submitted!(editor_c, "rename S5", "S5", "STOP FIVE");
    let own = submitted!(approver, "rename S6", "S6", "STOP SIX");
    let (s, out, _) = call!(
        &app,
        approver
            .req("POST", &format!("/feeds/{FEED}/change-sets/approve"))
            .set_json(json!({"change_set_ids": [e, f, own, f], "comment": "fine"}))
    );
    assert_eq!(s, 200, "{out}");
    assert_eq!(ids(&out["approved"]), vec![e.clone(), f.clone()], "{out}");
    let failed = out["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "{out}");
    assert_eq!(failed[0]["change_set_id"], own.as_str());
    assert_eq!(failed[0]["stage"], "approve");
    assert_eq!(failed[0]["error"]["code"], "own_change_set", "{out}");
    // approving moves no version
    assert_eq!(feed_version(&pool).await, before + 1);

    let (s, out, _) = call!(
        &app,
        approver
            .req("POST", &format!("/feeds/{FEED}/change-sets/commit"))
            .set_json(json!({"change_set_ids": [e, f]}))
    );
    assert_eq!(s, 200, "{out}");
    assert_eq!(ids(&out["committed"]), vec![e.clone(), f.clone()], "{out}");
    assert_eq!(out["feed_version"], before + 2, "{out}");
    assert_eq!(feed_version(&pool).await, before + 2);

    // ---- nothing committed: no version at all
    let (s, out, _) = call!(
        &app,
        approver
            .req("POST", &format!("/feeds/{FEED}/change-sets/commit"))
            .set_json(json!({"change_set_ids": [own]}))
    );
    assert_eq!(s, 200, "{out}");
    assert!(out["committed"].as_array().unwrap().is_empty(), "{out}");
    assert_eq!(
        out["failed"][0]["error"]["code"], "change_set_not_approved",
        "{out}"
    );
    assert_eq!(out["feed_version"], Value::Null);
    assert_eq!(feed_version(&pool).await, before + 2);

    // ---- a set of another feed is not found, and an empty list is refused
    let (s, out, _) = call!(
        &app,
        approver
            .req("POST", &format!("/feeds/{FEED}/change-sets/commit"))
            .set_json(json!({"change_set_ids": [uuid_nil()]}))
    );
    assert_eq!(s, 200, "{out}");
    assert_eq!(
        out["failed"][0]["error"]["code"], "change_set_not_found",
        "{out}"
    );
    let (s, out, _) = call!(
        &app,
        approver
            .req("POST", &format!("/feeds/{FEED}/change-sets/approve"))
            .set_json(json!({"change_set_ids": []}))
    );
    assert_eq!(s, 400, "{out}");
    assert_eq!(code_of(&out), "no_change_sets");

    exec(&pool, &clear_feed()).await;
    let _ = std::fs::remove_dir_all(dir);
}

fn uuid_nil() -> &'static str {
    "00000000-0000-0000-0000-000000000000"
}
