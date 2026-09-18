//! Webhooks and pod cache state end to end (docs/gtfs-editor.md section 12)
//! against a real Postgres holding the editor schema (db/gtfs_editor/0001..0013)
//! and a real HTTP receiver on localhost.
//!
//! What it proves:
//!
//!   - a webhook fires only once every live pod is serving the committed
//!     version, and only after the settle window;
//!   - it fires **exactly once** even though every pod dispatches at the same
//!     moment - the point of the whole design;
//!   - the request carries the credential from the pod's environment, and that
//!     credential never reaches the database, the API or an error message;
//!   - a receiver that rejects the call is retried and then recorded as failed;
//!   - the API: only an admin configures a webhook, only a host the deployment
//!     allows can be used, and cache-state names the pod being waited for.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local (see scripts/editor_flow_test.sh). Uses its own feed and
//! accounts and removes them afterwards; never touches chennai_bus.

use actix_web::{test, web, App, HttpResponse, HttpServer};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::services::webhook::{self, PodIdentity, WebhookPolicy};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

const AUD: &str = "gtfs.editor-webhook-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_webhook_test_feed";
const ADMIN: &str = "admin@editor-webhook-test.invalid";
const EDITOR_USER: &str = "editor@editor-webhook-test.invalid";
const TOKEN_VAR: &str = "GIMS_WEBHOOK_FLOW_TEST_TOKEN";
const TOKEN: &str = "sup3r-s3cret-jenkins-token";

// ---------------------------------------------------------------- receiver

#[derive(Default)]
struct Received {
    hits: Vec<Hit>,
}

struct Hit {
    query: String,
    auth: Option<String>,
    body: Value,
}

/// A stand-in for the Jenkins job: records what it was sent and answers with
/// whatever status the test currently wants.
async fn receive(
    req: actix_web::HttpRequest,
    body: web::Bytes,
    log: web::Data<Arc<Mutex<Received>>>,
    status: web::Data<Arc<AtomicU16>>,
) -> HttpResponse {
    log.lock().unwrap().hits.push(Hit {
        query: req.query_string().to_string(),
        auth: req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    });
    HttpResponse::build(
        actix_web::http::StatusCode::from_u16(status.load(Ordering::SeqCst)).unwrap(),
    )
    .body("ok")
}

fn start_receiver() -> (String, Arc<Mutex<Received>>, Arc<AtomicU16>) {
    let log = Arc::new(Mutex::new(Received::default()));
    let status = Arc::new(AtomicU16::new(200));
    let (l, s) = (log.clone(), status.clone());
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(l.clone()))
            .app_data(web::Data::new(s.clone()))
            .route("/build", web::to(receive))
    })
    .workers(1)
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    let running = server.run();
    tokio::spawn(running);
    (format!("http://127.0.0.1:{}", addr.port()), log, status)
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
    let accounts = format!("'{ADMIN}', '{EDITOR_USER}'");
    for stmt in [
        format!("DELETE FROM gtfs_pod_feed_state WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_webhook_delivery WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_webhook WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
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

fn state(
    pool: &PgPool,
    signer: &TestSigner,
    policy: WebhookPolicy,
) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-webhook-{}", crypto::random_token()));
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
            webhook_policy: policy,
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
            .set_json(json!({"code": crypto::totp_now(&secret, now_ts())}))
    );
    assert_eq!(s, 200, "{b}");
    c.session = cookie;
}

// ---------------------------------------------------------------- helpers

async fn set_version(pool: &PgPool, version: i64) {
    sqlx::query("UPDATE gtfs_feed SET version = $2 WHERE gtfs_id = $1")
        .bind(FEED)
        .bind(version)
        .execute(pool)
        .await
        .unwrap();
}

async fn beat(pool: &PgPool, pod: &PodIdentity, version: i64) {
    webhook::heartbeat(pool, pod, FEED, version, "db", None)
        .await
        .unwrap();
}

async fn deliveries(pool: &PgPool) -> Vec<(String, i64, i32, Option<i32>, Option<String>)> {
    sqlx::query(
        "SELECT status, feed_version, attempts, response_status, last_error \
           FROM gtfs_webhook_delivery WHERE gtfs_id = $1 AND kind = 'event' \
          ORDER BY feed_version",
    )
    .bind(FEED)
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| {
        (
            r.get::<String, _>("status"),
            r.get::<i64, _>("feed_version"),
            r.get::<i32, _>("attempts"),
            r.get::<Option<i32>, _>("response_status"),
            r.get::<Option<String>, _>("last_error"),
        )
    })
    .collect()
}

#[actix_web::test]
async fn a_webhook_fires_once_the_whole_fleet_is_serving_the_edit() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;
    std::env::set_var(TOKEN_VAR, TOKEN);

    let (base_url, log, receiver_status) = start_receiver();
    let policy = WebhookPolicy {
        enabled: true,
        allowed_hosts: vec!["127.0.0.1".to_string()],
    };
    let signer = TestSigner::generate("webhook-test-key");
    let (st, dir) = state(&pool, &signer, policy.clone());
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
         VALUES ($1, 'Webhook test feed', 1, 'db')",
    )
    .bind(FEED)
    .execute(&pool)
    .await
    .unwrap();

    // ---------------------------------------------------- the allow-list bites
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/webhooks"))
            .set_json(json!({
                "name": "somewhere else",
                "url": "https://evil.example.com/build",
            }))
    );
    assert_eq!(s, 400, "{b}");
    assert_eq!(code_of(&b), "host_not_allowed");

    // a placeholder that no environment variable can fill is caught at save
    // time, not at the first delivery
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/webhooks"))
            .set_json(json!({
                "name": "missing secret",
                "url": format!("{base_url}/build?token=${{GIMS_NO_SUCH_VAR_HERE}}"),
            }))
    );
    assert_eq!(s, 400, "{b}");
    assert_eq!(code_of(&b), "invalid_url");

    // ---------------------------------------------------- only an admin
    let mut editor_user = Caller {
        signer: &signer,
        email: EDITOR_USER.into(),
        session: None,
    };
    sqlx::query(
        "INSERT INTO gtfs_editor_user (email, role, status) VALUES ($1, 'editor', 'active') \
         ON CONFLICT (lower(email)) DO UPDATE SET role = 'editor', status = 'active'",
    )
    .bind(EDITOR_USER)
    .execute(&pool)
    .await
    .unwrap();
    sign_in(&app, &mut editor_user).await;
    let (s, b, _) = call!(
        &app,
        editor_user
            .req("POST", &format!("/feeds/{FEED}/webhooks"))
            .set_json(json!({"name": "nope", "url": format!("{base_url}/build")}))
    );
    assert_eq!(s, 403, "an editor must not configure a webhook: {b}");

    // ---------------------------------------------------- create it properly
    let (s, hook, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/webhooks"))
            .set_json(json!({
                "name": "frontline rebuild",
                "event": "feed_in_sync",
                // the credential is a placeholder; the literal never goes in
                "url": format!("{base_url}/build?token=${{{TOKEN_VAR}}}&FEED=${{event:gtfs_id}}"),
                "method": "POST",
                "headers": {"Authorization": format!("Bearer ${{{TOKEN_VAR}}}")},
                "settle_seconds": 0,
                "stale_after_seconds": 60,
            }))
    );
    assert_eq!(s, 201, "{hook}");
    let webhook_id = hook["webhook_id"].as_str().unwrap().to_string();

    // what is stored is the template, never the resolved secret
    let stored: String = sqlx::query("SELECT url FROM gtfs_webhook WHERE gtfs_id = $1")
        .bind(FEED)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("url");
    assert!(
        !stored.contains(TOKEN) && stored.contains(TOKEN_VAR),
        "the database must hold the placeholder, not the secret: {stored}"
    );
    assert!(
        !serde_json::to_string(&hook).unwrap().contains(TOKEN),
        "the API must not echo the resolved secret"
    );

    // ---------------------------------------------------- an edit lands
    set_version(&pool, 2).await;
    let pod_a = PodIdentity::from_env(Some("pod-a"));
    let pod_b = PodIdentity::from_env(Some("pod-b"));
    let http = reqwest::Client::new();

    // only pod-a has reloaded: nothing may fire
    beat(&pool, &pod_a, 2).await;
    beat(&pool, &pod_b, 1).await;
    webhook::dispatch_tick(&pool, &http, &pod_a, &policy).await;
    webhook::dispatch_tick(&pool, &http, &pod_b, &policy).await;
    assert!(
        deliveries(&pool).await.is_empty(),
        "a laggard pod must hold the delivery back"
    );
    assert_eq!(log.lock().unwrap().hits.len(), 0);

    // the dashboard says exactly who is being waited for
    let (s, cache, _) = call!(
        &app,
        admin.req("GET", &format!("/feeds/{FEED}/cache-state"))
    );
    assert_eq!(s, 200, "{cache}");
    assert_eq!(cache["in_sync"], json!(false));
    assert_eq!(cache["version"], json!(2));
    assert_eq!(cache["live_pods"], json!(2));
    assert_eq!(cache["waiting_for"][0]["pod_id"], json!("pod-b"));
    assert_eq!(cache["waiting_for"][0]["loaded_version"], json!(1));

    // ---------------------------------------------------- the fleet catches up
    beat(&pool, &pod_b, 2).await;

    // every pod dispatches at once, as they really do
    let (a, b) = tokio::join!(
        webhook::dispatch_tick(&pool, &http, &pod_a, &policy),
        webhook::dispatch_tick(&pool, &http, &pod_b, &policy),
    );
    let _ = (a, b);
    // and a third tick, to be sure a later poll does not send it again
    webhook::dispatch_tick(&pool, &http, &pod_a, &policy).await;

    let rows = deliveries(&pool).await;
    assert_eq!(rows.len(), 1, "exactly one delivery row: {rows:?}");
    assert_eq!(rows[0].0, "succeeded", "{rows:?}");
    assert_eq!(rows[0].1, 2);
    assert_eq!(rows[0].3, Some(200));

    let hits = log.lock().unwrap();
    assert_eq!(
        hits.hits.len(),
        1,
        "the receiver must be called exactly once"
    );
    let hit = &hits.hits[0];
    assert!(
        hit.query.contains(&format!("token={TOKEN}")),
        "the credential must be resolved from the environment: {}",
        hit.query
    );
    assert!(hit.query.contains(&format!("FEED={FEED}")), "{}", hit.query);
    assert_eq!(hit.auth.as_deref(), Some(&format!("Bearer {TOKEN}")[..]));
    assert_eq!(hit.body["event"], json!("feed_in_sync"));
    assert_eq!(hit.body["feed_version"], json!(2));
    assert_eq!(hit.body["gtfs_id"], json!(FEED));
    assert_eq!(hit.body["pod_count"], json!(2));
    drop(hits);

    let (s, cache, _) = call!(
        &app,
        admin.req("GET", &format!("/feeds/{FEED}/cache-state"))
    );
    assert_eq!(s, 200, "{cache}");
    assert_eq!(cache["in_sync"], json!(true));
    assert_eq!(cache["waiting_for"], json!([]));

    // ---------------------------------------------------- a rejected call
    receiver_status.store(500, Ordering::SeqCst);
    set_version(&pool, 3).await;
    beat(&pool, &pod_a, 3).await;
    beat(&pool, &pod_b, 3).await;
    // max_attempts lowered so the test does not wait out a real backoff
    let (s, b, _) = call!(
        &app,
        admin
            .req("PATCH", &format!("/webhooks/{webhook_id}"))
            .set_json(json!({"max_attempts": 1}))
    );
    assert_eq!(s, 200, "{b}");

    webhook::dispatch_tick(&pool, &http, &pod_a, &policy).await;
    let rows = deliveries(&pool).await;
    let failed = rows.iter().find(|r| r.1 == 3).expect("a row for v3");
    assert_eq!(failed.0, "failed", "{rows:?}");
    assert_eq!(failed.2, 1);
    assert_eq!(failed.3, Some(500));
    let error = failed.4.clone().unwrap_or_default();
    assert!(error.contains("500"), "{error}");
    assert!(
        !error.contains(TOKEN),
        "an error message must not carry the credential: {error}"
    );

    // the delivery history is readable, and still free of the secret
    let (s, hist, _) = call!(
        &app,
        admin.req("GET", &format!("/feeds/{FEED}/webhook-deliveries"))
    );
    assert_eq!(s, 200, "{hist}");
    assert_eq!(hist["items"].as_array().unwrap().len(), 2);
    assert!(!serde_json::to_string(&hist).unwrap().contains(TOKEN));

    // ---------------------------------------------------- a disabled webhook
    receiver_status.store(200, Ordering::SeqCst);
    let (s, b, _) = call!(
        &app,
        admin
            .req("PATCH", &format!("/webhooks/{webhook_id}"))
            .set_json(json!({"enabled": false}))
    );
    assert_eq!(s, 200, "{b}");
    set_version(&pool, 4).await;
    beat(&pool, &pod_a, 4).await;
    beat(&pool, &pod_b, 4).await;
    webhook::dispatch_tick(&pool, &http, &pod_a, &policy).await;
    assert!(
        !deliveries(&pool).await.iter().any(|r| r.1 == 4),
        "a disabled webhook must not fire"
    );

    // ---------------------------------------------------- delete
    let (s, b, _) = call!(
        &app,
        admin.req("DELETE", &format!("/webhooks/{webhook_id}"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, list, _) = call!(&app, admin.req("GET", &format!("/feeds/{FEED}/webhooks")));
    assert_eq!(s, 200, "{list}");
    assert_eq!(list["items"].as_array().unwrap().len(), 0);

    clear(&pool).await;
    std::env::remove_var(TOKEN_VAR);
    let _ = std::fs::remove_dir_all(dir);
}

/// A pod that stops heartbeating must not hold a delivery back for ever: the
/// fleet moves on without it once its row goes stale.
#[actix_web::test]
async fn a_dead_pod_stops_counting_and_the_delivery_goes_out() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let feed = format!("{FEED}_dead");
    sqlx::query("DELETE FROM gtfs_pod_feed_state WHERE gtfs_id = $1")
        .bind(&feed)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM gtfs_webhook_delivery WHERE gtfs_id = $1")
        .bind(&feed)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM gtfs_webhook WHERE gtfs_id = $1")
        .bind(&feed)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM gtfs_feed WHERE gtfs_id = $1")
        .bind(&feed)
        .execute(&pool)
        .await
        .unwrap();

    let (base_url, log, _status) = start_receiver();
    let policy = WebhookPolicy {
        enabled: true,
        allowed_hosts: vec!["127.0.0.1".to_string()],
    };

    sqlx::query(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, version, data_source) \
         VALUES ($1, 'Dead pod test', 1, 'db')",
    )
    .bind(&feed)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gtfs_webhook (gtfs_id, name, event, url, settle_seconds, stale_after_seconds) \
         VALUES ($1, 'rebuild', 'feed_in_sync', $2, 0, 10)",
    )
    .bind(&feed)
    .bind(format!("{base_url}/build"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE gtfs_feed SET version = 2 WHERE gtfs_id = $1")
        .bind(&feed)
        .execute(&pool)
        .await
        .unwrap();

    let alive = PodIdentity::from_env(Some("pod-alive"));
    let dead = PodIdentity::from_env(Some("pod-dead"));
    let http = reqwest::Client::new();
    webhook::heartbeat(&pool, &alive, &feed, 2, "db", None)
        .await
        .unwrap();
    webhook::heartbeat(&pool, &dead, &feed, 1, "db", None)
        .await
        .unwrap();

    webhook::dispatch_tick(&pool, &http, &alive, &policy).await;
    assert_eq!(
        log.lock().unwrap().hits.len(),
        0,
        "a laggard that is still alive holds the fleet back"
    );

    // age the dead pod's heartbeat past the 10s staleness window
    sqlx::query(
        "UPDATE gtfs_pod_feed_state SET updated_at = now() - interval '5 minutes' \
          WHERE gtfs_id = $1 AND pod_id = 'pod-dead'",
    )
    .bind(&feed)
    .execute(&pool)
    .await
    .unwrap();

    webhook::dispatch_tick(&pool, &http, &alive, &policy).await;
    assert_eq!(
        log.lock().unwrap().hits.len(),
        1,
        "once the dead pod goes stale the delivery must go out"
    );
    let status: String = sqlx::query(
        "SELECT status FROM gtfs_webhook_delivery WHERE gtfs_id = $1 AND feed_version = 2",
    )
    .bind(&feed)
    .fetch_one(&pool)
    .await
    .unwrap()
    .get("status");
    assert_eq!(status, "succeeded");

    for t in [
        "gtfs_pod_feed_state",
        "gtfs_webhook_delivery",
        "gtfs_webhook",
        "gtfs_feed",
    ] {
        sqlx::query(&format!("DELETE FROM {t} WHERE gtfs_id = $1"))
            .bind(&feed)
            .execute(&pool)
            .await
            .unwrap();
    }
}
