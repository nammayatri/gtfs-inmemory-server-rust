//! Coordinate reviews (docs/gtfs-editor.md sections 8 and 8.1) end to end
//! against a real Postgres holding the editor schema (db/gtfs_editor/0001..0007):
//! the list, summary and detail with the routes through a stop; a move taken
//! back by discarding its draft and by removing its change, then released
//! through submit, approval by someone else and commit; a split of some routes
//! onto a new stop; confirm and reopen; a stop merged away.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. The flow uses its own feed and accounts and removes its rows
//! afterwards. The timing test copies chennai_bus into a feed of its own for
//! anything it writes, and only reads chennai_bus's real reviews. See
//! scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use std::time::{Duration, Instant};

const AUD: &str = "gtfs.editor-review-test.local";
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
    let dir = std::env::temp_dir().join(format!("editor-review-{}", crypto::random_token()));
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

async fn scalar_i64(pool: &PgPool, sql: impl AsRef<str>) -> i64 {
    let sql = sql.as_ref();
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<i64, _>(0)
}

fn errors(v: &Value) -> Vec<Value> {
    v["validation"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| x["level"] == "error")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
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

const FEED: &str = "editor_review_test_feed";
const ADMIN: &str = "admin@editor-review-test.invalid";
const EDITOR: &str = "editor@editor-review-test.invalid";
const APPROVER: &str = "approver@editor-review-test.invalid";

fn seed() -> Vec<String> {
    let mut s = clear_feed(FEED);
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{FEED}', 'Editor review test feed')"
    ));
    // S sits 542 m east of the road its routes take between A and B; T sits on
    // the east road, where R7 passes, while R5 and R6 take the west road
    let stops = [
        ("A", "STOP A", 13.0, 80.2),
        ("B", "STOP B", 13.002, 80.2),
        ("S", "SUSPECT S", 13.001, 80.205),
        ("J", "JUMP J", 13.0005, 80.23),
        ("E", "STOP E", 13.01, 80.2),
        ("F", "STOP F", 13.012, 80.2),
        ("G", "STOP G", 13.01, 80.21),
        ("H", "STOP H", 13.012, 80.21),
        ("T", "SPLIT T", 13.011, 80.21),
        ("K1", "STOP K1", 13.03, 80.2),
        ("K2", "STOP K2", 13.04, 80.2),
        ("M1", "MERGED M", 13.035, 80.2),
        ("M2", "MERGED M", 13.0351, 80.2),
        ("N", "CONFIRM N", 13.05, 80.2),
        ("X", "SPARE X", 13.06, 80.2),
        // a stop several splits and a move act on in one draft, together
        ("KOL", "KOLATHUR", 13.07, 80.22),
    ];
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, platform_code) VALUES {}",
        stops
            .iter()
            .map(|(id, name, lat, lon)| format!(
                "('{FEED}', '{id}', 'code-{id}', '{name}', {lat}, {lon}, 'Towards somewhere')"
            ))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id, deleted) VALUES \
         ('{FEED}', 'R1', '1', 'A To B', 'TESTAG', false), ('{FEED}', 'R2', '2', 'A To B', 'TESTAG', false), \
         ('{FEED}', 'R3', '3', 'S To B', 'TESTAG', false), ('{FEED}', 'R4', '4', 'GONE', 'TESTAG', true), \
         ('{FEED}', 'R5', '5', 'E To F', 'TESTAG', false), ('{FEED}', 'R6', '6', 'E To F', 'TESTAG', false), \
         ('{FEED}', 'R7', '7', 'G To H', 'TESTAG', false), ('{FEED}', 'R8', '8', 'GONE', 'TESTAG', true), \
         ('{FEED}', 'RM1', 'M1', 'K1 To K2', 'TESTAG', false), ('{FEED}', 'RM2', 'M2', 'K1 To K2', 'TESTAG', false), \
         ('{FEED}', 'RN', 'N', 'N To X', 'TESTAG', false), \
         ('{FEED}', 'RKA', '583D', 'A To B via Kolathur', 'TESTAG', false), \
         ('{FEED}', 'RKB', '500V', 'A To B via Kolathur', 'TESTAG', false), \
         ('{FEED}', 'RKC', 'KC', 'A To B via Kolathur', 'TESTAG', false)"
    ));
    let stop_rows: [(&str, i32, &str, &str, i32, &str); 38] = [
        ("R1", 1, "A", "NEW STOP", 1, "STOP A"),
        ("R1", 2, "S", "INTERMEDIATE STOP", 1, "STOP A"),
        ("R1", 3, "B", "NEW STOP", 2, "STOP B"),
        ("R2", 1, "A", "NEW STOP", 1, "STOP A"),
        ("R2", 2, "J", "JUMP STOP", 1, "STOP A"),
        ("R2", 4, "S", "NEW STOP", 2, "SUSPECT S"),
        ("R2", 6, "B", "NEW STOP", 3, "STOP B"),
        ("R3", 1, "S", "NEW STOP", 1, "SUSPECT S"),
        ("R3", 2, "B", "NEW STOP", 2, "STOP B"),
        ("R4", 1, "A", "NEW STOP", 1, "STOP A"),
        ("R4", 2, "S", "NEW STOP", 2, "SUSPECT S"),
        ("R4", 3, "B", "NEW STOP", 3, "STOP B"),
        ("R5", 1, "E", "NEW STOP", 1, "STOP E"),
        ("R5", 2, "T", "INTERMEDIATE STOP", 1, "STOP E"),
        ("R5", 3, "F", "NEW STOP", 2, "STOP F"),
        ("R6", 1, "E", "NEW STOP", 1, "STOP E"),
        // a fare defect R6 already has at T: a split must not be blocked by it
        ("R6", 2, "T", "INTERMEDIATE STOP", 1, "WRONG STAGE"),
        ("R6", 3, "F", "NEW STOP", 2, "STOP F"),
        ("R7", 1, "G", "NEW STOP", 1, "STOP G"),
        ("R7", 2, "T", "INTERMEDIATE STOP", 1, "STOP G"),
        ("R7", 3, "H", "NEW STOP", 2, "STOP H"),
        ("R8", 1, "G", "NEW STOP", 1, "STOP G"),
        ("R8", 2, "T", "NEW STOP", 2, "SPLIT T"),
        ("RM1", 1, "K1", "NEW STOP", 1, "STOP K1"),
        ("RM1", 2, "M1", "NEW STOP", 2, "MERGED M"),
        ("RM1", 3, "K2", "NEW STOP", 3, "STOP K2"),
        ("RM2", 1, "K1", "NEW STOP", 1, "STOP K1"),
        ("RM2", 2, "M2", "NEW STOP", 2, "MERGED M"),
        ("RM2", 3, "K2", "NEW STOP", 3, "STOP K2"),
        // three routes calling KOL, one per group a split or the final move acts on
        ("RKA", 1, "A", "NEW STOP", 1, "STOP A"),
        ("RKA", 2, "KOL", "INTERMEDIATE STOP", 1, "STOP A"),
        ("RKA", 3, "B", "NEW STOP", 2, "STOP B"),
        ("RKB", 1, "A", "NEW STOP", 1, "STOP A"),
        ("RKB", 2, "KOL", "INTERMEDIATE STOP", 1, "STOP A"),
        ("RKB", 3, "B", "NEW STOP", 2, "STOP B"),
        ("RKC", 1, "A", "NEW STOP", 1, "STOP A"),
        ("RKC", 2, "KOL", "INTERMEDIATE STOP", 1, "STOP A"),
        ("RKC", 3, "B", "NEW STOP", 2, "STOP B"),
    ];
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, \
                                      stop_name_override, provider_id) VALUES {}",
        stop_rows
            .iter()
            .map(|(r, q, st, t, n, name)| {
                // the route's own spelling of T must survive a split
                let spelling = if *st == "T" && *r == "R5" { "'T ON FIVE'" } else { "NULL" };
                format!("('{FEED}', '{r}', {q}, '{st}', '{t}', {n}, '{name}', {spelling}, '7')")
            })
            .collect::<Vec<_>>()
            .join(", ")
    ));
    // R2's shaping markers are not served; neither is J
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, \
                                      marker_id, marker_lat, marker_lon) VALUES \
         ('{FEED}', 'R2', 3, NULL, 'ROUTE CORRECTION', 1, 'STOP A', 'rc_R2_3', 13.0007, 80.2), \
         ('{FEED}', 'R2', 5, NULL, 'ROUTE CORRECTION', 2, 'SUSPECT S', 'rc_R2_5', 13.0015, 80.2)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name) VALUES \
         ('{FEED}', 'RN', 1, 'N', 'NEW STOP', 1, 'CONFIRM N'), ('{FEED}', 'RN', 2, 'X', 'NEW STOP', 2, 'SPARE X')"
    ));
    let groups = json!({"mixed_origins": true, "route_groups": [
        {"origin_stop_id": "T", "origin_name": "SPLIT T", "suspect": false, "route_ids": ["R7"], "route_numbers": ["7"]},
        {"origin_stop_id": "T_WEST", "origin_name": "SPLIT T", "suspect": true, "reason": "west road",
         "raw_lat": 13.011, "raw_lon": 80.2, "route_ids": ["R5", "R6"], "route_numbers": ["5", "6"]}]});
    // four groups, one per action a reviewer takes: the stop's own (moved),
    // two split off (583D, 500V), and one already resolved upstream - a
    // load-time snapshot the live routes no longer confirm
    let kol_groups = json!({"mixed_origins": true, "route_groups": [
        {"origin_stop_id": "KOL", "origin_name": "KOLATHUR", "suspect": false,
         "route_ids": ["RKC"], "route_numbers": ["KC"]},
        {"origin_stop_id": "KOL_A", "origin_name": "KOLATHUR NEAR A", "suspect": true,
         "reason": "583D area", "route_ids": ["RKA"], "route_numbers": ["583D"]},
        {"origin_stop_id": "KOL_B", "origin_name": "KOLATHUR NEAR B", "suspect": true,
         "reason": "500V area", "route_ids": ["RKB"], "route_numbers": ["500V"]},
        {"origin_stop_id": "KOL_OLD", "origin_name": "KOLATHUR OLD", "suspect": true,
         "reason": "already resolved upstream", "route_ids": ["RKD"], "route_numbers": ["300X"]}]});
    s.push(format!(
        "INSERT INTO gtfs_position_review (gtfs_id, batch, stop_id, original_stop_id, stop_name, reason, lat, lon, \
                                           raw_lat, raw_lon, suggested_lat, suggested_lon, suggested_source, evidence, status) VALUES \
         ('{FEED}', 'test-batch', 'S', 'S_OLD', 'SUSPECT S', 'off its routes', 13.001, 80.205, 13.0011, 80.2001, 13.001, 80.2, \
          'google: Suspect S', '{{\"route_rows\": 3, \"mixed_origins\": false}}', 'pending'), \
         ('{FEED}', 'test-batch', 'T', 'T', 'SPLIT T', 'two places on one point', 13.011, 80.21, NULL, NULL, NULL, NULL, \
          NULL, '{groups}', 'pending'), \
         ('{FEED}', 'test-batch', 'M1', 'M1', 'MERGED M', 'a duplicate', 13.035, 80.2, NULL, NULL, NULL, NULL, NULL, '{{}}', 'pending'), \
         ('{FEED}', 'test-batch', 'N', 'N', 'CONFIRM N', 'looks right', 13.05045, 80.2, NULL, NULL, NULL, NULL, NULL, '{{}}', 'pending'), \
         ('{FEED}', 'test-batch', 'KOL', 'KOL', 'KOLATHUR', 'routes from several places on one point', 13.07, 80.22, \
          NULL, NULL, NULL, NULL, NULL, '{kol_groups}', 'pending'), \
         ('{FEED}', 'old-batch', 'A', 'A', 'STOP A', 'an older load', 13.0, 80.2, NULL, NULL, NULL, NULL, NULL, '{{}}', 'superseded')"
    ));
    s.extend(reset_accounts(&[ADMIN, EDITOR, APPROVER]));
    s
}

async fn review_id(pool: &PgPool, stop: &str) -> i64 {
    scalar_i64(
        pool,
        format!(
            "SELECT review_id FROM gtfs_position_review WHERE gtfs_id = '{FEED}' AND stop_id = '{stop}' \
             AND status <> 'superseded'"
        ),
    )
    .await
}

#[actix_web::test]
async fn review_move_split_confirm_and_release() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("review-test-key");
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
    }
    for c in [&mut editor_c, &mut approver] {
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

    let new_set = |c: &Caller, title: &str| {
        c.req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": title}))
    };
    let feed_version = || {
        scalar_i64(
            &pool,
            format!("SELECT version FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
        )
    };
    let (rs, rt, rm, rn, rk) = (
        review_id(&pool, "S").await,
        review_id(&pool, "T").await,
        review_id(&pool, "M1").await,
        review_id(&pool, "N").await,
        review_id(&pool, "KOL").await,
    );
    let get = |c: &Caller, id: i64| c.req("GET", &format!("/position-reviews/{id}"));
    let post = |c: &Caller, id: i64, action: &str, body: Value| {
        c.req("POST", &format!("/position-reviews/{id}/{action}"))
            .set_json(body)
    };
    let release = |set: String| {
        [
            (0, format!("/change-sets/{set}/submit")),
            (1, format!("/change-sets/{set}/approve")),
            (1, format!("/change-sets/{set}/commit")),
        ]
    };

    // ======================================================== reads
    let (s, list, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/position-reviews"))
    );
    assert_eq!(s, 200, "{list}");
    let listed: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["stop_id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec!["S", "T", "M1", "N", "KOL"]);
    let first = &list["items"][0];
    for field in [
        "review_id",
        "stop_id",
        "original_stop_id",
        "stop_name",
        "reason",
        "lat",
        "lon",
        "raw_lat",
        "raw_lon",
        "suggested_lat",
        "suggested_lon",
        "suggested_source",
        "evidence",
        "status",
        "change_set_id",
        "change_set_title",
        "reviewed_by_email",
        "reviewed_at",
        "review_note",
        "batch",
    ] {
        assert!(first.get(field).is_some(), "{field} missing from {first}");
    }
    assert_eq!(first["original_stop_id"], "S_OLD");
    assert_eq!(first["suggested_source"], "google: Suspect S");
    assert_eq!(
        list["items"][1]["evidence"]["route_groups"][1]["route_ids"],
        json!(["R5", "R6"])
    );
    let (_, summary, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/position-reviews/summary"))
    );
    assert_eq!(
        summary,
        json!({"pending": 5, "approved": 0, "committed": 0, "confirmed": 0,
               "auto_fix": {"merge": 0, "move": 0, "choose": 0, "none": 0}})
    );
    let find = |q: &str| editor_c.req("GET", &format!("/feeds/{FEED}/position-reviews?{q}"));
    let (_, found, _) = call!(&app, find("status=superseded"));
    assert_eq!(found["items"].as_array().unwrap().len(), 1, "{found}");
    let (s, b, _) = call!(&app, find("status=pending,bogus"));
    assert_eq!((s, code_of(&b)), (400, "invalid_status"));
    for (q, want) in [
        ("q=S_OLD", "S"),
        ("q=S", "S"),
        ("q=confirm", "N"),
        ("q=M1", "M1"),
    ] {
        let (_, found, _) = call!(&app, find(q));
        assert_eq!(found["items"][0]["stop_id"], want, "{q}: {found}");
    }
    let (_, found, _) = call!(&app, find("bbox=13.009,80.19,13.02,80.22"));
    assert_eq!(found["items"].as_array().unwrap().len(), 1, "{found}");
    assert_eq!(found["items"][0]["stop_id"], "T");
    let (_, page1, _) = call!(&app, find("limit=3"));
    let cursor = page1["next_cursor"].as_str().unwrap().to_string();
    let (_, page2, _) = call!(&app, find(&format!("limit=3&cursor={cursor}")));
    assert_eq!(page2["items"][0]["stop_id"], "N");
    assert!(page2["next_cursor"].is_null());

    // the routes through S: R4 is deleted; R2's jump stop and markers are skipped
    let (s, detail, _) = call!(&app, get(&editor_c, rs));
    assert_eq!(s, 200, "{detail}");
    assert_eq!(detail["stop"]["stop_id"], "S");
    assert_eq!(detail["stop"]["name"], "SUSPECT S");
    assert_eq!(detail["problems"], json!([]), "{detail}");
    let routes = detail["routes"].as_array().unwrap();
    let route_ids: Vec<&str> = routes
        .iter()
        .map(|r| r["route_id"].as_str().unwrap())
        .collect();
    assert_eq!(route_ids, vec!["R1", "R2", "R3"]);
    assert_eq!(
        (
            routes[1]["sequence"].clone(),
            routes[1]["stop_type"].clone()
        ),
        (json!(4), json!("NEW STOP"))
    );
    assert_eq!(
        routes[1]["prev"],
        json!({"stop_id": "A", "name": "STOP A", "lat": 13.0, "lon": 80.2})
    );
    assert_eq!(routes[1]["next"]["stop_id"], "B");
    assert_eq!(routes[2]["prev"], Value::Null);
    assert_eq!(routes[2]["detour_m"], Value::Null);
    let detour = f(&detail["detour_m"]);
    assert!((883.0..884.5).contains(&detour), "{detail}");
    assert_eq!(f(&routes[0]["detour_m"]), detour);
    assert_eq!(detail["detour_m_after"], Value::Null);
    assert_eq!(detail["new_position"], Value::Null);
    // what a point would give, before anything is drafted
    let (s, what_if, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/position-reviews/{rs}?lat=13.001&lon=80.2")
        )
    );
    assert_eq!(s, 200, "{what_if}");
    assert!(f(&what_if["detour_m_after"]) < 1.0, "{what_if}");
    assert!((883.0..884.5).contains(&f(&what_if["detour_m"])));
    let (s, b, _) = call!(
        &app,
        editor_c.req("GET", &format!("/position-reviews/{rs}?lat=13.001"))
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_position"), "{b}");
    let (s, b, _) = call!(&app, get(&editor_c, 999_999_999));
    assert_eq!((s, code_of(&b)), (404, "review_not_found"));

    // ======================================================== move: discard, remove, release
    let (_, d1, _) = call!(&app, new_set(&editor_c, "move S (discarded)"));
    let d1 = d1["change_set_id"].as_str().unwrap().to_string();
    let s_version = scalar_i64(
        &pool,
        format!(
            "SELECT row_version::int8 FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'S'"
        ),
    )
    .await;
    for (body, want) in [
        (
            json!({"change_set_id": d1, "lat": 95.0, "lon": 80.2}),
            (400, "invalid_position"),
        ),
        (
            json!({"change_set_id": d1, "lat": 13.001, "lon": 80.205}),
            (400, "position_unchanged"),
        ),
        (
            json!({"change_set_id": d1, "lat": 13.001, "lon": 80.2, "bogus": 1}),
            (400, "invalid_json"),
        ),
    ] {
        let (s, b, _) = call!(&app, post(&editor_c, rs, "move", body.clone()));
        assert_eq!((s, code_of(&b)), want, "{body}: {b}");
    }
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "move",
            json!({"change_set_id": d1, "lat": 13.001, "lon": 80.2, "note": "  onto the road  "})
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (
            b["status"].clone(),
            b["change_set_id"].clone(),
            b["change_set_title"].clone()
        ),
        (json!("approved"), json!(d1), json!("move S (discarded)"))
    );
    assert_eq!(b["reviewed_by_email"], EDITOR);
    assert_eq!(b["review_note"], "onto the road");
    assert!(f(&b["detour_m_after"]) < 1.0, "{b}");
    assert_eq!(b["new_position"], json!({"lat": 13.001, "lon": 80.2}));
    assert_eq!(b["new_stop_id"], Value::Null);
    let move_change = b["change_id"].as_i64().unwrap();
    let (_, set_now, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{d1}")));
    let change = &set_now["changes"][0];
    assert_eq!(change["change_id"].as_i64(), Some(move_change));
    assert_eq!(
        (
            change["entity"].clone(),
            change["op"].clone(),
            change["entity_key"].clone()
        ),
        (json!("stop"), json!("update"), json!("S"))
    );
    assert_eq!(
        change["after"],
        json!({"lat": 13.001, "lon": 80.2, "position_review_id": rs})
    );
    assert_eq!(change["base_row_version"].as_i64(), Some(s_version));
    assert_eq!(change["before"]["stop_id"], "S");
    assert!(errors(&set_now).is_empty(), "{set_now}");
    assert_eq!(set_now["can_submit"], true, "{set_now}");
    // a second move in the SAME draft is a repeat action, not a re-approval
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "move",
            json!({"change_set_id": d1, "lat": 13.0012, "lon": 80.2})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "draft_conflict"), "{b}");
    assert_eq!(b["error"]["details"]["change_ids"], json!([move_change]));
    let (_, summary, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/position-reviews/summary"))
    );
    assert_eq!(summary["approved"], 1);
    // discarding the draft: pending again, the review fields cleared
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{d1}/discard"))
    );
    assert_eq!(s, 200);
    let (_, b, _) = call!(&app, get(&editor_c, rs));
    assert_eq!(
        (
            b["status"].clone(),
            b["change_set_id"].clone(),
            b["change_id"].clone(),
            b["reviewed_by_email"].clone(),
            b["review_note"].clone(),
            b["new_position"].clone(),
        ),
        (
            json!("pending"),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null
        ),
        "{b}"
    );
    // removing the change: pending again
    let (_, d2, _) = call!(&app, new_set(&editor_c, "move S (removed)"));
    let d2 = d2["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "move",
            json!({"change_set_id": d2, "lat": 13.001, "lon": 80.2})
        )
    );
    assert_eq!(s, 200, "{b}");
    let cid = b["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        editor_c.req("DELETE", &format!("/change-sets/{d2}/changes/{cid}"))
    );
    assert_eq!(s, 200, "{b}");
    let (_, b, _) = call!(&app, get(&editor_c, rs));
    assert_eq!(
        (b["status"].clone(), b["change_id"].clone()),
        (json!("pending"), Value::Null)
    );
    // a discarded draft takes no move
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "move",
            json!({"change_set_id": d1, "lat": 13.001, "lon": 80.2})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_not_draft"), "{b}");

    // move, adjust the point in the draft, release
    let (_, d3, _) = call!(&app, new_set(&editor_c, "move S"));
    let d3 = d3["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "move",
            json!({"change_set_id": d3, "lat": 13.0012, "lon": 80.2003})
        )
    );
    assert_eq!(s, 200, "{b}");
    let cid = b["change_id"].as_i64().unwrap();
    let put = |after: Value| {
        editor_c
            .req("PUT", &format!("/change-sets/{d3}/changes/{cid}"))
            .set_json(json!({"after": after}))
    };
    // the change stays the review's move
    let (s, b, _) = call!(&app, put(json!({"name": "RENAMED INSTEAD"})));
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
    let (s, b, _) = call!(
        &app,
        put(json!({"lat": 13.001, "lon": 80.2, "position_review_id": rt}))
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
    assert_eq!(b["error"]["details"]["code"], "position_review_mismatch");
    let (s, b, _) = call!(&app, put(json!({"lat": 13.001, "lon": 80.2})));
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b["changes"][0]["after"],
        json!({"lat": 13.001, "lon": 80.2, "position_review_id": rs})
    );
    let version_before = feed_version().await;
    for (who, path) in release(d3.clone()) {
        let c = if who == 0 { &editor_c } else { &approver };
        let (s, b, _) = call!(&app, c.req("POST", &path));
        assert_eq!(s, 200, "{path}: {b}");
    }
    assert_eq!(feed_version().await, version_before + 1);
    let moved = sqlx::query(&format!(
        "SELECT lat, lon FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'S'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (moved.get::<f64, _>("lat"), moved.get::<f64, _>("lon")),
        (13.001, 80.2)
    );
    let (_, b, _) = call!(&app, get(&editor_c, rs));
    assert_eq!(b["status"], "committed", "{b}");
    // 111 m from where it was loaded, but that is the review's own move
    assert_eq!(b["problems"], json!([]), "{b}");
    assert!(
        f(&b["detour_m"]) < 1.0 && f(&b["detour_m_after"]) < 1.0,
        "{b}"
    );
    assert_eq!(b["new_position"], json!({"lat": 13.001, "lon": 80.2}));
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rs,
            "move",
            json!({"change_set_id": d3, "lat": 13.0, "lon": 80.2})
        )
    );
    assert_eq!(s, 409, "{b}");

    // ======================================================== split
    let (_, detail_t, _) = call!(&app, get(&editor_c, rt));
    let calling: Vec<&str> = detail_t["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["route_id"].as_str().unwrap())
        .collect();
    assert_eq!(calling, vec!["R5", "R6", "R7"]);
    assert!(f(&detail_t["detour_m"]) > 1900.0, "{detail_t}");
    let (_, what_if, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/position-reviews/{rt}?lat=13.011&lon=80.2&route_ids=R5,R6")
        )
    );
    assert!(f(&what_if["detour_m_after"]) < 1.0, "{what_if}");
    let split = |set: &str, routes: Value| json!({"change_set_id": set, "route_ids": routes, "lat": 13.011, "lon": 80.2});
    let (_, p1, _) = call!(&app, new_set(&editor_c, "split T (taken back)"));
    let p1 = p1["change_set_id"].as_str().unwrap().to_string();
    for (routes, want) in [
        (json!(["R5", "R6", "R7"]), vec!["would_empty_stop"]),
        (json!(["R5", "R1"]), vec!["route_not_at_stop"]),
        (json!(["R8"]), vec!["route_not_at_stop"]),
        (json!([]), vec!["route_ids_required"]),
        (json!(["R5", "R5"]), vec!["route_listed_twice"]),
    ] {
        let (s, b, _) = call!(
            &app,
            post(&editor_c, rt, "split", split(&p1, routes.clone()))
        );
        assert_eq!((s, code_of(&b)), (400, "invalid_split"), "{routes}: {b}");
        assert_eq!(
            codes(&b["error"]["details"]["problems"]),
            want,
            "{routes}: {b}"
        );
    }
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rt,
            "split",
            json!({"change_set_id": p1, "route_ids": ["R5"], "lat": 0.0, "lon": 0.0})
        )
    );
    assert_eq!(
        codes(&b["error"]["details"]["problems"]),
        vec!["invalid_position"],
        "{s} {b}"
    );
    let (_, set_now, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{p1}")));
    assert_eq!(
        set_now["change_count"], 0,
        "refusals add nothing: {set_now}"
    );
    // split two of the three routes
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rt,
            "split",
            json!({"change_set_id": p1, "route_ids": ["R6", "R5"], "lat": 13.011, "lon": 80.2, "note": "west road"})
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["status"], "approved");
    let new_stop = b["new_stop_id"].as_str().unwrap().to_string();
    assert!(new_stop.starts_with("ed_") && new_stop.len() == 13, "{b}");
    assert_eq!(b["split_route_ids"], json!(["R6", "R5"]));
    assert!(f(&b["detour_m_after"]) < 1.0, "{b}");
    assert_eq!(b["new_position"], json!({"lat": 13.011, "lon": 80.2}));
    let create_id = b["change_id"].as_i64().unwrap();
    let (_, set_now, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{p1}")));
    let changes = set_now["changes"].as_array().unwrap();
    let shape: Vec<(String, String, String)> = changes
        .iter()
        .map(|c| {
            (
                c["entity"].as_str().unwrap().into(),
                c["op"].as_str().unwrap().into(),
                c["entity_key"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            ("stop".into(), "create".into(), new_stop.clone()),
            ("route_stops".into(), "replace".into(), "R6".into()),
            ("route_stops".into(), "replace".into(), "R5".into()),
        ]
    );
    assert_eq!(changes[0]["change_id"].as_i64(), Some(create_id));
    assert_eq!(
        changes[0]["after"],
        json!({"stop_id": new_stop, "name": "SPLIT T", "lat": 13.011, "lon": 80.2, "position_review_id": rt})
    );
    let (_, r5_live, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R5"))
    );
    let r5_change = &changes[2];
    assert_eq!(r5_change["after"]["base_rows_hash"], r5_live["rows_hash"]);
    assert_eq!(r5_change["after"]["position_review_id"], rt);
    assert_eq!(r5_change["after"]["rows"][1]["stop_id"], new_stop.as_str());
    assert_eq!(
        r5_change["after"]["rows"][1]["stop_name_override"],
        "T ON FIVE"
    );
    assert_eq!(r5_change["after"]["rows"][0]["stop_id"], "E");
    assert_eq!(r5_change["before"], r5_live["rows"]);
    assert!(errors(&set_now).is_empty(), "{set_now}");
    let r6_change_id = changes[1]["change_id"].clone();
    let old_defect = set_now["validation"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["change_id"] == r6_change_id && v["code"] == "fare_stage_mismatch")
        .cloned()
        .unwrap_or_else(|| panic!("{set_now}"));
    assert_eq!(old_defect["level"], "warning");
    assert!(
        old_defect["message"]
            .as_str()
            .unwrap()
            .ends_with("(already present before this edit)"),
        "{old_defect}"
    );
    assert_eq!(set_now["can_submit"], true, "{set_now}");
    // the draft already changes these routes, or the stop
    let (s, b, _) = call!(&app, post(&editor_c, rs, "confirm", json!({})));
    assert_eq!(
        (s, code_of(&b)),
        (409, "review_not_pending"),
        "committed: {b}"
    );
    let (_, c1, _) = call!(&app, new_set(&editor_c, "conflicting draft"));
    let c1 = c1["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{c1}/changes"))
            .set_json(json!({"entity": "stop", "op": "update", "entity_key": "T", "after": {"name": "SPLIT T RENAMED"}}))
    );
    assert_eq!(s, 201, "{b}");
    let rename = b["change_id"].as_i64().unwrap();
    // T's review is approved in p1: a different draft is refused before the
    // conflict with c1's own rename is even checked
    let (s, b, _) = call!(
        &app,
        post(&editor_c, rt, "split", split(&c1, json!(["R5"])))
    );
    assert_eq!((s, code_of(&b)), (409, "review_in_other_draft"), "{b}");
    assert_eq!(b["error"]["details"]["change_set_id"], p1.as_str());
    // removing one of the stop lists leaves the rest, and the review approved
    let r6_change = changes[1]["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        editor_c.req("DELETE", &format!("/change-sets/{p1}/changes/{r6_change}"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["change_count"], 2);
    let (_, b, _) = call!(&app, get(&editor_c, rt));
    assert_eq!(b["status"], "approved");
    assert_eq!(b["split_route_ids"], json!(["R5"]));
    // removing the new stop takes its stop lists with it
    let (s, b, _) = call!(
        &app,
        editor_c.req("DELETE", &format!("/change-sets/{p1}/changes/{create_id}"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["change_count"], 0, "{b}");
    let (_, b, _) = call!(&app, get(&editor_c, rt));
    assert_eq!(
        (
            b["status"].clone(),
            b["change_id"].clone(),
            b["new_stop_id"].clone()
        ),
        (json!("pending"), Value::Null, Value::Null),
        "{b}"
    );
    let returned = scalar_i64(
        &pool,
        format!(
            "SELECT count(*) FROM gtfs_audit_log WHERE change_set_id = '{p1}' AND action = 'position_review_returned' \
             AND detail->>'reason' = 'change_removed' AND detail->'removed_change_ids' = '[{}]'::jsonb",
            changes[2]["change_id"]
        ),
    )
    .await;
    assert_eq!(returned, 1);
    // now pending, T collides with the rename in c1, and so would a stop list
    let (s, b, _) = call!(
        &app,
        post(&editor_c, rt, "split", split(&c1, json!(["R5"])))
    );
    assert_eq!((s, code_of(&b)), (409, "draft_conflict"), "{b}");
    assert_eq!(b["error"]["details"]["change_ids"], json!([rename]));
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{c1}/discard"))
    );
    assert_eq!(s, 200);
    let (_, c2, _) = call!(&app, new_set(&editor_c, "a stop list of R6"));
    let c2 = c2["change_set_id"].as_str().unwrap().to_string();
    let (_, r6_live, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R6"))
    );
    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{c2}/changes"))
            .set_json(json!({"entity": "route_stops", "op": "replace", "entity_key": "R6",
                             "after": {"base_rows_hash": r6_live["rows_hash"], "rows": r6_live["rows"].as_array().unwrap().iter().map(|r| json!({
                                 "stop_id": r["stop_id"], "stop_type": r["stop_type"], "stage_no": r["stage_no"], "stage_name": r["stage_name"]})).collect::<Vec<_>>()}}))
    );
    assert_eq!(s, 201, "{b}");
    let list_change = b["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        post(&editor_c, rt, "split", split(&c2, json!(["R5", "R6"])))
    );
    assert_eq!((s, code_of(&b)), (409, "draft_conflict"), "{b}");
    assert_eq!(b["error"]["details"]["change_ids"], json!([list_change]));
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{c2}/discard"))
    );
    assert_eq!(s, 200);

    // split again and release: the rows point at the new stop
    let (_, p2, _) = call!(&app, new_set(&editor_c, "split T"));
    let p2 = p2["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(&editor_c, rt, "split", split(&p2, json!(["R5", "R6"])))
    );
    assert_eq!(s, 200, "{b}");
    let new_stop = b["new_stop_id"].as_str().unwrap().to_string();
    let version_before = feed_version().await;
    for (who, path) in release(p2.clone()) {
        let c = if who == 0 { &editor_c } else { &approver };
        let (s, b, _) = call!(&app, c.req("POST", &path));
        assert_eq!(s, 200, "{path}: {b}");
    }
    assert_eq!(feed_version().await, version_before + 1);
    let at_rows: Vec<(String, i32, Option<String>, Option<String>)> = sqlx::query(&format!(
        "SELECT route_id, sequence, stop_id, stop_name_override FROM gtfs_route_stop \
         WHERE gtfs_id = '{FEED}' AND route_id IN ('R5', 'R6', 'R7') AND sequence = 2 ORDER BY route_id"
    ))
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get("route_id"), r.get("sequence"), r.get("stop_id"), r.get("stop_name_override")))
    .collect();
    assert_eq!(
        at_rows,
        vec![
            (
                "R5".into(),
                2,
                Some(new_stop.clone()),
                Some("T ON FIVE".into())
            ),
            ("R6".into(), 2, Some(new_stop.clone()), None),
            ("R7".into(), 2, Some("T".into()), None),
        ]
    );
    // the new stop inherits nothing but its name
    let made = sqlx::query(&format!(
        "SELECT name, lat, lon, parent_station, platform_code, stop_code, deleted FROM gtfs_stop \
         WHERE gtfs_id = '{FEED}' AND stop_id = '{new_stop}'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(made.get::<String, _>("name"), "SPLIT T");
    assert_eq!(
        (made.get::<f64, _>("lat"), made.get::<f64, _>("lon")),
        (13.011, 80.2)
    );
    assert_eq!(made.get::<Option<String>, _>("parent_station"), None);
    assert_eq!(made.get::<Option<String>, _>("platform_code"), None);
    assert_eq!(
        made.get::<Option<String>, _>("stop_code").as_deref(),
        Some(new_stop.as_str())
    );
    let (_, b, _) = call!(&app, get(&editor_c, rt));
    assert_eq!(b["status"], "committed", "{b}");
    assert_eq!(b["routes"].as_array().unwrap().len(), 1, "{b}");
    assert_eq!(b["split_route_ids"], json!(["R5", "R6"]));
    assert!(f(&b["detour_m_after"]) < 1.0, "{b}");
    assert!(f(&b["detour_m"]) < 1.0, "only R7 calls at T now: {b}");

    // ======================================================== confirm, reopen
    let (_, b, _) = call!(&app, get(&editor_c, rn));
    assert_eq!(codes(&b["problems"]), vec!["moved_since_load"], "{b}");
    assert_eq!(b["problems"][0]["level"], "warning");
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rn,
            "confirm",
            json!({"note": "the kerb is here"})
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (
            b["status"].clone(),
            b["review_note"].clone(),
            b["reviewed_by_email"].clone()
        ),
        (json!("confirmed"), json!("the kerb is here"), json!(EDITOR))
    );
    let (s, b, _) = call!(&app, post(&editor_c, rn, "confirm", json!({})));
    assert_eq!((s, code_of(&b)), (409, "review_not_pending"), "{b}");
    let (_, p3, _) = call!(&app, new_set(&editor_c, "move N"));
    let p3 = p3["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rn,
            "move",
            json!({"change_set_id": p3, "lat": 13.0501, "lon": 80.2})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "review_not_pending"), "{b}");
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/position-reviews/{rn}/reopen"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (
            b["status"].clone(),
            b["review_note"].clone(),
            b["reviewed_by_email"].clone()
        ),
        (json!("pending"), Value::Null, Value::Null)
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/position-reviews/{rn}/reopen"))
    );
    assert_eq!((s, code_of(&b)), (409, "review_not_confirmed"), "{b}");
    // a confirm without a body has no note
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/position-reviews/{rn}/confirm"))
    );
    assert_eq!(
        (s, b["status"].clone(), b["review_note"].clone()),
        (200, json!("confirmed"), Value::Null)
    );

    // ======================================================== merged away
    let (_, g1, _) = call!(&app, new_set(&editor_c, "merge M1 into M2"));
    let g1 = g1["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{g1}/changes"))
            .set_json(json!({"entity": "stop", "op": "merge", "entity_key": "M1", "after": {"into_stop_id": "M2"}}))
    );
    assert_eq!(s, 201, "{b}");
    let merge = b["change_id"].as_i64().unwrap();
    // in the draft that merges it away, the stop cannot be moved or split
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rm,
            "move",
            json!({"change_set_id": g1, "lat": 13.0352, "lon": 80.2})
        )
    );
    assert_eq!((s, code_of(&b)), (400, "review_has_problems"), "{b}");
    assert_eq!(
        codes(&b["error"]["details"]["problems"]),
        vec!["stop_merged_away"]
    );
    assert_eq!(
        b["error"]["details"]["problems"][0]["message"],
        format!("stop M1 is merged into M2 by change {merge} in the draft")
    );
    for (who, path) in release(g1.clone()) {
        let c = if who == 0 { &editor_c } else { &approver };
        let (s, b, _) = call!(&app, c.req("POST", &path));
        assert_eq!(s, 200, "{path}: {b}");
    }
    let (_, b, _) = call!(&app, get(&editor_c, rm));
    assert_eq!(codes(&b["problems"]), vec!["stop_merged_away"], "{b}");
    assert_eq!(b["problems"][0]["level"], "error");
    assert_eq!(b["stop"]["deleted"], true);
    assert_eq!(b["routes"], json!([]));
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rm,
            "move",
            json!({"change_set_id": p3, "lat": 13.0352, "lon": 80.2})
        )
    );
    assert_eq!((s, code_of(&b)), (400, "review_has_problems"), "{b}");
    assert_eq!(
        codes(&b["error"]["details"]["problems"]),
        vec!["stop_merged_away"]
    );
    let (s, b, _) = call!(
        &app,
        post(&editor_c, rm, "split", split(&p3, json!(["RM1"])))
    );
    assert_eq!((s, code_of(&b)), (400, "review_has_problems"), "{b}");
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{p3}/discard"))
    );
    assert_eq!(s, 200);

    // ======================================================== several actions, one review
    // KOLATHUR carries three groups: 583D (RKA) and 500V (RKB) from elsewhere,
    // and its own KC (RKC). A reviewer splits the first two off and moves the
    // stop itself for the third, all before releasing one draft.
    let (_, dk1, _) = call!(&app, new_set(&editor_c, "kolathur: split, split, move"));
    let dk1 = dk1["change_set_id"].as_str().unwrap().to_string();
    let kol_split = |set: &str, route_id: &str, lat: f64, lon: f64, name: &str| json!({"change_set_id": set, "route_ids": [route_id], "lat": lat, "lon": lon, "name": name});

    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rk,
            "split",
            kol_split(&dk1, "RKA", 13.071, 80.221, "KOLATHUR (583D)")
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["status"], "approved");
    let new_a = b["new_stop_id"].as_str().unwrap().to_string();
    assert!(new_a.starts_with("ed_"), "{new_a}");

    // a second split in the same draft: the review is already approved there
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rk,
            "split",
            kol_split(&dk1, "RKB", 13.072, 80.223, "KOLATHUR (500V)")
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["status"], "approved");
    let new_b = b["new_stop_id"].as_str().unwrap().to_string();
    assert!(new_b.starts_with("ed_") && new_b != new_a, "{new_b}");

    // a different draft may not act on this review while it has changes in dk1
    let (_, dk2, _) = call!(&app, new_set(&editor_c, "kolathur: a second draft"));
    let dk2 = dk2["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rk,
            "move",
            json!({"change_set_id": dk2, "lat": 13.075, "lon": 80.225})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "review_in_other_draft"), "{b}");
    assert_eq!(b["error"]["details"]["change_set_id"], dk1.as_str());
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rk,
            "split",
            kol_split(&dk2, "RKC", 13.08, 80.23, "no")
        )
    );
    assert_eq!((s, code_of(&b)), (409, "review_in_other_draft"), "{b}");
    assert_eq!(b["error"]["details"]["change_set_id"], dk1.as_str());
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{dk2}/discard"))
    );
    assert_eq!(s, 200);

    // move the stop itself, in the same draft as the two splits
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rk,
            "move",
            json!({"change_set_id": dk1, "lat": 13.075, "lon": 80.225})
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["status"], "approved");

    // the draft has 2 creates, their route replaces, and 1 stop/update, in order
    let (_, set_now, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{dk1}")));
    assert_eq!(set_now["change_count"], 5, "{set_now}");
    assert!(errors(&set_now).is_empty(), "{set_now}");
    let kol_changes = set_now["changes"].as_array().unwrap();
    let shape: Vec<(String, String, String)> = kol_changes
        .iter()
        .map(|c| {
            (
                c["entity"].as_str().unwrap().into(),
                c["op"].as_str().unwrap().into(),
                c["entity_key"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            ("stop".into(), "create".into(), new_a.clone()),
            ("route_stops".into(), "replace".into(), "RKA".into()),
            ("stop".into(), "create".into(), new_b.clone()),
            ("route_stops".into(), "replace".into(), "RKB".into()),
            ("stop".into(), "update".into(), "KOL".into()),
        ]
    );
    let create_a_id = kol_changes[0]["change_id"].as_i64().unwrap();
    let replace_a_id = kol_changes[1]["change_id"].as_i64().unwrap();
    let create_b_id = kol_changes[2]["change_id"].as_i64().unwrap();
    let replace_b_id = kol_changes[3]["change_id"].as_i64().unwrap();
    let move_id = kol_changes[4]["change_id"].as_i64().unwrap();

    // the review's draft_actions list all three, in change order; the singular
    // fields are the latest action's (the move)
    let (_, detail, _) = call!(&app, get(&editor_c, rk));
    assert_eq!(detail["status"], "approved", "{detail}");
    assert_eq!(detail["change_id"], create_a_id, "{detail}");
    let kol_actions = detail["draft_actions"].as_array().unwrap();
    assert_eq!(kol_actions.len(), 3, "{kol_actions:?}");
    assert_eq!(
        (
            kol_actions[0]["kind"].clone(),
            kol_actions[0]["change_id"].clone()
        ),
        (json!("split"), json!(create_a_id))
    );
    assert_eq!(kol_actions[0]["route_ids"], json!(["RKA"]));
    assert_eq!(
        (
            kol_actions[1]["kind"].clone(),
            kol_actions[1]["change_id"].clone()
        ),
        (json!("split"), json!(create_b_id))
    );
    assert_eq!(kol_actions[1]["route_ids"], json!(["RKB"]));
    assert_eq!(
        (
            kol_actions[2]["kind"].clone(),
            kol_actions[2]["change_id"].clone()
        ),
        (json!("move"), json!(move_id))
    );
    assert_eq!(
        detail["new_position"],
        json!({"lat": 13.075, "lon": 80.225})
    );
    assert_eq!(detail["new_stop_id"], Value::Null);
    assert_eq!(detail["split_route_ids"], Value::Null);

    // removing split A's create takes only its own route replace with it
    let (s, b, _) = call!(
        &app,
        editor_c.req(
            "DELETE",
            &format!("/change-sets/{dk1}/changes/{create_a_id}")
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["change_count"], 3, "{b}");
    let remaining_ids: Vec<i64> = b["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["change_id"].as_i64().unwrap())
        .collect();
    assert_eq!(remaining_ids, vec![create_b_id, replace_b_id, move_id]);
    let (_, detail, _) = call!(&app, get(&editor_c, rk));
    assert_eq!(detail["status"], "approved", "{detail}");
    assert_eq!(detail["change_id"], create_b_id, "{detail}");
    let cascade_audit = scalar_i64(
        &pool,
        format!(
            "SELECT count(*) FROM gtfs_audit_log WHERE change_set_id = '{dk1}' AND action = 'change_removed' \
             AND detail->>'change_id' = '{replace_a_id}' AND detail->>'review_id' = '{rk}' \
             AND detail->>'new_stop_id' = '{new_a}' AND detail->>'reason' = 'split_removed'"
        ),
    )
    .await;
    assert_eq!(cascade_audit, 1);

    // remove everything left: split B's create (cascading its replace), then the move
    let (s, b, _) = call!(
        &app,
        editor_c.req(
            "DELETE",
            &format!("/change-sets/{dk1}/changes/{create_b_id}")
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["change_count"], 1, "{b}");
    let (_, detail, _) = call!(&app, get(&editor_c, rk));
    assert_eq!(
        (detail["status"].clone(), detail["change_id"].clone()),
        (json!("approved"), json!(move_id)),
        "{detail}"
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req("DELETE", &format!("/change-sets/{dk1}/changes/{move_id}"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["change_count"], 0, "{b}");
    let (_, detail, _) = call!(&app, get(&editor_c, rk));
    assert_eq!(
        (
            detail["status"].clone(),
            detail["change_set_id"].clone(),
            detail["change_id"].clone(),
            detail["draft_actions"].clone(),
        ),
        (json!("pending"), Value::Null, Value::Null, json!([])),
        "{detail}"
    );
    // dk1 is now empty (nothing left to submit); tidy it away
    let (s, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{dk1}/discard"))
    );
    assert_eq!(s, 200);

    // rebuild the same three actions in a fresh draft, and release it
    let (_, dk3, _) = call!(&app, new_set(&editor_c, "kolathur: commit"));
    let dk3 = dk3["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rk,
            "split",
            kol_split(&dk3, "RKA", 13.071, 80.221, "KOLATHUR (583D)")
        )
    );
    assert_eq!(s, 200, "{b}");
    let new_a = b["new_stop_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rk,
            "split",
            kol_split(&dk3, "RKB", 13.072, 80.223, "KOLATHUR (500V)")
        )
    );
    assert_eq!(s, 200, "{b}");
    let new_b = b["new_stop_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        post(
            &editor_c,
            rk,
            "move",
            json!({"change_set_id": dk3, "lat": 13.075, "lon": 80.225})
        )
    );
    assert_eq!(s, 200, "{b}");

    let kolathur_version_before = feed_version().await;
    for (who, path) in release(dk3.clone()) {
        let c = if who == 0 { &editor_c } else { &approver };
        let (s, b, _) = call!(&app, c.req("POST", &path));
        assert_eq!(s, 200, "{path}: {b}");
    }
    assert_eq!(feed_version().await, kolathur_version_before + 1);

    // RKA and RKB call the new stops now; RKC, never split, still calls KOL -
    // at the point the move set
    let kol_rows: Vec<(String, Option<String>)> = sqlx::query(&format!(
        "SELECT route_id, stop_id FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' \
         AND route_id IN ('RKA', 'RKB', 'RKC') AND sequence = 2 ORDER BY route_id"
    ))
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get("route_id"), r.get("stop_id")))
    .collect();
    assert_eq!(
        kol_rows,
        vec![
            ("RKA".into(), Some(new_a.clone())),
            ("RKB".into(), Some(new_b.clone())),
            ("RKC".into(), Some("KOL".into())),
        ]
    );
    let kol_stop = sqlx::query(&format!(
        "SELECT lat, lon, deleted FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'KOL'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (kol_stop.get::<f64, _>("lat"), kol_stop.get::<f64, _>("lon")),
        (13.075, 80.225)
    );
    assert!(!kol_stop.get::<bool, _>("deleted"));
    let (_, detail, _) = call!(&app, get(&editor_c, rk));
    assert_eq!(detail["status"], "committed", "{detail}");
    assert!(detail["change_id"].is_i64(), "{detail}");
    assert_eq!(detail["change_set_id"], dk3.as_str());
    assert_eq!(
        detail["new_position"],
        json!({"lat": 13.075, "lon": 80.225})
    );

    // ======================================================== summary and audit
    let (_, summary, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/position-reviews/summary"))
    );
    assert_eq!(
        summary,
        json!({"pending": 1, "approved": 0, "committed": 3, "confirmed": 1,
               "auto_fix": {"merge": 0, "move": 0, "choose": 0, "none": 0}})
    );
    let actions: Vec<String> = sqlx::query(&format!(
        "SELECT DISTINCT action FROM gtfs_audit_log WHERE gtfs_id = '{FEED}' AND action LIKE 'position_review_%'"
    ))
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get("action"))
    .collect();
    for want in [
        "position_review_moved",
        "position_review_split",
        "position_review_confirmed",
        "position_review_reopened",
        "position_review_returned",
        "position_review_committed",
    ] {
        assert!(
            actions.iter().any(|a| a == want),
            "{want} missing from {actions:?}"
        );
    }
    let split_audit = scalar_i64(
        &pool,
        format!(
            "SELECT count(*) FROM gtfs_audit_log WHERE change_set_id = '{p2}' AND action = 'position_review_split' \
             AND detail->>'new_stop_id' = '{new_stop}' AND detail->'route_ids' = '[\"R5\", \"R6\"]'::jsonb \
             AND detail->>'change_set_id' = '{p2}'"
        ),
    )
    .await;
    assert_eq!(split_audit, 1);
    let discarded = scalar_i64(
        &pool,
        format!(
            "SELECT count(*) FROM gtfs_audit_log WHERE change_set_id = '{d1}' AND action = 'position_review_returned' \
             AND detail->>'reason' = 'change_set_discarded'"
        ),
    )
    .await;
    assert_eq!(discarded, 1);

    exec(&pool, &clear_feed(FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}

// ---------------------------------------------------------------- timings

const PERF_FEED: &str = "editor_review_perf_feed";
const PERF_ADMIN: &str = "admin@editor-review-perf-test.invalid";

/// The detail, a move and a split at the busiest stops of a copy of chennai_bus
/// (200+ routes each), and a detail of every real chennai_bus review, which is
/// only read. Prints the timings.
#[actix_web::test]
async fn position_review_timings() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let source = scalar_i64(
        &pool,
        "SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = 'chennai_bus'",
    )
    .await;
    if source < 5000 {
        eprintln!("chennai_bus has {source} route rows; skipping the review timings");
        return;
    }
    let mut setup = clear_feed(PERF_FEED);
    setup.extend([
        format!("INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{PERF_FEED}', 'Review timing copy of chennai_bus')"),
        format!(
            "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, location_type, platform_code, cluster_id, deleted) \
             SELECT '{PERF_FEED}', stop_id, stop_code, name, lat, lon, location_type, platform_code, cluster_id, deleted \
             FROM gtfs_stop WHERE gtfs_id = 'chennai_bus'"
        ),
        format!(
            "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type, agency_id, deleted) \
             SELECT '{PERF_FEED}', route_id, short_name, long_name, route_type, agency_id, deleted FROM gtfs_route WHERE gtfs_id = 'chennai_bus'"
        ),
        format!(
            "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, marker_id, marker_name, \
                                          marker_lat, marker_lon, stop_name_override, provider_id) \
             SELECT '{PERF_FEED}', route_id, sequence, stop_id, stop_type, stage_no, stage_name, marker_id, marker_name, \
                    marker_lat, marker_lon, stop_name_override, provider_id FROM gtfs_route_stop WHERE gtfs_id = 'chennai_bus'"
        ),
        // the three stops the most routes call at
        format!(
            "INSERT INTO gtfs_position_review (gtfs_id, batch, stop_id, original_stop_id, stop_name, reason, lat, lon) \
             SELECT '{PERF_FEED}', 'timing', s.stop_id, s.stop_id, s.name, 'busy', s.lat, s.lon FROM gtfs_stop s \
             JOIN (SELECT stop_id, count(DISTINCT route_id) AS n FROM gtfs_route_stop \
                   WHERE gtfs_id = '{PERF_FEED}' AND stop_id IS NOT NULL GROUP BY stop_id ORDER BY n DESC, stop_id LIMIT 3) b \
               ON b.stop_id = s.stop_id WHERE s.gtfs_id = '{PERF_FEED}'"
        ),
    ]);
    setup.extend(reset_accounts(&[PERF_ADMIN]));
    let copy = Instant::now();
    exec(&pool, &setup).await;
    eprintln!(
        "[timing] copied chennai_bus into {PERF_FEED} in {:?}",
        copy.elapsed()
    );

    let signer = TestSigner::generate("review-perf-key");
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

    let (s, list, _) = call!(
        &app,
        admin.req("GET", &format!("/feeds/{PERF_FEED}/position-reviews"))
    );
    assert_eq!(s, 200, "{list}");
    let busy: Vec<(i64, String)> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["review_id"].as_i64().unwrap(),
                x["stop_id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(busy.len(), 3);
    for (id, stop) in &busy {
        let calls = scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_route_stop rs JOIN gtfs_route r USING (gtfs_id, route_id) \
                 WHERE rs.gtfs_id = '{PERF_FEED}' AND rs.stop_id = '{stop}' AND NOT r.deleted"
            ),
        )
        .await;
        let mut took = Vec::new();
        for _ in 0..3 {
            let t = Instant::now();
            let (s, b, _) = call!(&app, admin.req("GET", &format!("/position-reviews/{id}")));
            took.push(t.elapsed());
            assert_eq!(s, 200, "{b}");
            assert_eq!(b["routes"].as_array().unwrap().len() as i64, calls);
        }
        eprintln!("[timing] detail of {stop}, {calls} calls: {took:?}");
        assert!(took[2] < Duration::from_secs(2), "{stop}: {took:?}");
    }

    // a split of every route but one at the busiest stop, and a move at the next.
    // The details above ran before the copy had planner statistics; the shared
    // loaders a split uses assume a live feed's, so give the copy those first.
    exec(
        &pool,
        &["ANALYZE gtfs_stop, gtfs_route, gtfs_route_stop".to_string()],
    )
    .await;
    let (_, set, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{PERF_FEED}/change-sets"))
            .set_json(json!({"title": "timings"}))
    );
    let set = set["change_set_id"].as_str().unwrap().to_string();
    let (id, stop) = &busy[0];
    let (_, detail, _) = call!(&app, admin.req("GET", &format!("/position-reviews/{id}")));
    let mut routes: Vec<String> = detail["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["route_id"].as_str().unwrap().to_string())
        .collect();
    routes.dedup();
    routes.pop();
    let (lat, lon) = (
        f(&detail["stop"]["lat"]) + 0.0003,
        f(&detail["stop"]["lon"]),
    );
    let t = Instant::now();
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/position-reviews/{id}/split"))
            .set_json(json!({"change_set_id": set, "route_ids": routes, "lat": lat, "lon": lon}))
    );
    let took = t.elapsed();
    assert_eq!(s, 200, "{}", b["error"]);
    eprintln!(
        "[timing] split of {} routes off {stop}: {took:?} (detour_m {} -> {})",
        routes.len(),
        b["detour_m"],
        b["detour_m_after"]
    );
    let (id, stop) = &busy[1];
    let t = Instant::now();
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/position-reviews/{id}/move"))
            .set_json(json!({"change_set_id": set, "lat": lat - 0.01, "lon": lon}))
    );
    assert_eq!(s, 200, "{}", b["error"]);
    eprintln!("[timing] move of {stop}: {:?}", t.elapsed());
    let t = Instant::now();
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set}/discard"))
    );
    assert_eq!(s, 200, "{}", b["error"]);
    eprintln!(
        "[timing] discard, {} changes: {:?}",
        routes.len() + 2,
        t.elapsed()
    );

    // every real chennai_bus review, read only
    let real = scalar_i64(
        &pool,
        "SELECT count(*) FROM gtfs_position_review WHERE gtfs_id = 'chennai_bus'",
    )
    .await;
    if real > 0 {
        let (s, summary, _) = call!(
            &app,
            admin.req("GET", "/feeds/chennai_bus/position-reviews/summary")
        );
        assert_eq!(s, 200, "{summary}");
        let mut cursor: Option<String> = None;
        let (mut seen, mut slowest, mut mixed) = (0, (Duration::ZERO, String::new()), 0);
        let started = Instant::now();
        loop {
            let q = match &cursor {
                Some(c) => format!(
                    "status=pending,approved,committed,confirmed,superseded&limit=100&cursor={c}"
                ),
                None => "status=pending,approved,committed,confirmed,superseded&limit=100".into(),
            };
            let (s, page, _) = call!(
                &app,
                admin.req("GET", &format!("/feeds/chennai_bus/position-reviews?{q}"))
            );
            assert_eq!(s, 200, "{page}");
            for item in page["items"].as_array().unwrap() {
                let id = item["review_id"].as_i64().unwrap();
                let t = Instant::now();
                let (s, b, _) = call!(&app, admin.req("GET", &format!("/position-reviews/{id}")));
                let took = t.elapsed();
                assert_eq!(s, 200, "{id}: {b}");
                assert!(b["routes"].is_array() && b["problems"].is_array(), "{b}");
                if item["evidence"]["mixed_origins"] == true {
                    mixed += 1;
                }
                if took > slowest.0 {
                    slowest = (
                        took,
                        format!(
                            "{} ({} calls)",
                            b["stop_id"],
                            b["routes"].as_array().unwrap().len()
                        ),
                    );
                }
                seen += 1;
            }
            cursor = page["next_cursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        assert!(seen > 0 && seen <= real + 5, "{seen} of {real}");
        eprintln!(
            "[timing] chennai_bus: {seen} reviews ({mixed} with mixed origins), summary {summary}, \
             every detail in {:?}, slowest {:?} at {}",
            started.elapsed(),
            slowest.0,
            slowest.1
        );
    }

    exec(&pool, &clear_feed(PERF_FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}
