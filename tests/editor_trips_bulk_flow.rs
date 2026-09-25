//! Trips, timing profiles and service calendars imported in bulk
//! (docs/gtfs-editor.md section 16.5), and a big draft answered a page of
//! changes at a time, end to end against a real Postgres holding the editor
//! schema (db/gtfs_editor/0001..0019):
//!
//! - `services`: one row per date, each repeating its service's days; rows of
//!   a service that disagree are refused, a service there is becomes an update,
//!   one that never runs a warning;
//! - `route_stops` with a `pattern_key`, a headsign and boarding fields: a new
//!   stop order;
//! - `timing_profiles`: one stop of one profile a row, `stop_sequence` 1 to n
//!   over the pattern's served stops once the draft applies - a gap, a duplicate
//!   and a pattern the route lacks are errors;
//! - `route_trips`: one trip a row, grouped per route in upload order, ids
//!   minted in the real run, checked against the patterns, profiles and
//!   services the draft makes, a trip id another route still holds taken - and
//!   free once the draft moves it;
//! - `GET /change-sets/{id}`: 200 changes a page, `change_count` and the
//!   validation summary on the first page, later pages without a replay.
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

const AUD: &str = "gtfs.editor-trips-bulk-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_trips_bulk_test_feed";
const ADMIN: &str = "admin@editor-trips-bulk-test.invalid";
const EDITOR: &str = "editor@editor-trips-bulk-test.invalid";
const APPROVER: &str = "approver@editor-trips-bulk-test.invalid";
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
/// line, route R1 calling at S1..S4, R2 at S5, S6, and R3 - which already runs
/// a trip, `R3-LIVE`, on a live service - at S1, S2.
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
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type) \
         VALUES ('{FEED}', 'R3', 'RED', 'ONE To TWO', 1)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type) VALUES \
         ('{FEED}', 'R3', 1, 'S1', 'NEW STOP'), ('{FEED}', 'R3', 2, 'S2', 'NEW STOP')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_service (gtfs_id, service_id, monday, tuesday, wednesday, thursday, friday, \
                                   saturday, sunday) \
         VALUES ('{FEED}', 'LIVE', true, true, true, true, true, true, true)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_trip (gtfs_id, trip_id, route_id, pattern_key, service_id, ref_s, sort_key, source) \
         VALUES ('{FEED}', 'R3-LIVE', 'R3', 1, 'LIVE', 21600, 1, 'import')"
    ));
    s
}

fn state(pool: &PgPool, signer: &TestSigner, admin: &str) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-trips-bulk-{}", crypto::random_token()));
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

/// Rows of an upload, and what became of each: `(status, [codes])`.
fn outcome(res: &Value) -> Vec<(String, Vec<String>)> {
    res["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["status"].as_str().unwrap().to_string(),
                r["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m["code"].as_str().unwrap().to_string())
                    .collect(),
            )
        })
        .collect()
}

#[actix_web::test]
async fn trips_profiles_and_services_are_imported_in_bulk_and_big_drafts_page() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("trips-bulk-test-key");
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

    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "import the timetable"}))
    );
    assert_eq!(s, 201, "{b}");
    let set = b["change_set_id"].as_str().unwrap().to_string();
    let bulk = |kind: &str, rows: Value, dry_run: bool| {
        editor_c
            .req("POST", &format!("/change-sets/{set}/bulk"))
            .set_json(json!({"kind": kind, "rows": rows, "dry_run": dry_run}))
    };

    // ---- services: one row per date, each repeating its service's days
    let week = |id: &str, date: Option<(&str, i32)>, monday: &str| {
        let mut r = json!({"action": if id == "LIVE" { "update" } else { "add" },
                           "service_id": id, "monday": monday, "tuesday": "1", "wednesday": 1,
                           "thursday": true, "friday": "1", "saturday": "0", "sunday": "",
                           "start_date": "2026-09-01", "end_date": "2026-12-31", "label": "weekdays"});
        if let Some((d, k)) = date {
            r["date"] = json!(d);
            r["exception_type"] = json!(k);
        }
        r
    };
    let rows = json!([
        week("WK", Some(("2026-10-02", 2)), "1"),
        week("WK", Some(("2026-10-03", 1)), "1"),
        {"action": "add", "service_id": "SUN", "sunday": 1},
        {"action": "add", "service_id": "STUB"},
        {"action": "add", "service_id": "BAD", "date": "2026-10-02"},
        week("ODD", None, "1"),
        week("ODD", None, "0"),
        week("LIVE", None, "1"),
    ]);
    let (s, res, _) = call!(&app, bulk("services", rows.clone(), true));
    assert_eq!(s, 200, "{res}");
    assert_eq!(
        outcome(&res),
        vec![
            ("ok".to_string(), vec![]),
            ("ok".into(), vec![]),
            ("ok".into(), vec![]),
            ("warning".into(), vec!["service_never_runs".to_string()]),
            ("error".into(), vec!["invalid_row".into()]),
            ("error".into(), vec!["invalid_row".into()]),
            ("error".into(), vec!["invalid_row".into()]),
            ("ok".into(), vec![]),
        ]
    );
    // a service there is becomes an update, with its version as the base
    let live = res["changes_preview"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["entity_key"] == "LIVE")
        .unwrap()
        .clone();
    assert_eq!(live["op"], "update", "{live}");
    let mut fixed = rows.as_array().unwrap().clone();
    fixed.remove(6);
    fixed.remove(5);
    fixed.remove(4);
    let (s, res, _) = call!(&app, bulk("services", json!(fixed), false));
    assert_eq!(s, 200, "{res}");
    assert_eq!(res["summary"]["changes"], 4);
    let wk = res["change_set"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["entity_key"] == "WK")
        .unwrap()
        .clone();
    assert_eq!(
        wk["after"]["dates"],
        json!([{"date": "2026-10-02", "exception_type": 2}, {"date": "2026-10-03", "exception_type": 1}])
    );
    assert_eq!(wk["after"]["days"]["saturday"], false);

    // ---- a second stop order for R1, with a headsign of its own
    let (s, res, _) = call!(
        &app,
        bulk(
            "route_stops",
            json!([
                {"action": "add", "route_id": "R1", "pattern_key": 2, "sequence": 1, "stop_id": "S1", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": ""},
                {"action": "add", "route_id": "R1", "pattern_key": "2", "sequence": 2, "stop_id": "S2", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": "",
                 "stop_headsign": "Towards TWO", "pickup_type": 1},
                {"action": "add", "route_id": "R1", "pattern_key": 2, "sequence": 3, "stop_id": "S3", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": ""}
            ]),
            false
        )
    );
    assert_eq!(s, 200, "{res}");
    assert_eq!(res["summary"]["errors"], 0, "{res}");
    let turn = &res["changes_preview"][0]["after"];
    assert_eq!(
        (
            &turn["pattern_key"],
            &turn["base_rows_hash"],
            &turn["rows"][1]["stop_headsign"],
            &turn["rows"][1]["pickup_type"]
        ),
        (&json!(2), &json!(NOTHING), &json!("Towards TWO"), &json!(1))
    );

    // ---- timing profiles, one stop of one profile a row, for the stop orders
    // the draft now has
    let stop = |route: &str, pattern: i32, profile: i32, seq: i32, a: i32, d: i32| {
        json!({"action": "add", "route_id": route, "pattern_key": pattern, "profile_key": profile,
               "stop_sequence": seq, "arrival_offset": a, "departure_offset": d})
    };
    let rows = json!([
        stop("R1", 1, 1, 2, 150, 170),
        stop("R1", 1, 1, 1, 0, 20),
        stop("R1", 1, 1, 3, 300, 320),
        stop("R1", 1, 1, 4, 450, 450),
        stop("R1", 2, 1, 1, 0, 20),
        stop("R1", 2, 1, 2, 150, 170),
        stop("R1", 2, 1, 3, 300, 300),
        stop("R2", 1, 1, 1, 0, 0),
        stop("R2", 1, 1, 3, 100, 100),
        stop("R3", 1, 1, 1, 0, 0),
        stop("R3", 1, 1, 1, 5, 5),
        stop("R3", 3, 1, 1, 0, 0),
    ]);
    let (s, res, _) = call!(&app, bulk("timing_profiles", rows.clone(), true));
    assert_eq!(s, 200, "{res}");
    let got = outcome(&res);
    for i in 0..7 {
        assert_eq!(got[i].0, "ok", "row {}: {res}", i + 1);
    }
    // a gap in the stop sequence, two rows for one stop, a pattern R3 lacks
    assert!(
        got[7].1.contains(&"profile_length_mismatch".to_string()),
        "{got:?}"
    );
    assert!(
        got[9].1.contains(&"duplicate_in_upload".to_string()),
        "{got:?}"
    );
    assert!(
        got[11].1.contains(&"pattern_not_found".to_string()),
        "{got:?}"
    );
    let first_two: Vec<Value> = rows.as_array().unwrap()[..7].to_vec();
    let (s, res, _) = call!(&app, bulk("timing_profiles", json!(first_two), false));
    assert_eq!(s, 200, "{res}");
    let peak = &res["changes_preview"][0]["after"];
    // sorted by stop_sequence, based on nothing, from an import
    assert_eq!(
        (
            &peak["arrival_s"],
            &peak["departure_s"],
            &peak["base_hash"],
            &peak["source"]
        ),
        (
            &json!([0, 150, 300, 450]),
            &json!([20, 170, 320, 450]),
            &json!(NOTHING),
            &json!("import")
        )
    );

    // ---- trips, one a row, in upload order per route
    let rows = json!([
        {"action": "add", "route_id": "R1", "pattern_key": 1, "profile_key": 1, "service_id": "WK", "direction_id": 0, "start_time": "06:00:00"},
        {"action": "add", "route_id": "R1", "trip_id": "R1-TURN", "pattern_key": "2", "profile_key": "1", "service_id": "WK", "start_time": "06:30", "shape_id": "SH2"},
        {"action": "add", "route_id": "R2", "trip_id": "R2-EVE", "pattern_key": 1, "service_id": "SUN", "start_time": "18:00:00",
         "frequencies": "[{\"start_time\": \"18:00:00\", \"end_time\": \"22:00:00\", \"headway_s\": 600}]",
         "source_ref": {"schedule_number": "N-1"}},
        {"action": "add", "route_id": "R1", "trip_id": "R1-TURN", "pattern_key": 1, "service_id": "WK", "start_time": "07:00:00"},
        {"action": "add", "route_id": "R2", "trip_id": "R2-X", "pattern_key": 1, "service_id": "NOPE", "start_time": "09:00:00"},
        {"action": "add", "route_id": "R2", "trip_id": "R3-LIVE", "pattern_key": 1, "service_id": "SUN", "start_time": "09:00:00"},
        {"action": "add", "route_id": "R2", "trip_id": "R2-Y", "pattern_key": 1, "service_id": "SUN", "start_time": "5.10"},
    ]);
    let (s, res, _) = call!(&app, bulk("route_trips", rows.clone(), true));
    assert_eq!(s, 200, "{res}");
    let got = outcome(&res);
    assert_eq!(got[0].0, "ok", "{res}");
    assert_eq!(got[2].0, "ok", "{res}");
    assert!(
        got[1].1.contains(&"duplicate_in_upload".to_string()),
        "{got:?}"
    );
    assert!(
        got[3].1.contains(&"duplicate_in_upload".to_string()),
        "{got:?}"
    );
    assert!(
        got[4].1.contains(&"service_not_found".to_string()),
        "{got:?}"
    );
    // R3 runs R3-LIVE, and this draft leaves it there
    assert!(got[5].1.contains(&"trip_id_taken".to_string()), "{got:?}");
    assert_eq!(got[6].1, vec!["invalid_time".to_string()]);
    let good: Vec<Value> = [0usize, 1, 2].iter().map(|i| rows[*i].clone()).collect();
    let (s, res, _) = call!(&app, bulk("route_trips", json!(good), false));
    assert_eq!(s, 200, "{res}");
    assert_eq!(res["summary"]["changes"], 2, "{res}");
    let r1 = res["change_set"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["entity"] == "route_trips" && c["entity_key"] == "R1")
        .unwrap()
        .clone();
    let minted = r1["after"]["trips"][0]["trip_id"].as_str().unwrap();
    assert!(minted.starts_with("R1-ed-"), "{r1}");
    assert_eq!(r1["after"]["trips"][1]["trip_id"], "R1-TURN");
    assert_eq!(r1["after"]["base_trips_hash"], NOTHING);
    // a route whose trips the draft already replaces says so
    let (_, res, _) = call!(&app, bulk("route_trips", json!([rows[0].clone()]), true));
    assert!(
        outcome(&res)[0]
            .1
            .contains(&"route_already_in_draft".to_string()),
        "{res}"
    );
    // once the draft moves R3's trip off it, its id is free for R2
    let (_, detail, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R3/trips"))
    );
    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(
                json!({"entity": "route_trips", "op": "replace", "entity_key": "R3",
                             "after": {"base_trips_hash": detail["trips_hash"], "trips": []}})
            )
    );
    assert_eq!(s, 201, "{b}");
    // the draft gives R2 its trips already, so this one replaces them
    let mut again = rows[5].clone();
    again["action"] = json!("update");
    let (_, res, _) = call!(&app, bulk("route_trips", json!([again]), true));
    assert_eq!(
        outcome(&res)[0],
        (
            "warning".to_string(),
            vec!["route_already_in_draft".to_string()]
        ),
        "{res}"
    );

    let (s, b, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{set}")));
    assert_eq!(s, 200);
    assert_eq!(b["validation_summary"]["errors"], 0, "{b}");
    assert!(b["can_submit"].as_bool().unwrap(), "{b}");
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set}/submit"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set}/approve"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set}/commit"))
    );
    assert_eq!(s, 200, "{b}");
    for (sql, n) in [
        (format!("SELECT count(*) FROM gtfs_service WHERE gtfs_id = '{FEED}'"), 4),
        (format!("SELECT count(*) FROM gtfs_service_date WHERE gtfs_id = '{FEED}'"), 2),
        (format!("SELECT count(*) FROM gtfs_timing_profile WHERE gtfs_id = '{FEED}'"), 2),
        (format!("SELECT count(*) FROM gtfs_trip WHERE gtfs_id = '{FEED}'"), 3),
        (format!("SELECT count(*) FROM gtfs_trip WHERE gtfs_id = '{FEED}' AND source = 'import'"), 3),
        (format!("SELECT count(*) FROM gtfs_frequency WHERE gtfs_id = '{FEED}' AND trip_id = 'R2-EVE'"), 1),
        (format!("SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND pattern_key = 2 AND stop_headsign = 'Towards TWO'"), 1),
    ] {
        assert_eq!(scalar_i64(&pool, &sql).await, n, "{sql}");
    }

    // ---- a big draft answers a page of changes at a time
    let (_, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "many services"}))
    );
    let big = b["change_set_id"].as_str().unwrap().to_string();
    let many: Vec<Value> = (0..450)
        .map(|i| json!({"action": "add", "service_id": format!("S{i:03}"), "monday": 1}))
        .collect();
    let (s, res, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{big}/bulk"))
            .set_json(json!({"kind": "services", "rows": many, "dry_run": false}))
    );
    assert_eq!(s, 200, "{res}");
    // the bulk response carries the set's first page too
    assert_eq!(res["change_set"]["changes"].as_array().unwrap().len(), 200);
    let (s, first, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{big}")));
    assert_eq!(s, 200, "{first}");
    assert_eq!(first["change_count"], 450);
    assert_eq!(first["changes"].as_array().unwrap().len(), 200);
    assert_eq!(first["changes"][0]["entity_key"], "S000");
    assert_eq!(
        first["validation_summary"],
        json!({"errors": 0, "warnings": 0})
    );
    assert!(first["can_submit"].as_bool().unwrap());
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    let (s, second, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{big}?cursor={cursor}"))
    );
    assert_eq!(s, 200, "{second}");
    assert_eq!(second["changes"][0]["entity_key"], "S200");
    // a later page is its slice of changes, without a replay of the draft
    assert!(
        second.get("validation").is_none() && second.get("can_submit").is_none(),
        "{second}"
    );
    assert_eq!(second["change_count"], 450);
    let cursor = second["next_cursor"].as_str().unwrap().to_string();
    let (_, third, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{big}?cursor={cursor}"))
    );
    assert_eq!(third["changes"].as_array().unwrap().len(), 50);
    assert!(third["next_cursor"].is_null());
    let (_, all, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{big}?limit=500"))
    );
    assert_eq!(all["changes"].as_array().unwrap().len(), 450);
    assert!(all["next_cursor"].is_null());
    for (query, code) in [
        ("limit=0", "invalid_limit"),
        ("limit=501", "invalid_limit"),
        ("cursor=nonsense", "invalid_cursor"),
    ] {
        let (s, b, _) = call!(
            &app,
            editor_c.req("GET", &format!("/change-sets/{big}?{query}"))
        );
        assert_eq!((s, code_of(&b)), (400, code), "{query}: {b}");
    }

    exec(&pool, &clear_feed()).await;
    let _ = std::fs::remove_dir_all(&dir);
}
