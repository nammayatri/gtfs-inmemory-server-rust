//! A route's map line from GPS, and the reasons a road route through its stops
//! fails (docs/gtfs-editor.md section 17), end to end: a real Postgres holding
//! the editor schema (db/gtfs_editor/*.sql, 0021 included), a fake ClickHouse
//! and a fake OSRM, both small HTTP servers on localhost started by the test.
//!
//! What it proves:
//!
//!   - the endpoint answers 503 `gps_unavailable` when GPS is not configured,
//!     or not for this feed;
//!   - it finds the runs of THIS route: the same route number's other
//!     direction and a variant that leaves the corridor get their own answers,
//!     and a route no bus passes in order is 422 `gps_not_enough_runs`, with
//!     the counts;
//!   - with `change_set`, it asks about the route as the draft has it;
//!   - OSRM snaps the line; where OSRM fails a stretch the answer is
//!     `partial`, and with OSRM down it is the GPS path (`none`) - and a
//!     second click then tries OSRM again without asking ClickHouse again,
//!     while a fully snapped answer is served from the cache;
//!   - a slow ClickHouse is 504 `gps_timeout`;
//!   - every statement ClickHouse received was a bounded SELECT sent with
//!     readonly=2, and the password never reached a response;
//!   - the line goes into a draft as `polyline_source: "gps"` and commits
//!     (the CHECK of 0021);
//!   - the road route through the stops is asked for in chunks of at most 25
//!     waypoints, and when it fails it says why and where.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local. Uses its own feeds and accounts; never touches chennai_bus.
//! See scripts/editor_flow_test.sh.

use actix_web::{test, web, App, HttpRequest, HttpResponse, HttpServer};
use gtfs_routes_service::editor::{
    self, crypto, gps_line, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::services::clickhouse_reader::ClickHouseSettings;
use gtfs_routes_service::services::osrm::{self, Planar};
use regex::Regex;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const AUD: &str = "gtfs.editor-gps-test.local";
const BASE: &str = "/internal/gtfs-editor";
const CH_USER: &str = "gims_gps_reader";
const CH_PASSWORD: &str = "s3cret-clickhouse-pw-in-the-test";

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
        let raw = String::from_utf8_lossy(&body).to_string();
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, json, cookie, raw)
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

fn clear_feed(feed: &str, emails: &[&str]) -> Vec<String> {
    let list = emails
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{feed}'"),
        format!(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, totp_last_step = NULL, \
             status = 'active' WHERE email IN ({list})"
        ),
        format!(
            "DELETE FROM gtfs_editor_session WHERE user_id IN (SELECT user_id FROM gtfs_editor_user WHERE email IN ({list}))"
        ),
    ]
}

fn settings(
    signer: &TestSigner,
    admin: &str,
    osrm_url: Option<String>,
) -> (EditorSettings, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-gps-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    use base64::Engine;
    (
        EditorSettings {
            jwks_url: format!("file://{}", jwks.display()),
            audience: AUD.into(),
            bootstrap_admins: vec![admin.to_string()],
            totp_key_b64: base64::engine::general_purpose::STANDARD
                .encode(crypto::random_bytes(32)),
            session_hours: 1,
            ui_dir: dir.join("no-ui"),
            osrm_url,
            webhook_policy: Default::default(),
        },
        dir,
    )
}

/// Sign `admin` in (bootstrap), create the others with their roles, sign them in.
async fn sign_in_all<'a, S>(
    app: &S,
    signer: &'a TestSigner,
    admin: &str,
    others: &[(&str, &str)],
) -> Vec<Caller<'a>>
where
    S: actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
{
    async fn enrol<S>(app: &S, c: &mut Caller<'_>)
    where
        S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
    {
        let (s, b, _, _) = call!(app, c.req("POST", "/auth/totp/enroll"));
        assert_eq!(s, 200, "{b}");
        let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
        let now = chrono::Utc::now().timestamp() as u64;
        let (s, b, cookie, _) = call!(
            app,
            c.req("POST", "/auth/totp/confirm")
                .set_json(json!({"code": crypto::totp_now(&secret, now)}))
        );
        assert_eq!(s, 200, "{b}");
        c.session = cookie;
    }
    let mut admin_c = Caller {
        signer,
        email: admin.into(),
        session: None,
    };
    enrol(app, &mut admin_c).await;
    for (email, role) in others {
        let (s, b, _, _) = call!(
            app,
            admin_c
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    }
    let (_, users, _, _) = call!(app, admin_c.req("GET", "/users"));
    let mut out = vec![];
    for (email, role) in others {
        let id = users["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["email"] == *email)
            .unwrap()["user_id"]
            .as_str()
            .unwrap()
            .to_string();
        let (s, b, _, _) = call!(
            app,
            admin_c
                .req("PATCH", &format!("/users/{id}"))
                .set_json(json!({"role": role, "status": "active"}))
        );
        assert_eq!(s, 200, "{b}");
        let mut c = Caller {
            signer,
            email: email.to_string(),
            session: None,
        };
        enrol(app, &mut c).await;
        out.push(c);
    }
    out.insert(0, admin_c);
    out
}

// ---------------------------------------------------------------- the corridor

const ORIGIN: (f64, f64) = (13.04, 80.23);

fn planar() -> Planar {
    Planar::new(ORIGIN.0, ORIGIN.1)
}

/// An 8 km road in planar metres: east, north, east.
fn corridor_xy() -> Vec<(f64, f64)> {
    vec![
        (0.0, 0.0),
        (3_000.0, 0.0),
        (3_000.0, 2_000.0),
        (6_000.0, 2_000.0),
    ]
}

/// The first 40% of the corridor, then a road of its own 1 km south.
fn variant_xy() -> Vec<(f64, f64)> {
    vec![
        (0.0, 0.0),
        (3_000.0, 0.0),
        (3_000.0, -1_000.0),
        (6_000.0, -1_000.0),
    ]
}

/// A branch no bus drives: the corridor's first 3 km, then south-west.
fn nobody_xy() -> Vec<(f64, f64)> {
    vec![
        (0.0, 0.0),
        (3_000.0, 0.0),
        (3_000.0, -3_000.0),
        (0.0, -3_000.0),
    ]
}

fn along(path: &[(f64, f64)], step: f64) -> Vec<(f64, f64)> {
    let mut out = vec![path[0]];
    let mut carry = 0.0;
    for w in path.windows(2) {
        let (a, b) = (w[0], w[1]);
        let seg = osrm::dist(a, b);
        let mut t = step - carry;
        while t <= seg {
            let f = t / seg;
            out.push((a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f));
            t += step;
        }
        carry = seg - (t - step);
    }
    if osrm::dist(*out.last().unwrap(), *path.last().unwrap()) > 1.0 {
        out.push(*path.last().unwrap());
    }
    out
}

fn ll(xy: (f64, f64)) -> (f64, f64) {
    planar().ll(xy)
}

fn reversed(xy: &[(f64, f64)]) -> Vec<(f64, f64)> {
    xy.iter().rev().copied().collect()
}

// ---------------------------------------------------------------- fake ClickHouse

#[derive(Clone, Debug)]
struct ChRequest {
    query: String,
    auth: Option<String>,
    body: String,
}

/// Deterministic jitter in metres from a key.
fn jitter(key: i64, salt: i64) -> f64 {
    let mut x = (key as u64).wrapping_mul(0x9E3779B97F4A7C15)
        ^ (salt as u64).wrapping_mul(0xD1B54A32D192ED03);
    x ^= x >> 29;
    x = x.wrapping_mul(0xBF58476D1CE4E5B9);
    x ^= x >> 32;
    (x % 1_000) as f64 / 1_000.0 * 8.0 - 4.0
}

/// What one bus did on the day starting `day0`: (unix second, lat, lon), a
/// point every 20 s, as ClickHouse averages them.
fn bus_pings(device: &str, day0: i64) -> Vec<(i64, f64, f64)> {
    let (legs, start): (Vec<Vec<(f64, f64)>>, i64) = match device {
        "dev-1" => (
            vec![
                corridor_xy(),
                reversed(&corridor_xy()),
                corridor_xy(),
                reversed(&corridor_xy()),
            ],
            6 * 3600,
        ),
        "dev-2" => (
            vec![corridor_xy(), reversed(&corridor_xy()), corridor_xy()],
            6 * 3600 + 1_500,
        ),
        "dev-v" => (
            vec![variant_xy(), reversed(&variant_xy()), variant_xy()],
            7 * 3600,
        ),
        _ => return vec![],
    };
    let mut out = vec![];
    let mut t = day0 + start;
    let stand = |out: &mut Vec<(i64, f64, f64)>, at: (f64, f64), t: &mut i64, secs: i64| {
        for _ in 0..secs / 20 {
            let (la, lo) = ll((at.0 + jitter(*t, 1), at.1 + jitter(*t, 2)));
            out.push((*t, la, lo));
            *t += 20;
        }
    };
    stand(&mut out, legs[0][0], &mut t, 300);
    for leg in &legs {
        // 8 m/s: a point every 160 m
        for q in along(leg, 160.0) {
            let (la, lo) = ll((q.0 + jitter(t, 3), q.1 + jitter(t, 4)));
            out.push((t, la, lo));
            t += 20;
        }
        stand(&mut out, *leg.last().unwrap(), &mut t, 720);
    }
    out
}

fn ist_day(t: i64) -> (i64, String) {
    let offset = 19_800;
    let d = (t + offset).div_euclid(86_400);
    let start = d * 86_400 - offset;
    let date = chrono::DateTime::from_timestamp(d * 86_400, 0)
        .unwrap()
        .date_naive();
    (start, date.to_string())
}

async fn fake_clickhouse(
    req: HttpRequest,
    body: String,
    log: web::Data<Arc<Mutex<Vec<ChRequest>>>>,
) -> HttpResponse {
    log.lock().unwrap().push(ChRequest {
        query: req.query_string().to_string(),
        auth: req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body: body.clone(),
    });
    let label = if body.contains("'t21g'") {
        "t21g"
    } else if body.contains("'slow'") {
        actix_web::rt::time::sleep(Duration::from_secs(6)).await;
        "slow"
    } else {
        ""
    };
    let offset: usize = Regex::new(r"OFFSET (\d+)")
        .unwrap()
        .captures(&body)
        .map(|c| c[1].parse().unwrap())
        .unwrap_or(0);
    let times: Vec<i64> = Regex::new(r"toDateTime\((\d+)\)")
        .unwrap()
        .captures_iter(&body)
        .map(|c| c[1].parse().unwrap())
        .collect();
    if label.is_empty() || offset > 0 || times.len() < 2 {
        return HttpResponse::Ok().body("");
    }
    let (from, to) = (times[0], times[1]);
    let mut out = String::new();
    if body.contains("arrayStringConcat") {
        let list = Regex::new(r"IN \(([^)]*)\)")
            .unwrap()
            .captures(&body)
            .unwrap()[1]
            .to_string();
        let devices: Vec<String> = list
            .split(',')
            .map(|d| d.trim().trim_matches('\'').to_string())
            .collect();
        let (day0, _) = ist_day(from);
        for device in devices {
            let mut hours: std::collections::BTreeMap<i64, Vec<String>> = Default::default();
            for (t, la, lo) in bus_pings(&device, day0) {
                if t >= from && t < to {
                    hours
                        .entry(t / 3600)
                        .or_default()
                        .push(format!("{t},{la:.6},{lo:.6}"));
                }
            }
            for (h, pts) in hours {
                out.push_str(&format!(
                    "{device}\t{h}\t{}\t{}\t{}\n",
                    pts.len(),
                    pts.len() * 2,
                    pts.join("|")
                ));
            }
        }
    } else if body.contains(" BY day") {
        let mut t = from;
        while t < to {
            let (start, day) = ist_day(t);
            for (device, n) in [("dev-1", 700), ("dev-2", 650), ("dev-v", 600)] {
                // the first and last ping of the day, as ClickHouse would see them
                let pings = bus_pings(device, start);
                let (first, last) = (pings[0].0, pings[pings.len() - 1].0);
                out.push_str(&format!("{device}\t{day}\t{n}\t{first}\t{last}\n"));
            }
            t = start + 86_400;
        }
    }
    HttpResponse::Ok().body(out)
}

fn start_clickhouse() -> (String, Arc<Mutex<Vec<ChRequest>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let l = log.clone();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(l.clone()))
            .default_service(web::to(fake_clickhouse))
    })
    .workers(2)
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    tokio::spawn(server.run());
    (format!("http://127.0.0.1:{}", addr.port()), log)
}

// ---------------------------------------------------------------- fake OSRM

/// 0: fine. 1: `/match` is down. 2: every second `/match` cannot match.
struct OsrmState {
    mode: AtomicU8,
    matches: AtomicUsize,
    paths: Mutex<Vec<String>>,
}

fn parse_coords(path: &str) -> Vec<(f64, f64)> {
    path.rsplit('/')
        .next()
        .unwrap()
        .split(';')
        .map(|c| {
            let mut it = c.split(',');
            let lon: f64 = it.next().unwrap().parse().unwrap();
            let lat: f64 = it.next().unwrap().parse().unwrap();
            (lat, lon)
        })
        .collect()
}

async fn fake_osrm(req: HttpRequest, st: web::Data<Arc<OsrmState>>) -> HttpResponse {
    let path = req.path().to_string();
    st.paths
        .lock()
        .unwrap()
        .push(format!("{path}?{}", req.query_string()));
    let coords = parse_coords(&path);
    if path.starts_with("/match/") {
        let n = st.matches.fetch_add(1, Ordering::SeqCst);
        match st.mode.load(Ordering::SeqCst) {
            1 => return HttpResponse::ServiceUnavailable().body("down"),
            2 if n % 2 == 1 => {
                return HttpResponse::Ok()
                    .json(json!({"code": "NoMatch", "message": "Could not match the trace."}))
            }
            _ => {}
        }
        // a road 2 m north of every point
        let snapped: Vec<(f64, f64)> = coords.iter().map(|&(la, lo)| (la + 0.000018, lo)).collect();
        return HttpResponse::Ok().json(json!({
            "code": "Ok",
            "matchings": [{"geometry": osrm::encode_polyline(&snapped), "confidence": 0.9}],
            "tracepoints": snapped.iter().enumerate().map(|(i, (la, lo))| json!({
                "matchings_index": 0, "waypoint_index": i, "location": [lo, la]
            })).collect::<Vec<_>>(),
        }));
    }
    if let Some(i) = coords.iter().position(|c| c.0 > 13.4) {
        return HttpResponse::BadRequest().json(json!({
            "code": "NoSegment", "message": format!("Could not find a matching segment for coordinate {i}")
        }));
    }
    if coords.len() > 1 && coords.iter().any(|c| (13.29..13.31).contains(&c.0)) {
        return HttpResponse::BadRequest()
            .json(json!({"code": "NoRoute", "message": "Impossible route between points"}));
    }
    let legs: Vec<Value> = coords
        .windows(2)
        .map(|w| {
            let d = gtfs_routes_service::editor::validation::haversine_m(
                w[0].0, w[0].1, w[1].0, w[1].1,
            );
            json!({"distance": d, "duration": d / 8.0})
        })
        .collect();
    HttpResponse::Ok().json(json!({
        "code": "Ok",
        "routes": [{"geometry": osrm::encode_polyline(&coords), "legs": legs}],
    }))
}

fn start_osrm() -> (String, Arc<OsrmState>) {
    let st = Arc::new(OsrmState {
        mode: AtomicU8::new(0),
        matches: AtomicUsize::new(0),
        paths: Mutex::new(vec![]),
    });
    let s = st.clone();
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(s.clone()))
            .default_service(web::to(fake_osrm))
    })
    .workers(2)
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    tokio::spawn(server.run());
    (format!("http://127.0.0.1:{}", addr.port()), st)
}

// ---------------------------------------------------------------- seed

/// A route's rows along a planar path: a stage stop every `step` metres.
fn route_rows(
    feed: &str,
    route: &str,
    short: Option<&str>,
    stops: &[(String, (f64, f64))],
) -> Vec<String> {
    let short = short
        .map(|s| format!("'{s}'"))
        .unwrap_or_else(|| "NULL".into());
    let mut s = vec![format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id) VALUES \
         ('{feed}', '{route}', {short}, '{route} test route', 'TESTAG')"
    )];
    let mut values = vec![];
    for (i, (id, (lat, lon))) in stops.iter().enumerate() {
        s.push(format!(
            "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
             ('{feed}', '{id}', '{id}', 'STOP {id}', {lat:.7}, {lon:.7}) ON CONFLICT DO NOTHING"
        ));
        values.push(format!(
            "('{feed}', '{route}', {}, '{id}', 'NEW STOP', {}, 'STOP {id}', '7')",
            i + 1,
            i + 1
        ));
    }
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES {}",
        values.join(", ")
    ));
    s
}

fn stops_on(path: &[(f64, f64)], step: f64, prefix: &str) -> Vec<(String, (f64, f64))> {
    along(path, step)
        .into_iter()
        .enumerate()
        .map(|(i, q)| (format!("{prefix}{i:02}"), ll(q)))
        .collect()
}

// ================================================================ the GPS flow

const GPS_FEED: &str = "editor_gps_test_feed";
const GPS_ADMIN: &str = "admin@editor-gps-test.invalid";
const GPS_EDITOR: &str = "editor@editor-gps-test.invalid";
const GPS_APPROVER: &str = "approver@editor-gps-test.invalid";

#[actix_web::test]
async fn a_map_line_from_gps_end_to_end() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let accounts = [GPS_ADMIN, GPS_EDITOR, GPS_APPROVER];
    let mut seed = clear_feed(GPS_FEED, &accounts);
    seed.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{GPS_FEED}', 'GPS line test feed')"
    ));
    let forward = stops_on(&corridor_xy(), 400.0, "F");
    let back = stops_on(&reversed(&corridor_xy()), 400.0, "B");
    let variant = stops_on(&variant_xy(), 400.0, "V");
    let nobody = stops_on(&nobody_xy(), 400.0, "N");
    seed.extend(route_rows(GPS_FEED, "R_FWD", Some("T21G"), &forward));
    seed.extend(route_rows(GPS_FEED, "R_BACK", Some(" t21g "), &back));
    seed.extend(route_rows(GPS_FEED, "R_VAR", Some("T21G"), &variant));
    seed.extend(route_rows(GPS_FEED, "R_NOBODY", Some("T21G"), &nobody));
    seed.extend(route_rows(GPS_FEED, "R_OLD", Some("OLDNUM"), &forward));
    seed.extend(route_rows(GPS_FEED, "R_NONUM", None, &forward));
    seed.extend(route_rows(GPS_FEED, "R_SLOW", Some("SLOW"), &forward));
    exec(&pool, &seed).await;

    let (ch_url, ch_log) = start_clickhouse();
    let (osrm_url, osrm_state) = start_osrm();
    let signer = TestSigner::generate("gps-test-key");

    // ---- without GPS configured: 503, before anything else
    let (plain, dir0) = settings(&signer, GPS_ADMIN, Some(osrm_url.clone()));
    let plain_app = test::init_service(App::new().configure(|cfg| {
        editor::configure(
            cfg,
            Some(Arc::new(EditorState::build(pool.clone(), plain).unwrap())),
        )
    }))
    .await;
    let callers = sign_in_all(
        &plain_app,
        &signer,
        GPS_ADMIN,
        &[(GPS_EDITOR, "editor"), (GPS_APPROVER, "approver")],
    )
    .await;
    let (s, b, _, _) = call!(
        &plain_app,
        callers[1].req(
            "POST",
            &format!("/feeds/{GPS_FEED}/routes/R_FWD/polyline:gps")
        )
    );
    assert_eq!(s, 503, "{b}");
    assert_eq!(code_of(&b), "gps_unavailable");

    // ---- with GPS: a fake ClickHouse holding three days of three buses
    let (with_gps, dir) = settings(&signer, GPS_ADMIN, Some(osrm_url.clone()));
    let gps = gps_line::GpsLine::new(gps_line::GpsLineSettings {
        clickhouse: ClickHouseSettings {
            url: ch_url.clone(),
            user: CH_USER.into(),
            password: Some(CH_PASSWORD.into()),
            query_timeout: Duration::from_secs(10),
            min_gap: Duration::from_millis(20),
        },
        table: gps_line::DEFAULT_TABLE.into(),
        days: 3,
        feeds: vec![GPS_FEED.into()],
        max_bus_days: 9,
        page_rows: 100,
        timeout: Duration::from_secs(4),
        params: gps_line::Params::default(),
    })
    .unwrap();
    let st = EditorState::build(pool.clone(), with_gps)
        .unwrap()
        .with_gps_line(gps);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;
    // the sessions are rows in the database: the same callers sign in here
    let (editor_c, approver) = (&callers[1], &callers[2]);
    let gps_of = |route: &str| {
        editor_c.req(
            "POST",
            &format!("/feeds/{GPS_FEED}/routes/{route}/polyline:gps"),
        )
    };
    let ch_count = || ch_log.lock().unwrap().len();

    // a feed the pings are not for
    let (s, b, _, _) = call!(
        &app,
        editor_c.req("POST", "/feeds/some_other_feed/routes/R_FWD/polyline:gps")
    );
    assert_eq!(s, 503, "{b}");
    assert_eq!(code_of(&b), "gps_unavailable");

    // ---- the route, while OSRM fails every second stretch: partial
    osrm_state.mode.store(2, Ordering::SeqCst);
    let (s, b, _, raw) = call!(&app, gps_of("R_FWD"));
    assert_eq!(s, 200, "{b}");
    assert!(
        !raw.contains(CH_PASSWORD),
        "the password never reaches a response"
    );
    let ev = &b["evidence"];
    assert_eq!(b["polyline_source"], "gps");
    assert_eq!(b["saved"], false);
    assert_eq!(ev["matched"], "partial", "{ev}");
    assert!(
        ev["osrm_error"].as_str().unwrap().contains("NoMatch"),
        "{ev}"
    );
    assert_eq!(ev["route_number"], "T21G");
    // dev-1 and dev-2 drive it forward 2 + 2 times a day (the window's two
    // whole days are 8, today's are more if the morning is over); dev-v's
    // variant leaves after 3 km and the reverse legs meet the stops backwards
    let used = ev["runs_used"].as_u64().unwrap();
    assert!(used >= 8, "{ev}");
    assert!(ev["runs_seen"].as_u64().unwrap() > used, "{ev}");
    assert_eq!(ev["buses"], 2, "{ev}");
    assert!(ev["stop_coverage"].as_f64().unwrap() >= 0.95, "{ev}");
    assert!(
        ev["pings"].as_u64().unwrap() > 0 && ev["bus_days"].as_u64().unwrap() > 0,
        "{ev}"
    );
    let line = osrm::decode_polyline(b["encoded_polyline"].as_str().unwrap()).unwrap();
    let first = forward[0].1;
    assert!(
        gtfs_routes_service::editor::validation::haversine_m(
            line[0].0, line[0].1, first.0, first.1
        ) < 80.0,
        "the line starts at the first stop"
    );
    let queries = ch_count();
    assert!(queries >= 2, "a bus-day query and at least one track query");

    // OSRM is fine again: the line is snapped all through, and ClickHouse is
    // not asked again - the GPS half was kept
    osrm_state.mode.store(0, Ordering::SeqCst);
    let (s, b, _, _) = call!(&app, gps_of("R_FWD"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["evidence"]["matched"], "osrm", "{b}");
    assert_eq!(b["evidence"]["cached"], false);
    assert_eq!(ch_count(), queries, "the GPS half came from the cache");
    let fwd_line = b["encoded_polyline"].as_str().unwrap().to_string();
    // and now the whole answer is kept
    let (s, b, _, _) = call!(&app, gps_of("R_FWD"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["evidence"]["cached"], true);
    assert_eq!(b["encoded_polyline"], fwd_line.as_str());
    assert_eq!(ch_count(), queries);

    // ---- the other direction of the same route number, with OSRM down
    osrm_state.mode.store(1, Ordering::SeqCst);
    let (s, b, _, _) = call!(&app, gps_of("R_BACK"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["evidence"]["matched"], "none", "{b}");
    assert!(
        b["evidence"]["osrm_error"]
            .as_str()
            .unwrap()
            .contains("503"),
        "{b}"
    );
    assert!(b["evidence"]["runs_used"].as_u64().unwrap() >= 6, "{b}");
    let back_line = osrm::decode_polyline(b["encoded_polyline"].as_str().unwrap()).unwrap();
    let last_back = back[back.len() - 1].1;
    let end = *back_line.last().unwrap();
    assert!(
        gtfs_routes_service::editor::validation::haversine_m(
            end.0,
            end.1,
            last_back.0,
            last_back.1
        ) < 80.0,
        "the reverse line ends where the reverse route does"
    );
    let queries = ch_count();
    osrm_state.mode.store(0, Ordering::SeqCst);
    let (s, b, _, _) = call!(&app, gps_of("R_BACK"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["evidence"]["matched"], "osrm", "OSRM is tried again: {b}");
    assert_eq!(ch_count(), queries);

    // ---- the variant: its own bus drove it
    let (s, b, _, _) = call!(&app, gps_of("R_VAR"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b["evidence"]["buses"], 1,
        "only dev-v drives the variant: {b}"
    );

    // ---- a branch nobody drives: the runs seen pass its first stops only
    let (s, b, _, _) = call!(&app, gps_of("R_NOBODY"));
    assert_eq!(s, 422, "{b}");
    assert_eq!(code_of(&b), "gps_not_enough_runs");
    let d = &b["error"]["details"];
    assert_eq!(d["runs_used"], 0, "{d}");
    assert!(d["runs_seen"].as_u64().unwrap() > 0, "{d}");
    assert_eq!(d["min_runs"], 3);

    // ---- a route number no bus carries, and a route without one
    let (s, b, _, _) = call!(&app, gps_of("R_OLD"));
    assert_eq!(s, 422, "{b}");
    assert_eq!(code_of(&b), "gps_not_enough_runs");
    assert_eq!(b["error"]["details"]["bus_days"], 0);
    let (s, b, _, _) = call!(&app, gps_of("R_NONUM"));
    assert_eq!(s, 422, "{b}");
    assert_eq!(code_of(&b), "gps_no_route_number");

    // ---- with change_set: the route as the draft has it
    let (s, set, _, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{GPS_FEED}/change-sets"))
            .set_json(json!({"title": "gps line"}))
    );
    assert_eq!(s, 201, "{set}");
    let set_id = set["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set_id}/changes")).set_json(json!({
            "entity": "route", "op": "update", "entity_key": "R_OLD", "after": {"short_name": "T21G"}
        }))
    );
    assert_eq!(s, 201, "{b}");
    let (s, b, _, _) = call!(
        &app,
        editor_c.req(
            "POST",
            &format!("/feeds/{GPS_FEED}/routes/R_OLD/polyline:gps?change_set={set_id}")
        )
    );
    assert_eq!(s, 200, "the draft's route number finds the buses: {b}");
    let old_line = b["encoded_polyline"].as_str().unwrap().to_string();

    // ---- into the draft as a gps line, approved and committed
    let (s, b, _, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{set_id}/changes"))
            .set_json(json!({
                "entity": "route", "op": "update", "entity_key": "R_FWD",
                "after": {"encoded_polyline": fwd_line, "polyline_source": "gps"}
            }))
    );
    assert_eq!(s, 201, "{b}");
    assert!(
        !b["problems"]
            .as_array()
            .map(|p| p.iter().any(|x| x["level"] == "error"))
            .unwrap_or(false),
        "{b}"
    );
    let (s, b, _, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set_id}/submit"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _, _) = call!(
        &app,
        approver
            .req("POST", &format!("/change-sets/{set_id}/approve"))
            .set_json(json!({}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set_id}/commit"))
    );
    assert_eq!(s, 200, "{b}");
    let row = sqlx::query("SELECT encoded_polyline, polyline_source FROM gtfs_route WHERE gtfs_id = $1 AND route_id = 'R_FWD'")
        .bind(GPS_FEED)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row.get::<Option<String>, _>("polyline_source").as_deref(),
        Some("gps")
    );
    assert_eq!(
        row.get::<Option<String>, _>("encoded_polyline").as_deref(),
        Some(fwd_line.as_str())
    );
    assert!(!old_line.is_empty());

    // ---- a ClickHouse that does not answer in time
    let (s, b, _, _) = call!(&app, gps_of("R_SLOW"));
    assert_eq!(s, 504, "{b}");
    assert_eq!(code_of(&b), "gps_timeout");

    // ---- every statement: a bounded SELECT, readonly=2, basic auth
    use base64::Engine;
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{CH_USER}:{CH_PASSWORD}"))
    );
    let log = ch_log.lock().unwrap().clone();
    assert!(log.len() >= 8, "{} statements", log.len());
    for r in &log {
        assert!(
            r.query.split('&').any(|p| p == "readonly=2"),
            "readonly=2: {}",
            r.query
        );
        assert!(r.query.contains("max_execution_time="), "{}", r.query);
        assert!(!r.query.contains(CH_PASSWORD) && !r.body.contains(CH_PASSWORD));
        assert_eq!(r.auth.as_deref(), Some(basic.as_str()));
        assert!(r.body.starts_with("SELECT "), "{}", r.body);
        assert!(r.body.ends_with(" FORMAT TabSeparated"), "{}", r.body);
        assert!(!r.body.contains(';'));
        assert!(
            Regex::new(r"LIMIT \d+ OFFSET \d+ FORMAT")
                .unwrap()
                .is_match(&r.body),
            "{}",
            r.body
        );
        assert!(
            r.body.contains("timestamp >= toDateTime(") && r.body.contains("timestamp <= now()"),
            "time bound: {}",
            r.body
        );
        assert!(
            r.body.contains("lat BETWEEN") && r.body.contains("long BETWEEN"),
            "box: {}",
            r.body
        );
        assert!(
            r.body.contains("routeNumber")
                && (r.body.contains("GROUP BY device, day") || r.body.contains("deviceId) IN (")),
            "entity bound: {}",
            r.body
        );
    }
    // a day's track query reads only around the hours its buses carried the
    // route number (they ran from 06:00 to about 09:00)
    let times = Regex::new(r"toDateTime\((\d+)\)").unwrap();
    for r in log.iter().filter(|r| r.body.contains("arrayStringConcat")) {
        let t: Vec<i64> = times
            .captures_iter(&r.body)
            .map(|c| c[1].parse().unwrap())
            .collect();
        assert!(
            t[1] - t[0] <= 6 * 3600,
            "{} s read for one day",
            t[1] - t[0]
        );
    }
    // OSRM /match: chunks of at most 100 points, the options asked for
    for p in osrm_state
        .paths
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.starts_with("/match/"))
    {
        let n = parse_coords(p.split('?').next().unwrap()).len();
        assert!((2..=100).contains(&n), "{n} points");
        assert!(
            p.contains("radiuses=25;")
                && p.contains("tidy=true")
                && p.contains("gaps=ignore")
                && p.contains("geometries=polyline"),
            "{p}"
        );
    }

    exec(&pool, &clear_feed(GPS_FEED, &accounts)).await;
    std::fs::remove_dir_all(dir).ok();
    std::fs::remove_dir_all(dir0).ok();
}

// ================================================================ OSRM through the stops

const OSRM_FEED: &str = "editor_osrm_reason_test_feed";
const OSRM_ADMIN: &str = "admin@editor-osrm-test.invalid";
const OSRM_EDITOR: &str = "editor@editor-osrm-test.invalid";

#[actix_web::test]
async fn a_route_through_the_stops_says_why_it_failed() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let accounts = [OSRM_ADMIN, OSRM_EDITOR];
    let mut seed = clear_feed(OSRM_FEED, &accounts);
    seed.push(format!("INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{OSRM_FEED}', 'OSRM reasons test feed')"));
    // 60 stops every 100 m
    let long = stops_on(&corridor_xy(), 100.0, "L")[..60].to_vec();
    seed.extend(route_rows(OSRM_FEED, "R_LONG", Some("L1"), &long));
    // the 31st stop is out at sea: no road near it
    let mut snap = long[..40].to_vec();
    snap[30] = ("SEA".to_string(), (13.45, 80.40));
    seed.extend(route_rows(OSRM_FEED, "R_SNAP", Some("L2"), &snap));
    // the 34th stop is on an island: no road to it
    let mut island = long[..40].to_vec();
    island[33] = ("ISLAND".to_string(), (13.30, 80.30));
    seed.extend(route_rows(OSRM_FEED, "R_ISLAND", Some("L3"), &island));
    exec(&pool, &seed).await;

    let (osrm_url, osrm_state) = start_osrm();
    let signer = TestSigner::generate("osrm-test-key");
    let (s, dir) = settings(&signer, OSRM_ADMIN, Some(osrm_url.clone()));
    let app = test::init_service(App::new().configure(|cfg| {
        editor::configure(
            cfg,
            Some(Arc::new(EditorState::build(pool.clone(), s).unwrap())),
        )
    }))
    .await;
    let callers = sign_in_all(&app, &signer, OSRM_ADMIN, &[(OSRM_EDITOR, "editor")]).await;
    let editor_c = &callers[1];
    let osrm_of = |route: &str| {
        editor_c.req(
            "POST",
            &format!("/feeds/{OSRM_FEED}/routes/{route}/polyline:osrm"),
        )
    };

    // ---- 60 waypoints: three requests of at most 25, sharing their ends
    let (s, b, _, _) = call!(&app, osrm_of("R_LONG"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["polyline_source"], "osrm");
    assert_eq!(b["waypoints"], 60);
    assert_eq!(b["saved"], false);
    let line = osrm::decode_polyline(b["encoded_polyline"].as_str().unwrap()).unwrap();
    assert_eq!(
        line.len(),
        60,
        "one point per waypoint, the shared ends once"
    );
    assert!(
        (b["distance_m"].as_f64().unwrap() - 5_900.0).abs() < 30.0,
        "{b}"
    );
    let routes: Vec<Vec<(f64, f64)>> = osrm_state
        .paths
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.starts_with("/route/"))
        .map(|p| parse_coords(p.split('?').next().unwrap()))
        .collect();
    assert_eq!(
        routes.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![25, 25, 12]
    );
    assert_eq!(routes[0].last(), routes[1].first());
    assert_eq!(routes[1].last(), routes[2].first());

    // ---- a stop OSRM cannot put on a road
    let (s, b, _, _) = call!(&app, osrm_of("R_SNAP"));
    assert_eq!(s, 502, "{b}");
    assert_eq!(code_of(&b), "osrm_failed");
    let d = &b["error"]["details"];
    assert_eq!(d["reason"], "no_segment", "{d}");
    assert_eq!(d["osrm_code"], "NoSegment");
    assert_eq!(
        d["waypoint"], 30,
        "the index in the whole route, not the chunk"
    );
    assert_eq!(d["stop_id"], "SEA");
    assert_eq!(d["sequence"], 31);
    assert!(
        b["error"]["message"].as_str().unwrap().contains("STOP SEA"),
        "{b}"
    );

    // ---- a leg with no road: named by its two stops
    let (s, b, _, _) = call!(&app, osrm_of("R_ISLAND"));
    assert_eq!(s, 502, "{b}");
    let d = &b["error"]["details"];
    assert_eq!(d["reason"], "no_route", "{d}");
    assert_eq!(d["leg"], 32, "{d}");
    assert_eq!(d["from_stop_id"], long[32].0.as_str());
    assert_eq!(d["to_stop_id"], "ISLAND");
    assert_eq!(
        (d["from_sequence"].as_i64(), d["to_sequence"].as_i64()),
        (Some(33), Some(34))
    );
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("to STOP ISLAND"),
        "{b}"
    );

    // ---- an OSRM that is not there
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let (s2, dir2) = settings(
        &signer,
        OSRM_ADMIN,
        Some(format!("http://127.0.0.1:{port}")),
    );
    let app2 = test::init_service(App::new().configure(|cfg| {
        editor::configure(
            cfg,
            Some(Arc::new(EditorState::build(pool.clone(), s2).unwrap())),
        )
    }))
    .await;
    let (s, b, _, _) = call!(&app2, osrm_of("R_LONG"));
    assert_eq!(s, 502, "{b}");
    assert_eq!(b["error"]["details"]["reason"], "unreachable", "{b}");

    // ---- and none configured at all
    let (s3, dir3) = settings(&signer, OSRM_ADMIN, None);
    let app3 = test::init_service(App::new().configure(|cfg| {
        editor::configure(
            cfg,
            Some(Arc::new(EditorState::build(pool.clone(), s3).unwrap())),
        )
    }))
    .await;
    let (s, b, _, _) = call!(&app3, osrm_of("R_LONG"));
    assert_eq!(s, 503, "{b}");
    assert_eq!(code_of(&b), "osrm_unavailable");

    exec(&pool, &clear_feed(OSRM_FEED, &accounts)).await;
    for d in [dir, dir2, dir3] {
        std::fs::remove_dir_all(d).ok();
    }
}
