//! Stop details (docs/gtfs-editor.md section 11), end to end against a real
//! Postgres holding the editor schema (db/gtfs_editor/0001..0012): a description
//! on stops and stations, a platform label on a stop that is in no station, the
//! field the public stop JSON gains, and the `stop_updates` bulk kind - dry run,
//! errors, the `unchanged` warning, an idempotent re-upload, 5,000 rows, commit
//! and conflict.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. Each test uses its own feed and accounts and removes its rows
//! afterwards; the timing test copies chennai_bus's stops into its own feed and
//! never writes to chennai_bus. See scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::services::gtfs_db_source::GtfsDbSource;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

const AUD: &str = "gtfs.editor-details-test.local";
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

fn clear_feed(feed: &str) -> Vec<String> {
    vec![
        format!("DELETE FROM gtfs_station_proposal WHERE gtfs_id = '{feed}'"),
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
    let dir = std::env::temp_dir().join(format!("editor-details-{}", crypto::random_token()));
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

async fn scalar_text(pool: &PgPool, sql: impl AsRef<str>) -> Option<String> {
    let sql = sql.as_ref();
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<Option<String>, _>(0)
}

/// The message codes of row `n` (1-based) of a bulk response.
fn row_codes(out: &Value, n: usize) -> Vec<String> {
    out["rows"][n - 1]["messages"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["code"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- the flow

const FEED: &str = "editor_details_test_feed";
const ADMIN: &str = "admin@editor-details-test.invalid";
const EDITOR: &str = "editor@editor-details-test.invalid";
const APPROVER: &str = "approver@editor-details-test.invalid";

fn seed() -> Vec<String> {
    let mut s = clear_feed(FEED);
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{FEED}', 'Editor stop details test feed')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', 'S' || i, 'S' || i, 'STOP ' || i, 13.0 + i * 0.001, 80.2 FROM generate_series(1, 12) i"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, location_type, description) VALUES \
         ('{FEED}', 'ST0', 'ST0', 'OLD HUB', 13.05, 80.25, 1, NULL)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, parent_station, platform_code, description, deleted) VALUES \
         ('{FEED}', 'PA', 'PA', 'OLD HUB', 13.0501, 80.25, 'ST0', 'Towards STOP 1', NULL, false), \
         ('{FEED}', 'PB', 'PB', 'OLD HUB', 13.0499, 80.25, 'ST0', 'Towards STOP 9', NULL, false), \
         ('{FEED}', 'KEPT', 'KEPT', 'TWIN', 13.07, 80.27, NULL, NULL, 'the kept description', false), \
         ('{FEED}', 'DUPE', 'DUPE', 'TWIN', 13.07, 80.27, NULL, 'Towards nowhere', 'the duplicate''s description', false), \
         ('{FEED}', 'GONE', 'GONE', 'GONE', 13.08, 80.28, NULL, NULL, NULL, true)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id) VALUES \
         ('{FEED}', 'R1', 'T1', 'STOP 1 To STOP 3', 'TESTAG')"
    ));
    let rows = [
        ("R1", 1, "S1", "NEW STOP", 1, "STOP 1"),
        ("R1", 2, "S2", "INTERMEDIATE STOP", 1, "STOP 1"),
        ("R1", 3, "S3", "NEW STOP", 2, "STOP 3"),
    ];
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES {}",
        rows.iter()
            .map(|(r, q, st, t, n, name)| format!("('{FEED}', '{r}', {q}, '{st}', '{t}', {n}, '{name}', '7')"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    s.extend(reset_accounts(&[ADMIN, EDITOR, APPROVER]));
    s
}

/// The public stop JSON of a DB-backed feed, by stop id (without the prefix).
async fn public_stops(pool: &PgPool, feed: &str) -> HashMap<String, Value> {
    let loaded = GtfsDbSource::new(pool.clone(), vec![])
        .load_feed(feed, &HashMap::new())
        .await
        .unwrap();
    loaded
        .stops
        .iter()
        .map(|s| {
            let v = serde_json::to_value(s).unwrap();
            let id = s.id.split_once(':').map(|(_, id)| id).unwrap_or(&s.id);
            (id.to_string(), v)
        })
        .collect()
}

#[actix_web::test]
async fn descriptions_labels_and_bulk_stop_updates() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("details-test-key");
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

    let new_set = |c: &Caller, title: &str| {
        c.req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": title}))
    };
    let add = |c: &Caller, set: &str, change: Value| {
        c.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(change)
    };
    let bulk = |c: &Caller, set: &str, rows: &Vec<Value>, dry_run: bool| {
        c.req("POST", &format!("/change-sets/{set}/bulk"))
            .set_json(json!({"kind": "stop_updates", "rows": rows, "dry_run": dry_run}))
    };
    let stop = |id: &str| editor_c.req("GET", &format!("/feeds/{FEED}/stops/{id}"));
    // submit (editor), approve and commit (approver)
    macro_rules! release {
        ($set:expr) => {{
            let (s, b, _) = call!(
                &app,
                editor_c.req("POST", &format!("/change-sets/{}/submit", $set))
            );
            assert_eq!(s, 200, "{b}");
            let (s, b, _) = call!(
                &app,
                approver
                    .req("POST", &format!("/change-sets/{}/approve", $set))
                    .set_json(json!({}))
            );
            assert_eq!(s, 200, "{b}");
            let (s, b, _) = call!(
                &app,
                approver.req("POST", &format!("/change-sets/{}/commit", $set))
            );
            assert_eq!(s, 200, "{b}");
        }};
    }

    // ======================================================== single changes
    let (s, set, _) = call!(&app, new_set(&editor_c, "descriptions and a solo label"));
    assert_eq!(s, 201, "{set}");
    let c1 = set["change_set_id"].as_str().unwrap().to_string();

    // a platform label and a description on a stop that is in no station
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c1,
            json!({"entity": "stop", "op": "update", "entity_key": "S1", "after": {
                "platform_code": "  Towards STOP 2 ", "description": " Opposite the temple tank "}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let change = b["changes"].as_array().unwrap().last().unwrap().clone();
    // the before snapshot is the read shape, description included
    assert!(change["before"]
        .as_object()
        .unwrap()
        .contains_key("description"));
    assert_eq!(change["before"]["description"], Value::Null);
    assert_eq!(change["before"]["parent_station"], Value::Null);
    assert_eq!(change["base_row_version"], 1);

    // a new stop with both, a new station with a description, an old one given one
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c1,
            json!({"entity": "stop", "op": "create", "after": {
                "stop_id": "NEW1", "name": "NEW KERB", "lat": 13.0095, "lon": 80.2005,
                "platform_code": "Towards STOP 3", "description": "Outside the library"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c1,
            json!({"entity": "station", "op": "create", "after": {
                "station_id": "STN", "name": "NEW HUB", "lat": 13.0045, "lon": 80.2,
                "description": "Stops on both sides of the junction",
                "members": [{"stop_id": "S4", "platform_code": "Towards STOP 5"}, {"stop_id": "S5"}]}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c1,
            json!({"entity": "station", "op": "update", "entity_key": "ST0", "after": {
                "description": "Both sides of the road"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    assert_eq!(b["can_submit"], true, "{}", b["validation"]);

    // the length rules, and a station never has a platform label of its own
    for (change, code) in [
        (
            json!({"entity": "stop", "op": "update", "entity_key": "S2", "after": {"description": "x".repeat(501)}}),
            "description_too_long",
        ),
        (
            json!({"entity": "stop", "op": "update", "entity_key": "S2", "after": {"platform_code": "x".repeat(121)}}),
            "invalid_platform_code",
        ),
        (
            json!({"entity": "stop", "op": "create", "after": {"name": "N", "lat": 13.0, "lon": 80.2, "description": "x".repeat(501)}}),
            "description_too_long",
        ),
        (
            json!({"entity": "station", "op": "update", "entity_key": "ST0", "after": {"description": "x".repeat(501)}}),
            "description_too_long",
        ),
        (
            json!({"entity": "station", "op": "update", "entity_key": "ST0", "after": {"platform_code": "Towards X"}}),
            "invalid_payload",
        ),
    ] {
        let (s, b, _) = call!(&app, add(&editor_c, &c1, change));
        assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
        assert_eq!(b["error"]["details"]["code"], code, "{b}");
    }
    // 500 characters fit
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &c1,
            json!({"entity": "stop", "op": "update", "entity_key": "S2", "after": {"description": "é".repeat(500)}})
        )
    );
    assert_eq!(s, 201, "{b}");

    // nothing is live until the commit
    let (_, live, _) = call!(&app, stop("S1"));
    assert_eq!(live["description"], Value::Null);
    assert_eq!(live["platform_code"], Value::Null);
    release!(c1);

    let (s, s1, _) = call!(&app, stop("S1"));
    assert_eq!(s, 200, "{s1}");
    assert_eq!(s1["description"], "Opposite the temple tank");
    assert_eq!(s1["platform_code"], "Towards STOP 2");
    assert_eq!(s1["parent_station"], Value::Null);
    assert_eq!(s1["row_version"], 2);
    let (_, new1, _) = call!(&app, stop("NEW1"));
    assert_eq!(new1["description"], "Outside the library");
    assert_eq!(new1["platform_code"], "Towards STOP 3");
    let (_, stn, _) = call!(&app, stop("STN"));
    assert_eq!(stn["location_type"], 1);
    assert_eq!(stn["description"], "Stops on both sides of the junction");
    // children, parent and nearby are stop rows too
    assert!(stn["children"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c.as_object().unwrap().contains_key("description")));
    let (_, s4, _) = call!(&app, stop("S4"));
    assert_eq!(
        s4["parent"]["description"],
        "Stops on both sides of the junction"
    );
    assert!(s4["nearby"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c.as_object().unwrap().contains_key("description")));
    let (_, st0, _) = call!(&app, stop("ST0"));
    assert_eq!(st0["description"], "Both sides of the road");
    let (_, listed, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/stops?q=S1"))
    );
    assert_eq!(
        listed["items"][0]["description"], "Opposite the temple tank",
        "{listed}"
    );
    let (_, platforms, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/stops?station=STN"))
    );
    assert_eq!(platforms["items"].as_array().unwrap().len(), 2);
    // a station says how many platforms it has, in a list and on its own page
    assert_eq!(stn["platform_count"], 2);
    assert_eq!(platforms["items"][0]["platform_count"], 0);
    let (_, stations, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/stops?station=true"))
    );
    let counts: Vec<(String, i64)> = stations["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["stop_id"].as_str().unwrap().to_string(),
                s["platform_count"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        counts,
        vec![("ST0".to_string(), 2), ("STN".to_string(), 2)],
        "{stations}"
    );

    // ---- the public stop JSON: `description`, only when there is one
    let public = public_stops(&pool, FEED).await;
    assert_eq!(public["S1"]["description"], "Opposite the temple tank");
    assert_eq!(public["S1"]["platformCode"], "Towards STOP 2");
    assert_eq!(public["S1"]["stationId"], Value::Null);
    assert_eq!(public["S1"]["locationType"], "0");
    assert_eq!(public["S2"]["description"], "é".repeat(500));
    assert!(!public["S3"]
        .as_object()
        .unwrap()
        .contains_key("description"));
    assert!(!public["S3"]
        .as_object()
        .unwrap()
        .contains_key("platformCode"));
    assert_eq!(public["ST0"]["description"], "Both sides of the road");
    assert_eq!(
        public["STN"]["description"],
        "Stops on both sides of the junction"
    );

    // ---- null and blank clear; a field left out stays; a merge keeps the kept
    // stop's description
    let (s, set, _) = call!(&app, new_set(&editor_c, "clear and merge"));
    assert_eq!(s, 201, "{set}");
    let c2 = set["change_set_id"].as_str().unwrap().to_string();
    for change in [
        json!({"entity": "stop", "op": "update", "entity_key": "S1", "after": {"description": null}}),
        json!({"entity": "stop", "op": "update", "entity_key": "S2", "after": {"description": "  ", "name": "STOP TWO"}}),
        json!({"entity": "station", "op": "update", "entity_key": "ST0", "after": {"name": "OLD HUB JN"}}),
        json!({"entity": "station", "op": "update", "entity_key": "STN", "after": {"description": ""}}),
        json!({"entity": "stop", "op": "merge", "entity_key": "DUPE", "after": {"into_stop_id": "KEPT"}}),
    ] {
        let (s, b, _) = call!(&app, add(&editor_c, &c2, change));
        assert_eq!(s, 201, "{b}");
    }
    release!(c2);
    let (_, s1, _) = call!(&app, stop("S1"));
    assert_eq!(s1["description"], Value::Null);
    assert_eq!(s1["platform_code"], "Towards STOP 2", "left out: as it was");
    let (_, s2, _) = call!(&app, stop("S2"));
    assert_eq!(
        (s2["description"].clone(), s2["name"].clone()),
        (Value::Null, json!("STOP TWO"))
    );
    let (_, st0, _) = call!(&app, stop("ST0"));
    assert_eq!(st0["description"], "Both sides of the road");
    assert_eq!(st0["name"], "OLD HUB JN");
    let (_, stn, _) = call!(&app, stop("STN"));
    assert_eq!(stn["description"], Value::Null);
    let (_, kept, _) = call!(&app, stop("KEPT"));
    assert_eq!(kept["description"], "the kept description");
    assert_eq!(kept["platform_code"], Value::Null);
    let public = public_stops(&pool, FEED).await;
    assert!(!public["S1"]
        .as_object()
        .unwrap()
        .contains_key("description"));
    assert_eq!(public["S1"]["platformCode"], "Towards STOP 2");

    // ---- a description change conflicts like any other when the row moved on
    let (_, set, _) = call!(&app, new_set(&editor_c, "stale description"));
    let stale = set["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &stale,
            json!({"entity": "stop", "op": "update", "entity_key": "S3", "after": {"description": "Near the signal"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let (_, set, _) = call!(&app, new_set(&editor_c, "gets there first"));
    let first = set["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &first,
            json!({"entity": "stop", "op": "update", "entity_key": "S3", "after": {"description": "By the flyover"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    release!(first);
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{stale}/submit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    let conflict = &b["error"]["details"]["conflicts"][0];
    assert_eq!(
        (
            conflict["entity_key"].clone(),
            conflict["reason"].clone(),
            conflict["expected"].clone(),
            conflict["actual"].clone()
        ),
        (json!("S3"), json!("changed"), json!(1), json!(2)),
        "{b}"
    );

    // ======================================================== bulk: stop_updates
    let (s, set, _) = call!(&app, new_set(&editor_c, "stop details upload"));
    assert_eq!(s, 201, "{set}");
    let up = set["change_set_id"].as_str().unwrap().to_string();
    let rows = vec![
        json!({"stop_id": "S6", "platform_code": "Towards STOP 7", "description": "Outside the bank"}), // 1 ok
        json!({"stop_id": "S7", "platform_code": " Towards STOP 8 ", "description": "", "name": null}), // 2 ok: blank = not given
        json!({"stop_id": "NOPE", "description": "x"}), // 3 stop_not_found
        json!({"stop_id": "GONE", "description": "x"}), // 4 stop_deleted
        json!({"stop_id": "S8", "description": "one"}), // 5 duplicate
        json!({"stop_id": "S8", "description": "two"}), // 6 duplicate
        json!({"stop_id": "S9"}),                       // 7 nothing_to_update
        json!({"stop_id": "STN", "platform_code": "Towards X"}), // 8 platform_code_on_station
        json!({"stop_id": "ST0", "name": "OLD HUB JUNCTION", "description": "Both sides of the road"}), // 9 ok: a station
        json!({"stop_id": "S10", "description": "x".repeat(501)}), // 10 description_too_long
        json!({"stop_id": "S10", "lat": 13.0}),                    // 11 invalid_row
        json!({"description": "no id"}),                           // 12 invalid_row
        json!({"stop_id": "S1", "platform_code": "Towards STOP 2"}), // 13 unchanged
        json!({"stop_id": "DUPE", "description": "x"}), // 14 stop_deleted (merged, committed)
        json!({"stop_id": "S11", "platform_code": "x".repeat(121)}), // 15 invalid_platform_code
    ];
    let (s, out, _) = call!(&app, bulk(&editor_c, &up, &rows, true));
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["dry_run"], true);
    assert_eq!(out["kind"], "stop_updates");
    assert_eq!(
        out["summary"],
        json!({"rows": 15, "ok": 3, "warnings": 1, "errors": 11, "changes": 3, "unchanged": 1}),
        "{out}"
    );
    for (n, code) in [
        (3, "stop_not_found"),
        (4, "stop_deleted"),
        (5, "duplicate_in_upload"),
        (6, "duplicate_in_upload"),
        (7, "nothing_to_update"),
        (8, "platform_code_on_station"),
        (10, "description_too_long"),
        (11, "invalid_row"),
        (12, "invalid_row"),
        (14, "stop_deleted"),
        (15, "invalid_platform_code"),
    ] {
        assert_eq!(row_codes(&out, n), vec![code.to_string()], "row {n}: {out}");
        assert_eq!(out["rows"][n - 1]["status"], "error");
        assert_eq!(out["rows"][n - 1]["change"], Value::Null, "row {n}");
    }
    // the unchanged row is a warning, and becomes no change
    assert_eq!(row_codes(&out, 13), vec!["unchanged".to_string()]);
    assert_eq!(out["rows"][12]["status"], "warning");
    assert_eq!(out["rows"][12]["messages"][0]["level"], "warning");
    assert_eq!(out["rows"][12]["change"], Value::Null);
    assert_eq!(
        out["rows"][0]["change"],
        json!({"entity": "stop", "op": "update", "entity_key": "S6"})
    );
    assert_eq!(
        out["rows"][8]["change"],
        json!({"entity": "station", "op": "update", "entity_key": "ST0"})
    );
    // each row that names a stop says which, as it is now: the preview's table and map
    assert_eq!(out["rows"][0]["stop"]["name"], "STOP 6");
    assert_eq!(out["rows"][0]["stop"]["lat"], 13.006);
    assert_eq!(out["rows"][12]["stop"]["platform_code"], "Towards STOP 2");
    assert_eq!(out["rows"][2].get("stop"), None);
    // exactly the given fields: the blank cells of row 2 are not in the change
    assert_eq!(
        out["changes_preview"][1]["after"],
        json!({"platform_code": "Towards STOP 8"})
    );
    // ST0 already has that description, but not that name: the row is a change
    assert_eq!(
        out["changes_preview"][2]["after"],
        json!({"description": "Both sides of the road", "name": "OLD HUB JUNCTION"})
    );
    // a dry run changes nothing
    let (_, detail, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{up}")));
    assert_eq!(detail["change_count"], 0);

    // a real run with an error row is refused whole
    let (s, b, _) = call!(&app, bulk(&editor_c, &up, &rows, false));
    assert_eq!((s, code_of(&b)), (400, "bulk_has_errors"), "{b}");
    assert_eq!(b["error"]["details"]["summary"]["errors"], 11);
    let (_, detail, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{up}")));
    assert_eq!(detail["change_count"], 0);

    // the clean rows, the unchanged one among them
    let clean: Vec<Value> = [0, 1, 8, 12].iter().map(|i| rows[*i].clone()).collect();
    let audits = || {
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_audit_log WHERE action = 'bulk_imported' AND change_set_id = '{up}'"
            ),
        )
    };
    let (s, out, _) = call!(&app, bulk(&editor_c, &up, &clean, false));
    assert_eq!(s, 200, "{out}");
    assert_eq!(
        out["summary"],
        json!({"rows": 4, "ok": 3, "warnings": 1, "errors": 0, "changes": 3, "unchanged": 1})
    );
    assert_eq!(out["change_set"]["change_count"], 3);
    assert_eq!(
        out["change_set"]["can_submit"], true,
        "{}",
        out["change_set"]["validation"]
    );
    assert!(out["rows"][0]["change"]["change_id"].is_i64());
    assert_eq!(out["rows"][3]["change"], Value::Null);
    assert_eq!(audits().await, 1);
    let changes = out["change_set"]["changes"].as_array().unwrap().clone();
    // base_row_version is the stop's current one; before is the read shape
    assert_eq!(
        (
            changes[0]["entity"].clone(),
            changes[0]["op"].clone(),
            changes[0]["entity_key"].clone(),
            changes[0]["base_row_version"].clone()
        ),
        (json!("stop"), json!("update"), json!("S6"), json!(1))
    );
    assert_eq!(changes[0]["before"]["name"], "STOP 6");
    assert_eq!(changes[0]["before"]["description"], Value::Null);
    assert_eq!(
        changes[0]["after"],
        json!({"platform_code": "Towards STOP 7", "description": "Outside the bank"})
    );
    // ST0 was updated twice by commits above; a station's before lists its members
    assert_eq!(changes[2]["entity"], "station");
    assert_eq!(changes[2]["base_row_version"], 3);
    assert_eq!(changes[2]["before"]["member_stop_ids"], json!(["PA", "PB"]));
    let audit_detail = scalar_text(
        &pool,
        format!(
            "SELECT detail::text FROM gtfs_audit_log WHERE action = 'bulk_imported' AND change_set_id = '{up}'"
        ),
    )
    .await
    .unwrap();
    let audit_detail: Value = serde_json::from_str(&audit_detail).unwrap();
    assert_eq!(
        (
            audit_detail["kind"].clone(),
            audit_detail["rows"].clone(),
            audit_detail["changes"].clone()
        ),
        (json!("stop_updates"), json!(4), json!(3))
    );

    // the same upload again: every row is unchanged once the draft applies, so
    // nothing is added and nothing is written
    for dry_run in [true, false] {
        let (s, out, _) = call!(&app, bulk(&editor_c, &up, &clean, dry_run));
        assert_eq!(s, 200, "{out}");
        assert_eq!(
            out["summary"],
            json!({"rows": 4, "ok": 0, "warnings": 4, "errors": 0, "changes": 0, "unchanged": 4}),
            "{out}"
        );
        if !dry_run {
            assert_eq!(out["change_set"]["change_count"], 3);
        }
    }
    assert_eq!(audits().await, 1);

    // a different value for a stop the draft already updates: a change, with a
    // warning; a stop the draft creates or merges away
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &up,
            json!({"entity": "stop", "op": "create", "after": {"stop_id": "NEW2", "name": "NEWER KERB", "lat": 13.02, "lon": 80.21}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &up,
            json!({"entity": "stop", "op": "merge", "entity_key": "S12", "after": {"into_stop_id": "S11"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let more = vec![
        json!({"stop_id": "S6", "description": "Outside the new bank"}),
        json!({"stop_id": "NEW2", "platform_code": "Towards STOP 1"}),
        json!({"stop_id": "S12", "description": "x"}),
        json!({"stop_id": "NEW2", "name": "NEWER KERB"}),
    ];
    let (s, out, _) = call!(&app, bulk(&editor_c, &up, &more, true));
    assert_eq!(s, 200, "{out}");
    assert_eq!(
        row_codes(&out, 1),
        vec!["stop_already_in_draft".to_string()]
    );
    assert_eq!(out["rows"][0]["status"], "warning");
    assert!(out["rows"][0]["change"].is_object());
    assert_eq!(row_codes(&out, 3), vec!["stop_merged_away".to_string()]);
    assert_eq!(
        row_codes(&out, 2),
        vec!["duplicate_in_upload".to_string()],
        "{out}"
    );
    let more: Vec<Value> = more[..2].to_vec();
    let (s, out, _) = call!(&app, bulk(&editor_c, &up, &more, false));
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["summary"]["changes"], 2);
    assert_eq!(out["change_set"]["change_count"], 7);
    assert_eq!(audits().await, 2);
    let changes = out["change_set"]["changes"].as_array().unwrap().clone();
    let last = changes.last().unwrap();
    // a stop of the draft has no live row: its before is the create, no base
    assert_eq!(last["entity_key"], "NEW2");
    assert_eq!(last["base_row_version"], Value::Null);
    assert_eq!(last["before"]["name"], "NEWER KERB");

    // commit applies every row
    release!(up);
    for (id, label, description) in [
        ("S6", Some("Towards STOP 7"), Some("Outside the new bank")),
        ("S7", Some("Towards STOP 8"), None),
        ("NEW2", Some("Towards STOP 1"), None),
    ] {
        let (_, row, _) = call!(&app, stop(id));
        assert_eq!(row["platform_code"], json!(label), "{id}");
        assert_eq!(row["description"], json!(description), "{id}");
    }
    let (_, st0, _) = call!(&app, stop("ST0"));
    assert_eq!(st0["name"], "OLD HUB JUNCTION");
    assert_eq!(st0["description"], "Both sides of the road");

    // ---- conflict: a stop's row_version moved after the upload
    let (_, set, _) = call!(&app, new_set(&editor_c, "stale upload"));
    let stale = set["change_set_id"].as_str().unwrap().to_string();
    let rows = vec![
        json!({"stop_id": "S9", "platform_code": "Towards STOP 10"}),
        json!({"stop_id": "S10", "platform_code": "Towards STOP 11"}),
    ];
    let (s, out, _) = call!(&app, bulk(&editor_c, &stale, &rows, false));
    assert_eq!(s, 200, "{out}");
    let (_, set, _) = call!(&app, new_set(&editor_c, "moves S10 on"));
    let mover = set["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &mover,
            json!({"entity": "stop", "op": "update", "entity_key": "S10", "after": {"name": "STOP TEN"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    release!(mover);
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{stale}/submit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    let conflicts = b["error"]["details"]["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{b}");
    assert_eq!(conflicts[0]["entity_key"], "S10");
    assert_eq!(conflicts[0]["reason"], "changed");
    let (_, s9, _) = call!(&app, stop("S9"));
    assert_eq!(
        s9["platform_code"],
        Value::Null,
        "nothing of a stale draft applies"
    );

    // ---- refusals of the request itself
    let (s, b, _) = call!(&app, bulk(&editor_c, &stale, &vec![], true));
    assert_eq!((s, code_of(&b)), (400, "rows_required"), "{b}");
    let too_many: Vec<Value> = (0..5001)
        .map(|i| json!({"stop_id": format!("X{i}"), "description": "d"}))
        .collect();
    let (s, b, _) = call!(&app, bulk(&editor_c, &stale, &too_many, true));
    assert_eq!((s, code_of(&b)), (400, "too_many_rows"), "{b}");
    assert_eq!(b["error"]["details"]["max_rows"], 5000);

    exec(&pool, &clear_feed(FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}

// ---------------------------------------------------------------- timings

const PERF_FEED: &str = "editor_details_perf_feed";
const PERF_ADMIN: &str = "admin@editor-details-perf.invalid";

/// `stop_updates` at the cap, against a copy of chennai_bus's stops (the source
/// feed is only read): the dry run, the real run, a second upload into a draft
/// that already holds 5,000 changes (it must not cost more than the first), the
/// same file again, and the commit. Prints the timings.
#[actix_web::test]
async fn stop_updates_at_5000_rows() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let source = scalar_i64(
        &pool,
        "SELECT count(*) FROM gtfs_stop WHERE gtfs_id = 'chennai_bus' AND NOT deleted AND location_type = 0",
    )
    .await;
    if source < 7000 {
        eprintln!("chennai_bus has {source} stops; skipping the stop_updates timings");
        return;
    }
    let mut setup = clear_feed(PERF_FEED);
    setup.extend([
        format!("INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{PERF_FEED}', 'Stop details timing copy of chennai_bus')"),
        format!(
            "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, location_type, platform_code, cluster_id, deleted) \
             SELECT '{PERF_FEED}', stop_id, stop_code, name, lat, lon, location_type, platform_code, cluster_id, deleted \
             FROM gtfs_stop WHERE gtfs_id = 'chennai_bus' AND parent_station IS NULL"
        ),
    ]);
    setup.extend(reset_accounts(&[PERF_ADMIN]));
    exec(&pool, &setup).await;

    let signer = TestSigner::generate("details-perf-key");
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
    let (s, set, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{PERF_FEED}/change-sets"))
            .set_json(json!({"title": "default platform labels"}))
    );
    assert_eq!(s, 201, "{set}");
    let set_id = set["change_set_id"].as_str().unwrap().to_string();

    let ids: Vec<String> = sqlx::query(&format!(
        "SELECT stop_id FROM gtfs_stop WHERE gtfs_id = '{PERF_FEED}' AND NOT deleted AND location_type = 0 \
         ORDER BY stop_id LIMIT 7000"
    ))
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<String, _>("stop_id"))
    .collect();
    let row = |id: &String| json!({"stop_id": id, "platform_code": format!("Towards {id}"), "description": format!("The stop {id}, by the main road")});
    let first: Vec<Value> = ids[..5000].iter().map(row).collect();
    let second: Vec<Value> = ids[5000..7000].iter().map(row).collect();
    let bulk = |rows: &Vec<Value>, dry_run: bool| {
        admin
            .req("POST", &format!("/change-sets/{set_id}/bulk"))
            .set_json(json!({"kind": "stop_updates", "rows": rows, "dry_run": dry_run}))
    };

    for round in 1..=3 {
        let t = Instant::now();
        let (s, out, _) = call!(&app, bulk(&first, true));
        let took = t.elapsed();
        assert_eq!(s, 200, "{}", out["error"]);
        assert_eq!(out["summary"]["ok"], 5000, "{}", out["summary"]);
        eprintln!(
            "[timing] stop_updates dry run, 5000 rows, empty draft, round {round}: {took:?} ({})",
            out["summary"]
        );
    }
    let t = Instant::now();
    let (s, out, _) = call!(&app, bulk(&first, false));
    assert_eq!(s, 200, "{}", out["error"]);
    eprintln!(
        "[timing] stop_updates apply, 5000 rows, including the set detail it returns: {:?}",
        t.elapsed()
    );
    assert_eq!(out["change_set"]["change_count"], 5000);
    assert_eq!(out["change_set"]["can_submit"], true);
    assert_eq!(out["summary"]["changes"], 5000);

    // a draft of 5,000 changes is read once, not once per row
    let t = Instant::now();
    let (s, out, _) = call!(&app, bulk(&second, true));
    let took = t.elapsed();
    assert_eq!(s, 200, "{}", out["error"]);
    assert_eq!(out["summary"]["ok"], 2000, "{}", out["summary"]);
    eprintln!("[timing] stop_updates dry run, 2000 rows, draft of 5000 changes: {took:?}");
    let t = Instant::now();
    let (s, out, _) = call!(&app, bulk(&first, true));
    let took = t.elapsed();
    assert_eq!(s, 200, "{}", out["error"]);
    assert_eq!(
        (
            out["summary"]["unchanged"].clone(),
            out["summary"]["changes"].clone()
        ),
        (json!(5000), json!(0)),
        "{}",
        out["summary"]
    );
    eprintln!("[timing] stop_updates dry run, the same 5000 rows again (all unchanged), draft of 5000 changes: {took:?}");
    assert!(
        took.as_secs() < 5,
        "validating an upload against a large draft must stay linear: {took:?}"
    );
    let t = Instant::now();
    let (s, out, _) = call!(&app, bulk(&first, false));
    assert_eq!(s, 200, "{}", out["error"]);
    assert_eq!(out["change_set"]["change_count"], 5000, "idempotent");
    eprintln!(
        "[timing] stop_updates apply of the same 5000 rows again (adds nothing), including the set detail: {:?}",
        t.elapsed()
    );
    let t = Instant::now();
    let (s, out, _) = call!(&app, bulk(&second, false));
    assert_eq!(s, 200, "{}", out["error"]);
    assert_eq!(out["change_set"]["change_count"], 7000);
    eprintln!(
        "[timing] stop_updates apply, 2000 rows onto a draft of 5000, including the set detail of 7000: {:?}",
        t.elapsed()
    );

    // released by the admin's own override, and every row applied
    let t = Instant::now();
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set_id}/submit"))
    );
    assert_eq!(s, 200, "{b}");
    eprintln!("[timing] submit of 7000 stop updates: {:?}", t.elapsed());
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/change-sets/{set_id}/approve"))
            .set_json(json!({"self_approve": true}))
    );
    assert_eq!(s, 200, "{b}");
    let t = Instant::now();
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set_id}/commit"))
    );
    assert_eq!(s, 200, "{b}");
    eprintln!("[timing] commit of 7000 stop updates: {:?}", t.elapsed());
    let applied = scalar_i64(
        &pool,
        format!(
            "SELECT count(*) FROM gtfs_stop WHERE gtfs_id = '{PERF_FEED}' \
               AND platform_code = 'Towards ' || stop_id \
               AND description = 'The stop ' || stop_id || ', by the main road' AND row_version = 2"
        ),
    )
    .await;
    assert_eq!(applied, 7000);
    let untouched = scalar_i64(
        &pool,
        format!("SELECT count(*) FROM gtfs_stop WHERE gtfs_id = '{PERF_FEED}' AND description IS NOT NULL"),
    )
    .await;
    assert_eq!(untouched, 7000, "no other stop was written");

    exec(&pool, &clear_feed(PERF_FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}

/// Parity with the preprocessed load is untouched where no stop has a
/// description: chennai_bus (only read here) serialises without the field.
#[actix_web::test]
async fn a_feed_without_descriptions_serves_no_description_field() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let described = scalar_i64(
        &pool,
        "SELECT count(*) FROM gtfs_stop WHERE gtfs_id = 'chennai_bus' AND description IS NOT NULL",
    )
    .await;
    let feeds = scalar_i64(
        &pool,
        "SELECT count(*) FROM gtfs_feed WHERE gtfs_id = 'chennai_bus'",
    )
    .await;
    if feeds == 0 || described > 0 {
        eprintln!("chennai_bus is missing or has descriptions ({described}); skipping");
        return;
    }
    let stops = public_stops(&pool, "chennai_bus").await;
    assert!(!stops.is_empty());
    assert!(stops
        .values()
        .all(|s| !s.as_object().unwrap().contains_key("description")));
}
