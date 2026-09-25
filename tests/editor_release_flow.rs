//! The Release to Nandi button end to end (docs/gtfs-editor.md section 12.6)
//! against a real Postgres holding the editor schema (db/gtfs_editor/0001..0022)
//! and a real HTTP receiver standing in for Jenkins.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local (see scripts/editor_flow_test.sh). Uses its own feed and
//! accounts and removes them afterwards; never touches chennai_bus.

use actix_web::{test, web, App, HttpResponse, HttpServer};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::services::webhook::{self, LivePolicy, PodIdentity, WebhookPolicy};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

const AUD: &str = "gtfs.editor-release-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_release_test_feed";
const ADMIN: &str = "admin@editor-release-test.invalid";
const APPROVER: &str = "approver@editor-release-test.invalid";
const EDITOR_USER: &str = "editor@editor-release-test.invalid";
const TOKEN_VAR: &str = "GIMS_RELEASE_FLOW_TEST_TOKEN";
const TOKEN: &str = "release-jenkins-token";

// both tests use one feed and one set of accounts, so they take turns
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let accounts = format!("'{ADMIN}', '{APPROVER}', '{EDITOR_USER}'");
    for stmt in [
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

fn state(
    pool: &PgPool,
    signer: &TestSigner,
    policy: WebhookPolicy,
) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-release-{}", crypto::random_token()));
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
            // no gtfs_webhook_settings row is written here, so this is also the
            // fallback rule under test: the deployment's values stay in force
            webhook_policy: LivePolicy::new(policy),
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

async fn user(pool: &PgPool, email: &str, role: &str) {
    sqlx::query(
        "INSERT INTO gtfs_editor_user (email, role, status) VALUES ($1, $2, 'active') \
         ON CONFLICT (lower(email)) DO UPDATE SET role = $2, status = 'active'",
    )
    .bind(email)
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    // since 0018 a member holds a feed through a grant on it, with this role
    sqlx::query(
        "INSERT INTO gtfs_editor_feed_access (user_id, gtfs_id, role) \
         SELECT user_id, $2, $3 FROM gtfs_editor_user WHERE lower(email) = lower($1) \
         ON CONFLICT (user_id, gtfs_id) DO UPDATE SET role = $3",
    )
    .bind(email)
    .bind(FEED)
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
}

async fn sql(pool: &PgPool, stmt: &str) {
    sqlx::query(stmt).bind(FEED).execute(pool).await.unwrap();
}

async fn count(pool: &PgPool, stmt: &str) -> i64 {
    sqlx::query(stmt)
        .bind(FEED)
        .fetch_one(pool)
        .await
        .unwrap()
        .get("n")
}

async fn release_deliveries(pool: &PgPool) -> Vec<(String, i64)> {
    sqlx::query(
        "SELECT status, feed_version FROM gtfs_webhook_delivery \
          WHERE gtfs_id = $1 AND kind = 'release' ORDER BY created_at",
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
        )
    })
    .collect()
}

#[actix_web::test]
async fn the_release_button_queues_one_request_and_unlocks_when_it_should() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;
    std::env::set_var(TOKEN_VAR, TOKEN);
    // the audit log is append-only, so this run counts only its own rows
    let started: chrono::DateTime<chrono::Utc> = sqlx::query("SELECT now() AS t")
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("t");

    let (base_url, log, _receiver_status) = start_receiver();
    let policy = WebhookPolicy {
        enabled: true,
        allowed_hosts: vec!["127.0.0.1".to_string()],
    };
    let signer = TestSigner::generate("release-test-key");
    let (st, dir) = state(&pool, &signer, policy.clone());
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    sqlx::query(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, version, data_source, released_version) \
         VALUES ($1, 'Release test feed', 2, 'db', 1)",
    )
    .bind(FEED)
    .execute(&pool)
    .await
    .unwrap();
    user(&pool, APPROVER, "approver").await;
    user(&pool, EDITOR_USER, "editor").await;
    let caller = |email: &str| Caller {
        signer: &signer,
        email: email.into(),
        session: None,
    };
    let (mut admin, mut approver, mut editor_user) =
        (caller(ADMIN), caller(APPROVER), caller(EDITOR_USER));
    sign_in(&app, &mut admin).await;
    sign_in(&app, &mut approver).await;
    sign_in(&app, &mut editor_user).await;
    let release_path = format!("/feeds/{FEED}/release");
    let state_path = format!("/feeds/{FEED}/cache-state");

    // ------------------------------------------ no webhook: says what to add
    let (s, b, _) = call!(&app, approver.req("GET", &state_path));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["release"]["reason"], "no_release_webhook", "{b}");
    assert_eq!(b["release"]["can_release"], false, "{b}");
    let (s, b, _) = call!(&app, approver.req("POST", &release_path));
    assert_eq!((s, code_of(&b)), (409, "no_release_webhook"), "{b}");

    // ------------------------------------------ point it at "Jenkins"
    let (s, hook, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/webhooks"))
            .set_json(json!({
                "name": "nandi release",
                "event": "release_requested",
                "url": format!("{base_url}/build?token=${{{TOKEN_VAR}}}&FEED=${{event:gtfs_id}}"),
                "method": "POST",
            }))
    );
    assert_eq!(s, 201, "{hook}");

    // ------------------------------------------ Test would start a real release
    let (s, b, _) = call!(
        &app,
        admin.req(
            "POST",
            &format!("/webhooks/{}/test", hook["webhook_id"].as_str().unwrap())
        )
    );
    assert_eq!((s, code_of(&b)), (400, "release_webhook_untestable"), "{b}");

    // ------------------------------------------ never fires on its own
    let pod = PodIdentity::from_env(Some("pod-a"));
    let http = reqwest::Client::new();
    sql(
        &pool,
        "UPDATE gtfs_feed SET version = 3, updated_at = now() WHERE gtfs_id = $1",
    )
    .await;
    webhook::heartbeat(&pool, &pod, FEED, 3, "db", None)
        .await
        .unwrap();
    for _ in 0..3 {
        webhook::dispatch_tick(&pool, &http, &pod, &policy).await;
    }
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) AS n FROM gtfs_webhook_delivery WHERE gtfs_id = $1"
        )
        .await,
        0,
        "a release webhook must never be queued by the dispatcher"
    );

    // ------------------------------------------ an editor may not press it
    let (s, b, _) = call!(&app, editor_user.req("POST", &release_path));
    assert_eq!(s, 403, "{b}");

    // ------------------------------------------ the check waits for the feed lock
    // Held from another connection, the lock must stall the request: that is what
    // stops two clicks both passing the in-progress check before either inserts.
    let mut hold = pool.begin().await.unwrap();
    editor::feed_lock::lock_feed(&mut hold, FEED).await.unwrap();
    let done = AtomicBool::new(false);
    let (first, blocked) = tokio::join!(
        async {
            let r =
                test::call_service(&app, approver.req("POST", &release_path).to_request()).await;
            done.store(true, Ordering::SeqCst);
            r.status().as_u16()
        },
        async {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let blocked = !done.load(Ordering::SeqCst);
            hold.commit().await.unwrap();
            blocked
        },
    );
    assert!(blocked, "the release must wait for the feed lock");
    assert_eq!(first, 200);

    // ------------------------------------------ a second click: already on its way
    let (s, b, _) = call!(&app, admin.req("POST", &release_path));
    assert_eq!((s, code_of(&b)), (409, "release_in_progress"), "{b}");
    assert_eq!(
        release_deliveries(&pool).await,
        vec![("pending".to_string(), 3)]
    );

    // ------------------------------------------ it is sent, with the secret
    webhook::dispatch_tick(&pool, &http, &pod, &policy).await;
    assert_eq!(
        release_deliveries(&pool).await,
        vec![("succeeded".to_string(), 3)]
    );
    {
        let hits = &log.lock().unwrap().hits;
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].query.contains(&format!("FEED={FEED}")),
            "{}",
            hits[0].query
        );
        assert!(
            hits[0].query.contains(TOKEN),
            "the pod resolves the credential"
        );
    }
    let (_, b, _) = call!(&app, approver.req("GET", &state_path));
    assert_eq!(b["release"]["reason"], "release_in_progress", "{b}");
    assert_eq!(b["release"]["last_request"]["status"], "succeeded", "{b}");
    assert!(
        b["release"]["last_request"]["requested_by"]
            .as_str()
            .is_some(),
        "{b}"
    );

    // ------------------------------------------ Jenkins marks it released
    sql(
        &pool,
        "UPDATE gtfs_feed SET released_version = 3, released_at = now() WHERE gtfs_id = $1",
    )
    .await;
    let (s, b, _) = call!(&app, approver.req("POST", &release_path));
    assert_eq!((s, code_of(&b)), (409, "nothing_to_release"), "{b}");

    // ------------------------------------------ a failed call never locks it
    sql(
        &pool,
        "UPDATE gtfs_feed SET version = 4, updated_at = now() WHERE gtfs_id = $1",
    )
    .await;
    let (s, b, _) = call!(&app, approver.req("POST", &release_path));
    assert_eq!(s, 200, "{b}");
    sql(
        &pool,
        "UPDATE gtfs_webhook_delivery SET status = 'failed' \
          WHERE gtfs_id = $1 AND kind = 'release' AND feed_version = 4",
    )
    .await;
    let (s, b, _) = call!(&app, approver.req("POST", &release_path));
    assert_eq!(
        s, 200,
        "a failed request must not keep the button locked: {b}"
    );

    // ------------------------------------------ a stuck build unlocks after 3 h
    // the Nandi build itself runs well past half an hour, so a request that old
    // is still a build in progress, not a stuck one
    let (s, b, _) = call!(&app, approver.req("POST", &release_path));
    assert_eq!((s, code_of(&b)), (409, "release_in_progress"), "{b}");
    sql(
        &pool,
        "UPDATE gtfs_webhook_delivery SET created_at = now() - interval '31 minutes' \
          WHERE gtfs_id = $1 AND kind = 'release'",
    )
    .await;
    let (s, b, _) = call!(&app, approver.req("POST", &release_path));
    assert_eq!(
        (s, code_of(&b)),
        (409, "release_in_progress"),
        "a build half an hour in is still running: {b}"
    );
    sql(
        &pool,
        "UPDATE gtfs_webhook_delivery SET created_at = now() - interval '181 minutes' \
          WHERE gtfs_id = $1 AND kind = 'release'",
    )
    .await;
    let (s, b, _) = call!(&app, approver.req("POST", &release_path));
    assert_eq!(
        s, 200,
        "a request older than the window must not block: {b}"
    );

    // ------------------------------------------ audited
    let audited: i64 = sqlx::query(
        "SELECT count(*) AS n FROM gtfs_audit_log \
          WHERE gtfs_id = $1 AND action = 'release_requested' AND at >= $2",
    )
    .bind(FEED)
    .bind(started)
    .fetch_one(&pool)
    .await
    .unwrap()
    .get("n");
    assert_eq!(audited, 4);

    clear(&pool).await;
    let _ = std::fs::remove_dir_all(dir);
}

/// The master editor shares the database with prod, so a press there must say
/// so on the request (whichever pod sends it) and must not be held back by
/// what prod has released - master never records a release.
#[actix_web::test]
async fn a_release_from_the_master_editor_targets_master_nandi() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;
    std::env::set_var(TOKEN_VAR, TOKEN);
    let (base_url, log, _receiver_status) = start_receiver();
    let policy = WebhookPolicy {
        enabled: true,
        allowed_hosts: vec!["127.0.0.1".to_string()],
    };
    let signer = TestSigner::generate("release-master-key");
    let (mut st, dir) = state(&pool, &signer, policy.clone());
    st.is_master = true;
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;
    // prod already has the committed version
    sqlx::query(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, version, data_source, released_version) \
         VALUES ($1, 'Release test feed', 5, 'db', 5)",
    )
    .bind(FEED)
    .execute(&pool)
    .await
    .unwrap();
    user(&pool, APPROVER, "approver").await;
    let mut approver = Caller {
        signer: &signer,
        email: APPROVER.into(),
        session: None,
    };
    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    sign_in(&app, &mut admin).await;
    sign_in(&app, &mut approver).await;
    let (s, hook, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/webhooks"))
            .set_json(json!({
                "name": "nandi release",
                "event": "release_requested",
                "url": format!("{base_url}/build?nandiTarget=${{event:target}}&FEED=${{event:gtfs_id}}"),
            }))
    );
    assert_eq!(s, 201, "{hook}");

    let release_path = format!("/feeds/{FEED}/release");
    let (_, b, _) = call!(
        &app,
        approver.req("GET", &format!("/feeds/{FEED}/cache-state"))
    );
    assert_eq!(b["release"]["target"], "master", "{b}");
    assert_eq!(
        b["release"]["can_release"], true,
        "master is not held back by prod: {b}"
    );
    let (s, b, _) = call!(&app, approver.req("POST", &release_path));
    assert_eq!(s, 200, "{b}");

    let pod = PodIdentity::from_env(Some("pod-m"));
    webhook::dispatch_tick(&pool, &reqwest::Client::new(), &pod, &policy).await;
    {
        let hits = &log.lock().unwrap().hits;
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].query.contains("nandiTarget=master"),
            "the request names master: {}",
            hits[0].query
        );
    }
    // Jenkins took it; master never marks, so the button is free again
    let (s, b, _) = call!(&app, approver.req("POST", &release_path));
    assert_eq!(
        s, 200,
        "a delivered master request does not lock the button: {b}"
    );

    clear(&pool).await;
    let _ = std::fs::remove_dir_all(dir);
}
