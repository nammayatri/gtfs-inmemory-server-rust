//! A commit and a burst of draft edits on the same feed at the same time
//! (docs/gtfs-editor.md section 3, "Commit"): every transaction that replays or
//! applies a feed's changes queues on the feed's advisory lock, so the two never
//! deadlock inside Postgres - and a deadlock, should one ever happen, is retried
//! and never reported as a change's own `database_rejected` finding.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that is
//! not local. It uses its own feed and accounts and removes its rows afterwards.
//! See scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

const FEED: &str = "editor_lock_test_feed";
const AUD: &str = "gtfs.editor-lock-test.local";
const ADMIN: &str = "admin@editor-lock-test.invalid";
const EDITOR: &str = "editor@editor-lock-test.invalid";
const APPROVER: &str = "approver@editor-lock-test.invalid";
const BASE: &str = "/internal/gtfs-editor";

/// Rounds of "commit draft B while draft A is being edited", and the edits of
/// draft A per round: `ROUNDS * ADDS_PER_ROUND` adds in all, each followed by
/// the removal of the draft's oldest change so the draft - and the replay every
/// edit runs - stays the same size throughout.
const ROUNDS: usize = 30;
const ADDS_PER_ROUND: usize = 10;
/// Changes draft A holds before the rounds start: the size of every replay.
const DRAFT_A_BASE: usize = 16;
/// Hot stops every route calls at and every draft B moves.
const HOT_STOPS: usize = 11;

// ---------------------------------------------------------------- harness

struct Caller<'a> {
    signer: &'a TestSigner,
    email: &'static str,
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
            .max_connections(6)
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

fn clear_feed() -> Vec<String> {
    vec![
        format!("DELETE FROM gtfs_position_review WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_station_proposal WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("UPDATE gtfs_stop SET parent_station = NULL WHERE gtfs_id = '{FEED}' AND parent_station IS NOT NULL"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
    ]
}

/// The feed: `HOT_STOPS` hot stops H1.. that every route calls at and every
/// draft B moves, and one pair of duplicates per round - `Da<n>` on routes R1
/// and R3, `Db<n>` on R2 and R4 - for that round's merge. Four routes, each the
/// hot stops then its duplicates.
fn seed() -> Vec<String> {
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name) VALUES ('{FEED}', 'Editor feed lock test feed')"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', 'H' || i, 'H' || i, 'HOT ' || i, 13.0 + i * 0.001, 80.2 FROM generate_series(1, {HOT_STOPS}) i"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', side || i, side || i, 'DUPLICATE ' || i, 13.1 + i * 0.001, 80.3 \
         FROM generate_series(1, {ROUNDS}) i, (VALUES ('Da'), ('Db')) v(side)"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name) \
         SELECT '{FEED}', 'R' || i, 'T' || i, 'HOT 1 To HOT {HOT_STOPS}' FROM generate_series(1, 4) i"
    ));
    for route in 1..=4 {
        let side = if route % 2 == 1 { "Da" } else { "Db" };
        s.push(format!(
            "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name) \
             SELECT '{FEED}', 'R{route}', i, 'H' || i, CASE WHEN i % 2 = 1 THEN 'NEW STOP' ELSE 'INTERMEDIATE STOP' END, \
                    (i + 1) / 2, 'HOT ' || (((i + 1) / 2) * 2 - 1) FROM generate_series(1, {HOT_STOPS}) i"
        ));
        s.push(format!(
            "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name) \
             SELECT '{FEED}', 'R{route}', {HOT_STOPS} + i, '{side}' || i, 'INTERMEDIATE STOP', \
                    ({HOT_STOPS} + 1) / 2, 'HOT {HOT_STOPS}' FROM generate_series(1, {ROUNDS}) i"
        ));
    }
    let list = [ADMIN, EDITOR, APPROVER]
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(", ");
    s.push(format!(
        "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, totp_last_step = NULL, \
         status = 'active' WHERE email IN ({list})"
    ));
    s.push(format!(
        "DELETE FROM gtfs_editor_session WHERE user_id IN (SELECT user_id FROM gtfs_editor_user WHERE email IN ({list}))"
    ));
    s
}

fn state(pool: &PgPool, signer: &TestSigner) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-lock-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    use base64::Engine;
    let st = EditorState::build(
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
            ops_pool: None,
        },
    )
    .unwrap();
    (st, dir)
}

/// Every validation finding in a response (a set detail, or an error's
/// details) that reports a deadlock as the change's own fault.
fn deadlock_findings(body: &Value) -> Vec<Value> {
    let lists = [
        &body["validation"],
        &body["error"]["details"]["validation"],
        &body["change_set"]["validation"],
    ];
    lists
        .iter()
        .filter_map(|v| v.as_array())
        .flatten()
        .filter(|f| {
            f["code"] == "database_rejected"
                && f["message"]
                    .as_str()
                    .map(|m| m.contains("deadlock"))
                    .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// A `route_stops/replace` of route `R<route>`: the route's hot stops as they
/// were read at the start (the duplicates a round merges away are left out, so
/// the change keeps applying after every commit), the last one spelt with
/// `tag`. Its replay deletes and rewrites every row of the route.
fn replace_change(route: usize, detail: &Value, tag: usize) -> Value {
    let rows: Vec<Value> = detail["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["stop_id"].as_str().is_some_and(|id| id.starts_with('H')))
        .map(|r| {
            let last = r["stop_id"] == format!("H{HOT_STOPS}");
            json!({
                "stop_id": r["stop_id"], "stop_type": r["stop_type"],
                "stage_no": r["stage_no"], "stage_name": r["stage_name"],
                "stop_name_override": if last { json!(format!("HOT {HOT_STOPS} #{tag}")) } else { Value::Null },
            })
        })
        .collect();
    json!({
        "entity": "route_stops", "op": "replace", "entity_key": format!("R{route}"),
        "after": {"base_rows_hash": detail["rows_hash"], "rows": rows},
    })
}

// ---------------------------------------------------------------- the flow

#[actix_web::test]
async fn a_commit_and_concurrent_draft_edits_never_deadlock() {
    let Some(pool) = local_pool().await else {
        return;
    };
    exec(&pool, &seed()).await;
    let signer = TestSigner::generate("lock-test-key");
    let (st, dir) = state(&pool, &signer);
    let app =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(st)))))
            .await;

    // ---- accounts
    let mut admin = Caller {
        signer: &signer,
        email: ADMIN,
        session: None,
    };
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
    let editor_c = editor_c;
    let approver = approver;

    let new_set = |title: &str| {
        editor_c
            .req("POST", &format!("/feeds/{FEED}/change-sets"))
            .set_json(json!({"title": title}))
    };
    let add = |set: &str, change: Value| {
        editor_c
            .req("POST", &format!("/change-sets/{set}/changes"))
            .set_json(change)
    };

    // ---- draft A: a script's stop-list rewrites, `DRAFT_A_BASE` of them to start
    let (s, set_a, _) = call!(&app, new_set("script: stop-list rewrites"));
    assert_eq!(s, 201, "{set_a}");
    let set_a = set_a["change_set_id"].as_str().unwrap().to_string();
    let mut details = Vec::new();
    for route in 1..=4 {
        let (s, d, _) = call!(
            &app,
            editor_c.req("GET", &format!("/feeds/{FEED}/routes/R{route}"))
        );
        assert_eq!(s, 200, "{d}");
        details.push(d);
    }
    let mut a_changes: VecDeque<i64> = VecDeque::new();
    let mut alone = Vec::new();
    for k in 0..DRAFT_A_BASE {
        let route = k % 4 + 1;
        let t = Instant::now();
        let (s, b, _) = call!(
            &app,
            add(&set_a, replace_change(route, &details[route - 1], k))
        );
        alone.push(t.elapsed());
        assert_eq!(s, 201, "{b}");
        assert!(deadlock_findings(&b).is_empty(), "{b}");
        a_changes.push_back(b["change_id"].as_i64().unwrap());
    }
    let avg = |v: &[Duration]| v.iter().sum::<Duration>() / v.len().max(1) as u32;
    let alone_avg = avg(&alone[DRAFT_A_BASE / 2..]);
    eprintln!(
        "[timing] one add to draft A (replaying {}..{DRAFT_A_BASE} stop-list changes), nothing else running: {alone_avg:?} average, {:?} max",
        DRAFT_A_BASE / 2,
        alone.iter().max().unwrap()
    );

    // ---- the rounds
    let mut adds = 0usize;
    let mut removals = 0usize;
    let mut add_retries = 0usize;
    let mut removal_retries = 0usize;
    let mut deadlocks: Vec<Value> = Vec::new();
    let mut failed_commits: Vec<Value> = Vec::new();
    let mut commit_took = Vec::new();
    let mut add_took = Vec::new();
    for round in 1..=ROUNDS {
        // draft B: a person's 11 moves, a merge and a rename, submitted and approved
        let (s, set_b, _) = call!(&app, new_set(&format!("round {round}: moves and a merge")));
        assert_eq!(s, 201, "{set_b}");
        let set_b = set_b["change_set_id"].as_str().unwrap().to_string();
        let flip = if round % 2 == 1 { 0.0002 } else { -0.0002 };
        let mut changes: Vec<Value> = (1..=HOT_STOPS)
            .map(|i| {
                json!({"entity": "stop", "op": "update", "entity_key": format!("H{i}"),
                       "after": {"lat": 13.0 + i as f64 * 0.001 + flip, "lon": 80.2}})
            })
            .collect();
        changes.push(
            json!({"entity": "stop", "op": "merge", "entity_key": format!("Da{round}"),
                            "after": {"into_stop_id": format!("Db{round}")}}),
        );
        changes.push(
            json!({"entity": "stop", "op": "update", "entity_key": format!("Db{round}"),
                            "after": {"name": format!("DUPLICATE {round} (kept)")}}),
        );
        for c in changes {
            let (s, b, _) = call!(&app, add(&set_b, c));
            assert_eq!(s, 201, "round {round}: {b}");
        }
        let (s, b, _) = call!(
            &app,
            editor_c.req("POST", &format!("/change-sets/{set_b}/submit"))
        );
        assert_eq!(s, 200, "round {round}: {b}");
        let (s, b, _) = call!(
            &app,
            approver.req("POST", &format!("/change-sets/{set_b}/approve"))
        );
        assert_eq!(s, 200, "round {round}: {b}");

        // the round: commit B while A takes ADDS_PER_ROUND adds (each followed
        // by the removal of its oldest change), all in flight together. The
        // commit starts at a different point of the edit burst each round.
        let commit = async {
            tokio::time::sleep(Duration::from_millis((round * 7 % 40) as u64)).await;
            let t = Instant::now();
            let (s, b, _) = call!(
                &app,
                approver.req("POST", &format!("/change-sets/{set_b}/commit"))
            );
            (s, b, t.elapsed())
        };
        let edits = async {
            let mut out = Vec::new();
            for k in 0..ADDS_PER_ROUND {
                let route = (round + k) % 4 + 1;
                let tag = DRAFT_A_BASE + round * ADDS_PER_ROUND + k;
                let t = Instant::now();
                let (s, b, _) = call!(
                    &app,
                    add(&set_a, replace_change(route, &details[route - 1], tag))
                );
                let took = t.elapsed();
                let removal = if s == 201 {
                    let oldest = a_changes.pop_front().unwrap();
                    a_changes.push_back(b["change_id"].as_i64().unwrap());
                    let (ds, db, _) = call!(
                        &app,
                        editor_c.req("DELETE", &format!("/change-sets/{set_a}/changes/{oldest}"))
                    );
                    Some((ds, db))
                } else {
                    None
                };
                out.push((s, b, took, removal));
            }
            out
        };
        let ((cs, cb, ct), edits) = futures::join!(commit, edits);
        commit_took.push(ct);
        deadlocks.extend(deadlock_findings(&cb));
        if cs != 200 {
            failed_commits.push(json!({"round": round, "status": cs, "body": cb}));
        }
        for (s, b, took, removal) in edits {
            deadlocks.extend(deadlock_findings(&b));
            match (s, code_of(&b)) {
                (201, _) => {
                    adds += 1;
                    add_took.push(took);
                }
                (503, "try_again") => add_retries += 1,
                _ => panic!("round {round}: an add answered {s}: {b}"),
            }
            if let Some((ds, db)) = removal {
                deadlocks.extend(deadlock_findings(&db));
                match (ds, code_of(&db)) {
                    (200, _) => removals += 1,
                    (503, "try_again") => removal_retries += 1,
                    _ => panic!("round {round}: a removal answered {ds}: {db}"),
                }
            }
        }
        if cs == 200 {
            // the round's merge went live
            let gone: bool = sqlx::query(&format!(
                "SELECT deleted FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'Da{round}'"
            ))
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("deleted");
            assert!(gone, "round {round}: Da{round} was merged away");
        }
    }

    eprintln!(
        "[lock test] {ROUNDS} commits of 13 changes each; concurrently {adds} adds and {removals} removals on a draft of {DRAFT_A_BASE} stop-list changes; \
         {add_retries} adds and {removal_retries} removals answered 503 try_again; {} deadlock findings; {} failed commits",
        deadlocks.len(),
        failed_commits.len()
    );
    eprintln!(
        "[timing] commit of 13 changes while draft A is edited: {:?} average, {:?} max",
        avg(&commit_took),
        commit_took.iter().max().unwrap()
    );
    eprintln!(
        "[timing] one add to draft A while a commit runs: {:?} average, {:?} max (alone: {alone_avg:?})",
        avg(&add_took),
        add_took.iter().max().unwrap()
    );
    assert!(
        deadlocks.is_empty(),
        "a deadlock was reported as a change's fault: {}",
        json!(deadlocks)
    );
    assert!(
        failed_commits.is_empty(),
        "every commit must succeed: {}",
        json!(failed_commits)
    );
    assert_eq!(adds + add_retries, ROUNDS * ADDS_PER_ROUND);
    assert_eq!(removals + removal_retries, adds);
    // serialised on the feed's lock, nothing should ever need the caller's retry;
    // a handful is tolerated so a one-off elsewhere does not fail the run
    assert!(
        add_retries + removal_retries <= (adds + removals) / 20,
        "{add_retries} adds and {removal_retries} removals were handed back to the caller as try_again"
    );

    // draft A is the size it started at (plus any change a failed removal left)
    // and still applies: its stop lists are conflicts now (the commits changed
    // the routes), but nothing is rejected
    let (s, detail, _) = call!(&app, editor_c.req("GET", &format!("/change-sets/{set_a}")));
    assert_eq!(s, 200, "{detail}");
    assert_eq!(detail["change_count"], DRAFT_A_BASE + removal_retries);
    assert!(deadlock_findings(&detail).is_empty(), "{detail}");
    assert!(
        detail["validation"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["level"] != "error"),
        "{}",
        detail["validation"]
    );

    exec(&pool, &clear_feed()).await;
    std::fs::remove_dir_all(dir).ok();
}
