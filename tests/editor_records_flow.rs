//! Every GTFS file the editor keeps as records, edited through drafts
//! (docs/gtfs-editor.md section 18), end to end against a real Postgres
//! holding the editor schema (db/gtfs_editor/0001..0023):
//!
//! - a record is created, updated and deleted by a change; a file with no id
//!   of its own gets a minted `r_` id; feed_info is the feed's one row; a
//!   shape's points are replaced whole;
//! - what the draft finds: a reference to nothing, a row still in use, a row
//!   saying what another already says - errors that block submit - and a row
//!   the live data already broke, which does not;
//! - an update based on a row another commit changed is a conflict;
//! - a `records` upload: one change a row, each row's findings the draft's;
//! - the reads: every file with its count, a file's rows, one row and what
//!   points at it, and a file's rows with a draft applied.
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

const AUD: &str = "gtfs.editor-records-test.local";
const BASE: &str = "/internal/gtfs-editor";
const FEED: &str = "editor_records_test_feed";
const ADMIN: &str = "admin@editor-records-test.invalid";
const EDITOR: &str = "editor@editor-records-test.invalid";
const APPROVER: &str = "approver@editor-records-test.invalid";

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
    let dir = std::env::temp_dir().join(format!("editor-records-{}", crypto::random_token()));
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
async fn records_are_edited_through_drafts() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;
    let who = feed_io::Importer {
        user_id: None,
        email: None,
        label: "editor_records_flow".into(),
    };
    let seeded = feed_io::import_zip(&pool, &gtfs_fixture::fixture(FEED), None, false, &who)
        .await
        .unwrap();
    assert!(seeded.seeded, "{seeded:#?}");

    let signer = TestSigner::generate("records-test-key");
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

    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "stations and fares"}))
    );
    assert_eq!(s, 201, "{b}");
    let set = b["change_set_id"].as_str().unwrap().to_string();
    let add = |body: Value| {
        editor_c
            .req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(body)
    };

    // ---- the changes a person drafts
    let (s, b, _) = call!(
        &app,
        add(json!({"entity": "level", "op": "create",
                   "after": {"level_id": "L1", "level_index": 1, "level_name": "Mezzanine"}}))
    );
    assert_eq!(s, 201, "{b}");
    let level = b["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        add(
            json!({"entity": "pathway", "op": "update", "entity_key": "PW1",
                   "after": {"length": 15, "signposted_as": "Platforms 1-2"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let pathway = b["change_id"].as_i64().unwrap();
    let pw1_before = &b["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["change_id"] == pathway)
        .unwrap()["before"];
    assert_eq!(
        pw1_before["length"], 12.5,
        "the live row as it was: {pw1_before}"
    );
    let (s, b, _) = call!(
        &app,
        add(json!({"entity": "transfer", "op": "create",
                   "after": {"from_stop_id": "P1", "to_stop_id": "B1", "transfer_type": 2, "min_transfer_time": 240}}))
    );
    assert_eq!(s, 201, "{b}");
    let transfer = b["change_id"].as_i64().unwrap();
    let minted = b["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["change_id"] == transfer)
        .unwrap()["entity_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(minted.starts_with("r_"), "{minted}");
    let (s, b, _) = call!(
        &app,
        add(json!({"entity": "feed_info", "op": "update", "after": {"feed_version": "v8"}}))
    );
    assert_eq!(s, 201, "{b}");
    let (s, b, _) = call!(
        &app,
        add(
            json!({"entity": "shape", "op": "update", "entity_key": "SH1",
                   "after": {"points": [{"lat": 13.08, "lon": 80.27}, {"lat": 13.085, "lon": 80.275}, {"lat": 13.1, "lon": 80.29}]}})
        )
    );
    assert_eq!(s, 201, "{b}");
    // ...and the ones the draft refuses
    let (s, b, _) = call!(
        &app,
        add(json!({"entity": "pathway", "op": "create",
                   "after": {"pathway_id": "PW9", "from_stop_id": "E1", "to_stop_id": "NOPE",
                             "pathway_mode": 1, "is_bidirectional": 1}}))
    );
    assert_eq!(s, 201, "{b}");
    let dangling = b["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        add(json!({"entity": "level", "op": "delete", "entity_key": "L-1"}))
    );
    assert_eq!(s, 201, "{b}");
    let in_use = b["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        add(
            json!({"entity": "stop_area", "op": "create", "after": {"area_id": "AR1", "stop_id": "P1"}})
        )
    );
    assert_eq!(s, 201, "{b}");
    let twin = b["change_id"].as_i64().unwrap();
    // what is wrong with a change's own fields is refused when it is added
    let (s, b, _) = call!(
        &app,
        add(json!({"entity": "pathway", "op": "create",
                   "after": {"pathway_id": "PWX", "from_stop_id": "E1", "to_stop_id": "P1",
                             "pathway_mode": 9, "is_bidirectional": 1}}))
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_change"), "{b}");
    assert_eq!(b["error"]["details"]["code"], "invalid_value");
    let (s, b, _) = call!(
        &app,
        add(
            json!({"entity": "pathway", "op": "update", "entity_key": "PW404", "after": {"length": 1}})
        )
    );
    assert_eq!((s, code_of(&b)), (404, "entity_not_found"), "{b}");

    let (s, detail, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{set}")));
    assert_eq!(s, 200, "{detail}");
    assert_eq!(
        codes_for(&detail, dangling),
        vec!["error:reference_not_found"]
    );
    assert_eq!(codes_for(&detail, in_use), vec!["error:record_in_use"]);
    assert_eq!(codes_for(&detail, twin), vec!["error:duplicate_record"]);
    assert!(codes_for(&detail, level).is_empty(), "{detail}");
    assert!(codes_for(&detail, pathway).is_empty(), "{detail}");
    assert!(codes_for(&detail, transfer).is_empty(), "{detail}");
    assert_eq!(detail["can_submit"], false);

    // ---- the draft's view of a file
    let (s, b, _) = call!(
        &app,
        editor_c.req(
            "GET",
            &format!("/change-sets/{set}/preview/files/levels.txt")
        )
    );
    assert_eq!(s, 200, "{b}");
    let ids: Vec<&str> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["level_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"L1"), "{b}");

    for id in [dangling, in_use, twin] {
        let (s, b, _) = call!(
            &app,
            editor_c.req("DELETE", &format!("/change-sets/{set}/changes/{id}"))
        );
        assert_eq!(s, 200, "{b}");
    }
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{set}/submit"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set}/approve"))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        approver.req("POST", &format!("/change-sets/{set}/commit"))
    );
    assert_eq!(s, 200, "{b}");

    // ---- committed
    assert_eq!(
        scalar_text(
            &pool,
            &format!(
                "SELECT level_name FROM gtfs_level WHERE gtfs_id = '{FEED}' AND level_id = 'L1'"
            )
        )
        .await
        .as_deref(),
        Some("Mezzanine")
    );
    assert_eq!(
        scalar_text(&pool, &format!("SELECT length::text FROM gtfs_pathway WHERE gtfs_id = '{FEED}' AND pathway_id = 'PW1'")).await.as_deref(),
        Some("15")
    );
    assert_eq!(
        scalar_text(&pool, &format!("SELECT to_stop_id FROM gtfs_transfer WHERE gtfs_id = '{FEED}' AND row_id = '{minted}'")).await.as_deref(),
        Some("B1")
    );
    assert_eq!(
        scalar_text(
            &pool,
            &format!("SELECT feed_version FROM gtfs_feed_info WHERE gtfs_id = '{FEED}'")
        )
        .await
        .as_deref(),
        Some("v8")
    );
    assert_eq!(
        scalar_i64(&pool, &format!("SELECT cardinality(shape_pt_lat)::bigint FROM gtfs_shape WHERE gtfs_id = '{FEED}' AND shape_id = 'SH1'")).await,
        3
    );
    // the export carries it
    let mut conn = pool.acquire().await.unwrap();
    let (m, _) = feed_io::load_model(&mut conn, FEED).await.unwrap();
    drop(conn);
    assert!(m.records["level"].iter().any(|r| r.key == "L1"));
    assert_eq!(m.shapes[0].points.len(), 3);

    // ---- a draft based on a row another commit then changed
    let (_, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "stale"}))
    );
    let stale = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{stale}/changes"))
            .set_json(json!({"entity": "pathway", "op": "update", "entity_key": "PW2", "after": {"traversal_time": 35}}))
    );
    assert_eq!(s, 201, "{b}");
    let (_, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "first"}))
    );
    let first = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        editor_c
            .req("POST", &format!("/change-sets/{first}/changes"))
            .set_json(json!({"entity": "pathway", "op": "update", "entity_key": "PW2", "after": {"traversal_time": 25}}))
    );
    assert_eq!(s, 201, "{b}");
    for step in ["submit", "approve", "commit"] {
        let who = if step == "submit" { editor_c } else { approver };
        let (s, b, _) = call!(
            &app,
            who.req("POST", &format!("/change-sets/{first}/{step}"))
        );
        assert_eq!(s, 200, "{step}: {b}");
    }
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{stale}/submit"))
    );
    assert_eq!((s, code_of(&b)), (409, "change_set_conflicts"), "{b}");
    let conflict = &b["error"]["details"]["conflicts"][0];
    assert_eq!(
        (conflict["entity"].clone(), conflict["reason"].clone()),
        (json!("pathway"), json!("changed"))
    );
    assert!(
        conflict["message"]
            .as_str()
            .unwrap()
            .starts_with("Pathway PW2"),
        "{conflict}"
    );

    // ---- a records upload: one change a row, found as the draft finds them
    let bulk = |rows: Value, dry_run: bool| {
        editor_c
            .req("POST", &format!("/change-sets/{stale}/bulk"))
            .set_json(json!({"kind": "records", "file": "pathways.txt", "rows": rows, "dry_run": dry_run}))
    };
    let rows = json!([
        {"action": "add", "pathway_id": "PW10", "from_stop_id": "E1", "to_stop_id": "P2", "pathway_mode": "4", "is_bidirectional": "0"},
        {"action": "update", "pathway_id": "PW3", "length": "6.5"},
        {"action": "delete", "pathway_id": "PW3"},
        {"action": "add", "pathway_id": "PW11", "from_stop_id": "E1", "to_stop_id": "NOPE", "pathway_mode": "1", "is_bidirectional": "1"},
        {"action": "add", "pathway_id": "PW12", "from_stop_id": "E1", "pathway_mode": "1", "is_bidirectional": "1"},
        {"action": "delete", "pathway_id": "PW404"},
    ]);
    let (s, res, _) = call!(&app, bulk(rows.clone(), true));
    assert_eq!(s, 200, "{res}");
    let status: Vec<(String, Vec<String>)> = res["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["status"].as_str().unwrap().to_string(),
                r["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| m["code"].as_str().unwrap().to_string())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(status[0].0, "ok", "{res}");
    assert!(
        status[1].1.contains(&"duplicate_in_upload".to_string()),
        "{status:?}"
    );
    assert!(
        status[3].1.contains(&"reference_not_found".to_string()),
        "{status:?}"
    );
    assert!(
        status[4].1.contains(&"missing_field".to_string()),
        "{status:?}"
    );
    assert!(
        status[5].1.contains(&"record_not_found".to_string()),
        "{status:?}"
    );
    let (s, res, _) = call!(&app, bulk(rows, false));
    assert_eq!((s, code_of(&res)), (400, "bulk_has_errors"), "{res}");
    let (s, res, _) = call!(&app, bulk(json!([rows_ok()]), false));
    assert_eq!(s, 200, "{res}");
    assert_eq!(res["summary"]["changes"], 1, "{res}");

    // ---- the reads
    let (s, b, _) = call!(&app, editor_c.req("GET", &format!("/feeds/{FEED}/files")));
    assert_eq!(s, 200, "{b}");
    let rows_of = |file: &str| {
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["file"] == file)
            .unwrap()["rows"]
            .as_i64()
            .unwrap()
    };
    assert_eq!(rows_of("pathways.txt"), 3);
    assert_eq!(rows_of("levels.txt"), 3);
    assert_eq!(rows_of("stops.txt"), 8);
    assert_eq!(rows_of("stop_times.txt"), 15);
    let (s, b, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/files/pathways?q=E1&limit=1"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["items"].as_array().unwrap().len(), 1);
    assert!(b["next_cursor"].is_string(), "{b}");
    let (s, b, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/files/levels.txt/L-1"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b["used_by"],
        json!([{"file": "stops.txt", "field": "level_id", "rows": 2}])
    );
    let (s, b, _) = call!(
        &app,
        editor_c.req("GET", &format!("/feeds/{FEED}/files/stops.txt"))
    );
    assert_eq!((s, code_of(&b)), (400, "not_a_record_file"), "{b}");

    clear(&pool).await;
}

fn rows_ok() -> Value {
    json!({"action": "add", "pathway_id": "PW10", "from_stop_id": "E1", "to_stop_id": "P2",
           "pathway_mode": "4", "is_bidirectional": "0"})
}
