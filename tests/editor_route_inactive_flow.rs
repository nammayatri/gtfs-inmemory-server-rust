//! An inactive route in the editor (docs/gtfs-editor.md section 18.17), end to
//! end against a real Postgres holding the editor schema (db/gtfs_editor/0001..0026):
//!
//! - a route is made inactive through a draft like any other change: refused
//!   unless `active` is true or false, committed by someone else, the feed's
//!   version moved;
//! - the routes list says which routes are inactive, and lists only those with
//!   `active=false`;
//! - the full download marks it `route_active = 0` and gives it back on a
//!   reload as it was; the published zip (`as=published`) leaves it and its
//!   trips out;
//! - a reload from a zip that does not mark it says it would make it active
//!   again.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local. Uses its own feed and accounts and removes its rows
//! afterwards.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, feed_io, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::gtfs::{read, spec};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

#[path = "support/gtfs_fixture.rs"]
mod gtfs_fixture;

const AUD: &str = "gtfs.editor-route-inactive-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_route_inactive_test_feed";
const ADMIN: &str = "admin@editor-route-inactive-test.invalid";
const EDITOR: &str = "editor@editor-route-inactive-test.invalid";
const APPROVER: &str = "approver@editor-route-inactive-test.invalid";

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

/// `(status, body as JSON (or Null), session cookie, body bytes, the
/// X-Inactive-Routes-Left-Out header)`
macro_rules! call {
    ($app:expr, $req:expr) => {{
        let resp = test::call_service($app, $req.to_request()).await;
        let status = resp.status().as_u16();
        let cookie = session_from(&resp);
        let left_out = resp
            .headers()
            .get("x-inactive-routes-left-out")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = test::read_body(resp).await;
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, json, cookie, body, left_out)
    }};
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
    let mut tables: Vec<String> = [
        "gtfs_change_set",
        "gtfs_frequency",
        "gtfs_trip",
        "gtfs_timing_profile",
        "gtfs_route_stop",
        "gtfs_pattern",
        "gtfs_service_date",
        "gtfs_service",
        "gtfs_route",
        "gtfs_stop",
    ]
    .iter()
    .map(|t| t.to_string())
    .collect();
    tables.extend(spec::FILES.iter().filter_map(|f| f.table()));
    tables.push("gtfs_editor_feed_access".into());
    tables.push("gtfs_feed".into());
    for t in tables {
        sqlx::query(&format!("DELETE FROM {t} WHERE gtfs_id = $1"))
            .bind(FEED)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{t}: {e}"));
    }
    for email in [ADMIN, EDITOR, APPROVER] {
        sqlx::query(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, \
             totp_last_step = NULL, status = 'active' WHERE email = $1",
        )
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn state(pool: &PgPool, signer: &TestSigner) -> EditorState {
    let dir =
        std::env::temp_dir().join(format!("editor-route-inactive-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    use base64::Engine;
    EditorState::build(
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
    .unwrap()
}

async fn sign_in(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    c: &mut Caller<'_>,
) {
    let (s, b, _, _, _) = call!(app, c.req("POST", "/auth/totp/enroll"));
    assert_eq!(s, 200, "{b}");
    let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let now = chrono::Utc::now().timestamp() as u64;
    let (s, b, cookie, _, _) = call!(
        app,
        c.req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now)}))
    );
    assert_eq!(s, 200, "{b}");
    c.session = cookie;
}

async fn version(pool: &PgPool) -> i64 {
    sqlx::query("SELECT version FROM gtfs_feed WHERE gtfs_id = $1")
        .bind(FEED)
        .fetch_one(pool)
        .await
        .unwrap()
        .get(0)
}

async fn active(pool: &PgPool, route: &str) -> bool {
    sqlx::query("SELECT active FROM gtfs_route WHERE gtfs_id = $1 AND route_id = $2")
        .bind(FEED)
        .bind(route)
        .fetch_one(pool)
        .await
        .unwrap()
        .get(0)
}

/// A file of a zip as `(header, rows)`.
fn file_of(zip: &[u8], name: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let (raw, _) = read::read_zip(zip).unwrap();
    let t = raw
        .table(name)
        .unwrap_or_else(|| panic!("no {name} in the zip"));
    (t.header.clone(), t.rows.clone())
}

fn column(zip: &[u8], name: &str, field: &str) -> Vec<String> {
    let (raw, _) = read::read_zip(zip).unwrap();
    let t = raw.table(name).unwrap();
    t.rows
        .iter()
        .map(|r| t.cell(r, field).to_string())
        .collect()
}

#[actix_web::test]
async fn a_route_made_inactive_leaves_the_published_zip_and_comes_back_on_a_reload() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;
    let who = feed_io::Importer {
        user_id: None,
        email: None,
        label: "editor_route_inactive_flow".into(),
    };
    let shipped = gtfs_fixture::fixture(FEED);
    let seeded = feed_io::import_zip(&pool, &shipped, None, false, &who)
        .await
        .unwrap();
    assert!(seeded.seeded, "{seeded:#?}");
    assert!(active(&pool, "R2").await, "a seeded route is active");

    let signer = TestSigner::generate("route-inactive-test-key");
    let app = test::init_service(
        App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(state(&pool, &signer))))),
    )
    .await;
    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    sign_in(&app, &mut admin).await;
    let mut callers = Vec::new();
    for (email, role) in [(EDITOR, "editor"), (APPROVER, "approver")] {
        let (s, b, _, _, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || b["error"]["code"] == "user_exists", "{s} {b}");
        let (_, users, _, _, _) = call!(&app, admin.req("GET", "/users"));
        let id = users["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["email"] == email)
            .unwrap()["user_id"]
            .as_str()
            .unwrap()
            .to_string();
        let (s, b, _, _, _) = call!(
            &app,
            admin
                .req("PUT", &format!("/users/{id}/feeds/{FEED}"))
                .set_json(json!({"role": role}))
        );
        assert_eq!(s, 200, "{b}");
        let mut c = Caller {
            signer: &signer,
            email: email.into(),
            session: None,
        };
        sign_in(&app, &mut c).await;
        callers.push(c);
    }
    let (editor_c, approver) = (&callers[0], &callers[1]);
    let get = |path: String| editor_c.req("GET", &path);

    // ---- made inactive through a draft
    let (s, route, _, _, _) = call!(&app, get(format!("/feeds/{FEED}/routes/R2")));
    assert_eq!(s, 200, "{route}");
    assert_eq!(route["active"], true, "{route}");
    let (s, b, _, _, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "retire 21G"}))
    );
    assert_eq!(s, 201, "{b}");
    let set = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _, _, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(
                json!({"entity": "route", "op": "update", "entity_key": "R2",
                             "after": {"active": "no"}})
            )
    );
    assert_eq!(s, 400, "only true or false: {b}");
    let (s, b, _, _, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(
                json!({"entity": "route", "op": "update", "entity_key": "R2",
                             "after": {"active": false}})
            )
    );
    assert_eq!(s, 201, "{b}");
    let before = version(&pool).await;
    let (s, b, _, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set}/submit"))
    );
    assert_eq!(s, 200, "submit: {b}");
    for step in ["approve", "commit"] {
        let (s, b, _, _, _) = call!(
            &app,
            approver.req("POST", &format!("/change-sets/{set}/{step}"))
        );
        assert_eq!(s, 200, "{step}: {b}");
    }
    assert!(!active(&pool, "R2").await);
    assert!(version(&pool).await > before, "GIMS sees the feed move");

    // ---- the routes list says so, and lists only the inactive ones on asking
    let listed = |v: &Value| -> Vec<(String, bool)> {
        v["items"]
            .as_array()
            .unwrap_or_else(|| panic!("{v}"))
            .iter()
            .map(|r| {
                (
                    r["route_id"].as_str().unwrap().to_string(),
                    r["active"].as_bool().unwrap(),
                )
            })
            .collect()
    };
    let (s, all, _, _, _) = call!(&app, get(format!("/feeds/{FEED}/routes")));
    assert_eq!(s, 200, "{all}");
    assert!(listed(&all).contains(&("R2".into(), false)), "{all}");
    assert!(listed(&all).contains(&("R1".into(), true)), "{all}");
    let (_, inactive, _, _, _) = call!(&app, get(format!("/feeds/{FEED}/routes?active=false")));
    assert_eq!(listed(&inactive), vec![("R2".to_string(), false)]);
    let (_, live, _, _, _) = call!(&app, get(format!("/feeds/{FEED}/routes?active=true")));
    assert!(!listed(&live).iter().any(|(id, _)| id == "R2"), "{live}");

    // ---- the full download marks it; the published zip leaves it out
    let (s, b, _, full, left_out) = call!(&app, get(format!("/feeds/{FEED}/gtfs.zip")));
    assert_eq!(s, 200, "{b}");
    assert_eq!(left_out.as_deref(), Some("0"));
    let (header, _) = file_of(&full, "routes.txt");
    assert!(header.iter().any(|h| h == "route_active"), "{header:?}");
    let ids = column(&full, "routes.txt", "route_id");
    let marks = column(&full, "routes.txt", "route_active");
    let mark_of = |id: &str| marks[ids.iter().position(|r| r == id).unwrap()].clone();
    assert_eq!(
        (mark_of("R1"), mark_of("R2")),
        ("".to_string(), "0".to_string())
    );
    assert!(column(&full, "trips.txt", "trip_id").contains(&"T5".to_string()));

    let (s, b, _, published, left_out) =
        call!(&app, get(format!("/feeds/{FEED}/gtfs.zip?as=published")));
    assert_eq!(s, 200, "{b}");
    assert_eq!(left_out.as_deref(), Some("1"));
    let (header, _) = file_of(&published, "routes.txt");
    assert!(!header.iter().any(|h| h == "route_active"), "{header:?}");
    let ids = column(&published, "routes.txt", "route_id");
    assert!(
        !ids.contains(&"R2".to_string()) && ids.contains(&"R1".to_string()),
        "{ids:?}"
    );
    let trips = column(&published, "trips.txt", "trip_id");
    assert!(!trips.contains(&"T5".to_string()), "R2's trip: {trips:?}");
    let mut timed = column(&published, "stop_times.txt", "trip_id");
    timed.dedup();
    assert!(!timed.contains(&"T5".to_string()), "{timed:?}");
    let (s, b, _, _, _) = call!(&app, get(format!("/feeds/{FEED}/gtfs.zip?as=nope")));
    assert_eq!(s, 400, "{b}");

    // ---- a reload from the full download keeps it inactive; from a zip that
    // does not mark it, it says it would make it active again
    let report = feed_io::reload_zip(&pool, &full, FEED, true, &who, None)
        .await
        .unwrap();
    assert!(
        report.round_trip.is_empty(),
        "{:#?}",
        report.round_trip_sample
    );
    assert!(report.reactivated.is_empty(), "{:?}", report.reactivated);
    let report = feed_io::reload_zip(&pool, &shipped, FEED, true, &who, None)
        .await
        .unwrap();
    assert_eq!(report.reactivated, vec!["R2".to_string()]);
    assert!(!active(&pool, "R2").await, "a dry run writes nothing");
    let report = feed_io::reload_zip(&pool, &full, FEED, false, &who, None)
        .await
        .unwrap();
    assert!(report.seeded, "{report:#?}");
    assert!(!active(&pool, "R2").await, "reloaded as it was");
    assert!(active(&pool, "R1").await);

    clear(&pool).await;
}
