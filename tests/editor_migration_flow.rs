//! Migrations from master to prod (docs/gtfs-editor.md section 23), end to end
//! against two real Postgres databases holding the editor schema
//! (db/gtfs_editor/0001..0031): one is master's, the other prod's, each with its
//! own editor in front of it.
//!
//! Master builds its routes from stages; prod serves each route's own stop list
//! and has edits of its own. The whole feed moves in steps - stops and routes,
//! stages, the routes' stage lists, turning stages on - each a prod draft a
//! second person commits, the stages changing nothing prod serves until the
//! switch; prod's own edits are kept and listed until taken; the switch is
//! refused while an active route has no stages. Then single drafts: one that
//! goes in, the same one again (refused), and one that conflicts with an edit
//! of prod's own until its master side is taken.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` (master) and
//! `EDITOR_TEST_PROD_DATABASE_URL` (prod) are both set, local, and different.
//! Uses its own feed and accounts and removes its rows afterwards. See
//! scripts/editor_flow_test.sh.

use actix_web::{test, App};
use gtfs_routes_service::editor::{
    self, crypto, jwt::testing::TestSigner, EditorSettings, EditorState,
};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::Arc;

const AUD: &str = "gtfs.editor-migration-test.local";
const BASE: &str = "/internal/gtfs-editor";
const EMPTY_HASH: &str = "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945";

const FEED: &str = "editor_migration_test_feed";
const ADMIN: &str = "admin@editor-migration-test.invalid";
const APPROVER: &str = "approver@editor-migration-test.invalid";

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

macro_rules! call_bytes {
    ($app:expr, $req:expr) => {{
        let resp = test::call_service($app, $req.to_request()).await;
        let status = resp.status().as_u16();
        let disposition = resp
            .headers()
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = test::read_body(resp).await.to_vec();
        (status, body, disposition)
    }};
}

fn code_of(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or("")
}

fn now() -> u64 {
    chrono::Utc::now().timestamp() as u64
}

async fn pool_of(var: &str) -> Option<PgPool> {
    let Ok(url) = std::env::var(var) else {
        eprintln!("{var} not set; skipping");
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

fn clear_feed() -> Vec<String> {
    vec![
        format!("DELETE FROM gtfs_migration_import WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_change_set WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route_stage WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stage_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stage WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_route WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_stop WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_editor_feed_access WHERE gtfs_id = '{FEED}'"),
        format!("DELETE FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
    ]
}

/// The same stops and routes on both sides; prod also holds each route's own
/// stop list, which is what it serves while its stages are off.
fn seed(use_stages: bool) -> Vec<String> {
    let list = [ADMIN, APPROVER]
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut s = clear_feed();
    s.push(format!(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, use_stages) \
         VALUES ('{FEED}', 'Editor migration test feed', {use_stages})"
    ));
    s.push(format!(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon) \
         SELECT '{FEED}', c, c, 'STOP ' || c, 13.0 + ascii(c) * 0.001, 80.2 \
         FROM unnest(ARRAY['A','B','C','D','E','F','G','H']) c"
    ));
    s.push(format!(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, agency_id, active) VALUES \
         ('{FEED}', 'R1', '1', 'A To E', 'AG', true), ('{FEED}', 'R2', '2', 'A To E short', 'AG', true), \
         ('{FEED}', 'R3', '3', 'retired', 'AG', false)"
    ));
    if !use_stages {
        s.push(format!(
            "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, provider_id) VALUES \
             ('{FEED}', 'R1', 1, 'A', 'NEW STOP', 1, 'ALPHA', '11'), ('{FEED}', 'R1', 2, 'B', 'INTERMEDIATE STOP', 1, 'ALPHA', '11'), \
             ('{FEED}', 'R1', 3, 'C', 'NEW STOP', 2, 'BETA', '11'), ('{FEED}', 'R1', 4, 'E', 'NEW STOP', 3, 'GAMMA', '11'), \
             ('{FEED}', 'R2', 1, 'A', 'NEW STOP', 1, 'ALPHA', '22'), ('{FEED}', 'R2', 2, 'B', 'INTERMEDIATE STOP', 1, 'ALPHA', '22'), \
             ('{FEED}', 'R2', 3, 'E', 'NEW STOP', 2, 'GAMMA', '22')"
        ));
    }
    s.push(format!(
        "UPDATE gtfs_editor_user SET totp_enabled = false, totp_secret_enc = NULL, totp_last_step = NULL, \
         status = 'active' WHERE email IN ({list})"
    ));
    s.push(format!(
        "DELETE FROM gtfs_editor_session WHERE user_id IN \
         (SELECT user_id FROM gtfs_editor_user WHERE email IN ({list}))"
    ));
    s
}

fn state(pool: &PgPool, signer: &TestSigner, is_master: bool) -> (EditorState, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("editor-migration-{}", crypto::random_token()));
    std::fs::create_dir_all(&dir).unwrap();
    let jwks = dir.join("jwks.json");
    std::fs::write(&jwks, signer.jwks()).unwrap();
    use base64::Engine;
    let mut st = EditorState::build(
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
    .unwrap();
    st.is_master = is_master;
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

async fn scalar_text(pool: &PgPool, sql: impl AsRef<str>) -> String {
    let sql = sql.as_ref();
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<String, _>(0)
}

/// What a route is served as, through the view every reader uses.
async fn served(pool: &PgPool, route: &str) -> Vec<String> {
    sqlx::query(&format!(
        "SELECT coalesce(stop_id, marker_id) || '/' || stop_type || '/' || stage_no || '/' || stage_name AS r \
         FROM gtfs_route_stop_effective WHERE gtfs_id = '{FEED}' AND route_id = '{route}' \
           AND pattern_key = 1 ORDER BY sequence"
    ))
    .fetch_all(pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<String, _>("r"))
    .collect()
}

fn stops(list: &[(&str, &str)]) -> Value {
    json!(list
        .iter()
        .map(|(id, t)| json!({"stop_id": id, "stop_type": t}))
        .collect::<Vec<_>>())
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- the flow

#[actix_web::test]
async fn master_moves_to_prod_a_step_at_a_time_and_a_draft_at_a_time() {
    let (Some(mpool), Some(ppool)) = (
        pool_of("EDITOR_TEST_DATABASE_URL").await,
        pool_of("EDITOR_TEST_PROD_DATABASE_URL").await,
    ) else {
        return;
    };
    assert_ne!(
        std::env::var("EDITOR_TEST_DATABASE_URL").unwrap(),
        std::env::var("EDITOR_TEST_PROD_DATABASE_URL").unwrap(),
        "master and prod are two databases"
    );
    exec(&mpool, &seed(true)).await;
    exec(&ppool, &seed(false)).await;
    let signer = TestSigner::generate("migration-test-key");
    let (mst, mdir) = state(&mpool, &signer, true);
    let (pst, pdir) = state(&ppool, &signer, false);
    let master =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(mst)))))
            .await;
    let prod =
        test::init_service(App::new().configure(|cfg| editor::configure(cfg, Some(Arc::new(pst)))))
            .await;

    // ---- an admin and an approver on each side
    let new_caller = |email: &str| Caller {
        signer: &signer,
        email: email.into(),
        session: None,
    };
    let (mut m_admin, mut m_ap) = (new_caller(ADMIN), new_caller(APPROVER));
    let (mut p_admin, mut p_ap) = (new_caller(ADMIN), new_caller(APPROVER));
    macro_rules! sign_in {
        ($app:expr, $who:expr) => {{
            let (s, b, _) = call!($app, $who.req("POST", "/auth/totp/enroll"));
            assert_eq!(s, 200, "{b}");
            let secret = crypto::base32_decode(b["secret_base32"].as_str().unwrap()).unwrap();
            let (s, b, cookie) = call!(
                $app,
                $who.req("POST", "/auth/totp/confirm")
                    .set_json(json!({"code": crypto::totp_now(&secret, now())}))
            );
            assert_eq!(s, 200, "{b}");
            $who.session = cookie;
        }};
    }
    macro_rules! approver {
        ($app:expr, $admin:expr, $ap:expr) => {{
            let (s, b, _) = call!(
                $app,
                $admin
                    .req("POST", "/users")
                    .set_json(json!({"email": APPROVER, "role": "approver"}))
            );
            assert!(s == 201 || code_of(&b) == "user_exists", "{s} {b}");
            let (_, users, _) = call!($app, $admin.req("GET", "/users"));
            let id = users["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|u| u["email"] == APPROVER)
                .unwrap()["user_id"]
                .as_str()
                .unwrap()
                .to_string();
            let (s, b, _) = call!(
                $app,
                $admin
                    .req("PATCH", &format!("/users/{id}"))
                    .set_json(json!({"role": "approver", "status": "active"}))
            );
            assert_eq!(s, 200, "{b}");
            let (s, b, _) = call!(
                $app,
                $admin
                    .req("PUT", &format!("/users/{id}/feeds/{FEED}"))
                    .set_json(json!({"role": "approver"}))
            );
            assert_eq!(s, 200, "{b}");
            sign_in!($app, $ap);
        }};
    }
    sign_in!(&master, m_admin);
    approver!(&master, m_admin, m_ap);
    sign_in!(&prod, p_admin);
    approver!(&prod, p_admin, p_ap);

    let (_, me, _) = call!(&master, m_admin.req("GET", "/auth/me"));
    assert_eq!(me["environment"], "master", "{me}");
    let (_, me, _) = call!(&prod, p_admin.req("GET", "/auth/me"));
    assert_eq!(me["environment"], "prod", "{me}");

    // a draft by the admin, submitted, then approved and committed by the approver
    macro_rules! draft {
        ($app:expr, $admin:expr, $title:expr, $changes:expr) => {{
            let (s, b, _) = call!(
                $app,
                $admin
                    .req("POST", &format!("/feeds/{FEED}/change-sets"))
                    .set_json(json!({"title": $title}))
            );
            assert_eq!(s, 201, "{b}");
            let id = b["change_set_id"].as_str().unwrap().to_string();
            for c in $changes {
                let (s, b, _) = call!(
                    $app,
                    $admin
                        .req("POST", &format!("/change-sets/{id}/changes"))
                        .set_json(c)
                );
                assert_eq!(s, 201, "{b}");
            }
            id
        }};
    }
    macro_rules! ship {
        ($app:expr, $admin:expr, $ap:expr, $id:expr) => {{
            let id: &str = &$id;
            let (s, b, _) = call!(
                $app,
                $admin.req("POST", &format!("/change-sets/{id}/submit"))
            );
            assert_eq!(s, 200, "submit: {b}");
            let (s, b, _) = call!($app, $ap.req("POST", &format!("/change-sets/{id}/approve")));
            assert_eq!(s, 200, "approve: {b}");
            let (s, b, _) = call!($app, $ap.req("POST", &format!("/change-sets/{id}/commit")));
            assert_eq!(s, 200, "commit: {b}");
        }};
    }
    let prod_version = || {
        scalar_i64(
            &ppool,
            format!("SELECT version FROM gtfs_feed WHERE gtfs_id = '{FEED}'"),
        )
    };

    // ======================================================== master builds its stages
    let m1 = draft!(
        &master,
        m_admin,
        "stages for routes 1 and 2",
        [
            json!({"entity": "stage", "op": "create", "entity_key": "stg_a", "after": {
                "name": "ALPHA", "rows": stops(&[("A", "NEW STOP"), ("B", "INTERMEDIATE STOP")])}}),
            json!({"entity": "stage", "op": "create", "entity_key": "stg_b", "after": {
                "name": "BETA", "rows": stops(&[("C", "NEW STOP"), ("D", "INTERMEDIATE STOP")])}}),
            json!({"entity": "stage", "op": "create", "entity_key": "stg_c", "after": {
                "name": "GAMMA", "rows": stops(&[("E", "NEW STOP")])}}),
            json!({"entity": "route_stages", "op": "replace", "entity_key": "R1", "after": {
                "stages": [{"stage_id": "stg_a"}, {"stage_id": "stg_b"}, {"stage_id": "stg_c"}],
                "base_stages_hash": EMPTY_HASH}}),
            json!({"entity": "route_stages", "op": "replace", "entity_key": "R2", "after": {
                "stages": [{"stage_id": "stg_a"}, {"stage_id": "stg_c"}],
                "base_stages_hash": EMPTY_HASH}}),
            json!({"entity": "stop", "op": "update", "entity_key": "B",
                   "after": {"name": "STOP B MASTER"}}),
            json!({"entity": "stop", "op": "create", "entity_key": "Z",
                   "after": {"stop_id": "Z", "name": "STOP Z", "lat": 13.2, "lon": 80.3}}),
        ]
    );
    ship!(&master, m_admin, m_ap, m1);

    // ======================================================== prod edits of its own
    let since = prod_version().await;
    let (_, r2, _) = call!(
        &prod,
        p_admin.req("GET", &format!("/feeds/{FEED}/routes/R2"))
    );
    let p1 = draft!(
        &prod,
        p_admin,
        "prod's own edits",
        [
            json!({"entity": "stop", "op": "update", "entity_key": "C", "after": {"name": "PROD C"}}),
            json!({"entity": "route_stops", "op": "replace", "entity_key": "R2", "after": {
                "rows": [
                    {"stop_id": "A", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "ALPHA"},
                    {"stop_id": "E", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "GAMMA"}],
                "base_rows_hash": r2["rows_hash"]}}),
        ]
    );
    ship!(&prod, p_admin, p_ap, p1);
    let r1_before = served(&ppool, "R1").await;
    assert_eq!(r1_before.len(), 4, "{r1_before:?}");

    // ======================================================== the whole feed
    let (s, file, disposition) = call_bytes!(
        &master,
        m_admin.req(
            "GET",
            &format!("/feeds/{FEED}/migration?scopes=stops,routes,stages")
        )
    );
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&file));
    assert!(disposition.contains(".migration.zip"), "{disposition}");
    let import = |q: &str| {
        p_admin
            .req("POST", &format!("/feeds/{FEED}/migrations?{q}"))
            .set_payload(file.clone())
    };

    // master never reads a migration in, and prod's first full one says from when
    let (s, b, _) = call!(
        &master,
        m_admin
            .req("POST", &format!("/feeds/{FEED}/migrations"))
            .set_payload(file.clone())
    );
    assert_eq!((s, code_of(&b)), (400, "import_on_master"), "{b}");
    let (s, b, _) = call!(&prod, import("dry_run=true"));
    assert_eq!((s, code_of(&b)), (400, "since_version_required"), "{b}");
    let (s, b, _) = call!(
        &prod,
        p_admin
            .req("POST", &format!("/feeds/{FEED}/migrations"))
            .set_payload(b"not a migration".to_vec())
    );
    assert_eq!((s, code_of(&b)), (400, "invalid_migration"), "{b}");

    // a dry run: what each step would do, and prod's own edits kept
    let (s, b, _) = call!(
        &prod,
        import(&format!("dry_run=true&since_version={since}"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["step"], "stops_routes", "{b}");
    let steps = b["steps"].as_array().unwrap();
    assert_eq!(steps[0]["planned"]["stop/create"], 1, "{b}");
    assert_eq!(steps[0]["planned"]["stop/update"], 1, "{b}");
    assert_eq!(steps[1]["planned"]["stage/create"], 3, "{b}");
    assert_eq!(steps[2]["planned"]["route_stages/replace"], 1, "{b}");
    assert_eq!(steps[3]["planned"]["feed_config/update"], 1, "{b}");
    let conflicts = ids(&b["conflicts"]);
    assert!(conflicts.contains(&"stop:C".to_string()), "{b}");
    assert!(conflicts.contains(&"route_list:R2".to_string()), "{b}");
    assert!(
        b["change_sets"][0]["change_set_id"].is_null(),
        "a dry run keeps nothing: {b}"
    );
    assert_eq!(
        scalar_i64(
            &ppool,
            format!("SELECT count(*) FROM gtfs_change_set WHERE gtfs_id = '{FEED}'")
        )
        .await,
        1,
        "a dry run writes no draft"
    );

    // step 1: stops and routes
    let (s, b, _) = call!(
        &prod,
        import(&format!("dry_run=false&since_version={since}"))
    );
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["step"], "stops_routes", "{b}");
    let set = b["change_sets"][0]["change_set_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(b["import_id"].is_string(), "{b}");
    // a second read-in waits for the open draft
    let (s, b, _) = call!(&prod, import("dry_run=false"));
    assert_eq!((s, code_of(&b)), (409, "migration_open"), "{b}");
    ship!(&prod, p_admin, p_ap, set);
    assert_eq!(
        scalar_text(
            &ppool,
            format!("SELECT name FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'B'")
        )
        .await,
        "STOP B MASTER"
    );
    assert_eq!(
        scalar_text(
            &ppool,
            format!("SELECT name FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'C'")
        )
        .await,
        "PROD C",
        "prod's own edit is kept"
    );
    assert_eq!(
        scalar_i64(
            &ppool,
            format!("SELECT count(*) FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'Z'")
        )
        .await,
        1
    );

    // step 2: stages, which change nothing prod serves while its stages are off;
    // later reads-in default to the since_version the first one named
    let (s, b, _) = call!(&prod, import("dry_run=false"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["step"], "stages", "{b}");
    assert_eq!(b["since_version"], since, "{b}");
    let set = b["change_sets"][0]["change_set_id"]
        .as_str()
        .unwrap()
        .to_string();
    ship!(&prod, p_admin, p_ap, set);
    assert_eq!(
        scalar_i64(
            &ppool,
            format!("SELECT count(*) FROM gtfs_stage WHERE gtfs_id = '{FEED}'")
        )
        .await,
        3
    );
    assert_eq!(
        served(&ppool, "R1").await,
        r1_before,
        "stages change nothing served while off"
    );

    // step 3: the routes' stage lists; R2 is prod's own, so it is left out
    let (s, b, _) = call!(&prod, import("dry_run=false"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["step"], "route_stages", "{b}");
    assert_eq!(b["change_sets"][0]["changes"], 1, "{b}");
    let set = b["change_sets"][0]["change_set_id"]
        .as_str()
        .unwrap()
        .to_string();
    ship!(&prod, p_admin, p_ap, set);
    assert_eq!(
        served(&ppool, "R1").await,
        r1_before,
        "a stage list changes nothing served while off"
    );
    assert_eq!(
        scalar_i64(&ppool, format!("SELECT count(*) FROM gtfs_route_stage WHERE gtfs_id = '{FEED}' AND route_id = 'R1'")).await,
        3
    );

    // step 4 is refused while active route R2 has no stages: nothing is drafted
    let (s, b, _) = call!(&prod, import("dry_run=false"));
    assert_eq!(s, 200, "{b}");
    assert!(b["change_sets"].as_array().unwrap().is_empty(), "{b}");
    let dropped = b["dropped"].as_array().unwrap();
    assert!(
        dropped.iter().any(|d| d["id"] == "feed:use_stages"
            && d["findings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["code"] == "routes_without_stages")),
        "{b}"
    );

    // taking master's side of R2 drafts its list, and then the switch goes in
    let (s, b, _) = call!(&prod, import("dry_run=false&take=route_list:R2"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["step"], "route_stages", "{b}");
    let set = b["change_sets"][0]["change_set_id"]
        .as_str()
        .unwrap()
        .to_string();
    ship!(&prod, p_admin, p_ap, set);
    let (s, b, _) = call!(&prod, import("dry_run=false"));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["step"], "switch", "{b}");
    let set = b["change_sets"][0]["change_set_id"]
        .as_str()
        .unwrap()
        .to_string();
    ship!(&prod, p_admin, p_ap, set);
    assert_eq!(
        scalar_text(
            &ppool,
            format!("SELECT use_stages::text FROM gtfs_feed WHERE gtfs_id = '{FEED}'")
        )
        .await,
        "true"
    );
    assert_eq!(
        served(&ppool, "R1").await,
        vec![
            "A/NEW STOP/1/ALPHA",
            "B/INTERMEDIATE STOP/1/ALPHA",
            "C/NEW STOP/2/BETA",
            "D/INTERMEDIATE STOP/2/BETA",
            "E/NEW STOP/3/GAMMA"
        ],
        "once switched, the route is what master's stages say"
    );
    assert_eq!(served(&ppool, "R2").await, served(&mpool, "R2").await);

    // nothing is left to bring
    let (s, b, _) = call!(&prod, import("dry_run=false"));
    assert_eq!(s, 200, "{b}");
    assert!(b["change_sets"].as_array().unwrap().is_empty(), "{b}");
    assert!(b["import_id"].is_null(), "{b}");

    // ======================================================== a single draft
    let m2 = draft!(
        &master,
        m_admin,
        "BETA reaches G",
        [
            json!({"entity": "stage", "op": "update", "entity_key": "stg_b|", "after": {
            "rows": stops(&[("C", "NEW STOP"), ("D", "INTERMEDIATE STOP"), ("G", "INTERMEDIATE STOP")])}})
        ]
    );
    ship!(&master, m_admin, m_ap, m2);
    // a draft not committed does not move
    let open = draft!(
        &master,
        m_admin,
        "not yet",
        [json!({"entity": "stop", "op": "update", "entity_key": "H", "after": {"name": "H"}})]
    );
    let (s, b, _) = call_bytes!(
        &master,
        m_admin.req("GET", &format!("/change-sets/{open}/migration"))
    );
    assert_eq!(s, 409, "{}", String::from_utf8_lossy(&b));
    let (s, file2, _) = call_bytes!(
        &master,
        m_admin.req("GET", &format!("/change-sets/{m2}/migration"))
    );
    assert_eq!(s, 200);
    let import2 = |q: &str, f: &Vec<u8>| {
        p_admin
            .req("POST", &format!("/feeds/{FEED}/migrations?{q}"))
            .set_payload(f.clone())
    };
    let (s, b, _) = call!(&prod, import2("dry_run=true", &file2));
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["counts"]["added"], 1, "{b}");
    assert!(b["conflicts"].as_array().unwrap().is_empty(), "{b}");
    let (s, b, _) = call!(&prod, import2("dry_run=false", &file2));
    assert_eq!(s, 200, "{b}");
    let set = b["change_sets"][0]["change_set_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        b["change_sets"][0]["title"]
            .as_str()
            .unwrap()
            .starts_with("From master: BETA reaches G"),
        "{b}"
    );
    ship!(&prod, p_admin, p_ap, set);
    assert!(served(&ppool, "R1")
        .await
        .contains(&"G/INTERMEDIATE STOP/2/BETA".to_string()));
    // the same draft again is refused while its prod draft stands
    let (s, b, _) = call!(&prod, import2("dry_run=false", &file2));
    assert_eq!((s, code_of(&b)), (409, "already_imported"), "{b}");

    // a draft that meets prod's own edit: left out until master's side is taken
    let m3 = draft!(
        &master,
        m_admin,
        "B renamed again",
        [
            json!({"entity": "stop", "op": "update", "entity_key": "B", "after": {"name": "STOP B M3"}})
        ]
    );
    ship!(&master, m_admin, m_ap, m3);
    let p2 = draft!(
        &prod,
        p_admin,
        "prod renames B",
        [
            json!({"entity": "stop", "op": "update", "entity_key": "B", "after": {"name": "STOP B PROD"}})
        ]
    );
    ship!(&prod, p_admin, p_ap, p2);
    let (_, file3, _) = call_bytes!(
        &master,
        m_admin.req("GET", &format!("/change-sets/{m3}/migration"))
    );
    let (s, b, _) = call!(&prod, import2("dry_run=false", &file3));
    assert_eq!(s, 200, "{b}");
    assert!(b["change_sets"].as_array().unwrap().is_empty(), "{b}");
    assert_eq!(ids(&b["conflicts"]), vec!["change:1".to_string()], "{b}");
    assert_eq!(b["conflicts"][0]["prod"]["name"], "STOP B PROD", "{b}");
    assert_eq!(b["conflicts"][0]["master"]["name"], "STOP B MASTER", "{b}");
    let (s, b, _) = call!(&prod, import2("dry_run=false&take=change:1", &file3));
    assert_eq!(s, 200, "{b}");
    let set = b["change_sets"][0]["change_set_id"]
        .as_str()
        .unwrap()
        .to_string();
    ship!(&prod, p_admin, p_ap, set);
    assert_eq!(
        scalar_text(
            &ppool,
            format!("SELECT name FROM gtfs_stop WHERE gtfs_id = '{FEED}' AND stop_id = 'B'")
        )
        .await,
        "STOP B M3"
    );

    // what was read in, newest first
    let (s, b, _) = call!(
        &prod,
        p_admin.req("GET", &format!("/feeds/{FEED}/migrations"))
    );
    assert_eq!(s, 200, "{b}");
    // the read-ins that drafted something: four steps (R2's list taken once
    // more), the switch, and two single drafts
    assert_eq!(b["items"].as_array().unwrap().len(), 7, "{b}");
    assert_eq!(b["items"][0]["kind"], "change_set", "{b}");

    exec(&mpool, &clear_feed()).await;
    exec(&ppool, &clear_feed()).await;
    let _ = std::fs::remove_dir_all(mdir);
    let _ = std::fs::remove_dir_all(pdir);
}
