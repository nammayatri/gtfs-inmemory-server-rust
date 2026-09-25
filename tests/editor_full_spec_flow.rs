//! The stops, routes, stop orders and trips the editor always kept, with the
//! rest of their GTFS fields (docs/gtfs-editor.md section 18), end to end
//! against a real Postgres holding the editor schema (db/gtfs_editor/0001..0023):
//!
//! - a stop's zone, level, URL and accessibility, an entrance with its
//!   station, a route's URL, order and agency, a stop order's own numbering
//!   and continuous stops, a trip's cars - set through drafts and committed;
//! - what the draft refuses: a stop a route calls at turned into a node, an
//!   entrance under a platform, a platform's station set by a stop change, a
//!   level that does not exist, a new station from a stop change, and
//!   deleting what a record still names (an entrance a pathway leads from, a
//!   route an attribution credits, a service a timeframe runs on, a trip a
//!   transfer starts from, a station with entrances);
//! - a route calls only at stops: a station's node in an uploaded stop list
//!   is refused;
//! - the feed report says what the feed breaks of the reference;
//! - a stop merge moves the pathways, transfers, areas and join rules naming
//!   the duplicate to the stop kept, dropping any that would say twice what
//!   another already says.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local. Uses its own feed and accounts and removes its rows
//! afterwards.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, feed_io, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use gtfs_routes_service::gtfs::spec;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

#[path = "support/gtfs_fixture.rs"]
mod gtfs_fixture;

const AUD: &str = "gtfs.editor-full-spec-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_full_spec_test_feed";
const ADMIN: &str = "admin@editor-full-spec-test.invalid";
const EDITOR: &str = "editor@editor-full-spec-test.invalid";
const APPROVER: &str = "approver@editor-full-spec-test.invalid";

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

async fn clear(pool: &PgPool) {
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
            .bind(FEED)
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
    let dir = std::env::temp_dir().join(format!("editor-full-spec-{}", crypto::random_token()));
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

async fn scalar_text(pool: &PgPool, sql: &str) -> Option<String> {
    sqlx::query(sql)
        .fetch_optional(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .and_then(|r| r.try_get::<Option<String>, _>(0).ok().flatten())
}

async fn scalar_i64(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<i64, _>(0)
}

/// The codes a set's validation reports for one change.
fn codes_for(set: &Value, change_id: i64) -> Vec<String> {
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

#[actix_web::test]
async fn existing_entities_carry_every_gtfs_field() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;
    let who = feed_io::Importer {
        user_id: None,
        email: None,
        label: "editor_full_spec_flow".into(),
    };
    let seeded = feed_io::import_zip(&pool, &gtfs_fixture::fixture(FEED), None, false, &who)
        .await
        .unwrap();
    assert!(seeded.seeded, "{seeded:#?}");

    let signer = TestSigner::generate("full-spec-test-key");
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
    let mut callers = Vec::new();
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
    macro_rules! open_set {
        ($title:expr) => {{
            let (s, b, _) = call!(
                &app,
                editor_c
                    .req("POST", &format!("/feeds/{FEED}/change-sets"))
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
                editor_c
                    .req("POST", &format!("/change-sets/{}/changes", $set))
                    .set_json($change)
            );
            assert_eq!(s, 201, "{b}");
            b["change_id"].as_i64().unwrap()
        }};
    }
    macro_rules! bulk {
        ($set:expr, $kind:expr, $rows:expr, $dry_run:expr) => {{
            let (s, b, _) = call!(
                &app,
                editor_c
                    .req("POST", &format!("/change-sets/{}/bulk", $set))
                    .set_json(json!({"kind": $kind, "rows": $rows, "dry_run": $dry_run}))
            );
            assert_eq!(s, 200, "{b}");
            b
        }};
    }
    macro_rules! detail {
        ($set:expr) => {{
            let (s, b, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{}", $set)));
            assert_eq!(s, 200, "{b}");
            b
        }};
    }
    macro_rules! commit {
        ($set:expr) => {{
            let (s, b, _) = call!(
                &app,
                editor_c.req("POST", &format!("/change-sets/{}/submit", $set))
            );
            assert_eq!(s, 200, "submit: {b}");
            for step in ["approve", "commit"] {
                let (s, b, _) = call!(
                    &app,
                    approver.req("POST", &format!("/change-sets/{}/{step}", $set))
                );
                assert_eq!(s, 200, "{step}: {b}");
            }
        }};
    }
    let text = |sql: String| {
        let pool = pool.clone();
        async move { scalar_text(&pool, &sql).await }
    };

    // ---- the feed report: SELF is its own station, which is kept and said
    let (s, v, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/validation"))
    );
    assert_eq!(s, 200, "{v}");
    let codes = v["report"]["codes"].as_array().unwrap();
    let self_parent = codes
        .iter()
        .find(|c| c["code"] == "parent_is_itself")
        .unwrap_or_else(|| panic!("{v}"));
    assert_eq!(
        (&self_parent["level"], &self_parent["count"]),
        (&json!("warning"), &json!(1)),
        "{v}"
    );
    assert_eq!(v["counts"]["trips.txt"], 6, "{v}");

    // ---- the fields, set through a draft
    let a = open_set!("platforms, entrances, routes");
    added!(
        a,
        json!({"entity": "stop", "op": "update", "entity_key": "P2",
               "after": {"zone_id": "Z9", "wheelchair_boarding": 0, "stop_url": "https://m.example/p2", "level_id": "L0"}})
    );
    added!(
        a,
        json!({"entity": "stop", "op": "create",
               "after": {"stop_id": "E2", "name": "Central Gate B", "lat": 13.0806, "lon": 80.2706,
                         "location_type": 2, "parent_station": "STN", "level_id": "L0"}})
    );
    added!(
        a,
        json!({"entity": "route", "op": "update", "entity_key": "R2",
               "after": {"route_url": "https://b.example/21g", "route_sort_order": 5, "agency_id": "A1"}})
    );
    added!(
        a,
        json!({"entity": "service", "op": "create",
               "after": {"service_id": "SUN", "days": {"sunday": true},
                         "start_date": "2026-01-01", "end_date": "2026-12-31"}})
    );
    let (s, live, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R1/trips"))
    );
    assert_eq!(s, 200, "{live}");
    let mut items = live["items"].as_array().unwrap().clone();
    let t1 = items.iter().position(|t| t["trip_id"] == "T1").unwrap();
    assert_eq!(items[t1]["cars_allowed"], 2, "seeded: {}", items[t1]);
    let t2 = items.iter().position(|t| t["trip_id"] == "T2").unwrap();
    items[t2]["cars_allowed"] = json!(1);
    added!(
        a,
        json!({"entity": "route_trips", "op": "replace", "entity_key": "R1",
               "after": {"base_trips_hash": live["trips_hash"], "trips": items}})
    );
    // an upload's rows carry them by their GTFS names
    let res = bulk!(
        a,
        "stops",
        json!([{"action": "update", "stop_id": "B2", "zone_id": "Z3", "wheelchair_boarding": "2"}]),
        false
    );
    assert_eq!(res["summary"]["changes"], 1, "{res}");
    let res = bulk!(
        a,
        "route_stops",
        json!([
            {"action": "update", "route_id": "R2", "sequence": 1, "stop_id": "B2", "stop_type": "NEW STOP",
             "stage_no": 0, "stage_name": "", "stop_sequence": 10, "continuous_pickup": 0},
            {"action": "update", "route_id": "R2", "sequence": 2, "stop_id": "P2", "stop_type": "NEW STOP",
             "stage_no": 0, "stage_name": "", "stop_sequence": 20, "shape_dist_traveled": "3.5"},
        ]),
        false
    );
    assert_eq!(res["summary"]["changes"], 1, "{res}");
    let d = detail!(a);
    assert!(
        d["validation"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["level"] != "error"),
        "{d}"
    );
    commit!(a);

    // ---- committed
    let stop = |id: &str, cols: &str| {
        text(format!(
            "SELECT concat_ws('|', {cols}) FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = '{id}'"
        ))
    };
    assert_eq!(
        stop("P2", "zone_id, wheelchair_boarding, stop_url, level_id")
            .await
            .as_deref(),
        Some("Z9|0|https://m.example/p2|L0")
    );
    // the details carry them by their GTFS names, for the dashboard's forms
    let (s, p2, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/stops/P2"))
    );
    assert_eq!(s, 200, "{p2}");
    assert_eq!(
        (
            &p2["gtfs"]["zone_id"],
            &p2["gtfs"]["wheelchair_boarding"],
            &p2["gtfs"]["tts_stop_name"]
        ),
        (&json!("Z9"), &json!(0), &Value::Null),
        "{p2}"
    );
    let (s, r2, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R2"))
    );
    assert_eq!(s, 200, "{r2}");
    assert_eq!(
        (
            &r2["gtfs"]["route_url"],
            &r2["gtfs"]["agency_id"],
            &r2["gtfs"]["route_type"]
        ),
        (&json!("https://b.example/21g"), &json!("A1"), &json!(3)),
        "{r2}"
    );
    assert_eq!(
        stop("E2", "location_type, parent_station, level_id")
            .await
            .as_deref(),
        Some("2|STN|L0")
    );
    assert_eq!(
        stop("B2", "zone_id, wheelchair_boarding").await.as_deref(),
        Some("Z3|2")
    );
    assert_eq!(
        text(format!(
            "SELECT concat_ws('|', route_url, route_sort_order, agency_id) FROM gtfs_route \
             WHERE gtfs_id = '{FEED}' AND route_id = 'R2'"
        ))
        .await
        .as_deref(),
        Some("https://b.example/21g|5|A1")
    );
    assert_eq!(
        text(format!(
            "SELECT string_agg(concat_ws('/', stop_id, stop_sequence, continuous_pickup, shape_dist_traveled), ' ' ORDER BY sequence) \
             FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'R2' AND pattern_key = 1"
        ))
        .await
        .as_deref(),
        Some("B2/10/0 P2/20/3.5")
    );
    assert_eq!(
        text(format!(
            "SELECT string_agg(trip_id || '=' || coalesce(cars_allowed::text, '-'), ' ' ORDER BY trip_id) \
             FROM gtfs_trip WHERE gtfs_id = '{FEED}' AND route_id = 'R1'"
        ))
        .await
        .as_deref(),
        Some("T1=2 T2=1 T3=- T4=-")
    );

    // ---- what the draft refuses
    let b = open_set!("refused");
    let node = added!(
        b,
        json!({"entity": "stop", "op": "update", "entity_key": "P1", "after": {"location_type": 3}})
    );
    let under_platform = added!(
        b,
        json!({"entity": "stop", "op": "update", "entity_key": "E1", "after": {"parent_station": "P1"}})
    );
    let platform_parent = added!(
        b,
        json!({"entity": "stop", "op": "update", "entity_key": "B1", "after": {"parent_station": "STN"}})
    );
    let no_level = added!(
        b,
        json!({"entity": "stop", "op": "update", "entity_key": "B1", "after": {"level_id": "L9"}})
    );
    let entrance_used = added!(
        b,
        json!({"entity": "stop", "op": "delete", "entity_key": "E1", "after": null})
    );
    let route_used = added!(
        b,
        json!({"entity": "route", "op": "delete", "entity_key": "R2", "after": null})
    );
    let station_entrances = added!(
        b,
        json!({"entity": "station", "op": "delete", "entity_key": "STN", "after": null})
    );
    added!(
        b,
        json!({"entity": "timeframe", "op": "create",
               "after": {"timeframe_group_id": "sunday", "service_id": "SUN"}})
    );
    let service_used = added!(
        b,
        json!({"entity": "service", "op": "delete", "entity_key": "SUN", "after": null})
    );
    let (_, live, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R1/trips"))
    );
    let without_t1: Vec<Value> = live["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["trip_id"] != "T1")
        .cloned()
        .collect();
    let trip_used = added!(
        b,
        json!({"entity": "route_trips", "op": "replace", "entity_key": "R1",
               "after": {"base_trips_hash": live["trips_hash"], "trips": without_t1}})
    );
    let d = detail!(b);
    for (change, code) in [
        (node, "stop_in_use"),
        (under_platform, "parent_wrong_type"),
        (platform_parent, "use_station_change"),
        (no_level, "reference_not_found"),
        (entrance_used, "stop_in_use"),
        (route_used, "route_in_use"),
        (station_entrances, "station_has_entrances"),
        (service_used, "service_in_use"),
        (trip_used, "trip_in_use"),
    ] {
        assert_eq!(
            codes_for(&d, change),
            vec![format!("error:{code}")],
            "change {change}: {d}"
        );
    }
    // a station is made by station/create, not by a stop change
    let (s, res, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{b}/changes"))
            .set_json(json!({"entity": "stop", "op": "create",
                             "after": {"stop_id": "X1", "name": "X", "lat": 13.0, "lon": 80.0, "location_type": 1}}))
    );
    assert_eq!((s, code_of(&res)), (400, "invalid_change"), "{res}");
    assert_eq!(res["error"]["details"]["code"], "use_station_change");
    // a route calls only at stops
    let res = bulk!(
        b,
        "route_stops",
        json!([
            {"action": "update", "route_id": "R3", "sequence": 1, "stop_id": "N1", "stop_type": "NEW STOP",
             "stage_no": 0, "stage_name": ""},
            {"action": "update", "route_id": "R3", "sequence": 2, "stop_id": "B2", "stop_type": "NEW STOP",
             "stage_no": 0, "stage_name": ""},
        ]),
        true
    );
    let codes: Vec<&str> = res["rows"][0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, vec!["not_a_stop"], "{res}");
    let (s, res, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{b}/discard"))
    );
    assert_eq!(s, 200, "{res}");

    // ---- a merge moves what names the duplicate to the stop kept
    let c = open_set!("one platform");
    added!(
        c,
        json!({"entity": "stop", "op": "merge", "entity_key": "P2", "after": {"into_stop_id": "P1"}})
    );
    let d = detail!(c);
    assert!(
        d["validation"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["level"] != "error"),
        "{d}"
    );
    commit!(c);
    let n = |sql: String| {
        let pool = pool.clone();
        async move { scalar_i64(&pool, &sql).await }
    };
    for (table, cols) in [
        ("gtfs_pathway", "from_stop_id = 'P2' OR to_stop_id = 'P2'"),
        ("gtfs_transfer", "from_stop_id = 'P2' OR to_stop_id = 'P2'"),
        ("gtfs_stop_area", "stop_id = 'P2'"),
        (
            "gtfs_fare_leg_join_rule",
            "from_stop_id = 'P2' OR to_stop_id = 'P2'",
        ),
    ] {
        assert_eq!(
            n(format!(
                "SELECT count(*) FROM {table} WHERE gtfs_id = '{FEED}' AND ({cols})"
            ))
            .await,
            0,
            "{table} still names P2"
        );
    }
    assert_eq!(
        text(format!(
            "SELECT to_stop_id FROM gtfs_pathway WHERE gtfs_id = '{FEED}' AND pathway_id = 'PW2'"
        ))
        .await
        .as_deref(),
        Some("P1")
    );
    // P1 to P2 and P2 to P1 are now the same transfer, kept once; AR1 held both
    assert_eq!(
        n(format!(
            "SELECT count(*) FROM gtfs_transfer WHERE gtfs_id = '{FEED}' AND from_stop_id = 'P1' AND to_stop_id = 'P1'"
        ))
        .await,
        1
    );
    assert_eq!(
        n(format!(
            "SELECT count(*) FROM gtfs_stop_area WHERE gtfs_id = '{FEED}' AND area_id = 'AR1'"
        ))
        .await,
        1
    );

    clear(&pool).await;
}
