//! The feed data source (docs/gtfs-editor.md "Feed data source") end to end
//! against a real Postgres holding the editor schema (db/gtfs_editor/0001..0011):
//! `GET /feeds/{g}/config` for a feed with no row (404) and with one, and the
//! switch itself, which is a `feed_config/update` change in a draft - only an
//! admin adds or edits one, a bad value or another feed's key is refused, the
//! same value is a warning, the open drafts carrying one are listed as `pending`,
//! and it goes live through submit, approval by someone else and commit: the
//! data source switched, the version bumped once, the audit row written at
//! commit. A draft overtaken by another commit is a conflict. The old
//! `POST /feeds/{g}/config` is gone.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host
//! that is not local (see scripts/editor_flow_test.sh). Uses its own feed and
//! accounts and removes them afterwards; never touches chennai_bus.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

const AUD: &str = "gtfs.editor-feed-config-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_feed_config_test_feed";
const ADMIN: &str = "admin@editor-feed-config-test.invalid";
const VIEWER: &str = "viewer@editor-feed-config-test.invalid";
const EDITOR: &str = "editor@editor-feed-config-test.invalid";
const APPROVER: &str = "approver@editor-feed-config-test.invalid";

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

// gtfs_audit_log is append-only (docs/gtfs-editor.md), so `clear` cannot
// remove old audit rows for FEED; `audit_high_water` lets each run only look
// at the rows it created itself.
async fn audit_high_water(pool: &PgPool) -> i64 {
    sqlx::query("SELECT COALESCE(MAX(audit_id), 0) FROM gtfs_audit_log WHERE gtfs_id = $1")
        .bind(FEED)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<i64, _>(0)
}

async fn clear(pool: &PgPool) {
    let accounts = format!("'{ADMIN}', '{VIEWER}', '{EDITOR}', '{APPROVER}'");
    for stmt in [
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
        format!(
            "DELETE FROM gtfs_editor_session WHERE user_id IN \
             (SELECT user_id FROM gtfs_editor_user WHERE email IN ({accounts}))"
        ),
        format!(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, \
             totp_last_step = NULL, status = 'active' WHERE email IN ({accounts})"
        ),
    ] {
        sqlx::query(&stmt)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
}

fn state(pool: &PgPool, signer: &TestSigner, admin: &str) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-feed-config-{}", crypto::random_token()));
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

#[actix_web::test]
async fn feed_config_goes_through_a_draft() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;

    let signer = TestSigner::generate("feed-config-test-key");
    let (st, dir) = state(&pool, &signer, ADMIN);
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
    // an account left by an earlier run may carry another role
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

    // A feed with no gtfs_feed row at all: 404. The dashboard never creates
    // one - nandi's seed does - and a change set needs the row to exist.
    let (s, b, _) = call!(&app, admin.req("GET", &format!("/feeds/{FEED}/config")));
    assert_eq!((s, code_of(&b)), (404, "feed_not_found"), "{b}");
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "no such feed"}))
    );
    assert_eq!((s, code_of(&b)), (404, "feed_not_found"), "{b}");

    sqlx::query(
        "INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ($1, 'Feed config test feed')",
    )
    .bind(FEED)
    .execute(&pool)
    .await
    .unwrap();
    let before = audit_high_water(&pool).await;
    let config = |c: &Caller| c.req("GET", &format!("/feeds/{FEED}/config"));
    let (s, b, _) = call!(&app, config(&viewer));
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b,
        json!({"gtfs_id": FEED, "data_source": "preprocessed", "version": 1, "pending": []})
    );

    // Nothing writes the data source directly any more.
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/config"))
            .set_json(json!({"data_source": "db"}))
    );
    assert_eq!((s, code_of(&b)), (404, "endpoint_not_found"), "{b}");
    let (_, b, _) = call!(&app, config(&viewer));
    assert_eq!(
        (&b["data_source"], &b["version"]),
        (&json!("preprocessed"), &json!(1))
    );

    let new_set = |c: &Caller, title: &str| {
        c.req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": title}))
    };
    let switch = |c: &Caller, set: &str, key: &str, to: &str| {
        c.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(
                json!({"entity": "feed_config", "op": "update", "entity_key": key,
                             "after": {"data_source": to}}),
            )
    };
    let (s, set, _) = call!(&app, new_set(&editor_c, "serve the feed from the database"));
    assert_eq!(s, 201, "{set}");
    let set = set["change_set_id"].as_str().unwrap().to_string();

    // ---- only an admin adds the change, even to someone else's draft
    for c in [&editor_c, &approver] {
        let (s, b, _) = call!(&app, switch(c, &set, FEED, "db"));
        assert_eq!((s, code_of(&b)), (403, "role_required"), "{b}");
    }
    // ---- a bad value, a stray field, another feed's key
    let (s, b, _) = call!(&app, switch(&admin, &set, FEED, "nonsense"));
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
    assert_eq!(b["error"]["details"]["code"], "invalid_data_source");
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(
                json!({"entity": "feed_config", "op": "update", "entity_key": FEED,
                             "after": {"data_source": "db", "version": 9}})
            )
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
    let (s, b, _) = call!(&app, switch(&admin, &set, "chennai_bus", "db"));
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
    assert_eq!(b["error"]["details"]["code"], "feed_mismatch");
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({"entity": "feed_config", "op": "delete", "entity_key": FEED}))
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");

    // ---- the same value is a warning, and switches nothing
    let (s, b, _) = call!(&app, switch(&admin, &set, FEED, "preprocessed"));
    assert_eq!(s, 201, "{b}");
    let same = b["change_id"].as_i64().unwrap();
    assert_eq!(b["validation"].as_array().unwrap().len(), 1, "{b}");
    let warning = &b["validation"][0];
    assert_eq!(
        (&warning["level"], &warning["code"], &warning["change_id"]),
        (
            &json!("warning"),
            &json!("data_source_unchanged"),
            &json!(same)
        )
    );
    assert_eq!(b["can_submit"], true);
    // only an admin edits it; the edit makes it a real switch
    let edit = |c: &Caller, to: &str| {
        c.req("PUT", &format!("/change-sets/{set}/changes/{same}"))
            .set_json(json!({"after": {"data_source": to}}))
    };
    let (s, b, _) = call!(&app, edit(&editor_c, "db"));
    assert_eq!((s, code_of(&b)), (403, "role_required"), "{b}");
    let (s, b, _) = call!(&app, edit(&admin, "bogus"));
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
    assert_eq!(b["error"]["details"]["code"], "invalid_data_source");
    let (s, b, _) = call!(&app, edit(&admin, "db"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["validation"], json!([]), "{b}");

    // ---- the change in read shape, and the draft listed as pending
    let change = &b["changes"][0];
    assert_eq!(
        (&change["entity"], &change["op"], &change["entity_key"]),
        (&json!("feed_config"), &json!("update"), &json!(FEED))
    );
    assert_eq!(
        change["before"],
        json!({"gtfs_id": FEED, "data_source": "preprocessed", "version": 1})
    );
    assert_eq!(change["after"], json!({"data_source": "db"}));
    assert!(change["base_row_version"].is_null(), "{change}");
    let (_, b, _) = call!(&app, config(&viewer));
    assert_eq!(
        b["pending"],
        json!([{"change_set_id": set, "change_set_title": "serve the feed from the database",
                "status": "draft", "change_id": same, "data_source": "db"}])
    );
    // a second switch in the same draft is judged after the first: back to
    // what is live is a switch, to 'db' again is not
    let (s, b, _) = call!(&app, switch(&admin, &set, FEED, "db"));
    assert_eq!(s, 201, "{b}");
    let again = b["change_id"].as_i64().unwrap();
    assert_eq!(b["validation"][0]["code"], "data_source_unchanged", "{b}");
    assert_eq!(b["validation"][0]["change_id"], again);
    let (s, b, _) = call!(
        &app,
        editor_c.req("DELETE", &format!("/change-sets/{set}/changes/{again}"))
    );
    assert_eq!(s, 200, "{b}");
    // the draft's other reads do not mind the change
    sqlx::query(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, agency_id) VALUES ($1, 'R1', '1', 'TESTAG')",
    )
    .bind(FEED)
    .execute(&pool)
    .await
    .unwrap();
    let (s, b, _) = call!(
        &app,
        viewer.req("GET", &format!("/change-sets/{set}/preview/routes/R1"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (&b["validation"], &b["conflicts"]),
        (&json!([]), &json!([]))
    );

    // ---- submit, approval by someone else, commit
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set}/submit"))
    );
    assert_eq!(s, 200, "{b}");
    let (_, b, _) = call!(&app, config(&viewer));
    assert_eq!(b["pending"][0]["status"], "submitted");
    // reviewed like any draft: not by its submitter, and by an approver or admin
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set}/approve"))
    );
    assert_eq!(s, 403, "{b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set}/approve"))
    );
    assert_eq!(s, 200, "{b}");
    let (_, b, _) = call!(&app, config(&viewer));
    assert_eq!(
        (&b["data_source"], &b["version"]),
        (&json!("preprocessed"), &json!(1))
    );
    assert_eq!(b["pending"][0]["status"], "approved");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set}/commit"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["feed_version"], 2);

    // switched, the version bumped once, nothing pending
    let (_, b, _) = call!(&app, config(&viewer));
    assert_eq!(
        b,
        json!({"gtfs_id": FEED, "data_source": "db", "version": 2, "pending": []})
    );
    let (_, feeds, _) = call!(&app, viewer.req("GET", "/feeds"));
    let listed = feeds["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["gtfs_id"] == FEED)
        .unwrap();
    assert_eq!(
        (&listed["data_source"], &listed["version"]),
        (&json!("db"), &json!(2))
    );
    let (s, b, _) = call!(&app, viewer.req("GET", &format!("/change-sets/{set}")));
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (&b["status"], &b["committed_version"]),
        (&json!("committed"), &json!(2))
    );

    // ---- audited at commit, not when drafted
    let rows = sqlx::query(
        "SELECT a.detail::text AS detail, a.change_set_id, u.email FROM gtfs_audit_log a \
         JOIN gtfs_editor_user u ON u.user_id = a.actor \
         WHERE a.gtfs_id = $1 AND a.action = 'feed_data_source_changed' AND a.audit_id > $2 ORDER BY a.audit_id",
    )
    .bind(FEED)
    .bind(before)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    let detail: Value = serde_json::from_str(&rows[0].get::<String, _>("detail")).unwrap();
    assert_eq!(
        detail,
        json!({"gtfs_id": FEED, "from": "preprocessed", "to": "db", "change_id": same,
               "change_set_id": set})
    );
    assert_eq!(rows[0].get::<String, _>("email"), APPROVER);
    assert_eq!(
        rows[0].get::<uuid::Uuid, _>("change_set_id").to_string(),
        set
    );
    let drafted: i64 = sqlx::query(
        "SELECT count(*) FROM gtfs_audit_log WHERE change_set_id = $1::uuid AND action = 'change_added' \
         AND detail->>'entity' = 'feed_config'",
    )
    .bind(&set)
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    assert_eq!(drafted, 2);

    // ---- a draft overtaken by another commit is a conflict
    let (_, a, _) = call!(&app, new_set(&admin, "back to the build (first)"));
    let (_, b, _) = call!(&app, new_set(&admin, "back to the build (second)"));
    let first = a["change_set_id"].as_str().unwrap().to_string();
    let second = b["change_set_id"].as_str().unwrap().to_string();
    for id in [&first, &second] {
        let (s, b, _) = call!(&app, switch(&admin, id, FEED, "preprocessed"));
        assert_eq!(s, 201, "{b}");
        let (s, b, _) = call!(
            &app,
            admin.req("POST", &format!("/change-sets/{id}/submit"))
        );
        assert_eq!(s, 200, "{b}");
        let (s, b, _) = call!(
            &app,
            approver.req("POST", &format!("/change-sets/{id}/approve"))
        );
        assert_eq!(s, 200, "{b}");
    }
    let (_, b, _) = call!(&app, config(&viewer));
    assert_eq!(b["pending"].as_array().unwrap().len(), 2, "{b}");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{first}/commit"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{second}/commit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    let conflict = &b["error"]["details"]["conflicts"][0];
    assert_eq!(
        (
            &conflict["entity"],
            &conflict["entity_key"],
            &conflict["reason"]
        ),
        (&json!("feed_config"), &json!(FEED), &json!("changed"))
    );
    assert_eq!(
        (&conflict["expected"], &conflict["actual"]),
        (&json!("db"), &json!("preprocessed"))
    );
    assert!(
        conflict["message"]
            .as_str()
            .unwrap()
            .starts_with(&format!("The data source of feed {FEED} was changed")),
        "{conflict}"
    );
    // nothing was applied by the refused commit, and its detail says why
    let (_, b, _) = call!(&app, config(&viewer));
    assert_eq!(
        (&b["data_source"], &b["version"]),
        (&json!("preprocessed"), &json!(3))
    );
    let (_, b, _) = call!(&app, viewer.req("GET", &format!("/change-sets/{second}")));
    assert_eq!(b["conflicts"].as_array().unwrap().len(), 1, "{b}");

    clear(&pool).await;
    std::fs::remove_dir_all(dir).ok();
}
