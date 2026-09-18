//! Route map lines (docs/gtfs-editor.md section 14), end to end against a real
//! Postgres holding the editor schema (db/gtfs_editor/0001..0015): which routes
//! have no line, the operator's line kept in a draft as a `route/update` - the
//! router's proposal, an encoded line, the points of one - the refusal to cover
//! a line the route already has, the area and length checks, and the `polylines`
//! bulk kind: dry run, errors, the `unchanged` warning, an idempotent
//! re-upload, commit and conflict.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. It uses its own feed and accounts and removes its rows afterwards;
//! it never writes to chennai_bus. See scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto,
    jwt::testing::TestSigner,
    validation::{encode_polyline, POLYLINE_LONG_RATIO},
    EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;

const AUD: &str = "gtfs.editor-polyline-test.local";
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
    let dir = std::env::temp_dir().join(format!("editor-polyline-{}", crypto::random_token()));
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
            // no road router in a test: `from_osrm` is 503, which the flow proves
            osrm_url: None,
            ops_pool: None,
            webhook_policy: Default::default(),
        },
    )
    .unwrap();
    (st, dir)
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

const FEED: &str = "editor_polyline_test_feed";
const ADMIN: &str = "admin@editor-polyline-test.invalid";
const EDITOR: &str = "editor@editor-polyline-test.invalid";
const APPROVER: &str = "approver@editor-polyline-test.invalid";

/// Ten stops a kilometre apart up one meridian, so a line along them has a
/// length anyone can check by hand: R1 and R2 call at all ten, RLINE already
/// has a saved line, REMPTY has no stops at all.
fn seed() -> Vec<String> {
    let mut s = clear_feed(FEED);
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{FEED}', 'Editor route polyline test feed')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', 'S' || i, 'S' || i, 'STOP ' || i, 13.0 + i * 0.009, 80.2 FROM generate_series(1, 10) i"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id, encoded_polyline, polyline_source) VALUES \
         ('{FEED}', 'R1', 'P1', 'STOP 1 To STOP 10', 'TESTAG', NULL, NULL), \
         ('{FEED}', 'R2', 'P2', 'STOP 1 To STOP 10', 'TESTAG', NULL, NULL), \
         ('{FEED}', 'RLINE', 'P3', 'Already drawn', 'TESTAG', '{}', 'imported'), \
         ('{FEED}', 'RGONE', 'P4', 'Deleted route', 'TESTAG', NULL, NULL), \
         ('{FEED}', 'REMPTY', 'P5', 'No stops', 'TESTAG', NULL, NULL)",
        saved_line()
    ));
    s.push(format!(
        "UPDATE gtfs_route SET deleted = true WHERE gtfs_id = '{FEED}' AND route_id = 'RGONE'"
    ));
    for route in ["R1", "R2", "RLINE"] {
        s.push(format!(
            "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) \
             SELECT '{FEED}', '{route}', i, 'S' || i, CASE WHEN i % 3 = 1 THEN 'NEW STOP' ELSE 'INTERMEDIATE STOP' END, \
                    ((i - 1) / 3) + 1, 'STAGE ' || (((i - 1) / 3) + 1), '7' FROM generate_series(1, 10) i"
        ));
    }
    s.extend(reset_accounts(&[ADMIN, EDITOR, APPROVER]));
    s
}

/// The line RLINE starts with: up the same meridian, so it matches its stops.
fn saved_line() -> String {
    encode_polyline(
        &(1..=10)
            .map(|i| (13.0 + i as f64 * 0.009, 80.2))
            .collect::<Vec<_>>(),
    )
}

/// A line along the seeded stops, nudged east so it is a different string.
fn new_line() -> String {
    encode_polyline(
        &(1..=10)
            .map(|i| (13.0 + i as f64 * 0.009, 80.2001))
            .collect::<Vec<_>>(),
    )
}

#[actix_web::test]
async fn route_map_lines_by_hand_and_by_file() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("polyline-test-key");
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
    let set_line = |c: &Caller, set: &Uuid, route: &str, body: Value| {
        c.req(
            "POST",
            &format!("/change-sets/{set}/routes/{route}/polyline"),
        )
        .set_json(body)
    };
    let bulk = |c: &Caller, set: &Uuid, rows: &Vec<Value>, dry_run: bool| {
        c.req("POST", &format!("/change-sets/{set}/bulk"))
            .set_json(json!({"kind": "polylines", "rows": rows, "dry_run": dry_run}))
    };

    // ==================================================== which routes have none
    let (s, b, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/feeds/{FEED}/routes?polyline=missing&limit=50")
        )
    );
    assert_eq!(s, 200, "{b}");
    let missing: Vec<&str> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["route_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        missing,
        vec!["R1", "R2", "REMPTY"],
        "the deleted route is not listed"
    );
    assert!(
        b["items"][0]["has_polyline"] == json!(false)
            && b["items"][0].get("encoded_polyline").is_none(),
        "a list row says whether there is a line, and never carries one: {}",
        b["items"][0]
    );
    let (_, b, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/feeds/{FEED}/routes?polyline=present&limit=50")
        )
    );
    assert_eq!(b["items"].as_array().unwrap().len(), 1);
    assert_eq!(b["items"][0]["route_id"], "RLINE");
    let (s, b, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes?polyline=some"))
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_polyline_filter"), "{b}");

    // ==================================================== a line, by hand
    let (s, set, _) = call!(&app, new_set(&editor_c, "map lines by hand"));
    assert_eq!(s, 201, "{set}");
    let c1: Uuid = set["change_set_id"].as_str().unwrap().parse().unwrap();

    // points the operator has, which the server encodes
    let points: Vec<Value> = (1..=10)
        .map(|i| json!([13.0 + i as f64 * 0.009, 80.2001]))
        .collect();
    let (s, b, _) = call!(
        &app,
        set_line(&editor_c, &c1, "R1", json!({"points": points}))
    );
    assert_eq!(s, 201, "{b}");
    let line = &b["polyline"];
    assert_eq!(line["encoded_polyline"], json!(new_line()));
    assert_eq!(
        (
            line["polyline_source"].clone(),
            line["points"].clone(),
            line["replaced"].clone()
        ),
        (json!("manual"), json!(10), json!(false)),
        "{line}"
    );
    // ten stops 1 km apart: the line and the chain agree, so no warning
    assert!(
        (line["length_m"].as_f64().unwrap() - line["stop_chain_m"].as_f64().unwrap()).abs() < 50.0
            && line["warnings"].as_array().unwrap().is_empty(),
        "{line}"
    );
    // the change it became is the one POST /changes would have stored
    let change = b["changes"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        (
            change["entity"].clone(),
            change["op"].clone(),
            change["entity_key"].clone()
        ),
        (json!("route"), json!("update"), json!("R1"))
    );
    assert_eq!(change["after"]["encoded_polyline"], json!(new_line()));
    assert_eq!(change["base_row_version"], json!(1));
    assert_eq!(
        change["before"]["encoded_polyline"],
        Value::Null,
        "the diff shows there was no line before"
    );
    assert_eq!(b["change_id"], change["change_id"]);

    // the same line again is nothing to do
    let (s, b, _) = call!(
        &app,
        set_line(
            &editor_c,
            &c1,
            "R1",
            json!({"encoded_polyline": new_line(), "replace": true})
        )
    );
    assert_eq!((s, code_of(&b)), (400, "polyline_unchanged"), "{b}");

    // ==================================================== never silently over one
    let (s, b, _) = call!(
        &app,
        set_line(
            &editor_c,
            &c1,
            "RLINE",
            json!({"encoded_polyline": new_line()})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "polyline_exists"), "{b}");
    assert_eq!(
        (
            b["error"]["details"]["polyline_source"].clone(),
            b["error"]["details"]["points"].clone(),
            b["error"]["details"]["in_draft"].clone()
        ),
        (json!("imported"), json!(10), Value::Null),
        "the refusal says what would have been thrown away: {}",
        b["error"]["details"]
    );
    let (s, b, _) = call!(
        &app,
        set_line(
            &editor_c,
            &c1,
            "RLINE",
            json!({"encoded_polyline": new_line(), "polyline_source": "upload", "replace": true})
        )
    );
    assert_eq!(s, 201, "{b}");
    assert_eq!(b["polyline"]["replaced"], json!(true));
    let replaced = b["changes"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        replaced["before"]["encoded_polyline"],
        json!(saved_line()),
        "the draft's diff carries the line that is replaced"
    );
    assert_eq!(replaced["after"]["polyline_source"], json!("upload"));
    // a second line in the same draft is a replacement too, and says which change
    let (s, b, _) = call!(
        &app,
        set_line(
            &editor_c,
            &c1,
            "RLINE",
            json!({"encoded_polyline": saved_line()})
        )
    );
    assert_eq!((s, code_of(&b)), (409, "polyline_exists"), "{b}");
    assert_eq!(b["error"]["details"]["in_draft"], replaced["change_id"]);

    // ==================================================== what a line may be
    let outside = encode_polyline(&[(22.5726, 88.3639), (22.58, 88.37)]);
    let (s, b, _) = call!(
        &app,
        set_line(&editor_c, &c1, "R2", json!({"encoded_polyline": outside}))
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
    assert_eq!(
        b["error"]["details"]["code"],
        json!("polyline_outside_area")
    );
    for (body, code) in [
        (json!({"encoded_polyline": "!!!"}), "invalid_polyline"),
        (
            json!({"encoded_polyline": encode_polyline(&[(13.05, 80.2)])}),
            "invalid_polyline",
        ),
        (json!({"points": [[13.05, 80.2]]}), "invalid_polyline"),
        (json!({"points": [[13.05]]}), "invalid_points"),
        (json!({"points": "13.05,80.2"}), "invalid_points"),
        (
            json!({"encoded_polyline": new_line(), "polyline_source": "guess"}),
            "invalid_payload",
        ),
        (json!({}), "invalid_payload"),
        (
            json!({"encoded_polyline": new_line(), "points": []}),
            "invalid_payload",
        ),
    ] {
        let (s, b, _) = call!(&app, set_line(&editor_c, &c1, "R2", body.clone()));
        assert_eq!(
            (s, b["error"]["details"]["code"].as_str().unwrap_or("")),
            (400, code),
            "{body} -> {b}"
        );
    }
    // a deleted route, and one that is not there
    let (s, b, _) = call!(
        &app,
        set_line(
            &editor_c,
            &c1,
            "RGONE",
            json!({"encoded_polyline": new_line()})
        )
    );
    assert_eq!((s, code_of(&b)), (400, "route_deleted"), "{b}");
    let (s, b, _) = call!(
        &app,
        set_line(
            &editor_c,
            &c1,
            "NOPE",
            json!({"encoded_polyline": new_line()})
        )
    );
    assert_eq!((s, code_of(&b)), (404, "route_not_found"), "{b}");
    // no road router configured: the proposal path says so instead of failing oddly
    let (s, b, _) = call!(
        &app,
        set_line(&editor_c, &c1, "R2", json!({"from_osrm": true}))
    );
    assert_eq!((s, code_of(&b)), (503, "osrm_unavailable"), "{b}");

    // a line far shorter than the stops it claims to follow: a warning, not a block
    let short = encode_polyline(&[(13.009, 80.2), (13.011, 80.2)]);
    let (s, b, _) = call!(
        &app,
        set_line(&editor_c, &c1, "R2", json!({"encoded_polyline": short}))
    );
    assert_eq!(s, 201, "{b}");
    let warnings = b["polyline"]["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{b}");
    assert_eq!(warnings[0]["code"], json!("polyline_length_unlikely"));
    assert_eq!(warnings[0]["level"], json!("warning"));
    assert!(
        warnings[0]["message"].as_str().unwrap().contains("0.0x")
            || warnings[0]["message"].as_str().unwrap().contains("km"),
        "{}",
        warnings[0]["message"]
    );
    assert!(
        b["validation"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["level"] != json!("error")),
        "a long or short line never blocks the draft: {}",
        b["validation"]
    );

    // ==================================================== a file of lines
    let (s, set, _) = call!(&app, new_set(&editor_c, "map lines from a file"));
    assert_eq!(s, 201, "{set}");
    let c2: Uuid = set["change_set_id"].as_str().unwrap().parse().unwrap();
    let rows = vec![
        json!({"route_id": "R1", "encoded_polyline": new_line()}),
        json!({"route_id": "RLINE", "encoded_polyline": new_line()}),
        json!({"route_id": "R2", "encoded_polyline": saved_line(), "polyline_source": "osrm"}),
        json!({"route_id": "NOPE", "encoded_polyline": new_line()}),
        json!({"route_id": "RGONE", "encoded_polyline": new_line()}),
        json!({"route_id": "R1", "encoded_polyline": saved_line()}),
        json!({"route_id": "REMPTY", "encoded_polyline": "!!!"}),
        json!({"route_id": "R2", "encoded_polyline": new_line(), "notes": "x"}),
        json!({"route_id": "RNONE1", "encoded_polyline": new_line(), "replace": "perhaps"}),
        json!({"route_id": "RNONE2"}),
    ];
    let (s, out, _) = call!(&app, bulk(&editor_c, &c2, &rows, true));
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["kind"], json!("polylines"));
    assert_eq!(row_codes(&out, 1), vec!["duplicate_in_upload"]);
    assert_eq!(row_codes(&out, 2), vec!["polyline_exists"]);
    assert!(row_codes(&out, 3).is_empty(), "{}", out["rows"][2]);
    assert_eq!(row_codes(&out, 4), vec!["route_not_found"]);
    assert_eq!(row_codes(&out, 5), vec!["route_deleted"]);
    assert_eq!(row_codes(&out, 6), vec!["duplicate_in_upload"]);
    assert_eq!(row_codes(&out, 7), vec!["invalid_polyline"]);
    assert_eq!(row_codes(&out, 8), vec!["invalid_row"]);
    // a cell that will not read stops the row before the route is looked up at all
    assert_eq!(row_codes(&out, 9), vec!["invalid_row"]);
    assert_eq!(row_codes(&out, 10), vec!["invalid_row"]);
    assert_eq!(
        out["summary"],
        json!({"rows": 10, "ok": 1, "warnings": 0, "errors": 9, "changes": 1, "unchanged": 0}),
        "{}",
        out["summary"]
    );
    // the preview says what each line is, not what it spells
    let shown = &out["rows"][2]["polyline"];
    assert_eq!(shown["points"], json!(10));
    assert_eq!(shown["had_polyline"], json!(false));
    assert!(shown["length_m"].as_f64().unwrap() > 8000.0, "{shown}");
    // a dry run changes nothing, and a file with errors is refused outright
    let (s, b, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{c2}")));
    assert_eq!((s, b["change_count"].clone()), (200, json!(0)), "{b}");
    let (s, b, _) = call!(&app, bulk(&editor_c, &c2, &rows, false));
    assert_eq!((s, code_of(&b)), (400, "bulk_has_errors"), "{b}");
    assert_eq!(b["error"]["details"]["summary"]["errors"], json!(9));

    // the fixed file, with the replace said out loud
    let fixed = vec![
        json!({"route_id": "R1", "encoded_polyline": new_line()}),
        json!({"route_id": "R2", "encoded_polyline": saved_line(), "polyline_source": "osrm"}),
        json!({"route_id": "RLINE", "encoded_polyline": new_line(), "replace": "yes"}),
    ];
    let (s, out, _) = call!(&app, bulk(&editor_c, &c2, &fixed, true));
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["summary"]["errors"], json!(0));
    assert_eq!(out["summary"]["changes"], json!(3));
    assert_eq!(out["rows"][2]["polyline"]["had_polyline"], json!(true));
    let (s, out, _) = call!(&app, bulk(&editor_c, &c2, &fixed, false));
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["change_set"]["change_count"], json!(3));
    let stored = out["change_set"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["entity_key"] == "RLINE")
        .unwrap()
        .clone();
    assert_eq!(stored["after"]["polyline_source"], json!("upload"));
    assert_eq!(stored["before"]["encoded_polyline"], json!(saved_line()));
    assert_eq!(stored["base_row_version"], json!(1));
    assert!(
        out["rows"][0]["change"]["change_id"].is_number(),
        "the real run names the change each row became: {}",
        out["rows"][0]
    );

    // re-running the same file is idempotent: every row is already true
    let (s, out, _) = call!(&app, bulk(&editor_c, &c2, &fixed, true));
    assert_eq!(s, 200, "{out}");
    assert_eq!(
        out["summary"],
        json!({"rows": 3, "ok": 0, "warnings": 3, "errors": 0, "changes": 0, "unchanged": 3}),
        "{}",
        out["summary"]
    );
    assert_eq!(row_codes(&out, 1), vec!["unchanged"]);
    let (s, out, _) = call!(&app, bulk(&editor_c, &c2, &fixed, false));
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["summary"]["changes"], json!(0));
    assert_eq!(
        out["change_set"]["change_count"],
        json!(3),
        "a run with nothing to add writes nothing"
    );

    // ==================================================== released
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{c2}/submit"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        approver
            .req("POST", &format!("/change-sets/{c2}/approve"))
            .set_json(json!({}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{c2}/commit"))
    );
    assert_eq!(s, 200, "{b}");
    for (route, line, source) in [
        ("R1", new_line(), "upload"),
        ("R2", saved_line(), "osrm"),
        ("RLINE", new_line(), "upload"),
    ] {
        assert_eq!(
            scalar_text(
                &pool,
                format!("SELECT encoded_polyline FROM gtfs_route WHERE gtfs_id = '{FEED}' AND route_id = '{route}'")
            )
            .await,
            Some(line),
            "{route}"
        );
        assert_eq!(
            scalar_text(
                &pool,
                format!("SELECT polyline_source FROM gtfs_route WHERE gtfs_id = '{FEED}' AND route_id = '{route}'")
            )
            .await,
            Some(source.to_string()),
            "{route}"
        );
    }
    let (_, b, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/feeds/{FEED}/routes?polyline=missing&limit=50")
        )
    );
    assert_eq!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["route_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["REMPTY"],
        "only the route with no stops is still without a line"
    );

    // ==================================================== the base is checked
    // the first draft is still based on row_version 1, which the commit moved on
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{c1}/submit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    let conflicts = b["error"]["details"]["conflicts"].as_array().unwrap();
    assert!(
        conflicts
            .iter()
            .any(|c| c["entity"] == "route"
                && (c["entity_key"] == "R1" || c["entity_key"] == "RLINE")),
        "{conflicts:?}"
    );

    // a length far outside the band is only ever a warning, never a stored value
    assert!(POLYLINE_LONG_RATIO > 1.0);

    exec(&pool, &clear_feed(FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}
