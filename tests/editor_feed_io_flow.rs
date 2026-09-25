//! A feed's whole GTFS into the editor's tables and back out
//! (docs/gtfs-editor.md section 18), end to end against a real Postgres
//! holding the editor schema (db/gtfs_editor/0001..0023):
//!
//! - every record table is the file the spec says it is: a column per field,
//!   of the field's type;
//! - a dry run writes nothing; a seed writes every file, reads it back in the
//!   same transaction, compares it with the zip and audits `seed`;
//! - a feed that has rows, or open drafts, is not seeded again;
//! - `GET /feeds/{g}/gtfs.zip` gives the zip back, to anyone who may see the
//!   feed; `POST /feeds/{g}/import` is an admin's.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local. Uses its own feeds and accounts and removes their rows
//! afterwards; never touches chennai_bus.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, feed_io, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::gtfs::{compare, read, spec, write};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

#[path = "support/gtfs_fixture.rs"]
mod gtfs_fixture;
use gtfs_fixture::fixture;

const AUD: &str = "gtfs.editor-feed-io-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_feed_io_test_feed";
const HTTP_FEED: &str = "editor_feed_io_http_test_feed";
const ADMIN: &str = "admin@editor-feed-io-test.invalid";
const VIEWER: &str = "viewer@editor-feed-io-test.invalid";

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

/// `(status, body as JSON (or Null), body bytes, session cookie, content type)`
macro_rules! call {
    ($app:expr, $req:expr) => {{
        let resp = test::call_service($app, $req.to_request()).await;
        let status = resp.status().as_u16();
        let cookie = session_from(&resp);
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = test::read_body(resp).await;
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, json, body, cookie, ctype)
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

/// Every row of a test feed, in an order its keys allow; the audit log is
/// append-only and keeps its rows.
async fn clear_feed(pool: &PgPool, g: &str) {
    let mut tables = vec![
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
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    tables.extend(spec::FILES.iter().filter_map(|f| f.table()));
    tables.push("gtfs_editor_feed_access".into());
    tables.push("gtfs_feed".into());
    for t in tables {
        sqlx::query(&format!("DELETE FROM {t} WHERE gtfs_id = $1"))
            .bind(g)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{t}: {e}"));
    }
}

async fn count(pool: &PgPool, table: &str, g: &str) -> i64 {
    sqlx::query(&format!("SELECT count(*) FROM {table} WHERE gtfs_id = $1"))
        .bind(g)
        .fetch_one(pool)
        .await
        .unwrap()
        .get(0)
}

#[actix_web::test]
async fn every_record_table_is_the_file_the_spec_says() {
    let Some(pool) = local_pool().await else {
        return;
    };
    for fspec in spec::FILES {
        let (Some(table), Some(key)) = (fspec.table(), fspec.key()) else {
            continue;
        };
        let cols: Vec<(String, String)> = sqlx::query(
            "SELECT column_name, data_type FROM information_schema.columns WHERE table_name = $1",
        )
        .bind(&table)
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
        assert!(!cols.is_empty(), "{table} does not exist");
        let has = |name: &str| {
            cols.iter()
                .find(|(c, _)| c == name)
                .map(|(_, t)| t.as_str())
        };
        for fs in fspec.fields {
            let want = match spec::sql_type(fs.ty) {
                t if t.starts_with("text") => "text",
                t => t,
            };
            let want = if table == "gtfs_shape" && fs.name != "shape_id" {
                "ARRAY"
            } else {
                want
            };
            assert_eq!(has(fs.name), Some(want), "{table}.{}", fs.name);
        }
        for common in [
            "gtfs_id",
            "sort_key",
            "row_version",
            "updated_at",
            "updated_by",
        ] {
            assert!(has(common).is_some(), "{table}.{common}");
        }
        if matches!(key, spec::Key::Minted { .. }) {
            assert_eq!(has("row_id"), Some("text"), "{table}.row_id");
        }
    }
    // every change entity the spec knows is one gtfs_change accepts
    let check: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'gtfs_change_entity_check'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    for e in spec::BESPOKE_ENTITIES
        .iter()
        .copied()
        .chain(spec::record_entities())
    {
        assert!(check.contains(&format!("'{e}'")), "gtfs_change refuses {e}");
    }
}

#[actix_web::test]
async fn a_feed_is_seeded_from_its_zip_and_exports_it_back() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear_feed(&pool, FEED).await;
    let zip = fixture(FEED);
    let who = feed_io::Importer {
        user_id: None,
        email: None,
        label: "editor_feed_io_flow".into(),
    };

    // ---- a dry run reads, writes, compares and keeps nothing
    let report = feed_io::import_zip(&pool, &zip, None, true, &who)
        .await
        .unwrap();
    assert_eq!(report.gtfs_id, FEED, "the feed the zip names");
    assert_eq!(report.errors, 0, "{:#?}", report.findings);
    assert!(
        report.round_trip.is_empty(),
        "{:#?}",
        report.round_trip_sample
    );
    assert!(!report.seeded);
    assert_eq!(
        count(&pool, "gtfs_feed", FEED).await,
        0,
        "a dry run writes nothing"
    );

    // ---- the seed
    let started: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let report = feed_io::import_zip(&pool, &zip, None, false, &who)
        .await
        .unwrap();
    assert!(report.seeded, "{report:#?}");
    assert!(report.round_trip.is_empty());
    for (table, n) in [
        ("gtfs_stop", 8),
        ("gtfs_route", 3),
        ("gtfs_pattern", 5),
        ("gtfs_route_stop", 12),
        ("gtfs_timing_profile", 5),
        ("gtfs_trip", 6),
        ("gtfs_frequency", 2),
        ("gtfs_service", 2),
        ("gtfs_service_date", 3),
        ("gtfs_agency", 2),
        ("gtfs_feed_info", 1),
        ("gtfs_shape", 1),
        ("gtfs_level", 2),
        ("gtfs_pathway", 3),
        ("gtfs_transfer", 3),
        ("gtfs_fare_attribute", 1),
        ("gtfs_fare_rule", 2),
        ("gtfs_timeframe", 2),
        ("gtfs_rider_category", 2),
        ("gtfs_fare_media", 1),
        ("gtfs_fare_product", 2),
        ("gtfs_fare_leg_rule", 1),
        ("gtfs_fare_leg_join_rule", 1),
        ("gtfs_fare_transfer_rule", 1),
        ("gtfs_area", 1),
        ("gtfs_stop_area", 2),
        ("gtfs_network", 1),
        ("gtfs_route_network", 1),
        ("gtfs_location_group", 1),
        ("gtfs_location_group_stop", 2),
        ("gtfs_location", 1),
        ("gtfs_booking_rule", 1),
        ("gtfs_translation", 3),
        ("gtfs_attribution", 2),
    ] {
        assert_eq!(count(&pool, table, FEED).await, n, "{table}");
    }
    let feed = sqlx::query(
        "SELECT version, stops_scope, data_source, agency_name FROM gtfs_feed WHERE gtfs_id = $1",
    )
    .bind(FEED)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(feed.get::<i64, _>("version"), report.feed_version.unwrap());
    assert_eq!(feed.get::<String, _>("stops_scope"), "all");
    assert_eq!(
        feed.get::<String, _>("data_source"),
        "preprocessed",
        "seeding a feed does not switch what GIMS serves"
    );
    assert_eq!(
        feed.get::<Option<String>, _>("agency_name").as_deref(),
        Some("Metro")
    );
    // what the stop rows hold for the awkward cases
    let self_parent: Option<String> = sqlx::query_scalar(
        "SELECT parent_station FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = 'SELF'",
    )
    .bind(FEED)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(self_parent.as_deref(), Some("SELF"));
    let (lt, cluster): (i16, Option<String>) = sqlx::query_as(
        "SELECT location_type, cluster_id FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = 'B1'",
    )
    .bind(FEED)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((lt, cluster.as_deref()), (0, Some("c7")));
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gtfs_audit_log WHERE gtfs_id = $1 AND action = 'seed' \
         AND detail->>'zip_sha256' = $2 AND at >= $3",
    )
    .bind(FEED)
    .bind(&report.zip_sha256)
    .bind(started)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);

    // ---- the tables give the feed back
    let mut conn = pool.acquire().await.unwrap();
    let (back, findings) = feed_io::load_model(&mut conn, FEED).await.unwrap();
    assert!(findings.is_empty(), "{findings:#?}");
    let (raw, _) = read::read_zip(&zip).unwrap();
    let exported = write::zip_bytes(&write::to_raw(&back)).unwrap();
    let (again, _) = read::read_zip(&exported).unwrap();
    let diffs = compare::compare(&raw, &again, &Default::default());
    assert!(diffs.is_empty(), "{diffs:#?}");
    drop(conn);

    // ---- and it is not seeded twice
    let err = feed_io::import_zip(&pool, &zip, None, false, &who)
        .await
        .unwrap_err();
    assert_eq!(
        (err.status.as_u16(), err.code),
        (409, "feed_not_empty"),
        "{}",
        err.message
    );

    clear_feed(&pool, FEED).await;
}

fn state(pool: &PgPool, signer: &TestSigner) -> EditorState {
    let dir = std::env::temp_dir().join(format!("editor-feed-io-{}", crypto::random_token()));
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
    let (s, b, _, cookie, _) = call!(
        app,
        c.req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now)}))
    );
    assert_eq!(s, 200, "{b}");
    c.session = cookie;
}

#[actix_web::test]
async fn an_admin_imports_a_zip_and_a_viewer_downloads_it() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear_feed(&pool, HTTP_FEED).await;
    for email in [ADMIN, VIEWER] {
        sqlx::query(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, \
             totp_last_step = NULL, status = 'active' WHERE email = $1",
        )
        .bind(email)
        .execute(&pool)
        .await
        .unwrap();
    }
    let signer = TestSigner::generate("feed-io-test-key");
    let st = state(&pool, &signer);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;
    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    sign_in(&app, &mut admin).await;
    let zip = fixture(HTTP_FEED);

    // ---- the spec, for the dashboard's forms
    let (s, b, _, _, _) = call!(&app, admin.req("GET", "/gtfs-spec"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["files"].as_array().unwrap().len(), spec::FILES.len());

    // ---- a dry run, then the seed
    let (s, b, _, _, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{HTTP_FEED}/import"))
            .set_payload(zip.clone())
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (b["dry_run"].clone(), b["seeded"].clone()),
        (json!(true), json!(false)),
        "{b}"
    );
    assert_eq!(b["errors"], 0, "{b}");
    let (s, b, _, _, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{HTTP_FEED}/import?seed=true"))
            .set_payload(zip.clone())
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["seeded"], true, "{b}");
    // a zip that names another feed is refused as a finding, and writes nothing
    let (s, b, _, _, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{FEED}/import?seed=true"))
            .set_payload(fixture("some_other_feed"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["seeded"], false, "{b}");
    assert!(
        b["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["code"] == "feed_id_mismatch"),
        "{b}"
    );

    // ---- a viewer with a grant downloads it; the import is not theirs
    let (s, b, _, _, _) = call!(
        &app,
        admin
            .req("POST", "/users")
            .set_json(json!({"email": VIEWER, "role": "viewer"}))
    );
    assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    let (_, users, _, _, _) = call!(&app, admin.req("GET", "/users"));
    let viewer_id = users["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == VIEWER)
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, b, _, _, _) = call!(
        &app,
        admin
            .req("PUT", &format!("/users/{viewer_id}/feeds/{HTTP_FEED}"))
            .set_json(json!({"role": "viewer"}))
    );
    assert_eq!(s, 200, "{b}");
    let mut viewer = Caller {
        signer: &signer,
        email: VIEWER.into(),
        session: None,
    };
    sign_in(&app, &mut viewer).await;
    let (s, _, body, _, ctype) = call!(
        &app,
        viewer.req("GET", &format!("/feeds/{HTTP_FEED}/gtfs.zip"))
    );
    assert_eq!(s, 200);
    assert_eq!(ctype, "application/zip");
    let (raw, _) = read::read_zip(&zip).unwrap();
    let (back, _) = read::read_zip(&body).unwrap();
    let diffs = compare::compare(&raw, &back, &Default::default());
    assert!(diffs.is_empty(), "{diffs:#?}");
    let (s, b, _, _, _) = call!(
        &app,
        viewer
            .req("POST", &format!("/feeds/{HTTP_FEED}/import"))
            .set_payload(zip.clone())
    );
    assert_eq!(s, 403, "{b}");
    // a feed the viewer holds no grant on does not exist for them
    let (s, b, _, _, _) = call!(&app, viewer.req("GET", "/feeds/kochi_metro/gtfs.zip"));
    assert_eq!((s, code_of(&b)), (403, "no_feed_access"), "{b}");

    // (the zip that named another feed wrote nothing to FEED, which the seed
    // test above works on at the same time)
    clear_feed(&pool, HTTP_FEED).await;
}
