//! Who is calling, and what they may do.
//!
//! Three independent checks, all required for anything but `/auth/*`:
//!
//!   1. Pomerium's JWT (verified in [`guard`]) - identity. The email comes only
//!      from a token whose signature, audience and expiry check out.
//!   2. A session cookie created after a valid TOTP code - the second factor.
//!      The session's user must be the JWT's user.
//!   3. The user's role.
//!
//! Mutations additionally need `X-Requested-With: gtfs-editor`, which a
//! cross-site form cannot send.

use super::crypto;
use super::error::{EditorError, EditorResult};
use super::EditorState;
use actix_http::HttpMessage;
use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::Method;
use actix_web::middleware::Next;
use actix_web::{web, HttpRequest, ResponseError};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

pub const SESSION_COOKIE: &str = "gtfs_editor_session";
pub const JWT_HEADER: &str = "x-pomerium-jwt-assertion";
pub const CSRF_HEADER: &str = "x-requested-with";
pub const CSRF_VALUE: &str = "gtfs-editor";

pub const NO_SSO_IDENTITY: &str = "no_sso_identity";
pub const NOT_REGISTERED: &str = "not_registered";
pub const ACCOUNT_DISABLED: &str = "account_disabled";

/// Failed codes allowed in the window before sign-in locks.
pub const MAX_FAILED_CODES: i64 = 5;
pub const LOCK_MINUTES: i64 = 10;

#[derive(Debug, Clone)]
pub struct Identity {
    pub email: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer = 0,
    Editor = 1,
    Approver = 2,
    Admin = 3,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "viewer" => Some(Role::Viewer),
            "editor" => Some(Role::Editor),
            "approver" => Some(Role::Approver),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: Option<String>,
    pub role: String,
    pub totp_enabled: bool,
    pub totp_last_step: Option<i64>,
    pub totp_secret_enc: Option<Vec<u8>>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

impl User {
    pub fn role(&self) -> Role {
        Role::parse(&self.role).unwrap_or(Role::Viewer)
    }

    pub fn public_json(&self) -> Value {
        json!({
            "user_id": self.user_id,
            "email": self.email,
            "display_name": self.display_name,
            "role": self.role,
            "status": self.status,
            "totp_enabled": self.totp_enabled,
            "created_at": self.created_at,
            "last_login_at": self.last_login_at,
        })
    }
}

/// Verified caller: identity, second factor and an active account.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub user: User,
}

impl Ctx {
    pub fn require_role(&self, min: Role) -> EditorResult<()> {
        if self.user.role() >= min {
            Ok(())
        } else {
            Err(EditorError::forbidden(
                "role_required",
                format!("this needs the {:?} role or higher", min).to_lowercase(),
            ))
        }
    }
}

fn is_safe_method(m: &Method) -> bool {
    matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Middleware on the API scope: CSRF header on mutations, then the JWT.
pub async fn guard(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<actix_web::body::BoxBody>, actix_web::Error> {
    match check_request(&req).await {
        Ok(identity) => {
            req.extensions_mut().insert(identity);
            next.call(req)
                .await
                .map(ServiceResponse::map_into_boxed_body)
        }
        Err(e) => {
            let resp = e.error_response();
            Ok(req.into_response(resp))
        }
    }
}

async fn check_request(req: &ServiceRequest) -> EditorResult<Identity> {
    let state = req
        .app_data::<web::Data<EditorState>>()
        .ok_or_else(|| EditorError::internal("editor state missing"))?;
    if !is_safe_method(req.method()) {
        let ok = req
            .headers()
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == CSRF_VALUE);
        if !ok {
            return Err(EditorError::forbidden(
                "csrf_header_required",
                format!("send header {CSRF_HEADER}: {CSRF_VALUE} with every change"),
            ));
        }
    }
    // Every SSO failure is one code the dashboard can act on; the precise reason
    // (missing, expired, wrong audience, forged) goes in details for logs/tests.
    let token = req
        .headers()
        .get(JWT_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            EditorError::unauthorized(NO_SSO_IDENTITY, "open the editor through its SSO address")
                .with_details(json!({"reason": "jwt_missing"}))
        })?;
    let claims = state
        .jwks
        .verify(token, &state.audience)
        .await
        .map_err(|e| {
            EditorError::unauthorized(
                NO_SSO_IDENTITY,
                "the SSO sign-in is not valid; reload the page",
            )
            .with_details(json!({"reason": e.code()}))
        })?;
    Ok(Identity {
        email: claims.email,
    })
}

pub fn identity(req: &HttpRequest) -> EditorResult<Identity> {
    req.extensions()
        .get::<Identity>()
        .cloned()
        .ok_or_else(|| EditorError::unauthorized(NO_SSO_IDENTITY, "no verified identity"))
}

fn user_from_row(r: &sqlx::postgres::PgRow) -> Result<User, sqlx::Error> {
    Ok(User {
        user_id: r.try_get("user_id")?,
        email: r.try_get("email")?,
        display_name: r.try_get("display_name")?,
        role: r.try_get("role")?,
        totp_enabled: r.try_get("totp_enabled")?,
        totp_last_step: r.try_get("totp_last_step")?,
        totp_secret_enc: r.try_get("totp_secret_enc")?,
        status: r.try_get("status")?,
        created_at: r.try_get("created_at")?,
        last_login_at: r.try_get("last_login_at")?,
    })
}

const USER_COLUMNS: &str = "user_id, email, display_name, role, totp_enabled, totp_last_step, \
     totp_secret_enc, status, created_at, last_login_at";

pub async fn user_by_id(state: &EditorState, user_id: Uuid) -> EditorResult<Option<User>> {
    let row = sqlx::query(&format!(
        "SELECT {USER_COLUMNS} FROM gtfs_editor_user WHERE user_id = $1"
    ))
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(row.as_ref().map(user_from_row).transpose()?)
}

/// The account for a verified email, creating bootstrap admins on first sight.
pub async fn user_for_email(state: &EditorState, email: &str) -> EditorResult<Option<User>> {
    let fetch = || async {
        sqlx::query(&format!(
            "SELECT {USER_COLUMNS} FROM gtfs_editor_user WHERE lower(email) = lower($1)"
        ))
        .bind(email)
        .fetch_optional(&state.pool)
        .await
    };
    if let Some(row) = fetch().await? {
        return Ok(Some(user_from_row(&row)?));
    }
    if !state.bootstrap_admins.contains(&email.to_ascii_lowercase()) {
        return Ok(None);
    }
    let inserted = sqlx::query(
        "INSERT INTO gtfs_editor_user (email, role) VALUES (lower($1), 'admin') \
         ON CONFLICT DO NOTHING RETURNING user_id",
    )
    .bind(email)
    .fetch_optional(&state.pool)
    .await?;
    if let Some(r) = inserted {
        let user_id: Uuid = r.try_get("user_id")?;
        audit(
            &state.pool,
            Some(user_id),
            Some(email),
            "user_bootstrapped",
            None,
            None,
            json!({"role": "admin"}),
        )
        .await?;
    }
    Ok(fetch().await?.as_ref().map(user_from_row).transpose()?)
}

pub fn not_registered(email: &str) -> EditorError {
    EditorError::forbidden(
        NOT_REGISTERED,
        format!("{email} has no editor account; ask an admin to add you"),
    )
}

pub fn account_disabled() -> EditorError {
    EditorError::forbidden(
        ACCOUNT_DISABLED,
        "this editor account is turned off; ask an admin",
    )
}

pub async fn active_user(req: &HttpRequest, state: &EditorState) -> EditorResult<User> {
    let id = identity(req)?;
    let user = user_for_email(state, &id.email)
        .await?
        .ok_or_else(|| not_registered(&id.email))?;
    if user.status != "active" {
        return Err(account_disabled());
    }
    Ok(user)
}

/// Is the request's session cookie a live, second-factor session for `user`?
pub async fn session_user(
    req: &HttpRequest,
    state: &EditorState,
    user: &User,
) -> EditorResult<bool> {
    let Some(cookie) = req.cookie(SESSION_COOKIE) else {
        return Ok(false);
    };
    let hash = crypto::sha256(cookie.value().as_bytes());
    let row = sqlx::query(
        "UPDATE gtfs_editor_session SET last_seen_at = now() \
         WHERE token_hash = $1 AND user_id = $2 AND expires_at > now() \
           AND mfa_verified_at IS NOT NULL \
         RETURNING user_id",
    )
    .bind(hash)
    .bind(user.user_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(row.is_some())
}

/// Everything a data endpoint needs: JWT identity, active account, a second-
/// factor session for that same account, and at least `min` role.
pub async fn require(req: &HttpRequest, state: &EditorState, min: Role) -> EditorResult<Ctx> {
    let user = active_user(req, state).await?;
    if !user.totp_enabled {
        return Err(EditorError::unauthorized(
            "totp_enrollment_required",
            "set up your authenticator app first",
        ));
    }
    if !session_user(req, state, &user).await? {
        return Err(EditorError::unauthorized(
            "session_required",
            "enter a code from your authenticator app",
        ));
    }
    let ctx = Ctx { user };
    ctx.require_role(min)?;
    Ok(ctx)
}

/// Wrong codes counted toward the lockout right now (same window as
/// [`lockout_seconds`]), so a failed attempt can say how many tries are left.
pub async fn failed_codes(state: &EditorState, user_id: Uuid) -> EditorResult<i64> {
    let row = sqlx::query(
        "WITH last_ok AS ( \
            SELECT coalesce(max(a.at), '-infinity'::timestamptz) AS ok_at FROM gtfs_audit_log a \
            WHERE a.actor = $1 AND a.action IN ('session_created', 'totp_confirmed')) \
         SELECT count(*) AS n FROM gtfs_audit_log f, last_ok \
         WHERE f.actor = $1 AND f.action = 'totp_failed' \
           AND f.at > greatest(now() - make_interval(mins => $2), last_ok.ok_at)",
    )
    .bind(user_id)
    .bind(LOCK_MINUTES as i32)
    .fetch_one(&state.pool)
    .await?;
    Ok(row.try_get("n")?)
}

/// Failed-code lockout, counted in the audit log so it holds across pods:
/// `MAX_FAILED_CODES` failures within `LOCK_MINUTES`, since the last successful
/// sign-in, lock sign-in until the oldest of them ages out.
pub async fn lockout_seconds(state: &EditorState, user_id: Uuid) -> EditorResult<Option<i64>> {
    let row = sqlx::query(
        "WITH last_ok AS ( \
            SELECT coalesce(max(a.at), '-infinity'::timestamptz) AS ok_at FROM gtfs_audit_log a \
            WHERE a.actor = $1 AND a.action IN ('session_created', 'totp_confirmed')), \
         fails AS ( \
            SELECT f.at AS failed_at FROM gtfs_audit_log f, last_ok \
            WHERE f.actor = $1 AND f.action = 'totp_failed' \
              AND f.at > greatest(now() - make_interval(mins => $2), last_ok.ok_at) \
            ORDER BY f.at DESC LIMIT $3) \
         SELECT count(*) AS n, \
                ceil(extract(epoch FROM (min(failed_at) + make_interval(mins => $2) - now())))::bigint AS wait \
         FROM fails",
    )
    .bind(user_id)
    .bind(LOCK_MINUTES as i32)
    .bind(MAX_FAILED_CODES)
    .fetch_one(&state.pool)
    .await?;
    let n: i64 = row.try_get("n")?;
    let wait: Option<i64> = row.try_get("wait")?;
    Ok(if n >= MAX_FAILED_CODES {
        Some(wait.unwrap_or(LOCK_MINUTES * 60).max(1))
    } else {
        None
    })
}

pub async fn audit<'e, E>(
    executor: E,
    actor: Option<Uuid>,
    actor_email: Option<&str>,
    action: &str,
    gtfs_id: Option<&str>,
    change_set_id: Option<Uuid>,
    detail: Value,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(
        "INSERT INTO gtfs_audit_log (actor, actor_email, action, gtfs_id, change_set_id, detail) \
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind(actor)
    .bind(actor_email)
    .bind(action)
    .bind(gtfs_id)
    .bind(change_set_id)
    .bind(detail.to_string())
    .execute(executor)
    .await?;
    Ok(())
}

/// [`audit`] for many rows of one action in one statement: one row per detail.
pub async fn audit_many<'e, E>(
    executor: E,
    actor: Option<Uuid>,
    actor_email: Option<&str>,
    action: &str,
    gtfs_id: Option<&str>,
    change_set_id: Option<Uuid>,
    details: &[Value],
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if details.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO gtfs_audit_log (actor, actor_email, action, gtfs_id, change_set_id, detail) \
         SELECT $1, $2, $3, $4, $5, d::jsonb FROM UNNEST($6::text[]) WITH ORDINALITY AS u(d, n) ORDER BY n",
    )
    .bind(actor)
    .bind(actor_email)
    .bind(action)
    .bind(gtfs_id)
    .bind(change_set_id)
    .bind(details.iter().map(Value::to_string).collect::<Vec<_>>())
    .execute(executor)
    .await?;
    Ok(())
}
