//! A route's map line and a GTFS zip's shapes (docs/gtfs-editor.md section 32),
//! end to end against a real Postgres holding the editor schema:
//!
//! - a seed gives each route its main shape as its line (`imported`), a route
//!   with no trips the shape named for it, and exports the zip it was given;
//! - lines drawn in the editor go out in the zip a feed gives (downloaded over
//!   HTTP, with how many in a header): a shape another route runs stays its, a
//!   route whose trips ran no shape gets one; exporting again gives the same
//!   bytes, and that zip seeded as another feed gives the same lines back;
//! - a draft import writes routes only when `routes.txt` is asked for: then the
//!   routes the feed has get their changed fields and lines as route updates,
//!   a route the feed lacks is reported and not made, and once committed the
//!   routes say what the zip says.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local. Uses its own feeds and accounts and removes their rows.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, draft_import, feed_io, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::gtfs::{compare, read, spec};
use gtfs_routes_service::services::osrm;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

#[path = "support/gtfs_fixture.rs"]
#[allow(dead_code)]
mod gtfs_fixture;
use gtfs_fixture::zip_of;

const AUD: &str = "gtfs.route-lines-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "route_lines_test_feed";
const AGAIN: &str = "route_lines_test_feed_again";
const ADMIN: &str = "admin@route-lines-test.invalid";
const EDITOR: &str = "editor@route-lines-test.invalid";
const APPROVER: &str = "approver@route-lines-test.invalid";

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

async fn clear(pool: &PgPool, feed: &str) {
    let mut tables: Vec<String> = [
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
    .iter()
    .map(|t| t.to_string())
    .collect();
    tables.extend(spec::FILES.iter().filter_map(|f| f.table()));
    tables.push("gtfs_editor_feed_access".into());
    tables.push("gtfs_feed".into());
    for t in tables {
        sqlx::query(&format!("DELETE FROM {t} WHERE gtfs_id = $1"))
            .bind(feed)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{t}: {e}"));
    }
    for email in [ADMIN, EDITOR, APPROVER] {
        sqlx::query(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, \
             totp_last_step = NULL, status = 'active' WHERE email = $1",
        )
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn state(pool: &PgPool, signer: &TestSigner) -> EditorState {
    let dir = std::env::temp_dir().join(format!("route-lines-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    use base64::Engine;
    EditorState::build(
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
    .unwrap()
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
    let now = chrono::Utc::now().timestamp() as u64;
    let (s, b, cookie) = call!(
        app,
        c.req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now)}))
    );
    assert_eq!(s, 200, "{b}");
    c.session = cookie;
}

async fn scalar_i64(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<i64, _>(0)
}

const SH1: [(f64, f64); 3] = [(13.01, 80.01), (13.015, 80.012), (13.02, 80.02)];
const SH_R3: [(f64, f64); 2] = [(13.05, 80.05), (13.06, 80.06)];

fn shapes_txt(shapes: &[(&str, &[(f64, f64)])]) -> String {
    let mut s = "shape_id,shape_pt_lat,shape_pt_lon,shape_pt_sequence\n".to_string();
    for (id, pts) in shapes {
        for (i, (la, lo)) in pts.iter().enumerate() {
            s.push_str(&format!("{id},{la},{lo},{}\n", i + 1));
        }
    }
    s
}

/// R1 and R4 run SH1; R2's trip runs no shape; R3 has no trips and a shape
/// named for it.
fn seed_zip(feed: &str) -> Vec<u8> {
    let feed_info = format!(
        "feed_publisher_name,feed_publisher_url,feed_lang,feed_version,feed_id\nNY,https://ny.example,en,v1,{feed}\n"
    );
    zip_of(&[
        ("agency.txt", "agency_id,agency_name,agency_url,agency_timezone\nA1,Metro,https://m.example,Asia/Kolkata\n"),
        ("feed_info.txt", &feed_info),
        ("stops.txt", "stop_id,stop_name,stop_lat,stop_lon\nS1,One,13.01,80.01\nS2,Two,13.02,80.02\nS3,Three,13.03,80.03\n"),
        ("routes.txt", "route_id,agency_id,route_short_name,route_type\nR1,A1,1,3\nR2,A1,2,3\nR3,A1,3,3\nR4,A1,4,3\n"),
        ("calendar.txt", "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\nWK,1,1,1,1,1,0,0,20260101,20261231\n"),
        ("trips.txt", "route_id,service_id,trip_id,shape_id\nR1,WK,T1,SH1\nR1,WK,T1B,SH1\nR2,WK,T2,\nR4,WK,T4,SH1\n"),
        ("stop_times.txt", "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
T1,06:00:00,06:00:15,S1,1\nT1,06:02:15,06:02:15,S2,2\n\
T1B,07:00:00,07:00:15,S1,1\nT1B,07:02:15,07:02:15,S2,2\n\
T2,06:30:00,06:30:15,S2,1\nT2,06:32:15,06:32:15,S3,2\n\
T4,08:00:00,08:00:15,S1,1\nT4,08:02:15,08:02:15,S2,2\n"),
        ("shapes.txt", &shapes_txt(&[("SH1", &SH1), ("R3", &SH_R3)])),
    ])
}

async fn line_of(pool: &PgPool, feed: &str, route: &str) -> (Option<String>, Option<String>) {
    let r = sqlx::query(
        "SELECT encoded_polyline, polyline_source FROM gtfs_route WHERE gtfs_id = $1 AND route_id = $2",
    )
    .bind(feed)
    .bind(route)
    .fetch_one(pool)
    .await
    .unwrap();
    (r.get(0), r.get(1))
}

fn enc(pts: &[(f64, f64)]) -> String {
    osrm::encode_polyline(pts)
}

/// The zip's trips' shapes and its shapes' lines.
fn zip_shapes(
    bytes: &[u8],
) -> (
    Vec<(String, String)>,
    std::collections::BTreeMap<String, String>,
) {
    let (raw, _) = read::read_zip(bytes).unwrap();
    let trips = raw.table("trips.txt").unwrap();
    let mut by_trip: Vec<(String, String)> = trips
        .rows
        .iter()
        .map(|r| {
            (
                trips.cell(r, "trip_id").to_string(),
                trips.cell(r, "shape_id").to_string(),
            )
        })
        .collect();
    by_trip.sort();
    let mut pts: std::collections::BTreeMap<String, Vec<(i64, f64, f64)>> = Default::default();
    if let Some(t) = raw.table("shapes.txt") {
        for r in &t.rows {
            pts.entry(t.cell(r, "shape_id").to_string())
                .or_default()
                .push((
                    t.cell(r, "shape_pt_sequence").parse().unwrap(),
                    t.cell(r, "shape_pt_lat").parse().unwrap(),
                    t.cell(r, "shape_pt_lon").parse().unwrap(),
                ));
        }
    }
    let lines = pts
        .into_iter()
        .map(|(id, mut p)| {
            p.sort_by_key(|x| x.0);
            (id, enc(&p.iter().map(|x| (x.1, x.2)).collect::<Vec<_>>()))
        })
        .collect();
    (by_trip, lines)
}

#[actix_web::test]
async fn map_lines_go_out_as_shapes_and_come_back_in() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool, FEED).await;
    clear(&pool, AGAIN).await;
    let who = feed_io::Importer {
        user_id: None,
        email: None,
        label: "gtfs_route_lines_flow".into(),
    };

    // ---- the seed: main shapes as lines, the zip back as it was
    let seeded = feed_io::import_zip(&pool, &seed_zip(FEED), Some(FEED), false, &who)
        .await
        .unwrap();
    assert!(seeded.seeded, "{seeded:#?}");
    assert!(seeded.round_trip.is_empty(), "{:?}", seeded.round_trip);
    assert_eq!(seeded.map_lines, 3, "R1 and R4 from SH1, R3 from its own");
    let imported = Some("imported".to_string());
    assert_eq!(
        line_of(&pool, FEED, "R1").await,
        (Some(enc(&SH1)), imported.clone())
    );
    assert_eq!(
        line_of(&pool, FEED, "R4").await,
        (Some(enc(&SH1)), imported.clone())
    );
    assert_eq!(
        line_of(&pool, FEED, "R3").await,
        (Some(enc(&SH_R3)), imported.clone())
    );
    assert_eq!(line_of(&pool, FEED, "R2").await, (None, None));
    {
        let mut conn = pool.acquire().await.unwrap();
        let (mut m, _) = feed_io::load_model(&mut conn, FEED).await.unwrap();
        let r = feed_io::with_route_lines(&mut conn, FEED, &mut m)
            .await
            .unwrap();
        assert_eq!(
            r.routes, 0,
            "imported lines whose shapes are there: nothing to write"
        );
        let (raw, _) = read::read_zip(&seed_zip(FEED)).unwrap();
        let diffs = compare::compare(
            &raw,
            &gtfs_routes_service::gtfs::write::to_raw(&m),
            &m.dropped,
        );
        assert!(diffs.is_empty(), "{diffs:?}");
    }

    // ---- lines drawn in the editor: R1 by hand, R2 from GPS
    const L1: [(f64, f64); 3] = [(13.01, 80.01), (13.012, 80.016), (13.02, 80.02)];
    const L2: [(f64, f64); 2] = [(13.02, 80.02), (13.03, 80.03)];
    for (route, line, source) in [("R1", &L1[..], "manual"), ("R2", &L2[..], "gps")] {
        sqlx::query("UPDATE gtfs_route SET encoded_polyline = $3, polyline_source = $4 WHERE gtfs_id = $1 AND route_id = $2")
            .bind(FEED)
            .bind(route)
            .bind(enc(line))
            .bind(source)
            .execute(&pool)
            .await
            .unwrap();
    }

    // ---- downloaded: in the zip, R4 keeping SH1
    let signer = TestSigner::generate("route-lines-test-key");
    let app = test::init_service(
        App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(state(&pool, &signer))))),
    )
    .await;
    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    sign_in(&app, &mut admin).await;
    let download = || async {
        let resp = test::call_service(
            &app,
            admin
                .req("GET", &format!("/feeds/{FEED}/gtfs.zip"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 200);
        let lines: String = resp
            .headers()
            .get("x-map-lines-as-shapes")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        (lines, test::read_body(resp).await.to_vec())
    };
    let (header, zip) = download().await;
    assert_eq!(header, "2");
    let (trips, shapes) = zip_shapes(&zip);
    assert_eq!(
        trips,
        vec![
            ("T1".into(), "R1".into()),
            ("T1B".into(), "R1".into()),
            ("T2".into(), "R2".into()),
            ("T4".into(), "SH1".into()),
        ]
    );
    assert_eq!(
        shapes.get("R1"),
        Some(&enc(&L1)),
        "R1's line, its own shape"
    );
    assert_eq!(shapes.get("R2"), Some(&enc(&L2)));
    assert_eq!(shapes.get("SH1"), Some(&enc(&SH1)), "R4's shape as it was");
    assert_eq!(shapes.get("R3"), Some(&enc(&SH_R3)));
    let (_, zip_again) = download().await;
    assert_eq!(zip, zip_again, "the same bytes again");

    // ---- that zip seeded as another feed: the same lines back
    let (raw, _) = read::read_zip(&zip).unwrap();
    let mut raw = raw;
    if let Some(fi) = raw.files.get_mut("feed_info.txt") {
        let col = fi.col("feed_id").unwrap();
        fi.rows[0][col] = AGAIN.to_string();
    }
    let bytes = gtfs_routes_service::gtfs::write::zip_bytes(&raw).unwrap();
    let again = feed_io::import_zip(&pool, &bytes, Some(AGAIN), false, &who)
        .await
        .unwrap();
    assert!(again.seeded && again.round_trip.is_empty(), "{again:#?}");
    assert_eq!(line_of(&pool, AGAIN, "R1").await.0, Some(enc(&L1)));
    assert_eq!(line_of(&pool, AGAIN, "R2").await.0, Some(enc(&L2)));
    assert_eq!(line_of(&pool, AGAIN, "R4").await.0, Some(enc(&SH1)));

    // ---- a draft import changes routes only when routes.txt is asked for
    let mut callers = Vec::new();
    let mut editor_id = None;
    for (email, role) in [(EDITOR, "editor"), (APPROVER, "approver")] {
        let (s, b, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
        let (_, users, _) = call!(&app, admin.req("GET", "/users"));
        let id = users["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["email"] == email)
            .unwrap()["user_id"]
            .as_str()
            .unwrap()
            .to_string();
        if role == "editor" {
            editor_id = Some(uuid::Uuid::parse_str(&id).unwrap());
        }
        let (s, b, _) = call!(
            &app,
            admin
                .req("PUT", &format!("/users/{id}/feeds/{FEED}"))
                .set_json(json!({"role": role}))
        );
        assert_eq!(s, 200, "{b}");
        let mut c = Caller {
            signer: &signer,
            email: email.into(),
            session: None,
        };
        sign_in(&app, &mut c).await;
        callers.push(c);
    }
    let (editor_c, approver) = (&callers[0], &callers[1]);
    // the operator's next zip: R1 renamed and coloured, R4 described and on a
    // new shape; R9 is new
    const SH4: [(f64, f64); 2] = [(13.01, 80.01), (13.025, 80.022)];
    let next = {
        let feed_info = format!(
            "feed_publisher_name,feed_publisher_url,feed_lang,feed_version,feed_id\nNY,https://ny.example,en,v2,{FEED}\n"
        );
        zip_of(&[
            ("agency.txt", "agency_id,agency_name,agency_url,agency_timezone\nA1,Metro,https://m.example,Asia/Kolkata\n"),
            ("feed_info.txt", &feed_info),
            ("stops.txt", "stop_id,stop_name,stop_lat,stop_lon\nS1,One,13.01,80.01\nS2,Two,13.02,80.02\nS3,Three,13.03,80.03\n"),
            ("routes.txt", "route_id,agency_id,route_short_name,route_type,route_color,route_desc\nR1,A1,1A,3,ff0000,\nR2,A1,2,3,,\nR3,A1,3,3,,\nR4,A1,4,3,,Via the beach\nR9,A1,9,3,,\n"),
            ("calendar.txt", "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\nWK,1,1,1,1,1,0,0,20260101,20261231\n"),
            ("trips.txt", "route_id,service_id,trip_id,shape_id\nR1,WK,T1,R1\nR1,WK,T1B,R1\nR2,WK,T2,R2\nR4,WK,T4,SH4\nR9,WK,T9,\n"),
            ("stop_times.txt", "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
T1,06:00:00,06:00:15,S1,1\nT1,06:02:15,06:02:15,S2,2\n\
T1B,07:00:00,07:00:15,S1,1\nT1B,07:02:15,07:02:15,S2,2\n\
T2,06:30:00,06:30:15,S2,1\nT2,06:32:15,06:32:15,S3,2\n\
T4,08:00:00,08:00:15,S1,1\nT4,08:02:15,08:02:15,S2,2\n\
T9,09:00:00,09:00:15,S1,1\nT9,09:02:15,09:02:15,S3,2\n"),
            ("shapes.txt", &shapes_txt(&[("R1", &L1), ("R2", &L2), ("R3", &SH_R3), ("SH1", &SH1), ("SH4", &SH4)])),
        ])
    };
    let import = |files: Option<Vec<String>>, dry_run: bool| draft_import::DraftImport {
        with_routes: files.as_deref() == Some(&["+routes".to_string()][..]),
        files: files.filter(|f| f[..] != ["+routes".to_string()][..]),
        dry_run,
        user_id: editor_id.unwrap(),
        email: EDITOR.into(),
        exclude_routes: Default::default(),
        title: None,
    };
    let r = draft_import::draft_import(&pool, &next, Some(FEED), &import(None, true))
        .await
        .unwrap();
    assert!(
        !r.files.contains(&"routes.txt".to_string()),
        "{:?}",
        r.files
    );
    assert_eq!(r.routes.updated, 0, "not asked for: no route touched");
    // the default files and the routes too: the dashboard's checkbox
    let r = draft_import::draft_import(
        &pool,
        &next,
        Some(FEED),
        &import(Some(vec!["+routes".to_string()]), true),
    )
    .await
    .unwrap();
    assert!(
        r.files.contains(&"routes.txt".to_string()) && r.files.contains(&"trips.txt".to_string()),
        "{:?}",
        r.files
    );
    assert_eq!(r.routes.updated, 2, "{:?}", r.routes);
    let routes_only = Some(vec!["routes.txt".to_string()]);
    let r =
        draft_import::draft_import(&pool, &next, Some(FEED), &import(routes_only.clone(), true))
            .await
            .unwrap();
    assert_eq!((r.errors, r.step), (0, "records"), "{r:#?}");
    assert_eq!((r.routes.updated, r.routes.lines), (2, 1), "{:?}", r.routes);
    assert_eq!(r.routes.updated_sample, vec!["R1", "R4"]);
    assert_eq!(
        (r.routes.not_in_feed, r.routes.not_in_feed_sample.clone()),
        (1, vec!["R9".to_string()])
    );
    let r = draft_import::draft_import(&pool, &next, Some(FEED), &import(routes_only, false))
        .await
        .unwrap();
    assert_eq!(r.errors, 0, "{r:#?}");
    let set = r.change_sets[0].change_set_id.unwrap();
    let changes = sqlx::query("SELECT entity, op, entity_key, after::text FROM gtfs_change WHERE change_set_id = $1 ORDER BY position")
        .bind(set)
        .fetch_all(&pool)
        .await
        .unwrap();
    let afters: Vec<(String, String, String, Value)> = changes
        .iter()
        .map(|c| {
            (
                c.get(0),
                c.get(1),
                c.get(2),
                serde_json::from_str(&c.get::<String, _>(3)).unwrap(),
            )
        })
        .collect();
    assert_eq!(
        afters,
        vec![
            (
                "route".into(),
                "update".into(),
                "R1".into(),
                json!({"short_name": "1A", "color": "#FF0000"})
            ),
            (
                "route".into(),
                "update".into(),
                "R4".into(),
                json!({"route_desc": "Via the beach", "encoded_polyline": enc(&SH4), "polyline_source": "manual"})
            ),
        ],
        "only what differs; R1's line is its shape already"
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set}/submit"))
    );
    assert_eq!(s, 200, "submit: {b}");
    for step in ["approve", "commit"] {
        let (s, b, _) = call!(
            &app,
            approver.req("POST", &format!("/change-sets/{set}/{step}"))
        );
        assert_eq!(s, 200, "{step}: {b}");
    }
    let r1 = sqlx::query(
        "SELECT short_name, color FROM gtfs_route WHERE gtfs_id = $1 AND route_id = 'R1'",
    )
    .bind(FEED)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (r1.get::<String, _>(0), r1.get::<String, _>(1)),
        ("1A".into(), "#FF0000".into())
    );
    // routes.txt alone: the line came without its shape and the trips that
    // run it, so it is kept as a line given by hand
    assert_eq!(
        line_of(&pool, FEED, "R4").await,
        (Some(enc(&SH4)), Some("manual".to_string()))
    );
    assert_eq!(
        scalar_i64(
            &pool,
            &format!(
                "SELECT count(*) FROM gtfs_route WHERE gtfs_id = '{FEED}' AND route_id = 'R9'"
            )
        )
        .await,
        0,
        "a route the feed lacks is not made"
    );
    // R4's line has no shape in the feed (shapes.txt was not asked
    // for): the zip the feed gives carries it all the same
    let (_, zip) = download().await;
    let (trips, shapes) = zip_shapes(&zip);
    // R1 (by hand) shared SH1 with R4: R1's trips run a shape of their own,
    // and SH1, now R4's alone, takes R4's line
    let run = |trip: &str| trips.iter().find(|(t, _)| t == trip).unwrap().1.clone();
    assert_eq!(
        (run("T1"), run("T1B"), run("T4")),
        ("R1".into(), "R1".into(), "SH1".into())
    );
    assert_eq!(shapes.get("R1"), Some(&enc(&L1)));
    assert_eq!(
        shapes.get("SH1"),
        Some(&enc(&SH4)),
        "R4 runs SH1, with its line"
    );

    clear(&pool, FEED).await;
    clear(&pool, AGAIN).await;
}
