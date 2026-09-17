//! `GET`/`POST /feeds/{g}/config` (docs/gtfs-editor.md "Feed data source") end
//! to end against a real Postgres holding the editor schema
//! (db/gtfs_editor/0001..0008): reading a feed with no row (404), a viewer
//! reading a feed that has one, a non-admin refused the POST, an invalid
//! value refused, first switch inserting a row at version 1, a later switch
//! updating it and bumping version, and the audit row each POST leaves.
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
    for stmt in [
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
        format!(
            "DELETE FROM gtfs_editor_session WHERE user_id IN \
             (SELECT user_id FROM gtfs_editor_user WHERE email IN ('{ADMIN}', '{VIEWER}'))"
        ),
        format!(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, \
             totp_last_step = NULL, status = 'active' WHERE email IN ('{ADMIN}', '{VIEWER}')"
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
async fn feed_config_get_and_post() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;

    let signer = TestSigner::generate("feed-config-test-key");
    let (st, _dir) = state(&pool, &signer, ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    sign_in(&app, &mut admin).await;

    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/users")
            .set_json(json!({"email": VIEWER, "role": "viewer"}))
    );
    assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    let mut viewer = Caller {
        signer: &signer,
        email: VIEWER.into(),
        session: None,
    };
    sign_in(&app, &mut viewer).await;

    let before = audit_high_water(&pool).await;

    // A feed with no gtfs_feed row at all: 404.
    let (s, b, _) = call!(&app, admin.req("GET", &format!("/feeds/{FEED}/config")));
    assert_eq!(s, 404, "{b}");
    assert_eq!(code_of(&b), "feed_not_found");

    // A non-admin cannot flip the data source.
    let (s, b, _) = call!(
        &app,
        viewer
            .req("POST", &format!("/feeds/{FEED}/config"))
            .set_json(json!({"data_source": "db"}))
    );
    assert_eq!(s, 403, "{b}");

    // An invalid value is refused, even for an admin.
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/config"))
            .set_json(json!({"data_source": "nonsense"}))
    );
    assert_eq!(s, 400, "{b}");
    assert_eq!(code_of(&b), "invalid_data_source");

    // First switch: no row yet, so one is created at version 1.
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/config"))
            .set_json(json!({"data_source": "db"}))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["gtfs_id"], FEED);
    assert_eq!(b["data_source"], "db");
    assert_eq!(b["version"], 1);

    // A viewer can now read it.
    let (s, b, _) = call!(&app, viewer.req("GET", &format!("/feeds/{FEED}/config")));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["data_source"], "db");
    assert_eq!(b["version"], 1);

    // Flip it back: the existing row is updated and version bumps.
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/config"))
            .set_json(json!({"data_source": "preprocessed"}))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["data_source"], "preprocessed");
    assert_eq!(b["version"], 2);

    // Both POSTs are audited, with the right from/to.
    let rows = sqlx::query(
        "SELECT detail->>'from' AS from_val, detail->>'to' AS to_val \
         FROM gtfs_audit_log WHERE gtfs_id = $1 AND action = 'feed_data_source_changed' \
         AND audit_id > $2 ORDER BY audit_id",
    )
    .bind(FEED)
    .bind(before)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    let from0: Option<String> = rows[0].try_get("from_val").unwrap();
    let to0: String = rows[0].try_get("to_val").unwrap();
    assert_eq!(from0, None);
    assert_eq!(to0, "db");
    let from1: Option<String> = rows[1].try_get("from_val").unwrap();
    let to1: String = rows[1].try_get("to_val").unwrap();
    assert_eq!(from1.as_deref(), Some("db"));
    assert_eq!(to1, "preprocessed");

    clear(&pool).await;
}
