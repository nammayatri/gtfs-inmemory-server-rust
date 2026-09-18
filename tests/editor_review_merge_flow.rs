//! Coordinate reviews resolved by a merge (docs/gtfs-editor.md section 8.2), and
//! the cleanup context reads (section 9), end to end against a real Postgres
//! holding the editor schema (db/gtfs_editor/0001..0011): the `auto_fix` list
//! filter and summary counts, the evidence returned as stored; the dry
//! `?stop_id=` question answered by the merge's own validation; a merge refused
//! by it (`merge_would_repeat_stop`); a merge against a move of the same review,
//! both ways round; a merge taken back by removing its change and by discarding
//! its draft; two duplicates merged into one stop in one draft, released through
//! submit, approval by someone else and commit; and what the stop and route
//! context say before and after.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. Uses its own feed and accounts and removes its rows afterwards; the
//! timings only read chennai_bus. See scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use std::time::Instant;

const AUD: &str = "gtfs.editor-review-merge-test.local";
const BASE: &str = "/internal/gtfs-editor";

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

/// Reviews go first: an approved review refuses to lose its draft.
fn clear_feed(feed: &str) -> Vec<String> {
    vec![
        format!("DELETE FROM gtfs_position_review WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{feed}'"),
        format!("UPDATE gtfs_stop SET parent_station = NULL WHERE gtfs_id = '{feed}' AND parent_station IS NOT NULL"),
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
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, totp_last_step = NULL, \
             status = 'active' WHERE email IN ({list})"
        ),
        format!(
            "DELETE FROM gtfs_editor_session WHERE user_id IN (SELECT user_id FROM gtfs_editor_user WHERE email IN ({list}))"
        ),
    ]
}

fn state(pool: &PgPool, signer: &TestSigner, admin: &str) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-review-merge-{}", crypto::random_token()));
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
            ops_pool: None,
        },
    )
    .unwrap();
    (st, dir)
}

async fn scalar_i64(pool: &PgPool, sql: impl AsRef<str>) -> i64 {
    let sql = sql.as_ref();
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<i64, _>(0)
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

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"))
}

// ---------------------------------------------------------------- the flow

const FEED: &str = "editor_review_merge_test_feed";
const ADMIN: &str = "admin@editor-review-merge-test.invalid";
const EDITOR: &str = "editor@editor-review-merge-test.invalid";
const APPROVER: &str = "approver@editor-review-merge-test.invalid";
const VIEWER: &str = "viewer@editor-review-merge-test.invalid";

fn seed() -> Vec<String> {
    let mut s = clear_feed(FEED);
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{FEED}', 'Editor review merge test feed')"
    ));
    // S and S2 sit 542 m east of the road their routes take between A and B,
    // where C - the same name - is; P and Q are back to back on R3
    let stops = [
        ("A", "STOP A", 13.0, 80.2, 0),
        ("B", "STOP B", 13.002, 80.2, 0),
        ("S", "ANNA NAGAR", 13.001, 80.205, 0),
        ("S2", "ANNA NAGAR", 13.0011, 80.205, 0),
        ("C", "ANNA-NAGAR.", 13.001, 80.2, 0),
        ("D", "ANNA NAGAR WEST", 13.003, 80.2, 0),
        ("E", "A N N A NAGAR", 13.004, 80.2, 0),
        ("F", "K.K. NAGAR", 13.0012, 80.2, 0),
        ("Z", "ANNA NAGAR", 13.1, 80.2, 0),
        ("GONE", "ANNA NAGAR", 13.001, 80.2001, 0),
        ("ST", "ANNA NAGAR", 13.001, 80.2002, 1),
        ("P", "PERAMBUR", 13.02, 80.2, 0),
        ("Q", "PERAMBUR", 13.0201, 80.2, 0),
        ("prm_1", "PERAMBUR", 13.0202, 80.2, 0),
        ("N", "NO ADVICE", 13.05, 80.2, 0),
        ("M", "NOTHING FITS", 13.06, 80.2, 0),
        ("W", "MOVE ME", 13.07, 80.2, 0),
    ];
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, location_type, deleted) VALUES {}",
        stops
            .iter()
            .map(|(id, name, lat, lon, kind)| format!(
                "('{FEED}', '{id}', 'code-{id}', '{name}', {lat}, {lon}, {kind}, {})",
                *id == "GONE"
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id) VALUES \
         ('{FEED}', 'R1', '1', 'A To B', 'TESTAG'), ('{FEED}', 'R2', '2', 'A To B', 'TESTAG'), \
         ('{FEED}', 'R3', '3', 'A To B via Perambur', 'TESTAG'), ('{FEED}', 'R4', '4', 'A To B', 'TESTAG'), \
         ('{FEED}', 'R5', '5', 'A To B on the road', 'TESTAG')"
    ));
    let rows = [
        ("R1", 1, "A", "NEW STOP", 1, "STOP A"),
        ("R1", 2, "S", "INTERMEDIATE STOP", 1, "STOP A"),
        ("R1", 3, "B", "NEW STOP", 2, "STOP B"),
        ("R2", 1, "A", "NEW STOP", 1, "STOP A"),
        ("R2", 2, "S", "INTERMEDIATE STOP", 1, "STOP A"),
        ("R2", 3, "B", "NEW STOP", 2, "STOP B"),
        ("R3", 1, "A", "NEW STOP", 1, "STOP A"),
        ("R3", 2, "P", "NEW STOP", 2, "PERAMBUR"),
        ("R3", 3, "Q", "NEW STOP", 3, "PERAMBUR"),
        ("R3", 4, "B", "NEW STOP", 4, "STOP B"),
        ("R4", 1, "A", "NEW STOP", 1, "STOP A"),
        ("R4", 2, "S2", "INTERMEDIATE STOP", 1, "STOP A"),
        ("R4", 3, "B", "NEW STOP", 2, "STOP B"),
        ("R5", 1, "A", "NEW STOP", 1, "STOP A"),
        ("R5", 2, "C", "INTERMEDIATE STOP", 1, "STOP A"),
        ("R5", 3, "B", "NEW STOP", 2, "STOP B"),
    ];
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES {}",
        rows.iter()
            .map(|(r, q, st, t, n, name)| format!("('{FEED}', '{r}', {q}, '{st}', '{t}', {n}, '{name}', '7')"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    // what nandi's advisory tool stores (section 8.2), returned as stored
    let merge_evidence = json!({
        "same_name_candidates": [
            {"stop_id": "C", "name": "ANNA-NAGAR.", "lat": 13.001, "lon": 80.2, "distance_m": 542.0,
             "name_similarity": 1.0, "route_count": 1, "detour_m_after": 0.0, "shares_route": false,
             "verdict": "fits"},
            {"stop_id": "D", "name": "ANNA NAGAR WEST", "lat": 13.003, "lon": 80.2, "distance_m": 585.4,
             "name_similarity": 0.69, "route_count": 0, "detour_m_after": 222.4, "shares_route": false,
             "verdict": "no_fit"}],
        "auto_fix": {"action": "merge", "into_stop_id": "C", "detour_m": 883.5, "detour_m_after": 0.0,
                     "reason": "the same name on the road its routes take", "tool": "same_name_fix v1",
                     "threshold_m": 150}});
    let fix = |action: &str| {
        json!({"auto_fix": {"action": action, "detour_m": 0.0, "reason": "test",
                                                  "tool": "same_name_fix v1", "threshold_m": 150}})
    };
    let reviews = [
        ("S", "ANNA NAGAR", 13.001, 80.205, merge_evidence.clone()),
        ("S2", "ANNA NAGAR", 13.0011, 80.205, merge_evidence),
        ("P", "PERAMBUR", 13.02, 80.2, fix("choose")),
        ("M", "NOTHING FITS", 13.06, 80.2, fix("none")),
        ("W", "MOVE ME", 13.07, 80.2, fix("move")),
        ("N", "NO ADVICE", 13.05, 80.2, json!({})),
    ];
    s.push(format!(
        "INSERT INTO gtfs_position_review (gtfs_id, batch, stop_id, original_stop_id, stop_name, reason, lat, lon, evidence) VALUES {}",
        reviews
            .iter()
            .map(|(id, name, lat, lon, evidence)| format!(
                "('{FEED}', 'test-batch', '{id}', '{id}', '{name}', 'off its routes', {lat}, {lon}, '{evidence}')"
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    // an approved review counts in no auto_fix total
    s.extend(reset_accounts(&[ADMIN, EDITOR, APPROVER, VIEWER]));
    s
}

async fn review_id(pool: &PgPool, stop: &str) -> i64 {
    scalar_i64(
        pool,
        format!("SELECT review_id FROM gtfs_position_review WHERE gtfs_id = '{FEED}' AND stop_id = '{stop}'"),
    )
    .await
}

#[actix_web::test]
async fn review_merge_and_context() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("review-merge-test-key");
    let (st, dir) = state(&pool, &signer, ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    // ---- accounts
    let caller = |email: &str| Caller {
        signer: &signer,
        email: email.into(),
        session: None,
    };
    let mut admin = caller(ADMIN);
    let (s, b, _) = call!(&app, admin.req("POST", "/auth/totp/enroll"));
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
    let others = [
        (EDITOR, "editor"),
        (APPROVER, "approver"),
        (VIEWER, "viewer"),
    ];
    for (email, role) in others {
        let (s, b, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    }
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    for (email, role) in others {
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
    let (mut editor_c, mut approver, mut viewer) =
        (caller(EDITOR), caller(APPROVER), caller(VIEWER));
    for c in [&mut editor_c, &mut approver, &mut viewer] {
        let (s, b, _) = call!(&app, c.req("POST", "/auth/totp/enroll"));
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

    // the audit log is append-only, so an earlier run's rows about this feed are
    // still there: only this run's are looked at
    let before = scalar_i64(
        &pool,
        "SELECT coalesce(max(audit_id), 0) FROM gtfs_audit_log",
    )
    .await;
    let this_run = |audit: &Value| -> Vec<String> {
        audit
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["audit_id"].as_i64().unwrap() > before)
            .map(|a| a["action"].as_str().unwrap().to_string())
            .collect()
    };

    let new_set = |c: &Caller, title: &str| {
        c.req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": title}))
    };
    let (rs, rs2, rp) = (
        review_id(&pool, "S").await,
        review_id(&pool, "S2").await,
        review_id(&pool, "P").await,
    );
    let get = |c: &Caller, id: i64, q: &str| c.req("GET", &format!("/position-reviews/{id}{q}"));
    let post = |c: &Caller, id: i64, action: &str, body: Value| {
        c.req("POST", &format!("/position-reviews/{id}/{action}"))
            .set_json(body)
    };
    let changes_in = |set: &str| {
        scalar_i64(
            &pool,
            format!("SELECT count(*) FROM gtfs_change WHERE change_set_id = '{set}'"),
        )
    };
    let status_of = |id: i64| {
        let pool = pool.clone();
        async move {
            sqlx::query("SELECT status FROM gtfs_position_review WHERE review_id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
                .get::<String, _>(0)
        }
    };

    // ======================================================== the advice: filter, counts, evidence
    let find = |q: &str| editor_c.req("GET", &format!("/feeds/{FEED}/position-reviews?{q}"));
    for (q, want) in [
        ("auto_fix=merge", vec!["S", "S2"]),
        ("auto_fix=choose", vec!["P"]),
        ("auto_fix=none", vec!["M"]),
        ("auto_fix=move", vec!["W"]),
        ("auto_fix=merge&q=S2", vec!["S2"]),
        ("auto_fix=", vec!["S", "S2", "P", "M", "W", "N"]),
    ] {
        let (s, found, _) = call!(&app, find(q));
        assert_eq!(s, 200, "{q}: {found}");
        let listed: Vec<&str> = found["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["stop_id"].as_str().unwrap())
            .collect();
        assert_eq!(listed, want, "{q}");
    }
    let (s, b, _) = call!(&app, find("auto_fix=bogus"));
    assert_eq!((s, code_of(&b)), (400, "invalid_auto_fix"), "{b}");
    let summary = || viewer.req("GET", &format!("/feeds/{FEED}/position-reviews/summary"));
    let (_, b, _) = call!(&app, summary());
    assert_eq!(
        b,
        json!({"pending": 6, "approved": 0, "committed": 0, "confirmed": 0,
               "auto_fix": {"merge": 2, "move": 1, "choose": 1, "none": 1}})
    );
    let (_, detail, _) = call!(&app, get(&viewer, rs, ""));
    let candidates = &detail["evidence"]["same_name_candidates"];
    assert_eq!(candidates.as_array().unwrap().len(), 2, "{detail}");
    assert_eq!(
        (
            &candidates[0]["stop_id"],
            &candidates[0]["verdict"],
            &candidates[0]["shares_route"]
        ),
        (&json!("C"), &json!("fits"), &json!(false))
    );
    assert_eq!(detail["evidence"]["auto_fix"]["into_stop_id"], "C");
    assert_eq!(detail["evidence"]["auto_fix"]["threshold_m"], 150);
    assert!((883.0..884.0).contains(&f(&detail["detour_m"])), "{detail}");
    assert!(detail["merge_problems"].is_null() && detail["merge_into_stop_id"].is_null());

    // ======================================================== the dry question
    let (s, dry, _) = call!(&app, get(&viewer, rs, "?stop_id=C"));
    assert_eq!(s, 200, "{dry}");
    assert!(f(&dry["detour_m_after"]) < 0.1, "{dry}");
    assert_eq!(
        codes(&dry["merge_problems"]),
        vec!["merge_far_apart", "merge_names_differ"]
    );
    assert!(dry["merge_problems"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["level"] == "warning" && p["message"].is_string()));
    assert_eq!(dry["draft_actions"], json!([]));
    // D is 111 m past B: there and back
    let (_, dry, _) = call!(&app, get(&viewer, rs, "?stop_id=D"));
    assert!((222.0..223.0).contains(&f(&dry["detour_m_after"])), "{dry}");
    for (review, into, code) in [
        (rp, "Q", "merge_would_repeat_stop"),
        (rs, "S", "merge_same_stop"),
        (rs, "NOPE", "stop_not_found"),
        (rs, "GONE", "stop_deleted"),
        (rs, "ST", "stop_is_station"),
        (rp, "prm_1", "merge_prm_stop"),
    ] {
        let (s, dry, _) = call!(&app, get(&viewer, review, &format!("?stop_id={into}")));
        assert_eq!(s, 200, "{into}: {dry}");
        let found = &dry["merge_problems"];
        assert!(codes(found).contains(&code.to_string()), "{into}: {found}");
        assert!(
            found
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["level"] == "error"),
            "{into}: {found}"
        );
        if into == "NOPE" {
            assert!(dry["detour_m_after"].is_null(), "{dry}");
        }
    }
    let (_, dry, _) = call!(&app, get(&viewer, rp, "?stop_id=Q"));
    let repeat = dry["merge_problems"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["code"] == "merge_would_repeat_stop")
        .unwrap();
    assert!(
        repeat["message"].as_str().unwrap().contains("route R3")
            && repeat["message"]
                .as_str()
                .unwrap()
                .contains("sequences 2 and 3"),
        "{repeat}"
    );
    for q in ["?stop_id=C&lat=13.0&lon=80.2", "?stop_id=C&route_ids=R1"] {
        let (s, b, _) = call!(&app, get(&viewer, rs, q));
        assert_eq!((s, code_of(&b)), (400, "invalid_query"), "{q}: {b}");
    }
    // asking changed nothing
    assert_eq!(
        scalar_i64(
            &pool,
            format!("SELECT count(*) FROM gtfs_position_review WHERE gtfs_id = '{FEED}' AND status = 'pending'")
        )
        .await,
        6
    );
    assert_eq!(
        scalar_i64(
            &pool,
            format!("SELECT count(*) FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND deleted")
        )
        .await,
        1
    );

    // ======================================================== refused by the merge's own validation
    let (s, set, _) = call!(&app, new_set(&editor_c, "merges, first try"));
    assert_eq!(s, 201, "{set}");
    let set1 = set["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rp,
            "merge",
            json!({"change_set_id": set1, "into_stop_id": "Q"})
        )
    );
    assert_eq!((s, code_of(&b)), (400, "review_has_problems"), "{b}");
    let found = &b["error"]["details"]["problems"];
    assert!(
        codes(found).contains(&"merge_would_repeat_stop".to_string()),
        "{found}"
    );
    for (into, code) in [
        ("ST", "stop_is_station"),
        ("NOPE", "stop_not_found"),
        ("P", "merge_same_stop"),
    ] {
        let (s, b, _) = call!(
            &app,
            post(
                &editor_c,
                rp,
                "merge",
                json!({"change_set_id": set1, "into_stop_id": into})
            )
        );
        assert_eq!(
            (s, code_of(&b)),
            (400, "review_has_problems"),
            "{into}: {b}"
        );
        let found = &b["error"]["details"]["problems"];
        assert!(codes(found).contains(&code.to_string()), "{into}: {found}");
    }
    assert_eq!(status_of(rp).await, "pending");
    assert_eq!(changes_in(&set1).await, 0);
    // the request itself
    let (s, b, _) = call!(
        &app,
        post(
            &viewer,
            rs,
            "merge",
            json!({"change_set_id": set1, "into_stop_id": "C"})
        )
    );
    assert_eq!(s, 403, "{b}");
    for body in [
        json!({"change_set_id": set1, "into_stop_id": " "}),
        json!({"change_set_id": set1, "into_stop_id": "C", "keep_name": "both"}),
    ] {
        let (s, b, _) = call!(&app, post(&editor_c, rs, "merge", body));
        assert_eq!((s, code_of(&b)), (400, "invalid_merge"), "{b}");
    }
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "merge",
            json!({"change_set_id": set1, "into_stop_id": "C", "lat": 1})
        )
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_json"), "{b}");
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            0,
            "merge",
            json!({"change_set_id": set1, "into_stop_id": "C"})
        )
    );
    assert_eq!((s, code_of(&b)), (404, "review_not_found"), "{b}");

    // ======================================================== a merge and a move do not share a review
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "move",
            json!({"change_set_id": set1, "lat": 13.001, "lon": 80.2001})
        )
    );
    assert_eq!(s, 200, "{b}");
    let moved = b["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "merge",
            json!({"change_set_id": set1, "into_stop_id": "C"})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "draft_conflict"), "{b}");
    assert_eq!(b["error"]["details"]["change_ids"], json!([moved]));
    // nor does the draft's own edit of either stop
    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{set1}/changes"))
            .set_json(json!({"entity": "stop", "op": "update", "entity_key": "C", "after": {"name": "ANNA NAGAR"}}))
    );
    assert_eq!(s, 201, "{b}");
    let renamed = b["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs2,
            "merge",
            json!({"change_set_id": set1, "into_stop_id": "C"})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "draft_conflict"), "{b}");
    assert_eq!(b["error"]["details"]["change_ids"], json!([renamed]));
    for change in [moved, renamed] {
        let (s, b, _) = call!(
            &app,
            editor_c.req("DELETE", &format!("/change-sets/{set1}/changes/{change}"))
        );
        assert_eq!(s, 200, "{b}");
    }
    assert_eq!(status_of(rs).await, "pending");

    // ======================================================== the merge, taken back twice
    let (s, merged, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "merge",
            json!({"change_set_id": set1, "into_stop_id": " C ", "note": "same place"})
        )
    );
    assert_eq!(s, 200, "{merged}");
    assert_eq!(merged["status"], "approved");
    assert_eq!(merged["change_set_id"], set1.as_str());
    assert_eq!(merged["review_note"], "same place");
    assert_eq!(merged["reviewed_by_email"], EDITOR);
    assert_eq!(
        codes(&merged["warnings"]),
        vec!["merge_far_apart", "merge_names_differ"]
    );
    let change_id = merged["change_id"].as_i64().unwrap();
    assert_eq!(merged["merge_into_stop_id"], "C");
    assert_eq!(merged["new_position"], json!({"lat": 13.001, "lon": 80.2}));
    assert!(merged["new_stop_id"].is_null() && merged["split_route_ids"].is_null());
    assert!(f(&merged["detour_m_after"]) < 0.1, "{merged}");
    assert_eq!(
        merged["draft_actions"].as_array().unwrap().len(),
        1,
        "{merged}"
    );
    let action = &merged["draft_actions"][0];
    assert_eq!(
        (
            &action["kind"],
            &action["change_id"],
            &action["into_stop_id"]
        ),
        (&json!("merge"), &json!(change_id), &json!("C"))
    );
    assert_eq!((f(&action["lat"]), f(&action["lon"])), (13.001, 80.2));
    assert!(f(&action["detour_m_after"]) < 0.1, "{action}");
    // the change is the one POST /changes would have stored
    let (_, draft, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{set1}")));
    assert_eq!(draft["changes"].as_array().unwrap().len(), 1, "{draft}");
    let change = &draft["changes"][0];
    assert_eq!(
        (
            &change["entity"],
            &change["op"],
            &change["entity_key"],
            &change["base_row_version"]
        ),
        (&json!("stop"), &json!("merge"), &json!("S"), &json!(1))
    );
    assert_eq!(
        change["after"],
        json!({"into_stop_id": "C", "into_row_version": 1, "keep_name": "into", "keep_position": "into",
               "position_review_id": rs})
    );
    assert_eq!(change["before"]["from"]["stop_id"], "S");
    assert_eq!(change["before"]["into"]["stop_id"], "C");
    let affected: Vec<&str> = change["before"]["affected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["route_id"].as_str().unwrap())
        .collect();
    assert_eq!(affected, vec!["R1", "R2"]);
    assert_eq!(
        codes(&draft["validation"]),
        vec!["merge_far_apart", "merge_names_differ"]
    );
    assert_eq!(draft["can_submit"], true);
    // the draft previews R1 through C
    let (_, preview, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{set1}/preview/routes/R1"))
    );
    assert_eq!(preview["rows"][1]["stop_id"], "C", "{preview}");
    // nothing else can be done to a stop that goes away
    for (action, body) in [
        (
            "move",
            json!({"change_set_id": set1, "lat": 13.001, "lon": 80.2001}),
        ),
        (
            "split",
            json!({"change_set_id": set1, "route_ids": ["R1"], "lat": 13.001, "lon": 80.2001}),
        ),
        ("merge", json!({"change_set_id": set1, "into_stop_id": "D"})),
    ] {
        let (s, b, _) = call!(&app, post(&editor_c, rs, action, body));
        assert_eq!((s, code_of(&b)), (409, "draft_conflict"), "{action}: {b}");
        assert_eq!(
            b["error"]["details"]["change_ids"],
            json!([change_id]),
            "{action}"
        );
    }
    let (s, other, _) = call!(&app, new_set(&editor_c, "another draft"));
    assert_eq!(s, 201);
    let other = other["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "merge",
            json!({"change_set_id": other, "into_stop_id": "C"})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "review_in_other_draft"), "{b}");
    assert_eq!(b["error"]["details"]["change_set_id"], set1.as_str());
    // an edit of the change keeps the review, as for a move
    let edit = |after: Value| {
        editor_c
            .req("PUT", &format!("/change-sets/{set1}/changes/{change_id}"))
            .set_json(json!({"after": after}))
    };
    let (s, b, _) = call!(
        &app,
        edit(json!({"into_stop_id": "C", "keep_name": "from"}))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["changes"][0]["after"]["position_review_id"], rs);
    assert_eq!(b["changes"][0]["after"]["keep_name"], "from");
    assert_eq!(b["changes"][0]["after"]["into_row_version"], 1);
    let (s, b, _) = call!(
        &app,
        edit(json!({"into_stop_id": "C", "position_review_id": rs2}))
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
    assert_eq!(b["error"]["details"]["code"], "position_review_mismatch");
    let (_, b, _) = call!(&app, summary());
    assert_eq!((&b["pending"], &b["approved"]), (&json!(5), &json!(1)));
    assert_eq!(
        b["auto_fix"]["merge"], 1,
        "only the reviews still waiting: {b}"
    );

    // removing the change returns the review
    let (s, b, _) = call!(
        &app,
        editor_c.req(
            "DELETE",
            &format!("/change-sets/{set1}/changes/{change_id}")
        )
    );
    assert_eq!(s, 200, "{b}");
    let (_, detail, _) = call!(&app, get(&editor_c, rs, ""));
    assert_eq!(detail["status"], "pending");
    assert!(
        detail["change_set_id"].is_null() && detail["change_id"].is_null(),
        "{detail}"
    );
    assert!(detail["review_note"].is_null() && detail["merge_into_stop_id"].is_null());
    assert_eq!(detail["draft_actions"], json!([]));
    // so does discarding the draft
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "merge",
            json!({"change_set_id": set1, "into_stop_id": "C"})
        )
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set1}/discard"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(status_of(rs).await, "pending");
    let returned: Vec<String> = sqlx::query(
        "SELECT detail->>'reason' FROM gtfs_audit_log WHERE change_set_id = $1::uuid \
         AND action = 'position_review_returned' AND (detail->>'review_id')::bigint = $2 ORDER BY audit_id",
    )
    .bind(&set1)
    .bind(rs)
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get(0))
    .collect();
    assert_eq!(
        returned,
        vec!["change_removed", "change_removed", "change_set_discarded"]
    );

    // ======================================================== two duplicates into one stop, released
    let set2 = other;
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "merge",
            json!({"change_set_id": set2, "into_stop_id": "C", "keep_name": "from", "note": "the road stop"})
        )
    );
    assert_eq!(s, 200, "{b}");
    let first = b["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs2,
            "merge",
            json!({"change_set_id": set2, "into_stop_id": "C"})
        )
    );
    assert_eq!(
        s, 200,
        "a second merge into the same stop is no conflict: {b}"
    );
    let second = b["change_id"].as_i64().unwrap();
    // the dry question on top of this draft: S2 is already merged away in it
    let (s, dry, _) = call!(
        &app,
        get(&viewer, rs2, &format!("?stop_id=D&change_set={set2}"))
    );
    assert_eq!(s, 200, "{dry}");
    assert!(
        codes(&dry["merge_problems"]).contains(&"stop_merged_away".to_string()),
        "{dry}"
    );
    let (s, b, _) = call!(
        &app,
        get(
            &viewer,
            rs2,
            "?stop_id=D&change_set=00000000-0000-0000-0000-000000000000"
        )
    );
    assert_eq!((s, code_of(&b)), (404, "change_set_not_found"), "{b}");
    // a route change in the same draft, for the route context
    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{set2}/changes"))
            .set_json(json!({"entity": "route", "op": "update", "entity_key": "R1", "after": {"color": "#0A7E3C"}}))
    );
    assert_eq!(s, 201, "{b}");
    let recolour = b["change_id"].as_i64().unwrap();

    // ---- context, while the draft is open
    let stop_context =
        |c: &Caller, id: &str| c.req("GET", &format!("/feeds/{FEED}/stops/{id}/context"));
    let route_context =
        |c: &Caller, id: &str| c.req("GET", &format!("/feeds/{FEED}/routes/{id}/context"));
    let (s, ctx_s, _) = call!(&app, stop_context(&viewer, "S"));
    assert_eq!(s, 200, "{ctx_s}");
    assert_eq!(ctx_s["stop_id"], "S");
    assert!((883.0..884.0).contains(&f(&ctx_s["detour_m"])), "{ctx_s}");
    assert_eq!(ctx_s["routes_measured"], 2);
    assert_eq!(
        ctx_s["position_reviews"],
        json!({"pending": 0, "approved": 1, "committed": 0, "confirmed": 0,
               "items": [{"review_id": rs, "status": "approved", "reason": "off its routes"}]})
    );
    // nearest first; F is another name, Z too far, GONE deleted, ST a station
    let same: Vec<(&str, f64)> = ctx_s["same_name"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["stop_id"].as_str().unwrap(), f(&x["distance_m"])))
        .collect();
    let ids: Vec<&str> = same.iter().map(|x| x.0).collect();
    assert_eq!(ids, vec!["S2", "C", "D", "E"], "{ctx_s}");
    assert!(same.windows(2).all(|w| w[0].1 <= w[1].1), "{same:?}");
    let c_entry = &ctx_s["same_name"][1];
    for field in [
        "stop_id",
        "name",
        "lat",
        "lon",
        "distance_m",
        "route_count",
        "parent_station",
        "similarity",
    ] {
        assert!(
            c_entry.get(field).is_some(),
            "{field} missing from {c_entry}"
        );
    }
    assert_eq!(
        (&c_entry["name"], &c_entry["route_count"]),
        (&json!("ANNA-NAGAR."), &json!(1))
    );
    assert_eq!(f(&c_entry["similarity"]), 1.0);
    assert!(
        (541.0..543.0).contains(&f(&c_entry["distance_m"])),
        "{c_entry}"
    );
    // E is the same name only once spaces are ignored
    assert!(f(&ctx_s["same_name"][3]["similarity"]) < 0.6, "{ctx_s}");
    assert_eq!(
        ctx_s["open_drafts"],
        json!([{"change_set_id": set2, "title": "another draft", "status": "draft", "change_id": first,
                "entity": "stop", "op": "merge"}])
    );
    let actions = this_run(&ctx_s["audit"]);
    assert_eq!(actions.len(), 10, "the latest ten: {actions:?}");
    assert_eq!(&actions[..2], ["position_review_merged", "change_added"]);
    let newest = &ctx_s["audit"][0];
    for field in [
        "audit_id",
        "at",
        "actor_email",
        "action",
        "change_set_id",
        "detail",
    ] {
        assert!(newest.get(field).is_some(), "{field} missing from {newest}");
    }
    assert_eq!(newest["actor_email"], EDITOR);
    assert_eq!(newest["detail"]["into_stop_id"], "C");
    // the stop both merge into
    let (_, ctx_c, _) = call!(&app, stop_context(&viewer, "C"));
    assert!(f(&ctx_c["detour_m"]) < 0.1, "{ctx_c}");
    assert_eq!(ctx_c["routes_measured"], 1);
    assert_eq!(ctx_c["position_reviews"]["items"], json!([]));
    let into_c: Vec<i64> = ctx_c["open_drafts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["change_id"].as_i64().unwrap())
        .collect();
    assert_eq!(into_c, vec![first, second]);
    let (s, b, _) = call!(&app, stop_context(&viewer, "NOPE"));
    assert_eq!((s, code_of(&b)), (404, "stop_not_found"), "{b}");

    let (s, ctx_r, _) = call!(&app, route_context(&viewer, "R1"));
    assert_eq!(s, 200, "{ctx_r}");
    assert_eq!(ctx_r["route_id"], "R1");
    assert_eq!(
        ctx_r["stops_with_reviews"],
        json!([{"stop_id": "S", "sequence": 2, "review_id": rs, "status": "approved"}])
    );
    assert_eq!(
        ctx_r["worst_detours"].as_array().unwrap().len(),
        1,
        "{ctx_r}"
    );
    let worst = &ctx_r["worst_detours"][0];
    assert_eq!(
        (&worst["stop_id"], &worst["name"], &worst["sequence"]),
        (&json!("S"), &json!("ANNA NAGAR"), &json!(2))
    );
    assert!((883.0..884.0).contains(&f(&worst["detour_m"])), "{worst}");
    assert_eq!(
        ctx_r["open_drafts"],
        json!([{"change_set_id": set2, "title": "another draft", "status": "draft", "change_id": recolour,
                "entity": "route", "op": "update"}])
    );
    assert_eq!(this_run(&ctx_r["audit"]), vec!["change_added"], "{ctx_r}");
    // R5 runs along the road: nothing over 300 m
    let (_, ctx_r5, _) = call!(&app, route_context(&viewer, "R5"));
    assert_eq!(
        (
            &ctx_r5["stops_with_reviews"],
            &ctx_r5["worst_detours"],
            &ctx_r5["open_drafts"]
        ),
        (&json!([]), &json!([]), &json!([]))
    );
    let (s, b, _) = call!(&app, route_context(&viewer, "NOPE"));
    assert_eq!((s, code_of(&b)), (404, "route_not_found"), "{b}");

    // ---- released: submit, approval by someone else, commit
    let version = scalar_i64(
        &pool,
        format!("SELECT version FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
    )
    .await;
    for (who, step) in [
        (&editor_c, "submit"),
        (&approver, "approve"),
        (&approver, "commit"),
    ] {
        let (s, b, _) = call!(
            &app,
            who.req("POST", &format!("/change-sets/{set2}/{step}"))
        );
        assert_eq!(s, 200, "{step}: {b}");
    }
    // every row that called S or S2 calls C; the duplicates are gone, C took S's name
    let calls_c: Vec<String> = sqlx::query(&format!(
        "SELECT route_id FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'C' ORDER BY route_id"
    ))
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get(0))
    .collect();
    assert_eq!(calls_c, vec!["R1", "R2", "R4", "R5"]);
    assert_eq!(
        scalar_i64(
            &pool,
            format!("SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND stop_id IN ('S', 'S2')")
        )
        .await,
        0
    );
    let gone: Vec<(String, bool, Option<String>)> = sqlx::query(&format!(
        "SELECT stop_id, deleted, provenance->>'merged_into' FROM gtfs_stop \
         WHERE gtfs_id = '{FEED}' AND stop_id IN ('S', 'S2') ORDER BY stop_id"
    ))
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get(0), r.get(1), r.get(2)))
    .collect();
    assert_eq!(
        gone,
        vec![
            ("S".to_string(), true, Some("C".to_string())),
            ("S2".to_string(), true, Some("C".to_string()))
        ]
    );
    let kept: String = sqlx::query(&format!(
        "SELECT name FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'C'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    assert_eq!(kept, "ANNA NAGAR");
    assert_eq!(
        scalar_i64(
            &pool,
            format!("SELECT version FROM gtfs_feed WHERE gtfs_id = '{FEED}'")
        )
        .await,
        version + 1
    );
    // the reviews are committed, and read as the merges they were
    let (_, detail, _) = call!(&app, get(&viewer, rs, ""));
    assert_eq!(detail["status"], "committed");
    assert_eq!(
        (&detail["change_set_id"], &detail["change_id"]),
        (&json!(set2), &json!(first))
    );
    assert_eq!(detail["merge_into_stop_id"], "C");
    assert_eq!(detail["draft_actions"][0]["kind"], "merge");
    assert!(
        f(&detail["draft_actions"][0]["detour_m_after"]) < 0.1,
        "{detail}"
    );
    assert_eq!(codes(&detail["problems"]), vec!["stop_merged_away"]);
    assert_eq!(detail["routes"], json!([]));
    assert_eq!(status_of(rs2).await, "committed");
    // a committed review takes nothing more
    let (s, third, _) = call!(&app, new_set(&editor_c, "too late"));
    assert_eq!(s, 201);
    let third = third["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "merge",
            json!({"change_set_id": third, "into_stop_id": "C"})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "review_not_pending"), "{b}");

    // ---- audit: drafted as a merge for a review, merged at commit
    let audit: Vec<(String, Value)> = sqlx::query(
        "SELECT action, detail::text FROM gtfs_audit_log WHERE change_set_id = $1::uuid ORDER BY audit_id",
    )
    .bind(&set2)
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get(0), serde_json::from_str(&r.get::<String, _>(1)).unwrap()))
    .collect();
    let of = |action: &str| -> Vec<&Value> {
        audit
            .iter()
            .filter(|a| a.0 == action)
            .map(|a| &a.1)
            .collect()
    };
    let merged = of("position_review_merged");
    assert_eq!(merged.len(), 2, "{audit:?}");
    let d = merged[0];
    assert_eq!(
        (
            &d["review_id"],
            &d["stop_id"],
            &d["into_stop_id"],
            &d["change_id"],
            &d["change_set_id"],
            &d["note"]
        ),
        (
            &json!(rs),
            &json!("S"),
            &json!("C"),
            &json!(first),
            &json!(set2),
            &json!("the road stop")
        )
    );
    assert!((883.0..884.0).contains(&f(&d["detour_m"])), "{d}");
    assert!(f(&d["detour_m_after"]) < 0.1, "{d}");
    assert!((541.0..543.0).contains(&f(&d["moved_m"])), "{d}");
    assert!(merged[1]["note"].is_null());
    // added by the path every change is added by
    let added: Vec<&Value> = of("change_added")
        .into_iter()
        .filter(|d| d["op"] == "merge")
        .collect();
    assert_eq!(added.len(), 2);
    assert_eq!(
        (&added[0]["change_id"], &added[0]["entity_key"]),
        (&json!(first), &json!("S"))
    );
    let stop_merged = of("stop_merged");
    assert_eq!(stop_merged.len(), 2);
    assert_eq!(
        (
            &stop_merged[0]["from"],
            &stop_merged[0]["into"],
            &stop_merged[0]["routes"],
            &stop_merged[0]["keep_name"]
        ),
        (&json!("S"), &json!("C"), &json!(2), &json!("from"))
    );
    let committed = of("position_review_committed");
    assert_eq!(committed.len(), 2);
    assert_eq!(
        (
            &committed[0]["review_id"],
            &committed[0]["change_id"],
            &committed[0]["feed_version"]
        ),
        (&json!(rs), &json!(first), &json!(version + 1))
    );

    // ---- context, after
    let (_, ctx_s, _) = call!(&app, stop_context(&viewer, "S"));
    assert!(ctx_s["detour_m"].is_null(), "{ctx_s}");
    assert_eq!(ctx_s["routes_measured"], 0);
    assert_eq!(ctx_s["position_reviews"]["committed"], 1);
    assert_eq!(ctx_s["open_drafts"], json!([]));
    let actions = this_run(&ctx_s["audit"]);
    assert_eq!(
        &actions[..5],
        [
            "position_review_committed",
            "stop_merged",
            "change_set_committed",
            "position_review_merged",
            "change_added"
        ],
        "{actions:?}"
    );
    let (_, ctx_c, _) = call!(&app, stop_context(&viewer, "C"));
    assert_eq!(ctx_c["routes_measured"], 4);
    let actions = this_run(&ctx_c["audit"]);
    // kept by both merges; before that, the rename drafted on it and taken back
    assert_eq!(actions, vec!["stop_merged", "stop_merged", "change_added"]);
    // S and S2 are deleted: no longer the same name as anything
    let same: Vec<&str> = ctx_c["same_name"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["stop_id"].as_str().unwrap())
        .collect();
    assert_eq!(same, vec!["D", "E"]);
    let (_, ctx_r, _) = call!(&app, route_context(&viewer, "R1"));
    assert_eq!(
        (
            &ctx_r["stops_with_reviews"],
            &ctx_r["worst_detours"],
            &ctx_r["open_drafts"]
        ),
        (&json!([]), &json!([]), &json!([]))
    );
    let actions = this_run(&ctx_r["audit"]);
    assert_eq!(actions, vec!["change_set_committed", "change_added"]);

    exec(&pool, &clear_feed(FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}

// ---------------------------------------------------------------- timings

/// The two context reads and the dry merge question on chennai_bus, which is
/// only read. Prints the timings.
#[actix_web::test]
async fn context_timings() {
    const PERF_ADMIN: &str = "admin@editor-context-perf-test.invalid";
    let Some(pool) = local_pool().await else {
        return;
    };
    let source = scalar_i64(
        &pool,
        "SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = 'chennai_bus'",
    )
    .await;
    if source < 5000 {
        eprintln!("chennai_bus has {source} route rows; skipping the context timings");
        return;
    }
    exec(&pool, &reset_accounts(&[PERF_ADMIN])).await;
    let signer = TestSigner::generate("context-perf-key");
    let (st, dir) = state(&pool, &signer, PERF_ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;
    let mut admin = Caller {
        signer: &signer,
        email: PERF_ADMIN.into(),
        session: None,
    };
    let (s, b, _) = call!(&app, admin.req("POST", "/auth/totp/enroll"));
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

    let text = |sql: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(sql)
                .fetch_all(&pool)
                .await
                .unwrap()
                .iter()
                .map(|r| r.get::<String, _>(0))
                .collect::<Vec<_>>()
        }
    };
    // the busiest stops, the stops whose name the most stops share, the longest routes
    let mut stops = text(
        "SELECT stop_id FROM gtfs_route_stop WHERE gtfs_id = 'chennai_bus' AND stop_id IS NOT NULL \
         GROUP BY stop_id ORDER BY count(DISTINCT route_id) DESC, stop_id LIMIT 3",
    )
    .await;
    stops.extend(
        text(
            "SELECT min(stop_id) FROM gtfs_stop WHERE gtfs_id = 'chennai_bus' AND NOT deleted \
             GROUP BY name ORDER BY count(*) DESC, name LIMIT 3",
        )
        .await,
    );
    let routes = text(
        "SELECT route_id FROM gtfs_route_stop WHERE gtfs_id = 'chennai_bus' \
         GROUP BY route_id ORDER BY count(*) DESC, route_id LIMIT 3",
    )
    .await;
    for stop in &stops {
        let mut took = Vec::new();
        let mut last = Value::Null;
        for _ in 0..3 {
            let t = Instant::now();
            let (s, b, _) = call!(
                &app,
                admin.req("GET", &format!("/feeds/chennai_bus/stops/{stop}/context"))
            );
            took.push(t.elapsed());
            assert_eq!(s, 200, "{b}");
            last = b;
        }
        eprintln!(
            "[timing] stop context of {stop}: {took:?} ({} calls measured, {} same-named, {} audit rows)",
            last["routes_measured"],
            last["same_name"].as_array().unwrap().len(),
            last["audit"].as_array().unwrap().len()
        );
        assert!(took[2].as_secs() < 2, "{stop}: {took:?}");
    }
    for route in &routes {
        let mut took = Vec::new();
        let mut last = Value::Null;
        for _ in 0..3 {
            let t = Instant::now();
            let (s, b, _) = call!(
                &app,
                admin.req("GET", &format!("/feeds/chennai_bus/routes/{route}/context"))
            );
            took.push(t.elapsed());
            assert_eq!(s, 200, "{b}");
            last = b;
        }
        eprintln!(
            "[timing] route context of {route}: {took:?} ({} stops with reviews, {} worst detours)",
            last["stops_with_reviews"].as_array().unwrap().len(),
            last["worst_detours"].as_array().unwrap().len()
        );
        assert!(took[2].as_secs() < 2, "{route}: {took:?}");
    }
    // the dry merge question for a real review, against its nearest same-named stop
    let (_, list, _) = call!(
        &app,
        admin.req("GET", "/feeds/chennai_bus/position-reviews?limit=25")
    );
    let mut asked = 0;
    for item in list["items"].as_array().unwrap() {
        let (id, stop) = (
            item["review_id"].as_i64().unwrap(),
            item["stop_id"].as_str().unwrap(),
        );
        let (_, ctx, _) = call!(
            &app,
            admin.req("GET", &format!("/feeds/chennai_bus/stops/{stop}/context"))
        );
        let Some(into) = ctx["same_name"][0]["stop_id"].as_str() else {
            continue;
        };
        let t = Instant::now();
        let (s, b, _) = call!(
            &app,
            admin.req("GET", &format!("/position-reviews/{id}?stop_id={into}"))
        );
        assert_eq!(s, 200, "{b}");
        eprintln!(
            "[timing] would {stop} merge into {into}: {:?} (detour {} -> {}, {})",
            t.elapsed(),
            b["detour_m"],
            b["detour_m_after"],
            b["merge_problems"]
        );
        asked += 1;
        if asked == 3 {
            break;
        }
    }
    std::fs::remove_dir_all(dir).ok();
}
