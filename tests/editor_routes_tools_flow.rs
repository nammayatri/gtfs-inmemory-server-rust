//! The routes page's tools (docs/gtfs-editor.md section 34), end to end against
//! a real Postgres holding the editor schema:
//!
//! - the routes list: every route with its stop orders, trips, status, tag and
//!   line source, and the list as a draft makes the feed (`?change_set=`);
//! - a route cloned into a draft (`POST /feeds/{g}/routes/{id}:clone`): the
//!   new route has the same stop lists (markers included), stop order names,
//!   timings, trips (new ids, no block), map line, fare tag, status and
//!   routes.txt fields, read as the draft has the route; an id in use - live,
//!   or created by an open draft - is 409, a viewer is refused, and the draft
//!   commits; on a feed built from stages the clone runs the same stages;
//! - routes exported as CSV files (`GET /feeds/{g}/routes:export`) and read
//!   back by the bulk import: as `update` into the same feed, and as `add`
//!   under new ids, the routes made again whole - map line, tag and status
//!   included - and the export as a draft has the routes.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local (see scripts/editor_flow_test.sh). Uses its own feeds and
//! accounts and removes their rows afterwards; never touches chennai_bus.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, bulk::Kind, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Map, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

const AUD: &str = "gtfs.editor-routes-tools-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_routes_tools_feed";
const STAGES_FEED: &str = "editor_routes_tools_stages_feed";
const ADMIN: &str = "admin@editor-routes-tools-test.invalid";
const EDITOR: &str = "editor@editor-routes-tools-test.invalid";
const APPROVER: &str = "approver@editor-routes-tools-test.invalid";
const VIEWER: &str = "viewer@editor-routes-tools-test.invalid";
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

/// A response kept as bytes, with its headers: a zip.
macro_rules! call_raw {
    ($app:expr, $req:expr) => {{
        let resp = test::call_service($app, $req.to_request()).await;
        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        let (kind, disposition, exported) = (
            header("content-type"),
            header("content-disposition"),
            header("x-routes-exported"),
        );
        let body = test::read_body(resp).await.to_vec();
        (status, body, kind, disposition, exported)
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

fn clear_feeds() -> Vec<String> {
    let accounts = format!("'{ADMIN}', '{EDITOR}', '{APPROVER}', '{VIEWER}'");
    let mut out = Vec::new();
    for feed in [FEED, STAGES_FEED] {
        out.extend([
            format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{feed}'"),
            format!("DELETE FROM gtfs_trip WHERE gtfs_id = '{feed}'"),
            format!("DELETE FROM gtfs_service WHERE gtfs_id = '{feed}'"),
            format!("DELETE FROM gtfs_route_stage WHERE gtfs_id = '{feed}'"),
            format!("DELETE FROM gtfs_stage_stop WHERE gtfs_id = '{feed}'"),
            format!("DELETE FROM gtfs_stage WHERE gtfs_id = '{feed}'"),
            format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{feed}'"),
            format!("DELETE FROM gtfs_route WHERE gtfs_id = '{feed}'"),
            format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{feed}'"),
            format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{feed}'"),
            format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{feed}'"),
        ]);
    }
    out.extend([
        format!(
            "DELETE FROM gtfs_editor_session WHERE user_id IN \
             (SELECT user_id FROM gtfs_editor_user WHERE email IN ({accounts}))"
        ),
        format!(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, \
             totp_last_step = NULL, status = 'active' WHERE email IN ({accounts})"
        ),
    ]);
    out
}

/// A Google encoded polyline (precision 5).
fn encode_polyline(points: &[(f64, f64)]) -> String {
    let mut out = String::new();
    let (mut plat, mut plon) = (0i64, 0i64);
    for (lat, lon) in points {
        let (la, lo) = ((lat * 1e5).round() as i64, (lon * 1e5).round() as i64);
        for d in [la - plat, lo - plon] {
            let mut v = if d < 0 { !(d << 1) } else { d << 1 };
            while v >= 0x20 {
                out.push(char::from((((v & 0x1f) | 0x20) + 63) as u8));
                v >>= 5;
            }
            out.push(char::from((v + 63) as u8));
        }
        plat = la;
        plon = lo;
    }
    out
}

/// A feed with no fare stages: stops about a kilometre apart on a line. R1
/// calls at S1..S4 with a map shaping point between S2 and S3, and carries a
/// colour, a text colour, a description, a fare tag and a map line; R2 calls
/// at S5, S6 and is inactive.
fn seed(line: &str) -> Vec<String> {
    let mut s = clear_feeds();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, agency_name) \
         VALUES ('{FEED}', 'Editor routes tools test feed', 'TOOLSAG')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
         ('{FEED}', 'S1', 'S1', 'ONE',   13.000, 80.2), ('{FEED}', 'S2', 'S2', 'TWO',   13.010, 80.2), \
         ('{FEED}', 'S3', 'S3', 'THREE', 13.020, 80.2), ('{FEED}', 'S4', 'S4', 'FOUR',  13.030, 80.2), \
         ('{FEED}', 'S5', 'S5', 'FIVE',  13.040, 80.2), ('{FEED}', 'S6', 'S6', 'SIX',   13.050, 80.2)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type, agency_id, \
                                 color, text_color, route_desc, encoded_polyline, polyline_source, \
                                 route_tag, active) VALUES \
         ('{FEED}', 'R1', 'BLUE', 'ONE To FOUR', 3, 'AG', '#1166AA', '#FFFFFF', 'the blue line', \
          '{line}', 'manual', 'METRO_FEEDER', true), \
         ('{FEED}', 'R2', 'GREEN', 'FIVE To SIX', 3, 'AG', NULL, NULL, NULL, NULL, NULL, NULL, false)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, \
                                      marker_id, marker_lat, marker_lon) VALUES \
         ('{FEED}', 'R1', 1, 'S1', 'NEW STOP', NULL, NULL, NULL), \
         ('{FEED}', 'R1', 2, 'S2', 'NEW STOP', NULL, NULL, NULL), \
         ('{FEED}', 'R1', 3, NULL, 'ROUTE CORRECTION', 'M1', 13.015, 80.201), \
         ('{FEED}', 'R1', 4, 'S3', 'NEW STOP', NULL, NULL, NULL), \
         ('{FEED}', 'R1', 5, 'S4', 'NEW STOP', NULL, NULL, NULL), \
         ('{FEED}', 'R2', 1, 'S5', 'NEW STOP', NULL, NULL, NULL), \
         ('{FEED}', 'R2', 2, 'S6', 'NEW STOP', NULL, NULL, NULL)"
    ));
    // a feed built from stages: SR1 runs through U1 and U2 (up) and V, which
    // runs either way
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, use_stages) \
         VALUES ('{STAGES_FEED}', 'Editor routes tools stages feed', true)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{STAGES_FEED}', c, c, 'STOP ' || c, 13.0 + ascii(c) * 0.001, 80.2 \
         FROM unnest(ARRAY['A','B','C','D','E','F']) c"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, direction, name) VALUES \
         ('{STAGES_FEED}', 'U1', 'up', 'A STAGE'), ('{STAGES_FEED}', 'U2', 'up', 'C STAGE'), \
         ('{STAGES_FEED}', 'V', '', 'E STAGE')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, direction, position, stop_id, stop_type) VALUES \
         ('{STAGES_FEED}', 'U1', 'up', 1, 'A', 'NEW STOP'), ('{STAGES_FEED}', 'U1', 'up', 2, 'B', 'INTERMEDIATE STOP'), \
         ('{STAGES_FEED}', 'U2', 'up', 1, 'C', 'NEW STOP'), ('{STAGES_FEED}', 'U2', 'up', 2, 'D', 'INTERMEDIATE STOP'), \
         ('{STAGES_FEED}', 'V', '', 1, 'E', 'NEW STOP'), ('{STAGES_FEED}', 'V', '', 2, 'F', 'INTERMEDIATE STOP')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id) VALUES \
         ('{STAGES_FEED}', 'SR1', '21', 'A To E', 'AG')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stage (gtfs_id, route_id, position, stage_id, direction, stage_no) VALUES \
         ('{STAGES_FEED}', 'SR1', 1, 'U1', 'up', 1), ('{STAGES_FEED}', 'SR1', 2, 'U2', 'up', 2), \
         ('{STAGES_FEED}', 'SR1', 3, 'V', '', 3)"
    ));
    s
}

fn state(pool: &PgPool, signer: &TestSigner, admin: &str) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-routes-tools-{}", crypto::random_token()));
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

fn errors(set: &Value) -> Vec<Value> {
    set["validation"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| x["level"] == "error")
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// A zip's files by name.
fn unzip(bytes: &[u8]) -> BTreeMap<String, String> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("a zip");
    let mut out = BTreeMap::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        let name = f.name().to_string();
        let mut text = String::new();
        f.read_to_string(&mut text).unwrap();
        out.insert(name, text);
    }
    out
}

/// A CSV file's header and rows, as text.
fn read_csv(text: &str) -> (Vec<String>, Vec<Vec<String>>) {
    assert!(text.starts_with('\u{feff}'), "a byte order mark first");
    let mut r = csv::Reader::from_reader(text.trim_start_matches('\u{feff}').as_bytes());
    let header = r.headers().unwrap().iter().map(str::to_string).collect();
    let rows = r
        .records()
        .map(|rec| rec.unwrap().iter().map(str::to_string).collect())
        .collect();
    (header, rows)
}

/// The rows of a CSV file as the import page sends them: an empty cell left
/// out, except a stage name (a feed with no fare stages names none).
fn upload_rows(text: &str) -> Vec<Value> {
    let (header, rows) = read_csv(text);
    rows.into_iter()
        .map(|rec| {
            let mut m = Map::new();
            for (h, v) in header.iter().zip(rec) {
                if !v.trim().is_empty() || h == "stage_name" {
                    m.insert(h.clone(), json!(v));
                }
            }
            Value::Object(m)
        })
        .collect()
}

/// A column of a CSV file, by name.
fn column(text: &str, name: &str) -> Vec<String> {
    let (header, rows) = read_csv(text);
    let i = header.iter().position(|h| h == name).unwrap();
    rows.into_iter().map(|r| r[i].clone()).collect()
}

/// A route's rows as compared: what a stop list says about each.
fn row_keys(detail: &Value) -> Vec<Value> {
    detail["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            json!([
                r["stop_id"],
                r["stop_type"],
                r["stage_no"],
                r["stage_name"],
                r["marker_id"],
                r["marker_lat"],
                r["marker_lon"]
            ])
        })
        .collect()
}

/// A route's stop orders and timings as compared.
fn timetable_keys(detail: &Value) -> (Vec<Value>, Vec<Value>) {
    let patterns = detail["patterns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            json!([
                p["pattern_key"],
                p["name"],
                p["direction_id"],
                p["stop_count"],
                p["rows_hash"],
                p["trip_count"]
            ])
        })
        .collect();
    let profiles = detail["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            json!([
                p["pattern_key"],
                p["profile_key"],
                p["label"],
                p["arrival_s"],
                p["departure_s"],
                p["hash"]
            ])
        })
        .collect();
    (patterns, profiles)
}

/// Trips as compared: everything but what makes each its own.
fn trip_keys(trips: &Value, drop: &[&str]) -> Vec<Value> {
    trips["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let mut t = t.clone();
            let m = t.as_object_mut().unwrap();
            for k in ["trip_id", "sort_key", "ref_s", "source"]
                .iter()
                .chain(drop)
            {
                m.remove(*k);
            }
            t
        })
        .collect()
}

#[actix_web::test]
async fn routes_are_listed_cloned_exported_and_imported_back() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let line = encode_polyline(&[(13.0, 80.2), (13.01, 80.2), (13.02, 80.2), (13.03, 80.2)]);
    exec(&pool, &seed(&line)).await;
    let signer = TestSigner::generate("routes-tools-test-key");
    let (st, dir) = state(&pool, &signer, ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    // ---- accounts, each with a grant on both feeds
    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    sign_in(&app, &mut admin).await;
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
        for feed in [FEED, STAGES_FEED] {
            let (s, b, _) = call!(
                &app,
                admin
                    .req("PUT", &format!("/users/{id}/feeds/{feed}"))
                    .set_json(json!({"role": role}))
            );
            assert_eq!(s, 200, "{b}");
        }
    }
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
    let mut viewer = Caller {
        signer: &signer,
        email: VIEWER.into(),
        session: None,
    };
    sign_in(&app, &mut ed).await;
    sign_in(&app, &mut ap).await;
    sign_in(&app, &mut viewer).await;

    macro_rules! open_set {
        ($feed:expr, $title:expr) => {{
            let (s, b, _) = call!(
                &app,
                ed.req("POST", &format!("/feeds/{}/change-sets", $feed))
                    .set_json(json!({"title": $title}))
            );
            assert_eq!(s, 201, "{b}");
            b["change_set_id"].as_str().unwrap().to_string()
        }};
    }
    macro_rules! added {
        ($set:expr, $change:expr) => {{
            let (s, b, _) = call!(
                &app,
                ed.req("POST", &format!("/change-sets/{}/changes", $set))
                    .set_json($change)
            );
            assert_eq!(s, 201, "{b}");
            b
        }};
    }
    macro_rules! commit {
        ($set:expr) => {{
            let (s, b, _) = call!(
                &app,
                ed.req("POST", &format!("/change-sets/{}/submit", $set))
            );
            assert_eq!(s, 200, "submit: {b}");
            let (s, b, _) = call!(
                &app,
                ap.req("POST", &format!("/change-sets/{}/approve", $set))
            );
            assert_eq!(s, 200, "approve: {b}");
            let (s, b, _) = call!(
                &app,
                ap.req("POST", &format!("/change-sets/{}/commit", $set))
            );
            assert_eq!(s, 200, "commit: {b}");
        }};
    }
    macro_rules! get {
        ($c:expr, $path:expr) => {{
            let (s, b, _) = call!(&app, $c.req("GET", &$path));
            assert_eq!(s, 200, "GET {}: {b}", $path);
            b
        }};
    }
    macro_rules! clone {
        ($c:expr, $set:expr, $from:expr, $body:expr) => {{
            call!(
                &app,
                $c.req(
                    "POST",
                    &format!("/feeds/{FEED}/routes/{}:clone?change_set={}", $from, $set)
                )
                .set_json($body)
            )
        }};
    }
    macro_rules! bulk {
        ($set:expr, $kind:expr, $rows:expr, $dry:expr) => {{
            let (s, b, _) = call!(
                &app,
                ed.req("POST", &format!("/change-sets/{}/bulk", $set))
                    .set_json(json!({"kind": $kind, "rows": $rows, "dry_run": $dry}))
            );
            assert_eq!(s, 200, "bulk {} (dry run {}): {b}", $kind, $dry);
            assert_eq!(b["summary"]["errors"], 0, "bulk {}: {b}", $kind);
            b
        }};
    }

    // ---- R1's timetable, through a committed draft: a second stop order with
    // a name, a service, a timing for each stop order, and three trips
    let setup = open_set!(FEED, "R1's timetable");
    added!(
        setup,
        json!({"entity": "route_stops", "op": "replace", "entity_key": "R1", "after": {
            "pattern_key": 2, "base_rows_hash": NOTHING, "rows": [
                {"stop_id": "S2", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": ""},
                {"stop_id": "S3", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": ""},
                {"stop_id": "S4", "stop_type": "NEW STOP", "stage_no": 0, "stage_name": ""}]}})
    );
    added!(
        setup,
        json!({"entity": "pattern", "op": "update", "entity_key": "R1",
               "after": {"pattern_key": 2, "name": "short turn", "direction_id": 0}})
    );
    added!(
        setup,
        json!({"entity": "service", "op": "create", "entity_key": "WK", "after": {
            "service_id": "WK", "label": "weekdays",
            "days": {"monday": true, "tuesday": true, "wednesday": true, "thursday": true, "friday": true},
            "start_date": "2026-01-01", "end_date": "2026-12-31",
            "dates": [{"date": "2026-08-15", "exception_type": 2}]}})
    );
    added!(
        setup,
        json!({"entity": "timing_profile", "op": "replace", "entity_key": "R1", "after": {
            "pattern_key": 1, "profile_key": 1, "label": "peak", "base_hash": NOTHING,
            "arrival_s": [0, 300, 600, 900], "departure_s": [30, 330, 630, 900]}})
    );
    added!(
        setup,
        json!({"entity": "timing_profile", "op": "replace", "entity_key": "R1", "after": {
            "pattern_key": 2, "profile_key": 1, "base_hash": NOTHING,
            "arrival_s": [0, 300, 600], "departure_s": [30, 330, 600]}})
    );
    added!(
        setup,
        json!({"entity": "route_trips", "op": "replace", "entity_key": "R1", "after": {
            "base_trips_hash": NOTHING, "trips": [
                {"pattern_key": 1, "profile_key": 1, "service_id": "WK", "start_time": "06:00",
                 "headsign": "FOUR", "direction_id": 0, "block_id": "B1"},
                {"pattern_key": 1, "service_id": "WK", "start_time": "07:00:00"},
                {"pattern_key": 2, "profile_key": 1, "service_id": "WK", "start_time": "08:00",
                 "frequencies": [{"start_time": "08:00:00", "end_time": "10:00:00", "headway_s": 600}]}]}})
    );
    commit!(setup);

    // ======================================================== the routes list
    let list = get!(viewer, format!("/feeds/{FEED}/routes?limit=500"));
    let items = list["items"].as_array().unwrap();
    let r1 = items.iter().find(|r| r["route_id"] == "R1").unwrap();
    let r2 = items.iter().find(|r| r["route_id"] == "R2").unwrap();
    assert_eq!(
        (&r1["stop_count"], &r1["pattern_count"], &r1["trip_count"]),
        (&json!(4), &json!(2), &json!(3)),
        "{r1}"
    );
    assert_eq!(
        (
            &r1["route_tag"],
            &r1["polyline_source"],
            &r1["has_polyline"],
            &r1["active"],
            &r1["agency_id"]
        ),
        (
            &json!("METRO_FEEDER"),
            &json!("manual"),
            &json!(true),
            &json!(true),
            &json!("AG")
        )
    );
    assert!(
        r1.get("encoded_polyline").is_none(),
        "the list leaves the line out"
    );
    assert_eq!(
        (&r2["active"], &r2["pattern_count"], &r2["trip_count"]),
        (&json!(false), &json!(1), &json!(0))
    );

    // ======================================================== clone
    let d = open_set!(FEED, "clones");
    // a viewer may not, and a clone goes into a draft
    let (s, b, _) = clone!(viewer, d, "R1", json!({"route_id": "R1C"}));
    assert_eq!((s, code_of(&b)), (403, "role_required"), "{b}");
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/feeds/{FEED}/routes/R1:clone"))
            .set_json(json!({"route_id": "R1C"}))
    );
    assert_eq!((s, code_of(&b)), (400, "change_set_required"), "{b}");
    // the new id: unused, and an id
    for (to, status, code) in [
        ("R2", 409, "route_exists"),
        ("R1", 409, "route_exists"),
        ("bad id", 400, "invalid_change"),
        ("a:b", 400, "invalid_change"),
    ] {
        let (s, b, _) = clone!(ed, d, "R1", json!({"route_id": to}));
        assert_eq!((s, code_of(&b)), (status, code), "{to}: {b}");
    }
    let (s, b, _) = clone!(ed, d, "NOPE", json!({"route_id": "R9"}));
    assert_eq!((s, code_of(&b)), (404, "route_not_found"), "{b}");
    let (s, b, _) = clone!(ed, d, "R1", json!({"route_id": "R1C", "colour": "#000000"}));
    assert_eq!(s, 400, "an unknown field is refused: {b}");

    let (s, made, _) = clone!(
        ed,
        d,
        "R1",
        json!({"route_id": "R1C", "short_name": "BLUE-X"})
    );
    assert_eq!(s, 201, "{made}");
    assert_eq!(
        (&made["route_id"], &made["from_route_id"]),
        (&json!("R1C"), &json!("R1"))
    );
    assert_eq!(
        made["copied"],
        json!({"patterns": 2, "stop_lists": 2, "stages": 0, "profiles": 2, "trips": 3, "map_line": true}),
        "{made}"
    );
    assert_eq!(
        made["left_out"],
        json!({"patterns": [], "profiles": 0, "trips": 0})
    );
    let ids: Vec<i64> = made["change_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    let set = &made["change_set"];
    assert!(
        errors(set).is_empty(),
        "the clone applies cleanly: {}",
        set["validation"]
    );
    let kinds: Vec<String> = ids
        .iter()
        .map(|id| {
            let c = set["changes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["change_id"] == *id)
                .unwrap();
            assert_eq!(c["entity_key"], "R1C");
            format!(
                "{}/{}",
                c["entity"].as_str().unwrap(),
                c["op"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "route/create",
            "route/update",
            "route_stops/replace",
            "route_stops/replace",
            "pattern/update",
            "timing_profile/replace",
            "timing_profile/replace",
            "route_trips/replace"
        ],
        "one change of each, in an order that applies"
    );
    let changes = set["changes"].as_array().unwrap();
    let change = |id: i64| changes.iter().find(|c| c["change_id"] == id).unwrap();
    // every list is based on nothing: the route is new
    assert_eq!(change(ids[2])["after"]["base_rows_hash"], NOTHING);
    assert_eq!(change(ids[7])["after"]["base_trips_hash"], NOTHING);
    assert_eq!(change(ids[1])["after"]["schedule_source"], "editor");

    // the new route, as the draft has it, against R1
    let live = get!(ed, format!("/feeds/{FEED}/routes/R1"));
    let copy = get!(ed, format!("/change-sets/{d}/preview/routes/R1C"));
    assert_eq!(
        (&copy["short_name"], &copy["long_name"]),
        (&json!("BLUE-X"), &json!("ONE To FOUR"))
    );
    for k in [
        "route_type",
        "agency_id",
        "color",
        "text_color",
        "encoded_polyline",
        "polyline_source",
        "route_tag",
        "active",
        "gtfs",
    ] {
        assert_eq!(copy[k], live[k], "{k}");
    }
    assert_eq!(copy["gtfs"]["route_desc"], "the blue line");
    assert_eq!(
        row_keys(&copy),
        row_keys(&live),
        "the stop list, its shaping point too"
    );
    assert_eq!(copy["rows_hash"], live["rows_hash"]);
    assert_eq!(
        timetable_keys(&copy),
        timetable_keys(&live),
        "stop orders and timings"
    );
    let live_trips = get!(ed, format!("/feeds/{FEED}/routes/R1/trips"));
    let copy_trips = get!(ed, format!("/change-sets/{d}/preview/routes/R1C/trips"));
    assert_eq!(
        trip_keys(&copy_trips, &[]),
        trip_keys(&live_trips, &["block_id"])
            .into_iter()
            .map(|mut t| {
                t["block_id"] = Value::Null;
                t
            })
            .collect::<Vec<_>>(),
        "the trips, each its own: no block shared with R1"
    );
    for t in copy_trips["items"].as_array().unwrap() {
        assert!(t["trip_id"].as_str().unwrap().starts_with("R1C-ed-"), "{t}");
    }

    // ids now in use: by this draft, and so for any other open draft
    let (s, b, _) = clone!(ed, d, "R1", json!({"route_id": "R1C"}));
    assert_eq!((s, code_of(&b)), (409, "route_exists"), "{b}");
    assert!(
        b["error"]["message"].as_str().unwrap().contains("clones"),
        "{b}"
    );
    let other = open_set!(FEED, "another draft");
    let (s, b, _) = clone!(ed, other, "R1", json!({"route_id": "R1C"}));
    assert_eq!((s, code_of(&b)), (409, "route_exists"), "{b}");

    // the source as the draft has it: a name the draft gives R1, and a route
    // only the draft has
    added!(
        d,
        json!({"entity": "route", "op": "update", "entity_key": "R1", "after": {"long_name": "DRAFTED"}})
    );
    let (s, b, _) = clone!(
        ed,
        d,
        "R1",
        json!({"route_id": "R1D", "short_name": "BLUE-D"})
    );
    assert_eq!(s, 201, "{b}");
    let r1d = get!(ed, format!("/change-sets/{d}/preview/routes/R1D"));
    assert_eq!(r1d["long_name"], "DRAFTED");
    let (s, b, _) = clone!(ed, d, "R1C", json!({"route_id": "R1CC", "long_name": null}));
    assert_eq!(s, 201, "{b}");
    let r1cc = get!(ed, format!("/change-sets/{d}/preview/routes/R1CC"));
    assert_eq!(
        (&r1cc["short_name"], &r1cc["long_name"]),
        (&json!("BLUE-X"), &Value::Null)
    );
    assert_eq!(row_keys(&r1cc), row_keys(&live));
    // an inactive route stays inactive
    let (s, b, _) = clone!(ed, d, "R2", json!({"route_id": "R2C"}));
    assert_eq!(s, 201, "{b}");
    assert_eq!(
        b["copied"],
        json!({"patterns": 1, "stop_lists": 1, "stages": 0, "profiles": 0, "trips": 0, "map_line": false})
    );
    let r2c = get!(ed, format!("/change-sets/{d}/preview/routes/R2C"));
    assert_eq!(
        (&r2c["active"], &r2c["short_name"]),
        (&json!(false), &json!("GREEN"))
    );

    // the list as the draft makes the feed
    let drafted = get!(
        viewer,
        format!("/feeds/{FEED}/routes?limit=500&change_set={d}")
    );
    let listed: Vec<&str> = drafted["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["route_id"].as_str().unwrap())
        .collect();
    for id in ["R1", "R1C", "R1CC", "R1D", "R2", "R2C"] {
        assert!(listed.contains(&id), "{id} in {listed:?}");
    }
    let r1c = drafted["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["route_id"] == "R1C")
        .unwrap();
    assert_eq!(
        (
            &r1c["pattern_count"],
            &r1c["trip_count"],
            &r1c["stop_count"]
        ),
        (&json!(2), &json!(3), &json!(4))
    );
    let plain = get!(viewer, format!("/feeds/{FEED}/routes?limit=500"));
    assert!(!plain["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["route_id"] == "R1C"));
    // a draft of another feed is not this feed's
    let stages_draft = open_set!(STAGES_FEED, "stage clones");
    let (s, b, _) = call!(
        &app,
        viewer.req(
            "GET",
            &format!("/feeds/{FEED}/routes?change_set={stages_draft}")
        )
    );
    assert_eq!((s, code_of(&b)), (400, "feed_mismatch"), "{b}");

    commit!(d);
    let n = |sql: &str| scalar_i64(&pool, sql.replace("{F}", FEED));
    assert_eq!(
        n("SELECT count(*) FROM gtfs_trip WHERE gtfs_id = '{F}' AND route_id = 'R1C'").await,
        3
    );
    assert_eq!(
        n("SELECT count(*) FROM gtfs_trip WHERE gtfs_id = '{F}' AND route_id = 'R1C' AND block_id IS NOT NULL").await,
        0
    );
    assert_eq!(
        n("SELECT count(*) FROM gtfs_timing_profile WHERE gtfs_id = '{F}' AND route_id = 'R1C'")
            .await,
        2
    );
    assert_eq!(
        n("SELECT count(*) FROM gtfs_pattern WHERE gtfs_id = '{F}' AND route_id = 'R1C' AND name = 'short turn'").await,
        1
    );
    assert_eq!(
        scalar_text(
            &pool,
            format!(
                "SELECT schedule_source || ' ' || route_tag || ' ' || polyline_source FROM gtfs_route \
                 WHERE gtfs_id = '{FEED}' AND route_id = 'R1C'"
            )
        )
        .await
        .as_deref(),
        Some("editor METRO_FEEDER manual")
    );
    // one per clone that went in (audit rows stay, so this run's draft's)
    assert_eq!(
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_audit_log WHERE change_set_id = '{d}' \
                 AND action = 'route_cloned'"
            )
        )
        .await,
        4
    );
    // now live: the other draft is told so
    let (s, b, _) = clone!(ed, other, "R1", json!({"route_id": "R1C"}));
    assert_eq!((s, code_of(&b)), (409, "route_exists"), "{b}");

    // ======================================================== a feed built from stages
    let (s, made, _) = call!(
        &app,
        ed.req(
            "POST",
            &format!("/feeds/{STAGES_FEED}/routes/SR1:clone?change_set={stages_draft}")
        )
        .set_json(json!({"route_id": "SR1C", "short_name": "21X"}))
    );
    assert_eq!(s, 201, "{made}");
    assert_eq!(made["copied"]["stages"], 3);
    assert_eq!(made["copied"]["stop_lists"], 0, "its stops are its stages'");
    assert!(
        errors(&made["change_set"]).is_empty(),
        "{}",
        made["change_set"]["validation"]
    );
    let entities: Vec<&str> = made["change_set"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["entity"].as_str().unwrap())
        .collect();
    assert_eq!(entities, ["route", "route_stages"]);
    let live = get!(ed, format!("/feeds/{STAGES_FEED}/routes/SR1"));
    let copy = get!(
        ed,
        format!("/change-sets/{stages_draft}/preview/routes/SR1C")
    );
    assert!(!row_keys(&live).is_empty());
    assert_eq!(
        row_keys(&copy),
        row_keys(&live),
        "the same stops, from the same stages"
    );
    let stage_keys = |v: &Value| -> Vec<Value> {
        v["stages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| json!([s["stage_id"], s["direction"], s["stage_no"]]))
            .collect()
    };
    let live_stages = get!(ed, format!("/feeds/{STAGES_FEED}/routes/SR1/stages"));
    let copy_stages = get!(
        ed,
        format!("/change-sets/{stages_draft}/preview/routes/SR1C/stages")
    );
    assert_eq!(stage_keys(&copy_stages), stage_keys(&live_stages));
    assert_eq!(stage_keys(&live_stages).len(), 3);

    // its export says which stages, and reads back
    let (s, bytes, ..) = call_raw!(
        &app,
        viewer.req(
            "GET",
            &format!("/feeds/{STAGES_FEED}/routes:export?route_ids=SR1&action=update")
        )
    );
    assert_eq!(s, 200);
    let files = unzip(&bytes);
    assert!(
        files.contains_key("route_stages.csv") && !files.contains_key("route_stops.csv"),
        "{:?}",
        files.keys()
    );
    assert_eq!(
        column(&files["route_stages.csv"], "stage_id"),
        ["U1", "U2", "V"]
    );
    assert_eq!(
        column(&files["route_stages.csv"], "direction"),
        ["up", "up", ""]
    );
    let back = open_set!(STAGES_FEED, "stages back");
    bulk!(
        back,
        "route_stages",
        upload_rows(&files["route_stages.csv"]),
        true
    );

    // ======================================================== export
    // a viewer may; every row says the action asked for
    let (s, bytes, kind, disposition, exported) = call_raw!(
        &app,
        viewer.req(
            "GET",
            &format!("/feeds/{FEED}/routes:export?route_ids=R1&action=update")
        )
    );
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&bytes));
    assert_eq!(kind, "application/zip");
    assert!(
        disposition.contains(&format!("{FEED}-route-R1.zip")),
        "{disposition}"
    );
    assert_eq!(exported, "1");
    let files = unzip(&bytes);
    assert_eq!(
        files.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "README.txt",
            "route_stops.csv",
            "route_trips.csv",
            "routes.csv",
            "services.csv",
            "timing_profiles.csv"
        ]
    );
    // each file is what its kind of upload reads, column for column
    for (file, kind) in [
        ("routes.csv", Kind::Routes),
        ("route_stops.csv", Kind::RouteStops),
        ("timing_profiles.csv", Kind::TimingProfiles),
        ("services.csv", Kind::Services),
        ("route_trips.csv", Kind::RouteTrips),
    ] {
        assert_eq!(read_csv(&files[file]).0, kind.columns(), "{file}");
        assert!(
            column(&files[file], "action").iter().all(|a| a == "update"),
            "{file}"
        );
    }
    assert_eq!(
        column(&files["routes.csv"], "encoded_polyline"),
        [line.as_str()]
    );
    assert_eq!(column(&files["routes.csv"], "route_tag"), ["METRO_FEEDER"]);
    assert_eq!(
        column(&files["routes.csv"], "route_desc"),
        ["the blue line"]
    );
    // the shaping point is not a stop: left out, and said so
    assert_eq!(
        column(&files["route_stops.csv"], "stop_id"),
        ["S1", "S2", "S3", "S4", "S2", "S3", "S4"]
    );
    assert_eq!(
        column(&files["route_stops.csv"], "pattern_key"),
        ["", "", "", "", "2", "2", "2"]
    );
    assert!(
        files["README.txt"].contains("map shaping point"),
        "{}",
        files["README.txt"]
    );
    assert_eq!(
        column(&files["timing_profiles.csv"], "arrival_offset").len(),
        7
    );
    assert_eq!(
        column(&files["route_trips.csv"], "start_time"),
        ["06:00:00", "07:00:00", "08:00:00"]
    );
    assert_eq!(column(&files["services.csv"], "date"), ["2026-08-15"]);

    // ---- update: back into the same feed, every file checked and added
    let u = open_set!(FEED, "R1 back from its files");
    let mut said = Vec::new();
    for (file, kind) in [
        ("routes.csv", "routes"),
        ("route_stops.csv", "route_stops"),
        ("timing_profiles.csv", "timing_profiles"),
        ("services.csv", "services"),
        ("route_trips.csv", "route_trips"),
    ] {
        let rows = upload_rows(&files[file]);
        let dry = bulk!(u, kind, rows.clone(), true);
        said.extend(
            dry["rows"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|r| r["messages"].as_array().unwrap().clone())
                .map(|m| m["code"].as_str().unwrap().to_string()),
        );
        bulk!(u, kind, rows, false);
    }
    assert!(said.contains(&"markers_dropped".to_string()), "{said:?}");
    let back = get!(ed, format!("/change-sets/{u}"));
    assert!(errors(&back).is_empty(), "{}", back["validation"]);
    let r1_back = get!(ed, format!("/change-sets/{u}/preview/routes/R1"));
    let r1_live = get!(ed, format!("/feeds/{FEED}/routes/R1"));
    for k in [
        "short_name",
        "encoded_polyline",
        "polyline_source",
        "route_tag",
        "text_color",
        "gtfs",
    ] {
        assert_eq!(r1_back[k], r1_live[k], "{k}");
    }
    assert_eq!(r1_back["stop_count"], 4);

    // ---- add: the same files under new ids make the routes again, whole
    let (s, bytes, ..) = call_raw!(
        &app,
        viewer.req(
            "GET",
            &format!("/feeds/{FEED}/routes:export?route_ids=R1,R2")
        )
    );
    assert_eq!(s, 200);
    let files = unzip(&bytes);
    assert!(column(&files["routes.csv"], "action")
        .iter()
        .all(|a| a == "add"));
    assert_eq!(column(&files["routes.csv"], "active"), ["1", "0"]);
    let renamed = |file: &str| -> Vec<Value> {
        upload_rows(&files[file])
            .into_iter()
            .map(|mut r| {
                let id = r["route_id"].as_str().unwrap().to_string();
                r["route_id"] = json!(format!("{id}X"));
                // new trip ids: R1's own are taken
                r.as_object_mut().unwrap().remove("trip_id");
                r
            })
            .collect()
    };
    let x = open_set!(FEED, "R1X and R2X from files");
    // the services are there already: services.csv is skipped
    for (file, kind) in [
        ("routes.csv", "routes"),
        ("route_stops.csv", "route_stops"),
        ("timing_profiles.csv", "timing_profiles"),
        ("route_trips.csv", "route_trips"),
    ] {
        bulk!(x, kind, renamed(file), true);
        bulk!(x, kind, renamed(file), false);
    }
    let made = get!(ed, format!("/change-sets/{x}"));
    assert!(errors(&made).is_empty(), "{}", made["validation"]);
    let r1x = get!(ed, format!("/change-sets/{x}/preview/routes/R1X"));
    let r1 = get!(ed, format!("/feeds/{FEED}/routes/R1"));
    for k in [
        "short_name",
        "long_name",
        "color",
        "text_color",
        "encoded_polyline",
        "polyline_source",
        "route_tag",
        "active",
        "gtfs",
    ] {
        assert_eq!(r1x[k], r1[k], "{k}");
    }
    let stops = |v: &Value| -> Vec<Value> {
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["stop_type"] != "ROUTE CORRECTION")
            .map(|r| json!([r["stop_id"], r["stop_type"]]))
            .collect()
    };
    assert_eq!(stops(&r1x), stops(&r1));
    let profiles = |v: &Value| -> Vec<Value> {
        v["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                json!([
                    p["pattern_key"],
                    p["profile_key"],
                    p["label"],
                    p["arrival_s"],
                    p["departure_s"]
                ])
            })
            .collect()
    };
    assert_eq!(profiles(&r1x), profiles(&r1));
    let r1x_trips = get!(ed, format!("/change-sets/{x}/preview/routes/R1X/trips"));
    let r1_trips = get!(ed, format!("/feeds/{FEED}/routes/R1/trips"));
    assert_eq!(
        trip_keys(&r1x_trips, &[]),
        trip_keys(&r1_trips, &[]),
        "the trips, block included"
    );
    let r2x = get!(ed, format!("/change-sets/{x}/preview/routes/R2X"));
    assert_eq!(
        (&r2x["active"], &r2x["short_name"]),
        (&json!(false), &json!("GREEN"))
    );

    // ---- as a draft has the routes, and every route when none is named
    let v = open_set!(FEED, "a name for R2");
    added!(
        v,
        json!({"entity": "route", "op": "update", "entity_key": "R2", "after": {"long_name": "DRAFT NAME"}})
    );
    let (s, b, _) = clone!(ed, v, "R2", json!({"route_id": "R2V"}));
    assert_eq!(s, 201, "{b}");
    let (s, body, ..) = call_raw!(
        &app,
        viewer.req("GET", &format!("/feeds/{FEED}/routes:export?route_ids=R2V"))
    );
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    assert_eq!(
        (s, code_of(&b)),
        (404, "route_not_found"),
        "only the draft has R2V"
    );
    let (s, bytes, ..) = call_raw!(
        &app,
        viewer
            .req(
                "POST",
                &format!("/feeds/{FEED}/routes:export?change_set={v}")
            )
            .set_json(json!({"route_ids": ["R2", "R2V"], "action": "update"}))
    );
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&bytes));
    let files = unzip(&bytes);
    assert_eq!(
        column(&files["routes.csv"], "long_name"),
        ["DRAFT NAME", "DRAFT NAME"]
    );
    assert!(
        files["README.txt"].contains("a name for R2"),
        "{}",
        files["README.txt"]
    );
    let (s, _, _, _, exported) = call_raw!(
        &app,
        viewer.req("GET", &format!("/feeds/{FEED}/routes:export"))
    );
    let live_routes =
        n("SELECT count(*) FROM gtfs_route WHERE gtfs_id = '{F}' AND NOT deleted").await;
    assert_eq!((s, exported), (200, live_routes.to_string()));
    for (query, status, code) in [
        ("route_ids=R1,NOPE", 404, "route_not_found"),
        ("action=delete", 400, "invalid_query"),
    ] {
        let (s, body, ..) = call_raw!(
            &app,
            viewer.req("GET", &format!("/feeds/{FEED}/routes:export?{query}"))
        );
        let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        assert_eq!((s, code_of(&b)), (status, code), "{query}: {b}");
    }

    exec(&pool, &clear_feeds()).await;
    std::fs::remove_dir_all(dir).ok();
}
