//! Stage reviews (docs/gtfs-editor.md section 19.1), end to end against a real
//! Postgres holding the editor schema (db/gtfs_editor/0001..0025): the queue the
//! stage backfill raises when routes disagree about what a stage name means, and
//! the operations team working through it - listing it, filtering by reason, the
//! detail that puts the disagreeing stages side by side, closing a row as fixed
//! or confirmed, what that does to `gtfs_stage.review`, and reopening it.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. Uses its own feed and accounts and removes its rows afterwards.
//! See scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

const AUD: &str = "gtfs.editor-stage-review-test.local";
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
        format!("DELETE FROM gtfs_stage_review WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route_stage_issue WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route_stage WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_stage_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_stage WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{feed}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{feed}'"),
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
    let dir = std::env::temp_dir().join(format!("editor-stage-review-{}", crypto::random_token()));
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

// ---------------------------------------------------------------- the flow

const FEED: &str = "editor_stage_review_test_feed";
const ADMIN: &str = "admin@editor-stage-review-test.invalid";
const EDITOR: &str = "editor@editor-stage-review-test.invalid";
const VIEWER: &str = "viewer@editor-stage-review-test.invalid";

/// Two routes whose SAIDAPET stage starts at a different stop, which is what the
/// backfill raises `head_differs` for, plus a KOYAMBEDU pair the routes spell two
/// ways. The rows are the shape `scripts/backfill_stages.py` writes.
fn seed() -> Vec<String> {
    seed_for(FEED, &[ADMIN, EDITOR, VIEWER])
}

fn seed_for(feed: &str, accounts: &[&str]) -> Vec<String> {
    let mut s = clear_feed(feed);
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, use_stages) VALUES ('{feed}', 'Stage review test feed', true)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{feed}', c, c, 'STOP ' || c, 13.0 + ascii(c) * 0.001, 80.2 \
         FROM unnest(ARRAY['A','B','C','D','E']) c"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id) VALUES \
         ('{feed}', 'R1', '1', 'up', 'AG'), ('{feed}', 'R2', '2', 'also up', 'AG')"
    ));
    // SAIDAPET: R1 starts it at A, R2 at B - the same name, two places
    s.push(format!(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, name, direction, review, provenance) VALUES \
         ('{feed}', 'stg_one', 'SAIDAPET', 'up', 'head_differs', \
          '{{\"source\": \"backfill\", \"review\": {{\"heads\": [\"A\", \"B\"]}}}}'::jsonb), \
         ('{feed}', 'stg_two', 'SAIDAPET', 'up', 'head_differs', \
          '{{\"source\": \"backfill\"}}'::jsonb), \
         ('{feed}', 'stg_other', 'SAIDAPET', 'down', NULL, '{{\"source\": \"backfill\"}}'::jsonb)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, direction, position, stop_id, stop_type) VALUES \
         ('{feed}', 'stg_one', 'up', 1, 'A', 'NEW STOP'), \
         ('{feed}', 'stg_one', 'up', 2, 'C', 'INTERMEDIATE STOP'), \
         ('{feed}', 'stg_one', 'up', 3, 'D', 'INTERMEDIATE STOP'), \
         ('{feed}', 'stg_two', 'up', 1, 'B', 'NEW STOP'), \
         ('{feed}', 'stg_other', 'down', 1, 'E', 'NEW STOP')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stage (gtfs_id, route_id, position, stage_id, direction, stage_no) VALUES \
         ('{feed}', 'R1', 1, 'stg_one', 'up', 1), ('{feed}', 'R2', 1, 'stg_two', 'up', 1)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES \
         ('{feed}', 'R1', 1, 'A', 'NEW STOP', 1, 'SAIDAPET', '11'), \
         ('{feed}', 'R1', 2, 'C', 'INTERMEDIATE STOP', 1, 'SAIDAPET', '11'), \
         ('{feed}', 'R1', 3, 'D', 'INTERMEDIATE STOP', 1, 'SAIDAPET', '11'), \
         ('{feed}', 'R2', 1, 'B', 'NEW STOP', 1, 'SAIDAPET', '22')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage_review (gtfs_id, batch, name, name_key, direction, reason, impact, evidence) VALUES \
         ('{feed}', 'test batch', 'SAIDAPET', 'SAIDAPET', 'up', 'head_differs', 40, \
          '{{\"heads\": [\"A\", \"B\"], \"routes\": [\"R1\", \"R2\"], \"stages\": [\"stg_one\", \"stg_two\"]}}'::jsonb), \
         ('{feed}', 'test batch', 'KOYAMBEDU', 'KOYAMBEDU', 'up', 'stretch_differs', 4, \
          '{{\"stops\": [\"C\"], \"routes\": [\"R1\"]}}'::jsonb)"
    ));
    s.extend(reset_accounts(accounts));
    s
}

#[actix_web::test]
async fn the_team_works_through_the_stage_review_queue() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("stage-review-test-key");
    let (st, dir) = state(&pool, &signer, ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    let mut admin = Caller {
        signer: &signer,
        email: ADMIN.into(),
        session: None,
    };
    let mut ed = Caller {
        signer: &signer,
        email: EDITOR.into(),
        session: None,
    };
    let mut vw = Caller {
        signer: &signer,
        email: VIEWER.into(),
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
    for (email, role) in [(EDITOR, "editor"), (VIEWER, "viewer")] {
        let (s, b, _) = call!(
            &app,
            admin
                .req("POST", "/users")
                .set_json(json!({"email": email, "role": role}))
        );
        assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    }
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    for (email, role) in [(EDITOR, "editor"), (VIEWER, "viewer")] {
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
    for c in [&mut ed, &mut vw] {
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

    // the audit log is immutable, so its rows outlive clear_feed and a run counts
    // only what it adds itself
    let closes = || async {
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_audit_log WHERE gtfs_id = '{FEED}' \
                 AND action = 'stage_review_closed'"
            ),
        )
        .await
    };
    let closes_before = closes().await;

    // ---- the queue, as the team first sees it
    let (s, b, _) = call!(&app, vw.req("GET", &format!("/feeds/{FEED}/stage-reviews")));
    assert_eq!(s, 200, "{b}");
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{b}");
    // the reason that needs a decision about the name comes before the tidying
    assert_eq!(items[0]["reason"], "head_differs", "{b}");
    assert_eq!(items[0]["name"], "SAIDAPET", "{b}");
    assert_eq!(items[0]["direction"], "up", "{b}");
    assert_eq!(items[1]["reason"], "stretch_differs", "{b}");
    // the queue is ordered by how much the guess gets wrong
    assert_eq!(items[0]["impact"], 40, "{b}");
    assert!(
        items[0]["impact"].as_i64() > items[1]["impact"].as_i64(),
        "{b}"
    );

    let (s, b, _) = call!(
        &app,
        vw.req("GET", &format!("/feeds/{FEED}/stage-reviews/summary"))
    );
    assert_eq!(s, 200, "{b}");
    // the summary has two sides: names somebody has to settle, and names every
    // route agreed about, which only want looking over. Both count STAGE IDS,
    // not review rows, because that is what the list shows.
    assert_eq!(b["settle"]["pending"], 2, "{b}");
    assert_eq!(b["verify"]["pending"], 0, "{b}");
    assert_eq!(b["reason"]["head_differs"], 1, "{b}");
    assert_eq!(b["reason"]["stretch_differs"], 1, "{b}");
    assert_eq!(b["reason"]["agreed"], 0, "{b}");
    // only the one that changes enough stop calls is worth a look
    assert_eq!(b["settle"]["worth_a_look"]["names"], 1, "{b}");
    assert_eq!(b["settle"]["worth_a_look"]["small"], 1, "{b}");
    assert_eq!(b["settle"]["worth_a_look"]["threshold"], 20, "{b}");

    // filtering by reason, and by name
    let (s, b, _) = call!(
        &app,
        vw.req(
            "GET",
            &format!("/feeds/{FEED}/stage-reviews?reason=stretch_differs")
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["items"].as_array().unwrap().len(), 1, "{b}");
    let (s, b, _) = call!(
        &app,
        vw.req("GET", &format!("/feeds/{FEED}/stage-reviews?min_impact=20"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b["items"].as_array().unwrap().len(),
        1,
        "only the big one: {b}"
    );
    let (s, b, _) = call!(
        &app,
        vw.req("GET", &format!("/feeds/{FEED}/stage-reviews?q=saidap"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["items"][0]["name"], "SAIDAPET", "{b}");
    let (s, b, _) = call!(
        &app,
        vw.req(
            "GET",
            &format!("/feeds/{FEED}/stage-reviews?reason=nonsense")
        )
    );
    assert_eq!(s, 400, "{b}");
    assert_eq!(code_of(&b), "invalid_reason", "{b}");

    let id = items[0]["review_id"].as_i64().unwrap();

    // ---- the detail puts the disagreeing stages side by side, longest first,
    //      and only the stages of THIS direction
    let (s, b, _) = call!(&app, vw.req("GET", &format!("/stage-reviews/{id}")));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["stage_count"], 2, "{b}");
    let shown = b["stages"].as_array().unwrap();
    assert_eq!(
        shown[0]["stage_id"], "stg_one",
        "the longest list first: {b}"
    );
    assert_eq!(shown[0]["rows"].as_array().unwrap().len(), 3, "{b}");
    assert_eq!(shown[1]["stage_id"], "stg_two", "{b}");
    assert_eq!(shown[1]["rows"].as_array().unwrap().len(), 1, "{b}");
    // the stage carries the flag too, so it shows wherever the stage does
    assert_eq!(shown[0]["review"], "head_differs", "{b}");
    // the down stage of the same name is somebody else's problem
    assert!(
        !shown.iter().any(|x| x["stage_id"] == "stg_other"),
        "the other direction is not part of this review: {b}"
    );
    // and each side says which routes run it
    assert_eq!(shown[0]["routes"][0]["route_id"], "R1", "{b}");
    assert_eq!(shown[1]["routes"][0]["route_id"], "R2", "{b}");

    // ---- a viewer may look and not close
    let (s, b, _) = call!(
        &app,
        vw.req("POST", &format!("/stage-reviews/{id}/close"))
            .set_json(json!({"decision": "confirmed", "note": "they differ"}))
    );
    assert_eq!(s, 403, "a viewer cannot close a review: {b}");

    // ---- the rules of closing
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/stage-reviews/{id}/close"))
            .set_json(json!({"decision": "whatever"}))
    );
    assert_eq!(s, 400, "{b}");
    assert_eq!(code_of(&b), "invalid_decision", "{b}");
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/stage-reviews/{id}/close"))
            .set_json(json!({"decision": "confirmed"}))
    );
    assert_eq!(s, 400, "a confirmed review says why: {b}");
    assert_eq!(code_of(&b), "note_required", "{b}");
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/stage-reviews/{id}/close"))
            .set_json(
                json!({"decision": "fixed", "change_set": "00000000-0000-0000-0000-000000000000"})
            )
    );
    assert_eq!(s, 404, "{b}");
    assert_eq!(code_of(&b), "change_set_not_found", "{b}");

    // ---- fixed, naming the draft the renaming went into
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "Name the two SAIDAPET stages apart"}))
    );
    assert_eq!(s, 201, "{b}");
    let set = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/stage-reviews/{id}/close"))
            .set_json(json!({
                "decision": "fixed",
                "note": "stg_two renamed SAIDAPET (towards Guindy) in this draft",
                "change_set": set,
            }))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["status"], "fixed", "{b}");
    assert_eq!(b["change_set_id"], set, "{b}");
    assert_eq!(b["reviewed_by_email"], EDITOR, "{b}");
    assert!(b["reviewed_at"].is_string(), "{b}");
    // closing clears the flag on every stage of the name, in the same transaction
    for stage in ["stg_one", "stg_two"] {
        assert_eq!(
            scalar_text(
                &pool,
                format!("SELECT review FROM gtfs_stage WHERE gtfs_id = '{FEED}' AND stage_id = '{stage}'")
            )
            .await,
            None,
            "{stage} still flagged"
        );
    }
    // and it is audited
    assert_eq!(closes().await, closes_before + 1, "the close is audited");

    // a closed review is not closed twice
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/stage-reviews/{id}/close"))
            .set_json(json!({"decision": "confirmed", "note": "again"}))
    );
    assert_eq!(s, 409, "{b}");
    assert_eq!(code_of(&b), "review_not_pending", "{b}");

    // it has left the pending list, and is on the fixed one
    let (_, b, _) = call!(&app, vw.req("GET", &format!("/feeds/{FEED}/stage-reviews")));
    assert_eq!(b["items"].as_array().unwrap().len(), 1, "{b}");
    let (_, b, _) = call!(
        &app,
        vw.req("GET", &format!("/feeds/{FEED}/stage-reviews?status=fixed"))
    );
    assert_eq!(b["items"][0]["review_id"], id, "{b}");
    assert_eq!(
        b["items"][0]["change_set_title"], "Name the two SAIDAPET stages apart",
        "{b}"
    );

    // ---- reopening puts the flag back
    let (s, b, _) = call!(&app, ed.req("POST", &format!("/stage-reviews/{id}/reopen")));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["status"], "pending", "{b}");
    assert_eq!(b["change_set_id"], Value::Null, "{b}");
    assert_eq!(b["review_note"], Value::Null, "{b}");
    assert_eq!(
        scalar_text(
            &pool,
            format!(
                "SELECT review FROM gtfs_stage WHERE gtfs_id = '{FEED}' AND stage_id = 'stg_one'"
            )
        )
        .await
        .as_deref(),
        Some("head_differs"),
        "reopening puts the flag back"
    );
    let (s, b, _) = call!(&app, ed.req("POST", &format!("/stage-reviews/{id}/reopen")));
    assert_eq!(s, 409, "{b}");
    assert_eq!(code_of(&b), "review_pending", "{b}");

    // ---- confirmed: the routes really do differ, and the flag still clears
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/stage-reviews/{id}/close"))
            .set_json(json!({
                "decision": "confirmed",
                "note": "checked on the map: R2 turns off before C, so these are two stages",
            }))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["status"], "confirmed", "{b}");
    assert_eq!(b["change_set_id"], Value::Null, "{b}");
    assert_eq!(
        scalar_text(
            &pool,
            format!(
                "SELECT review FROM gtfs_stage WHERE gtfs_id = '{FEED}' AND stage_id = 'stg_two'"
            )
        )
        .await,
        None
    );

    // ---- the stage list can be filtered by what is left to look at
    let (s, b, _) = call!(
        &app,
        vw.req("GET", &format!("/feeds/{FEED}/stages?review=any"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        b["items"].as_array().unwrap().len(),
        0,
        "nothing flagged once the review is closed: {b}"
    );
    let (_, b, _) = call!(
        &app,
        vw.req("GET", &format!("/feeds/{FEED}/stages?review=none"))
    );
    assert_eq!(b["items"].as_array().unwrap().len(), 3, "{b}");

    // ---- merging: only ever within one direction
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": "Merge the SAIDAPET stages"}))
    );
    assert_eq!(s, 201, "{b}");
    let mset = b["change_set_id"].as_str().unwrap().to_string();
    // the up stage cannot be merged into the down one: they hold different stops
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{mset}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "merge", "entity_key": "stg_two|up",
                "after": {"into_stage_id": "stg_other|down"}
            }))
    );
    assert!(s >= 400, "merging across directions is refused: {s} {b}");
    assert!(
        format!("{b}").contains("merge_across_directions"),
        "and says why: {b}"
    );
    // nor into itself
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{mset}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "merge", "entity_key": "stg_two|up",
                "after": {"into_stage_id": "stg_two|up"}
            }))
    );
    assert!(
        s >= 400 && format!("{b}").contains("merge_same_stage"),
        "{s} {b}"
    );
    // the same way round is allowed, and moves the routes over
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{mset}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "merge", "entity_key": "stg_two|up",
                "after": {"into_stage_id": "stg_one|up"}
            }))
    );
    assert_eq!(s, 201, "merging one up stage into another: {b}");
    call!(
        &app,
        ed.req("POST", &format!("/change-sets/{mset}/discard"))
    );

    // ---- the mapping every reader uses IS the stages, row for row
    // gtfs_route_stop is what the GTFS export and the in-memory loader read;
    // gtfs_route_stop_from_stages is the same rows derived from route_stage ->
    // stage -> stage_stop. They are kept identical, and that is what lets a
    // stage edit rewrite a route at all.
    assert_eq!(
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM ( \
                   SELECT route_id, sequence, stop_id, stop_type, stage_no, stage_name, \
                          stop_name_override, marker_id, marker_name, marker_lat, marker_lon \
                     FROM gtfs_route_stop_from_stages WHERE gtfs_id = '{FEED}' \
                   EXCEPT ALL \
                   SELECT route_id, sequence, stop_id, stop_type, stage_no, stage_name, \
                          stop_name_override, marker_id, marker_name, marker_lat, marker_lon \
                     FROM gtfs_route_stop_effective WHERE gtfs_id = '{FEED}' AND pattern_key = 1) d"
            )
        )
        .await,
        0,
        "the stages and the rows every reader uses have come apart"
    );

    // ---- a review on another feed is not reachable through this one
    let (s, b, _) = call!(&app, vw.req("GET", "/stage-reviews/999999999"));
    assert_eq!(s, 404, "{b}");
    assert_eq!(code_of(&b), "review_not_found", "{b}");

    call!(&app, ed.req("POST", &format!("/change-sets/{set}/discard")));
    exec(&pool, &clear_feed(FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}

// ---------------------------------------------------------------- fixing one in a draft

const DRAFT_FEED: &str = "editor_stage_review_draft_feed";
const DRAFT_ADMIN: &str = "admin@editor-stage-review-draft-test.invalid";
const DRAFT_EDITOR: &str = "editor@editor-stage-review-draft-test.invalid";

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

/// The order the review page asks for: fix the stage in a draft, then Mark
/// fixed naming that draft. Closing is not an edit of the stage, so the draft
/// it names still submits. And a stage the draft makes - by a create or a
/// split - can be edited again in that draft by the key every page uses for a
/// stage, `id|direction`.
#[actix_web::test]
async fn a_stage_fixed_in_a_draft_and_marked_fixed_still_submits() {
    let Some(pool) = local_pool().await else {
        return;
    };
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT count(*) FROM pg_trigger WHERE tgname = 'gtfs_stage_touch' \
             AND tgfoid = 'gtfs_stage_touch_row'::regproc"
        )
        .await,
        1,
        "apply db/gtfs_editor/0029_stage_review_flag_keeps_version.sql"
    );
    exec(&pool, &seed_for(DRAFT_FEED, &[DRAFT_ADMIN, DRAFT_EDITOR])).await;
    let signer = TestSigner::generate("stage-review-draft-test-key");
    let (st, dir) = state(&pool, &signer, DRAFT_ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;
    let mut admin = Caller {
        signer: &signer,
        email: DRAFT_ADMIN.into(),
        session: None,
    };
    let mut ed = Caller {
        signer: &signer,
        email: DRAFT_EDITOR.into(),
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
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/users")
            .set_json(json!({"email": DRAFT_EDITOR, "role": "editor"}))
    );
    assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    let uid = users["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == DRAFT_EDITOR)
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, b, _) = call!(
        &app,
        admin
            .req("PATCH", &format!("/users/{uid}"))
            .set_json(json!({"role": "editor", "status": "active"}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        admin
            .req("PUT", &format!("/users/{uid}/feeds/{DRAFT_FEED}"))
            .set_json(json!({"role": "editor"}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(&app, ed.req("POST", "/auth/totp/enroll"));
    assert_eq!(s, 200, "{b}");
    let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let (s, b, cookie) = call!(
        &app,
        ed.req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now())}))
    );
    assert_eq!(s, 200, "{b}");
    ed.session = cookie;

    let version = || async {
        scalar_i64(
            &pool,
            format!(
                "SELECT row_version::bigint FROM gtfs_stage \
                 WHERE gtfs_id = '{DRAFT_FEED}' AND stage_id = 'stg_one' AND direction = 'up'"
            ),
        )
        .await
    };
    let (_, b, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{DRAFT_FEED}/stage-reviews"))
    );
    let id = b["items"][0]["review_id"].as_i64().unwrap();
    assert_eq!(b["items"][0]["name"], "SAIDAPET", "{b}");

    // ---- the stage's stops fixed in a draft, from the review
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/feeds/{DRAFT_FEED}/change-sets"))
            .set_json(json!({"title": "SAIDAPET ends at C"}))
    );
    assert_eq!(s, 201, "{b}");
    let set = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "update", "entity_key": "stg_one|up",
                "after": {"name": "SAIDAPET", "direction": "up", "rows": [
                    {"stop_id": "A", "stop_type": "NEW STOP"},
                    {"stop_id": "C", "stop_type": "INTERMEDIATE STOP"},
                ]}
            }))
    );
    assert_eq!(s, 201, "{b}");
    assert!(errors(&b).is_empty(), "{b}");

    // ---- then marked fixed, naming it: the flag comes off, the version stays
    let before = version().await;
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/stage-reviews/{id}/close"))
            .set_json(json!({"decision": "fixed", "change_set": set}))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        scalar_text(
            &pool,
            format!(
                "SELECT review FROM gtfs_stage WHERE gtfs_id = '{DRAFT_FEED}' AND stage_id = 'stg_one'"
            )
        )
        .await,
        None
    );
    assert_eq!(
        version().await,
        before,
        "closing a review is not an edit of the stage"
    );
    // reopening is not either
    let (s, b, _) = call!(&app, ed.req("POST", &format!("/stage-reviews/{id}/reopen")));
    assert_eq!(s, 200, "{b}");
    assert_eq!(version().await, before);
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/stage-reviews/{id}/close"))
            .set_json(json!({"decision": "fixed", "change_set": set}))
    );
    assert_eq!(s, 200, "{b}");

    // ---- a stage made in this draft is edited by the key every page uses
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "create", "entity_key": "",
                "after": {"name": "GUINDY", "direction": "down", "rows": [
                    {"stop_id": "E", "stop_type": "NEW STOP"},
                ]}
            }))
    );
    assert_eq!(s, 201, "{b}");
    let made = b["changes"].as_array().unwrap().last().unwrap()["entity_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        !made.contains('|'),
        "a create names its stage by id: {made}"
    );
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "update", "entity_key": format!("{made}|down"),
                "after": {"name": "GUINDY", "direction": "down", "rows": [
                    {"stop_id": "E", "stop_type": "NEW STOP"},
                    {"stop_id": "D", "stop_type": "INTERMEDIATE STOP"},
                ]}
            }))
    );
    assert_eq!(s, 201, "editing a stage this draft creates: {b}");
    assert!(errors(&b).is_empty(), "{b}");
    // the other direction is not the stage the draft made
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "update", "entity_key": format!("{made}|up"),
                "after": {"name": "GUINDY"}
            }))
    );
    assert_eq!(s, 404, "{b}");
    assert_eq!(code_of(&b), "entity_not_found", "{b}");
    let (s, p, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/change-sets/{set}/preview/stages/{made}%7Cdown")
        )
    );
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["rows"].as_array().unwrap().len(), 2, "{p}");

    // ---- and so is one a split makes, routes and all
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "split", "entity_key": "stg_split",
                "after": {"stage_id": "stg_split", "name": "SAIDAPET (to B)", "direction": "up",
                    "from_stage_id": "stg_two|up", "routes": ["R2"],
                    "rows": [{"stop_id": "B", "stop_type": "NEW STOP"}]}
            }))
    );
    assert_eq!(s, 201, "{b}");
    assert!(errors(&b).is_empty(), "{b}");
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "update", "entity_key": "stg_split|up",
                "after": {"name": "SAIDAPET (towards B)", "direction": "up"}
            }))
    );
    assert_eq!(s, 201, "editing a stage this draft splits off: {b}");
    assert!(errors(&b).is_empty(), "{b}");
    let (s, p, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/change-sets/{set}/preview/stages/stg_split%7Cup")
        )
    );
    assert_eq!(s, 200, "{p}");
    assert_eq!(p["name"], "SAIDAPET (towards B)", "{p}");
    assert_eq!(p["routes"][0]["route_id"], "R2", "{p}");

    // ---- none of it conflicts with itself
    let (s, b, _) = call!(&app, ed.req("POST", &format!("/change-sets/{set}/submit")));
    assert_eq!(s, 200, "submit: {b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set}/approve"))
    );
    assert_eq!(s, 200, "approve: {b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set}/commit"))
    );
    assert_eq!(s, 200, "commit: {b}");
    assert_eq!(
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_stage_stop WHERE gtfs_id = '{DRAFT_FEED}' \
                 AND stage_id = 'stg_one' AND direction = 'up'"
            )
        )
        .await,
        2,
        "the fix is live"
    );
    assert_eq!(
        scalar_text(
            &pool,
            format!(
                "SELECT stage_id FROM gtfs_route_stage WHERE gtfs_id = '{DRAFT_FEED}' AND route_id = 'R2'"
            )
        )
        .await
        .as_deref(),
        Some("stg_split")
    );

    // ---- a temporary route R1 is not running still names its stages: stg_two,
    //      which no running list uses any more, is not deleted from under it,
    //      nor merged into stg_one, which that temporary route runs as well
    exec(
        &pool,
        &[format!(
            "INSERT INTO gtfs_route_stage (gtfs_id, route_id, variant_id, position, stage_id, direction, stage_no) VALUES \
             ('{DRAFT_FEED}', 'R1', 'r1_detour', 1, 'stg_one', 'up', 1), \
             ('{DRAFT_FEED}', 'R1', 'r1_detour', 2, 'stg_two', 'up', 2)"
        )],
    )
    .await;
    for (change, code) in [
        (
            json!({"entity": "stage", "op": "delete", "entity_key": "stg_two|up", "after": null}),
            "stage_in_use",
        ),
        (
            json!({"entity": "stage", "op": "merge", "entity_key": "stg_two|up",
                   "after": {"into_stage_id": "stg_one|up"}}),
            "stage_repeated",
        ),
    ] {
        let (s, b, _) = call!(
            &app,
            ed.req("POST", &format!("/feeds/{DRAFT_FEED}/change-sets"))
                .set_json(json!({"title": format!("tidy SAIDAPET: {code}")}))
        );
        assert_eq!(s, 201, "{b}");
        let tidy = b["change_set_id"].as_str().unwrap().to_string();
        let (s, b, _) = call!(
            &app,
            ed.req("POST", &format!("/change-sets/{tidy}/changes"))
                .set_json(change)
        );
        assert!(format!("{b}").contains(code), "{code}: {s} {b}");
        assert!(
            format!("{b}").contains("1 (R1)"),
            "and names the route: {b}"
        );
        call!(
            &app,
            ed.req("POST", &format!("/change-sets/{tidy}/discard"))
        );
    }

    // ---- a real edit still moves the version, as a stop merge's touch does
    let before = version().await;
    exec(
        &pool,
        &[format!(
            "UPDATE gtfs_stage SET updated_by = 'stop merge' \
             WHERE gtfs_id = '{DRAFT_FEED}' AND stage_id = 'stg_one' AND direction = 'up'"
        )],
    )
    .await;
    assert_eq!(version().await, before + 1);

    exec(&pool, &clear_feed(DRAFT_FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}

const REPLACE_FEED: &str = "editor_stage_review_replace_feed";
const REPLACE_ADMIN: &str = "admin@editor-stage-review-replace-test.invalid";
const REPLACE_EDITOR: &str = "editor@editor-stage-review-replace-test.invalid";

/// A stage that is wrong altogether, replaced by one made in the draft: the
/// review page offers the stages that may be the same place (by name, a name
/// written another way, or where they start), the stage made in the draft
/// takes the wrong one's routes by a merge, and the draft still submits. The
/// review names its routes by number, and a route to review names the one MTC
/// stage its stage duplicates.
#[actix_web::test]
async fn a_wrong_stage_is_replaced_by_one_the_draft_makes() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let mut seed = seed_for(REPLACE_FEED, &[REPLACE_ADMIN, REPLACE_EDITOR]);
    seed.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
         ('{REPLACE_FEED}', 'Z', 'Z', 'STOP Z', 14.5, 80.9)"
    ));
    // SAIDAPET B.T: the same place written another way, starting at C; GUINDY
    // is far away and named nothing like it
    seed.push(format!(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, name, direction, provenance) VALUES \
         ('{REPLACE_FEED}', 'stg_bt', 'SAIDAPET B.T', 'up', '{{}}'::jsonb), \
         ('{REPLACE_FEED}', 'stg_far', 'GUINDY', 'up', '{{}}'::jsonb)"
    ));
    seed.push(format!(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, direction, position, stop_id, stop_type) VALUES \
         ('{REPLACE_FEED}', 'stg_bt', 'up', 1, 'C', 'NEW STOP'), \
         ('{REPLACE_FEED}', 'stg_far', 'up', 1, 'Z', 'NEW STOP')"
    ));
    // a temporary route of R1 runs both SAIDAPET stages
    seed.push(format!(
        "INSERT INTO gtfs_route_stage (gtfs_id, route_id, variant_id, position, stage_id, direction, stage_no) VALUES \
         ('{REPLACE_FEED}', 'R1', 'r1_detour', 1, 'stg_one', 'up', 1), \
         ('{REPLACE_FEED}', 'R1', 'r1_detour', 2, 'stg_two', 'up', 2)"
    ));
    seed.push(format!(
        "INSERT INTO gtfs_route_stage_issue (gtfs_id, batch, route_id, short_name, issue) VALUES \
         ('{REPLACE_FEED}', 'test batch', 'R1', '1', 'count_differs')"
    ));
    exec(&pool, &seed).await;
    let signer = TestSigner::generate("stage-review-replace-test-key");
    let (st, dir) = state(&pool, &signer, REPLACE_ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;
    let mut admin = Caller {
        signer: &signer,
        email: REPLACE_ADMIN.into(),
        session: None,
    };
    let mut ed = Caller {
        signer: &signer,
        email: REPLACE_EDITOR.into(),
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
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/users")
            .set_json(json!({"email": REPLACE_EDITOR, "role": "editor"}))
    );
    assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    let uid = users["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == REPLACE_EDITOR)
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, b, _) = call!(
        &app,
        admin
            .req("PATCH", &format!("/users/{uid}"))
            .set_json(json!({"role": "editor", "status": "active"}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        admin
            .req("PUT", &format!("/users/{uid}/feeds/{REPLACE_FEED}"))
            .set_json(json!({"role": "editor"}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(&app, ed.req("POST", "/auth/totp/enroll"));
    assert_eq!(s, 200, "{b}");
    let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let (s, b, cookie) = call!(
        &app,
        ed.req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now())}))
    );
    assert_eq!(s, 200, "{b}");
    ed.session = cookie;

    // ---- the review names its routes by number as well as id
    let (_, b, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/feeds/{REPLACE_FEED}/stage-reviews?q=SAIDAPET")
        )
    );
    let id = b["items"][0]["review_id"].as_i64().unwrap();
    let (s, r, _) = call!(&app, ed.req("GET", &format!("/stage-reviews/{id}")));
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["route_names"], json!({"R1": "1", "R2": "2"}), "{r}");

    // ---- the stages that may be the same place: the same name first, then
    //      the name written another way; not the other direction, not GUINDY
    let (s, b, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/feeds/{REPLACE_FEED}/stages/stg_one%7Cup/similar")
        )
    );
    assert_eq!(s, 200, "{b}");
    let keys: Vec<&str> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["stage_key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["stg_two|up", "stg_bt|up"], "{b}");
    assert_eq!(b["items"][0]["why"], "same_name", "{b}");
    assert_eq!(b["items"][1]["why"], "like_name", "{b}");
    assert!(b["items"][1]["distance_m"].as_i64().unwrap() < 500, "{b}");
    // R1's temporary route runs both, so neither merge between them is possible
    assert_eq!(
        b["items"][0]["shared_routes"],
        json!([{"route_id": "R1", "short_name": "1"}]),
        "{b}"
    );
    assert_eq!(b["items"][1]["shared_routes"], json!([]), "{b}");

    // ---- the right stage is made in a draft, and takes the wrong one's place
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/feeds/{REPLACE_FEED}/change-sets"))
            .set_json(json!({"title": "SAIDAPET is A to D"}))
    );
    assert_eq!(s, 201, "{b}");
    let set = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "create", "entity_key": "",
                "after": {"name": "SAIDAPET", "direction": "up", "rows": [
                    {"stop_id": "A", "stop_type": "NEW STOP"},
                    {"stop_id": "D", "stop_type": "INTERMEDIATE STOP"},
                ]}
            }))
    );
    assert_eq!(s, 201, "{b}");
    let made = b["changes"].as_array().unwrap().last().unwrap()["entity_key"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "merge", "entity_key": "stg_one|up",
                "after": {"into_stage_id": format!("{made}|up")}
            }))
    );
    assert_eq!(s, 201, "replacing a stage by one the draft makes: {b}");
    assert!(errors(&b).is_empty(), "{b}");
    let (s, p, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/change-sets/{set}/preview/stages/{made}%7Cup")
        )
    );
    assert_eq!(s, 200, "{p}");
    let runs: Vec<&str> = p["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["route_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        runs,
        vec!["R1", "R1"],
        "its normal and temporary route: {p}"
    );
    // and a stage the draft makes has stages like it too; the one it replaced
    // is gone from them
    let (s, b, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/change-sets/{set}/preview/stages/{made}%7Cup/similar")
        )
    );
    assert_eq!(s, 200, "{b}");
    let keys: Vec<&str> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["stage_key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, vec!["stg_two|up", "stg_bt|up"], "{b}");

    // ---- marked fixed naming it, and it goes live
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/stage-reviews/{id}/close"))
            .set_json(json!({"decision": "fixed", "change_set": set}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(&app, ed.req("POST", &format!("/change-sets/{set}/submit")));
    assert_eq!(s, 200, "submit: {b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set}/approve"))
    );
    assert_eq!(s, 200, "approve: {b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set}/commit"))
    );
    assert_eq!(s, 200, "commit: {b}");
    assert_eq!(
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_route_stage WHERE gtfs_id = '{REPLACE_FEED}' \
                 AND route_id = 'R1' AND stage_id = '{made}'"
            )
        )
        .await,
        2,
        "R1 runs the new stage, normally and on its temporary route"
    );
    assert_eq!(
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_stage WHERE gtfs_id = '{REPLACE_FEED}' \
                 AND stage_id = 'stg_one' AND deleted"
            )
        )
        .await,
        1,
        "the wrong stage is gone"
    );

    // ---- a route to review names the one MTC stage its stage duplicates
    let (_, b, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{REPLACE_FEED}/route-issues"))
    );
    let issue = b["items"][0]["issue_id"].as_i64().unwrap();
    let (s, b, _) = call!(&app, ed.req("GET", &format!("/route-issues/{issue}")));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["stages"][0]["stage_id"], made.as_str(), "{b}");
    assert_eq!(b["stages"][0]["twin"], true, "{b}");
    assert_eq!(b["stages"][0]["twin_key"], "stg_two|up", "{b}");
    assert_eq!(b["stages"][0]["twin_far"], false, "{b}");
    // R1's temporary route runs both, so the merge would be refused
    assert_eq!(b["stages"][0]["twin_shared"], json!(["1"]), "{b}");

    exec(&pool, &clear_feed(REPLACE_FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}

const CHECK_FEED: &str = "editor_stage_review_check_feed";
const CHECK_ADMIN: &str = "admin@editor-stage-review-check-test.invalid";
const CHECK_EDITOR: &str = "editor@editor-stage-review-check-test.invalid";

fn keys_of(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| i["stage_key"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// What the dashboard asks before a merge or a replacement goes into a draft,
/// and what it is told around one:
///
///   - the stages like a stage put a same-named one across the city last, and
///     say it is far; they mention the other direction's stage and a stage
///     being made in somebody else's draft without offering either;
///   - the merge check says which stops the routes stop calling at, how far
///     apart the two start, the open reviews it touches, and refuses what the
///     draft would refuse;
///   - a replacement can be replaced again in the same draft, and the routes
///     end on the last stage;
///   - a review closed as fixed naming a draft that is then discarded is found
///     as such;
///   - every stage named after itself with one MTC stage of its name is listed
///     for replacing in one go.
#[actix_web::test]
async fn the_page_is_told_what_a_merge_would_do_before_it_is_made() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let mut seed = seed_for(CHECK_FEED, &[CHECK_ADMIN, CHECK_EDITOR]);
    seed.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) VALUES \
         ('{CHECK_FEED}', 'Z', 'Z', 'STOP Z', 14.5, 80.9)"
    ));
    // SAIDAPET B.T at C; a SAIDAPET across the city at Z; two stages named
    // after themselves, one with a single MTC twin and one with several
    seed.push(format!(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, name, direction, provenance) VALUES \
         ('{CHECK_FEED}', 'stg_bt', 'SAIDAPET B.T', 'up', '{{}}'::jsonb), \
         ('{CHECK_FEED}', 'stg_far_same', 'SAIDAPET', 'up', '{{}}'::jsonb), \
         ('{CHECK_FEED}', 'nm_SAIDAPET_BT', 'Saidapet B.T', 'up', '{{}}'::jsonb), \
         ('{CHECK_FEED}', 'nm_SAIDAPET', 'Saidapet', 'up', '{{}}'::jsonb)"
    ));
    seed.push(format!(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, direction, position, stop_id, stop_type) VALUES \
         ('{CHECK_FEED}', 'stg_bt', 'up', 1, 'C', 'NEW STOP'), \
         ('{CHECK_FEED}', 'stg_bt', 'up', 2, 'D', 'INTERMEDIATE STOP'), \
         ('{CHECK_FEED}', 'stg_far_same', 'up', 1, 'Z', 'NEW STOP'), \
         ('{CHECK_FEED}', 'nm_SAIDAPET_BT', 'up', 1, 'C', 'NEW STOP'), \
         ('{CHECK_FEED}', 'nm_SAIDAPET', 'up', 1, 'A', 'NEW STOP')"
    ));
    // R1's temporary route runs both SAIDAPET stages
    seed.push(format!(
        "INSERT INTO gtfs_route_stage (gtfs_id, route_id, variant_id, position, stage_id, direction, stage_no) VALUES \
         ('{CHECK_FEED}', 'R1', 'r1_detour', 1, 'stg_one', 'up', 1), \
         ('{CHECK_FEED}', 'R1', 'r1_detour', 2, 'stg_two', 'up', 2)"
    ));
    // a review of stg_one as the mapper writes one now, by the stage's id
    seed.push(format!(
        "INSERT INTO gtfs_stage_review (gtfs_id, batch, name, name_key, direction, reason, impact, evidence) VALUES \
         ('{CHECK_FEED}', 'test batch', 'SAIDAPET', 'stg_one', 'up', 'head_differs', 12, '{{}}'::jsonb)"
    ));
    seed.push(format!(
        "INSERT INTO gtfs_route_stage_issue (gtfs_id, batch, route_id, short_name, issue) VALUES \
         ('{CHECK_FEED}', 'test batch', 'R1', '1', 'count_differs')"
    ));
    exec(&pool, &seed).await;
    let signer = TestSigner::generate("stage-review-check-test-key");
    let (st, dir) = state(&pool, &signer, CHECK_ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;
    let mut admin = Caller {
        signer: &signer,
        email: CHECK_ADMIN.into(),
        session: None,
    };
    let mut ed = Caller {
        signer: &signer,
        email: CHECK_EDITOR.into(),
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
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/users")
            .set_json(json!({"email": CHECK_EDITOR, "role": "editor"}))
    );
    assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    let uid = users["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == CHECK_EDITOR)
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, b, _) = call!(
        &app,
        admin
            .req("PATCH", &format!("/users/{uid}"))
            .set_json(json!({"role": "editor", "status": "active"}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        admin
            .req("PUT", &format!("/users/{uid}/feeds/{CHECK_FEED}"))
            .set_json(json!({"role": "editor"}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(&app, ed.req("POST", "/auth/totp/enroll"));
    assert_eq!(s, 200, "{b}");
    let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let (s, b, cookie) = call!(
        &app,
        ed.req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now())}))
    );
    assert_eq!(s, 200, "{b}");
    ed.session = cookie;

    // ---- somebody else is making a SAIDAPET in a draft of their own
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/feeds/{CHECK_FEED}/change-sets"))
            .set_json(json!({"title": "Another SAIDAPET"}))
    );
    assert_eq!(s, 201, "{b}");
    let theirs = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", &format!("/change-sets/{theirs}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "create", "entity_key": "",
                "after": {"name": "SAIDAPET", "direction": "up", "rows": [
                    {"stop_id": "A", "stop_type": "NEW STOP"},
                ]}
            }))
    );
    assert_eq!(s, 201, "{b}");

    // ---- the stages like stg_one: the same name close by first, then names
    //      written another way, and the SAIDAPET across the city last
    let (s, b, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/feeds/{CHECK_FEED}/stages/stg_one%7Cup/similar")
        )
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(
        keys_of(&b["items"]),
        vec![
            "nm_SAIDAPET|up",
            "stg_two|up",
            "nm_SAIDAPET_BT|up",
            "stg_bt|up",
            "stg_far_same|up"
        ],
        "{b}"
    );
    let far = &b["items"][4];
    assert_eq!(far["why"], "same_name", "{b}");
    assert_eq!(far["far"], true, "{b}");
    assert!(far["distance_m"].as_i64().unwrap() > 100_000, "{b}");
    assert_eq!(b["items"][1]["far"], false, "{b}");
    // the other way is mentioned, never offered
    assert_eq!(keys_of(&b["other_way"]), vec!["stg_other|down"], "{b}");
    // and so is the stage in the admin's draft, with the draft it is in
    let elsewhere = b["in_other_drafts"].as_array().unwrap();
    assert_eq!(elsewhere.len(), 1, "{b}");
    assert_eq!(elsewhere[0]["name"], "SAIDAPET", "{b}");
    assert_eq!(elsewhere[0]["change_set_id"], theirs.as_str(), "{b}");
    assert_eq!(elsewhere[0]["change_set_status"], "draft", "{b}");
    assert_eq!(elsewhere[0]["author_email"], CHECK_ADMIN, "{b}");

    // ---- the editor's draft makes the right stage
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/feeds/{CHECK_FEED}/change-sets"))
            .set_json(json!({"title": "SAIDAPET is A to D"}))
    );
    assert_eq!(s, 201, "{b}");
    let set = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "create", "entity_key": "",
                "after": {"name": "SAIDAPET", "direction": "up", "rows": [
                    {"stop_id": "A", "stop_type": "NEW STOP"},
                    {"stop_id": "D", "stop_type": "INTERMEDIATE STOP"},
                ]}
            }))
    );
    assert_eq!(s, 201, "{b}");
    let made = b["changes"].as_array().unwrap().last().unwrap()["entity_key"]
        .as_str()
        .unwrap()
        .to_string();
    // its own draft's stage is not somebody else's
    let (s, b, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/change-sets/{set}/preview/stages/stg_one%7Cup/similar")
        )
    );
    assert_eq!(s, 200, "{b}");
    assert!(keys_of(&b["items"]).contains(&format!("{made}|up")), "{b}");
    assert_eq!(b["in_other_drafts"].as_array().unwrap().len(), 1, "{b}");
    assert_eq!(
        b["in_other_drafts"][0]["change_set_id"],
        theirs.as_str(),
        "{b}"
    );

    // ---- asked before it is drafted: replacing stg_one with the made stage
    let (s, m, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/change-sets/{set}/preview/stages/stg_one%7Cup/merge?into={made}%7Cup")
        )
    );
    assert_eq!(s, 200, "{m}");
    assert!(
        m["problems"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["level"] != "error"),
        "{m}"
    );
    assert_eq!(m["stage"]["stage_key"], "stg_one|up", "{m}");
    assert_eq!(m["into"]["stage_key"], format!("{made}|up"), "{m}");
    assert_eq!(
        m["stops_lost"],
        json!([{"stop_id": "C", "name": "STOP C"}]),
        "{m}"
    );
    assert_eq!(m["stops_gained"], json!([]), "{m}");
    assert_eq!(m["stops_kept"], 2, "{m}");
    assert_eq!(m["distance_m"], 0, "{m}");
    assert_eq!(m["far"], false, "{m}");
    assert_eq!(
        m["routes"],
        json!([{"route_id": "R1", "short_name": "1", "normal": true, "temporary": ["r1_detour"]}]),
        "{m}"
    );
    assert_eq!(
        keys_of(&m["reviews"]["stage_reviews"]),
        vec!["stg_one|up"],
        "{m}"
    );
    assert_eq!(m["reviews"]["route_issues"][0]["route_id"], "R1", "{m}");
    // nothing went into the draft by asking
    let (_, d, _) = call!(&app, ed.req("GET", &format!("/change-sets/{set}")));
    assert_eq!(d["changes"].as_array().unwrap().len(), 1, "{d}");

    // a route running both is refused before anything is drafted
    let (s, m, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/change-sets/{set}/preview/stages/stg_one%7Cup/merge?into=stg_two%7Cup")
        )
    );
    assert_eq!(s, 200, "{m}");
    assert!(
        m["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["level"] == "error" && p["code"] == "stage_repeated"),
        "{m}"
    );
    assert_eq!(
        m["shared_routes"],
        json!([{"route_id": "R1", "short_name": "1"}]),
        "{m}"
    );
    // and the same name across the city is said to be far
    let (s, m, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/change-sets/{set}/preview/stages/stg_one%7Cup/merge?into=stg_far_same%7Cup")
        )
    );
    assert_eq!(s, 200, "{m}");
    assert_eq!(m["far"], true, "{m}");
    assert_eq!(
        m["stops_gained"],
        json!([{"stop_id": "Z", "name": "STOP Z"}]),
        "{m}"
    );

    // ---- the open reviews a stage touches, read on their own
    let (s, o, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/feeds/{CHECK_FEED}/open-reviews?stages=stg_one%7Cup,stg_bt%7Cup")
        )
    );
    assert_eq!(s, 200, "{o}");
    assert_eq!(keys_of(&o["stage_reviews"]), vec!["stg_one|up"], "{o}");
    assert_eq!(o["route_issues"][0]["route_id"], "R1", "{o}");

    // ---- replaced, and the replacement replaced again in the same draft:
    //      the routes end on the last one
    for (gone, into) in [
        ("stg_one|up".to_string(), format!("{made}|up")),
        (format!("{made}|up"), "stg_bt|up".to_string()),
    ] {
        let (s, b, _) = call!(
            &app,
            ed.req("POST", &format!("/change-sets/{set}/changes"))
                .set_json(json!({
                    "entity": "stage", "op": "merge", "entity_key": gone,
                    "after": {"into_stage_id": into}
                }))
        );
        assert_eq!(s, 201, "{gone} into {into}: {b}");
        assert!(errors(&b).is_empty(), "{gone} into {into}: {b}");
    }
    let (s, b, _) = call!(&app, ed.req("POST", &format!("/change-sets/{set}/submit")));
    assert_eq!(s, 200, "submit: {b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set}/approve"))
    );
    assert_eq!(s, 200, "approve: {b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set}/commit"))
    );
    assert_eq!(s, 200, "commit: {b}");
    assert_eq!(
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_route_stage WHERE gtfs_id = '{CHECK_FEED}' \
                 AND route_id = 'R1' AND stage_id = 'stg_bt'"
            )
        )
        .await,
        2,
        "R1 runs the last stage of the chain, normally and on its temporary route"
    );
    assert_eq!(
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_stage WHERE gtfs_id = '{CHECK_FEED}' \
                 AND stage_id IN ('stg_one', '{made}') AND deleted"
            )
        )
        .await,
        2,
        "both stages on the way are gone"
    );

    // ---- fixed with a draft that is then thrown away: found as such
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/feeds/{CHECK_FEED}/change-sets"))
            .set_json(json!({"title": "Will not happen"}))
    );
    assert_eq!(s, 201, "{b}");
    let lost = b["change_set_id"].as_str().unwrap().to_string();
    let (_, b, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/feeds/{CHECK_FEED}/stage-reviews?q=KOYAMBEDU")
        )
    );
    let koyambedu = b["items"][0]["review_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/stage-reviews/{koyambedu}/close"))
            .set_json(json!({"decision": "fixed", "change_set": lost}))
    );
    assert_eq!(s, 200, "{b}");
    let (_, b, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{CHECK_FEED}/route-issues"))
    );
    let issue = b["items"][0]["issue_id"].as_i64().unwrap();
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/route-issues/{issue}/close"))
            .set_json(json!({"decision": "fixed", "change_set": lost}))
    );
    assert_eq!(s, 200, "{b}");
    let (_, b, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/feeds/{CHECK_FEED}/stage-reviews?status=fixed&lost=true&group=stage")
        )
    );
    assert_eq!(b["items"].as_array().unwrap().len(), 0, "not lost yet: {b}");
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{lost}/discard"))
    );
    assert_eq!(s, 200, "discard: {b}");
    let (_, b, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/feeds/{CHECK_FEED}/stage-reviews?status=fixed&lost=true&group=stage")
        )
    );
    let items = b["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{b}");
    assert_eq!(items[0]["stage_id"], "KOYAMBEDU", "{b}");
    assert_eq!(items[0]["lost"], 1, "{b}");
    assert_eq!(items[0]["review_id"], koyambedu, "{b}");
    let (_, b, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{CHECK_FEED}/stage-reviews/summary"))
    );
    assert_eq!(b["settle"]["fixed_lost"], 1, "{b}");
    let (_, b, _) = call!(&app, ed.req("GET", &format!("/stage-reviews/{koyambedu}")));
    assert_eq!(b["change_set_status"], "discarded", "{b}");
    let (_, b, _) = call!(
        &app,
        ed.req(
            "GET",
            &format!("/feeds/{CHECK_FEED}/route-issues?status=fixed&lost=true")
        )
    );
    assert_eq!(b["items"].as_array().unwrap().len(), 1, "{b}");
    let (_, b, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{CHECK_FEED}/route-issues/summary"))
    );
    assert_eq!(b["fixed_lost"], 1, "{b}");

    // ---- the stages named after themselves that have one MTC twin
    let (s, b, _) = call!(
        &app,
        ed.req("GET", &format!("/feeds/{CHECK_FEED}/stage-twins"))
    );
    assert_eq!(s, 200, "{b}");
    let twins = b["items"].as_array().unwrap();
    assert_eq!(twins.len(), 1, "{b}");
    assert_eq!(twins[0]["stage_key"], "nm_SAIDAPET_BT|up", "{b}");
    assert_eq!(twins[0]["twin"]["stage_key"], "stg_bt|up", "{b}");
    assert_eq!(twins[0]["distance_m"], 0, "{b}");
    assert_eq!(twins[0]["far"], false, "{b}");
    // nm_SAIDAPET has several: SAIDAPET is stg_two and stg_far_same
    assert_eq!(b["several"], 1, "{b}");

    exec(&pool, &clear_feed(CHECK_FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}

const GONE_FEED: &str = "editor_stage_review_gone_feed";
const GONE_ADMIN: &str = "admin@editor-stage-review-gone-test.invalid";
const GONE_EDITOR: &str = "editor@editor-stage-review-gone-test.invalid";

fn row_of<'a>(list: &'a Value, stage_id: &str) -> Option<&'a Value> {
    list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["stage_id"] == stage_id)
}

/// The queue lists only what still wants a decision, and says what has
/// happened around the rest:
///
///   - "To settle" never lists a stage that mapped cleanly, and "Mapped
///     cleanly" lists nothing else;
///   - a search finds a stage by its id, and puts the names it matches ahead
///     of the ones it only resembles;
///   - a stage whose other way is already closed says so;
///   - a committed draft that merges a stage away closes its open review by
///     itself, naming the draft; a stage it only edits keeps its review, which
///     now says the draft changed it;
///   - migration 0030 does the same for drafts committed before.
#[actix_web::test]
async fn a_stage_merged_away_leaves_the_queue_with_it() {
    let Some(pool) = local_pool().await else {
        return;
    };
    let mut seed = seed_for(GONE_FEED, &[GONE_ADMIN, GONE_EDITOR]);
    seed.push(format!(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, name, direction, provenance) VALUES \
         ('{GONE_FEED}', 'stg_clean', 'GUINDY', 'up', '{{}}'::jsonb), \
         ('{GONE_FEED}', 'stg_away', 'SAIDAPET', 'up', '{{}}'::jsonb)"
    ));
    seed.push(format!(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, direction, position, stop_id, stop_type) VALUES \
         ('{GONE_FEED}', 'stg_clean', 'up', 1, 'E', 'NEW STOP'), \
         ('{GONE_FEED}', 'stg_away', 'up', 1, 'A', 'NEW STOP')"
    ));
    // reviews as the mapper writes them, by stage id: stg_two both ways (down
    // already left alone), stg_other down, and GUINDY that mapped cleanly
    seed.push(format!(
        "INSERT INTO gtfs_stage_review (gtfs_id, batch, name, name_key, direction, reason, impact, evidence, \
                                        status, review_note) VALUES \
         ('{GONE_FEED}', 'test batch', 'SAIDAPET', 'stg_two', 'up', 'head_differs', 25, '{{}}'::jsonb, 'pending', NULL), \
         ('{GONE_FEED}', 'test batch', 'SAIDAPET', 'stg_two', 'down', 'stretch_differs', 5, '{{}}'::jsonb, \
          'confirmed', 'the routes really differ'), \
         ('{GONE_FEED}', 'test batch', 'SAIDAPET', 'stg_other', 'down', 'stretch_differs', 30, '{{}}'::jsonb, 'pending', NULL), \
         ('{GONE_FEED}', 'test batch', 'GUINDY', 'stg_clean', 'up', 'agreed', 0, '{{}}'::jsonb, 'pending', NULL)"
    ));
    exec(&pool, &seed).await;
    let signer = TestSigner::generate("stage-review-gone-test-key");
    let (st, dir) = state(&pool, &signer, GONE_ADMIN);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;
    let mut admin = Caller {
        signer: &signer,
        email: GONE_ADMIN.into(),
        session: None,
    };
    let mut ed = Caller {
        signer: &signer,
        email: GONE_EDITOR.into(),
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
    let (s, b, _) = call!(
        &app,
        admin
            .req("POST", "/users")
            .set_json(json!({"email": GONE_EDITOR, "role": "editor"}))
    );
    assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
    let (_, users, _) = call!(&app, admin.req("GET", "/users"));
    let uid = users["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["email"] == GONE_EDITOR)
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, b, _) = call!(
        &app,
        admin
            .req("PATCH", &format!("/users/{uid}"))
            .set_json(json!({"role": "editor", "status": "active"}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(
        &app,
        admin
            .req("PUT", &format!("/users/{uid}/feeds/{GONE_FEED}"))
            .set_json(json!({"role": "editor"}))
    );
    assert_eq!(s, 200, "{b}");
    let (s, b, _) = call!(&app, ed.req("POST", "/auth/totp/enroll"));
    assert_eq!(s, 200, "{b}");
    let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
    let (s, b, cookie) = call!(
        &app,
        ed.req("POST", "/auth/totp/confirm")
            .set_json(json!({"code": crypto::totp_now(&secret, now())}))
    );
    assert_eq!(s, 200, "{b}");
    ed.session = cookie;
    let queue =
        |extra: &str| format!("/feeds/{GONE_FEED}/stage-reviews?group=stage&status=pending{extra}");

    // ---- the two sides do not mix
    let (s, b, _) = call!(&app, ed.req("GET", &queue("&side=settle")));
    assert_eq!(s, 200, "{b}");
    assert!(
        row_of(&b, "stg_clean").is_none(),
        "mapped cleanly is not to settle: {b}"
    );
    assert!(row_of(&b, "stg_two").is_some(), "{b}");
    let (_, b, _) = call!(&app, ed.req("GET", &queue("&side=verify")));
    let ids: Vec<&str> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["stage_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["stg_clean"], "{b}");
    let (s, b, _) = call!(&app, ed.req("GET", &queue("&side=sideways")));
    assert_eq!(s, 400, "{b}");
    assert_eq!(code_of(&b), "invalid_side", "{b}");

    // ---- a search finds a stage by its id, and the names it matches first
    let (_, b, _) = call!(&app, ed.req("GET", &queue("&side=settle&q=STG_OTHER")));
    let ids: Vec<&str> = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["stage_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["stg_other"], "{b}");
    let (_, b, _) = call!(&app, ed.req("GET", &queue("&q=KOYAMBEDU")));
    assert_eq!(b["items"][0]["stage_id"], "KOYAMBEDU", "{b}");

    // ---- stg_two's down was left alone: its row says so
    let (_, b, _) = call!(&app, ed.req("GET", &queue("&side=settle")));
    let two = row_of(&b, "stg_two").unwrap();
    assert_eq!(
        two["closed_ways"],
        json!([{"direction": "down", "status": "confirmed"}]),
        "{b}"
    );
    assert_eq!(two["directions"], json!(["up"]), "{b}");
    assert!(two["changed"].is_null(), "{b}");

    // ---- a draft merges stg_two into stg_one and edits stg_other, and goes
    //      live without anybody marking either review
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/feeds/{GONE_FEED}/change-sets"))
            .set_json(json!({"title": "One SAIDAPET"}))
    );
    assert_eq!(s, 201, "{b}");
    let set = b["change_set_id"].as_str().unwrap().to_string();
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "merge", "entity_key": "stg_two|up",
                "after": {"into_stage_id": "stg_one|up"}
            }))
    );
    assert_eq!(s, 201, "{b}");
    assert!(errors(&b).is_empty(), "{b}");
    let (s, b, _) = call!(
        &app,
        ed.req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(json!({
                "entity": "stage", "op": "update", "entity_key": "stg_other|down",
                "after": {"name": "SAIDAPET", "direction": "down", "rows": [
                    {"stop_id": "E", "stop_type": "NEW STOP"},
                    {"stop_id": "D", "stop_type": "INTERMEDIATE STOP"},
                ]}
            }))
    );
    assert_eq!(s, 201, "{b}");
    assert!(errors(&b).is_empty(), "{b}");
    let (s, b, _) = call!(&app, ed.req("POST", &format!("/change-sets/{set}/submit")));
    assert_eq!(s, 200, "submit: {b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set}/approve"))
    );
    assert_eq!(s, 200, "approve: {b}");
    let (s, b, _) = call!(
        &app,
        admin.req("POST", &format!("/change-sets/{set}/commit"))
    );
    assert_eq!(s, 200, "commit: {b}");

    // the merged-away stage's review closed by itself, naming the draft and
    // whoever made it
    let row = sqlx::query(&format!(
        "SELECT r.status, r.change_set_id::text AS set, r.review_note, u.email \
         FROM gtfs_stage_review r LEFT JOIN gtfs_editor_user u ON u.user_id = r.reviewed_by \
         WHERE r.gtfs_id = '{GONE_FEED}' AND r.name_key = 'stg_two' AND r.direction = 'up'"
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("status"), "fixed");
    assert_eq!(
        row.get::<Option<String>, _>("set").as_deref(),
        Some(set.as_str())
    );
    assert_eq!(
        row.get::<Option<String>, _>("review_note").as_deref(),
        Some("Closed on its own: draft “One SAIDAPET” merged stage stg_two into stg_one.")
    );
    assert_eq!(
        row.get::<Option<String>, _>("email").as_deref(),
        Some(GONE_EDITOR)
    );
    assert_eq!(
        scalar_i64(
            &pool,
            format!(
                "SELECT count(*) FROM gtfs_audit_log WHERE gtfs_id = '{GONE_FEED}' \
                 AND action = 'stage_review_closed' AND change_set_id = '{set}' \
                 AND detail->>'automatic' = 'true' AND detail->>'merged_into' = 'stg_one'"
            )
        )
        .await,
        1
    );
    // the older review by name stays: stg_one is still a SAIDAPET going up
    assert_eq!(
        scalar_text(
            &pool,
            format!(
                "SELECT status FROM gtfs_stage_review WHERE gtfs_id = '{GONE_FEED}' \
                 AND name_key = 'SAIDAPET' AND direction = 'up'"
            )
        )
        .await
        .as_deref(),
        Some("pending")
    );
    // the edited one stays, and says the draft changed it
    let (_, b, _) = call!(&app, ed.req("GET", &queue("&side=settle")));
    assert!(row_of(&b, "stg_two").is_none(), "gone from the queue: {b}");
    let other = row_of(&b, "stg_other").unwrap();
    assert_eq!(other["changed"]["title"], "One SAIDAPET", "{b}");
    assert_eq!(other["changed"]["change_set_id"], set.as_str(), "{b}");
    let id = other["review_id"].as_i64().unwrap();
    let (s, r, _) = call!(&app, ed.req("GET", &format!("/stage-reviews/{id}")));
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["changed"]["change_set_id"], set.as_str(), "{r}");
    // an edit of the other direction is not a change to this one
    let (_, b, _) = call!(&app, ed.req("GET", &queue("&q=SAIDAPET")));
    let by_name = row_of(&b, "SAIDAPET").unwrap();
    let id = by_name["review_id"].as_i64().unwrap();
    let (_, r, _) = call!(&app, ed.req("GET", &format!("/stage-reviews/{id}")));
    assert!(r["changed"].is_null(), "{r}");

    // ---- migration 0030: a review left open by a draft committed before
    //      commits closed them is closed the same way, once
    exec(
        &pool,
        &[format!(
            "INSERT INTO gtfs_stage_review (gtfs_id, batch, name, name_key, direction, reason, impact, created_at) \
             VALUES ('{GONE_FEED}', 'older batch', 'SAIDAPET', 'stg_two', 'up', 'head_differs', 7, \
                     now() - interval '1 day')"
        )],
    )
    .await;
    let migration = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/db/gtfs_editor/0030_close_reviews_of_stages_gone.sql"
    ))
    .unwrap();
    // in a transaction of the test's own, rolled back after, so the other
    // feeds in this database are not touched
    let body = migration.replace("BEGIN;", "").replace("COMMIT;", "");
    let mut tx = pool.begin().await.unwrap();
    for _ in 0..2 {
        // a &str runs unprepared, so every statement in the file runs
        sqlx::Executor::execute(&mut *tx, body.as_str())
            .await
            .unwrap();
    }
    let rows = sqlx::query(&format!(
        "SELECT r.status, r.change_set_id::text AS set, r.review_note, \
                (SELECT count(*) FROM gtfs_audit_log a WHERE a.gtfs_id = r.gtfs_id \
                   AND a.actor_email = 'migration 0030' \
                   AND a.detail->>'review_id' = r.review_id::text) AS audited \
         FROM gtfs_stage_review r \
         WHERE r.gtfs_id = '{GONE_FEED}' AND r.batch = 'older batch'"
    ))
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<String, _>("status"), "fixed");
    assert_eq!(
        rows[0].get::<Option<String>, _>("set").as_deref(),
        Some(set.as_str())
    );
    assert!(rows[0]
        .get::<Option<String>, _>("review_note")
        .unwrap()
        .starts_with("Closed on its own: draft “One SAIDAPET” merged stage stg_two"));
    assert_eq!(
        rows[0].get::<i64, _>("audited"),
        1,
        "audited once though run twice"
    );
    // nothing else of this feed is closed by it: the edited stage is live
    assert_eq!(
        sqlx::query_scalar::<_, String>(&format!(
            "SELECT status FROM gtfs_stage_review WHERE gtfs_id = '{GONE_FEED}' \
             AND name_key = 'stg_other'"
        ))
        .fetch_one(&mut *tx)
        .await
        .unwrap(),
        "pending"
    );
    tx.rollback().await.unwrap();

    exec(&pool, &clear_feed(GONE_FEED)).await;
    std::fs::remove_dir_all(dir).ok();
}
