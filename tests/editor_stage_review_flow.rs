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
    let mut s = clear_feed(FEED);
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{FEED}', 'Stage review test feed')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', c, c, 'STOP ' || c, 13.0 + ascii(c) * 0.001, 80.2 \
         FROM unnest(ARRAY['A','B','C','D','E']) c"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id) VALUES \
         ('{FEED}', 'R1', '1', 'up', 'AG'), ('{FEED}', 'R2', '2', 'also up', 'AG')"
    ));
    // SAIDAPET: R1 starts it at A, R2 at B - the same name, two places
    s.push(format!(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, name, direction, review, provenance) VALUES \
         ('{FEED}', 'stg_one', 'SAIDAPET', 'up', 'head_differs', \
          '{{\"source\": \"backfill\", \"review\": {{\"heads\": [\"A\", \"B\"]}}}}'::jsonb), \
         ('{FEED}', 'stg_two', 'SAIDAPET', 'up', 'head_differs', \
          '{{\"source\": \"backfill\"}}'::jsonb), \
         ('{FEED}', 'stg_other', 'SAIDAPET', 'down', NULL, '{{\"source\": \"backfill\"}}'::jsonb)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, direction, position, stop_id, stop_type) VALUES \
         ('{FEED}', 'stg_one', 'up', 1, 'A', 'NEW STOP'), \
         ('{FEED}', 'stg_one', 'up', 2, 'C', 'INTERMEDIATE STOP'), \
         ('{FEED}', 'stg_one', 'up', 3, 'D', 'INTERMEDIATE STOP'), \
         ('{FEED}', 'stg_two', 'up', 1, 'B', 'NEW STOP'), \
         ('{FEED}', 'stg_other', 'down', 1, 'E', 'NEW STOP')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stage (gtfs_id, route_id, position, stage_id, direction, stage_no) VALUES \
         ('{FEED}', 'R1', 1, 'stg_one', 'up', 1), ('{FEED}', 'R2', 1, 'stg_two', 'up', 1)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES \
         ('{FEED}', 'R1', 1, 'A', 'NEW STOP', 1, 'SAIDAPET', '11'), \
         ('{FEED}', 'R1', 2, 'C', 'INTERMEDIATE STOP', 1, 'SAIDAPET', '11'), \
         ('{FEED}', 'R1', 3, 'D', 'INTERMEDIATE STOP', 1, 'SAIDAPET', '11'), \
         ('{FEED}', 'R2', 1, 'B', 'NEW STOP', 1, 'SAIDAPET', '22')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stage_review (gtfs_id, batch, name, name_key, direction, reason, impact, evidence) VALUES \
         ('{FEED}', 'test batch', 'SAIDAPET', 'SAIDAPET', 'up', 'head_differs', 40, \
          '{{\"heads\": [\"A\", \"B\"], \"routes\": [\"R1\", \"R2\"], \"stages\": [\"stg_one\", \"stg_two\"]}}'::jsonb), \
         ('{FEED}', 'test batch', 'KOYAMBEDU', 'KOYAMBEDU', 'up', 'stretch_differs', 4, \
          '{{\"stops\": [\"C\"], \"routes\": [\"R1\"]}}'::jsonb)"
    ));
    s.extend(reset_accounts(&[ADMIN, EDITOR, VIEWER]));
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
                     FROM gtfs_route_stop WHERE gtfs_id = '{FEED}' AND pattern_key = 1) d"
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
