//! Trips, stop times and service calendars as editor changes
//! (docs/gtfs-editor.md section 16.4), end to end against a real Postgres
//! holding the editor schema (db/gtfs_editor/0001..0019):
//!
//! - `service/create`, `update` and `delete`, with dates; a service that never
//!   runs is a warning, one in use cannot go;
//! - a second stop order (`route_stops/replace` with a `pattern_key`), named by
//!   `pattern/update`, and `pattern/delete` refused while trips run it and
//!   always for pattern 1;
//! - `timing_profile/replace` - a profile key minted when it is added, one offset
//!   per served stop, never backwards, implausible hops a warning - and
//!   `timing_profile/delete` refused while trips run to it;
//! - `route_trips/replace`: ids minted, reference times from the start time and
//!   the profile, frequency windows, a trip id another route holds, sort keys and
//!   sources kept for trips sent back;
//! - a stop list change carrying its pattern's profiles over, with the
//!   `timing_interpolated` warning and the new times in the preview;
//! - conflicts on a moved trip list and a moved profile, like `route_stops`;
//! - a stop only a second stop order calls at is still in use, and a merge
//!   switches it on every stop order;
//! - `feed_config` gains the trips settings (`trips_need_db`), `route/update`
//!   gains `schedule_source`, and the commit audits the settings it switched.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local (see scripts/editor_flow_test.sh). Uses its own feed and
//! accounts and removes its rows afterwards; never touches chennai_bus.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

const AUD: &str = "gtfs.editor-trips-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_trips_test_feed";
const ADMIN: &str = "admin@editor-trips-test.invalid";
const EDITOR: &str = "editor@editor-trips-test.invalid";
const APPROVER: &str = "approver@editor-trips-test.invalid";
/// sha256 of `[]`: no trips, no profile, no rows (section 5).
const NOTHING: &str = "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945";

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

async fn exec(pool: &PgPool, stmts: &[String]) {
    for s in stmts {
        sqlx::query(s)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{s}: {e}"));
    }
}

fn clear_feed() -> Vec<String> {
    let accounts = format!("'{ADMIN}', '{EDITOR}', '{APPROVER}'");
    vec![
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_trip WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_service WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
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
    ]
}

/// A metro-like feed with no fare stages: stops about a kilometre apart on a
/// line, route R1 calling at S1..S4 and R2 at S5, S6.
fn seed() -> Vec<String> {
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, agency_name) \
         VALUES ('{FEED}', 'Editor trips test feed', 'TRIPSAG')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
         ('{FEED}', 'S1', 'S1', 'ONE',   13.000, 80.2), ('{FEED}', 'S2', 'S2', 'TWO',   13.010, 80.2), \
         ('{FEED}', 'S3', 'S3', 'THREE', 13.020, 80.2), ('{FEED}', 'S4', 'S4', 'FOUR',  13.030, 80.2), \
         ('{FEED}', 'S5', 'S5', 'FIVE',  13.015, 80.2), ('{FEED}', 'S6', 'S6', 'SIX',   13.040, 80.2), \
         ('{FEED}', 'S7', 'S7', 'SEVEN', 13.050, 80.2), ('{FEED}', 'S8', 'S8', 'EIGHT', 13.015, 80.2)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type) VALUES \
         ('{FEED}', 'R1', 'BLUE', 'ONE To FOUR', 1), ('{FEED}', 'R2', 'GREEN', 'FIVE To SIX', 1)"
    ));
    // no stage_no / stage_name: a feed with no fare stages (0017's defaults)
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type) VALUES \
         ('{FEED}', 'R1', 1, 'S1', 'NEW STOP'), ('{FEED}', 'R1', 2, 'S2', 'NEW STOP'), \
         ('{FEED}', 'R1', 3, 'S3', 'NEW STOP'), ('{FEED}', 'R1', 4, 'S4', 'NEW STOP'), \
         ('{FEED}', 'R2', 1, 'S5', 'NEW STOP'), ('{FEED}', 'R2', 2, 'S6', 'NEW STOP')"
    ));
    s
}

fn state(pool: &PgPool, signer: &TestSigner, admin: &str) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-trips-{}", crypto::random_token()));
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

async fn scalar_i64(pool: &PgPool, sql: impl AsRef<str>) -> i64 {
    let sql = sql.as_ref();
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<i64, _>(0)
}

/// The findings of one change, as `level:code`.
fn findings_of(set: &Value, change_id: i64) -> Vec<String> {
    set["validation"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["change_id"] == change_id)
        .map(|v| {
            format!(
                "{}:{}",
                v["level"].as_str().unwrap(),
                v["code"].as_str().unwrap()
            )
        })
        .collect()
}

fn change_of(set: &Value, change_id: i64) -> Value {
    set["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["change_id"] == change_id)
        .cloned()
        .unwrap_or(Value::Null)
}

#[actix_web::test]
async fn trips_profiles_patterns_and_services_go_through_drafts() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("trips-test-key");
    let (st, dir) = state(&pool, &signer, ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    sign_in(&app, &mut admin).await;
    for (email, role) in [(EDITOR, "editor"), (APPROVER, "approver")] {
        let (s, b, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
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
    let mut editor_c = Caller {
        signer: &signer,
        email: EDITOR.into(),
        session: None,
    };
    let mut approver = Caller {
        signer: &signer,
        email: APPROVER.into(),
        session: None,
    };
    sign_in(&app, &mut editor_c).await;
    sign_in(&app, &mut approver).await;

    let new_set = |c: &Caller, title: &str| {
        c.req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": title}))
    };
    let add = |c: &Caller, set: &str, change: Value| {
        c.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(change)
    };
    macro_rules! open_set {
        ($c:expr, $title:expr) => {{
            let (s, b, _) = call!(&app, new_set($c, $title));
            assert_eq!(s, 201, "{b}");
            b["change_set_id"].as_str().unwrap().to_string()
        }};
    }
    macro_rules! added {
        ($c:expr, $set:expr, $change:expr) => {{
            let (s, b, _) = call!(&app, add($c, $set, $change));
            assert_eq!(s, 201, "{b}");
            (b["change_id"].as_i64().unwrap(), b)
        }};
    }
    macro_rules! commit {
        ($set:expr) => {{
            let (s, b, _) = call!(
                &app,
                editor_c.req("POST", &format!("/change-sets/{}/submit", $set))
            );
            assert_eq!(s, 200, "submit: {b}");
            let (s, b, _) = call!(
                &app,
                approver.req("POST", &format!("/change-sets/{}/approve", $set))
            );
            assert_eq!(s, 200, "approve: {b}");
            let (s, b, _) = call!(
                &app,
                approver.req("POST", &format!("/change-sets/{}/commit", $set))
            );
            assert_eq!(s, 200, "commit: {b}");
            b
        }};
    }

    // ---- shapes refused when a change is added
    let shapes = open_set!(&editor_c, "bad shapes");
    for (change, code) in [
        (
            json!({"entity": "route_trips", "op": "replace", "entity_key": "R1", "after": {"base_trips_hash": NOTHING,
                   "trips": [{"pattern_key": 1, "service_id": "WK", "start_time": "5.10"}]}}),
            "invalid_time",
        ),
        (
            json!({"entity": "route_trips", "op": "replace", "entity_key": "R1", "after": {"base_trips_hash": NOTHING,
                   "trips": [{"trip_id": "R1:T", "pattern_key": 1, "service_id": "WK", "start_time": "05:10"}]}}),
            "invalid_payload",
        ),
        (
            json!({"entity": "service", "op": "create", "entity_key": "BAD", "after": {"service_id": "BAD",
                   "days": {"monday": true}, "start_date": "2026-10-01", "end_date": "2026-09-01"}}),
            "invalid_dates",
        ),
        (
            json!({"entity": "timing_profile", "op": "replace", "entity_key": "R1",
                   "after": {"pattern_key": 1, "arrival_s": [0, 90, 80, 200], "departure_s": [10, 100, 90, 200]}}),
            "timing_goes_backwards",
        ),
        (
            json!({"entity": "pattern", "op": "update", "entity_key": "R1", "after": {"pattern_key": 1}}),
            "invalid_payload",
        ),
    ] {
        let (s, b, _) = call!(&app, add(&editor_c, &shapes, change.clone()));
        assert_eq!(
            (s, code_of(&b), b["error"]["details"]["code"].as_str()),
            (400, "invalid_change", Some(code)),
            "{change}: {b}"
        );
    }
    // the trips settings of a feed are an admin's, like its data source
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &shapes,
            json!({"entity": "feed_config", "op": "update", "entity_key": FEED, "after": {"trips_source": "db"}})
        )
    );
    assert_eq!((s, code_of(&b)), (403, "role_required"), "{b}");

    // ======================================================== draft A
    let a = open_set!(&editor_c, "timetable for R1");
    let (wk, _) = added!(
        &editor_c,
        &a,
        json!({"entity": "service", "op": "create", "after": {"service_id": "WK",
               "days": {"monday": true, "tuesday": true, "wednesday": true, "thursday": true, "friday": true},
               "start_date": "2026-09-01", "end_date": "2026-12-31", "label": "weekdays",
               "dates": [{"date": "2026-10-02", "exception_type": 2}]}})
    );
    // a service that never runs is allowed - stub trips hang off one - and said
    let (stub, b) = added!(
        &editor_c,
        &a,
        json!({"entity": "service", "op": "create", "entity_key": "STUB", "after": {"service_id": "STUB", "days": {}}})
    );
    assert_eq!(findings_of(&b, stub), vec!["warning:service_never_runs"]);
    assert_eq!(change_of(&b, wk)["entity_key"], "WK");

    // a second stop order: a short turn, replacing a pattern R1 does not have yet
    let (turn, b) = added!(
        &editor_c,
        &a,
        json!({"entity": "route_stops", "op": "replace", "entity_key": "R1", "after": {
               "pattern_key": 2, "base_rows_hash": NOTHING, "rows": [
               {"stop_id": "S1", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": ""},
               {"stop_id": "S2", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": "", "stop_headsign": "Towards TWO"},
               {"stop_id": "S7", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": "", "pickup_type": 1}]}})
    );
    // no fare stages on this feed: stage 0 and no stage name are fine
    assert!(findings_of(&b, turn).is_empty(), "{b}");
    assert_eq!(change_of(&b, turn)["before"], json!([]));
    let (named, _) = added!(
        &editor_c,
        &a,
        json!({"entity": "pattern", "op": "update", "entity_key": "R1",
               "after": {"pattern_key": 2, "name": "short turn to SEVEN", "direction_id": 0}})
    );
    // a profile for pattern 1: its key is minted, and it is new
    let (peak, b) = added!(
        &editor_c,
        &a,
        json!({"entity": "timing_profile", "op": "replace", "entity_key": "R1", "after": {
               "pattern_key": 1, "label": "peak",
               "arrival_s": [0, 150, 300, 450], "departure_s": [20, 170, 320, 450]}})
    );
    let profile = change_of(&b, peak);
    assert_eq!(profile["after"]["profile_key"], 1, "{profile}");
    assert_eq!(profile["after"]["base_hash"], NOTHING);
    // a hop of ~1.1 km in 10 s is a warning, the wrong number of stops an error
    let (fast, b) = added!(
        &editor_c,
        &a,
        json!({"entity": "timing_profile", "op": "replace", "entity_key": "R1", "after": {
               "pattern_key": 2, "arrival_s": [0, 10, 200], "departure_s": [0, 10, 200]}})
    );
    assert_eq!(change_of(&b, fast)["after"]["profile_key"], 1);
    assert_eq!(findings_of(&b, fast), vec!["warning:timing_implausible"]);
    let (short, b) = added!(
        &editor_c,
        &a,
        json!({"entity": "timing_profile", "op": "replace", "entity_key": "R1", "after": {
               "pattern_key": 2, "arrival_s": [0, 100], "departure_s": [0, 100]}})
    );
    assert_eq!(change_of(&b, short)["after"]["profile_key"], 2);
    assert_eq!(
        findings_of(&b, short),
        vec!["error:profile_length_mismatch"]
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req("DELETE", &format!("/change-sets/{a}/changes/{short}"))
    );
    assert_eq!(s, 200, "{b}");

    // the trips: ids minted, a trip on the default timing, one on the profile,
    // one on the short turn, and one running every 10 minutes all evening
    let (trips, b) = added!(
        &editor_c,
        &a,
        json!({"entity": "route_trips", "op": "replace", "entity_key": "R1", "after": {
               "base_trips_hash": NOTHING, "trips": [
               {"pattern_key": 1, "service_id": "WK", "direction_id": 0, "start_time": "06:00:00", "shape_id": "SH1"},
               {"trip_id": "R1-PEAK", "pattern_key": 1, "profile_key": 1, "service_id": "WK", "direction_id": 0,
                "start_time": "8:30", "headsign": "FOUR", "source_ref": {"schedule_number": "B-12"}},
               {"trip_id": "R1-TURN", "pattern_key": 2, "profile_key": 1, "service_id": "WK", "start_time": "07:00:00"},
               {"trip_id": "R1-EVE", "pattern_key": 1, "service_id": "STUB", "start_time": "18:00:00",
                "frequencies": [{"start_time": "18:00:00", "end_time": "22:00:00", "headway_s": 600},
                                {"start_time": "22:00:00", "end_time": "48:00:00", "headway_s": 1800, "exact_times": 1}]}]}})
    );
    let minted = change_of(&b, trips)["after"]["trips"][0]["trip_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        minted.starts_with("R1-ed-") && minted.len() == "R1-ed-".len() + 8,
        "{minted}"
    );
    assert!(findings_of(&b, trips).is_empty(), "{b}");
    // R2 cannot take a trip id R1 holds, even in the same draft
    let (taken, b) = added!(
        &editor_c,
        &a,
        json!({"entity": "route_trips", "op": "replace", "entity_key": "R2", "after": {
               "base_trips_hash": NOTHING, "trips": [
               {"trip_id": "R1-PEAK", "pattern_key": 1, "service_id": "WK", "start_time": "09:00:00"},
               {"trip_id": "R2-A", "pattern_key": 3, "service_id": "WK", "start_time": "09:00:00"},
               {"trip_id": "R2-B", "pattern_key": 1, "service_id": "NOPE", "start_time": "09:00:00"},
               {"trip_id": "R2-C", "pattern_key": 1, "profile_key": 4, "service_id": "WK", "start_time": "09:30:00"}]}})
    );
    assert_eq!(
        findings_of(&b, taken),
        vec![
            "error:trip_id_taken",
            "error:pattern_not_found",
            "error:service_not_found",
            "error:profile_not_found"
        ]
    );
    let (s, _, _) = call!(
        &app,
        editor_c.req("DELETE", &format!("/change-sets/{a}/changes/{taken}"))
    );
    assert_eq!(s, 200);

    // the draft applied: R1's timetable and trips in its preview
    let (s, b, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{a}/preview/routes/R1"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["trip_count"], 4, "{b}");
    let patterns = b["patterns"].as_array().unwrap();
    assert_eq!(patterns.len(), 2);
    assert_eq!(patterns[1]["name"], "short turn to SEVEN");
    assert_eq!(patterns[1]["trip_count"], 1);
    // a stop order's public id is the preprocessor's md5 of its stop ids
    assert_eq!(
        patterns[0]["pattern_id"],
        format!("{FEED}:R1:{}", &md5_hex("S1|S2|S3|S4")[..8])
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/change-sets/{a}/preview/routes/R1/patterns/2")
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["rows"][1]["stop_headsign"], "Towards TWO");
    assert_eq!(b["rows"][2]["pickup_type"], 1);
    let (s, b, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{a}/preview/routes/R1/trips"))
    );
    assert_eq!(s, 200, "{b}");
    let starts: Vec<(String, String)> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["trip_id"].as_str().unwrap().to_string(),
                t["start_time"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        starts,
        vec![
            (minted.clone(), "06:00:00".to_string()),
            ("R1-PEAK".into(), "08:30:00".into()),
            ("R1-TURN".into(), "07:00:00".into()),
            ("R1-EVE".into(), "18:00:00".into()),
        ]
    );
    let set = commit!(&a);
    let version_a = set["feed_version"].as_i64().unwrap();
    let _ = named;

    // ---- what the commit wrote
    let row = sqlx::query(&format!(
        "SELECT ref_s, sort_key, source, source_ref->>'schedule_number' AS schedule, headsign \
         FROM gtfs_trip WHERE gtfs_id = '{FEED}' AND trip_id = 'R1-PEAK'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i32, _>("ref_s"), 8 * 3600 + 30 * 60);
    assert_eq!(row.get::<i32, _>("sort_key"), 2);
    assert_eq!(row.get::<String, _>("source"), "editor");
    assert_eq!(
        row.get::<Option<String>, _>("schedule").as_deref(),
        Some("B-12")
    );
    assert_eq!(
        scalar_i64(&pool, format!("SELECT count(*) FROM gtfs_frequency WHERE gtfs_id = '{FEED}' AND trip_id = 'R1-EVE'")).await,
        2
    );
    assert_eq!(
        scalar_i64(
            &pool,
            format!("SELECT max(end_s)::int8 FROM gtfs_frequency WHERE gtfs_id = '{FEED}'")
        )
        .await,
        48 * 3600
    );
    assert_eq!(
        scalar_i64(&pool, format!("SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'R1' AND pattern_key = 2")).await,
        3
    );
    assert_eq!(
        scalar_i64(&pool, format!("SELECT count(*) FROM gtfs_service_date WHERE gtfs_id = '{FEED}' AND service_id = 'WK' AND exception_type = 2")).await,
        1
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/services"))
    );
    assert_eq!(s, 200, "{b}");
    let wk_row = &b["items"][1];
    assert_eq!(
        (
            &wk_row["service_id"],
            &wk_row["days"]["friday"],
            &wk_row["trip_count"],
            &wk_row["dates"][0]["date"]
        ),
        (&json!("WK"), &json!(true), &json!(3), &json!("2026-10-02"))
    );
    let (s, live, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R1/trips"))
    );
    assert_eq!(s, 200, "{live}");
    let trips_hash = live["trips_hash"].as_str().unwrap().to_string();
    assert_ne!(trips_hash, NOTHING);
    assert_eq!(live["items"][0]["shape_id"], "SH1");

    // ---- sent back as read, a trip keeps its id, sort key and source
    let b_set = open_set!(&editor_c, "shift the evening trip");
    let mut items = live["items"].as_array().unwrap().clone();
    items[3]["start_time"] = json!("18:05:00");
    items[3]["frequencies"] = json!([]);
    let (shift, _) = added!(
        &editor_c,
        &b_set,
        json!({"entity": "route_trips", "op": "replace", "entity_key": "R1",
               "after": {"base_trips_hash": trips_hash, "trips": items.clone()}})
    );
    // a second draft based on the same list, committed first, moves it on
    let c_set = open_set!(&editor_c, "drop the peak trip");
    let fewer: Vec<Value> = items
        .iter()
        .filter(|t| t["trip_id"] != "R1-PEAK")
        .cloned()
        .collect();
    added!(
        &editor_c,
        &c_set,
        json!({"entity": "route_trips", "op": "replace", "entity_key": "R1",
               "after": {"base_trips_hash": trips_hash, "trips": fewer}})
    );
    commit!(&c_set);
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{b_set}/submit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    let conflict = &b["error"]["details"]["conflicts"][0];
    assert_eq!(
        (
            &conflict["change_id"],
            &conflict["entity"],
            &conflict["reason"]
        ),
        (&json!(shift), &json!("route_trips"), &json!("changed"))
    );
    assert!(conflict["message"]
        .as_str()
        .unwrap()
        .starts_with("The trips of route R1"));
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{b_set}/discard"))
    );
    assert_eq!(s, 200);
    let row = sqlx::query(&format!(
        "SELECT sort_key, source FROM gtfs_trip WHERE gtfs_id = '{FEED}' AND trip_id = 'R1-EVE'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (
            row.get::<i32, _>("sort_key"),
            row.get::<String, _>("source")
        ),
        (4, "editor".into())
    );

    // ---- a stop added to pattern 1 carries its profile over
    let (_, detail, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R1"))
    );
    let profile_hash = detail["profiles"][0]["hash"].as_str().unwrap().to_string();
    let d = open_set!(&editor_c, "add EIGHT between TWO and THREE");
    let (insert, b) = added!(
        &editor_c,
        &d,
        json!({"entity": "route_stops", "op": "replace", "entity_key": "R1", "after": {
               "base_rows_hash": detail["rows_hash"], "rows": [
               {"stop_id": "S1", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": ""},
               {"stop_id": "S2", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": ""},
               {"stop_id": "S8", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": ""},
               {"stop_id": "S3", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": ""},
               {"stop_id": "S4", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": ""}]}})
    );
    assert_eq!(findings_of(&b, insert), vec!["warning:timing_interpolated"]);
    let (_, preview, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{d}/preview/routes/R1"))
    );
    let carried = preview["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pattern_key"] == 1)
        .unwrap()
        .clone();
    // S8 sits halfway between S2 and S3: kept stops keep their offsets, the new
    // one arrives halfway through the 130 s of running and waits the median 20 s
    assert_eq!(
        carried["arrival_s"],
        json!([0, 150, 225, 300, 450]),
        "{carried}"
    );
    assert_eq!(carried["departure_s"], json!([20, 170, 245, 320, 450]));
    assert_eq!(carried["source"], "interpolated");
    // a profile change based on the old profile conflicts once this commits
    let e = open_set!(&editor_c, "slower peak");
    added!(
        &editor_c,
        &e,
        json!({"entity": "timing_profile", "op": "replace", "entity_key": "R1", "after": {
               "pattern_key": 1, "profile_key": 1, "base_hash": profile_hash,
               "arrival_s": [0, 180, 360, 540], "departure_s": [20, 200, 380, 540]}})
    );
    commit!(&d);
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{e}/submit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    assert_eq!(
        b["error"]["details"]["conflicts"][0]["entity"],
        "timing_profile"
    );
    call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{e}/discard"))
    );

    // ---- a stop only the short turn calls at is still in use; a merge
    // switches it on every stop order
    let f = open_set!(&editor_c, "delete SEVEN");
    let (del, b) = added!(
        &editor_c,
        &f,
        json!({"entity": "stop", "op": "delete", "entity_key": "S7", "after": null})
    );
    assert_eq!(findings_of(&b, del), vec!["error:stop_in_use"]);
    call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{f}/discard"))
    );
    let (_, stop, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/stops/S7"))
    );
    assert_eq!(stop["routes"][0]["pattern_key"], 2, "{stop}");
    let g = open_set!(&editor_c, "merge SEVEN into SIX");
    let (merge, b) = added!(
        &editor_c,
        &g,
        json!({"entity": "stop", "op": "merge", "entity_key": "S7", "after": {"into_stop_id": "S6"}})
    );
    assert_eq!(
        change_of(&b, merge)["before"]["affected"],
        json!([{"route_id": "R1", "short_name": "BLUE", "sequences": [], "pattern_keys": [2]}])
    );
    commit!(&g);
    assert_eq!(
        scalar_i64(&pool, format!("SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND pattern_key = 2 AND stop_id = 'S6'")).await,
        1
    );

    // ---- what cannot go while something uses it, and what can once it does not
    let h = open_set!(&editor_c, "clear R1's timetable");
    let refused = [
        added!(&editor_c, &h, json!({"entity": "service", "op": "delete", "entity_key": "WK", "after": null})).0,
        added!(&editor_c, &h, json!({"entity": "pattern", "op": "delete", "entity_key": "R1", "after": {"pattern_key": 2}})).0,
        added!(&editor_c, &h, json!({"entity": "pattern", "op": "delete", "entity_key": "R1", "after": {"pattern_key": 1}})).0,
        added!(&editor_c, &h, json!({"entity": "timing_profile", "op": "delete", "entity_key": "R1",
                                     "after": {"pattern_key": 2, "profile_key": 1}})).0,
    ];
    let (_, b, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{h}")));
    let codes: Vec<Vec<String>> = refused.iter().map(|c| findings_of(&b, *c)).collect();
    assert_eq!(
        codes,
        vec![
            vec!["error:service_in_use".to_string()],
            vec!["error:pattern_in_use".to_string()],
            vec!["error:pattern_one".to_string()],
            vec!["error:profile_in_use".to_string()],
        ]
    );
    for c in refused {
        call!(
            &app,
            editor_c.req("DELETE", &format!("/change-sets/{h}/changes/{c}"))
        );
    }
    let (_, live, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R1/trips"))
    );
    added!(
        &editor_c,
        &h,
        json!({"entity": "route_trips", "op": "replace", "entity_key": "R1",
               "after": {"base_trips_hash": live["trips_hash"], "trips": []}})
    );
    for change in [
        json!({"entity": "timing_profile", "op": "delete", "entity_key": "R1", "after": {"pattern_key": 1, "profile_key": 1}}),
        json!({"entity": "timing_profile", "op": "delete", "entity_key": "R1", "after": {"pattern_key": 2, "profile_key": 1}}),
        json!({"entity": "pattern", "op": "delete", "entity_key": "R1", "after": {"pattern_key": 2}}),
        json!({"entity": "service", "op": "delete", "entity_key": "WK", "after": null}),
        json!({"entity": "service", "op": "update", "entity_key": "STUB", "after": {"days": {"sunday": true}, "label": "Sundays"}}),
    ] {
        let (id, b) = added!(&editor_c, &h, change.clone());
        assert!(findings_of(&b, id).is_empty(), "{change}: {b}");
    }
    commit!(&h);
    for (table, n) in [
        ("gtfs_trip", 0),
        ("gtfs_timing_profile", 0),
        ("gtfs_service", 1),
    ] {
        assert_eq!(
            scalar_i64(
                &pool,
                format!("SELECT count(*) FROM {table} WHERE gtfs_id = '{FEED}'")
            )
            .await,
            n,
            "{table}"
        );
    }
    assert_eq!(
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_pattern WHERE gtfs_id = '{FEED}' AND route_id = 'R1'"
            )
        )
        .await,
        1
    );

    // ---- the feed's trips settings, an admin's through a draft like its data source
    let k = open_set!(&admin, "serve trips from the tables");
    let (need, b) = added!(
        &admin,
        &k,
        json!({"entity": "feed_config", "op": "update", "entity_key": FEED, "after": {"trips_source": "db"}})
    );
    assert_eq!(findings_of(&b, need), vec!["error:trips_need_db"]);
    let (s, b, _) = call!(
        &app,
        admin
            .req("PUT", &format!("/change-sets/{k}/changes/{need}"))
            .set_json(
                json!({"after": {"data_source": "db", "trips_source": "db", "default_run_s": 100}})
            )
    );
    assert_eq!(s, 200, "{b}");
    assert!(findings_of(&b, need).is_empty(), "{b}");
    let (schedule, _) = added!(
        &admin,
        &k,
        json!({"entity": "route", "op": "update", "entity_key": "R2", "after": {"schedule_source": "editor"}})
    );
    let _ = schedule;
    let (s, b, _) = call!(&app, admin.req("POST", &format!("/change-sets/{k}/submit")));
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{k}/approve"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{k}/commit"))
    );
    assert_eq!(s, 200, "{b}");
    assert!(b["feed_version"].as_i64().unwrap() > version_a);
    let (_, config, _) = call!(&app, editor_c.req("GET", &format!("/feeds/{FEED}/config")));
    assert_eq!(
        (
            &config["data_source"],
            &config["trips_source"],
            &config["default_run_s"]
        ),
        (&json!("db"), &json!("db"), &json!(100))
    );
    let audited: Vec<(String, String)> = sqlx::query(&format!(
        "SELECT action, coalesce(detail->>'setting', 'data_source') AS setting FROM gtfs_audit_log \
         WHERE change_set_id = '{k}' AND action IN ('feed_data_source_changed', 'feed_config_changed') \
         ORDER BY audit_id"
    ))
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get("action"), r.get("setting")))
    .collect();
    assert_eq!(
        audited,
        vec![
            (
                "feed_data_source_changed".to_string(),
                "data_source".to_string()
            ),
            ("feed_config_changed".into(), "trips_source".into()),
            ("feed_config_changed".into(), "default_run_s".into()),
        ]
    );
    let (_, route, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R2"))
    );
    assert_eq!(route["schedule_source"], "editor");
    // data_source back to preprocessed while trips come from the tables: refused
    let l = open_set!(&admin, "back to preprocessed");
    let (back, b) = added!(
        &admin,
        &l,
        json!({"entity": "feed_config", "op": "update", "entity_key": FEED, "after": {"data_source": "preprocessed"}})
    );
    assert_eq!(findings_of(&b, back), vec!["error:trips_need_db"]);

    exec(&pool, &clear_feed()).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// md5 in hex, for the expected public pattern id.
fn md5_hex(text: &str) -> String {
    use md5::{Digest, Md5};
    hex::encode(Md5::digest(text.as_bytes()))
}
