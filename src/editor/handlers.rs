//! HTTP handlers: parse, authorize, call the service, shape the response.

use super::auth::{self, Role};
use super::bulk;
use super::context;
use super::crypto::{self, TotpCheck};
use super::error::{EditorError, EditorResult};
use super::feed_lock;
use super::position_reviews;
use super::proposals;
use super::service::{self as svc, Page, StopQuery};
use super::trips;
use super::validation::valid_lat_lon;
use super::EditorState;
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
    ok(json!({
        "user_id": user.user_id,
        "email": user.email,
        "display_name": user.display_name,
        "role": user.role,
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

pub async fn feeds(req: HttpRequest, st: Data) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Viewer).await?;
    ok(svc::feeds(&st).await?)
}

pub async fn feed_config(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Viewer).await?;
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
    auth::require(&req, &st, Role::Viewer).await?;
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
    auth::require(&req, &st, Role::Viewer).await?;
    let (g, id) = path.into_inner();
    let mut conn = st.pool.acquire().await?;
    ok(svc::stop_detail(&mut conn, &g, &id).await?)
}

#[derive(Deserialize)]
pub struct RoutesQuery {
    q: Option<String>,
    /// `missing` or `present`: whether the route has a map line (section 14).
    polyline: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

pub async fn routes(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    q: web::Query<RoutesQuery>,
) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Viewer).await?;
    let page = Page::parse(q.limit, q.cursor.as_deref())?;
    let polyline = match q
        .polyline
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => None,
        Some("missing") => Some(false),
        Some("present") => Some(true),
        Some(_) => {
            return Err(EditorError::bad_request(
                "invalid_polyline_filter",
                "polyline is missing or present",
            ))
        }
    };
    ok(svc::list_routes(&st, &path, q.q.as_deref(), polyline, &page).await?)
}

pub async fn route(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Viewer).await?;
    let (g, id) = path.into_inner();
    let mut conn = st.pool.acquire().await?;
    ok(svc::route_detail(&mut conn, &g, &id).await?)
}

/// What a person cleaning up a stop wants beside it (docs section 9).
pub async fn stop_context(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Viewer).await?;
    let (g, id) = path.into_inner();
    let mut conn = st.pool.acquire().await?;
    ok(context::stop(&mut conn, &g, &id).await?)
}

#[derive(Deserialize)]
pub struct TripsQuery {
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

/// What a route has actually been running (docs/gtfs-editor.md section 13):
/// its trips inside the window, or its last known trip when it has none.
/// Readable by anyone who can sign in.
pub async fn route_trips(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
    q: web::Query<TripsQuery>,
) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Viewer).await?;
    let (g, route_id) = path.into_inner();
    let mut conn = st.pool.acquire().await?;
    // 404 on a route the feed does not have, so a typo is not an empty answer
    svc::route_row(&mut conn, &g, &route_id)
        .await?
        .ok_or_else(|| EditorError::not_found("route_not_found", format!("no route {route_id}")))?;
    ok(trips::for_route(
        st.ops_pool.as_ref(),
        &st.trips_cache,
        &route_id,
        trips::clamp_days(q.days),
        trips::clamp_limit(q.limit),
    )
    .await?)
}

pub async fn route_context(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Viewer).await?;
    let (g, id) = path.into_inner();
    let mut conn = st.pool.acquire().await?;
    ok(context::route(&mut conn, &g, &id).await?)
}

#[derive(Deserialize)]
pub struct PolylineQuery {
    change_set: Option<Uuid>,
}

pub async fn polyline_osrm(
    req: HttpRequest,
    st: Data,
    path: web::Path<(String, String)>,
    q: web::Query<PolylineQuery>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    let (g, route_id) = path.into_inner();
    let detail = match q.change_set {
        Some(id) => svc::preview_route(&st, &ctx, id, &route_id).await?,
        None => {
            let mut conn = st.pool.acquire().await?;
            svc::route_detail(&mut conn, &g, &route_id).await?
        }
    };
    let points = svc::polyline_waypoints(&detail);
    if st.osrm_url.as_deref().unwrap_or("").is_empty() {
        return Err(EditorError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "osrm_unavailable",
            "no OSRM server is configured",
        ));
    }
    let Some((polyline, legs)) =
        crate::services::operator::osrm_route(st.osrm_url.as_deref(), &points).await
    else {
        return Err(EditorError::new(
            StatusCode::BAD_GATEWAY,
            "osrm_failed",
            "OSRM could not route through these stops",
        ));
    };
    let distance: f64 = legs.iter().map(|(d, _)| d).sum();
    ok(json!({
        "route_id": route_id,
        "encoded_polyline": polyline,
        "polyline_source": "osrm",
        "waypoints": points.len(),
        "distance_m": distance.round(),
        "saved": false,
    }))
}

/// Keep a map line on a route, as a `route/update` in the draft: the proposal
/// above, a line the operator has, or the points of one (docs section 14).
pub async fn route_polyline_set(
    req: HttpRequest,
    st: Data,
    path: web::Path<(Uuid, String)>,
    body: web::Json<svc::PolylineRequest>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    let (set_id, route_id) = path.into_inner();
    let out = svc::set_route_polyline(&st, &ctx, set_id, &route_id, body.into_inner()).await?;
    Ok(HttpResponse::Created().json(out))
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
    auth::require(&req, &st, Role::Viewer).await?;
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
    auth::require(&req, &st, Role::Viewer).await?;
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
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    let set = svc::create_set(&st, &ctx, &path, &body.title, body.description.as_deref()).await?;
    Ok(HttpResponse::Created().json(set))
}

pub async fn change_set(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Viewer).await?;
    ok(svc::set_detail(&st, &ctx, *path).await?)
}

pub async fn change_add(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
    body: web::Json<svc::NewChange>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
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
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    let (id, change_id) = path.into_inner();
    svc::update_change(&st, &ctx, id, change_id, body.into_inner()).await?;
    ok(svc::set_detail(&st, &ctx, id).await?)
}

pub async fn change_delete(
    req: HttpRequest,
    st: Data,
    path: web::Path<(Uuid, i64)>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    let (id, change_id) = path.into_inner();
    svc::delete_change(&st, &ctx, id, change_id).await?;
    ok(svc::set_detail(&st, &ctx, id).await?)
}

pub async fn change_set_preview_route(
    req: HttpRequest,
    st: Data,
    path: web::Path<(Uuid, String)>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Viewer).await?;
    let (id, route_id) = path.into_inner();
    ok(svc::preview_route(&st, &ctx, id, &route_id).await?)
}

pub async fn submit(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    svc::submit(&st, &ctx, *path).await?;
    ok(svc::set_detail(&st, &ctx, *path).await?)
}

pub async fn reopen(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
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
    let ctx = auth::require(&req, &st, Role::Approver).await?;
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
    let ctx = auth::require(&req, &st, Role::Approver).await?;
    svc::review(&st, &ctx, *path, false, body.comment.as_deref(), false).await?;
    ok(svc::set_detail(&st, &ctx, *path).await?)
}

pub async fn commit(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Approver).await?;
    ok(svc::commit(&st, &ctx, *path).await?)
}

pub async fn discard(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    svc::discard(&st, &ctx, *path).await?;
    ok(svc::set_detail(&st, &ctx, *path).await?)
}

pub async fn bulk_import(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
    body: web::Json<bulk::BulkRequest>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    ok(bulk::run(&st, &ctx, *path, body.into_inner()).await?)
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
    auth::require(&req, &st, Role::Viewer).await?;
    let page = Page::parse(q.limit, q.cursor.as_deref())?;
    ok(proposals::list(&st, &path, &q.filters()?, &page).await?)
}

pub async fn station_proposal_summary(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Viewer).await?;
    ok(proposals::summary(&st, &path).await?)
}

pub async fn station_proposal(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Viewer).await?;
    let mut conn = st.pool.acquire().await?;
    ok(proposals::detail(&mut conn, *path).await?)
}

pub async fn station_proposal_approve(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
    body: web::Json<proposals::ApproveBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    ok(proposals::approve(&st, &ctx, *path, body.into_inner()).await?)
}

pub async fn station_proposals_approve(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    body: web::Json<proposals::BulkApproveBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
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
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    let body = body.map(|b| b.into_inner()).unwrap_or_default();
    ok(proposals::reject(&st, &ctx, *path, body.note.as_deref()).await?)
}

pub async fn station_proposal_reopen(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    ok(proposals::reopen(&st, &ctx, *path).await?)
}

// ---------------------------------------------------------------- position reviews

pub async fn position_reviews(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
    q: web::Query<ProposalsQuery>,
) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Viewer).await?;
    let page = Page::parse(q.limit, q.cursor.as_deref())?;
    ok(position_reviews::list(&st, &path, &q.filters()?, q.auto_fix.as_deref(), &page).await?)
}

pub async fn position_review_summary(
    req: HttpRequest,
    st: Data,
    path: web::Path<String>,
) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Viewer).await?;
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
    let ctx = auth::require(&req, &st, Role::Viewer).await?;
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
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    ok(position_reviews::move_stop(&st, &ctx, *path, body.into_inner()).await?)
}

pub async fn position_review_split(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
    body: web::Json<position_reviews::SplitBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    ok(position_reviews::split(&st, &ctx, *path, body.into_inner()).await?)
}

pub async fn position_review_merge(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
    body: web::Json<position_reviews::MergeBody>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    ok(position_reviews::merge(&st, &ctx, *path, body.into_inner()).await?)
}

pub async fn position_review_confirm(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
    body: Option<web::Json<NoteBody>>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    let body = body.map(|b| b.into_inner()).unwrap_or_default();
    ok(position_reviews::confirm(&st, &ctx, *path, body.note.as_deref()).await?)
}

pub async fn position_review_reopen(
    req: HttpRequest,
    st: Data,
    path: web::Path<i64>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Editor).await?;
    ok(position_reviews::reopen(&st, &ctx, *path).await?)
}

// ---------------------------------------------------------------- admin

pub async fn users(req: HttpRequest, st: Data) -> EditorResult<HttpResponse> {
    auth::require(&req, &st, Role::Admin).await?;
    ok(svc::users(&st).await?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewUser {
    email: String,
    #[serde(default)]
    display_name: Option<String>,
    role: String,
}

pub async fn user_create(
    req: HttpRequest,
    st: Data,
    body: web::Json<NewUser>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Admin).await?;
    let user = svc::create_user(
        &st,
        &ctx,
        &body.email,
        body.display_name.as_deref(),
        &body.role,
    )
    .await?;
    Ok(HttpResponse::Created().json(user))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserPatch {
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
    let ctx = auth::require(&req, &st, Role::Admin).await?;
    svc::update_user(
        &st,
        &ctx,
        *path,
        body.role.as_deref(),
        body.status.as_deref(),
    )
    .await?;
    let user = auth::user_by_id(&st, *path)
        .await?
        .ok_or_else(|| EditorError::not_found("user_not_found", "no such user"))?;
    ok(user.public_json())
}

pub async fn user_reset_totp(
    req: HttpRequest,
    st: Data,
    path: web::Path<Uuid>,
) -> EditorResult<HttpResponse> {
    let ctx = auth::require(&req, &st, Role::Admin).await?;
    svc::reset_totp(&st, &ctx, *path).await?;
    Ok(HttpResponse::NoContent().finish())
}
