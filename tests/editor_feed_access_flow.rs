//! Feed access (docs/gtfs-editor.md section 15) end to end against a real
//! Postgres holding the editor schema (db/gtfs_editor/0001..0018): two feeds,
//! members at every level on the first and none on the second. Every endpoint
//! group answers 403 `no_feed_access` across feeds and `role_required` within
//! one; a path keyed by an object resolves the object's feed first (feed B's
//! change set opened by id from a feed-A-only session), and so does a change
//! set named in a body or a query; a revocation bites on the next request with
//! the same session, and the revoked member's draft stays for the others; the
//! users API speaks `admin` and `feeds` and still takes the old `{role}` body;
//! a system account cannot sign in; and 0018's backfill, run in a scratch
//! schema, gives chennai_bus grants equal to the old roles - once.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local (see scripts/editor_flow_test.sh). Uses its own feeds and
//! accounts and removes the feeds afterwards; never touches chennai_bus.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, Executor, PgConnection, PgPool, Row};
use std::collections::HashMap;
use std::sync::Arc;

const AUD: &str = "gtfs.editor-access-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED_A: &str = "editor_access_test_a";
const FEED_B: &str = "editor_access_test_b";
const ADMIN: &str = "admin@editor-access-test.invalid";
const VIEWER_A: &str = "viewer-a@editor-access-test.invalid";
const EDITOR_A: &str = "editor-a@editor-access-test.invalid";
const APPROVER_A: &str = "approver-a@editor-access-test.invalid";
const APPROVER_B: &str = "approver-b@editor-access-test.invalid";
const NOBODY: &str = "nobody@editor-access-test.invalid";
const PROMOTED: &str = "promoted@editor-access-test.invalid";
/// A system account (`kind = 'system'`), and a bootstrap admin's email as well:
/// neither makes it an account anyone signs in with.
const SYSTEM: &str = "sync@editor-access-test.invalid";
/// Only ever created here, never signed in with, so removed before each run.
const NEW_MEMBER: &str = "new-member@editor-access-test.invalid";
const NEW_LEGACY: &str = "new-legacy@editor-access-test.invalid";
const NEW_ADMIN: &str = "new-admin@editor-access-test.invalid";
/// The 0018 migration itself, for the backfill test.
const MIGRATION: &str = include_str!("../db/gtfs_editor/0018_feed_access.sql");

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

    /// `req` with a JSON body when there is one.
    fn send(&self, method: &str, path: &str, body: &Option<Value>) -> test::TestRequest {
        match body {
            Some(b) => self.req(method, path).set_json(b),
            None => self.req(method, path),
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

fn database_url() -> Option<String> {
    let Ok(url) = std::env::var("EDITOR_TEST_DATABASE_URL") else {
        eprintln!("EDITOR_TEST_DATABASE_URL not set; skipping");
        return None;
    };
    assert!(
        url.contains("@127.0.0.1") || url.contains("@localhost"),
        "this test only runs against a local database"
    );
    Some(url)
}

async fn local_pool() -> Option<PgPool> {
    let url = database_url()?;
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

async fn scalar_i64(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<i64, _>(0)
}

fn clear_feed(g: &str) -> Vec<String> {
    vec![
        format!("DELETE FROM gtfs_webhook_delivery WHERE gtfs_id = '{g}'"),
        format!("DELETE FROM gtfs_webhook WHERE gtfs_id = '{g}'"),
        format!("DELETE FROM gtfs_position_review WHERE gtfs_id = '{g}'"),
        format!("DELETE FROM gtfs_station_proposal WHERE gtfs_id = '{g}'"),
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{g}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{g}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{g}'"),
        format!(
            "UPDATE gtfs_stop SET parent_station = NULL \
             WHERE gtfs_id = '{g}' AND parent_station IS NOT NULL"
        ),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{g}'"),
        // gtfs_audit_log is append-only and is never cleared; the assertions
        // below read it by this run's own users and feeds
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{g}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{g}'"),
    ]
}

/// One small feed: four stops, a route over three of them, a suggested station,
/// a coordinate review and a webhook - one object of every kind a path can be
/// keyed by.
fn seed_feed(g: &str, lat: f64) -> Vec<String> {
    let members = json!([
        {"stop_id": "S1", "name": "x", "lat": lat + 0.001, "lon": 80.2, "platform_code": "A", "route_count": 1},
        {"stop_id": "S2", "name": "x", "lat": lat + 0.002, "lon": 80.2, "platform_code": "B", "route_count": 1},
    ]);
    vec![
        format!("INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{g}', 'Feed access test {g}')"),
        format!(
            "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
             SELECT '{g}', 'S' || i, 'S' || i, 'STOP ' || i, {lat} + i * 0.001, 80.2 \
             FROM generate_series(1, 4) i"
        ),
        format!(
            "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name) \
             VALUES ('{g}', 'R1', 'T1', 'STOP 1 To STOP 3')"
        ),
        format!(
            "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name) VALUES \
             ('{g}', 'R1', 1, 'S1', 'NEW STOP', 1, 'STOP 1'), \
             ('{g}', 'R1', 2, 'S2', 'INTERMEDIATE STOP', 1, 'STOP 1'), \
             ('{g}', 'R1', 3, 'S3', 'NEW STOP', 2, 'STOP 3')"
        ),
        format!(
            "INSERT INTO gtfs_station_proposal (gtfs_id, batch, station_id, name, lat, lon, members, spread_m) \
             VALUES ('{g}', 'access-test', 'STN1', 'PLACE', {lat}, 80.2, '{members}'::jsonb, 20)"
        ),
        format!(
            "INSERT INTO gtfs_position_review (gtfs_id, batch, stop_id, original_stop_id, stop_name, reason, lat, lon) \
             VALUES ('{g}', 'access-test', 'S4', 'S4', 'STOP 4', 'off its routes', {lat} + 0.004, 80.2)"
        ),
        format!(
            "INSERT INTO gtfs_webhook (gtfs_id, name, event, url) \
             VALUES ('{g}', 'rebuild', 'feed_in_sync', 'https://build.invalid/{g}')"
        ),
    ]
}

fn people() -> String {
    [
        ADMIN, VIEWER_A, EDITOR_A, APPROVER_A, APPROVER_B, NOBODY, PROMOTED, SYSTEM,
    ]
    .iter()
    .map(|e| format!("'{e}'"))
    .collect::<Vec<_>>()
    .join(", ")
}

/// Accounts from an earlier run start over: no authenticator, no session, no
/// grant, active, and the old role column a member is created with.
fn reset_accounts() -> Vec<String> {
    let people = people();
    vec![
        format!("DELETE FROM gtfs_editor_user WHERE email IN ('{NEW_MEMBER}', '{NEW_LEGACY}', '{NEW_ADMIN}')"),
        format!(
            "DELETE FROM gtfs_editor_feed_access WHERE user_id IN \
             (SELECT user_id FROM gtfs_editor_user WHERE email IN ({people}))"
        ),
        format!(
            "DELETE FROM gtfs_editor_session WHERE user_id IN \
             (SELECT user_id FROM gtfs_editor_user WHERE email IN ({people}))"
        ),
        format!(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, totp_last_step = NULL, \
             status = 'active', role = CASE WHEN email = '{ADMIN}' THEN 'admin' ELSE 'viewer' END \
             WHERE email IN ({people})"
        ),
        // only a migration makes a system account; this one stands in for it
        format!(
            "INSERT INTO gtfs_editor_user (email, display_name, role, kind) \
             VALUES ('{SYSTEM}', 'Sync (test)', 'editor', 'system') \
             ON CONFLICT (lower(email)) DO UPDATE SET role = 'editor', kind = 'system'"
        ),
    ]
}

fn state(pool: &PgPool, signer: &TestSigner, admins: &[&str]) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-feed-access-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    use base64::Engine;
    let st = EditorState::build(
        pool.clone(),
        EditorSettings {
            jwks_url: format!("file://{}", jwks.display()),
            audience: AUD.into(),
            bootstrap_admins: admins.iter().map(|a| a.to_string()).collect(),
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
    assert_eq!(s, 200, "{} {b}", c.email);
    let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let (s, b, cookie) = call!(
        app,
        c.req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now())}))
    );
    assert_eq!(s, 200, "{} {b}", c.email);
    c.session = cookie;
}

fn ids_of(users: &Value) -> HashMap<String, String> {
    users["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (
                u["email"].as_str().unwrap().to_string(),
                u["user_id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn feed_ids(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|f| f["gtfs_id"].as_str().unwrap().to_string())
        .filter(|g| g.starts_with("editor_access_test_"))
        .collect()
}

/// A request a table of expectations sends: method, path, body.
type Call = (&'static str, String, Option<Value>);

#[actix_web::test]
async fn members_work_only_on_the_feeds_they_are_granted() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let mut setup = clear_feed(FEED_A);
    setup.extend(clear_feed(FEED_B));
    setup.extend(seed_feed(FEED_A, 13.0));
    setup.extend(seed_feed(FEED_B, 14.0));
    setup.extend(reset_accounts());
    exec(&pool, &setup).await;
    // gtfs_audit_log is append-only: every count below is of this run's rows
    let since = scalar_i64(
        &pool,
        "SELECT coalesce(max(audit_id), 0) FROM gtfs_audit_log",
    )
    .await;

    let signer = TestSigner::generate("feed-access-test-key");
    let (st, dir) = state(&pool, &signer, &[ADMIN, SYSTEM]);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;
    let caller = |email: &'static str| Caller {
        signer: &signer,
        email,
        session: None,
    };

    let mut admin = caller(ADMIN);
    sign_in(&app, &mut admin).await;
    let (s, me, _) = call!(&app, admin.req("GET", "/auth/me"));
    assert_eq!(s, 200, "{me}");
    assert_eq!(me["is_admin"], true, "{me}");
    assert_eq!(feed_ids(&me["feeds"]), vec![FEED_A, FEED_B], "{me}");
    assert!(
        me["feeds"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["role"] == "admin"),
        "an admin has every feed as admin: {me}"
    );

    // ---- members, and their grants through the API
    for email in [VIEWER_A, EDITOR_A, APPROVER_A, APPROVER_B, NOBODY] {
        let (s, b, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "admin": false}))
        );
        assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    }
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/users")
            .set_json(json!({"email": PROMOTED, "role": "viewer"}))
    );
    assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    let (s, users, _) = call!(&app, admin.req("GET", "/users"));
    assert_eq!(s, 200, "{users}");
    let ids = ids_of(&users);
    let grants = [
        (VIEWER_A, FEED_A, "viewer"),
        (EDITOR_A, FEED_A, "editor"),
        (APPROVER_A, FEED_A, "approver"),
        (APPROVER_B, FEED_B, "approver"),
    ];
    for (email, g, role) in grants {
        let (s, b, _) = call!(
            &app,
            admin
                .req("PUT", &format!("/users/{}/feeds/{g}", ids[email]))
                .set_json(json!({"role": role}))
        );
        assert_eq!(s, 200, "{b}");
        assert_eq!(b["email"], email, "{b}");
        assert_eq!(b["is_admin"], false, "{b}");
        assert_eq!(b["kind"], "person", "{b}");
        let feeds = b["feeds"].as_array().unwrap();
        assert_eq!(feeds.len(), 1, "{b}");
        assert_eq!(
            (
                &feeds[0]["gtfs_id"],
                &feeds[0]["role"],
                &feeds[0]["granted_by_email"]
            ),
            (&json!(g), &json!(role), &json!(ADMIN)),
            "{b}"
        );
        assert!(feeds[0]["granted_at"].is_string(), "{b}");
    }
    // the rows carry the feed, so the feed's own history shows who was let in
    let granted = scalar_i64(
        &pool,
        &format!(
            "SELECT count(*) FROM gtfs_audit_log WHERE audit_id > {since} \
             AND action = 'feed_access_granted' AND gtfs_id = '{FEED_A}' \
             AND detail->>'role_before' IS NULL AND detail->>'user_id' IN ('{}', '{}', '{}')",
            ids[VIEWER_A], ids[EDITOR_A], ids[APPROVER_A]
        ),
    )
    .await;
    assert_eq!(granted, 3, "feed_access_granted rows on {FEED_A}");

    let mut viewer_a = caller(VIEWER_A);
    let mut editor_a = caller(EDITOR_A);
    let mut approver_a = caller(APPROVER_A);
    let mut approver_b = caller(APPROVER_B);
    let mut nobody = caller(NOBODY);
    let mut promoted = caller(PROMOTED);
    for c in [
        &mut viewer_a,
        &mut editor_a,
        &mut approver_a,
        &mut approver_b,
        &mut nobody,
        &mut promoted,
    ] {
        sign_in(&app, c).await;
    }

    // ---- what each sees: /auth/me and /feeds
    let (s, me, _) = call!(&app, viewer_a.req("GET", "/auth/me"));
    assert_eq!(s, 200, "{me}");
    assert_eq!(me["is_admin"], false, "{me}");
    assert_eq!(
        me["feeds"],
        json!([{"gtfs_id": FEED_A, "display_name": format!("Feed access test {FEED_A}"), "role": "viewer"}]),
        "{me}"
    );
    let (s, b, _) = call!(&app, approver_a.req("GET", "/feeds"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["items"].as_array().unwrap().len(), 1, "{b}");
    assert_eq!(
        (&b["items"][0]["gtfs_id"], &b["items"][0]["my_role"]),
        (&json!(FEED_A), &json!("approver")),
        "{b}"
    );
    let (_, b, _) = call!(&app, admin.req("GET", "/feeds"));
    assert_eq!(feed_ids(&b["items"]), vec![FEED_A, FEED_B], "{b}");
    assert!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["my_role"] == "admin"),
        "{b}"
    );
    // a member with no grant has no feeds at all, and sees nothing
    let (_, me, _) = call!(&app, nobody.req("GET", "/auth/me"));
    assert_eq!(
        (&me["session"], &me["feeds"]),
        (&json!(true), &json!([])),
        "{me}"
    );
    let (s, b, _) = call!(&app, nobody.req("GET", "/feeds"));
    assert_eq!((s, &b["items"]), (200, &json!([])), "{b}");
    let (s, b, _) = call!(&app, nobody.req("GET", &format!("/feeds/{FEED_A}/stops")));
    assert_eq!((s, code_of(&b)), (403, "no_feed_access"), "{b}");
    // anyone signed in reads the webhook policy: it is the deployment's
    let (s, b, _) = call!(&app, nobody.req("GET", "/webhook-settings"));
    assert_eq!(s, 200, "{b}");

    // ---- one object of each kind on each feed
    let add_change = |g: &str| json!({"entity": "stop", "op": "update", "entity_key": "S1", "after": {"name": format!("STOP ONE {g}")}});
    let (s, b, _) = call!(
        &app,
        approver_b
            .req("POST", &format!("/feeds/{FEED_B}/change-sets"))
            .set_json(json!({"title": "B's draft"}))
    );
    assert_eq!(s, 201, "{b}");
    let set_b = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        approver_b
            .req("POST", &format!("/change-sets/{set_b}/changes"))
            .set_json(add_change(FEED_B))
    );
    assert_eq!(s, 201, "{b}");
    let change_b = b["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        editor_a
            .req("POST", &format!("/feeds/{FEED_A}/change-sets"))
            .set_json(json!({"title": "A's draft"}))
    );
    assert_eq!(s, 201, "{b}");
    let set_a = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        editor_a
            .req("POST", &format!("/change-sets/{set_a}/changes"))
            .set_json(add_change(FEED_A))
    );
    assert_eq!(s, 201, "{b}");
    let change_a = b["change_id"].as_i64().unwrap();
    let object = |table: &str, id: &str, g: &str| {
        format!("SELECT {id}::text FROM {table} WHERE gtfs_id = '{g}'")
    };
    let mut objects: HashMap<(&str, &str), String> = HashMap::new();
    for g in [FEED_A, FEED_B] {
        for (kind, table, id) in [
            ("proposal", "gtfs_station_proposal", "proposal_id"),
            ("review", "gtfs_position_review", "review_id"),
            ("webhook", "gtfs_webhook", "webhook_id"),
        ] {
            let v: String = sqlx::query(&object(table, id, g))
                .fetch_one(&pool)
                .await
                .unwrap()
                .get(0);
            objects.insert((kind, g), v);
        }
    }
    let (prop_a, rev_a, hook_a) = (
        &objects[&("proposal", FEED_A)],
        &objects[&("review", FEED_A)],
        &objects[&("webhook", FEED_A)],
    );
    let (prop_b, rev_b, hook_b) = (
        &objects[&("proposal", FEED_B)],
        &objects[&("review", FEED_B)],
        &objects[&("webhook", FEED_B)],
    );

    // ---- every endpoint group on a feed, by what it needs
    let reads = |g: &str, set: &str, prop: &str, rev: &str| -> Vec<Call> {
        vec![
            ("GET", format!("/feeds/{g}/config"), None),
            ("GET", format!("/feeds/{g}/stops"), None),
            ("GET", format!("/feeds/{g}/stops/S1"), None),
            ("GET", format!("/feeds/{g}/stops/S1/context"), None),
            ("GET", format!("/feeds/{g}/routes"), None),
            ("GET", format!("/feeds/{g}/routes/R1"), None),
            ("GET", format!("/feeds/{g}/routes/R1/context"), None),
            ("GET", format!("/feeds/{g}/audit"), None),
            ("GET", format!("/feeds/{g}/change-sets"), None),
            ("GET", format!("/feeds/{g}/station-proposals"), None),
            ("GET", format!("/feeds/{g}/station-proposals/summary"), None),
            ("GET", format!("/feeds/{g}/position-reviews"), None),
            ("GET", format!("/feeds/{g}/position-reviews/summary"), None),
            ("GET", format!("/feeds/{g}/cache-state"), None),
            ("GET", format!("/feeds/{g}/webhooks"), None),
            ("GET", format!("/feeds/{g}/webhook-deliveries"), None),
            ("GET", format!("/change-sets/{set}"), None),
            ("GET", format!("/change-sets/{set}/preview/routes/R1"), None),
            ("GET", format!("/station-proposals/{prop}"), None),
            ("GET", format!("/position-reviews/{rev}"), None),
        ]
    };
    let edits = |g: &str, set: &str, change: i64, prop: &str, rev: &str| -> Vec<Call> {
        let at = json!({"change_set_id": set, "lat": 13.5, "lon": 80.2});
        vec![
            (
                "POST",
                format!("/feeds/{g}/change-sets"),
                Some(json!({"title": "x"})),
            ),
            ("POST", format!("/feeds/{g}/routes/R1/polyline:osrm"), None),
            (
                "POST",
                format!("/feeds/{g}/station-proposals/approve"),
                Some(json!({"change_set_id": set, "proposal_ids": [prop.parse::<i64>().unwrap()]})),
            ),
            (
                "POST",
                format!("/change-sets/{set}/changes"),
                Some(add_change(g)),
            ),
            (
                "PUT",
                format!("/change-sets/{set}/changes/{change}"),
                Some(json!({"after": {"name": "Y"}})),
            ),
            (
                "DELETE",
                format!("/change-sets/{set}/changes/{change}"),
                None,
            ),
            ("POST", format!("/change-sets/{set}/submit"), None),
            ("POST", format!("/change-sets/{set}/reopen"), None),
            ("POST", format!("/change-sets/{set}/discard"), None),
            (
                "POST",
                format!("/change-sets/{set}/bulk"),
                Some(json!({"kind": "stops", "rows": [], "dry_run": true})),
            ),
            (
                "POST",
                format!("/station-proposals/{prop}/approve"),
                Some(json!({"change_set_id": set})),
            ),
            (
                "POST",
                format!("/station-proposals/{prop}/reject"),
                Some(json!({})),
            ),
            ("POST", format!("/station-proposals/{prop}/reopen"), None),
            (
                "POST",
                format!("/position-reviews/{rev}/move"),
                Some(at.clone()),
            ),
            (
                "POST",
                format!("/position-reviews/{rev}/split"),
                Some(json!({"change_set_id": set, "route_ids": ["R1"], "lat": 13.5, "lon": 80.2})),
            ),
            (
                "POST",
                format!("/position-reviews/{rev}/merge"),
                Some(json!({"change_set_id": set, "into_stop_id": "S1"})),
            ),
            (
                "POST",
                format!("/position-reviews/{rev}/confirm"),
                Some(json!({})),
            ),
            ("POST", format!("/position-reviews/{rev}/reopen"), None),
        ]
    };
    let reviews = |set: &str| -> Vec<Call> {
        vec![
            (
                "POST",
                format!("/change-sets/{set}/approve"),
                Some(json!({})),
            ),
            (
                "POST",
                format!("/change-sets/{set}/reject"),
                Some(json!({"comment": "no"})),
            ),
            ("POST", format!("/change-sets/{set}/commit"), None),
        ]
    };
    let admin_only = |g: &str, hook: &str, set: &str| -> Vec<Call> {
        vec![
            (
                "POST",
                format!("/feeds/{g}/webhooks"),
                Some(json!({"name": "n", "url": "https://build.invalid/x"})),
            ),
            (
                "PATCH",
                format!("/webhooks/{hook}"),
                Some(json!({"enabled": false})),
            ),
            ("DELETE", format!("/webhooks/{hook}"), None),
            ("POST", format!("/webhooks/{hook}/test"), None),
            // which data source a feed is served from is an admin's call
            (
                "POST",
                format!("/change-sets/{set}/changes"),
                Some(
                    json!({"entity": "feed_config", "op": "update", "entity_key": g,
                            "after": {"data_source": "db"}}),
                ),
            ),
        ]
    };

    // ---- across feeds: the most a member holds on A is nothing on B, and an
    // object of B named by id is B's - feed B's change set opened by id from a
    // feed-A-only session included
    let mut across = reads(FEED_B, &set_b, prop_b, rev_b);
    across.extend(edits(FEED_B, &set_b, change_b, prop_b, rev_b));
    across.extend(reviews(&set_b));
    across.extend(admin_only(FEED_B, hook_b, &set_b));
    for (method, path, body) in &across {
        let (s, b, _) = call!(&app, approver_a.send(method, path, body));
        assert_eq!(
            (s, code_of(&b), &b["error"]["details"]["gtfs_id"]),
            (403, "no_feed_access", &json!(FEED_B)),
            "{method} {path}: {b}"
        );
    }
    // and the other way round, and for a member with no feed at all
    for (method, path, body) in reads(FEED_A, &set_a, prop_a, rev_a) {
        for c in [&approver_b, &nobody] {
            let (s, b, _) = call!(&app, c.send(method, &path, &body));
            assert_eq!(
                (s, code_of(&b), &b["error"]["details"]["gtfs_id"]),
                (403, "no_feed_access", &json!(FEED_A)),
                "{} {method} {path}: {b}",
                c.email
            );
        }
    }
    // a change set named in a body or a query is resolved too: it answers for
    // its own feed, never with a feed_mismatch that names it
    let named: Vec<Call> = vec![
        (
            "POST",
            format!("/position-reviews/{rev_a}/move"),
            Some(json!({"change_set_id": set_b, "lat": 13.5, "lon": 80.2})),
        ),
        (
            "POST",
            format!("/position-reviews/{rev_a}/merge"),
            Some(json!({"change_set_id": set_b, "into_stop_id": "S1"})),
        ),
        (
            "POST",
            format!("/station-proposals/{prop_a}/approve"),
            Some(json!({"change_set_id": set_b})),
        ),
        (
            "POST",
            format!("/feeds/{FEED_A}/station-proposals/approve"),
            Some(json!({"change_set_id": set_b, "proposal_ids": [prop_a.parse::<i64>().unwrap()]})),
        ),
        (
            "POST",
            format!("/feeds/{FEED_A}/routes/R1/polyline:osrm?change_set={set_b}"),
            None,
        ),
        (
            "GET",
            format!("/position-reviews/{rev_a}?stop_id=S1&change_set={set_b}"),
            None,
        ),
    ];
    for (method, path, body) in &named {
        let (s, b, _) = call!(&app, editor_a.send(method, path, body));
        assert_eq!(
            (s, code_of(&b), &b["error"]["details"]["gtfs_id"]),
            (403, "no_feed_access", &json!(FEED_B)),
            "{method} {path}: {b}"
        );
    }

    // ---- within a feed: the role each endpoint needs has not changed
    for (c, calls) in [
        (&viewer_a, edits(FEED_A, &set_a, change_a, prop_a, rev_a)),
        (&editor_a, reviews(&set_a)),
        (&approver_a, admin_only(FEED_A, hook_a, &set_a)),
    ] {
        for (method, path, body) in calls {
            let (s, b, _) = call!(&app, c.send(method, &path, &body));
            assert_eq!(
                (s, code_of(&b)),
                (403, "role_required"),
                "{} {method} {path}: {b}",
                c.email
            );
        }
    }
    for (method, path, body) in reads(FEED_A, &set_a, prop_a, rev_a) {
        let (s, b, _) = call!(&app, viewer_a.send(method, &path, &body));
        assert_eq!(s, 200, "{method} {path}: {b}");
    }
    // users, grants and the webhook policy are an admin's, whatever the feed
    let admin_calls: Vec<Call> = vec![
        ("GET", "/users".into(), None),
        ("POST", "/users".into(), Some(json!({"email": NEW_MEMBER}))),
        (
            "PATCH",
            format!("/users/{}", ids[VIEWER_A]),
            Some(json!({"admin": true})),
        ),
        ("POST", format!("/users/{}/reset-totp", ids[VIEWER_A]), None),
        (
            "PUT",
            format!("/users/{}/feeds/{FEED_A}", ids[VIEWER_A]),
            Some(json!({"role": "approver"})),
        ),
        (
            "DELETE",
            format!("/users/{}/feeds/{FEED_A}", ids[VIEWER_A]),
            None,
        ),
        (
            "PUT",
            "/webhook-settings".into(),
            Some(json!({"enabled": true, "allowed_hosts": ["127.0.0.1"]})),
        ),
    ];
    for (method, path, body) in &admin_calls {
        let (s, b, _) = call!(&app, approver_a.send(method, path, body));
        assert_eq!(
            (s, code_of(&b)),
            (403, "role_required"),
            "{method} {path}: {b}"
        );
    }
    // an admin holds no grant and needs none: B's webhook, from nowhere
    let (s, b, _) = call!(
        &app,
        admin
            .req("PATCH", &format!("/webhooks/{hook_b}"))
            .set_json(json!({"enabled": false}))
    );
    assert_eq!((s, &b["enabled"]), (200, &json!(false)), "{b}");
    // 404 stays for what does not exist in a feed they can see
    let (s, b, _) = call!(
        &app,
        viewer_a.req("GET", &format!("/feeds/{FEED_A}/stops/NO_SUCH_STOP"))
    );
    assert_eq!((s, code_of(&b)), (404, "stop_not_found"), "{b}");
    let nowhere = uuid::Uuid::new_v4();
    let (s, b, _) = call!(
        &app,
        viewer_a.req("GET", &format!("/change-sets/{nowhere}"))
    );
    assert_eq!((s, code_of(&b)), (404, "change_set_not_found"), "{b}");
    let (s, b, _) = call!(&app, viewer_a.req("GET", "/position-reviews/0"));
    assert_eq!((s, code_of(&b)), (404, "review_not_found"), "{b}");

    // ---- maker-checker stays per set
    let (s, b, _) = call!(
        &app,
        editor_a.req("POST", &format!("/change-sets/{set_a}/submit"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        approver_a
            .req("POST", &format!("/change-sets/{set_a}/approve"))
            .set_json(json!({}))
    );
    assert_eq!((s, &b["status"]), (200, &json!("approved")), "{b}");
    let (s, b, _) = call!(
        &app,
        approver_a.req("POST", &format!("/change-sets/{set_a}/commit"))
    );
    assert_eq!((s, &b["status"]), (200, &json!("committed")), "{b}");
    let (s, b, _) = call!(
        &app,
        approver_a
            .req("POST", &format!("/feeds/{FEED_A}/change-sets"))
            .set_json(json!({"title": "the approver's own"}))
    );
    assert_eq!(s, 201, "{b}");
    let own = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        approver_a
            .req("POST", &format!("/change-sets/{own}/changes"))
            .set_json(json!({"entity": "stop", "op": "update", "entity_key": "S2", "after": {"name": "OWN"}}))
    );
    assert_eq!(s, 201, "{b}");
    let (s, b, _) = call!(
        &app,
        approver_a.req("POST", &format!("/change-sets/{own}/submit"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        approver_a
            .req("POST", &format!("/change-sets/{own}/approve"))
            .set_json(json!({"self_approve": true}))
    );
    assert_eq!((s, code_of(&b)), (403, "own_change_set"), "{b}");
    assert_eq!(b["error"]["details"]["can_self_approve"], false, "{b}");

    // ---- a revocation bites on the next request, with the same session
    let (s, b, _) = call!(
        &app,
        editor_a
            .req("POST", &format!("/feeds/{FEED_A}/change-sets"))
            .set_json(json!({"title": "left open"}))
    );
    assert_eq!(s, 201, "{b}");
    let open = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        editor_a
            .req("POST", &format!("/change-sets/{open}/changes"))
            .set_json(json!({"entity": "stop", "op": "update", "entity_key": "S3", "after": {"name": "OPEN"}}))
    );
    assert_eq!(s, 201, "{b}");
    let (s, _, _) = call!(&app, editor_a.req("GET", &format!("/feeds/{FEED_A}/stops")));
    assert_eq!(s, 200);
    let (s, b, _) = call!(
        &app,
        admin.req(
            "DELETE",
            &format!("/users/{}/feeds/{FEED_A}", ids[EDITOR_A])
        )
    );
    assert_eq!(s, 204, "{b}");
    let (s, b, _) = call!(&app, editor_a.req("GET", &format!("/feeds/{FEED_A}/stops")));
    assert_eq!((s, code_of(&b)), (403, "no_feed_access"), "{b}");
    let (s, b, _) = call!(&app, editor_a.req("GET", &format!("/change-sets/{open}")));
    assert_eq!((s, code_of(&b)), (403, "no_feed_access"), "{b}");
    // the session is not ended: the feed is simply gone from what they hold
    let (s, me, _) = call!(&app, editor_a.req("GET", "/auth/me"));
    assert_eq!(
        (s, &me["session"], &me["feeds"]),
        (200, &json!(true), &json!([])),
        "{me}"
    );
    let (_, b, _) = call!(&app, editor_a.req("GET", "/feeds"));
    assert_eq!(b["items"], json!([]), "{b}");
    let revoked = scalar_i64(
        &pool,
        &format!(
            "SELECT count(*) FROM gtfs_audit_log WHERE audit_id > {since} \
             AND action = 'feed_access_revoked' AND gtfs_id = '{FEED_A}' AND detail->>'user_id' = '{}' \
             AND detail->>'role_before' = 'editor' AND detail->'role_after' = 'null'::jsonb \
             AND detail->>'email' = '{EDITOR_A}'",
            ids[EDITOR_A]
        ),
    )
    .await;
    assert_eq!(revoked, 1);
    // the feed's own history shows it, to those who may read the feed
    let (s, b, _) = call!(
        &app,
        approver_a.req("GET", &format!("/feeds/{FEED_A}/audit"))
    );
    assert_eq!(s, 200, "{b}");
    assert!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["action"] == "feed_access_revoked" && a["detail"]["email"] == EDITOR_A),
        "{b}"
    );
    // the revoked member's draft stays: the others see it, edit it, submit it
    let (s, b, _) = call!(&app, approver_a.req("GET", &format!("/change-sets/{open}")));
    assert_eq!((s, &b["status"]), (200, &json!("draft")), "{b}");
    let (s, b, _) = call!(
        &app,
        approver_a
            .req("POST", &format!("/change-sets/{open}/changes"))
            .set_json(json!({"entity": "stop", "op": "update", "entity_key": "S4", "after": {"name": "FINISHED"}}))
    );
    assert_eq!(s, 201, "{b}");
    let (s, b, _) = call!(
        &app,
        approver_a.req("POST", &format!("/change-sets/{open}/submit"))
    );
    assert_eq!((s, &b["status"]), (200, &json!("submitted")), "{b}");
    // no grant left to take away
    let (s, b, _) = call!(
        &app,
        admin.req(
            "DELETE",
            &format!("/users/{}/feeds/{FEED_A}", ids[EDITOR_A])
        )
    );
    assert_eq!((s, code_of(&b)), (404, "grant_not_found"), "{b}");

    // ---- a changed grant bites the same way
    let grant = |email: &str, role: &str| {
        admin
            .req("PUT", &format!("/users/{}/feeds/{FEED_A}", ids[email]))
            .set_json(json!({"role": role}))
    };
    let (s, b, _) = call!(&app, grant(EDITOR_A, "viewer"));
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        editor_a
            .req("POST", &format!("/feeds/{FEED_A}/change-sets"))
            .set_json(json!({"title": "as a viewer"}))
    );
    assert_eq!((s, code_of(&b)), (403, "role_required"), "{b}");
    let (s, b, _) = call!(&app, grant(EDITOR_A, "editor"));
    assert_eq!((s, &b["feeds"][0]["role"]), (200, &json!("editor")), "{b}");
    let (s, b, _) = call!(
        &app,
        editor_a
            .req("POST", &format!("/feeds/{FEED_A}/change-sets"))
            .set_json(json!({"title": "an editor again"}))
    );
    assert_eq!(s, 201, "{b}");
    let access_rows = format!(
        "SELECT count(*) FROM gtfs_audit_log WHERE audit_id > {since} \
         AND action LIKE 'feed_access_%' AND detail->>'user_id' = '{}'",
        ids[EDITOR_A]
    );
    let rows_before = scalar_i64(&pool, &access_rows).await;
    let changed = scalar_i64(
        &pool,
        &format!(
            "SELECT count(*) FROM gtfs_audit_log WHERE audit_id > {since} \
             AND action = 'feed_access_changed' AND gtfs_id = '{FEED_A}' AND detail->>'user_id' = '{}' \
             AND detail->>'role_before' = 'viewer' AND detail->>'role_after' = 'editor'",
            ids[EDITOR_A]
        ),
    )
    .await;
    assert_eq!(changed, 1);
    // the role it already has: nothing changes, nothing is written
    let (s, b, _) = call!(&app, grant(EDITOR_A, "editor"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(scalar_i64(&pool, &access_rows).await, rows_before);

    // ---- what a grant cannot be
    for (who, g, role, status, code) in [
        (EDITOR_A, FEED_A, "admin", 400, "invalid_role"),
        (EDITOR_A, FEED_A, "owner", 400, "invalid_role"),
        (ADMIN, FEED_A, "viewer", 400, "admin_has_all_feeds"),
        (
            EDITOR_A,
            "editor_access_test_none",
            "viewer",
            404,
            "feed_not_found",
        ),
    ] {
        let (s, b, _) = call!(
            &app,
            admin
                .req("PUT", &format!("/users/{}/feeds/{g}", ids[who]))
                .set_json(json!({"role": role}))
        );
        assert_eq!((s, code_of(&b)), (status, code), "{who} {g} {role}: {b}");
    }
    let (s, b, _) = call!(
        &app,
        admin
            .req("PUT", &format!("/users/{nowhere}/feeds/{FEED_A}"))
            .set_json(json!({"role": "viewer"}))
    );
    assert_eq!((s, code_of(&b)), (404, "user_not_found"), "{b}");
    let (s, b, _) = call!(
        &app,
        admin.req("DELETE", &format!("/users/{}/feeds/{FEED_A}", ids[NOBODY]))
    );
    assert_eq!((s, code_of(&b)), (404, "grant_not_found"), "{b}");

    // ---- the users API: admin and feeds, and the old {role} body
    let (s, b, _) = call!(
        &app,
        admin.req("POST", "/users").set_json(json!({
            "email": NEW_MEMBER, "display_name": "New member",
            "feeds": [{"gtfs_id": FEED_A, "role": "editor"}, {"gtfs_id": FEED_B, "role": "viewer"}],
        }))
    );
    assert_eq!(s, 201, "{b}");
    assert_eq!(
        (&b["is_admin"], &b["kind"], &b["role"]),
        (&json!(false), &json!("person"), &json!("viewer")),
        "{b}"
    );
    let roles: Vec<(String, String)> = b["feeds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["gtfs_id"].as_str().unwrap().into(),
                f["role"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        roles,
        vec![
            (FEED_A.to_string(), "editor".to_string()),
            (FEED_B.to_string(), "viewer".to_string())
        ]
    );
    for (body, status, code) in [
        (
            json!({"email": NEW_ADMIN, "admin": true, "feeds": [{"gtfs_id": FEED_A, "role": "viewer"}]}),
            400,
            "admin_has_all_feeds",
        ),
        (
            json!({"email": NEW_ADMIN, "feeds": [{"gtfs_id": FEED_A, "role": "admin"}]}),
            400,
            "invalid_role",
        ),
        (
            json!({"email": NEW_ADMIN, "feeds": [{"gtfs_id": FEED_A, "role": "viewer"}, {"gtfs_id": FEED_A, "role": "editor"}]}),
            400,
            "duplicate_feed",
        ),
        (
            json!({"email": NEW_ADMIN, "feeds": [{"gtfs_id": "editor_access_test_none", "role": "viewer"}]}),
            404,
            "feed_not_found",
        ),
        (
            json!({"email": NEW_ADMIN, "admin": false, "role": "admin"}),
            400,
            "invalid_role",
        ),
        (
            json!({"email": NEW_ADMIN, "role": "owner"}),
            400,
            "invalid_role",
        ),
    ] {
        let (s, b, _) = call!(&app, admin.req("POST", "/users").set_json(&body));
        assert_eq!((s, code_of(&b)), (status, code), "{body}: {b}");
    }
    // nothing was created by any of those
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    assert!(!ids_of(&users).contains_key(NEW_ADMIN), "{users}");
    // the old body: "admin" is an admin, any other role a member with no grants
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/users")
            .set_json(json!({"email": NEW_LEGACY, "role": "editor"}))
    );
    assert_eq!(s, 201, "{b}");
    assert_eq!(
        (&b["is_admin"], &b["role"], &b["feeds"]),
        (&json!(false), &json!("editor"), &json!([])),
        "{b}"
    );
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/users")
            .set_json(json!({"email": NEW_ADMIN, "role": "admin"}))
    );
    assert_eq!(s, 201, "{b}");
    assert_eq!(
        (&b["is_admin"], &b["role"]),
        (&json!(true), &json!("admin")),
        "{b}"
    );
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    for u in users["items"].as_array().unwrap() {
        assert!(u["kind"].is_string() && u["feeds"].is_array(), "{u}");
    }
    let system = users["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == SYSTEM)
        .unwrap();
    assert_eq!(system["kind"], "system", "{system}");

    // ---- promoting and demoting an admin
    let pid = &ids[PROMOTED];
    let (s, b, _) = call!(&app, grant(PROMOTED, "viewer"));
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        admin
            .req("PATCH", &format!("/users/{pid}"))
            .set_json(json!({"admin": true}))
    );
    assert_eq!(s, 200, "{b}");
    // an admin holds no grants: the one they had is superseded
    assert_eq!(
        (&b["is_admin"], &b["role"], &b["feeds"]),
        (&json!(true), &json!("admin"), &json!([])),
        "{b}"
    );
    let (_, me, _) = call!(&app, promoted.req("GET", "/auth/me"));
    assert_eq!(me["is_admin"], true, "{me}");
    assert_eq!(feed_ids(&me["feeds"]), vec![FEED_A, FEED_B], "{me}");
    let (s, b, _) = call!(&app, promoted.req("GET", "/users"));
    assert_eq!(
        s, 200,
        "a new admin manages users on their next request: {b}"
    );
    // the old body demotes too, and a demoted admin has no grants until given some
    let (s, b, _) = call!(
        &app,
        admin
            .req("PATCH", &format!("/users/{pid}"))
            .set_json(json!({"role": "editor"}))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (&b["is_admin"], &b["role"], &b["feeds"]),
        (&json!(false), &json!("editor"), &json!([])),
        "{b}"
    );
    let (_, me, _) = call!(&app, promoted.req("GET", "/auth/me"));
    assert_eq!(
        (&me["is_admin"], &me["feeds"]),
        (&json!(false), &json!([])),
        "{me}"
    );
    let (s, b, _) = call!(&app, promoted.req("GET", &format!("/feeds/{FEED_A}/stops")));
    assert_eq!((s, code_of(&b)), (403, "no_feed_access"), "{b}");
    let (s, b, _) = call!(&app, promoted.req("GET", "/users"));
    assert_eq!((s, code_of(&b)), (403, "role_required"), "{b}");
    let flips = scalar_i64(
        &pool,
        &format!(
            "SELECT count(*) FROM gtfs_audit_log WHERE audit_id > {since} \
             AND action = 'user_admin_changed' AND detail->>'user_id' = '{pid}'"
        ),
    )
    .await;
    assert_eq!(flips, 2, "a promotion and a demotion");
    let superseded = scalar_i64(
        &pool,
        &format!(
            "SELECT count(*) FROM gtfs_audit_log WHERE audit_id > {since} \
             AND action = 'feed_access_revoked' AND gtfs_id = '{FEED_A}' AND detail->>'user_id' = '{pid}'"
        ),
    )
    .await;
    assert_eq!(
        superseded, 1,
        "the promotion revoked the grant it superseded"
    );
    // what PATCH refuses
    let admin_id = &ids[ADMIN];
    for (id, body, code) in [
        (admin_id, json!({"admin": false}), "cannot_change_self"),
        (admin_id, json!({"role": "viewer"}), "cannot_change_self"),
        (&ids[SYSTEM], json!({"admin": true}), "system_account"),
        (pid, json!({}), "nothing_to_change"),
        (
            pid,
            json!({"admin": true, "role": "editor"}),
            "invalid_role",
        ),
    ] {
        let (s, b, _) = call!(
            &app,
            admin.req("PATCH", &format!("/users/{id}")).set_json(&body)
        );
        assert_eq!((s, code_of(&b)), (400, code), "{body}: {b}");
    }

    // ---- a system account cannot sign in, on any path, bootstrap email or not
    let (s, b, _) = call!(&app, grant(SYSTEM, "editor"));
    assert_eq!(
        (s, &b["kind"], &b["feeds"][0]["role"]),
        (200, &json!("system"), &json!("editor")),
        "admins grant it feeds like anyone else: {b}"
    );
    let sync = caller(SYSTEM);
    for (method, path, body) in [
        ("GET", "/auth/me", None),
        ("POST", "/auth/totp/enroll", None),
        (
            "POST",
            "/auth/totp/confirm",
            Some(json!({"code": "000000"})),
        ),
        ("POST", "/auth/session", Some(json!({"code": "000000"}))),
        ("GET", "/feeds", None),
    ] {
        let (s, b, _) = call!(&app, sync.send(method, path, &body));
        assert_eq!(
            (s, code_of(&b)),
            (403, "account_disabled"),
            "{method} {path}: {b}"
        );
    }
    let (s, b, _) = call!(&app, sync.req("GET", &format!("/feeds/{FEED_A}/stops")));
    assert_eq!((s, code_of(&b)), (403, "account_disabled"), "{b}");
    let still = sqlx::query("SELECT role, totp_enabled FROM gtfs_editor_user WHERE email = $1")
        .bind(SYSTEM)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (
            still.get::<String, _>("role"),
            still.get::<bool, _>("totp_enabled")
        ),
        ("editor".to_string(), false),
        "a system account is never made a bootstrap admin, and never enrols"
    );

    let mut cleanup = clear_feed(FEED_A);
    cleanup.extend(clear_feed(FEED_B));
    exec(&pool, &cleanup).await;
    std::fs::remove_dir_all(dir).ok();
}

/// 0018's backfill, the migration file itself, in a scratch schema holding
/// copies of the two tables it reads: every non-admin gets a chennai_bus grant
/// at the role they hold, admins and other feeds get none, and a second run
/// neither fails nor hands back a grant revoked since.
#[actix_web::test]
async fn the_backfill_gives_chennai_bus_the_old_roles_once() {
    let Some(url) = database_url() else {
        return;
    };
    let mut conn = PgConnection::connect(&url).await.unwrap();
    let schema = format!(
        "feed_access_backfill_{}",
        crypto::random_token()[..10]
            .to_lowercase()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
    );
    let setup = format!(
        "CREATE SCHEMA {schema}; \
         SET search_path TO {schema}; \
         CREATE TABLE gtfs_feed (LIKE public.gtfs_feed INCLUDING ALL); \
         CREATE TABLE gtfs_editor_user (LIKE public.gtfs_editor_user INCLUDING ALL); \
         ALTER TABLE gtfs_editor_user DROP COLUMN kind; \
         INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('chennai_bus', 'Chennai bus'), ('other', 'Other'); \
         INSERT INTO gtfs_editor_user (email, role, status) VALUES \
           ('admin@x.invalid', 'admin', 'active'), ('viewer@x.invalid', 'viewer', 'active'), \
           ('editor@x.invalid', 'editor', 'active'), ('approver@x.invalid', 'approver', 'active'), \
           ('off@x.invalid', 'editor', 'disabled');"
    );
    let grants =
        "SELECT u.email || ':' || coalesce(a.gtfs_id, '-') || ':' || coalesce(a.role, '-') \
                  || ':' || u.kind FROM gtfs_editor_user u \
                  LEFT JOIN gtfs_editor_feed_access a ON a.user_id = u.user_id ORDER BY u.email";
    let run = async {
        conn.execute(setup.as_str()).await?;
        conn.execute(MIGRATION).await?;
        let first: Vec<String> = sqlx::query_scalar(grants).fetch_all(&mut conn).await?;
        conn.execute(
            "DELETE FROM gtfs_editor_feed_access WHERE user_id = \
             (SELECT user_id FROM gtfs_editor_user WHERE email = 'editor@x.invalid')",
        )
        .await?;
        conn.execute(MIGRATION).await?;
        let second: Vec<String> = sqlx::query_scalar(grants).fetch_all(&mut conn).await?;
        Ok::<_, sqlx::Error>((first, second))
    };
    let result = run.await;
    // the scratch schema goes whatever happened, before anything is asserted
    let mut drop_conn = PgConnection::connect(&url).await.unwrap();
    drop_conn
        .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
        .await
        .unwrap();
    let (first, second) = result.unwrap();
    assert_eq!(
        first,
        vec![
            "admin@x.invalid:-:-:person",
            "approver@x.invalid:chennai_bus:approver:person",
            "editor@x.invalid:chennai_bus:editor:person",
            "off@x.invalid:chennai_bus:editor:person",
            "viewer@x.invalid:chennai_bus:viewer:person",
        ]
    );
    assert_eq!(
        second,
        vec![
            "admin@x.invalid:-:-:person",
            "approver@x.invalid:chennai_bus:approver:person",
            "editor@x.invalid:-:-:person",
            "off@x.invalid:chennai_bus:editor:person",
            "viewer@x.invalid:chennai_bus:viewer:person",
        ],
        "a second run hands back nothing an admin has revoked since"
    );
}
