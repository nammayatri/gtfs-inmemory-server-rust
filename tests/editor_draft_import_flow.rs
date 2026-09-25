//! A GTFS zip brought into a feed that already has rows, through drafts
//! (docs/gtfs-editor.md section 18), end to end against a real Postgres
//! holding the editor schema (db/gtfs_editor/0001..0023):
//!
//! - the first step drafts what the trips need - the agency and feed_info as
//!   the zip has them, a new service, and a split: a stop order the feed has,
//!   with another pickup - and nothing else; once it is committed the second
//!   drafts the trips of every route whose trips differ;
//! - a trip on the feed's default timing keeps the explicit profile a seed
//!   gave that timing; a route whose one stop order the feed has otherwise,
//!   running on the default timing, gets its trips on the feed's stop order;
//!   a route whose stop order and timing are the zip's own keeps its trips, as
//!   does a route the feed does not have;
//! - stops and routes are compared and never written;
//! - run again, it finds nothing to do;
//! - over HTTP it is an admin's, a dry run unless asked otherwise.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local. Uses its own feed and accounts and removes its rows
//! afterwards.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, draft_import, feed_io, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::gtfs::spec;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

#[path = "support/gtfs_fixture.rs"]
#[allow(dead_code)]
mod gtfs_fixture;
use gtfs_fixture::zip_of;

const AUD: &str = "gtfs.editor-draft-import-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_draft_import_test_feed";
const ADMIN: &str = "admin@editor-draft-import-test.invalid";
const EDITOR: &str = "editor@editor-draft-import-test.invalid";
const APPROVER: &str = "approver@editor-draft-import-test.invalid";

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
    let dir = std::env::temp_dir().join(format!("editor-draft-import-{}", crypto::random_token()));
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
    let (s, b, _) = call!(app, c.req("POST", "/auth/totp/enroll"));
    assert_eq!(s, 200, "{b}");
    let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let now = chrono::Utc::now().timestamp() as u64;
    let (s, b, cookie) = call!(
        app,
        c.req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now)}))
    );
    assert_eq!(s, 200, "{b}");
    c.session = cookie;
}

async fn scalar_text(pool: &PgPool, sql: &str) -> Option<String> {
    sqlx::query(sql)
        .fetch_optional(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .and_then(|r| r.try_get::<Option<String>, _>(0).ok().flatten())
}

async fn scalar_i64(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<i64, _>(0)
}

const STOPS: &str = "stop_id,stop_name,stop_lat,stop_lon\n\
S1,One,13.01,80.01\nS2,Two,13.02,80.02\nS3,Three,13.03,80.03\n\
S4,Four,13.04,80.04\nS5,Five,13.05,80.05\nS6,Six,13.06,80.06\n";

/// The feed as the editor holds it: every trip on the default timing (120 s
/// a hop, 15 s at a stop) but R3's.
fn seed_zip() -> Vec<u8> {
    let feed_info = format!(
        "feed_publisher_name,feed_publisher_url,feed_lang,feed_version,feed_id\nNY,https://ny.example,en,v1,{FEED}\n"
    );
    zip_of(&[
        ("agency.txt", "agency_id,agency_name,agency_url,agency_timezone\nA1,Metro,https://m.example,Asia/Kolkata\n"),
        ("feed_info.txt", &feed_info),
        ("stops.txt", STOPS),
        ("routes.txt", "route_id,agency_id,route_short_name,route_type\nR1,A1,1,3\nR2,A1,2,3\nR3,A1,3,3\nR4,A1,4,3\n"),
        ("calendar.txt", "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\nWK,1,1,1,1,1,0,0,20260101,20261231\n"),
        ("trips.txt", "route_id,service_id,trip_id\nR1,WK,T1\nR2,WK,T2\nR3,WK,T3\nR4,WK,T4\n"),
        ("stop_times.txt", "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
T1,06:00:00,06:00:15,S1,1\nT1,06:02:15,06:02:30,S2,2\nT1,06:04:30,06:04:30,S3,3\n\
T2,07:00:00,07:00:15,S1,1\nT2,07:02:15,07:02:30,S4,2\nT2,07:04:30,07:04:30,S5,3\n\
T3,08:00:00,08:00:00,S2,1\nT3,08:10:00,08:10:00,S3,2\nT3,08:20:00,08:20:00,S6,3\n\
T4,08:00:00,08:00:15,S5,1\nT4,08:02:15,08:02:15,S6,2\n"),
    ])
}

/// What the operator sends next.
fn next_zip() -> Vec<u8> {
    let feed_info = format!(
        "feed_publisher_name,feed_publisher_url,feed_lang,feed_version,feed_id\nNY,https://ny.example,en,v2,{FEED}\n"
    );
    zip_of(&[
        ("agency.txt", "agency_id,agency_name,agency_url,agency_timezone,agency_phone\nA1,Metro,https://m2.example,Asia/Kolkata,044-1\n"),
        ("feed_info.txt", &feed_info),
        // S1 renamed: compared, never written
        ("stops.txt", &STOPS.replace("S1,One", "S1,Uno")),
        ("routes.txt", "route_id,agency_id,route_short_name,route_type\nR1,A1,1,3\nR2,A1,2,3\nR3,A1,3,3\nR4,A1,4,3\nR9,A1,9,3\n"),
        ("calendar.txt", "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\n\
WK,1,1,1,1,1,0,0,20260101,20261231\nSAT,0,0,0,0,0,1,0,20260101,20261231\n"),
        ("trips.txt", "route_id,service_id,trip_id\nR1,WK,T1\nR1,SAT,T1B\nR2,WK,T2\nR3,WK,T3\nR4,WK,T4\nR9,WK,T9\n"),
        // R1: T1 as it was and T1B; R2: no pickup at S1 (a split); R3: its
        // own stop order and timing; R4: the other way round, an hour later;
        // R9: a route the feed does not have
        ("stop_times.txt", "trip_id,arrival_time,departure_time,stop_id,stop_sequence,pickup_type\n\
T1,06:00:00,06:00:15,S1,1,\nT1,06:02:15,06:02:30,S2,2,\nT1,06:04:30,06:04:30,S3,3,\n\
T1B,09:00:00,09:00:15,S1,1,\nT1B,09:02:15,09:02:30,S2,2,\nT1B,09:04:30,09:04:30,S3,3,\n\
T2,07:00:00,07:00:15,S1,1,1\nT2,07:02:15,07:02:30,S4,2,\nT2,07:04:30,07:04:30,S5,3,\n\
T3,08:00:00,08:00:00,S2,1,\nT3,08:10:00,08:10:00,S3,2,\n\
T4,09:00:00,09:00:15,S6,1,\nT4,09:02:15,09:02:15,S5,2,\n\
T9,10:00:00,10:00:15,S1,1,\nT9,10:02:15,10:02:15,S6,2,\n"),
    ])
}

#[actix_web::test]
async fn a_zip_comes_into_a_feed_with_rows_through_drafts() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;
    let who = feed_io::Importer {
        user_id: None,
        email: None,
        label: "editor_draft_import_flow".into(),
    };
    let seeded = feed_io::import_zip(&pool, &seed_zip(), Some(FEED), false, &who)
        .await
        .unwrap();
    assert!(seeded.seeded, "{seeded:#?}");

    let signer = TestSigner::generate("draft-import-test-key");
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
    let mut editor_id = None;
    for (email, role) in [(EDITOR, "editor"), (APPROVER, "approver")] {
        let (s, b, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
        let (_, users, _) = call!(&app, admin.req("GET", "/users"));
        let id = users["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["email"] == email)
            .unwrap()["user_id"]
            .as_str()
            .unwrap()
            .to_string();
        if role == "editor" {
            editor_id = Some(uuid::Uuid::parse_str(&id).unwrap());
        }
        let (s, b, _) = call!(
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
    macro_rules! commit {
        ($set:expr) => {{
            let set = $set;
            let (s, b, _) = call!(
                &app,
                editor_c.req("POST", &format!("/change-sets/{set}/submit"))
            );
            assert_eq!(s, 200, "submit: {b}");
            for step in ["approve", "commit"] {
                let (s, b, _) = call!(
                    &app,
                    approver.req("POST", &format!("/change-sets/{set}/{step}"))
                );
                assert_eq!(s, 200, "{step}: {b}");
            }
        }};
    }
    let import = |dry_run: bool| draft_import::DraftImport {
        files: None,
        dry_run,
        user_id: editor_id.unwrap(),
        email: EDITOR.into(),
    };
    let text = |sql: String| {
        let pool = pool.clone();
        async move { scalar_text(&pool, &sql).await }
    };
    let sets_of_feed = || {
        let pool = pool.clone();
        async move {
            scalar_i64(
                &pool,
                &format!("SELECT count(*) FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
            )
            .await
        }
    };

    // ---- step 1, previewed: nothing written
    let r = draft_import::draft_import(&pool, &next_zip(), Some(FEED), &import(true))
        .await
        .unwrap();
    assert_eq!((r.step, r.errors), ("records", 0), "{r:#?}");
    assert_eq!(r.change_sets.len(), 1);
    assert_eq!(r.change_sets[0].change_set_id, None);
    assert_eq!(sets_of_feed().await, 0);
    assert_eq!(
        r.differences.get("stops.txt.stop_name"),
        Some(&1),
        "{:?}",
        r.differences
    );
    assert_eq!(
        r.differences.get("routes.txt rows only in the first"),
        Some(&1),
        "{:?}",
        r.differences
    );
    let so = &r.stop_orders;
    assert_eq!(
        (so.same, so.split, so.moved, so.routes_left),
        (1, 1, 1, 2),
        "{so:?}"
    );
    assert_eq!(so.moved_sample, vec!["R4"]);
    assert_eq!(so.routes_left_sample, vec!["R3", "R9"]);

    // ---- step 1: the records, the new service and the split
    let r = draft_import::draft_import(&pool, &next_zip(), Some(FEED), &import(false))
        .await
        .unwrap();
    assert_eq!((r.step, r.errors), ("records", 0), "{r:#?}");
    assert!(r.next.is_some());
    let a = r.change_sets[0].change_set_id.unwrap();
    let (s, detail, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{a}")));
    assert_eq!(s, 200, "{detail}");
    let mut what: Vec<String> = detail["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{}/{} {}",
                c["entity"].as_str().unwrap(),
                c["op"].as_str().unwrap(),
                c["entity_key"].as_str().unwrap()
            )
        })
        .collect();
    what.sort();
    assert_eq!(
        what,
        vec![
            "agency/update A1".to_string(),
            format!("feed_info/update {FEED}"),
            "route_stops/replace R2".to_string(),
            "service/create SAT".to_string(),
        ]
    );
    let agency = detail["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["entity"] == "agency")
        .unwrap();
    assert_eq!(
        agency["after"],
        json!({"agency_url": "https://m2.example", "agency_phone": "044-1"})
    );
    // the trips wait for this set: running again drafts it again, nothing more
    commit!(a);

    // ---- step 2: the trips of every route whose trips differ
    let r = draft_import::draft_import(&pool, &next_zip(), Some(FEED), &import(false))
        .await
        .unwrap();
    assert_eq!((r.step, r.errors), ("trips", 0), "{r:#?}");
    assert_eq!(r.change_sets.len(), 1);
    assert_eq!(
        (r.change_sets[0].routes, r.change_sets[0].trips),
        (3, 4),
        "{r:#?}"
    );
    commit!(r.change_sets[0].change_set_id.unwrap());

    // ---- what the feed now holds
    assert_eq!(
        text(format!(
            "SELECT concat_ws('|', agency_url, agency_phone) FROM gtfs_agency WHERE gtfs_id = '{FEED}'"
        ))
        .await
        .as_deref(),
        Some("https://m2.example|044-1")
    );
    assert_eq!(
        text(format!(
            "SELECT feed_version FROM gtfs_feed_info WHERE gtfs_id = '{FEED}'"
        ))
        .await
        .as_deref(),
        Some("v2")
    );
    assert_eq!(
        text(format!(
            "SELECT string_agg(concat_ws(':', stop_id, pickup_type), ' ' ORDER BY sequence) \
             FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'R2' AND pattern_key = 2"
        ))
        .await
        .as_deref(),
        Some("S1:1 S4 S5")
    );
    assert_eq!(
        text(format!(
            "SELECT string_agg(concat_ws(':', trip_id, service_id, pattern_key, coalesce(profile_key::text, '-'), ref_s), ' ' \
                               ORDER BY route_id, trip_id) \
             FROM gtfs_trip WHERE gtfs_id = '{FEED}'"
        ))
        .await
        .as_deref(),
        Some(
            "T1:WK:1:1:21600 T1B:SAT:1:1:32400 T2:WK:2:-:25200 T3:WK:1:1:28800 T4:WK:1:1:32400"
        )
    );
    assert_eq!(
        text(format!(
            "SELECT name FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'S1'"
        ))
        .await
        .as_deref(),
        Some("One"),
        "a draft import never writes a stop"
    );

    // ---- again: nothing to do
    let before = sets_of_feed().await;
    let r = draft_import::draft_import(&pool, &next_zip(), Some(FEED), &import(false))
        .await
        .unwrap();
    assert_eq!((r.step, r.change_sets.len()), ("none", 0), "{r:#?}");
    assert_eq!(sets_of_feed().await, before);

    // ---- over HTTP: an admin's, a dry run unless asked otherwise
    let upload = |c: &Caller, query: &str| {
        c.req("POST", &format!("/feeds/{FEED}/import?{query}"))
            .insert_header(("content-type", "application/zip"))
            .set_payload(next_zip())
    };
    let (s, b, _) = call!(&app, upload(editor_c, "mode=drafts"));
    assert_eq!(s, 403, "{b}");
    let (s, b, _) = call!(&app, upload(&admin, "mode=drafts&files=stops.txt"));
    assert_eq!((s, code_of(&b)), (400, "compared_only"), "{b}");
    let (s, b, _) = call!(&app, upload(&admin, "mode=drafts"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (&b["dry_run"], &b["step"]),
        (&json!(true), &json!("none")),
        "{b}"
    );

    clear(&pool).await;
}
