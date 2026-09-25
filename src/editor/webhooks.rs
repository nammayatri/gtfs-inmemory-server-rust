//! Webhook configuration and cache state, for the dashboard.
//!
//! The delivery machinery is [`crate::services::webhook`], which every pod runs
//! beside its version poll. This module is only the operator's view of it: what
//! is configured, what each pod is serving, and what was delivered.
//!
//! # Why this is not a draft change
//!
//! Everything about a *feed* is edited through a draft that a second person
//! approves. A webhook is not feed data - it is delivery plumbing, and a draft
//! would not make it safer, because the risk here is not a bad edit reaching
//! passengers but GIMS being pointed at a host it should not call.
//!
//! That risk is answered by the allow-list: a URL must match one of its hosts,
//! checked when it is saved and again when the request is about to go out. The
//! list itself is [`settings_update`] - admin only, and audited like every
//! other change here.

use super::auth::{self, Ctx};
use super::error::{EditorError, EditorResult};
use super::EditorState;
use crate::services::webhook::{self, check_host, fleet_from, EffectivePolicy, PodState};
use actix_web::http::StatusCode;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// Longest a webhook may wait before the dashboard considers the fleet stale,
/// mirrored from the CHECK constraints in `0013_webhooks.sql` so a bad value is
/// refused with a readable message instead of a database error.
const LIMITS: &[(&str, i64, i64)] = &[
    ("stale_after_seconds", 10, 3600),
    ("settle_seconds", 0, 3600),
    ("give_up_after_seconds", 60, 86400),
    ("request_timeout_seconds", 1, 300),
    ("max_attempts", 1, 20),
];

const EVENTS: &[&str] = &[
    webhook::EVENT_IN_SYNC,
    webhook::EVENT_COMMITTED,
    webhook::EVENT_RELOAD_FAILED,
    webhook::EVENT_RELEASE_REQUESTED,
];

const METHODS: &[&str] = &["POST", "PUT", "GET"];

const WEBHOOKS_INACTIVE: &str =
    "webhooks are off or the allow-list is empty, so nothing would be sent";

#[derive(Debug, Clone, Deserialize)]
pub struct WebhookBody {
    pub name: Option<String>,
    pub event: Option<String>,
    pub url: Option<String>,
    pub method: Option<String>,
    pub headers: Option<Value>,
    pub body: Option<Value>,
    pub enabled: Option<bool>,
    pub stale_after_seconds: Option<i64>,
    pub settle_seconds: Option<i64>,
    pub give_up_after_seconds: Option<i64>,
    pub request_timeout_seconds: Option<i64>,
    pub max_attempts: Option<i64>,
}

fn bad(code: &'static str, message: impl Into<String>) -> EditorError {
    EditorError::bad_request(code, message)
}

fn check_event(event: &str) -> EditorResult<()> {
    if EVENTS.contains(&event) {
        Ok(())
    } else {
        Err(bad(
            "invalid_event",
            format!("event is one of {}", EVENTS.join(", ")),
        ))
    }
}

fn check_method(method: &str) -> EditorResult<()> {
    if METHODS.contains(&method) {
        Ok(())
    } else {
        Err(bad(
            "invalid_method",
            format!("method is one of {}", METHODS.join(", ")),
        ))
    }
}

fn check_limit(field: &str, value: i64) -> EditorResult<()> {
    for (name, lo, hi) in LIMITS {
        if *name == field && !(*lo..=*hi).contains(&value) {
            return Err(bad(
                "out_of_range",
                format!("{field} is between {lo} and {hi}"),
            ));
        }
    }
    Ok(())
}

/// Headers must be a flat object of strings: the values are templates resolved
/// against the pod's environment, and anything else has no meaning on the wire.
fn check_headers(headers: &Value) -> EditorResult<()> {
    let obj = headers
        .as_object()
        .ok_or_else(|| bad("invalid_headers", "headers is an object of strings"))?;
    for (k, v) in obj {
        if k.trim().is_empty() || !k.is_ascii() {
            return Err(bad(
                "invalid_headers",
                format!("{k:?} is not a usable header name"),
            ));
        }
        if !v.is_string() {
            return Err(bad(
                "invalid_headers",
                format!("the value of {k} must be a string"),
            ));
        }
    }
    Ok(())
}

/// The policy in force. Read per call rather than held anywhere, because this
/// same API edits it: a list read a moment ago may already be the wrong one.
async fn live_policy(state: &EditorState) -> EffectivePolicy {
    state.webhook_policy.get(&state.pool).await
}

/// A URL is usable if the allow-list in force covers its host *and* every
/// placeholder in it can be resolved right now. Catching a missing environment
/// variable here, rather than at the first delivery, is the difference between
/// a typo the admin sees immediately and a cache that silently never refreshes.
async fn check_url(state: &EditorState, url: &str) -> EditorResult<()> {
    let mut probe = Map::new();
    for (k, v) in [
        ("gtfs_id", json!("probe")),
        ("feed_version", json!(0)),
        ("event", json!(webhook::EVENT_IN_SYNC)),
        ("delivery_id", json!(Uuid::nil())),
        ("webhook", json!("probe")),
        ("pod_count", json!(0)),
        ("fired_at", json!(Utc::now())),
        ("target", json!("prod")),
    ] {
        probe.insert(k.into(), v);
    }
    let resolved = webhook::resolve(url, &probe)
        .map_err(|e| bad("invalid_url", format!("the URL cannot be resolved: {e}")))?;
    let allowed = live_policy(state).await.policy.allowed_hosts;
    check_host(&resolved, &allowed).map_err(|e| bad("host_not_allowed", e))?;
    Ok(())
}

fn row_json(r: &sqlx::postgres::PgRow) -> Result<Value, sqlx::Error> {
    Ok(json!({
        "webhook_id": r.try_get::<Uuid, _>("webhook_id")?,
        "gtfs_id": r.try_get::<String, _>("gtfs_id")?,
        "name": r.try_get::<String, _>("name")?,
        "event": r.try_get::<String, _>("event")?,
        // the stored template, which holds ${PLACEHOLDERS} and never a secret
        "url": r.try_get::<String, _>("url")?,
        "method": r.try_get::<String, _>("method")?,
        "headers": r.try_get::<Value, _>("headers")?,
        "body": r.try_get::<Option<Value>, _>("body")?,
        "enabled": r.try_get::<bool, _>("enabled")?,
        "stale_after_seconds": r.try_get::<i32, _>("stale_after_seconds")?,
        "settle_seconds": r.try_get::<i32, _>("settle_seconds")?,
        "give_up_after_seconds": r.try_get::<i32, _>("give_up_after_seconds")?,
        "request_timeout_seconds": r.try_get::<i32, _>("request_timeout_seconds")?,
        "max_attempts": r.try_get::<i32, _>("max_attempts")?,
        "created_at": r.try_get::<chrono::DateTime<Utc>, _>("created_at")?,
        "created_by": r.try_get::<Option<String>, _>("created_by")?,
        "updated_at": r.try_get::<chrono::DateTime<Utc>, _>("updated_at")?,
        "updated_by": r.try_get::<Option<String>, _>("updated_by")?,
    }))
}

const COLS: &str = "webhook_id, gtfs_id, name, event, url, method, headers, body, enabled, \
     stale_after_seconds, settle_seconds, give_up_after_seconds, request_timeout_seconds, \
     max_attempts, created_at, created_by, updated_at, updated_by";

/// The policy in force, as the API reports it everywhere. `source` is the
/// whole point of the shape: an operator looking at an allow-list needs to know
/// whether changing it here will do anything, or whether they are reading the
/// deployment's values because nobody has saved any yet.
fn policy_json(effective: &EffectivePolicy) -> Value {
    json!({
        "enabled": effective.policy.enabled,
        "allowed_hosts": effective.policy.allowed_hosts,
        "active": effective.policy.is_active(),
        "source": effective.source.as_str(),
        "updated_at": effective.updated_at,
        "updated_by": effective.updated_by,
    })
}

pub async fn list(state: &EditorState, gtfs_id: &str) -> EditorResult<Value> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM gtfs_webhook WHERE gtfs_id = $1 ORDER BY name"
    ))
    .bind(gtfs_id)
    .fetch_all(&state.pool)
    .await?;
    let items = rows.iter().map(row_json).collect::<Result<Vec<_>, _>>()?;
    let effective = live_policy(state).await;
    Ok(json!({
        "items": items,
        // what is permitted right now, so the dashboard can say why a URL was
        // refused instead of showing a bare error
        "policy": policy_json(&effective),
        "events": EVENTS,
    }))
}

/// The policy in force, plus what the deployment's own config says, so the
/// dashboard can show what saving would replace - and what it would fall back
/// to if this row had never been written.
pub async fn settings_get(state: &EditorState) -> EditorResult<Value> {
    let effective = live_policy(state).await;
    let seed = state.webhook_policy.seed();
    Ok(json!({
        "policy": policy_json(&effective),
        "config": {
            "enabled": seed.enabled,
            "allowed_hosts": seed.allowed_hosts,
        },
        "max_allowed_hosts": webhook::MAX_ALLOWED_HOSTS,
    }))
}

#[derive(Debug, Clone, Deserialize)]
pub struct SettingsBody {
    pub enabled: Option<bool>,
    pub allowed_hosts: Option<Vec<String>>,
}

/// Save the policy. Both fields are written together, absent ones keeping what
/// is in force, because a half-saved policy is the dangerous kind: turning
/// webhooks on without saying where they may point, or replacing a host list
/// while the switch is stale, are exactly the states worth not creating.
///
/// **Admin only**, and the deployment no longer caps the result: after this
/// runs, the hosts GIMS may call are the ones in this row and not the ones in
/// the configmap. That is the trade the feature makes (docs section 12.5,
/// "What this gives up"), and it is why the endpoint is an admin's and why the
/// audit row carries the whole policy, before and after.
pub async fn settings_update(
    state: &EditorState,
    ctx: &Ctx,
    b: SettingsBody,
) -> EditorResult<Value> {
    ctx.require_admin()?;
    let before = live_policy(state).await;
    let enabled = b.enabled.unwrap_or(before.policy.enabled);
    let hosts = match &b.allowed_hosts {
        Some(h) => webhook::normalise_hosts(h).map_err(|e| bad("invalid_host", e))?,
        None => before.policy.allowed_hosts.clone(),
    };

    sqlx::query(
        "INSERT INTO gtfs_webhook_settings (singleton, enabled, allowed_hosts, updated_by) \
         VALUES (true, $1, $2, $3) \
         ON CONFLICT (singleton) DO UPDATE SET \
           enabled = EXCLUDED.enabled, \
           allowed_hosts = EXCLUDED.allowed_hosts, \
           updated_by = EXCLUDED.updated_by",
    )
    .bind(enabled)
    .bind(&hosts)
    .bind(&ctx.user.email)
    .execute(&state.pool)
    .await?;

    // `from` carries its source, because "it was on" reads very differently
    // when the deployment said so and when a person did
    auth::audit(
        &state.pool,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "webhook_settings_updated",
        None,
        None,
        json!({
            "from": {
                "enabled": before.policy.enabled,
                "allowed_hosts": before.policy.allowed_hosts,
                "source": before.source.as_str(),
            },
            "to": {"enabled": enabled, "allowed_hosts": hosts},
        }),
    )
    .await?;
    settings_get(state).await
}

pub async fn create(
    state: &EditorState,
    ctx: &Ctx,
    gtfs_id: &str,
    b: WebhookBody,
) -> EditorResult<Value> {
    ctx.require_admin()?;
    let name = b
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad("name_required", "name is required"))?
        .to_string();
    let url = b
        .url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad("url_required", "url is required"))?
        .to_string();
    let event = b
        .event
        .as_deref()
        .unwrap_or(webhook::EVENT_IN_SYNC)
        .to_string();
    let method = b.method.as_deref().unwrap_or("POST").to_uppercase();
    let headers = b.headers.clone().unwrap_or_else(|| json!({}));

    check_event(&event)?;
    check_method(&method)?;
    check_headers(&headers)?;
    check_url(state, &url).await?;
    for (field, v) in [
        ("stale_after_seconds", b.stale_after_seconds),
        ("settle_seconds", b.settle_seconds),
        ("give_up_after_seconds", b.give_up_after_seconds),
        ("request_timeout_seconds", b.request_timeout_seconds),
        ("max_attempts", b.max_attempts),
    ] {
        if let Some(v) = v {
            check_limit(field, v)?;
        }
    }

    let row = sqlx::query(&format!(
        "INSERT INTO gtfs_webhook \
           (gtfs_id, name, event, url, method, headers, body, enabled, \
            stale_after_seconds, settle_seconds, give_up_after_seconds, \
            request_timeout_seconds, max_attempts, created_by, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6::jsonb, $7::jsonb, coalesce($8, true), \
            coalesce($9, 60), coalesce($10, 30), coalesce($11, 1800), \
            coalesce($12, 30), coalesce($13, 5), $14, $14) \
         RETURNING {COLS}"
    ))
    .bind(gtfs_id)
    .bind(&name)
    .bind(&event)
    .bind(&url)
    .bind(&method)
    .bind(headers.to_string())
    .bind(b.body.as_ref().map(Value::to_string))
    .bind(b.enabled)
    .bind(b.stale_after_seconds.map(|v| v as i32))
    .bind(b.settle_seconds.map(|v| v as i32))
    .bind(b.give_up_after_seconds.map(|v| v as i32))
    .bind(b.request_timeout_seconds.map(|v| v as i32))
    .bind(b.max_attempts.map(|v| v as i32))
    .bind(&ctx.user.email)
    .fetch_one(&state.pool)
    .await
    .map_err(duplicate_name)?;
    let out = row_json(&row)?;
    auth::audit(
        &state.pool,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "webhook_created",
        Some(gtfs_id),
        None,
        out.clone(),
    )
    .await?;
    Ok(out)
}

fn duplicate_name(e: sqlx::Error) -> EditorError {
    match &e {
        sqlx::Error::Database(db) if db.constraint() == Some("gtfs_webhook_gtfs_id_name_key") => {
            EditorError::conflict(
                "duplicate_name",
                "this feed already has a webhook by that name",
            )
        }
        _ => EditorError::from(e),
    }
}

pub async fn update(
    state: &EditorState,
    ctx: &Ctx,
    webhook_id: Uuid,
    b: WebhookBody,
) -> EditorResult<Value> {
    ctx.require_admin()?;
    if let Some(e) = b.event.as_deref() {
        check_event(e)?;
    }
    if let Some(m) = b.method.as_deref() {
        check_method(&m.to_uppercase())?;
    }
    if let Some(h) = b.headers.as_ref() {
        check_headers(h)?;
    }
    if let Some(u) = b.url.as_deref() {
        check_url(state, u.trim()).await?;
    }
    for (field, v) in [
        ("stale_after_seconds", b.stale_after_seconds),
        ("settle_seconds", b.settle_seconds),
        ("give_up_after_seconds", b.give_up_after_seconds),
        ("request_timeout_seconds", b.request_timeout_seconds),
        ("max_attempts", b.max_attempts),
    ] {
        if let Some(v) = v {
            check_limit(field, v)?;
        }
    }
    // COALESCE per column: an absent field keeps what is stored, so the
    // dashboard can send only what the operator actually changed
    let row = sqlx::query(&format!(
        "UPDATE gtfs_webhook SET \
            name = coalesce($2, name), \
            event = coalesce($3, event), \
            url = coalesce($4, url), \
            method = coalesce($5, method), \
            headers = coalesce($6::jsonb, headers), \
            body = CASE WHEN $7 THEN $8::jsonb ELSE body END, \
            enabled = coalesce($9, enabled), \
            stale_after_seconds = coalesce($10, stale_after_seconds), \
            settle_seconds = coalesce($11, settle_seconds), \
            give_up_after_seconds = coalesce($12, give_up_after_seconds), \
            request_timeout_seconds = coalesce($13, request_timeout_seconds), \
            max_attempts = coalesce($14, max_attempts), \
            updated_by = $15 \
          WHERE webhook_id = $1 RETURNING {COLS}"
    ))
    .bind(webhook_id)
    .bind(b.name.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .bind(b.event.as_deref())
    .bind(b.url.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .bind(b.method.as_deref().map(str::to_uppercase))
    .bind(b.headers.as_ref().map(Value::to_string))
    // `body` is nullable and null is meaningful (send the built-in payload), so
    // "was it sent at all" cannot be read off the value alone
    .bind(b.body.is_some())
    .bind(
        b.body
            .as_ref()
            .filter(|v| !v.is_null())
            .map(Value::to_string),
    )
    .bind(b.enabled)
    .bind(b.stale_after_seconds.map(|v| v as i32))
    .bind(b.settle_seconds.map(|v| v as i32))
    .bind(b.give_up_after_seconds.map(|v| v as i32))
    .bind(b.request_timeout_seconds.map(|v| v as i32))
    .bind(b.max_attempts.map(|v| v as i32))
    .bind(&ctx.user.email)
    .fetch_optional(&state.pool)
    .await
    .map_err(duplicate_name)?
    .ok_or_else(|| EditorError::not_found("webhook_not_found", "no such webhook"))?;
    let out = row_json(&row)?;
    auth::audit(
        &state.pool,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "webhook_updated",
        out["gtfs_id"].as_str(),
        None,
        out.clone(),
    )
    .await?;
    Ok(out)
}

pub async fn delete(state: &EditorState, ctx: &Ctx, webhook_id: Uuid) -> EditorResult<Value> {
    ctx.require_admin()?;
    let row = sqlx::query("DELETE FROM gtfs_webhook WHERE webhook_id = $1 RETURNING gtfs_id, name")
        .bind(webhook_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| EditorError::not_found("webhook_not_found", "no such webhook"))?;
    let gtfs_id: String = row.try_get("gtfs_id")?;
    let name: String = row.try_get("name")?;
    auth::audit(
        &state.pool,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "webhook_deleted",
        Some(&gtfs_id),
        None,
        json!({"webhook_id": webhook_id, "name": name}),
    )
    .await?;
    Ok(json!({"webhook_id": webhook_id, "deleted": true}))
}

/// Queue a one-off delivery. It is sent by whichever pod picks it up, exactly
/// like a real one, so a green test proves the whole path: the allow-list, the
/// placeholders, the pod's egress and the receiver's own authentication.
pub async fn test(state: &EditorState, ctx: &Ctx, webhook_id: Uuid) -> EditorResult<Value> {
    ctx.require_admin()?;
    if !live_policy(state).await.policy.is_active() {
        return Err(bad("webhooks_inactive", WEBHOOKS_INACTIVE));
    }
    let event: Option<String> =
        sqlx::query_scalar("SELECT event FROM gtfs_webhook WHERE webhook_id = $1")
            .bind(webhook_id)
            .fetch_optional(&state.pool)
            .await?;
    // its URL starts a real Nandi build and prod release, past every check the
    // Release to Nandi button makes
    if event.as_deref() == Some(webhook::EVENT_RELEASE_REQUESTED) {
        return Err(bad(
            "release_webhook_untestable",
            "a release webhook starts a real Nandi release; use Release to Nandi instead",
        ));
    }
    let delivery_id = webhook::enqueue_test(&state.pool, webhook_id, &ctx.user.email)
        .await
        .map_err(|_| EditorError::not_found("webhook_not_found", "no such webhook"))?;
    auth::audit(
        &state.pool,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "webhook_tested",
        None,
        None,
        json!({"webhook_id": webhook_id, "delivery_id": delivery_id}),
    )
    .await?;
    Ok(json!({
        "delivery_id": delivery_id,
        "status": "pending",
        "message": "queued; a pod will send it within one poll interval",
    }))
}

pub async fn deliveries(state: &EditorState, gtfs_id: &str, limit: i64) -> EditorResult<Value> {
    let rows = sqlx::query(
        "SELECT d.delivery_id, d.webhook_id, w.name, d.event, d.feed_version, d.kind, d.status, \
                d.attempts, d.next_attempt_at, d.pod_count, d.pods, d.response_status, \
                d.last_error, d.claimed_by, d.requested_by, d.created_at, d.completed_at \
           FROM gtfs_webhook_delivery d \
           LEFT JOIN gtfs_webhook w ON w.webhook_id = d.webhook_id \
          WHERE d.gtfs_id = $1 ORDER BY d.created_at DESC LIMIT $2",
    )
    .bind(gtfs_id)
    .bind(limit.clamp(1, 500))
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            Ok(json!({
                "delivery_id": r.try_get::<Uuid, _>("delivery_id")?,
                "webhook_id": r.try_get::<Uuid, _>("webhook_id")?,
                "webhook": r.try_get::<Option<String>, _>("name")?,
                "event": r.try_get::<String, _>("event")?,
                "feed_version": r.try_get::<i64, _>("feed_version")?,
                "kind": r.try_get::<String, _>("kind")?,
                "status": r.try_get::<String, _>("status")?,
                "attempts": r.try_get::<i32, _>("attempts")?,
                "next_attempt_at": r.try_get::<Option<chrono::DateTime<Utc>>, _>("next_attempt_at")?,
                "pod_count": r.try_get::<Option<i32>, _>("pod_count")?,
                "pods": r.try_get::<Option<Value>, _>("pods")?,
                "response_status": r.try_get::<Option<i32>, _>("response_status")?,
                "last_error": r.try_get::<Option<String>, _>("last_error")?,
                "claimed_by": r.try_get::<Option<String>, _>("claimed_by")?,
                "requested_by": r.try_get::<Option<String>, _>("requested_by")?,
                "created_at": r.try_get::<chrono::DateTime<Utc>, _>("created_at")?,
                "completed_at": r.try_get::<Option<chrono::DateTime<Utc>>, _>("completed_at")?,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"items": items}))
}

// ------------------------------------------------------------ release to Nandi

/// How long a request that Jenkins accepted keeps the button locked while its
/// version is not yet marked released: the release job's own timeout, since
/// the Nandi image build runs well past half an hour, and a build stuck longer
/// than that should not stop anyone asking again.
const RELEASE_WINDOW_MINUTES: i32 = 180;

/// Why the Release to Nandi button may not be pressed now, or `None` when it
/// may. In this order, so the reason names the first thing to fix.
fn release_refusal(
    policy_active: bool,
    has_webhook: bool,
    version: i64,
    released: Option<i64>,
    in_progress: bool,
    is_master: bool,
) -> Option<(StatusCode, &'static str, &'static str)> {
    if !policy_active {
        return Some((
            StatusCode::BAD_REQUEST,
            "webhooks_inactive",
            WEBHOOKS_INACTIVE,
        ));
    }
    if !has_webhook {
        return Some((
            StatusCode::CONFLICT,
            "no_release_webhook",
            "add a webhook for release_requested to point this button at the Jenkins job",
        ));
    }
    // released_version is prod's; master never records a release
    if !is_master && released.is_some_and(|r| r >= version) {
        return Some((
            StatusCode::CONFLICT,
            "nothing_to_release",
            "Nandi already has the committed version",
        ));
    }
    if in_progress {
        return Some((
            StatusCode::CONFLICT,
            "release_in_progress",
            "a release is already on its way",
        ));
    }
    None
}

struct ReleaseState {
    version: i64,
    released_version: Option<i64>,
    released_at: Option<chrono::DateTime<Utc>>,
    has_webhook: bool,
    in_progress: bool,
    last_request: Value,
}

impl ReleaseState {
    fn refusal(
        &self,
        policy_active: bool,
        is_master: bool,
    ) -> Option<(StatusCode, &'static str, &'static str)> {
        release_refusal(
            policy_active,
            self.has_webhook,
            self.version,
            self.released_version,
            self.in_progress,
            is_master,
        )
    }
}

fn target(is_master: bool) -> &'static str {
    if is_master {
        "master"
    } else {
        "prod"
    }
}

async fn release_state(
    conn: &mut PgConnection,
    gtfs_id: &str,
    target: &str,
) -> EditorResult<ReleaseState> {
    let f = sqlx::query(
        "SELECT f.version, f.released_version, f.released_at, \
                EXISTS (SELECT 1 FROM gtfs_webhook w \
                         WHERE w.gtfs_id = f.gtfs_id AND w.enabled \
                           AND w.event = 'release_requested') AS has_webhook, \
                EXISTS (SELECT 1 FROM gtfs_webhook_delivery d \
                         WHERE d.gtfs_id = f.gtfs_id AND d.kind = 'release' AND d.target = $3 \
                           AND d.created_at > now() - make_interval(mins => $2) \
                           AND (d.status IN ('pending', 'in_flight') \
                                OR ($3 = 'prod' AND d.status = 'succeeded' \
                                    AND d.feed_version > coalesce(f.released_version, 0)))) \
                  AS in_progress, \
                l.delivery_id, l.requested_by, l.created_at, l.status, l.response_status, \
                l.feed_version \
           FROM gtfs_feed f \
           LEFT JOIN LATERAL ( \
                SELECT delivery_id, requested_by, created_at, status, response_status, feed_version \
                  FROM gtfs_webhook_delivery \
                 WHERE gtfs_id = f.gtfs_id AND kind = 'release' AND target = $3 \
                 ORDER BY created_at DESC LIMIT 1) l ON true \
          WHERE f.gtfs_id = $1",
    )
    .bind(gtfs_id)
    .bind(RELEASE_WINDOW_MINUTES)
    .bind(target)
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| EditorError::not_found("feed_not_found", format!("no feed {gtfs_id}")))?;
    let last_request = match f.try_get::<Option<Uuid>, _>("delivery_id")? {
        Some(id) => json!({
            "delivery_id": id,
            "requested_by": f.try_get::<Option<String>, _>("requested_by")?,
            "created_at": f.try_get::<chrono::DateTime<Utc>, _>("created_at")?,
            "status": f.try_get::<String, _>("status")?,
            "response_status": f.try_get::<Option<i32>, _>("response_status")?,
            "feed_version": f.try_get::<i64, _>("feed_version")?,
        }),
        None => Value::Null,
    };
    Ok(ReleaseState {
        version: f.try_get("version")?,
        released_version: f.try_get("released_version")?,
        released_at: f.try_get("released_at")?,
        has_webhook: f.try_get("has_webhook")?,
        in_progress: f.try_get("in_progress")?,
        last_request,
    })
}

/// The Release to Nandi button: queue the request for the committed version.
/// The check and the insert share one transaction under the feed lock, so two
/// clicks at once queue one release.
pub async fn release(state: &EditorState, ctx: &Ctx, gtfs_id: &str) -> EditorResult<Value> {
    ctx.require_feed_role(gtfs_id, super::auth::Role::Approver)?;
    let active = live_policy(state).await.policy.is_active();
    let mut tx = state.pool.begin().await?;
    super::feed_lock::lock_feed(&mut tx, gtfs_id).await?;
    let target = target(state.is_master);
    let rs = release_state(&mut tx, gtfs_id, target).await?;
    if let Some((status, code, message)) = rs.refusal(active, state.is_master) {
        return Err(EditorError::new(status, code, message));
    }
    let ids = webhook::enqueue_release(&mut tx, gtfs_id, &ctx.user.email, target)
        .await
        .map_err(|e| EditorError::internal(e.to_string()))?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "release_requested",
        Some(gtfs_id),
        None,
        json!({"feed_version": rs.version, "target": target, "delivery_ids": ids}),
    )
    .await?;
    tx.commit().await?;
    Ok(
        json!({"delivery_ids": ids, "feed_version": rs.version, "target": target, "status": "pending"}),
    )
}

/// Which version of the feed each pod is serving, and whether the fleet has
/// caught up with the committed one. This is the answer to "is my edit live
/// yet" - and, when a webhook has not fired, to "what is it waiting for".
pub async fn cache_state(state: &EditorState, gtfs_id: &str) -> EditorResult<Value> {
    let feed =
        sqlx::query("SELECT version, data_source, updated_at FROM gtfs_feed WHERE gtfs_id = $1")
            .bind(gtfs_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| {
                EditorError::not_found("feed_not_found", format!("no feed {gtfs_id}"))
            })?;
    let version: i64 = feed.try_get("version")?;
    let data_source: String = feed.try_get("data_source")?;
    let version_at: chrono::DateTime<Utc> = feed.try_get("updated_at")?;

    let rows = sqlx::query(
        "SELECT pod_id, loaded_version, loaded_at, updated_at, data_source, \
                failing_version, last_error, image_tag, started_at \
           FROM gtfs_pod_feed_state WHERE gtfs_id = $1 ORDER BY pod_id",
    )
    .bind(gtfs_id)
    .fetch_all(&state.pool)
    .await?;

    // the window a reader sees is the shortest any webhook on this feed uses,
    // so the dashboard never claims a pod is healthy that a delivery counts out
    let stale_after: i64 = sqlx::query(
        "SELECT min(stale_after_seconds)::bigint AS s FROM gtfs_webhook \
          WHERE gtfs_id = $1 AND enabled",
    )
    .bind(gtfs_id)
    .fetch_one(&state.pool)
    .await?
    .try_get::<Option<i64>, _>("s")?
    .unwrap_or(60);

    let mut pods = Vec::new();
    let mut states = Vec::new();
    for r in &rows {
        let p = PodState {
            pod_id: r.try_get("pod_id")?,
            loaded_version: r.try_get("loaded_version")?,
            loaded_at: r.try_get("loaded_at")?,
            updated_at: r.try_get("updated_at")?,
            data_source: r.try_get("data_source")?,
            failing_version: r.try_get("failing_version")?,
            last_error: r.try_get("last_error")?,
        };
        pods.push(json!({
            "pod_id": p.pod_id,
            "loaded_version": p.loaded_version,
            "loaded_at": p.loaded_at,
            "last_seen_at": p.updated_at,
            "data_source": p.data_source,
            "failing_version": p.failing_version,
            "last_error": p.last_error,
            "image_tag": r.try_get::<Option<String>, _>("image_tag")?,
            "started_at": r.try_get::<chrono::DateTime<Utc>, _>("started_at")?,
            "up_to_date": p.data_source == "db" && p.loaded_version == version,
        }));
        states.push(p);
    }

    let now = Utc::now();
    let fleet = fleet_from(states, now, stale_after);
    let in_sync = !fleet.live.is_empty() && fleet.laggards(version).is_empty();
    let rs = release_state(
        &mut *state.pool.acquire().await?,
        gtfs_id,
        target(state.is_master),
    )
    .await?;
    let refusal = rs.refusal(live_policy(state).await.policy.is_active(), state.is_master);
    Ok(json!({
        "gtfs_id": gtfs_id,
        "data_source": data_source,
        "version": version,
        "version_at": version_at,
        "in_sync": in_sync,
        "live_pods": fleet.live.len(),
        "stale_pods": fleet.stale.len(),
        "fleet_version": fleet.min_version(),
        "settled_at": fleet.settled_at(version),
        "stale_after_seconds": stale_after,
        "waiting_for": fleet
            .laggards(version)
            .iter()
            .map(|p| json!({"pod_id": p.pod_id, "loaded_version": p.loaded_version}))
            .collect::<Vec<_>>(),
        "pods": pods,
        "release": {
            "released_version": rs.released_version,
            "released_at": rs.released_at,
            "last_request": rs.last_request,
            "target": target(state.is_master),
            "can_release": refusal.is_none(),
            "reason": refusal.map(|(_, code, _)| code),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_webhook_must_name_a_known_event_and_method() {
        assert!(check_event("feed_in_sync").is_ok());
        assert!(check_event("feed_committed").is_ok());
        assert!(check_event("whenever").is_err());
        assert!(check_method("POST").is_ok());
        assert!(check_method("DELETE").is_err());
        // lowercase is the caller's job to normalise, so it is refused here
        assert!(check_method("post").is_err());
    }

    #[test]
    fn a_release_is_refused_for_the_first_thing_to_fix() {
        let code = |active, hook, v, rel, busy| {
            release_refusal(active, hook, v, rel, busy, false).map(|(_, c, _)| c)
        };
        assert_eq!(
            code(false, false, 2, Some(1), true),
            Some("webhooks_inactive")
        );
        assert_eq!(
            code(true, false, 2, Some(1), true),
            Some("no_release_webhook")
        );
        assert_eq!(
            code(true, true, 2, Some(2), true),
            Some("nothing_to_release")
        );
        assert_eq!(
            code(true, true, 2, Some(5), false),
            Some("nothing_to_release")
        );
        assert_eq!(
            code(true, true, 2, Some(1), true),
            Some("release_in_progress")
        );
        assert_eq!(code(true, true, 2, Some(1), false), None);
        // a feed never released has everything to release
        assert_eq!(code(true, true, 1, None, false), None);
        // master never records a release, so prod's released_version is no bar
        let master = |rel, busy| release_refusal(true, true, 2, rel, busy, true).map(|(_, c, _)| c);
        assert_eq!(master(Some(2), false), None);
        assert_eq!(master(Some(2), true), Some("release_in_progress"));
    }

    #[test]
    fn release_requested_is_a_known_event() {
        assert!(check_event("release_requested").is_ok());
    }

    #[test]
    fn the_numeric_limits_match_the_database_constraints() {
        assert!(check_limit("settle_seconds", 0).is_ok());
        assert!(check_limit("settle_seconds", 3600).is_ok());
        assert!(check_limit("settle_seconds", 3601).is_err());
        assert!(check_limit("stale_after_seconds", 9).is_err());
        assert!(check_limit("max_attempts", 21).is_err());
        // an unknown field is not this function's business to reject
        assert!(check_limit("something_else", -1).is_ok());
    }

    #[test]
    fn headers_must_be_a_flat_object_of_strings() {
        assert!(check_headers(&json!({"Authorization": "Bearer ${T}"})).is_ok());
        assert!(check_headers(&json!({})).is_ok());
        assert!(check_headers(&json!([])).is_err());
        assert!(check_headers(&json!({"X": 1})).is_err());
        assert!(check_headers(&json!({"": "v"})).is_err());
    }
}
