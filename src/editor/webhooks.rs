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
//! That risk is answered where it belongs, in the deployment: a URL must match
//! `gtfs_webhook_allowed_hosts` from the dhall config, checked when it is saved
//! and again when the request is about to go out. A dashboard admin chooses the
//! URL within that list, and cannot widen the list. Every change is audited.

use super::auth::{self, Ctx, Role};
use super::error::{EditorError, EditorResult};
use super::EditorState;
use crate::services::webhook::{self, check_host, fleet_from, PodState};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::Row;
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
];

const METHODS: &[&str] = &["POST", "PUT", "GET"];

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

/// A URL is usable if this deployment allows its host *and* every placeholder
/// in it can be resolved right now. Catching a missing environment variable
/// here, rather than at the first delivery, is the difference between a typo
/// the admin sees immediately and a cache that silently never refreshes.
fn check_url(state: &EditorState, url: &str) -> EditorResult<()> {
    let mut probe = Map::new();
    for (k, v) in [
        ("gtfs_id", json!("probe")),
        ("feed_version", json!(0)),
        ("event", json!(webhook::EVENT_IN_SYNC)),
        ("delivery_id", json!(Uuid::nil())),
        ("webhook", json!("probe")),
        ("pod_count", json!(0)),
        ("fired_at", json!(Utc::now())),
    ] {
        probe.insert(k.into(), v);
    }
    let resolved = webhook::resolve(url, &probe)
        .map_err(|e| bad("invalid_url", format!("the URL cannot be resolved: {e}")))?;
    check_host(&resolved, &state.webhook_policy.allowed_hosts)
        .map_err(|e| bad("host_not_allowed", e))?;
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

pub async fn list(state: &EditorState, gtfs_id: &str) -> EditorResult<Value> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM gtfs_webhook WHERE gtfs_id = $1 ORDER BY name"
    ))
    .bind(gtfs_id)
    .fetch_all(&state.pool)
    .await?;
    let items = rows.iter().map(row_json).collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "items": items,
        // what the deployment permits, so the dashboard can say why a URL was
        // refused instead of showing a bare error
        "policy": {
            "enabled": state.webhook_policy.enabled,
            "allowed_hosts": state.webhook_policy.allowed_hosts,
            "active": state.webhook_policy.is_active(),
        },
        "events": EVENTS,
    }))
}

pub async fn create(
    state: &EditorState,
    ctx: &Ctx,
    gtfs_id: &str,
    b: WebhookBody,
) -> EditorResult<Value> {
    ctx.require_role(Role::Admin)?;
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
    check_url(state, &url)?;
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
    ctx.require_role(Role::Admin)?;
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
        check_url(state, u.trim())?;
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
    ctx.require_role(Role::Admin)?;
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
    ctx.require_role(Role::Admin)?;
    if !state.webhook_policy.is_active() {
        return Err(bad(
            "webhooks_inactive",
            "this deployment has no webhook allow-list, so nothing would be sent",
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
