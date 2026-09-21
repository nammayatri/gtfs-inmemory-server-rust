//! The webhook policy as an editable setting (docs/gtfs-editor.md section 12.5)
//! against a real Postgres holding the editor schema
//! (db/gtfs_editor/0001..0016) and a real HTTP receiver on localhost.
//!
//! What it proves:
//!
//!   - with no row saved, the deployment's dhall values are what is in force,
//!     and the API says so (`source: "config"`);
//!   - a saved row **supersedes** them rather than adding to them: a host the
//!     deployment allowed stops being callable, and one it never allowed
//!     becomes callable;
//!   - the switch alone is enough - a deployment shipped with
//!     `gtfs_webhooks_enabled = False` delivers once the row says true, with no
//!     restart and nothing else changed;
//!   - the dispatcher reads the list at the moment of the request, so taking a
//!     host out stops an already-configured webhook from being called;
//!   - only an admin may save, a viewer may read, a host that is not a host
//!     name is refused, the list is de-duplicated, and on-with-an-empty-list is
//!     legal and fires nothing;
//!   - the change is audited with what it was and what it became.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local (see scripts/editor_flow_test.sh). Uses its own feed and
//! accounts and removes them afterwards, and leaves the settings row as it
//! found it - absent; never touches chennai_bus.

use actix_web::{test, web, App, HttpResponse, HttpServer};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::services::webhook::{self, LivePolicy, PodIdentity, WebhookPolicy};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const AUD: &str = "gtfs.editor-webhook-settings-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_webhook_settings_test_feed";
const ADMIN: &str = "admin@editor-webhook-settings-test.invalid";
const VIEWER: &str = "viewer@editor-webhook-settings-test.invalid";

/// The host the deployment's own config allows. Nothing in this test ever
/// points at it: it is here so that every call that succeeds proves the saved
/// row, and not the configmap, decided.
const CONFIG_HOST: &str = "configured-only.invalid";

// ---------------------------------------------------------------- receiver

async fn receive(hits: web::Data<Arc<AtomicUsize>>) -> HttpResponse {
    hits.fetch_add(1, Ordering::SeqCst);
    HttpResponse::Ok().body("ok")
}

fn start_receiver() -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(h.clone()))
            .route("/build", web::to(receive))
    })
    .workers(1)
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    tokio::spawn(server.run());
    (format!("http://127.0.0.1:{}", addr.port()), hits)
}

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

fn now_ts() -> u64 {
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

async fn clear(pool: &PgPool) {
    let accounts = format!("'{ADMIN}', '{VIEWER}'");
    for stmt in [
        // the policy is one row for the whole deployment, so the test both
        // starts and ends with none: anything else would change what another
        // test's pods are allowed to call
        "DELETE FROM gtfs_webhook_settings".to_string(),
        format!("DELETE FROM gtfs_pod_feed_state WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_webhook_delivery WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_webhook WHERE gtfs_id = '{FEED}'"),
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
    ] {
        sqlx::query(&stmt)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
}

fn state(pool: &PgPool, signer: &TestSigner) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "editor-webhook-settings-{}",
        crypto::random_token()
    ));
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
            webhook_policy: LivePolicy::new(seed()),
        },
    )
    .unwrap();
    (st, dir)
}

/// A deployment with the feature **off** and one irrelevant host allowed: the
/// state every existing GIMS deployment is in today.
fn seed() -> WebhookPolicy {
    WebhookPolicy {
        enabled: false,
        allowed_hosts: vec![CONFIG_HOST.to_string()],
    }
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
            .set_json(json!({"code": crypto::totp_now(&secret, now_ts())}))
    );
    assert_eq!(s, 200, "{b}");
    c.session = cookie;
}

async fn set_version(pool: &PgPool, version: i64) {
    sqlx::query("UPDATE gtfs_feed SET version = $2 WHERE gtfs_id = $1")
        .bind(FEED)
        .bind(version)
        .execute(pool)
        .await
        .unwrap();
}

async fn delivery_of(pool: &PgPool, version: i64) -> (String, Option<String>) {
    let r = sqlx::query(
        "SELECT status, last_error FROM gtfs_webhook_delivery \
          WHERE gtfs_id = $1 AND feed_version = $2",
    )
    .bind(FEED)
    .bind(version)
    .fetch_one(pool)
    .await
    .unwrap();
    (r.get("status"), r.get("last_error"))
}

// ---------------------------------------------------------------- the flow

/// One test, not several: the policy is a single row for the whole deployment,
/// so two of these running at once would be editing each other's setting.
#[actix_web::test]
async fn the_saved_policy_supersedes_the_deployment_config() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;

    let (base_url, hits) = start_receiver();
    let signer = TestSigner::generate("webhook-settings-test-key");
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

    sqlx::query(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, version, data_source) \
         VALUES ($1, 'Webhook settings test feed', 1, 'db')",
    )
    .bind(FEED)
    .execute(&pool)
    .await
    .unwrap();

    // ------------------------------------------- with no row, dhall is in force
    let (s, b, _) = call!(&app, admin.req("GET", "/webhook-settings"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["policy"]["source"], json!("config"));
    assert_eq!(b["policy"]["enabled"], json!(false));
    assert_eq!(b["policy"]["allowed_hosts"], json!([CONFIG_HOST]));
    assert_eq!(b["policy"]["updated_by"], json!(null));
    assert_eq!(b["config"]["allowed_hosts"], json!([CONFIG_HOST]));

    // ------------------------------------------- a viewer reads but cannot save
    let mut viewer = Caller {
        signer: &signer,
        email: VIEWER.into(),
        session: None,
    };
    sqlx::query(
        "INSERT INTO gtfs_editor_user (email, role, status) VALUES ($1, 'viewer', 'active') \
         ON CONFLICT (lower(email)) DO UPDATE SET role = 'viewer', status = 'active'",
    )
    .bind(VIEWER)
    .execute(&pool)
    .await
    .unwrap();
    sign_in(&app, &mut viewer).await;
    let (s, b, _) = call!(&app, viewer.req("GET", "/webhook-settings"));
    assert_eq!(s, 200, "a viewer may read the policy: {b}");
    let (s, b, _) = call!(
        &app,
        viewer
            .req("PUT", "/webhook-settings")
            .set_json(json!({"enabled": true, "allowed_hosts": ["127.0.0.1"]}))
    );
    assert_eq!(s, 403, "a viewer must not save the policy: {b}");

    // ------------------------------------------- what is not a host name
    for bad in [
        "https://jenkins.example.com",
        "jenkins.example.com:8080",
        "jenkins.example.com/job/x",
        "jenkins example.com",
        "*.example.com",
        "",
    ] {
        let (s, b, _) = call!(
            &app,
            admin
                .req("PUT", "/webhook-settings")
                .set_json(json!({"enabled": true, "allowed_hosts": [bad]}))
        );
        assert_eq!(s, 400, "{bad:?} should be refused: {b}");
        assert_eq!(code_of(&b), "invalid_host", "{bad:?}: {b}");
    }
    // nothing was written by any of those
    let (s, b, _) = call!(&app, admin.req("GET", "/webhook-settings"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["policy"]["source"], json!("config"));

    // ------------------------------------------- save it, and the row wins
    let (s, b, _) = call!(
        &app,
        admin.req("PUT", "/webhook-settings").set_json(json!({
            "enabled": true,
            // spelt carelessly on purpose: trimmed, lowercased, de-duplicated
            "allowed_hosts": ["  127.0.0.1 ", "127.0.0.1", ".Example.COM"],
        }))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["policy"]["source"], json!("database"));
    assert_eq!(b["policy"]["enabled"], json!(true));
    assert_eq!(
        b["policy"]["allowed_hosts"],
        json!(["127.0.0.1", ".example.com"])
    );
    assert_eq!(b["policy"]["active"], json!(true));
    assert_eq!(b["policy"]["updated_by"], json!(ADMIN));
    // the deployment's own values are still reported, and are still what would
    // apply if this row were ever removed
    assert_eq!(b["config"]["enabled"], json!(false));
    assert_eq!(b["config"]["allowed_hosts"], json!([CONFIG_HOST]));

    // the change is in the history, with both sides of it
    let audit = sqlx::query(
        "SELECT detail FROM gtfs_audit_log WHERE action = 'webhook_settings_updated' \
          ORDER BY at DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap()
    .get::<Value, _>("detail");
    assert_eq!(audit["from"]["source"], json!("config"));
    assert_eq!(audit["from"]["enabled"], json!(false));
    assert_eq!(
        audit["to"]["allowed_hosts"],
        json!(["127.0.0.1", ".example.com"])
    );

    // ------------------------------------------- superseded, not widened
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/webhooks"))
            .set_json(json!({
                "name": "the deployment's host",
                "url": format!("https://{CONFIG_HOST}/build"),
            }))
    );
    assert_eq!(s, 400, "the config's host must stop being callable: {b}");
    assert_eq!(code_of(&b), "host_not_allowed");

    // and a host the deployment never allowed now is
    let (s, hook, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/webhooks"))
            .set_json(json!({
                "name": "frontline rebuild",
                "url": format!("{base_url}/build"),
                "settle_seconds": 0,
                "stale_after_seconds": 60,
            }))
    );
    assert_eq!(s, 201, "{hook}");

    // the feed's own page reports the same live policy, and where it came from
    let (s, list, _) = call!(&app, admin.req("GET", &format!("/feeds/{FEED}/webhooks")));
    assert_eq!(s, 200, "{list}");
    assert_eq!(list["policy"]["source"], json!("database"));
    assert_eq!(
        list["policy"]["allowed_hosts"],
        json!(["127.0.0.1", ".example.com"])
    );

    // ------------------------------------------- the switch alone delivers
    // The deployment still says gtfs_webhooks_enabled = False. Nothing about it
    // has changed and no pod has restarted; the row is the whole difference.
    let live = LivePolicy::new(seed());
    assert!(!live.seed().enabled);
    let effective = live.get(&pool).await;
    assert!(effective.policy.enabled, "the row turns the feature on");

    let pod = PodIdentity::from_env(Some("settings-pod-a"));
    let http = reqwest::Client::new();
    set_version(&pool, 2).await;
    webhook::heartbeat(&pool, &pod, FEED, 2, "db", None)
        .await
        .unwrap();
    webhook::dispatch_tick(&pool, &http, &pod, &effective.policy).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "a webhook enabled only in the database must be delivered"
    );
    assert_eq!(delivery_of(&pool, 2).await.0, "succeeded");

    // ------------------------------------------- the list is read at send time
    let (s, b, _) = call!(
        &app,
        admin
            .req("PUT", "/webhook-settings")
            .set_json(json!({"allowed_hosts": ["elsewhere.invalid"]}))
    );
    assert_eq!(s, 200, "{b}");
    // only the hosts were sent, so the switch is left as it was
    assert_eq!(b["policy"]["enabled"], json!(true));

    set_version(&pool, 3).await;
    webhook::heartbeat(&pool, &pod, FEED, 3, "db", None)
        .await
        .unwrap();
    let effective = live.get(&pool).await;
    webhook::dispatch_tick(&pool, &http, &pod, &effective.policy).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "the receiver's host is no longer allowed, so nothing may be sent to it"
    );
    let (status, error) = delivery_of(&pool, 3).await;
    assert_eq!(
        status, "pending",
        "it is a configuration fault, so it retries"
    );
    assert!(
        error.unwrap_or_default().contains("allow-list"),
        "the delivery says why it could not go out"
    );

    // ------------------------------------------- on, with nothing allowed
    let (s, b, _) = call!(
        &app,
        admin
            .req("PUT", "/webhook-settings")
            .set_json(json!({"allowed_hosts": []}))
    );
    assert_eq!(s, 200, "an empty list is a legal policy: {b}");
    assert_eq!(b["policy"]["enabled"], json!(true));
    assert_eq!(b["policy"]["active"], json!(false), "and it fires nothing");
    let (s, b, _) = call!(
        &app,
        admin.req(
            "POST",
            &format!("/webhooks/{}/test", hook["webhook_id"].as_str().unwrap())
        )
    );
    assert_eq!(s, 400, "{b}");
    assert_eq!(code_of(&b), "webhooks_inactive");

    // ------------------------------------------- and back to the deployment
    sqlx::query("DELETE FROM gtfs_webhook_settings")
        .execute(&pool)
        .await
        .unwrap();
    let (s, b, _) = call!(&app, admin.req("GET", "/webhook-settings"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b["policy"]["source"],
        json!("config"),
        "with the row gone the deployment's values are in force again"
    );
    assert_eq!(b["policy"]["allowed_hosts"], json!([CONFIG_HOST]));

    clear(&pool).await;
    let _ = std::fs::remove_dir_all(dir);
}
