//! HTTP handlers: parse, authorize, call the service, shape the response.

use super::auth::{self, Object, Role};
use super::bulk;
use super::context;
use super::crypto::{self, TotpCheck};
use super::error::{EditorError, EditorResult};
use super::feed_io;
use super::feed_lock;
use super::gps_line::{self, GpsFailure};
use super::position_reviews;
use super::proposals;
use super::records;
use super::service::{self as svc, Page, StopQuery};
use super::trips;
use super::validation::valid_lat_lon;
use super::webhooks;
use super::EditorState;
use crate::gtfs::write as gtfs_write;
use crate::services::osrm;
use actix_web::cookie::{time::Duration as CookieDuration, Cookie, SameSite};
use actix_web::error::{JsonPayloadError, PathError, QueryPayloadError};
use actix_web::http::StatusCode;
use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

type Data = web::Data<EditorState>;

pub fn json_error(err: JsonPayloadError, _req: &HttpRequest) -> actix_web::Error {
    EditorError::bad_request("invalid_json", format!("request body is not valid: {err}")).into()
}

pub fn query_error(err: QueryPayloadError, _req: &HttpRequest) -> actix_web::Error {
    EditorError::bad_request("invalid_query", format!("query string is not valid: {err}")).into()
}

pub fn path_error(err: PathError, _req: &HttpRequest) -> actix_web::Error {
    EditorError::bad_request("invalid_path", format!("path is not valid: {err}")).into()
}

pub async fn not_found() -> EditorResult<HttpResponse> {
    Err(EditorError::not_found(
        "endpoint_not_found",
        "no such editor endpoint",
    ))
}

fn ok(v: serde_json::Value) -> EditorResult<HttpResponse> {
    Ok(HttpResponse::Ok().json(v))
}

// ---------------------------------------------------------------- auth

fn session_cookie(token: String, hours: i64) -> Cookie<'static> {
    Cookie::build(auth::SESSION_COOKIE, token)
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Strict)
        .max_age(CookieDuration::hours(hours))
        .finish()
}

fn client_ip(req: &HttpRequest) -> Option<String> {
    let addr = req.peer_addr()?;
    Some(addr.ip().to_string())
}

async fn start_session(
    req: &HttpRequest,
    st: &EditorState,
    user: &auth::User,
) -> EditorResult<(String, chrono::DateTime<chrono::Utc>)> {
    let token = crypto::random_token();
    sqlx::query("DELETE FROM gtfs_editor_session WHERE expires_at < now() - interval '1 day'")
        .execute(&st.pool)
        .await?;
    let expires_at: chrono::DateTime<chrono::Utc> = sqlx::query(
        "INSERT INTO gtfs_editor_session (token_hash, user_id, expires_at, mfa_verified_at, last_seen_at, client_ip, user_agent) \
         VALUES ($1, $2, now() + make_interval(hours => $3), now(), now(), $4::inet, $5) RETURNING expires_at",
    )
    .bind(crypto::sha256(token.as_bytes()))
    .bind(user.user_id)
    .bind(st.session_hours as i32)
    .bind(client_ip(req))
    .bind(
        req.headers()
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.chars().take(300).collect::<String>()),
    )
    .fetch_one(&st.pool)
    .await?
    .try_get("expires_at")?;
    Ok((token, expires_at))
}

pub async fn me(req: HttpRequest, st: Data) -> EditorResult<HttpResponse> {
    let user = auth::active_user(&req, &st).await?;
    let session = user.totp_enabled && auth::session_user(&req, &st, &user).await?;
    let locked = auth::lockout_seconds(&st, user.user_id).await?;
    let feeds = svc::my_feeds(&st, &user).await?;
    ok(json!({
        "user_id": user.user_id,
        "email": user.email,
        "display_name": user.display_name,
        "role": user.role,
        "is_admin": user.is_admin(),
        "feeds": feeds,
        "status": user.status,
        "totp_enabled": user.totp_enabled,
        "session": session,
        "sign_in_locked_seconds": locked,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeBody {
    code: String,
}

fn locked_error(seconds: i64) -> EditorError {
    EditorError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "locked",
        format!(
            "too many wrong codes; try again in {} minute(s)",
            (seconds + 59) / 60
        ),
    )
    .with_details(json!({"retry_after_seconds": seconds}))
}

pub async fn totp_enroll(req: HttpRequest, st: Data) -> EditorResult<HttpResponse> {
    let user = auth::active_user(&req, &st).await?;
    if user.totp_enabled {
        return Err(EditorError::conflict(
            "totp_already_enabled",
            "an authenticator is already set up; ask an admin to reset it",
        ));
    }
    let secret = crypto::random_bytes(20);
    let sealed = st.secrets.seal(&secret, user.user_id.as_bytes());
    let n = sqlx::query(
        "UPDATE gtfs_editor_user SET totp_secret_enc = $2, totp_last_step = NULL \
         WHERE user_id = $1 AND NOT totp_enabled",
    )
    .bind(user.user_id)
    .bind(sealed)
    .execute(&st.pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(EditorError::conflict(
            "totp_already_enabled",
            "an authenticator is already set up",
        ));
    }
    auth::audit(
        &st.pool,
        Some(user.user_id),
        Some(&user.email),
        "totp_enroll_started",
        None,
        None,
        json!({}),
    )
    .await?;
    let b32 = crypto::base32_encode(&secret);
    let issuer = "GTFS Editor";
    let uri = format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
        urlencoding::encode(issuer),
        urlencoding::encode(&user.email),
        b32,
        urlencoding::encode(issuer),
    );
    ok(json!({"otpauth_uri": uri, "secret_base32": b32}))
}

fn open_secret(st: &EditorState, user: &auth::User) -> EditorResult<Vec<u8>> {
    let sealed = user.totp_secret_enc.as_ref().ok_or_else(|| {
        EditorError::conflict("totp_not_enrolled", "start authenticator setup first")
    })?;
    st.secrets
        .open(sealed, user.user_id.as_bytes())
        .ok_or_else(|| {
            EditorError::internal(
                "the stored authenticator secret cannot be read; ask an admin to reset it",
            )
        })
}

/// Record a wrong or replayed code; returns how many tries are left before
/// sign-in locks.
async fn record_failure(st: &EditorState, user: &auth::User, reason: &str) -> EditorResult<i64> {
    auth::audit(
        &st.pool,
        Some(user.user_id),
        Some(&user.email),
        "totp_failed",
        None,
        None,
        json!({"reason": reason}),
    )
    .await?;
    let failed = auth::failed_codes(st, user.user_id).await?;
    Ok((auth::MAX_FAILED_CODES - failed).max(0))
}

fn invalid_code(attempts_left: i64) -> EditorError {
    EditorError::unauthorized(
        "invalid_code",
        "that code is not right; check the time on your phone",
    )
    .with_details(json!({"attempts_left": attempts_left}))
}

fn code_reused(attempts_left: i64) -> EditorError {
    EditorError::unauthorized(
        "code_reused",
        "that code was already used; wait for the next one",
    )
    .with_details(json!({"attempts_left": attempts_left}))
}

pub async fn totp_confirm(
    req: HttpRequest,
    st: Data,
    body: web::Json<CodeBody>,
) -> EditorResult<HttpResponse> {
    let user = auth::active_user(&req, &st).await?;
    if user.totp_enabled {
        return Err(EditorError::conflict(
            "totp_already_enabled",
            "an authenticator is already set up",
        ));
    }
    if let Some(s) = auth::lockout_seconds(&st, user.user_id).await? {
        return Err(locked_error(s));
    }
    let secret = open_secret(&st, &user)?;
    let now = chrono::Utc::now().timestamp() as u64;
    let TotpCheck::Valid(step) = crypto::totp_check(&secret, &body.code, now, None) else {
        let left = record_failure(&st, &user, "invalid").await?;
        return Err(invalid_code(left));
    };
    let updated = sqlx::query(
        "UPDATE gtfs_editor_user SET totp_enabled = true, totp_last_step = $2, last_login_at = now() \
         WHERE user_id = $1 AND NOT totp_enabled RETURNING user_id",
    )
    .bind(user.user_id)
    .bind(step)
    .fetch_optional(&st.pool)
    .await?;
    if updated.is_none() {
        return Err(EditorError::conflict(
            "totp_already_enabled",
            "an authenticator is already set up",
        ));
    }
    let (token, expires_at) = start_session(&req, &st, &user).await?;
    auth::audit(
        &st.pool,
        Some(user.user_id),
        Some(&user.email),
        "totp_confirmed",
        None,
        None,
        json!({}),
    )
    .await?;
    Ok(HttpResponse::Ok()
        .cookie(session_cookie(token, st.session_hours))
        .json(json!({"totp_enabled": true, "expires_at": expires_at})))
}

pub async fn session_create(
    req: HttpRequest,
    st: Data,
    body: web::Json<CodeBody>,
) -> EditorResult<HttpResponse> {
    let user = auth::active_user(&req, &st).await?;
    if !user.totp_enabled {
        return Err(EditorError::unauthorized(
            "totp_enrollment_required",
            "set up your authenticator app first",
        ));
    }
    if let Some(s) = auth::lockout_seconds(&st, user.user_id).await? {
        return Err(locked_error(s));
    }
    let secret = open_secret(&st, &user)?;
    let now = chrono::Utc::now().timestamp() as u64;
    match crypto::totp_check(&secret, &body.code, now, user.totp_last_step) {
        TotpCheck::Valid(step) => {
            // conditional on the step: two pods racing the same code cannot both win
            let won = sqlx::query(
                "UPDATE gtfs_editor_user SET totp_last_step = $2, last_login_at = now() \
                 WHERE user_id = $1 AND (totp_last_step IS NULL OR totp_last_step < $2) RETURNING user_id",
            )
            .bind(user.user_id)
            .bind(step)
            .fetch_optional(&st.pool)
            .await?;
            if won.is_none() {
                let left = record_failure(&st, &user, "reused").await?;
                return Err(code_reused(left));
            }
            let (token, expires_at) = start_session(&req, &st, &user).await?;
            auth::audit(
                &st.pool,
                Some(user.user_id),
                Some(&user.email),
                "session_created",
                None,
                None,
                json!({}),
            )
            .await?;
            Ok(HttpResponse::Ok()
                .cookie(session_cookie(token, st.session_hours))
                .json(json!({"expires_at": expires_at})))
        }
        TotpCheck::Reused => {
            let left = record_failure(&st, &user, "reused").await?;
            Err(code_reused(left))
        }
        TotpCheck::Invalid => {
            let left = record_failure(&st, &user, "invalid").await?;
            Err(invalid_code(left))
        }
    }
}

pub async fn session_delete(req: HttpRequest, st: Data) -> EditorResult<HttpResponse> {
    auth::identity(&req)?;
    if let Some(c) = req.cookie(auth::SESSION_COOKIE) {
        sqlx::query("DELETE FROM gtfs_editor_session WHERE token_hash = $1")
            .bind(crypto::sha256(c.value().as_bytes()))
            .execute(&st.pool)
            .await?;
    }
    let mut gone = session_cookie(String::new(), 1);
    gone.make_removal();
    Ok(HttpResponse::NoContent().cookie(gone).finish())
}

// ---------------------------------------------------------------- reads

/// The GTFS reference as data (section 18): every file and field, what each
/// may hold, what it points at and where the editor keeps it. The dashboard
/// builds its file list and forms from this.
pub async fn gtfs_spec(req: HttpRequest, st: Data) -> EditorResult<HttpResponse> {
    auth::signed_in(&req, &st).await?;
    ok(crate::gtfs::spec::spec_json())
}

pub async fn feeds(req: HttpRequest, st: Data) -> EditorResult<HttpResponse> {
    let ctx = auth::signed_in(&req, &st).await?;
    ok(svc::feeds(&st, &ctx).await?)
}

// ---------------------------------------------------------------- GTFS files (section 18)

#[derive(Deserialize)]
pub struct FilesQuery {
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

/// The record file `file` names (`pathways.txt` or `pathways`); a file the
/// editor keeps in tables of its own is read through its own endpoints.
fn record_file(file: &str) -> EditorResult<&'static crate::gtfs::spec::FileSpec> {
    let fspec = crate::gtfs::spec::file(file).ok_or_else(|| {
        EditorError::not_found(
            "file_not_found",
            format!("{file} is not a file of the GTFS reference"),
        )
    })?;
    if fspec.table().is_none() {
        return Err(EditorError::bad_request(
            "not_a_record_file",
            format!(
                "{} is kept in the editor's own tables: read it through /stops, /routes, /routes/{{id}}/trips or /services",
                fspec.name
            ),
        ));
    }
    Ok(fspec)
}

/// Every file of the reference, with how many rows the feed has in each.
pub async fn files(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    ok(records::list_files(&mut conn, &path).await?)
}

/// A record file's rows, paged, `q` matching any of their text.
pub async fn file_rows(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
    q: web::Query<FilesQuery>,
) -> EditorResult<HttpResponse> {
    let (g, file) = path.into_inner();
    auth::require_feed(&req, &st, &g, Role::Viewer).await?;
    let fspec = record_file(&file)?;
    let page = Page::parse(q.limit, q.cursor.as_deref())?;
    let mut conn = st.pool.acquire().await?;
    ok(records::list_records(&mut conn, &g, fspec, q.q.as_deref(), &page).await?)
}

/// One record, with what points at it.
pub async fn file_row(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String, String)>,
) -> EditorResult<HttpResponse> {
    let (g, file, key) = path.into_inner();
    auth::require_feed(&req, &st, &g, Role::Viewer).await?;
    let fspec = record_file(&file)?;
    let mut conn = st.pool.acquire().await?;
    ok(records::record_detail(&mut conn, &g, fspec, &key).await?)
}

/// A record file's rows with a draft applied.
pub async fn change_set_preview_file(
    req: HttpRequest,
    st: Data,
    path: web::Path<(Uuid, String)>,
    q: web::Query<FilesQuery>,
) -> EditorResult<HttpResponse> {
    let (id, file) = path.into_inner();
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(id), Role::Viewer).await?;
    let fspec = record_file(&file)?;
    let page = Page::parse(q.limit, q.cursor.as_deref())?;
    ok(svc::preview_records(&st, &ctx, id, fspec, q.q.as_deref(), &page).await?)
}

/// A feed's whole GTFS as the zip it publishes, from the tables (section 18).
pub async fn feed_gtfs_zip(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    let g = path.into_inner();
    auth::require_feed(&req, &st, &g, Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    let (m, findings) = feed_io::load_model(&mut conn, &g).await?;
    let bytes = gtfs_write::zip_bytes(&gtfs_write::to_raw(&m))
        .map_err(|e| EditorError::new(StatusCode::INTERNAL_SERVER_ERROR, "export_failed", e))?;
    Ok(HttpResponse::Ok()
        .content_type("application/zip")
        .insert_header((
            "Content-Disposition",
            format!("attachment; filename=\"{g}.gtfs.zip\""),
        ))
        // values the tables hold that the reference does not accept, left out
        .insert_header(("X-Export-Findings", findings.len().to_string()))
        .body(bytes))
}

/// What the feed breaks of the GTFS reference as it stands (section 18): the
/// report its zip would get, counted by kind.
pub async fn feed_validation(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    let g = path.into_inner();
    auth::require_feed(&req, &st, &g, Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    let (m, mut findings) = feed_io::load_model(&mut conn, &g).await?;
    let today = chrono::Utc::now().date_naive().to_string();
    findings.extend(crate::gtfs::validate::validate(&m, &today));
    ok(json!({
        "gtfs_id": g,
        "today": today,
        "counts": m.counts(),
        "report": crate::gtfs::validate::summarise(&findings, 20),
    }))
}

#[derive(Deserialize)]
pub struct ImportQuery {
    /// `true` writes a seed; anything else is a dry run.
    pub seed: Option<bool>,
    /// `drafts`: bring the zip into a feed that has rows, through change sets
    /// (section 18); `seed` (the default): load a feed that has none.
    pub mode: Option<String>,
    /// mode `drafts`: `false` writes the change sets; a dry run otherwise.
    pub dry_run: Option<bool>,
    /// mode `drafts`: the files it may write, comma separated.
    pub files: Option<String>,
}

/// Load a feed that has no rows yet from its GTFS zip, the request's body
/// (section 18). Admin only: it writes a whole feed without a draft, which is
/// allowed only while there is nothing a draft could have been based on. With
/// `mode=drafts` it instead drafts change sets that bring the zip into a feed
/// that has rows, for review like any other.
pub async fn feed_import(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    q: web::Query<ImportQuery>,
    body: web::Bytes,
) -> EditorResult<HttpResponse> {
    let g = path.into_inner();
    let ctx = auth::require_admin(&req, &st).await?;
    let who = feed_io::Importer {
        user_id: Some(ctx.user.user_id),
        email: Some(ctx.user.email.clone()),
        label: ctx.user.email.clone(),
    };
    match q.mode.as_deref() {
        None | Some("seed") => {}
        Some("drafts") => {
            let opts = super::draft_import::DraftImport {
                files: q
                    .files
                    .as_deref()
                    .map(|f| f.split(',').map(str::to_string).collect()),
                dry_run: q.dry_run.unwrap_or(true),
                user_id: ctx.user.user_id,
                email: ctx.user.email.clone(),
            };
            let report =
                super::draft_import::draft_import(&st.pool, &body, Some(&g), &opts).await?;
            return ok(json!(report));
        }
        Some(other) => {
            return Err(EditorError::bad_request(
                "invalid_mode",
                format!("mode {other:?} is not seed or drafts"),
            ))
        }
    }
    let dry_run = !q.seed.unwrap_or(false);
    let report = feed_io::import_zip(&st.pool, &body, Some(&g), dry_run, &who).await?;
    ok(json!(report))
}

pub async fn feed_config(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    ok(svc::feed_config(&st, &path).await?)
}

#[derive(Deserialize)]
pub struct StopsQuery {
    q: Option<String>,
    bbox: Option<String>,
    station: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

fn parse_bbox(s: &str) -> EditorResult<(f64, f64, f64, f64)> {
    let v: Vec<f64> = s
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| {
            EditorError::bad_request("invalid_bbox", "bbox is minLat,minLon,maxLat,maxLon")
        })?;
    match v.as_slice() {
        [a, b, c, d] if a <= c && b <= d => Ok((*a, *b, *c, *d)),
        _ => Err(EditorError::bad_request(
            "invalid_bbox",
            "bbox is minLat,minLon,maxLat,maxLon",
        )),
    }
}

pub async fn stops(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    q: web::Query<StopsQuery>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    let page = Page::parse(q.limit, q.cursor.as_deref())?;
    let query = StopQuery {
        q: q.q.clone(),
        bbox: q.bbox.as_deref().map(parse_bbox).transpose()?,
        station: q.station.clone().filter(|s| !s.is_empty()),
    };
    ok(svc::list_stops(&st, &path, &query, &page).await?)
}

pub async fn stop(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
) -> EditorResult<HttpResponse> {
    let (g, id) = path.into_inner();
    auth::require_feed(&req, &st, &g, Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    ok(svc::stop_detail(&mut conn, &g, &id).await?)
}

#[derive(Deserialize)]
pub struct RoutesQuery {
    q: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

pub async fn routes(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    q: web::Query<RoutesQuery>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    let page = Page::parse(q.limit, q.cursor.as_deref())?;
    ok(svc::list_routes(&st, &path, q.q.as_deref(), &page).await?)
}

pub async fn route(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
) -> EditorResult<HttpResponse> {
    let (g, id) = path.into_inner();
    auth::require_feed(&req, &st, &g, Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    ok(svc::route_detail(&mut conn, &g, &id).await?)
}

/// What a person cleaning up a stop wants beside it (docs section 9).
pub async fn stop_context(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
) -> EditorResult<HttpResponse> {
    let (g, id) = path.into_inner();
    auth::require_feed(&req, &st, &g, Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    ok(context::stop(&mut conn, &g, &id).await?)
}

pub async fn route_context(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
) -> EditorResult<HttpResponse> {
    let (g, id) = path.into_inner();
    auth::require_feed(&req, &st, &g, Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    ok(context::route(&mut conn, &g, &id).await?)
}

#[derive(Deserialize)]
pub struct PolylineQuery {
    change_set: Option<Uuid>,
}

/// How long a suggestion through the stops may take, all chunks together.
const OSRM_ROUTE_BUDGET: std::time::Duration = std::time::Duration::from_secs(25);

async fn route_for_line(
    st: &EditorState,
    ctx: &auth::Ctx,
    g: &str,
    route_id: &str,
    change_set: Option<Uuid>,
) -> EditorResult<serde_json::Value> {
    match change_set {
        Some(id) => {
            also_on_set(st, ctx, id, Role::Editor).await?;
            svc::preview_route(st, ctx, id, route_id).await
        }
        None => {
            let mut conn = st.pool.acquire().await?;
            svc::route_detail(&mut conn, g, route_id).await
        }
    }
}

/// "KOYAMBEDU (stop ab12, row 7)" - a waypoint as a person finds it.
fn describe_waypoint(w: &svc::Waypoint) -> String {
    let row = w.sequence.map(|q| format!(", row {q}")).unwrap_or_default();
    let id = w.id.as_deref().unwrap_or("?");
    match (w.marker, w.name.as_deref().filter(|n| !n.is_empty())) {
        (true, Some(n)) => format!("the shaping marker {n} ({id}{row})"),
        (true, None) => format!("a shaping marker ({id}{row})"),
        (false, Some(n)) => format!("{n} (stop {id}{row})"),
        (false, None) => format!("stop {id}{row}"),
    }
}

/// The 502 for a road route OSRM would not give, saying why and where.
fn osrm_failure(f: &osrm::RouteFailure, waypoints: &[svc::Waypoint]) -> EditorError {
    use osrm::FailReason as R;
    let wp = |i: Option<usize>| i.and_then(|i| waypoints.get(i));
    let (from, to) = match (f.leg, f.waypoint) {
        (Some(l), _) => (wp(Some(l)), wp(Some(l + 1))),
        _ => (None, None),
    };
    let snapped = wp(f.waypoint);
    let why = match (f.reason, snapped, from, to) {
        (R::NoSegment, Some(w), _, _) => format!("there is no road near {}", describe_waypoint(w)),
        (R::NoSegment, None, _, _) => "one of the stops is not near any road".to_string(),
        (R::NoRoute, _, Some(a), Some(b)) => format!(
            "there is no road route from {} to {}",
            describe_waypoint(a),
            describe_waypoint(b)
        ),
        (R::NoRoute, _, _, _) => "there is no road route through them".to_string(),
        (R::Timeout, _, _, _) => "OSRM did not answer in time".to_string(),
        (R::Unreachable, _, _, _) => "OSRM could not be reached".to_string(),
        (R::HttpError, _, _, _) => format!("OSRM refused the request ({})", f.message),
    };
    let id = |w: Option<&svc::Waypoint>| w.and_then(|w| w.id.clone());
    EditorError::new(
        StatusCode::BAD_GATEWAY,
        "osrm_failed",
        format!("OSRM could not route through these stops: {why}"),
    )
    .with_details(json!({
        "reason": f.reason,
        "osrm_code": f.osrm_code,
        "message": f.message,
        "leg": f.leg,
        "from_stop_id": id(from),
        "to_stop_id": id(to),
        "from_sequence": from.and_then(|w| w.sequence),
        "to_sequence": to.and_then(|w| w.sequence),
        "from_name": from.and_then(|w| w.name.clone()),
        "to_name": to.and_then(|w| w.name.clone()),
        "waypoint": f.waypoint,
        "stop_id": id(snapped),
        "sequence": snapped.and_then(|w| w.sequence),
        "stop_name": snapped.and_then(|w| w.name.clone()),
    }))
}

pub async fn polyline_osrm(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
    q: web::Query<PolylineQuery>,
) -> EditorResult<HttpResponse> {
    let (g, route_id) = path.into_inner();
    let ctx = auth::require_feed(&req, &st, &g, Role::Editor).await?;
    let detail = route_for_line(&st, &ctx, &g, &route_id, q.change_set).await?;
    let waypoints = svc::polyline_waypoint_rows(&detail);
    let Some(base) = st.osrm_url.as_deref().filter(|u| !u.is_empty()) else {
        return Err(EditorError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "osrm_unavailable",
            "no OSRM server is configured",
        ));
    };
    let points: Vec<(f64, f64)> = waypoints.iter().map(|w| (w.lat, w.lon)).collect();
    let line = osrm::route_through(&st.http, base, &points, OSRM_ROUTE_BUDGET)
        .await
        .map_err(|f| osrm_failure(&f, &waypoints))?;
    let distance: f64 = line.legs.iter().map(|(d, _)| d).sum();
    ok(json!({
        "route_id": route_id,
        "encoded_polyline": osrm::encode_polyline(&line.points),
        "polyline_source": "osrm",
        "waypoints": points.len(),
        "distance_m": distance.round(),
        "saved": false,
    }))
}

/// A map line from the buses' own GPS (docs section 17): not saved, like the
/// OSRM one - the dashboard adds it to the draft as a `route` change.
pub async fn polyline_gps(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
    q: web::Query<PolylineQuery>,
) -> EditorResult<HttpResponse> {
    let (g, route_id) = path.into_inner();
    let ctx = auth::require_feed(&req, &st, &g, Role::Editor).await?;
    let unavailable =
        |why: String| EditorError::new(StatusCode::SERVICE_UNAVAILABLE, "gps_unavailable", why);
    let Some(gps) = st.gps_line.clone() else {
        return Err(unavailable(
            "map lines from GPS are not set up on this server".into(),
        ));
    };
    if !gps.serves(&g) {
        return Err(unavailable(format!("there are no GPS pings for feed {g}")));
    }
    let detail = route_for_line(&st, &ctx, &g, &route_id, q.change_set).await?;
    let stops = gps_line::served_stops(&detail);
    let short_name = detail["short_name"].as_str().unwrap_or("").to_string();
    let asked = gps_line::RouteQuery {
        gtfs_id: &g,
        route_id: &route_id,
        short_name: &short_name,
        stops: &stops,
    };
    match gps.suggest(st.osrm_url.as_deref(), asked).await {
        Ok(v) => ok(v),
        Err(GpsFailure::NoRouteNumber) => Err(EditorError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "gps_no_route_number",
            "this route has no route number to find its buses by",
        )),
        Err(GpsFailure::NotEnoughStops(n)) => Err(EditorError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "gps_not_enough_stops",
            format!("the route has {n} stop(s) with a position; a line from GPS needs two"),
        )
        .with_details(json!({"stops": n}))),
        Err(GpsFailure::NotEnoughRuns(counts)) => {
            let used = counts["runs_used"].as_u64().unwrap_or(0);
            let seen = counts["runs_seen"].as_u64().unwrap_or(0);
            let need = counts["min_runs"].as_u64().unwrap_or(0);
            let days = counts["days_read"].as_u64().unwrap_or(0);
            let mut message = format!(
                "in the last {days} day(s) {used} of {seen} bus runs labelled {} passed this route's \
                 stops in order; a line needs at least {need}",
                short_name.trim()
            );
            if counts["stopped"] == "budget" {
                message.push_str(
                    ". Reading stopped early to answer in time; asking again usually reads further",
                );
            }
            Err(EditorError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "gps_not_enough_runs",
                message,
            )
            .with_details(counts))
        }
        Err(GpsFailure::Timeout) => Err(EditorError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "gps_timeout",
            "reading the GPS pings took too long; try again in a minute",
        )),
        Err(GpsFailure::Query(why)) => {
            tracing::error!(tag = "[GTFS EDITOR GPS]", error = %why);
            Err(EditorError::new(
                StatusCode::BAD_GATEWAY,
                "gps_query_failed",
                format!("the GPS pings could not be read: {why}"),
            ))
        }
        Err(GpsFailure::Internal(why)) => {
            tracing::error!(tag = "[GTFS EDITOR GPS]", error = %why);
            Err(EditorError::internal("the GPS query was refused"))
        }
    }
}

#[derive(Deserialize)]
pub struct AuditQuery {
    change_set: Option<Uuid>,
    limit: Option<i64>,
    cursor: Option<String>,
}

pub async fn audit(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    q: web::Query<AuditQuery>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    let page = Page::parse(q.limit, q.cursor.as_deref())?;
    ok(svc::audit_list(&st, &path, q.change_set, &page).await?)
}

// ---------------------------------------------------------------- drafts

#[derive(Deserialize)]
pub struct SetsQuery {
    status: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

pub async fn change_sets(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    q: web::Query<SetsQuery>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    let page = Page::parse(q.limit, q.cursor.as_deref())?;
    ok(svc::list_sets(
        &st,
        &path,
        q.status.as_deref().filter(|s| !s.is_empty()),
        &page,
    )
    .await?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSet {
    title: String,
    #[serde(default)]
    description: Option<String>,
}

pub async fn change_set_create(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    body: web::Json<NewSet>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_feed(&req, &st, &path, Role::Editor).await?;
    let set = svc::create_set(&st, &ctx, &path, &body.title, body.description.as_deref()).await?;
    Ok(HttpResponse::Created().json(set))
}

/// A change set's changes come a page at a time (section 16.5, "Big drafts").
#[derive(Deserialize)]
pub struct ChangesQuery {
    limit: Option<i64>,
    cursor: Option<String>,
}

pub async fn change_set(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
    q: web::Query<ChangesQuery>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(*path), Role::Viewer).await?;
    let first = q.cursor.as_deref().is_none_or(str::is_empty);
    let page = Page::parse(
        Some(q.limit.unwrap_or(svc::CHANGES_PAGE)),
        q.cursor.as_deref(),
    )?;
    ok(svc::set_detail_page(&st, &ctx, *path, &page, first).await?)
}

pub async fn change_add(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
    body: web::Json<svc::NewChange>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(*path), Role::Editor).await?;
    let change_id = svc::add_change(&st, &ctx, *path, body.into_inner()).await?;
    let mut detail = svc::set_detail(&st, &ctx, *path).await?;
    detail["change_id"] = json!(change_id);
    Ok(HttpResponse::Created().json(detail))
}

pub async fn change_update(
    req: HttpRequest,
    st: Data,
    path: web::Path<(Uuid, i64)>,
    body: web::Json<svc::ChangeUpdate>,
) -> EditorResult<HttpResponse> {
    let (id, change_id) = path.into_inner();
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(id), Role::Editor).await?;
    svc::update_change(&st, &ctx, id, change_id, body.into_inner()).await?;
    ok(svc::set_detail(&st, &ctx, id).await?)
}

pub async fn change_delete(
    req: HttpRequest,
    st: Data,
    path: web::Path<(Uuid, i64)>,
) -> EditorResult<HttpResponse> {
    let (id, change_id) = path.into_inner();
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(id), Role::Editor).await?;
    svc::delete_change(&st, &ctx, id, change_id).await?;
    ok(svc::set_detail(&st, &ctx, id).await?)
}

pub async fn change_set_preview_route(
    req: HttpRequest,
    st: Data,
    path: web::Path<(Uuid, String)>,
) -> EditorResult<HttpResponse> {
    let (id, route_id) = path.into_inner();
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(id), Role::Viewer).await?;
    ok(svc::preview_route(&st, &ctx, id, &route_id).await?)
}

pub async fn submit(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(*path), Role::Editor).await?;
    svc::submit(&st, &ctx, *path).await?;
    ok(svc::set_detail(&st, &ctx, *path).await?)
}

pub async fn reopen(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(*path), Role::Editor).await?;
    svc::reopen(&st, &ctx, *path).await?;
    ok(svc::set_detail(&st, &ctx, *path).await?)
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ReviewBody {
    #[serde(default)]
    comment: Option<String>,
    /// Approve only: an admin approving a set they submitted says so here.
    #[serde(default)]
    self_approve: bool,
}

pub async fn approve(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
    body: Option<web::Json<ReviewBody>>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(*path), Role::Approver).await?;
    let body = body.map(|b| b.into_inner()).unwrap_or_default();
    svc::review(
        &st,
        &ctx,
        *path,
        true,
        body.comment.as_deref(),
        body.self_approve,
    )
    .await?;
    ok(svc::set_detail(&st, &ctx, *path).await?)
}

pub async fn reject(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
    body: web::Json<ReviewBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(*path), Role::Approver).await?;
    svc::review(&st, &ctx, *path, false, body.comment.as_deref(), false).await?;
    ok(svc::set_detail(&st, &ctx, *path).await?)
}

pub async fn commit(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(*path), Role::Approver).await?;
    ok(svc::commit(&st, &ctx, *path).await?)
}

pub async fn discard(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(*path), Role::Editor).await?;
    svc::discard(&st, &ctx, *path).await?;
    ok(svc::set_detail(&st, &ctx, *path).await?)
}

pub async fn bulk_import(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
    body: web::Json<bulk::BulkRequest>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(*path), Role::Editor).await?;
    ok(bulk::run(&st, &ctx, *path, body.into_inner()).await?)
}

// ---------------------------------------------------------------- timetable (section 16)

/// A route's other stop orders read like its stop list: the route detail with
/// pattern `k`'s rows.
pub async fn route_pattern(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String, i16)>,
) -> EditorResult<HttpResponse> {
    let (g, id, pattern) = path.into_inner();
    auth::require_feed(&req, &st, &g, Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    ok(svc::route_pattern_detail(&mut conn, &g, &id, pattern).await?)
}

pub async fn route_trips(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
) -> EditorResult<HttpResponse> {
    let (g, id) = path.into_inner();
    auth::require_feed(&req, &st, &g, Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    ok(trips::route_trips_detail(&mut conn, &g, &id).await?)
}

pub async fn services(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    ok(trips::list_services(&mut conn, &path).await?)
}

pub async fn change_set_preview_pattern(
    req: HttpRequest,
    st: Data,
    path: web::Path<(Uuid, String, i16)>,
) -> EditorResult<HttpResponse> {
    let (id, route_id, pattern) = path.into_inner();
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(id), Role::Viewer).await?;
    ok(svc::preview_route_as(
        &st,
        &ctx,
        id,
        &route_id,
        svc::RoutePreview::Pattern(pattern),
    )
    .await?)
}

pub async fn change_set_preview_trips(
    req: HttpRequest,
    st: Data,
    path: web::Path<(Uuid, String)>,
) -> EditorResult<HttpResponse> {
    let (id, route_id) = path.into_inner();
    let ctx = auth::require_object(&req, &st, Object::ChangeSet(id), Role::Viewer).await?;
    ok(svc::preview_route_as(&st, &ctx, id, &route_id, svc::RoutePreview::Trips).await?)
}

// ---------------------------------------------------------------- station proposals

/// The query of a review list: station proposals and position reviews.
#[derive(Deserialize)]
pub struct ProposalsQuery {
    status: Option<String>,
    bbox: Option<String>,
    q: Option<String>,
    /// position reviews only: what the advisory tool suggests (section 8.2)
    auto_fix: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

impl ProposalsQuery {
    fn filters(&self) -> EditorResult<proposals::ListQuery> {
        Ok(proposals::ListQuery {
            status: self.status.clone(),
            bbox: self
                .bbox
                .as_deref()
                .filter(|b| !b.is_empty())
                .map(parse_bbox)
                .transpose()?,
            q: self.q.clone(),
        })
    }
}

pub async fn station_proposals(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    q: web::Query<ProposalsQuery>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    let page = Page::parse(q.limit, q.cursor.as_deref())?;
    ok(proposals::list(&st, &path, &q.filters()?, &page).await?)
}

pub async fn station_proposal_summary(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    ok(proposals::summary(&st, &path).await?)
}

pub async fn station_proposal(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
) -> EditorResult<HttpResponse> {
    auth::require_object(&req, &st, Object::StationProposal(*path), Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    ok(proposals::detail(&mut conn, *path).await?)
}

pub async fn station_proposal_approve(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
    body: web::Json<proposals::ApproveBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::StationProposal(*path), Role::Editor).await?;
    also_on_set(&st, &ctx, body.change_set_id, Role::Editor).await?;
    ok(proposals::approve(&st, &ctx, *path, body.into_inner()).await?)
}

pub async fn station_proposals_approve(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    body: web::Json<proposals::BulkApproveBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_feed(&req, &st, &path, Role::Editor).await?;
    also_on_set(&st, &ctx, body.change_set_id, Role::Editor).await?;
    ok(proposals::approve_many(&st, &ctx, &path, body.into_inner()).await?)
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NoteBody {
    #[serde(default)]
    note: Option<String>,
}

pub async fn station_proposal_reject(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
    body: Option<web::Json<NoteBody>>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::StationProposal(*path), Role::Editor).await?;
    let body = body.map(|b| b.into_inner()).unwrap_or_default();
    ok(proposals::reject(&st, &ctx, *path, body.note.as_deref()).await?)
}

pub async fn station_proposal_reopen(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::StationProposal(*path), Role::Editor).await?;
    ok(proposals::reopen(&st, &ctx, *path).await?)
}

// ---------------------------------------------------------------- position reviews

pub async fn position_reviews(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    q: web::Query<ProposalsQuery>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    let page = Page::parse(q.limit, q.cursor.as_deref())?;
    ok(position_reviews::list(&st, &path, &q.filters()?, q.auto_fix.as_deref(), &page).await?)
}

pub async fn position_review_summary(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    ok(position_reviews::summary(&st, &path).await?)
}

/// `lat` and `lon` ask what detour a point would give, over the routes in
/// `route_ids` (a comma list) or over every route. `stop_id` asks instead what
/// merging the reviewed stop into that stop would give, and what the merge's
/// validation says - on top of draft `change_set`, when given.
#[derive(Deserialize)]
pub struct ReviewDetailQuery {
    lat: Option<f64>,
    lon: Option<f64>,
    route_ids: Option<String>,
    stop_id: Option<String>,
    change_set: Option<Uuid>,
}

pub async fn position_review(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
    q: web::Query<ReviewDetailQuery>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::PositionReview(*path), Role::Viewer).await?;
    if let Some(id) = q.change_set {
        also_on_set(&st, &ctx, id, Role::Viewer).await?;
    }
    let route_ids = q.route_ids.as_deref().map(|ids| {
        ids.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    let into = q
        .stop_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let what_if = match (q.lat, q.lon, into) {
        (None, None, Some(stop_id)) if route_ids.is_none() => {
            Some(position_reviews::WhatIf::MergeInto {
                stop_id: stop_id.to_string(),
                change_set: q.change_set,
            })
        }
        (_, _, Some(_)) => {
            return Err(EditorError::bad_request(
                "invalid_query",
                "stop_id asks about a merge and is sent without lat, lon or route_ids",
            ))
        }
        (None, None, None) if route_ids.is_none() && q.change_set.is_none() => None,
        (Some(lat), Some(lon), None)
            if valid_lat_lon(lat, lon) && (lat, lon) != (0.0, 0.0) && q.change_set.is_none() =>
        {
            Some(position_reviews::WhatIf::Point {
                at: (lat, lon),
                route_ids,
            })
        }
        _ => return Err(EditorError::bad_request(
            "invalid_position",
            "lat and lon are sent together, with route_ids if any, and must be a valid position",
        )),
    };
    // a what-if merge replays the draft in a transaction: retried like every
    // other replay should that transaction hit a serialization failure
    let detail = feed_lock::retry_transient(|| async {
        let mut conn = st.pool.acquire().await?;
        position_reviews::detail(&mut conn, *path, what_if.as_ref(), &ctx.user.email).await
    })
    .await?;
    ok(detail)
}

pub async fn position_review_move(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
    body: web::Json<position_reviews::MoveBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::PositionReview(*path), Role::Editor).await?;
    also_on_set(&st, &ctx, body.change_set_id, Role::Editor).await?;
    ok(position_reviews::move_stop(&st, &ctx, *path, body.into_inner()).await?)
}

pub async fn position_review_split(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
    body: web::Json<position_reviews::SplitBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::PositionReview(*path), Role::Editor).await?;
    also_on_set(&st, &ctx, body.change_set_id, Role::Editor).await?;
    ok(position_reviews::split(&st, &ctx, *path, body.into_inner()).await?)
}

pub async fn position_review_merge(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
    body: web::Json<position_reviews::MergeBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::PositionReview(*path), Role::Editor).await?;
    also_on_set(&st, &ctx, body.change_set_id, Role::Editor).await?;
    ok(position_reviews::merge(&st, &ctx, *path, body.into_inner()).await?)
}

pub async fn position_review_confirm(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
    body: Option<web::Json<NoteBody>>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::PositionReview(*path), Role::Editor).await?;
    let body = body.map(|b| b.into_inner()).unwrap_or_default();
    ok(position_reviews::confirm(&st, &ctx, *path, body.note.as_deref()).await?)
}

pub async fn position_review_reopen(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::PositionReview(*path), Role::Editor).await?;
    ok(position_reviews::reopen(&st, &ctx, *path).await?)
}

// ---------------------------------------------------------------- admin

pub async fn users(req: HttpRequest, st: Data) -> EditorResult<HttpResponse> {
    auth::require_admin(&req, &st).await?;
    ok(svc::users(&st).await?)
}

/// `admin` makes an admin, `feeds` lets a member into feeds at once. The older
/// `{role}` body still works: `role: "admin"` is `admin: true`, any other role a
/// member with no grants (docs/gtfs-editor.md section 15.3).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewUser {
    email: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    admin: Option<bool>,
    #[serde(default)]
    feeds: Vec<svc::NewGrant>,
    #[serde(default)]
    role: Option<String>,
}

pub async fn user_create(
    req: HttpRequest,
    st: Data,
    body: web::Json<NewUser>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_admin(&req, &st).await?;
    let user = svc::create_user(
        &st,
        &ctx,
        &body.email,
        body.display_name.as_deref(),
        body.admin,
        body.role.as_deref(),
        &body.feeds,
    )
    .await?;
    Ok(HttpResponse::Created().json(user))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserPatch {
    #[serde(default)]
    admin: Option<bool>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

pub async fn user_update(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
    body: web::Json<UserPatch>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_admin(&req, &st).await?;
    svc::update_user(
        &st,
        &ctx,
        *path,
        body.admin,
        body.role.as_deref(),
        body.status.as_deref(),
    )
    .await?;
    ok(svc::user_item(&st, *path).await?)
}

pub async fn user_reset_totp(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_admin(&req, &st).await?;
    svc::reset_totp(&st, &ctx, *path).await?;
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantBody {
    role: String,
}

/// Create or change a member's grant on a feed; takes effect on their next
/// request.
pub async fn user_feed_put(
    req: HttpRequest,
    st: Data,
    path: web::Path<(Uuid, String)>,
    body: web::Json<GrantBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_admin(&req, &st).await?;
    let (user_id, g) = path.into_inner();
    svc::set_feed_access(&st, &ctx, user_id, &g, &body.role).await?;
    ok(svc::user_item(&st, user_id).await?)
}

pub async fn user_feed_delete(
    req: HttpRequest,
    st: Data,
    path: web::Path<(Uuid, String)>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_admin(&req, &st).await?;
    let (user_id, g) = path.into_inner();
    svc::revoke_feed_access(&st, &ctx, user_id, &g).await?;
    Ok(HttpResponse::NoContent().finish())
}

/// A change set named in a body or a query beside the object of the path: the
/// caller needs `min` on its feed as well, so an id from a feed they cannot see
/// answers `no_feed_access` - never a `feed_mismatch` that names that feed.
async fn also_on_set(st: &EditorState, ctx: &auth::Ctx, id: Uuid, min: Role) -> EditorResult<()> {
    let g = auth::feed_of(&st.pool, Object::ChangeSet(id)).await?;
    ctx.require_feed_role(&g, min)
}

// ---------------------------------------------------------------- webhooks

/// Which version of the feed each pod is serving, and whether the fleet has
/// caught up with the committed one (docs/gtfs-editor.md section 12). Readable
/// by anyone who can sign in: it answers "is my edit live yet".
pub async fn cache_state(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    ok(webhooks::cache_state(&st, &path).await?)
}

/// The webhook policy in force. Readable by anyone who can sign in: the
/// allow-list is the reason a URL is refused, and a viewer already sees that
/// refusal.
pub async fn webhook_settings(req: HttpRequest, st: Data) -> EditorResult<HttpResponse> {
    auth::signed_in(&req, &st).await?;
    ok(webhooks::settings_get(&st).await?)
}

pub async fn webhook_settings_update(
    req: HttpRequest,
    st: Data,
    body: web::Json<webhooks::SettingsBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_admin(&req, &st).await?;
    ok(webhooks::settings_update(&st, &ctx, body.into_inner()).await?)
}

pub async fn webhook_list(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    ok(webhooks::list(&st, &path).await?)
}

pub async fn webhook_create(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    body: web::Json<webhooks::WebhookBody>,
) -> EditorResult<HttpResponse> {
    // a webhook calls out of the cluster: an admin's, on whichever feed
    let ctx = auth::require_feed(&req, &st, &path, Role::Admin).await?;
    let out = webhooks::create(&st, &ctx, &path, body.into_inner()).await?;
    Ok(HttpResponse::Created().json(out))
}

pub async fn webhook_update(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
    body: web::Json<webhooks::WebhookBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::Webhook(*path), Role::Admin).await?;
    ok(webhooks::update(&st, &ctx, *path, body.into_inner()).await?)
}

pub async fn webhook_delete(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::Webhook(*path), Role::Admin).await?;
    ok(webhooks::delete(&st, &ctx, *path).await?)
}

pub async fn webhook_test(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require_object(&req, &st, Object::Webhook(*path), Role::Admin).await?;
    ok(webhooks::test(&st, &ctx, *path).await?)
}

/// The Release to Nandi button (docs/gtfs-editor.md section 12.6).
pub async fn feed_release(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    // an approver on the feed, or an admin (section 15)
    let ctx = auth::require_feed(&req, &st, &path, Role::Approver).await?;
    ok(webhooks::release(&st, &ctx, &path).await?)
}

#[derive(Deserialize)]
pub struct DeliveriesQuery {
    #[serde(default)]
    limit: Option<i64>,
}

pub async fn webhook_deliveries(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    q: web::Query<DeliveriesQuery>,
) -> EditorResult<HttpResponse> {
    auth::require_feed(&req, &st, &path, Role::Viewer).await?;
    ok(webhooks::deliveries(&st, &path, q.limit.unwrap_or(50)).await?)
}
