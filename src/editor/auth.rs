//! Who is calling, and what they may do.
//!
//! Three independent checks, all required for anything but `/auth/*`:
//!
//!   1. Pomerium's JWT (verified in [`guard`]) - identity. The email comes only
//!      from a token whose signature, audience and expiry check out.
//!   2. A session cookie created after a valid TOTP code - the second factor.
//!      The session's user must be the JWT's user.
//!   3. The user's role on the feed the request is about (docs/gtfs-editor.md
//!      section 15): an admin holds every feed at every role; anyone else holds
//!      a feed only through a grant on it, loaded with the user on every request.
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
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const SESSION_COOKIE: &str = "gtfs_editor_session";
pub const JWT_HEADER: &str = "x-pomerium-jwt-assertion";
pub const CSRF_HEADER: &str = "x-requested-with";
pub const CSRF_VALUE: &str = "gtfs-editor";

pub const NO_SSO_IDENTITY: &str = "no_sso_identity";
pub const NOT_REGISTERED: &str = "not_registered";
pub const ACCOUNT_DISABLED: &str = "account_disabled";
pub const NO_FEED_ACCESS: &str = "no_feed_access";

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

    /// A role a grant may carry: every role but admin, which is global.
    pub fn parse_grant(s: &str) -> Option<Role> {
        Self::parse(s).filter(|r| *r != Role::Admin)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Approver => "approver",
            Role::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: Option<String>,
    /// The column as stored. Only `admin` means anything since 0018; the other
    /// values are kept for an older image rolled back onto the database.
    pub role: String,
    /// `person`, or `system` for an account nobody signs in with.
    pub kind: String,
    /// The feeds this user holds a grant on, and the role held on each. An admin
    /// holds none: an admin has every feed.
    pub grants: BTreeMap<String, Role>,
    pub totp_enabled: bool,
    pub totp_last_step: Option<i64>,
    pub totp_secret_enc: Option<Vec<u8>>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

impl User {
    /// The one global role: every feed, at every role.
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }

    pub fn is_system(&self) -> bool {
        self.kind == "system"
    }
}

/// Verified caller: identity, second factor and an active account, with the
/// grants it held when this request came in.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub user: User,
}

impl Ctx {
    pub fn is_admin(&self) -> bool {
        self.user.is_admin()
    }

    /// The caller's role on feed `g`: `Admin` for an admin, else the grant's
    /// role, else none - the feed does not exist for them.
    pub fn feed_role(&self, g: &str) -> Option<Role> {
        if self.is_admin() {
            Some(Role::Admin)
        } else {
            self.user.grants.get(g).copied()
        }
    }

    /// At least `min` on feed `g`: 403 `no_feed_access` without a grant there,
    /// 403 `role_required` with one that is too low.
    pub fn require_feed_role(&self, g: &str, min: Role) -> EditorResult<()> {
        match self.feed_role(g) {
            None => Err(no_feed_access(g)),
            Some(role) if role >= min => Ok(()),
            Some(_) => Err(role_required(min)),
        }
    }

    /// Users, grants, webhook settings: an admin's, whatever the feed.
    pub fn require_admin(&self) -> EditorResult<()> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(role_required(Role::Admin))
        }
    }
}

pub fn role_required(min: Role) -> EditorError {
    EditorError::forbidden(
        "role_required",
        format!("this needs the {} role or higher", min.as_str()),
    )
}

/// The same answer for a feed the caller has no grant on and for an object that
/// belongs to one, so an id from another feed says nothing about that feed.
pub fn no_feed_access(g: &str) -> EditorError {
    EditorError::forbidden(
        NO_FEED_ACCESS,
        format!("you have no access to feed {g}; ask an admin"),
    )
    .with_details(json!({"gtfs_id": g}))
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
    let feeds: Vec<String> = r.try_get("grant_feeds")?;
    let roles: Vec<String> = r.try_get("grant_roles")?;
    Ok(User {
        user_id: r.try_get("user_id")?,
        email: r.try_get("email")?,
        display_name: r.try_get("display_name")?,
        role: r.try_get("role")?,
        kind: r.try_get("kind")?,
        // the table's CHECK allows only grant roles; anything else is no grant
        grants: feeds
            .into_iter()
            .zip(roles)
            .filter_map(|(g, role)| Some((g, Role::parse_grant(&role)?)))
            .collect(),
        totp_enabled: r.try_get("totp_enabled")?,
        totp_last_step: r.try_get("totp_last_step")?,
        totp_secret_enc: r.try_get("totp_secret_enc")?,
        status: r.try_get("status")?,
        created_at: r.try_get("created_at")?,
        last_login_at: r.try_get("last_login_at")?,
    })
}

/// A user and their grants, in one join: `{USER_SELECT} WHERE ... GROUP BY
/// u.user_id`. Every request reads it, so a grant or a revocation bites on the
/// holder's next request.
const USER_SELECT: &str =
    "SELECT u.user_id, u.email, u.display_name, u.role, u.kind, u.totp_enabled, \
     u.totp_last_step, u.totp_secret_enc, u.status, u.created_at, u.last_login_at, \
     array_remove(array_agg(a.gtfs_id::text ORDER BY a.gtfs_id), NULL) AS grant_feeds, \
     array_remove(array_agg(a.role ORDER BY a.gtfs_id), NULL) AS grant_roles \
     FROM gtfs_editor_user u LEFT JOIN gtfs_editor_feed_access a ON a.user_id = u.user_id";

pub async fn user_by_id(state: &EditorState, user_id: Uuid) -> EditorResult<Option<User>> {
    let row = sqlx::query(&format!(
        "{USER_SELECT} WHERE u.user_id = $1 GROUP BY u.user_id"
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
            "{USER_SELECT} WHERE lower(u.email) = lower($1) GROUP BY u.user_id"
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

/// The JWT's account, if it may sign in at all. Every sign-in path starts here,
/// so a system account (section 15) is refused on all of them - whatever SSO
/// identity turns up with its email.
pub async fn active_user(req: &HttpRequest, state: &EditorState) -> EditorResult<User> {
    let id = identity(req)?;
    let user = user_for_email(state, &id.email)
        .await?
        .ok_or_else(|| not_registered(&id.email))?;
    if user.status != "active" || user.is_system() {
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

/// Everything a data endpoint needs before its feed is known: JWT identity,
/// active account, a second-factor session for that same account. The caller's
/// grants come with it; what it may do on a feed is [`Ctx::require_feed_role`].
pub async fn signed_in(req: &HttpRequest, state: &EditorState) -> EditorResult<Ctx> {
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
    Ok(Ctx { user })
}

/// [`signed_in`], and at least `min` on feed `g` (a path's `{gtfs_id}`).
pub async fn require_feed(
    req: &HttpRequest,
    state: &EditorState,
    g: &str,
    min: Role,
) -> EditorResult<Ctx> {
    let ctx = signed_in(req, state).await?;
    ctx.require_feed_role(g, min)?;
    Ok(ctx)
}

/// [`signed_in`], and an admin.
pub async fn require_admin(req: &HttpRequest, state: &EditorState) -> EditorResult<Ctx> {
    let ctx = signed_in(req, state).await?;
    ctx.require_admin()?;
    Ok(ctx)
}

/// What a path keyed by an object names. Its feed is looked up before any role
/// is checked, so the role is always the one held on that object's feed.
#[derive(Debug, Clone, Copy)]
pub enum Object {
    ChangeSet(Uuid),
    StationProposal(i64),
    PositionReview(i64),
    Webhook(Uuid),
}

/// The feed an object belongs to; 404 when there is no such object. An object
/// never moves to another feed, so the answer holds for the whole request.
pub async fn feed_of(pool: &PgPool, object: Object) -> EditorResult<String> {
    let row = match object {
        Object::ChangeSet(id) => {
            sqlx::query("SELECT gtfs_id FROM gtfs_change_set WHERE change_set_id = $1")
                .bind(id)
                .fetch_optional(pool)
                .await?
        }
        Object::StationProposal(id) => {
            sqlx::query("SELECT gtfs_id FROM gtfs_station_proposal WHERE proposal_id = $1")
                .bind(id)
                .fetch_optional(pool)
                .await?
        }
        Object::PositionReview(id) => {
            sqlx::query("SELECT gtfs_id FROM gtfs_position_review WHERE review_id = $1")
                .bind(id)
                .fetch_optional(pool)
                .await?
        }
        Object::Webhook(id) => {
            sqlx::query("SELECT gtfs_id FROM gtfs_webhook WHERE webhook_id = $1")
                .bind(id)
                .fetch_optional(pool)
                .await?
        }
    };
    match row {
        Some(r) => Ok(r.try_get("gtfs_id")?),
        // the same codes and words as each object's own loader
        None => Err(match object {
            Object::ChangeSet(_) => {
                EditorError::not_found("change_set_not_found", "no such change set")
            }
            Object::StationProposal(id) => {
                EditorError::not_found("proposal_not_found", format!("no proposal {id}"))
            }
            Object::PositionReview(id) => {
                EditorError::not_found("review_not_found", format!("no position review {id}"))
            }
            Object::Webhook(_) => EditorError::not_found("webhook_not_found", "no such webhook"),
        }),
    }
}

/// [`signed_in`], then the object's feed, then at least `min` there.
pub async fn require_object(
    req: &HttpRequest,
    state: &EditorState,
    object: Object,
    min: Role,
) -> EditorResult<Ctx> {
    let ctx = signed_in(req, state).await?;
    let g = feed_of(&state.pool, object).await?;
    ctx.require_feed_role(&g, min)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(role: &str, grants: &[(&str, Role)]) -> Ctx {
        Ctx {
            user: User {
                user_id: Uuid::nil(),
                email: "someone@example.invalid".into(),
                display_name: None,
                role: role.into(),
                kind: "person".into(),
                grants: grants.iter().map(|(g, r)| (g.to_string(), *r)).collect(),
                totp_enabled: true,
                totp_last_step: None,
                totp_secret_enc: None,
                status: "active".into(),
                created_at: Utc::now(),
                last_login_at: None,
            },
        }
    }

    fn code(r: EditorResult<()>) -> Option<(u16, &'static str, Value)> {
        r.err().map(|e| (e.status.as_u16(), e.code, e.details))
    }

    /// Docs section 15.2: the grant's role on its feed, nothing elsewhere, and
    /// the column's old role counts for nothing but `admin`.
    #[test]
    fn a_members_role_is_the_one_granted_on_that_feed() {
        let c = ctx("approver", &[("a", Role::Editor)]);
        assert_eq!(c.feed_role("a"), Some(Role::Editor));
        assert_eq!(c.feed_role("b"), None);
        assert!(c.require_feed_role("a", Role::Viewer).is_ok());
        assert!(c.require_feed_role("a", Role::Editor).is_ok());
        assert_eq!(
            code(c.require_feed_role("a", Role::Approver)),
            Some((403, "role_required", Value::Null))
        );
        assert_eq!(
            code(c.require_feed_role("b", Role::Viewer)),
            Some((403, NO_FEED_ACCESS, json!({"gtfs_id": "b"})))
        );
        assert!(!c.is_admin());
        assert_eq!(
            code(c.require_admin()),
            Some((403, "role_required", Value::Null))
        );
    }

    #[test]
    fn an_admin_has_every_feed_at_every_role() {
        let c = ctx("admin", &[]);
        assert_eq!(c.feed_role("any"), Some(Role::Admin));
        assert!(c.require_feed_role("any", Role::Admin).is_ok());
        assert!(c.require_admin().is_ok());
    }

    #[test]
    fn a_grant_never_carries_admin() {
        assert_eq!(Role::parse_grant("approver"), Some(Role::Approver));
        assert_eq!(Role::parse_grant("admin"), None);
        assert_eq!(Role::parse_grant("owner"), None);
        for r in [Role::Viewer, Role::Editor, Role::Approver, Role::Admin] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
    }
}
