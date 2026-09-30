//! Scripted writes without a replay per change (docs/gtfs-editor.md section 19),
//! end to end against a real Postgres holding the editor schema: `replay=false`
//! on adding, editing and removing one change, and the `stop_merges` bulk kind -
//! its checks, a dry run, a real run that creates and merges retired ids in one
//! upload, changes identical to single adds, nothing written when a row has an
//! error, a commit after which GIMS's loader answers the retired ids with their
//! survivors, and a second upload of the same rows that adds nothing.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. Uses its own feed and accounts and removes its rows afterwards.
//! See scripts/editor_flow_test.sh.

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

const AUD: &str = "gtfs.editor-fast-writes-test.local";
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
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{feed}'"),
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
    let dir = std::env::temp_dir().join(format!("editor-fast-writes-{}", crypto::random_token()));
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

/// What a stored change says, without the ids and times that differ between
/// two drafts holding the same change.
fn stored(change: &Value) -> Value {
    json!({
        "entity": change["entity"], "op": change["op"], "entity_key": change["entity_key"],
        "base_row_version": change["base_row_version"], "before": change["before"],
        "after": change["after"],
    })
}

// ---------------------------------------------------------------- the flow

const FEED: &str = "editor_fast_writes_test_feed";
const ADMIN: &str = "admin@editor-fast-writes-test.invalid";
const EDITOR: &str = "editor@editor-fast-writes-test.invalid";
const APPROVER: &str = "approver@editor-fast-writes-test.invalid";

fn seed() -> Vec<String> {
    let mut s = clear_feed(FEED);
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, headsign_source) VALUES ('{FEED}', 'Editor fast writes test feed', 'fare_stage')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, location_type, deleted, provenance) VALUES \
         ('{FEED}', 'KEEP1', 'KEEP1', 'KEPT ONE', 13.01, 80.21, 0, false, '{{}}'), \
         ('{FEED}', 'KEEP2', 'KEEP2', 'KEPT TWO', 13.02, 80.22, 0, false, '{{}}'), \
         ('{FEED}', 'LIVEDUP', 'LIVEDUP', 'KEPT TWO', 13.0201, 80.22, 0, false, '{{}}'), \
         ('{FEED}', 'LIVE2', 'LIVE2', 'KEPT TWO', 13.0199, 80.22, 0, false, '{{}}'), \
         ('{FEED}', 'TWIN', 'TWIN', 'KEPT ONE', 13.0101, 80.21, 0, false, '{{}}'), \
         ('{FEED}', 'HUB', 'HUB', 'A HUB', 13.05, 80.25, 1, false, '{{}}'), \
         ('{FEED}', 'GONE_M', 'GONE_M', 'KEPT TWO', 13.02, 80.22, 0, true, '{{\"merged_into\": \"KEEP2\"}}')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id) VALUES \
         ('{FEED}', 'R1', 'T1', 'KEPT ONE To KEPT TWO', 'TESTAG')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES \
         ('{FEED}', 'R1', 1, 'KEEP1', 'NEW STOP', 1, 'KEPT ONE', '7'), \
         ('{FEED}', 'R1', 2, 'LIVEDUP', 'NEW STOP', 2, 'KEPT TWO', '7')"
    ));
    s.extend(reset_accounts(&[ADMIN, EDITOR, APPROVER]));
    s
}

#[actix_web::test]
async fn replay_false_and_bulk_stop_merges() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("fast-writes-test-key");
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
        let (s, b, _) = call!(
            &app,
            admin
                .req("PUT", &format!("/users/{id}/feeds/{FEED}"))
                .set_json(json!({"role": role}))
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
    let add = |c: &Caller, set: &str, query: &str, change: Value| {
        c.req("POST", &format!("/change-sets/{set}/changes{query}"))
            .set_json(change)
    };
    let bulk = |c: &Caller, set: &str, rows: &Vec<Value>, dry_run: bool, replay: Option<bool>| {
        let mut body = json!({"kind": "stop_merges", "rows": rows, "dry_run": dry_run});
        if let Some(r) = replay {
            body["replay"] = json!(r);
        }
        c.req("POST", &format!("/change-sets/{set}/bulk"))
            .set_json(body)
    };
    let changes_of = |set: &str| editor_c.req("GET", &format!("/change-sets/{set}?limit=500"));
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

    // ======================================================== replay=false
    let (s, set, _) = call!(&app, new_set(&editor_c, "single changes, no replay"));
    assert_eq!(s, 201, "{set}");
    let quiet = set["change_set_id"].as_str().unwrap().to_string();
    let (s, set, _) = call!(&app, new_set(&editor_c, "single changes, replayed"));
    assert_eq!(s, 201, "{set}");
    let loud = set["change_set_id"].as_str().unwrap().to_string();

    let create = json!({"entity": "stop", "op": "create", "entity_key": "OLDA",
                        "after": {"stop_id": "OLDA", "name": "KEPT ONE", "lat": 13.0102, "lon": 80.21}});
    let merge_a = json!({"entity": "stop", "op": "merge", "entity_key": "OLDA",
                         "after": {"into_stop_id": "KEEP1"}});
    let merge_live = json!({"entity": "stop", "op": "merge", "entity_key": "LIVEDUP",
                            "after": {"into_stop_id": "KEEP2"}});

    // the ids alone: no page of the set, so no replay
    let (s, b, _) = call!(
        &app,
        add(&editor_c, &quiet, "?replay=false", create.clone())
    );
    assert_eq!(s, 201, "{b}");
    let mut keys: Vec<&String> = b.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["change_id", "change_set_id"], "{b}");
    assert_eq!(b["change_set_id"], json!(quiet));
    for c in [&merge_a, &merge_live] {
        let (s, b, _) = call!(&app, add(&editor_c, &quiet, "?replay=false", c.clone()));
        assert_eq!(s, 201, "{b}");
        assert!(b.get("validation").is_none(), "{b}");
    }
    // the default is unchanged: the set's first page, replayed, with the id
    for c in [&create, &merge_a, &merge_live] {
        let (s, b, _) = call!(&app, add(&editor_c, &loud, "", c.clone()));
        assert_eq!(s, 201, "{b}");
        assert!(b["change_id"].is_i64(), "{b}");
        assert!(b.get("validation").is_some(), "{b}");
        assert!(b.get("can_submit").is_some(), "{b}");
    }
    // and both store the very same changes
    let (_, q, _) = call!(&app, changes_of(&quiet));
    let (_, l, _) = call!(&app, changes_of(&loud));
    let q_changes: Vec<Value> = q["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(stored)
        .collect();
    let l_changes: Vec<Value> = l["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(stored)
        .collect();
    assert_eq!(q_changes.len(), 3);
    assert_eq!(q_changes, l_changes);
    // a merge's snapshot names the route calling at the stop that goes
    assert_eq!(
        q_changes[2]["before"]["affected"][0]["route_id"],
        json!("R1"),
        "{q}"
    );
    assert_eq!(q["validation_summary"]["errors"], json!(0), "{q}");

    // editing and removing a change take the flag too
    let merge_change_id = q["changes"][1]["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        editor_c
            .req(
                "PUT",
                &format!("/change-sets/{quiet}/changes/{merge_change_id}?replay=false")
            )
            .set_json(json!({"after": {"into_stop_id": "KEEP1", "keep_name": "into"}}))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b,
        json!({"change_set_id": quiet, "change_id": merge_change_id})
    );
    let live_change_id = q["changes"][2]["change_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        editor_c.req(
            "DELETE",
            &format!("/change-sets/{quiet}/changes/{live_change_id}?replay=false")
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b,
        json!({"change_set_id": quiet, "removed_change_id": live_change_id})
    );
    let (_, q, _) = call!(&app, changes_of(&quiet));
    assert_eq!(q["change_count"], json!(2), "{q}");
    assert_eq!(q["changes"][1]["after"]["keep_name"], json!("into"), "{q}");
    // a bad change is still refused before anything is stored
    let (s, b, _) = call!(
        &app,
        add(
            &editor_c,
            &quiet,
            "?replay=false",
            json!({"entity": "stop", "op": "merge", "entity_key": "KEEP1", "after": {"into_stop_id": "KEEP1"}})
        )
    );
    assert_eq!(s, 400, "{b}");
    for set in [&quiet, &loud] {
        let (s, b, _) = call!(
            &app,
            editor_c.req("POST", &format!("/change-sets/{set}/discard"))
        );
        assert_eq!(s, 200, "{b}");
    }

    // ======================================================== bulk stop_merges
    let (s, set, _) = call!(&app, new_set(&editor_c, "retired ids"));
    assert_eq!(s, 201, "{set}");
    let merges = set["change_set_id"].as_str().unwrap().to_string();

    let rows = vec![
        // 1: retired before the editor was seeded: created, then merged
        json!({"from_stop_id": "OLD1", "into_stop_id": "KEEP1", "name": "KEPT ONE", "lat": 13.0102, "lon": 80.21}),
        // 2: no row and nothing to create it from
        json!({"from_stop_id": "OLD2", "into_stop_id": "KEEP1"}),
        // 3: a live duplicate on a route: merged, its route rows follow
        json!({"from_stop_id": "LIVEDUP", "into_stop_id": "KEEP2"}),
        // 4: already merged into the same stop
        json!({"from_stop_id": "GONE_M", "into_stop_id": "KEEP2"}),
        // 5: only stops are merged
        json!({"from_stop_id": "OLD3", "into_stop_id": "HUB", "name": "X", "lat": 13.0, "lon": 80.2}),
        // 6: kept stop that another row merges away
        json!({"from_stop_id": "OLD4", "into_stop_id": "OLD1", "name": "X", "lat": 13.0, "lon": 80.2}),
        // 7: into itself
        json!({"from_stop_id": "KEEP1", "into_stop_id": "KEEP1"}),
        // 8: prm_ stops are never merged
        json!({"from_stop_id": "prm_x", "into_stop_id": "KEEP1", "name": "X", "lat": 13.0, "lon": 80.2}),
        // 9: exists: the cells to create it are not used
        json!({"from_stop_id": "LIVE2", "into_stop_id": "KEEP2", "name": "X", "lat": 13.0, "lon": 80.2}),
        // 10: a kept stop that does not exist
        json!({"from_stop_id": "OLD5", "into_stop_id": "NOWHERE", "name": "X", "lat": 13.0, "lon": 80.2}),
        // 11: a column the kind does not have
        json!({"action": "add", "from_stop_id": "OLD6", "into_stop_id": "KEEP1"}),
    ];
    let (s, out, _) = call!(&app, bulk(&editor_c, &merges, &rows, true, None));
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["kind"], json!("stop_merges"));
    assert_eq!(out["rows"][0]["status"], json!("ok"), "{out}");
    assert_eq!(out["rows"][0]["change"]["op"], json!("merge"), "{out}");
    assert_eq!(row_codes(&out, 2), ["stop_not_found"], "{out}");
    assert_eq!(out["rows"][2]["status"], json!("ok"), "{out}");
    assert_eq!(row_codes(&out, 4), ["unchanged"], "{out}");
    assert_eq!(row_codes(&out, 5), ["stop_is_station"], "{out}");
    assert_eq!(row_codes(&out, 6), ["into_merged_in_upload"], "{out}");
    assert_eq!(row_codes(&out, 7), ["merge_same_stop"], "{out}");
    assert_eq!(row_codes(&out, 8), ["merge_prm_stop"], "{out}");
    assert_eq!(row_codes(&out, 9), ["cells_not_used"], "{out}");
    assert_eq!(row_codes(&out, 10), ["stop_not_found"], "{out}");
    assert_eq!(row_codes(&out, 11), ["invalid_row"], "{out}");
    // OLD1's create and merge, LIVEDUP's merge, LIVE2's merge
    assert_eq!(out["summary"]["changes"], json!(4), "{out}");
    assert_eq!(out["changes_preview"][0]["op"], json!("create"), "{out}");
    assert_eq!(
        out["changes_preview"][0]["entity_key"],
        json!("OLD1"),
        "{out}"
    );
    // a dry run writes nothing
    assert_eq!(
        scalar_i64(
            &pool,
            format!("SELECT count(*) FROM gtfs_change WHERE change_set_id = '{merges}'")
        )
        .await,
        0
    );
    // a real run with an error row writes nothing either
    let (s, b, _) = call!(&app, bulk(&editor_c, &merges, &rows, false, None));
    assert_eq!(s, 400, "{b}");
    assert_eq!(code_of(&b), "bulk_has_errors", "{b}");
    assert_eq!(
        scalar_i64(
            &pool,
            format!("SELECT count(*) FROM gtfs_change WHERE change_set_id = '{merges}'")
        )
        .await,
        0
    );
    // a duplicate from id marks both its rows
    let dupes = vec![
        json!({"from_stop_id": "OLD7", "into_stop_id": "KEEP1", "name": "X", "lat": 13.0, "lon": 80.2}),
        json!({"from_stop_id": "OLD7", "into_stop_id": "KEEP2", "name": "X", "lat": 13.0, "lon": 80.2}),
    ];
    let (s, out, _) = call!(&app, bulk(&editor_c, &merges, &dupes, true, None));
    assert_eq!(s, 200, "{out}");
    assert_eq!(row_codes(&out, 1), ["duplicate_in_upload"], "{out}");
    assert_eq!(row_codes(&out, 2), ["duplicate_in_upload"], "{out}");

    // the good rows, for real, without the set's page
    let good = vec![
        rows[0].clone(),
        rows[2].clone(),
        rows[3].clone(),
        rows[8].clone(),
    ];
    let (s, out, _) = call!(&app, bulk(&editor_c, &merges, &good, false, Some(false)));
    assert_eq!(s, 200, "{out}");
    assert!(out.get("change_set").is_none(), "{out}");
    let ids: Vec<i64> = out["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["change"]["change_id"].as_i64())
        .collect();
    assert_eq!(ids.len(), 3, "one merge change per row that merges: {out}");
    let (_, bulked, _) = call!(&app, changes_of(&merges));
    assert_eq!(bulked["change_count"], json!(4), "{bulked}");
    assert_eq!(bulked["validation_summary"]["errors"], json!(0), "{bulked}");

    // each change is exactly what a single add stores
    let (s, set, _) = call!(&app, new_set(&editor_c, "the same, one by one"));
    assert_eq!(s, 201, "{set}");
    let singles = set["change_set_id"].as_str().unwrap().to_string();
    for c in [
        json!({"entity": "stop", "op": "create", "entity_key": "OLD1",
               "after": {"stop_id": "OLD1", "name": "KEPT ONE", "lat": 13.0102, "lon": 80.21}}),
        json!({"entity": "stop", "op": "merge", "entity_key": "OLD1", "after": {"into_stop_id": "KEEP1"}}),
        json!({"entity": "stop", "op": "merge", "entity_key": "LIVEDUP", "after": {"into_stop_id": "KEEP2"}}),
        json!({"entity": "stop", "op": "merge", "entity_key": "LIVE2", "after": {"into_stop_id": "KEEP2"}}),
    ] {
        let (s, b, _) = call!(&app, add(&editor_c, &singles, "?replay=false", c));
        assert_eq!(s, 201, "{b}");
    }
    let (_, one_by_one, _) = call!(&app, changes_of(&singles));
    let a: Vec<Value> = bulked["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(stored)
        .collect();
    let b: Vec<Value> = one_by_one["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(stored)
        .collect();
    assert_eq!(a, b);
    let (s, b, _) = call!(
        &app,
        editor_c.req("POST", &format!("/change-sets/{singles}/discard"))
    );
    assert_eq!(s, 200, "{b}");

    // a second upload of the same rows into the draft adds nothing
    let (s, out, _) = call!(&app, bulk(&editor_c, &merges, &good, true, None));
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["summary"]["changes"], json!(0), "{out}");
    assert!(
        row_codes(&out, 1).contains(&"unchanged".to_string()),
        "{out}"
    );

    // ======================================================== commit
    release!(merges);
    let merged_into = |id: &str| {
        format!("SELECT provenance->>'merged_into' FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = '{id}' AND deleted")
    };
    assert_eq!(
        scalar_text(&pool, merged_into("OLD1")).await.as_deref(),
        Some("KEEP1")
    );
    assert_eq!(
        scalar_text(&pool, merged_into("LIVEDUP")).await.as_deref(),
        Some("KEEP2")
    );
    assert_eq!(
        scalar_text(&pool, merged_into("LIVE2")).await.as_deref(),
        Some("KEEP2")
    );
    assert_eq!(
        scalar_text(
            &pool,
            format!("SELECT stop_id FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND route_id = 'R1' AND sequence = 2")
        )
        .await
        .as_deref(),
        Some("KEEP2"),
        "the route calls at the stop that stays"
    );
    // GIMS answers every retired id with its survivor
    let loaded = GtfsDbSource::new(pool.clone(), vec![])
        .load_feed(FEED, &HashMap::new())
        .await
        .unwrap();
    for (old, new) in [
        ("OLD1", "KEEP1"),
        ("LIVEDUP", "KEEP2"),
        ("LIVE2", "KEEP2"),
        ("GONE_M", "KEEP2"),
    ] {
        assert_eq!(
            loaded.aliases.get(old).map(String::as_str),
            Some(new),
            "{old}"
        );
    }

    // once committed, the same upload finds every row already true
    let (s, set, _) = call!(&app, new_set(&editor_c, "again"));
    assert_eq!(s, 201, "{set}");
    let again = set["change_set_id"].as_str().unwrap().to_string();
    let (s, out, _) = call!(&app, bulk(&editor_c, &again, &good, false, None));
    assert_eq!(s, 200, "{out}");
    assert_eq!(out["summary"]["changes"], json!(0), "{out}");
    for n in 1..=3 {
        assert_eq!(row_codes(&out, n), ["unchanged"], "row {n}: {out}");
    }
    assert_eq!(
        scalar_i64(
            &pool,
            format!("SELECT count(*) FROM gtfs_change WHERE change_set_id = '{again}'")
        )
        .await,
        0
    );

    exec(&pool, &clear_feed(FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}
