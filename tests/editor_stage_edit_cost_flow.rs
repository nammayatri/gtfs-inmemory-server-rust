//! What one stage edit costs when many routes share the stage (docs/gtfs-editor.md
//! section 19), end to end against a real Postgres holding the editor schema.
//!
//! Since chennai_bus's stages were unified, a stage is shared by a hundred
//! routes and more, and every replay of a draft - each read of its page, its
//! submit, approve and commit, all under the feed's lock - rewrites every route
//! a stage edit reaches. This builds a feed whose one stage is shared by
//! [`ROUTES`] routes, each with a stored timing profile, edits that stage, and
//! checks what the edit does to every route and its timings through the whole
//! life of the draft. It prints how long each step took and, with
//! `EDITOR_TEST_STATEMENT_MARKERS=1`, marks each step in the database's log so the
//! statements between two marks can be counted there.
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
use std::time::Instant;

const AUD: &str = "gtfs.editor-stage-cost-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_stage_cost_test_feed";
const ADMIN: &str = "admin@editor-stage-cost-test.invalid";
const APPROVER: &str = "approver@editor-stage-cost-test.invalid";
/// The routes sharing the stage: about what the busiest chennai_bus stages carry.
const ROUTES: usize = 120;

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

fn clear_feed() -> Vec<String> {
    vec![
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_timing_profile WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_pattern WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route_stage WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stage_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stage WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
    ]
}

/// A feed served from its stages: route `R<i>` runs its own head stage, then the
/// stage every route shares, then its own tail stage, and has one stored timing
/// profile over its five served stops.
fn seed() -> Vec<String> {
    let n = ROUTES as i64;
    let list = format!("'{ADMIN}', '{APPROVER}'");
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, use_stages) \
         VALUES ('{FEED}', 'Editor stage cost test feed', true)"
    ));
    // stops S0000 ..: two for the shared stage and the one it gains, two heads'
    // and one tail's for every route
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', 'S' || lpad(i::text, 4, '0'), 'S' || lpad(i::text, 4, '0'), \
                'STOP ' || i, 13.0 + i * 0.0005, 80.2 + (i % 7) * 0.0003 \
         FROM generate_series(0, {last}) i",
        last = 3 + 3 * n
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id, active) \
         SELECT '{FEED}', 'R' || i, 'R' || i, 'Route ' || i, 'AG', true FROM generate_series(0, {}) i",
        n - 1
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, direction, name) VALUES ('{FEED}', 'stg_shared', '', 'SHARED')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, direction, position, stop_id, stop_type) VALUES \
         ('{FEED}', 'stg_shared', '', 1, 'S0000', 'NEW STOP'), ('{FEED}', 'stg_shared', '', 2, 'S0001', 'INTERMEDIATE STOP')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, direction, name) \
         SELECT '{FEED}', k || i, '', upper(k) || ' ' || i FROM generate_series(0, {}) i, \
                (VALUES ('stg_head_'), ('stg_tail_')) v(k)",
        n - 1
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, direction, position, stop_id, stop_type) \
         SELECT '{FEED}', 'stg_head_' || i, '', p, 'S' || lpad((4 + 3 * i + p - 1)::text, 4, '0'), \
                CASE WHEN p = 1 THEN 'NEW STOP' ELSE 'INTERMEDIATE STOP' END \
         FROM generate_series(0, {last}) i, generate_series(1, 2) p",
        last = n - 1
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, direction, position, stop_id, stop_type) \
         SELECT '{FEED}', 'stg_tail_' || i, '', 1, 'S' || lpad((6 + 3 * i)::text, 4, '0'), 'NEW STOP' \
         FROM generate_series(0, {}) i",
        n - 1
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stage (gtfs_id, route_id, position, stage_id, direction, stage_no) \
         SELECT '{FEED}', 'R' || i, p, CASE p WHEN 1 THEN 'stg_head_' || i WHEN 2 THEN 'stg_shared' \
                ELSE 'stg_tail_' || i END, '', p \
         FROM generate_series(0, {}) i, generate_series(1, 3) p",
        n - 1
    ));
    // a route's first pattern comes with it; make sure of it
    s.push(format!(
        "INSERT INTO gtfs_pattern (gtfs_id, route_id, pattern_key) \
         SELECT '{FEED}', 'R' || i, 1 FROM generate_series(0, {}) i ON CONFLICT DO NOTHING",
        n - 1
    ));
    s.push(format!(
        "INSERT INTO gtfs_timing_profile (gtfs_id, route_id, pattern_key, profile_key, arrival_s, departure_s, label, source) \
         SELECT '{FEED}', 'R' || i, 1, 1, ARRAY[0, 60, 120, 180, 240], ARRAY[0, 60, 120, 180, 240], 'imported', 'import' \
         FROM generate_series(0, {}) i",
        n - 1
    ));
    s.push(format!(
        "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, totp_last_step = NULL, \
         status = 'active' WHERE email IN ({list})"
    ));
    s.push(format!(
        "DELETE FROM gtfs_editor_session WHERE user_id IN \
         (SELECT user_id FROM gtfs_editor_user WHERE email IN ({list}))"
    ));
    s
}

fn state(pool: &PgPool, signer: &TestSigner) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-stage-cost-{}", crypto::random_token()));
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

/// What every route is served as: `route -> [stop_id/stop_type/stage_no/stage_name]`.
async fn served(pool: &PgPool) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut out = std::collections::BTreeMap::new();
    for r in sqlx::query(&format!(
        "SELECT route_id, coalesce(stop_id, marker_id) || '/' || stop_type || '/' || stage_no || '/' || stage_name AS r \
         FROM gtfs_route_stop_effective WHERE gtfs_id = '{FEED}' AND pattern_key = 1 ORDER BY route_id, sequence"
    ))
    .fetch_all(pool)
    .await
    .unwrap()
    {
        out.entry(r.get::<String, _>("route_id"))
            .or_insert_with(Vec::new)
            .push(r.get::<String, _>("r"));
    }
    out
}

/// With `EDITOR_TEST_STATEMENT_MARKERS=1`, a statement the database logs, to
/// count the statements of a step between two of them.
async fn mark(pool: &PgPool, what: &str) {
    if std::env::var("EDITOR_TEST_STATEMENT_MARKERS").as_deref() == Ok("1") {
        sqlx::query(&format!("SELECT 'stage-cost-marker: {what}'"))
            .execute(pool)
            .await
            .unwrap();
    }
}

fn now() -> u64 {
    chrono::Utc::now().timestamp() as u64
}

#[actix_web::test]
async fn one_stage_edit_reaching_every_route_through_a_draft() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("stage-cost-test-key");
    let (st, dir) = state(&pool, &signer);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    let mut ap = Caller {
        signer: &signer,
        email: APPROVER.into(),
        session: None,
    };
    macro_rules! sign_in {
        ($who:expr) => {{
            let (s, b, _) = call!(&app, $who.req("POST", "/auth/totp/enroll"));
            assert_eq!(s, 200, "{b}");
            let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
            let (s, b, cookie) = call!(
                &app,
                $who.req("POST", "/auth/totp/confirm")
                    .set_json(json!({"code": crypto::totp_now(&secret, now())}))
            );
            assert_eq!(s, 200, "{b}");
            $who.session = cookie;
        }};
    }
    sign_in!(admin);
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/users")
            .set_json(json!({"email": APPROVER, "role": "approver"}))
    );
    assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    let id = users["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == APPROVER)
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, b, _) = call!(
        &app,
        admin
            .req("PATCH", &format!("/users/{id}"))
            .set_json(json!({"role": "approver", "status": "active"}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        admin
            .req("PUT", &format!("/users/{id}/feeds/{FEED}"))
            .set_json(json!({"role": "approver"}))
    );
    assert_eq!(s, 200, "{b}");
    sign_in!(ap);

    let before = served(&pool).await;
    assert_eq!(before.len(), ROUTES);
    assert!(before.values().all(|rows| rows.len() == 5));

    // ---- the shared stage gains a stop: every route reaches it
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "the shared stage gains a stop"}))
    );
    assert_eq!(s, 201, "{b}");
    let set = b["change_set_id"].as_str().unwrap().to_string();
    let mut timings = Vec::new();
    macro_rules! step {
        ($what:expr, $req:expr) => {{
            mark(&pool, concat!($what, " start")).await;
            let t = Instant::now();
            let (s, b, _) = call!(&app, $req);
            timings.push(($what, t.elapsed()));
            mark(&pool, concat!($what, " end")).await;
            (s, b)
        }};
    }
    let (s, b) = step!(
        "add",
        admin
            .req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(
                json!({"entity": "stage", "op": "update", "entity_key": "stg_shared|", "after": {
                "rows": [{"stop_id": "S0000", "stop_type": "NEW STOP"},
                         {"stop_id": "S0001", "stop_type": "INTERMEDIATE STOP"},
                         {"stop_id": "S0002", "stop_type": "INTERMEDIATE STOP"}]}})
            )
    );
    assert_eq!(s, 201, "{b}");
    let errors: Vec<&Value> = b["validation"]
        .as_array()
        .map(|a| a.iter().filter(|v| v["level"] == "error").collect())
        .unwrap_or_default();
    assert!(errors.is_empty(), "{errors:?}");
    // the edit names every route it changes, and their timings are carried
    let warnings = b["validation"].as_array().cloned().unwrap_or_default();
    assert!(
        warnings.iter().any(|v| v["code"] == "stage_changes_routes"
            && v["message"]
                .as_str()
                .unwrap_or("")
                .contains(&format!("{ROUTES} routes"))),
        "{warnings:?}"
    );
    assert_eq!(
        warnings
            .iter()
            .filter(|v| v["code"] == "timing_interpolated")
            .count(),
        ROUTES,
        "every route's profile is carried over to its new stops"
    );
    let (s, b) = step!("read", admin.req("GET", &format!("/change-sets/{set}")));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["can_submit"], true, "{b}");
    let (s, b) = step!(
        "submit",
        admin.req("POST", &format!("/change-sets/{set}/submit"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b) = step!(
        "approve",
        ap.req("POST", &format!("/change-sets/{set}/approve"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b) = step!(
        "commit",
        ap.req("POST", &format!("/change-sets/{set}/commit"))
    );
    assert_eq!(s, 200, "{b}");

    // ---- every route runs the stage's new stops, and its timings follow
    let after = served(&pool).await;
    for (route, rows) in &after {
        assert_eq!(rows.len(), 6, "{route}: {rows:?}");
        assert_eq!(rows[2], "S0000/NEW STOP/2/SHARED", "{route}: {rows:?}");
        assert_eq!(
            rows[4], "S0002/INTERMEDIATE STOP/2/SHARED",
            "{route}: {rows:?}"
        );
        let mut was = before[route].clone();
        was.insert(4, "S0002/INTERMEDIATE STOP/2/SHARED".into());
        assert_eq!(
            rows, &was,
            "{route}: only the shared stage's new stop is added"
        );
    }
    let profiles = sqlx::query(&format!(
        "SELECT count(*) AS n, count(*) FILTER (WHERE cardinality(arrival_s) = 6 AND source = 'interpolated') AS carried \
         FROM gtfs_timing_profile WHERE gtfs_id = '{FEED}'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(profiles.get::<i64, _>("n"), ROUTES as i64);
    assert_eq!(
        profiles.get::<i64, _>("carried"),
        ROUTES as i64,
        "every profile is carried over to the new stop list"
    );

    for (what, t) in &timings {
        println!(
            "stage edit reaching {ROUTES} routes: {what} {} ms",
            t.as_millis()
        );
    }
    exec(&pool, &clear_feed()).await;
    std::fs::remove_dir_all(dir).ok();
}
