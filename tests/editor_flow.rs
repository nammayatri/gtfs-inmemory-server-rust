//! End-to-end editor flow against a real Postgres holding the editor schema.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. It uses its own feed (`editor_test_feed`) and its own accounts, so
//! it never touches another feed's rows. See scripts/editor_flow_test.sh.
//!
//! The JWT path is the real one: an ES256 key is generated, its JWKS written to
//! a temp file, and every request carries a token signed with it.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

const FEED: &str = "editor_test_feed";
const AUD: &str = "gtfs.editor-test.local";
const ADMIN: &str = "admin@editor-test.invalid";
const EDITOR: &str = "editor@editor-test.invalid";
const APPROVER: &str = "approver@editor-test.invalid";
const BASE: &str = "/internal/gtfs-editor";

async fn setup_db(pool: &PgPool) {
    let stmts = [
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("UPDATE gtfs_stop SET parent_station = NULL WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
        format!("INSERT INTO gtfs_feed (gtfs_id, display_name, headsign_source) VALUES ('{FEED}', 'Editor test feed', 'fare_stage')"),
        format!(
            "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) SELECT '{FEED}', 'S' || i, 'S' || i, \
             'STOP ' || i, 13.0 + i * 0.001, 80.2 FROM generate_series(1, 6) i"
        ),
        format!("INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name) VALUES ('{FEED}', 'R1', 'T1', 'STOP 1 To STOP 5')"),
        format!(
            "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name) VALUES \
             ('{FEED}', 'R1', 1, 'S1', 'NEW STOP', 1, 'STOP 1'), \
             ('{FEED}', 'R1', 2, 'S2', 'INTERMEDIATE STOP', 1, 'STOP 1'), \
             ('{FEED}', 'R1', 3, 'S3', 'NEW STOP', 2, 'STOP 3'), \
             ('{FEED}', 'R1', 4, 'S4', 'INTERMEDIATE STOP', 2, 'STOP 3'), \
             ('{FEED}', 'R1', 5, 'S5', 'NEW STOP', 3, 'STOP 5')"
        ),
        // what a replace must keep: a row's provenance (same stop, same place),
        // the route's provider id, and a route's own spelling of a stop
        format!(
            "UPDATE gtfs_route_stop SET provider_id = '7', \
               provenance = CASE WHEN sequence = 1 THEN '{{\"human_decision\": \"kept\"}}'::jsonb END, \
               stop_name_override = CASE WHEN sequence = 4 THEN 'STOP 4 ON R1' END \
             WHERE gtfs_id = '{FEED}' AND route_id = 'R1'"
        ),
        // accounts from an earlier run start over: no authenticator, active
        format!(
            "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, totp_last_step = NULL, \
             status = 'active' WHERE email IN ('{ADMIN}', '{EDITOR}', '{APPROVER}')"
        ),
        format!(
            "DELETE FROM gtfs_editor_session WHERE user_id IN (SELECT user_id FROM gtfs_editor_user \
             WHERE email IN ('{ADMIN}', '{EDITOR}', '{APPROVER}'))"
        ),
    ];
    for s in stmts {
        sqlx::query(&s)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{s}: {e}"));
    }
}

async fn cleanup_db(pool: &PgPool) {
    for s in [
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("UPDATE gtfs_stop SET parent_station = NULL WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
    ] {
        sqlx::query(&s).execute(pool).await.unwrap();
    }
}

struct Caller<'a> {
    signer: &'a TestSigner,
    email: &'static str,
    session: Option<String>,
}

impl Caller<'_> {
    fn req(&self, method: &str, path: &str) -> test::TestRequest {
        let uri = format!("{BASE}{path}");
        let r = match method {
            "GET" => test::TestRequest::get(),
            "POST" => test::TestRequest::post(),
            "PUT" => test::TestRequest::put(),
            "PATCH" => test::TestRequest::patch(),
            "DELETE" => test::TestRequest::delete(),
            _ => unreachable!(),
        }
        .uri(&uri)
        .insert_header((
            "x-pomerium-jwt-assertion",
            self.signer.token_for(self.email, AUD, 300),
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

#[actix_web::test]
async fn editor_end_to_end() {
    let Ok(url) = std::env::var("EDITOR_TEST_DATABASE_URL") else {
        eprintln!("EDITOR_TEST_DATABASE_URL not set; skipping the editor flow test");
        return;
    };
    assert!(
        url.contains("@127.0.0.1") || url.contains("@localhost"),
        "the editor flow test only runs against a local database"
    );
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .unwrap();
    setup_db(&pool).await;

    let signer = TestSigner::generate("test-key");
    let dir = std::env::temp_dir().join(format!("editor-flow-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    let state = EditorState::build(
        pool.clone(),
        EditorSettings {
            jwks_url: format!("file://{}", jwks.display()),
            audience: AUD.into(),
            bootstrap_admins: vec![ADMIN.to_uppercase()],
            totp_key_b64: base64_key(),
            session_hours: 1,
            ui_dir: dir.join("no-ui"),
            osrm_url: None,
            webhook_policy: Default::default(),
        },
    )
    .unwrap();
    let app = test::init_service(
        App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(state)))),
    )
    .await;

    // ---- the JWT gate
    let (s, b, _) = call!(
        &app,
        test::TestRequest::get().uri(&format!("{BASE}/auth/me"))
    );
    assert_eq!((s, code_of(&b)), (401, "no_sso_identity"));
    assert_eq!(b["error"]["details"]["reason"], "jwt_missing");
    let wrong_aud = test::TestRequest::get()
        .uri(&format!("{BASE}/auth/me"))
        .insert_header((
            "x-pomerium-jwt-assertion",
            signer.token_for(ADMIN, "gims.other.local", 300),
        ));
    let (s, b, _) = call!(&app, wrong_aud);
    assert_eq!((s, code_of(&b)), (401, "no_sso_identity"));
    assert_eq!(b["error"]["details"]["reason"], "jwt_wrong_audience");
    let stranger = TestSigner::generate("test-key");
    let forged = test::TestRequest::get()
        .uri(&format!("{BASE}/auth/me"))
        .insert_header((
            "x-pomerium-jwt-assertion",
            stranger.token_for(ADMIN, AUD, 300),
        ));
    let (s, b, _) = call!(&app, forged);
    assert_eq!((s, code_of(&b)), (401, "no_sso_identity"));
    assert_eq!(b["error"]["details"]["reason"], "jwt_bad_signature");
    // a real SSO identity with no editor account
    let unknown = test::TestRequest::get()
        .uri(&format!("{BASE}/auth/me"))
        .insert_header((
            "x-pomerium-jwt-assertion",
            signer.token_for("nobody@editor-test.invalid", AUD, 300),
        ));
    let (s, b, _) = call!(&app, unknown);
    assert_eq!((s, code_of(&b)), (403, "not_registered"));

    // the UI is served (placeholder) without a JWT
    let ui = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("{BASE}/ui/"))
            .to_request(),
    )
    .await;
    assert_eq!(ui.status().as_u16(), 200);

    // ---- admin bootstrap, enrolment, session
    let mut admin = Caller {
        signer: &signer,
        email: ADMIN,
        session: None,
    };
    let (s, b, _) = call!(&app, admin.req("GET", "/auth/me"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["role"], "admin");
    assert_eq!(b["session"], false);
    let (s, b, _) = call!(&app, admin.req("GET", "/feeds"));
    assert_eq!((s, code_of(&b)), (401, "totp_enrollment_required"));
    let (s, b, _) = call!(&app, admin.req("POST", "/auth/totp/enroll"));
    assert_eq!(s, 200, "{b}");
    assert!(b["otpauth_uri"]
        .as_str()
        .unwrap()
        .starts_with("otpauth://totp/"));
    let admin_secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": "000000"}))
    );
    assert!(
        s == 401 && code_of(&b) == "invalid_code" || s == 200,
        "{s} {b}"
    );
    let code = crypto::totp_now(&admin_secret, now());
    let (s, b, cookie) = call!(
        &app,
        admin
            .req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": code}))
    );
    assert_eq!(s, 200, "{b}");
    admin.session = cookie;
    assert!(admin.session.is_some());
    // the same code again is a replay
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/auth/session")
            .set_json(json!({"code": code}))
    );
    assert_eq!((s, code_of(&b)), (401, "code_reused"));
    let (s, b, _) = call!(&app, admin.req("GET", "/auth/me"));
    assert_eq!((s, b["session"].clone()), (200, json!(true)));

    // ---- mutations need X-Requested-With
    let no_csrf = test::TestRequest::post()
        .uri(&format!("{BASE}/users"))
        .insert_header((
            "x-pomerium-jwt-assertion",
            signer.token_for(ADMIN, AUD, 300),
        ))
        .insert_header((
            "cookie",
            format!("gtfs_editor_session={}", admin.session.clone().unwrap()),
        ))
        .set_json(json!({"email": EDITOR, "role": "editor"}));
    let (s, b, _) = call!(&app, no_csrf);
    assert_eq!((s, code_of(&b)), (403, "csrf_header_required"));

    // ---- users
    for (email, role) in [(EDITOR, "editor"), (APPROVER, "approver")] {
        let (s, b, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(
            s == 201 || (s == 409 && code_of(&b) == "user_exists"),
            "{s} {b}"
        );
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
    // an admin cannot demote themselves
    let admin_id = users["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == ADMIN)
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, b, _) = call!(
        &app,
        admin
            .req("PATCH", &format!("/users/{admin_id}"))
            .set_json(json!({"role": "viewer"}))
    );
    assert_eq!((s, code_of(&b)), (400, "cannot_change_self"));

    let mut editor_c = Caller {
        signer: &signer,
        email: EDITOR,
        session: None,
    };
    let mut approver = Caller {
        signer: &signer,
        email: APPROVER,
        session: None,
    };
    for c in [&mut editor_c, &mut approver] {
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
    // a session belongs to its user: the editor's cookie with the approver's JWT fails
    let swapped = Caller {
        signer: &signer,
        email: APPROVER,
        session: editor_c.session.clone(),
    };
    let (s, b, _) = call!(&app, swapped.req("GET", "/feeds"));
    assert_eq!((s, code_of(&b)), (401, "session_required"));

    // ---- reads
    let (s, feeds, _) = call!(&app, editor_c.req("GET", "/feeds"));
    assert_eq!(s, 200);
    assert!(feeds["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["gtfs_id"] == FEED));
    let (s, stops, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/stops?q=STOP&limit=4"))
    );
    assert_eq!(s, 200, "{stops}");
    assert_eq!(stops["items"].as_array().unwrap().len(), 4);
    assert!(stops["next_cursor"].is_string());
    let (_, bbox, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/feeds/{FEED}/stops?bbox=13.0015,80.1,13.0035,80.3")
        )
    );
    assert_eq!(bbox["items"].as_array().unwrap().len(), 2, "{bbox}");
    let (s, route, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R1"))
    );
    assert_eq!(s, 200, "{route}");
    let h0 = route["rows_hash"].as_str().unwrap().to_string();
    let (_, s2, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/stops/S2"))
    );
    let v_s2 = s2["row_version"].as_i64().unwrap();
    assert_eq!(s2["routes"][0]["route_id"], "R1");
    let version0 = feed_version(&pool).await;

    // ---- set A: rename S2, add S6 as an intermediate, club S5 into a station
    let (s, a, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "A"}))
    );
    assert_eq!(s, 201, "{a}");
    let set_a = a["change_set_id"].as_str().unwrap().to_string();
    let rows = |extra: Value| {
        json!([
            {"stop_id": "S1", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "STOP 1"},
            {"stop_id": "S2", "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "STOP 1"},
            extra,
            {"stop_id": "S3", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "STOP 3"},
            {"stop_id": "S4", "stop_type": "INTERMEDIATE STOP", "stage_no": 2, "stage_name": "STOP 3",
             "stop_name_override": "STOP 4 ON R1"},
            {"stop_id": "S5", "stop_type": "NEW STOP", "stage_no": 3, "stage_name": "STOP 5"},
        ])
    };
    for change in [
        json!({"entity": "stop", "op": "update", "entity_key": "S2", "after": {"name": "STOP 2 RENAMED"}, "base_row_version": v_s2}),
        json!({"entity": "route_stops", "op": "replace", "entity_key": "R1", "after": {
            "base_rows_hash": h0,
            "rows": rows(json!({"stop_id": "S6", "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "STOP 1"}))}}),
        json!({"entity": "station", "op": "create", "entity_key": "ST1", "after": {
            "station_id": "ST1", "name": "STATION ONE", "lat": 13.005, "lon": 80.2, "member_stop_ids": ["S5", "S6"]}}),
    ] {
        let (s, b, _) = call!(
            &app,
            editor_c
                .req("POST", &format!("/change-sets/{set_a}/changes"))
                .set_json(change)
        );
        assert_eq!(s, 201, "{b}");
    }
    let (_, detail, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{set_a}")));
    let errors: Vec<&Value> = detail["validation"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["level"] == "error")
        .collect();
    assert!(errors.is_empty(), "{detail}");
    assert_eq!(detail["can_submit"], true);
    let (s, preview, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{set_a}/preview/routes/R1"))
    );
    assert_eq!(s, 200, "{preview}");
    assert_eq!(preview["rows"].as_array().unwrap().len(), 6);
    assert_eq!(preview["rows"][1]["stop_name"], "STOP 2 RENAMED");
    assert!(preview["validation"].is_array(), "{preview}");
    // a route's own spelling of a stop is carried through the replace
    assert_eq!(preview["rows"][4]["stop_name"], "STOP 4 ON R1");
    assert_eq!(preview["rows"][4]["stop_name_override"], "STOP 4 ON R1");
    assert_eq!(preview["stop_count"], 6);
    // the preview left the live data alone
    assert_eq!(route_len(&pool).await, 5);
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set_a}/submit"))
    );
    assert_eq!((s, b["status"].clone()), (200, json!("submitted")), "{b}");

    // ---- set B: a stale edit of S2, submitted before A lands
    let (_, bset, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "B"}))
    );
    let set_b = bset["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set_b}/changes")).set_json(json!({
            "entity": "stop", "op": "update", "entity_key": "S2", "after": {"lat": 13.0021, "lon": 80.2001}, "base_row_version": v_s2}))
    );
    assert_eq!(s, 201, "{b}");
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set_b}/submit"))
    );
    assert_eq!(s, 200, "{b}");

    // ---- who may approve
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set_a}/approve"))
    );
    assert_eq!((s, code_of(&b)), (403, "role_required"));
    let (_, cset, _) = call!(
        &app,
        approver
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "C"}))
    );
    let set_c = cset["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        approver
            .req("POST", &format!("/change-sets/{set_c}/changes"))
            .set_json(json!({
            "entity": "route", "op": "update", "entity_key": "R1", "after": {"color": "#0A7E3C"}}))
    );
    assert_eq!(s, 201, "{b}");
    let (s, _, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set_c}/submit"))
    );
    assert_eq!(s, 200);
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set_c}/approve"))
    );
    assert_eq!((s, code_of(&b)), (403, "own_change_set"));
    // committing an unapproved set is refused
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set_c}/commit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_not_approved"));
    // ...and once someone else approves it, its submitter still cannot commit it
    let (s, _, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set_c}/approve"))
    );
    assert_eq!(s, 200);
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set_c}/commit"))
    );
    assert_eq!((s, code_of(&b)), (403, "own_change_set"));
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set_c}/discard"))
    );
    assert_eq!((s, b["status"].clone()), (200, json!("discarded")), "{b}");
    let (_, listed, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/feeds/{FEED}/change-sets?status=submitted,discarded")
        )
    );
    let statuses: Vec<&str> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["status"].as_str())
        .collect();
    assert!(
        statuses.contains(&"submitted") && statuses.contains(&"discarded"),
        "{listed}"
    );

    // ---- approve and commit A
    let (s, b, _) = call!(
        &app,
        approver
            .req("POST", &format!("/change-sets/{set_a}/approve"))
            .set_json(json!({"comment": "ok"}))
    );
    assert_eq!((s, b["status"].clone()), (200, json!("approved")), "{b}");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set_a}/commit"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["feed_version"].as_i64().unwrap(), version0 + 1);
    assert_eq!(feed_version(&pool).await, version0 + 1);
    assert_eq!(route_len(&pool).await, 6);
    let name: String = sqlx::query(&format!(
        "SELECT name FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'S2'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap()
    .get("name");
    assert_eq!(name, "STOP 2 RENAMED");
    let parent: Option<String> = sqlx::query(&format!(
        "SELECT parent_station FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'S5'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap()
    .get("parent_station");
    assert_eq!(parent.as_deref(), Some("ST1"));
    let kept = sqlx::query(&format!(
        "SELECT (SELECT provenance->>'human_decision' FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'R1' AND sequence = 1) AS prov, \
                (SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'R1' AND provider_id = '7') AS with_provider, \
                (SELECT stop_name_override FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'R1' AND stop_id = 'S4') AS over"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        kept.get::<Option<String>, _>("prov").as_deref(),
        Some("kept")
    );
    assert_eq!(
        kept.get::<i64, _>("with_provider"),
        6,
        "the inserted S6 row takes the route's provider"
    );
    assert_eq!(
        kept.get::<Option<String>, _>("over").as_deref(),
        Some("STOP 4 ON R1")
    );
    let (_, s5, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/stops/S5"))
    );
    assert_eq!(s5["parent"]["stop_id"], "ST1", "{s5}");
    assert_eq!(s5["route_count"], 1);
    assert!(
        s5["nearby"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["distance_m"].is_number() && n["route_count"].is_number()),
        "{s5}"
    );
    let (_, st1, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/stops/ST1"))
    );
    assert_eq!(st1["children"][0]["stop_id"], "S5");
    assert_eq!(st1["children"][0]["route_count"], 1);
    assert_eq!(st1["children"][1]["stop_id"], "S6");

    // ---- B is now stale: approve, then commit is a 409 and nothing changes
    let (s, _, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set_b}/approve"))
    );
    assert_eq!(s, 200);
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set_b}/commit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    assert_eq!(b["error"]["details"]["conflicts"][0]["entity_key"], "S2");
    assert_eq!(feed_version(&pool).await, version0 + 1);

    // ---- D: an intermediate carrying the wrong fare stage cannot be submitted
    let (_, route, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/routes/R1"))
    );
    let h1 = route["rows_hash"].as_str().unwrap().to_string();
    let (_, dset, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "D"}))
    );
    let set_d = dset["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set_d}/changes")).set_json(json!({
            "entity": "route_stops", "op": "replace", "entity_key": "R1", "after": {
                "base_rows_hash": h1,
                "rows": rows(json!({"stop_id": "S6", "stop_type": "INTERMEDIATE STOP", "stage_no": 2, "stage_name": "STOP 3"}))}}))
    );
    assert_eq!(s, 201, "{b}");
    assert!(
        b["validation"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["code"] == "fare_stage_mismatch"),
        "{b}"
    );
    // the preview shows the stop list as drafted, error and all, so the editor
    // reopens what was drafted rather than the live list
    let (s, preview, _) = call!(
        &app,
        editor_c.req("GET", &format!("/change-sets/{set_d}/preview/routes/R1"))
    );
    assert_eq!(s, 200, "{preview}");
    assert_eq!(
        (
            preview["rows"][2]["stop_id"].clone(),
            preview["rows"][2]["stage_no"].clone()
        ),
        (json!("S6"), json!(2)),
        "{preview}"
    );
    assert!(
        preview["validation"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["code"] == "fare_stage_mismatch" && v["level"] == "error"),
        "{preview}"
    );
    assert_eq!(route_len(&pool).await, 6, "the preview wrote nothing");
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set_d}/submit"))
    );
    assert_eq!((s, code_of(&b)), (400, "validation_failed"), "{b}");

    // a malformed change is refused outright
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set_d}/changes")).set_json(json!({
            "entity": "stop", "op": "update", "entity_key": "S1", "after": {"stop_type": "NEW STOP"}}))
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_change"));
    // deleting a stop a route still uses is an error in the draft
    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{set_d}/changes"))
            .set_json(json!({
            "entity": "stop", "op": "delete", "entity_key": "S1"}))
    );
    assert_eq!(s, 201);
    assert!(
        b["validation"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["code"] == "stop_in_use"),
        "{b}"
    );

    // ---- audit trail
    let (s, audit, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/audit?change_set={set_a}"))
    );
    assert_eq!(s, 200);
    let actions: Vec<&str> = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["action"].as_str())
        .collect();
    for want in [
        "change_set_created",
        "change_added",
        "change_set_submitted",
        "change_set_approved",
        "change_set_committed",
    ] {
        assert!(actions.contains(&want), "{want} missing from {actions:?}");
    }

    // ---- lockout: five wrong codes lock sign-in (a throwaway account per run)
    let lock_email: &'static str = Box::leak(
        format!(
            "lock-{}@editor-test.invalid",
            &crypto::random_token()[..10].to_lowercase()
        )
        .into_boxed_str(),
    );
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/users")
            .set_json(json!({"email": lock_email, "role": "viewer"}))
    );
    assert_eq!(s, 201, "{b}");
    let locker = Caller {
        signer: &signer,
        email: lock_email,
        session: None,
    };
    let (s, _, _) = call!(&app, locker.req("POST", "/auth/totp/enroll"));
    assert_eq!(s, 200);
    for left in (0..5).rev() {
        let (s, b, _) = call!(
            &app,
            locker
                .req("POST", "/auth/totp/confirm")
                .set_json(json!({"code": "12345"}))
        );
        assert_eq!((s, code_of(&b)), (401, "invalid_code"));
        assert_eq!(b["error"]["details"]["attempts_left"], left, "{b}");
    }
    let (s, b, _) = call!(
        &app,
        locker
            .req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": "123456"}))
    );
    assert_eq!((s, code_of(&b)), (429, "locked"), "{b}");
    assert!(
        b["error"]["details"]["retry_after_seconds"]
            .as_i64()
            .unwrap()
            > 0
    );

    // ---- admin self-approval: only an admin, only in so many words
    let draft_with_stop = |c: &Caller, title: &str| {
        (
            c.req("POST", &format!("/feeds/{FEED}/change-sets"))
                .set_json(json!({"title": title})),
            json!({"entity": "stop", "op": "create",
                   "after": {"name": title, "lat": 13.2, "lon": 80.2}}),
        )
    };
    let mut sets = Vec::new();
    for (c, title) in [(&admin, "ADMIN OWN"), (&approver, "APPROVER OWN")] {
        let (create, change) = draft_with_stop(c, title);
        let (s, b, _) = call!(&app, create);
        assert_eq!(s, 201, "{b}");
        assert_eq!(b["self_approved"], false, "{b}");
        let id = b["change_set_id"].as_str().unwrap().to_string();
        let (s, b, _) = call!(
            &app,
            c.req("POST", &format!("/change-sets/{id}/changes"))
                .set_json(change)
        );
        assert_eq!(s, 201, "{b}");
        let (s, b, _) = call!(&app, c.req("POST", &format!("/change-sets/{id}/submit")));
        assert_eq!(s, 200, "{b}");
        sets.push(id);
    }
    let (own, theirs) = (sets[0].clone(), sets[1].clone());
    let approve = |c: &Caller, id: &str, body: Value| {
        c.req("POST", &format!("/change-sets/{id}/approve"))
            .set_json(body)
    };
    // without the flag an admin is refused like anyone, and told the override exists
    for body in [json!({}), json!({"self_approve": false, "comment": "mine"})] {
        let (s, b, _) = call!(&app, approve(&admin, &own, body));
        assert_eq!((s, code_of(&b)), (403, "own_change_set"), "{b}");
        assert_eq!(b["error"]["details"]["can_self_approve"], true, "{b}");
    }
    // nobody else has the override, flag or not; and there is none for a reject
    let (s, b, _) = call!(
        &app,
        approve(&approver, &theirs, json!({"self_approve": true}))
    );
    assert_eq!((s, code_of(&b)), (403, "own_change_set"), "{b}");
    assert_eq!(b["error"]["details"]["can_self_approve"], false, "{b}");
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/change-sets/{own}/reject"))
            .set_json(json!({"comment": "no", "self_approve": true}))
    );
    assert_eq!((s, code_of(&b)), (403, "own_change_set"), "{b}");
    assert_eq!(b["error"]["details"]["can_self_approve"], false, "{b}");
    // a body that does not parse counts as no body: never the override
    let (s, b, _) = call!(&app, approve(&admin, &own, json!({"self_approve": "yes"})));
    assert_eq!((s, code_of(&b)), (403, "own_change_set"), "{b}");
    let (_, b, _) = call!(&app, admin.req("GET", &format!("/change-sets/{own}")));
    assert_eq!(
        (&b["status"], &b["self_approved"]),
        (&json!("submitted"), &json!(false))
    );

    // on someone else's set the flag is an ordinary approval
    let (s, b, _) = call!(
        &app,
        approve(&admin, &theirs, json!({"self_approve": true}))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (&b["status"], &b["self_approved"]),
        (&json!("approved"), &json!(false))
    );
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{theirs}/commit"))
    );
    assert_eq!((s, code_of(&b)), (403, "own_change_set"), "{b}");
    assert_eq!(b["error"]["details"]["can_self_approve"], false, "{b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{theirs}/commit"))
    );
    assert_eq!(s, 200, "{b}");

    // the override: approved, marked, and audited as what it was
    let (s, b, _) = call!(
        &app,
        approve(
            &admin,
            &own,
            json!({"self_approve": true, "comment": "urgent fix"})
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (&b["status"], &b["self_approved"]),
        (&json!("approved"), &json!(true))
    );
    assert_eq!(b["reviewed_by_email"], ADMIN);
    assert_eq!(b["submitted_by_email"], ADMIN);
    let (_, listed, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/change-sets?status=approved"))
    );
    let item = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["change_set_id"] == own.as_str())
        .unwrap();
    assert_eq!(item["self_approved"], true, "{item}");
    let audit_of = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "SELECT action, detail::text AS detail FROM gtfs_audit_log WHERE change_set_id = $1::uuid \
                 AND action IN ('change_set_approved', 'change_set_self_approved', 'change_set_committed') \
                 ORDER BY audit_id",
            )
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| {
                let detail: Value = serde_json::from_str(&r.get::<String, _>("detail")).unwrap();
                (r.get::<String, _>("action"), detail)
            })
            .collect::<Vec<_>>()
        }
    };
    let rows = audit_of(own.clone()).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "change_set_self_approved");
    assert_eq!(rows[0].1["comment"], "urgent fix");
    assert_eq!(rows[0].1["submitted_by_email"], ADMIN);
    assert!(rows[0].1["submitted_by"].is_string(), "{rows:?}");
    let rows = audit_of(theirs.clone()).await;
    let actions: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(actions, vec!["change_set_approved", "change_set_committed"]);
    assert_eq!(rows[1].1["self_approved"], false);

    // reopening clears the mark; approved by someone else, its submitter - admin
    // or not - still cannot commit it
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{own}/reopen"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        (&b["status"], &b["self_approved"]),
        (&json!("draft"), &json!(false))
    );
    assert!(b["reviewed_by"].is_null(), "{b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{own}/submit"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(&app, approve(&approver, &own, json!({})));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["self_approved"], false, "{b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{own}/commit"))
    );
    assert_eq!((s, code_of(&b)), (403, "own_change_set"), "{b}");

    // the override covers the commit too
    let (s, _, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{own}/reopen"))
    );
    assert_eq!(s, 200);
    let (s, _, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{own}/submit"))
    );
    assert_eq!(s, 200);
    let (s, b, _) = call!(&app, approve(&admin, &own, json!({"self_approve": true})));
    assert_eq!(s, 200, "{b}");
    let before_commit = feed_version(&pool).await;
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{own}/commit"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["feed_version"].as_i64().unwrap(), before_commit + 1);
    let (_, b, _) = call!(&app, admin.req("GET", &format!("/change-sets/{own}")));
    assert_eq!(
        (&b["status"], &b["self_approved"]),
        (&json!("committed"), &json!(true))
    );
    assert_eq!(
        (&b["committed_by_email"], &b["reviewed_by_email"]),
        (&json!(ADMIN), &json!(ADMIN))
    );
    let rows = audit_of(own.clone()).await;
    let actions: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(
        actions,
        vec![
            "change_set_self_approved",
            "change_set_approved",
            "change_set_self_approved",
            "change_set_committed"
        ]
    );
    assert_eq!(rows[3].1["self_approved"], true);

    // ---- sign out
    let (s, _, _) = call!(&app, editor_c.req("DELETE", "/auth/session"));
    assert_eq!(s, 204);
    let (s, b, _) = call!(&app, editor_c.req("GET", "/feeds"));
    assert_eq!((s, code_of(&b)), (401, "session_required"));

    cleanup_db(&pool).await;
    std::fs::remove_dir_all(dir).ok();
}

fn now() -> u64 {
    chrono::Utc::now().timestamp() as u64
}

fn base64_key() -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(crypto::random_bytes(32))
}

async fn feed_version(pool: &PgPool) -> i64 {
    sqlx::query(&format!(
        "SELECT version FROM gtfs_feed WHERE gtfs_id = '{FEED}'"
    ))
    .fetch_one(pool)
    .await
    .unwrap()
    .get("version")
}

async fn route_len(pool: &PgPool) -> i64 {
    sqlx::query(&format!(
        "SELECT count(*) AS n FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'R1'"
    ))
    .fetch_one(pool)
    .await
    .unwrap()
    .get("n")
}
