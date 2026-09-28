//! A stop out of use for a while (docs/gtfs-editor.md section 21), end to end
//! against a real Postgres holding the editor schema
//! (db/gtfs_editor/0001..0025):
//!
//! - the flag goes through a draft like any other stop edit, and says what it
//!   will do: how many routes stop calling there, the fare stage whose first
//!   stop it is, and the stop orders left with too little to publish;
//! - the export keeps the stop's `stops.txt` row and writes no `stop_times` row
//!   for it, while every other stop of every trip keeps the time it had;
//! - a trip left with one call is not published at all, and comes back with the
//!   stop;
//! - clearing the flag gives a zip byte-identical to the one before it was set,
//!   because nothing was stored or recomputed.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local. Uses its own feed and accounts and removes their rows
//! afterwards; never touches chennai_bus.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, feed_io, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::gtfs::{read, spec, write};
use gtfs_routes_service::services::gtfs_db_source::GtfsDbSource;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::Arc;

#[path = "support/gtfs_fixture.rs"]
mod gtfs_fixture;
use gtfs_fixture::fixture;

const AUD: &str = "gtfs.editor-unserviceable-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_unserviceable_test_feed";
const ADMIN: &str = "admin@editor-unserviceable-test.invalid";
const EDITOR: &str = "editor@editor-unserviceable-test.invalid";
const APPROVER: &str = "approver@editor-unserviceable-test.invalid";

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
    for e in [ADMIN, EDITOR, APPROVER] {
        sqlx::query(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, \
             totp_last_step = NULL, status = 'active' WHERE email = $1",
        )
        .bind(e)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "DELETE FROM gtfs_editor_session WHERE user_id IN \
             (SELECT user_id FROM gtfs_editor_user WHERE email = $1)",
        )
        .bind(e)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn state(pool: &PgPool, signer: &TestSigner) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-unserviceable-{}", crypto::random_token()));
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

/// The feed's GTFS as the exporter writes it now.
async fn export(pool: &PgPool) -> read::RawFeed {
    let mut conn = pool.acquire().await.unwrap();
    let (model, findings) = feed_io::load_model(&mut conn, FEED).await.unwrap();
    assert!(findings.is_empty(), "{findings:#?}");
    let bytes = write::zip_bytes(&write::to_raw(&model)).unwrap();
    read::read_zip(&bytes).unwrap().0
}

/// `stop_times.txt` as `trip_id|stop_id -> (arrival, departure, stop_sequence)`.
fn times_of(raw: &read::RawFeed) -> std::collections::BTreeMap<String, (String, String, String)> {
    let t = raw.table("stop_times.txt").expect("stop_times.txt");
    let at = |h: &str| t.header.iter().position(|c| c == h).expect("a column");
    let (trip, stop, arr, dep, seq) = (
        at("trip_id"),
        at("stop_id"),
        at("arrival_time"),
        at("departure_time"),
        at("stop_sequence"),
    );
    t.rows
        .iter()
        .map(|r| {
            (
                format!("{}|{}", r[trip], r[stop]),
                (r[arr].clone(), r[dep].clone(), r[seq].clone()),
            )
        })
        .collect()
}

fn ids_in(raw: &read::RawFeed, file: &str, column: &str) -> Vec<String> {
    let t = raw.table(file).unwrap_or_else(|| panic!("{file}"));
    let at = t
        .header
        .iter()
        .position(|c| c == column)
        .unwrap_or_else(|| panic!("{file}.{column}"));
    t.rows.iter().map(|r| r[at].clone()).collect()
}

fn codes(v: &Value) -> Vec<String> {
    v["validation"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|f| f["code"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- the flow

#[actix_web::test]
async fn a_stop_goes_out_of_use_and_comes_back() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("gtfs_routes_service=debug")
        .try_init();
    let Some(pool) = local_pool().await else {
        return;
    };
    clear_feed(&pool, FEED).await;
    let zip = fixture(FEED);
    let who = feed_io::Importer {
        user_id: None,
        email: None,
        label: "editor_unserviceable_flow".into(),
    };
    feed_io::import_zip(&pool, &zip, None, false, &who)
        .await
        .unwrap();

    let signer = TestSigner::generate("unserviceable-key");
    let (st, dir) = state(&pool, &signer);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;
    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    let mut ed = Caller {
        signer: &signer,
        email: EDITOR.into(),
        session: None,
    };
    let mut ap = Caller {
        signer: &signer,
        email: APPROVER.into(),
        session: None,
    };
    let enrol = |c: &Caller| c.req("POST", "/auth/totp/enroll");
    let (s, b, _) = call!(&app, enrol(&admin));
    assert_eq!(s, 200, "{b}");
    let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let (s, b, cookie) = call!(
        &app,
        admin
            .req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now())}))
    );
    assert_eq!(s, 200, "{b}");
    admin.session = cookie;
    for (email, role) in [(EDITOR, "editor"), (APPROVER, "approver")] {
        let (s, b, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || b["error"]["code"] == "user_exists", "{s} {b}");
    }
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    for (email, role) in [(EDITOR, "editor"), (APPROVER, "approver")] {
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
        // since 0018 a member works on a feed only through a grant on it
        let (s, b, _) = call!(
            &app,
            admin
                .req("PUT", &format!("/users/{id}/feeds/{FEED}"))
                .set_json(json!({"role": role}))
        );
        assert_eq!(s, 200, "{b}");
    }
    for c in [&mut ed, &mut ap] {
        let (s, b, _) = call!(&app, enrol(c));
        assert_eq!(s, 200, "{b}");
        let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
        let (s, b, cookie) = call!(
            &app,
            c.req("POST", "/auth/totp/confirm")
                .set_json(json!({"code": crypto::totp_now(&secret, now())}))
        );
        assert_eq!(s, 200, "{b}");
        c.session = cookie;
    }

    // submit, approve and commit: a draft goes live
    macro_rules! ship {
        ($set:expr) => {{
            for (who, action) in [(&ed, "submit"), (&ap, "approve"), (&ap, "commit")] {
                let (s, b, _) = call!(
                    &app,
                    who.req("POST", &format!("/change-sets/{}/{action}", $set))
                );
                assert_eq!(s, 200, "{action}: {b}");
            }
        }};
    }
    macro_rules! draft {
        ($title:expr) => {{
            let (s, b, _) = call!(
                &app,
                ed.req("POST", &format!("/feeds/{FEED}/change-sets"))
                    .set_json(json!({"title": $title}))
            );
            assert_eq!(s, 201, "{b}");
            b["change_set_id"].as_str().unwrap().to_string()
        }};
    }
    let flag = |set: &str, stop: &str, on: bool| {
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({"entity": "stop", "op": "update", "entity_key": stop,
                             "after": {"unserviceable": on}}))
    };

    // ---- the feed as it stands: B1 is the middle stop of R1's long trips, and
    // the second of two on T3, the short turn
    let before = export(&pool).await;
    let times_before = times_of(&before);
    assert!(
        times_before.contains_key("T1|B1"),
        "the fixture calls at B1"
    );
    assert!(times_before.contains_key("T3|B1"), "T3 is P1 then B1");
    assert!(ids_in(&before, "stops.txt", "stop_id").contains(&"B1".to_string()));

    // ======================================================== out of use
    let d1 = draft!("Beach is closed for the flyover work");
    let (s, b, _) = call!(&app, flag(&d1, "B1", true));
    assert_eq!(s, 201, "{b}");
    let found = codes(&b);
    assert!(
        found.iter().any(|c| c == "stop_unserviceable"),
        "it says what goes out of use: {b}"
    );
    assert!(
        found.iter().any(|c| c == "pattern_too_short"),
        "and that T3's stop order would be left with one call: {b}"
    );
    assert!(
        !found.iter().any(|c| c == "route_too_short"),
        "but R1 still has stop orders to publish: {b}"
    );
    ship!(d1);

    let (s, stop, _) = call!(&app, ed.req("GET", &format!("/feeds/{FEED}/stops/B1")));
    assert_eq!(s, 200, "{stop}");
    assert_eq!(stop["unserviceable"], true, "the stop reads as out of use");
    let (s, list, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{FEED}/unserviceable-stops"))
    );
    assert_eq!(s, 200, "{list}");
    let watchlist: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["stop_id"].as_str().unwrap())
        .collect();
    assert_eq!(watchlist, vec!["B1"], "the watchlist is the list to check");

    // ---- the export: the stop stays, its calls go
    let out = export(&pool).await;
    assert!(
        ids_in(&out, "stops.txt", "stop_id").contains(&"B1".to_string()),
        "the stops.txt row stays, so the stop still exists for the app and for a router"
    );
    let times_after = times_of(&out);
    assert!(
        times_after.keys().all(|k| !k.ends_with("|B1")),
        "no trip calls at it: {:?}",
        times_after.keys().collect::<Vec<_>>()
    );
    for (key, before) in &times_before {
        if key.ends_with("|B1") || key.starts_with("T3|") {
            continue;
        }
        assert_eq!(
            times_after.get(key),
            Some(before),
            "{key} keeps the time and the sequence it had"
        );
    }
    // the trip left with one call is not published at all, nor its frequencies
    assert!(
        !ids_in(&out, "trips.txt", "trip_id").contains(&"T3".to_string()),
        "T3 called at two stops and would now call at one"
    );
    assert!(
        times_after.keys().all(|k| !k.starts_with("T3|")),
        "so it has no stop times either"
    );
    // ---- the loader, which is what the live APIs serve: the stop stays on the
    // route so an app can grey it, and no trip calls there. The loader builds
    // patterns and trips for a feed that takes both from the tables.
    sqlx::query("UPDATE gtfs_feed SET data_source = 'db', trips_source = 'db' WHERE gtfs_id = $1")
        .bind(FEED)
        .execute(&pool)
        .await
        .unwrap();
    let db = GtfsDbSource::new(pool.clone(), vec![]);
    let feed = db
        .load_feed(FEED, &std::collections::HashMap::new())
        .await
        .unwrap();
    let on_route: Vec<(&str, Option<bool>)> = feed
        .patterns
        .iter()
        .flat_map(|p| p.stops.iter())
        .map(|st| (st.id.as_str(), st.unserviceable))
        .collect();
    assert!(
        on_route
            .iter()
            .any(|(id, flag)| id.ends_with(":B1") && *flag == Some(true)),
        "the stop stays on its routes, marked out of use: {on_route:?}"
    );
    let t1 = feed
        .trips
        .as_ref()
        .expect("a DB-trips feed")
        .trip("T1")
        .expect("trip T1");
    let called: Vec<String> = t1.stops.iter().map(|(st, _, _)| st.id.clone()).collect();
    assert!(
        called.iter().all(|id| !id.ends_with(":B1")),
        "the trip the API serves does not call there: {called:?}"
    );
    assert_eq!(
        called.len(),
        2,
        "its other two calls are unchanged: {called:?}"
    );

    // a fare stage whose first stop went out of use is still that stage
    let headsigns: Vec<String> = ids_in(&out, "stop_times.txt", "stop_headsign");
    assert!(
        headsigns.iter().any(|h| h == "Beach"),
        "the stage's headsign is still in the feed: {headsigns:?}"
    );

    // ======================================================== and back
    let d2 = draft!("the flyover work is done");
    let (s, b, _) = call!(&app, flag(&d2, "B1", false));
    assert_eq!(s, 201, "{b}");
    assert!(
        codes(&b).iter().any(|c| c == "stop_serviceable_again"),
        "{b}"
    );
    ship!(d2);

    let again = export(&pool).await;
    assert_eq!(
        times_of(&again),
        times_before,
        "every call is back, with the time it always had"
    );
    assert_eq!(
        write::csv_text(again.table("stop_times.txt").unwrap()),
        write::csv_text(before.table("stop_times.txt").unwrap()),
        "stop_times.txt is byte-identical to before the stop went out of use"
    );
    let (_, stop, _) = call!(&app, ed.req("GET", &format!("/feeds/{FEED}/stops/B1")));
    assert_eq!(stop["unserviceable"], false);

    // ======================================================== a route with nothing left
    // R3 runs one trip over SELF and B2; taking B2 out of use would leave every
    // stop order of it with one call, so nothing of the route could be published
    let d3 = draft!("try to close a stop a whole route needs");
    let (s, b, _) = call!(&app, flag(&d3, "B2", true));
    assert_eq!(s, 201, "{b}");
    assert!(
        codes(&b).iter().any(|c| c == "route_too_short"),
        "a route left with nothing to publish is an error, not a diversion: {b}"
    );
    let (s, b, _) = call!(&app, ed.req("POST", &format!("/change-sets/{d3}/submit")));
    assert_eq!(s, 400, "so the draft cannot be submitted: {b}");
    assert_eq!(b["error"]["code"], "validation_failed", "{b}");

    clear_feed(&pool, FEED).await;
    std::fs::remove_dir_all(dir).ok();
}
