//! Route reviews (docs/gtfs-editor.md section 14) end to end against a real
//! Postgres holding the editor schema (db/gtfs_editor/0001..0014): the queue in
//! rank order with its filters and summary; a detail whose problems are
//! recomputed from the live rows rather than read from the load; a fix refused
//! until the draft really changes the route, then tied to it, taken back by
//! removing the change and by discarding the draft, and finally released
//! through submit, approval by someone else and commit; confirm, reject, reopen
//! and a note.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local. The flow uses its own feed and accounts and removes its rows
//! afterwards. See scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

const AUD: &str = "gtfs.route-review-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_route_review_test_feed";
const ADMIN: &str = "admin@route-review-test.invalid";
const EDITOR: &str = "editor@route-review-test.invalid";
const APPROVER: &str = "approver@route-review-test.invalid";
const VIEWER: &str = "viewer@route-review-test.invalid";

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

/// Reviews go first: an approved review holds a reference to its draft.
fn clear_feed(feed: &str) -> Vec<String> {
    vec![
        format!("DELETE FROM gtfs_route_review WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_position_review WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{feed}'"),
    ]
}

fn reset_accounts(emails: &[&str]) -> Vec<String> {
    let list = emails
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        format!(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, \
             totp_last_step = NULL, status = 'active' WHERE email IN ({list})"
        ),
        format!(
            "DELETE FROM gtfs_editor_session WHERE user_id IN \
             (SELECT user_id FROM gtfs_editor_user WHERE email IN ({list}))"
        ),
    ]
}

fn state(pool: &PgPool, signer: &TestSigner, admin: &str) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-route-review-{}", crypto::random_token()));
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
            ops_pool: None,
        },
    )
    .unwrap();
    (st, dir)
}

fn codes(list: &Value) -> Vec<String> {
    list.as_array()
        .map(|a| {
            a.iter()
                .map(|x| x["code"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn ids(list: &Value, field: &str) -> Vec<String> {
    list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x[field].as_str().unwrap_or("").to_string())
        .collect()
}

// ---------------------------------------------------------------- the flow

fn seed() -> Vec<String> {
    let mut s = clear_feed(FEED);
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{FEED}', 'Route review test feed')"
    ));
    // A..E lie on one straight line; DET is 5 km off it, so a route calling
    // there takes a measurable way round
    let stops = [
        ("A", "STOP A", 13.00, 80.20),
        ("B", "STOP B", 13.01, 80.20),
        ("C", "STOP C", 13.02, 80.20),
        ("D", "STOP D", 13.03, 80.20),
        ("E", "STOP E", 13.04, 80.20),
        ("DET", "DETOUR STOP", 13.02, 80.25),
        ("REV", "REVIEWED STOP", 13.05, 80.20),
    ];
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES {}",
        stops
            .iter()
            .map(|(id, name, lat, lon)| format!(
                "('{FEED}', '{id}', 'code-{id}', '{name}', {lat}, {lon})"
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    // BUSY has a shape and a clean stop list; SHORT has neither; DUP lists a
    // stop twice running; WIDE goes the long way round; GONE is deleted
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id, \
                                 encoded_polyline, deleted) VALUES \
         ('{FEED}', 'BUSY', '600', 'A To E', 'TESTAG', 'abcdef', false), \
         ('{FEED}', 'SHORT', '598', 'A To B', 'TESTAG', NULL, false), \
         ('{FEED}', 'DUP', '543', 'A To E via C twice', 'TESTAG', 'abcdef', false), \
         ('{FEED}', 'WIDE', '546', 'A To E via the detour', 'TESTAG', 'abcdef', false), \
         ('{FEED}', 'GONE', '999', 'Withdrawn', 'TESTAG', NULL, true)"
    ));
    let rows: [(&str, i32, &str); 18] = [
        ("BUSY", 1, "A"),
        ("BUSY", 2, "B"),
        ("BUSY", 3, "C"),
        ("BUSY", 4, "D"),
        ("BUSY", 5, "E"),
        ("SHORT", 1, "A"),
        ("SHORT", 2, "B"),
        ("DUP", 1, "A"),
        ("DUP", 2, "B"),
        ("DUP", 3, "C"),
        ("DUP", 4, "C"),
        ("DUP", 5, "D"),
        ("DUP", 6, "E"),
        ("WIDE", 1, "A"),
        ("WIDE", 2, "B"),
        ("WIDE", 3, "DET"),
        ("WIDE", 4, "D"),
        ("WIDE", 5, "E"),
    ];
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, \
                                      stage_name) VALUES {}",
        rows.iter()
            .map(|(r, q, st)| format!(
                "('{FEED}', '{r}', {q}, '{st}', 'NEW STOP', {q}, 'STAGE {q}')"
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    // a coordinate review open on a stop BUSY calls at: the route detail counts it
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, \
                                      stage_name) VALUES ('{FEED}', 'BUSY', 6, 'REV', 'NEW STOP', 6, 'STAGE 6')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_position_review (gtfs_id, batch, stop_id, original_stop_id, stop_name, \
                                           reason, lat, lon, status) VALUES \
         ('{FEED}', 'rr-test', 'REV', 'REV', 'REVIEWED STOP', 'off its routes', 13.05, 80.20, 'pending')"
    ));
    // the queue as a load writes it: rank 1 is the most booked
    let reason = |code: &str| json!([{"code": code, "severity": "warning", "message": code}]);
    let queued: [(&str, &str, i32, f64, Value); 4] = [
        (
            "BUSY",
            "600",
            1,
            48414.0,
            reason("stops_under_position_review"),
        ),
        ("SHORT", "598", 2, 39854.0, reason("no_polyline")),
        ("DUP", "543", 3, 36503.0, reason("repeated_stop")),
        ("WIDE", "546", 4, 25681.0, reason("worst_detour")),
    ];
    s.push(format!(
        "INSERT INTO gtfs_route_review (gtfs_id, batch, route_id, route_short_name, \
             route_long_name, queue_rank, measure, measure_value, measure_window, reasons, \
             evidence, status) VALUES {}",
        queued
            .iter()
            .map(|(rid, num, rank, value, reasons)| format!(
                "('{FEED}', 'rr-test-batch', '{rid}', '{num}', 'Route {num}', {rank}, 'bookings', \
                  {value}, '2026-08-20..2026-09-19', '{reasons}'::jsonb, \
                  '{{\"source\": \"test\"}}'::jsonb, 'pending')"
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    s.extend(reset_accounts(&[ADMIN, EDITOR, APPROVER, VIEWER]));
    s
}

async fn review_id(pool: &PgPool, route: &str) -> i64 {
    sqlx::query(&format!(
        "SELECT review_id FROM gtfs_route_review WHERE gtfs_id = '{FEED}' AND route_id = '{route}'"
    ))
    .fetch_one(pool)
    .await
    .unwrap()
    .get::<i64, _>(0)
}

#[actix_web::test]
async fn route_review_queue_fix_and_release() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("route-review-test-key");
    let (st, dir) = state(&pool, &signer, ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    // ---- accounts
    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
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
    let mut viewer = Caller {
        signer: &signer,
        email: VIEWER.into(),
        session: None,
    };
    let sign_in = |c: &Caller| c.req("POST", "/auth/totp/enroll");
    let (s, b, _) = call!(&app, sign_in(&admin));
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
    for (email, role) in [
        (EDITOR, "editor"),
        (APPROVER, "approver"),
        (VIEWER, "viewer"),
    ] {
        let (s, b, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    }
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    for (email, role) in [
        (EDITOR, "editor"),
        (APPROVER, "approver"),
        (VIEWER, "viewer"),
    ] {
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
    }
    for c in [&mut editor_c, &mut approver, &mut viewer] {
        let (s, b, _) = call!(&app, sign_in(c));
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

    let (busy, short, dup, wide) = (
        review_id(&pool, "BUSY").await,
        review_id(&pool, "SHORT").await,
        review_id(&pool, "DUP").await,
        review_id(&pool, "WIDE").await,
    );
    let get = |c: &Caller, id: i64| c.req("GET", &format!("/route-reviews/{id}"));
    let post = |c: &Caller, id: i64, action: &str, body: Value| {
        c.req("POST", &format!("/route-reviews/{id}/{action}"))
            .set_json(body)
    };
    let status_of = |id: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query(&format!(
                "SELECT status FROM gtfs_route_review WHERE review_id = {id}"
            ))
            .fetch_one(&pool)
            .await
            .unwrap()
            .get::<String, _>(0)
        }
    };

    // ======================================================== the queue
    let (s, list, _) = call!(
        &app,
        viewer.req("GET", &format!("/feeds/{FEED}/route-reviews"))
    );
    assert_eq!(s, 200, "{list}");
    assert_eq!(
        ids(&list, "route_id"),
        vec!["BUSY", "SHORT", "DUP", "WIDE"],
        "the busiest route is reviewed first"
    );
    let first = &list["items"][0];
    for field in [
        "review_id",
        "route_id",
        "route_short_name",
        "queue_rank",
        "measure",
        "measure_value",
        "measure_window",
        "reasons",
        "evidence",
        "status",
        "batch",
    ] {
        assert!(!first[field].is_null(), "{field} is missing from {first}");
    }
    assert_eq!(first["measure"], "bookings");
    assert_eq!(first["measure_value"], 48414.0);

    // the reason filter, and a reason nobody defined
    let (s, only, _) = call!(
        &app,
        viewer.req(
            "GET",
            &format!("/feeds/{FEED}/route-reviews?reason=repeated_stop")
        )
    );
    assert_eq!(s, 200, "{only}");
    assert_eq!(ids(&only, "route_id"), vec!["DUP"]);
    let (s, b, _) = call!(
        &app,
        viewer.req(
            "GET",
            &format!("/feeds/{FEED}/route-reviews?reason=made_up_code")
        )
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_reason"), "{b}");
    let (s, b, _) = call!(
        &app,
        viewer.req(
            "GET",
            &format!("/feeds/{FEED}/route-reviews?status=nonsense")
        )
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_status"), "{b}");

    // search by route id and by number
    let (_, by_id, _) = call!(
        &app,
        viewer.req("GET", &format!("/feeds/{FEED}/route-reviews?q=WIDE"))
    );
    assert_eq!(ids(&by_id, "route_id"), vec!["WIDE"]);
    let (_, by_num, _) = call!(
        &app,
        viewer.req("GET", &format!("/feeds/{FEED}/route-reviews?q=598"))
    );
    assert_eq!(ids(&by_num, "route_id"), vec!["SHORT"]);

    let (s, sum, _) = call!(
        &app,
        viewer.req("GET", &format!("/feeds/{FEED}/route-reviews/summary"))
    );
    assert_eq!(s, 200, "{sum}");
    assert_eq!(sum["pending"], 4);
    assert_eq!(sum["approved"], 0);
    assert_eq!(sum["reasons"]["repeated_stop"], 1);
    assert_eq!(sum["reasons"]["no_polyline"], 1);
    assert_eq!(sum["batch"]["measure"], "bookings");
    assert_eq!(sum["batch"]["reviews"], 4);

    // ======================================================== detail: judged live
    let (s, d, _) = call!(&app, get(&viewer, short));
    assert_eq!(s, 200, "{d}");
    assert_eq!(d["route"]["route_id"], "SHORT");
    assert_eq!(d["stop_count"], 2);
    assert_eq!(d["has_polyline"], false);
    let found = codes(&d["problems"]);
    assert!(found.contains(&"no_polyline".into()), "{found:?}");
    assert!(found.contains(&"short_stop_list".into()), "{found:?}");

    let (_, d, _) = call!(&app, get(&viewer, dup));
    assert_eq!(codes(&d["problems"]), vec!["repeated_stop"], "{d}");

    let (_, d, _) = call!(&app, get(&viewer, wide));
    assert_eq!(codes(&d["problems"]), vec!["worst_detour"], "{d}");
    assert!(
        d["context"]["worst_detours"][0]["stop_id"] == "DET",
        "the cleanup context names the same stop: {}",
        d["context"]
    );

    // the busiest route's own list is clean; what it inherits is a stop under
    // coordinate review, which the context also lists
    let (_, d, _) = call!(&app, get(&viewer, busy));
    assert_eq!(
        codes(&d["problems"]),
        vec!["stops_under_position_review"],
        "{d}"
    );
    assert_eq!(d["context"]["stops_with_reviews"][0]["stop_id"], "REV");

    // ======================================================== a viewer may not act
    let (s, b, _) = call!(&app, post(&viewer, busy, "confirm", json!({})));
    assert_eq!(s, 403, "{b}");

    // ======================================================== fix
    let (s, cs, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "fix the busiest routes"}))
    );
    assert_eq!(s, 201, "{cs}");
    let set = cs["change_set_id"].as_str().unwrap().to_string();

    // a draft that does not touch the route has no fix to record
    let (s, b, _) = call!(
        &app,
        post(&editor_c, dup, "fix", json!({"change_set_id": set}))
    );
    assert_eq!((s, code_of(&b)), (409, "no_change_for_route"), "{b}");
    assert_eq!(status_of(dup).await, "pending");

    // the fix itself is an ordinary route_stops change: DUP's repeated C goes
    let (_, route, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/DUP"))
    );
    let hash = route["rows_hash"].as_str().unwrap().to_string();
    let fixed_rows = json!([
        {"stop_id": "A", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "STAGE 1"},
        {"stop_id": "B", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "STAGE 2"},
        {"stop_id": "C", "stop_type": "NEW STOP", "stage_no": 3, "stage_name": "STAGE 3"},
        {"stop_id": "D", "stop_type": "NEW STOP", "stage_no": 5, "stage_name": "STAGE 5"},
        {"stop_id": "E", "stop_type": "NEW STOP", "stage_no": 6, "stage_name": "STAGE 6"},
    ]);
    let add_fix = |set: &str, hash: &str| {
        editor_c
            .req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(
                json!({"entity": "route_stops", "op": "replace", "entity_key": "DUP",
                             "after": {"base_rows_hash": hash, "rows": fixed_rows}}),
            )
    };
    let (s, ch, _) = call!(&app, add_fix(&set, &hash));
    assert_eq!(s, 201, "{ch}");
    let change_of = |set: &Value, route: &str| {
        set["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["entity_key"] == route)
            .unwrap_or_else(|| panic!("no change for {route} in {set}"))["change_id"]
            .as_i64()
            .unwrap()
    };
    let change_id = change_of(&ch, "DUP");

    let (s, d, _) = call!(
        &app,
        post(
            &editor_c,
            dup,
            "fix",
            json!({"change_set_id": set, "note": "the repeated C was a duplicated row"})
        )
    );
    assert_eq!(s, 200, "{d}");
    assert_eq!(d["status"], "approved");
    assert_eq!(d["change_set_id"].as_str().unwrap(), set);
    assert_eq!(d["change_id"], change_id);
    assert_eq!(d["reviewed_by_email"], EDITOR);
    assert_eq!(d["review_note"], "the repeated C was a duplicated row");

    // a second draft cannot also claim it
    let (s, other, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "another draft"}))
    );
    assert_eq!(s, 201, "{other}");
    let set_b = other["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(&editor_c, dup, "fix", json!({"change_set_id": set_b}))
    );
    assert_eq!((s, code_of(&b)), (409, "review_in_other_draft"), "{b}");
    assert_eq!(
        b["error"]["details"]["change_set_id"].as_str().unwrap(),
        set
    );

    // taking the change out of the draft leaves nothing to follow
    let (s, b, _) = call!(
        &app,
        editor_c.req("DELETE", &format!("/change-sets/{set}/changes/{change_id}"))
    );
    assert!(s == 200 || s == 204, "{s} {b}");
    assert_eq!(status_of(dup).await, "pending");

    // put it back, then discard the draft: pending again
    let (s, ch, _) = call!(&app, add_fix(&set, &hash));
    assert_eq!(s, 201, "{ch}");
    let (s, b, _) = call!(
        &app,
        post(&editor_c, dup, "fix", json!({"change_set_id": set}))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(status_of(dup).await, "approved");
    let (s, _, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{set}/discard"))
            .set_json(json!({}))
    );
    assert_eq!(s, 200);
    assert_eq!(status_of(dup).await, "pending");

    // ======================================================== released
    let (s, cs, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "release the fix"}))
    );
    assert_eq!(s, 201, "{cs}");
    let live = cs["change_set_id"].as_str().unwrap().to_string();
    let (s, ch, _) = call!(&app, add_fix(&live, &hash));
    assert_eq!(s, 201, "{ch}");
    let (s, b, _) = call!(
        &app,
        post(&editor_c, dup, "fix", json!({"change_set_id": live}))
    );
    assert_eq!(s, 200, "{b}");
    for (who, path) in [
        (&editor_c, format!("/change-sets/{live}/submit")),
        (&approver, format!("/change-sets/{live}/approve")),
        (&approver, format!("/change-sets/{live}/commit")),
    ] {
        let (s, b, _) = call!(&app, who.req("POST", &path).set_json(json!({})));
        assert_eq!(s, 200, "{path}: {b}");
    }
    assert_eq!(status_of(dup).await, "committed");
    // and the defect it was queued for is gone from the live rows
    let (_, d, _) = call!(&app, get(&viewer, dup));
    assert!(
        !codes(&d["problems"]).contains(&"repeated_stop".into()),
        "the committed fix shows on the review: {}",
        d["problems"]
    );
    // a committed review takes no further action, and is not reopened either:
    // the fix is live, so there is nothing to go back to
    let (s, b, _) = call!(&app, post(&editor_c, dup, "confirm", json!({})));
    assert_eq!((s, code_of(&b)), (409, "review_not_open"), "{b}");
    let (s, b, _) = call!(&app, post(&editor_c, dup, "reopen", json!({})));
    assert_eq!((s, code_of(&b)), (409, "review_not_closed"), "{b}");

    // ======================================================== confirm, reject, reopen, note
    let (s, d, _) = call!(
        &app,
        post(
            &editor_c,
            busy,
            "confirm",
            json!({"note": "walked it; the stop list matches the road"})
        )
    );
    assert_eq!(s, 200, "{d}");
    assert_eq!(d["status"], "confirmed");
    let (s, b, _) = call!(&app, post(&editor_c, busy, "confirm", json!({})));
    assert_eq!((s, code_of(&b)), (409, "review_not_open"), "{b}");
    let (s, d, _) = call!(&app, post(&editor_c, busy, "reopen", json!({})));
    assert_eq!(s, 200, "{d}");
    assert_eq!(d["status"], "pending");
    assert!(d["review_note"].is_null(), "reopening clears the note");

    let (s, d, _) = call!(
        &app,
        post(
            &editor_c,
            wide,
            "reject",
            json!({"note": "the detour is a real diversion round the lake"})
        )
    );
    assert_eq!(s, 200, "{d}");
    assert_eq!(d["status"], "rejected");
    // a judgement about priority can be revisited
    let (s, d, _) = call!(&app, post(&editor_c, wide, "reopen", json!({})));
    assert_eq!(s, 200, "{d}");
    assert_eq!(d["status"], "pending");
    let (s, b, _) = call!(&app, post(&editor_c, wide, "reopen", json!({})));
    assert_eq!((s, code_of(&b)), (409, "review_not_closed"), "{b}");

    let (s, d, _) = call!(
        &app,
        post(
            &editor_c,
            short,
            "note",
            json!({"note": "asked the depot for the missing stops"})
        )
    );
    assert_eq!(s, 200, "{d}");
    assert_eq!(d["review_note"], "asked the depot for the missing stops");
    assert_eq!(d["status"], "pending", "a note closes nothing");

    // ======================================================== what the audit says
    let actions: Vec<String> = sqlx::query(
        "SELECT DISTINCT action FROM gtfs_audit_log WHERE gtfs_id = $1 AND action LIKE 'route_review%'",
    )
    .bind(FEED)
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<String, _>("action"))
    .collect();
    for expected in [
        "route_review_fixed",
        "route_review_returned",
        "route_review_committed",
        "route_review_confirmed",
        "route_review_rejected",
        "route_review_reopened",
        "route_review_noted",
    ] {
        assert!(
            actions.contains(&expected.to_string()),
            "{expected} was never audited; got {actions:?}"
        );
    }

    // ======================================================== gone from the feed
    let (s, b, _) = call!(&app, get(&viewer, 0));
    assert_eq!((s, code_of(&b)), (404, "review_not_found"), "{b}");

    exec(&pool, &clear_feed(FEED)).await;
    exec(&pool, &reset_accounts(&[ADMIN, EDITOR, APPROVER, VIEWER])).await;
    std::fs::remove_dir_all(dir).ok();
}
