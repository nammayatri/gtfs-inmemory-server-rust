//! Outbound webhooks, and the per-pod cache state one of their events is built
//! on. See `docs/gtfs-editor.md` section 12, `db/gtfs_editor/0013_webhooks.sql`
//! and `db/gtfs_editor/0016_webhook_settings.sql`.
//!
//! # What this is for
//!
//! GIMS holds every feed in memory. An editor commit bumps `gtfs_feed.version`,
//! each pod's poll loop notices and rebuilds that feed, and for a few seconds
//! the fleet is mixed: some pods answer with the new data, some with the old.
//!
//! Anything downstream that caches GIMS's answers has to be told *after* that
//! settles. The frontline layer is static files on S3 behind CloudFront,
//! rebuilt and invalidated by a Jenkins job; run that job on commit and a pod
//! still holding the previous version can serve the request that the freshly
//! invalidated CloudFront passes through, and the stale answer is cached again.
//!
//! So each pod reports the version it has loaded ([`heartbeat`]), and a webhook
//! on the `feed_in_sync` event fires once every live pod is at the committed
//! version. Nothing observes a pod from outside: the process that owns the
//! cache is the one that reports it.
//!
//! # Exactly once, without a leader
//!
//! Every pod runs the same dispatcher, and they all notice the fleet has
//! settled within a tick of each other. They all try to insert the delivery
//! row; the partial unique index on `(webhook_id, feed_version)` lets exactly
//! one through, and only that pod sends the request. There is no leader to
//! elect and nothing to fail over.
//!
//! A claimed delivery whose pod dies is reclaimed by any other pod once the
//! claim goes stale, so a kill mid-request costs a retry, not a delivery.

use crate::tools::error::{AppError, AppResult};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{json, Map, Value};
use sqlx::postgres::PgPool;
use sqlx::PgConnection;
use sqlx::Row;
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Events a webhook can subscribe to. One webhook row picks one.
pub const EVENT_IN_SYNC: &str = "feed_in_sync";
pub const EVENT_COMMITTED: &str = "feed_committed";
pub const EVENT_RELOAD_FAILED: &str = "feed_reload_failed";
/// Sent only when an approver asks for it (the Release to Nandi button), never
/// by `enqueue_due`.
pub const EVENT_RELEASE_REQUESTED: &str = "release_requested";

/// A claim older than this is assumed to belong to a pod that died mid-request
/// and is handed back. Well clear of the largest allowed request timeout (300s)
/// so a slow-but-alive request is never stolen and sent twice.
const CLAIM_STALE_SECONDS: i64 = 600;

/// How many deliveries one pod sends per tick. Deliveries are rare (one per
/// commit) and the cap only matters when a backlog has built up behind an
/// outage; sending them a few per tick keeps one pod from blocking its poll
/// loop on a long queue of slow requests.
const MAX_DELIVERIES_PER_TICK: usize = 4;

// ---------------------------------------------------------------- identity

/// Who this pod says it is in `gtfs_pod_feed_state`.
#[derive(Debug, Clone)]
pub struct PodIdentity {
    pub pod_id: String,
    /// New on every process start, so a restarted pod that kept its name is
    /// not mistaken for the instance that had already loaded the feed.
    pub boot_id: Uuid,
    pub image_tag: Option<String>,
}

impl PodIdentity {
    /// The configured id, else the downward-API `POD_NAME`, else the hostname.
    /// A pod with no identity at all still gets a unique one, because two pods
    /// sharing a `pod_id` would overwrite each other's heartbeat and the fleet
    /// would look smaller than it is - which would fire a webhook early.
    pub fn from_env(configured: Option<&str>) -> Self {
        let boot_id = Uuid::new_v4();
        let pod_id = configured
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| std::env::var("POD_NAME").ok().filter(|s| !s.is_empty()))
            .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| format!("unnamed-{}", &boot_id.to_string()[..8]));
        Self {
            pod_id,
            boot_id,
            image_tag: std::env::var("IMAGE_TAG").ok().filter(|s| !s.is_empty()),
        }
    }
}

/// Whether webhooks may fire at all, and which hosts they may be pointed at.
/// A dashboard admin configures *which* URL is called, but only within these
/// hosts.
///
/// Where the values came from is [`PolicySource`]: the `gtfs_webhook_settings`
/// row if there is one, else the dhall config that seeded it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebhookPolicy {
    pub enabled: bool,
    /// Hosts a webhook URL may point at. Empty means no webhook can fire:
    /// failing closed, so turning the feature on is a deliberate act and not
    /// something a half-filled form does by accident.
    pub allowed_hosts: Vec<String>,
}

impl WebhookPolicy {
    pub fn is_active(&self) -> bool {
        self.enabled && !self.allowed_hosts.is_empty()
    }
}

/// Which of the two places the policy in force was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicySource {
    /// The `gtfs_webhook_settings` row, saved from the dashboard.
    Database,
    /// There is no row, so the deployment's dhall values are still in force.
    Config,
}

impl PolicySource {
    /// The word the API and the dashboard use for it.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicySource::Database => "database",
            PolicySource::Config => "config",
        }
    }
}

/// The policy in force, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectivePolicy {
    pub policy: WebhookPolicy,
    pub source: PolicySource,
    /// When the row was last saved, and by whom; both `None` for `Config`.
    pub updated_at: Option<DateTime<Utc>>,
    pub updated_by: Option<String>,
}

/// The `gtfs_webhook_settings` row, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPolicy {
    pub enabled: bool,
    pub allowed_hosts: Vec<String>,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<String>,
}

/// The precedence rule: **a stored row wins outright, and the dhall value is
/// only the seed used while there is no row.**
///
/// Deliberately the same rule `gtfs_feed.data_source` has over the static
/// `gtfs_db_feeds` list (`gtfs_db_source::merge_live_feeds`), so this system
/// has one answer to "which of these two wins" rather than two. A row saying
/// `enabled = false`, or one with an empty host list, therefore overrides a
/// permissive deployment config instead of being merged with it: a union would
/// make turning something *off* from the dashboard impossible.
///
/// Pure, so the rule is tested without a database.
pub fn resolve_policy(row: Option<StoredPolicy>, seed: &WebhookPolicy) -> EffectivePolicy {
    match row {
        Some(r) => EffectivePolicy {
            policy: WebhookPolicy {
                enabled: r.enabled,
                allowed_hosts: r.allowed_hosts,
            },
            source: PolicySource::Database,
            updated_at: Some(r.updated_at),
            updated_by: r.updated_by,
        },
        None => EffectivePolicy {
            policy: seed.clone(),
            source: PolicySource::Config,
            updated_at: None,
            updated_by: None,
        },
    }
}

/// The longest allow-list that is still a list someone reads, and a bound on
/// what one careless paste can put in the row.
pub const MAX_ALLOWED_HOSTS: usize = 64;

/// The longest a host name may be, plus the leading `.` of a subdomain entry.
const MAX_HOST_LEN: usize = 254;

/// Normalise and check one allow-list entry. An entry is a bare host name, or
/// one written `.example.com` to match that domain and its subdomains, because
/// that is all [`check_host`] ever compares against: a scheme, port or path in
/// here would match nothing at all, so it is a typo to report rather than
/// something to quietly strip.
pub fn normalise_host(raw: &str) -> Result<String, String> {
    let host = raw.trim().to_ascii_lowercase();
    if host.is_empty() {
        return Err("a host cannot be empty".to_string());
    }
    if host.len() > MAX_HOST_LEN {
        return Err(format!("{host} is too long to be a host name"));
    }
    for (what, found) in [
        ("a scheme", host.contains("://")),
        ("a port", host.contains(':')),
        ("a path", host.contains('/')),
        ("a query", host.contains('?') || host.contains('#')),
        ("a user", host.contains('@')),
        ("a space", host.chars().any(char::is_whitespace)),
        ("a wildcard", host.contains('*')),
    ] {
        if found {
            return Err(format!(
                "{host} has {what}; an entry is a host name such as jenkins.example.com, \
                 or .example.com for a domain and its subdomains"
            ));
        }
    }
    // the leading dot is the subdomain marker, not a label of its own
    let labels = host.strip_prefix('.').unwrap_or(&host);
    if labels.is_empty() {
        return Err("a host cannot be just a dot".to_string());
    }
    for label in labels.split('.') {
        if label.is_empty() {
            return Err(format!("{host} has an empty part between two dots"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!("{host} has a part that starts or ends with a dash"));
        }
        if !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("{host} has a part that is not a host name"));
        }
    }
    Ok(host)
}

/// [`normalise_host`] over a whole list, de-duplicated, order kept.
///
/// An empty list is valid and means nothing can fire: turning webhooks on
/// before deciding where they may point is a legitimate half-step, and it fails
/// closed, so it needs no refusing.
pub fn normalise_hosts(raw: &[String]) -> Result<Vec<String>, String> {
    if raw.len() > MAX_ALLOWED_HOSTS {
        return Err(format!(
            "an allow-list holds at most {MAX_ALLOWED_HOSTS} hosts"
        ));
    }
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for entry in raw {
        let host = normalise_host(entry)?;
        if !out.contains(&host) {
            out.push(host);
        }
    }
    Ok(out)
}

// ------------------------------------------------------------ live settings

/// Read the one `gtfs_webhook_settings` row, if it has ever been saved.
pub async fn load_policy_row(pool: &PgPool) -> AppResult<Option<StoredPolicy>> {
    let row = sqlx::query(
        "SELECT enabled, allowed_hosts, updated_at, updated_by FROM gtfs_webhook_settings",
    )
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    row.map(|r| {
        Ok(StoredPolicy {
            enabled: r.try_get("enabled").map_err(db)?,
            allowed_hosts: r.try_get("allowed_hosts").map_err(db)?,
            updated_at: r.try_get("updated_at").map_err(db)?,
            updated_by: r.try_get("updated_by").map_err(db)?,
        })
    })
    .transpose()
}

/// The dhall seed, plus the one read that turns it into the policy in force.
///
/// Everything that acts on the policy holds one of these rather than a
/// [`WebhookPolicy`], because the value has to be read *now*: an admin turning
/// webhooks on from the dashboard must take effect within a poll interval, and
/// a host removed from the list must stop being callable at the next request,
/// neither of them at the next restart.
#[derive(Debug, Default)]
pub struct LivePolicy {
    seed: WebhookPolicy,
    /// A persistent read failure - almost always
    /// `db/gtfs_editor/0016_webhook_settings.sql` not applied yet - is worth
    /// one line, not one per poll tick.
    warned: std::sync::atomic::AtomicBool,
}

impl LivePolicy {
    pub fn new(seed: WebhookPolicy) -> Self {
        Self {
            seed,
            warned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// What the deployment's config says, which is the policy in force only
    /// while nothing has been saved from the dashboard.
    pub fn seed(&self) -> &WebhookPolicy {
        &self.seed
    }

    /// The policy in force right now. Never fails: a database that cannot
    /// answer falls back to the deployment's own values, which is what was in
    /// force before this table existed, rather than to a policy nobody chose.
    pub async fn get(&self, pool: &PgPool) -> EffectivePolicy {
        match load_policy_row(pool).await {
            Ok(row) => resolve_policy(row, &self.seed),
            Err(e) => {
                if !self.warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    warn!(
                        "Reading the webhook settings failed ({e}); using this deployment's \
                         gtfs_webhooks_enabled and gtfs_webhook_allowed_hosts instead. Has \
                         db/gtfs_editor/0016_webhook_settings.sql been applied? This is logged once."
                    );
                }
                resolve_policy(None, &self.seed)
            }
        }
    }
}

// ---------------------------------------------------------------- pure: urls

/// Is `url` one this deployment allows a webhook to call? Matches the host
/// exactly, or as a subdomain of an entry written `.example.com`.
///
/// Checked when the URL is saved *and* again when it is about to be sent: the
/// allow-list can be tightened after a webhook was configured, and the check
/// that matters is the one at the moment of the request.
pub fn check_host(url: &str, allowed: &[String]) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("not a URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        s => return Err(format!("scheme {s} is not allowed; use http or https")),
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "the URL has no host".to_string())?
        .to_ascii_lowercase();
    let ok = allowed.iter().any(|a| {
        let a = a.trim().to_ascii_lowercase();
        match a.strip_prefix('.') {
            Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
            None => host == a,
        }
    });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "host {host} is not in this deployment's webhook allow-list"
        ))
    }
}

/// Resolve `${...}` placeholders in `template`.
///
/// - `${NAME}` and `${env:NAME}` read the pod's environment, which is where a
///   credential lives: it stays in the mounted secret and never reaches the
///   database, the API or a log line.
/// - `${event:field}` reads a field of the event payload (`gtfs_id`,
///   `feed_version`, `event`, `pod_count`, `delivery_id`).
///
/// An unknown placeholder is an error, never a literal: sending `${JENKINS_TOKEN}`
/// as text would put a useless request on the wire and, worse, make a missing
/// secret look like a server-side rejection rather than a configuration fault.
pub fn resolve(template: &str, event: &Map<String, Value>) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| "unterminated ${...} placeholder".to_string())?;
        let name = after[..end].trim();
        out.push_str(&lookup(name, event)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn lookup(name: &str, event: &Map<String, Value>) -> Result<String, String> {
    let (ns, key) = match name.split_once(':') {
        Some((ns, key)) => (ns.trim(), key.trim()),
        None => ("env", name),
    };
    match ns {
        "env" => std::env::var(key)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("environment variable {key} is not set on this pod")),
        "event" => match event.get(key) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(Value::Null) | None => Err(format!("the event has no field {key}")),
            Some(v) => Ok(v.to_string()),
        },
        other => Err(format!(
            "unknown placeholder namespace {other}; use env: or event:"
        )),
    }
}

/// [`resolve`] over every string leaf of a JSON body, keys included.
pub fn resolve_json(v: &Value, event: &Map<String, Value>) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) => Value::String(resolve(s, event)?),
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| resolve_json(x, event))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, val) in o {
                out.insert(resolve(k, event)?, resolve_json(val, event)?);
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

/// Retry delay after `attempts` failures: 30s, 1m, 2m, 4m, capped at 15m.
/// The receiver here is a build system, so retrying for a long while is much
/// better than dropping the request - a missed delivery means CloudFront serves
/// yesterday's data until someone notices.
pub fn backoff_seconds(attempts: i32) -> i64 {
    let n = attempts.clamp(1, 16) - 1;
    (30i64 << n.min(5)).min(900)
}

// ---------------------------------------------------------------- pure: fleet

/// One pod's reported state, as the fleet check sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct PodState {
    pub pod_id: String,
    pub loaded_version: i64,
    pub loaded_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub data_source: String,
    pub failing_version: Option<i64>,
    pub last_error: Option<String>,
}

impl PodState {
    fn json(&self) -> Value {
        json!({
            "pod_id": self.pod_id,
            "loaded_version": self.loaded_version,
            "loaded_at": self.loaded_at,
            "updated_at": self.updated_at,
        })
    }
}

/// What the live pods of one feed are serving.
#[derive(Debug, Clone, PartialEq)]
pub struct Fleet {
    /// Pods heartbeating within the staleness window and serving this feed
    /// from the DB. Only these count: a pod on preprocessed data has no feed
    /// version to be in sync with, and a silent pod is treated as gone.
    pub live: Vec<PodState>,
    /// Pods present but too old to count, named so the dashboard can show why
    /// a delivery is waiting.
    pub stale: Vec<PodState>,
}

impl Fleet {
    /// The oldest version any live pod is serving: the version the whole fleet
    /// can be said to be at.
    pub fn min_version(&self) -> Option<i64> {
        self.live.iter().map(|p| p.loaded_version).min()
    }

    /// When the last pod arrived at `version`; the settle window runs from here.
    pub fn settled_at(&self, version: i64) -> Option<DateTime<Utc>> {
        self.live
            .iter()
            .filter(|p| p.loaded_version == version)
            .map(|p| p.loaded_at)
            .max()
    }

    pub fn laggards(&self, version: i64) -> Vec<&PodState> {
        self.live
            .iter()
            .filter(|p| p.loaded_version != version)
            .collect()
    }

    fn pods_json(&self) -> Value {
        Value::Array(self.live.iter().map(PodState::json).collect())
    }
}

/// Split reported pods into live and stale at `now`.
pub fn fleet_from(pods: Vec<PodState>, now: DateTime<Utc>, stale_after_seconds: i64) -> Fleet {
    let cutoff = now - ChronoDuration::seconds(stale_after_seconds);
    let mut live = Vec::new();
    let mut stale = Vec::new();
    for p in pods {
        if p.updated_at < cutoff {
            stale.push(p);
        } else if p.data_source == "db" {
            live.push(p);
        }
        // a live pod on preprocessed data is in neither list: it is healthy,
        // but this feed's version means nothing to it
    }
    live.sort_by(|a, b| a.pod_id.cmp(&b.pod_id));
    stale.sort_by(|a, b| a.pod_id.cmp(&b.pod_id));
    Fleet { live, stale }
}

/// What to do about one feed version, for a `feed_in_sync` webhook.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Every live pod is at `version` and has been for the settle window.
    Fire,
    /// Not yet; the reason is shown in the dashboard, not acted on.
    Wait(String),
    /// Waited past `give_up_after_seconds`. Recorded as an abandoned delivery
    /// so the failure is loud: the alternative, firing anyway, would rebuild
    /// the downstream cache from a fleet we know is inconsistent.
    GiveUp(String),
}

/// Should a `feed_in_sync` webhook fire for `version` now?
///
/// `version_at` is when the feed row last moved, which is when this version
/// appeared - the clock the give-up window runs on.
pub fn in_sync_verdict(
    fleet: &Fleet,
    version: i64,
    version_at: DateTime<Utc>,
    now: DateTime<Utc>,
    settle_seconds: i64,
    give_up_after_seconds: i64,
) -> Verdict {
    let waited = (now - version_at).num_seconds();
    let give_up = |why: String| {
        if waited > give_up_after_seconds {
            Verdict::GiveUp(why.clone())
        } else {
            Verdict::Wait(why)
        }
    };

    if fleet.live.is_empty() {
        // No pod is serving this feed from the DB. Firing would tell the
        // frontline to rebuild from data nothing is actually serving.
        return give_up(match fleet.stale.len() {
            0 => "no pod is serving this feed from the database".to_string(),
            n => format!("no pod is heartbeating; {n} last reported before the staleness window"),
        });
    }
    let laggards = fleet.laggards(version);
    if !laggards.is_empty() {
        let names: Vec<_> = laggards
            .iter()
            .take(5)
            .map(|p| format!("{} at v{}", p.pod_id, p.loaded_version))
            .collect();
        return give_up(format!(
            "{} of {} pods have not loaded v{version} yet: {}",
            laggards.len(),
            fleet.live.len(),
            names.join(", ")
        ));
    }
    match fleet.settled_at(version) {
        Some(at) if (now - at).num_seconds() >= settle_seconds => Verdict::Fire,
        Some(at) => Verdict::Wait(format!(
            "all {} pods are at v{version}; settling for another {}s",
            fleet.live.len(),
            settle_seconds - (now - at).num_seconds()
        )),
        // unreachable while laggards is empty and live is not, but a Wait here
        // is the safe reading of a fleet we could not measure
        None => Verdict::Wait("the fleet's arrival time is unknown".to_string()),
    }
}

// ---------------------------------------------------------------- heartbeat

/// Report what this pod is serving for one feed.
///
/// Called on every poll tick, not only when the version changes: the row is
/// how other pods know this one is alive, and a pod that stopped writing it
/// would silently drop out of the fleet and let a webhook fire without it.
///
/// `loaded_at` only moves when the version does, because the settle window is
/// measured from when the last pod *arrived* at a version.
#[allow(clippy::too_many_arguments)]
pub async fn heartbeat(
    pool: &PgPool,
    pod: &PodIdentity,
    gtfs_id: &str,
    loaded_version: i64,
    data_source: &str,
    failure: Option<(i64, String)>,
) -> AppResult<()> {
    let (failing_version, last_error) = match failure {
        Some((v, e)) => (Some(v), Some(e.chars().take(2000).collect::<String>())),
        None => (None, None),
    };
    sqlx::query(
        "INSERT INTO gtfs_pod_feed_state \
           (gtfs_id, pod_id, boot_id, loaded_version, loaded_at, data_source, \
            last_error, failing_version, image_tag, started_at, updated_at) \
         VALUES ($1, $2, $3, $4, now(), $5, $6, $7, $8, now(), now()) \
         ON CONFLICT (gtfs_id, pod_id) DO UPDATE SET \
           boot_id = EXCLUDED.boot_id, \
           loaded_version = EXCLUDED.loaded_version, \
           loaded_at = CASE \
             WHEN gtfs_pod_feed_state.loaded_version = EXCLUDED.loaded_version \
              AND gtfs_pod_feed_state.boot_id = EXCLUDED.boot_id \
             THEN gtfs_pod_feed_state.loaded_at ELSE now() END, \
           data_source = EXCLUDED.data_source, \
           last_error = EXCLUDED.last_error, \
           failing_version = EXCLUDED.failing_version, \
           image_tag = EXCLUDED.image_tag, \
           started_at = CASE \
             WHEN gtfs_pod_feed_state.boot_id = EXCLUDED.boot_id \
             THEN gtfs_pod_feed_state.started_at ELSE now() END, \
           updated_at = now()",
    )
    .bind(gtfs_id)
    .bind(&pod.pod_id)
    .bind(pod.boot_id)
    .bind(loaded_version)
    .bind(data_source)
    .bind(last_error)
    .bind(failing_version)
    .bind(pod.image_tag.as_deref())
    .execute(pool)
    .await
    .map_err(|e| AppError::Internal(format!("pod heartbeat for {gtfs_id}: {e}")))?;
    Ok(())
}

/// Every pod's reported state for one feed.
pub async fn pods_of(pool: &PgPool, gtfs_id: &str) -> AppResult<Vec<PodState>> {
    let rows = sqlx::query(
        "SELECT pod_id, loaded_version, loaded_at, updated_at, data_source, \
                failing_version, last_error \
           FROM gtfs_pod_feed_state WHERE gtfs_id = $1",
    )
    .bind(gtfs_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AppError::Internal(format!("pod state for {gtfs_id}: {e}")))?;
    rows.iter()
        .map(|r| {
            Ok(PodState {
                pod_id: r.try_get("pod_id").map_err(db)?,
                loaded_version: r.try_get("loaded_version").map_err(db)?,
                loaded_at: r.try_get("loaded_at").map_err(db)?,
                updated_at: r.try_get("updated_at").map_err(db)?,
                data_source: r.try_get("data_source").map_err(db)?,
                failing_version: r.try_get("failing_version").map_err(db)?,
                last_error: r.try_get("last_error").map_err(db)?,
            })
        })
        .collect()
}

fn db(e: sqlx::Error) -> AppError {
    AppError::Internal(format!("webhook query: {e}"))
}

// ---------------------------------------------------------------- dispatcher

/// One webhook row, as the dispatcher needs it.
#[derive(Debug, Clone)]
pub struct Webhook {
    pub webhook_id: Uuid,
    pub gtfs_id: String,
    pub name: String,
    pub event: String,
    pub url: String,
    pub method: String,
    pub headers: Value,
    pub body: Option<Value>,
    pub stale_after_seconds: i64,
    pub settle_seconds: i64,
    pub give_up_after_seconds: i64,
    pub request_timeout_seconds: u64,
    pub max_attempts: i32,
}

/// The whole per-tick job: hand back stranded claims, decide what is now due,
/// and send what is owed. Every pod runs it; the database decides who acts.
///
/// `policy` must be the one [`LivePolicy::get`] returned **this tick**, not a
/// value the caller kept from boot: an admin turning webhooks on, or taking a
/// host out of the allow-list, has to reach the dispatcher within a poll
/// interval. The same value is carried down to [`send_once`], so the host a
/// request is actually sent to is checked against the list as it is now.
///
/// Never returns an error to the caller - a webhook problem must not disturb
/// the poll loop that keeps the feed data fresh. Everything is logged instead.
pub async fn dispatch_tick(
    pool: &PgPool,
    http: &reqwest::Client,
    pod: &PodIdentity,
    policy: &WebhookPolicy,
) {
    if !policy.is_active() {
        return;
    }
    if let Err(e) = reclaim_stranded(pool, pod).await {
        warn!("webhook: reclaiming stranded deliveries failed: {e}");
    }
    if let Err(e) = enqueue_due(pool).await {
        warn!("webhook: deciding what is due failed: {e}");
    }
    if let Err(e) = send_due(pool, http, pod, policy).await {
        warn!("webhook: sending due deliveries failed: {e}");
    }
}

/// A delivery claimed by a pod that never finished it goes back in the queue.
async fn reclaim_stranded(pool: &PgPool, pod: &PodIdentity) -> AppResult<()> {
    let n = sqlx::query(
        "UPDATE gtfs_webhook_delivery \
            SET status = 'pending', next_attempt_at = now(), \
                last_error = coalesce(last_error, '') || \
                  case when last_error is null then '' else ' | ' end || \
                  'the pod holding this delivery stopped responding; requeued' \
          WHERE status = 'in_flight' \
            AND claimed_at < now() - make_interval(secs => $1)",
    )
    .bind(CLAIM_STALE_SECONDS as f64)
    .execute(pool)
    .await
    .map_err(db)?
    .rows_affected();
    if n > 0 {
        info!(pod = %pod.pod_id, "webhook: requeued {n} stranded deliveries");
    }
    Ok(())
}

/// Load every enabled webhook together with its feed's current version.
///
/// `created_at` guards against a retro-fire: a webhook only ever fires for a
/// version that appeared after it was configured, so adding one to a feed that
/// has been quiet for a week does not immediately trigger a rebuild.
async fn load_armed(pool: &PgPool) -> AppResult<Vec<(Webhook, i64, DateTime<Utc>)>> {
    let rows = sqlx::query(
        "SELECT w.webhook_id, w.gtfs_id, w.name, w.event, w.url, w.method, w.headers, w.body, \
                w.stale_after_seconds, w.settle_seconds, w.give_up_after_seconds, \
                w.request_timeout_seconds, w.max_attempts, \
                f.version AS feed_version, f.updated_at AS version_at \
           FROM gtfs_webhook w JOIN gtfs_feed f ON f.gtfs_id = w.gtfs_id \
          WHERE w.enabled AND f.updated_at > w.created_at \
            AND w.event <> 'release_requested'",
    )
    .fetch_all(pool)
    .await
    .map_err(db)?;
    rows.iter()
        .map(|r| {
            Ok((
                Webhook {
                    webhook_id: r.try_get("webhook_id").map_err(db)?,
                    gtfs_id: r.try_get("gtfs_id").map_err(db)?,
                    name: r.try_get("name").map_err(db)?,
                    event: r.try_get("event").map_err(db)?,
                    url: r.try_get("url").map_err(db)?,
                    method: r.try_get("method").map_err(db)?,
                    headers: r.try_get("headers").map_err(db)?,
                    body: r.try_get("body").map_err(db)?,
                    stale_after_seconds: r.try_get::<i32, _>("stale_after_seconds").map_err(db)?
                        as i64,
                    settle_seconds: r.try_get::<i32, _>("settle_seconds").map_err(db)? as i64,
                    give_up_after_seconds: r
                        .try_get::<i32, _>("give_up_after_seconds")
                        .map_err(db)? as i64,
                    request_timeout_seconds: r
                        .try_get::<i32, _>("request_timeout_seconds")
                        .map_err(db)? as u64,
                    max_attempts: r.try_get("max_attempts").map_err(db)?,
                },
                r.try_get("feed_version").map_err(db)?,
                r.try_get("version_at").map_err(db)?,
            ))
        })
        .collect()
}

/// Decide, for each armed webhook, whether its event has happened, and record
/// the delivery if so. The insert is the race: every pod runs this, and the
/// unique index means exactly one of them creates the row.
async fn enqueue_due(pool: &PgPool) -> AppResult<()> {
    let now = Utc::now();
    // one read of the pod table per feed, however many webhooks it has
    let mut fleets: BTreeMap<String, Fleet> = BTreeMap::new();
    for (w, version, version_at) in load_armed(pool).await? {
        // the staleness window is per webhook, so the cached rows are the raw
        // pod states and the split is redone whenever the window differs
        let key = format!("{}@{}", w.gtfs_id, w.stale_after_seconds);
        let fleet = match fleets.get(&key) {
            Some(f) => f.clone(),
            None if w.event == EVENT_COMMITTED => Fleet {
                live: vec![],
                stale: vec![],
            },
            None => {
                let f = fleet_from(pods_of(pool, &w.gtfs_id).await?, now, w.stale_after_seconds);
                fleets.insert(key, f.clone());
                f
            }
        };

        match w.event.as_str() {
            // the commit itself is the event; there is no fleet to wait for
            EVENT_COMMITTED => enqueue_one(pool, &w, version, None, None).await?,

            EVENT_IN_SYNC => match in_sync_verdict(
                &fleet,
                version,
                version_at,
                now,
                w.settle_seconds,
                w.give_up_after_seconds,
            ) {
                Verdict::Fire => enqueue_one(pool, &w, version, Some(&fleet), None).await?,
                Verdict::GiveUp(why) => abandon(pool, &w, version, &why).await?,
                Verdict::Wait(why) => {
                    debug!(webhook = %w.name, feed = %w.gtfs_id, version, "webhook waiting: {why}")
                }
            },

            EVENT_RELOAD_FAILED => {
                let failing: Vec<_> = fleet
                    .live
                    .iter()
                    .filter(|p| p.failing_version.is_some())
                    .collect();
                // keyed on the version that failed, so one alert per bad
                // version rather than one per tick while it stays bad
                if let Some(first) = failing.first() {
                    let v = first.failing_version.unwrap_or(version);
                    let why = failing
                        .iter()
                        .take(5)
                        .map(|p| {
                            format!("{}: {}", p.pod_id, p.last_error.as_deref().unwrap_or("?"))
                        })
                        .collect::<Vec<_>>()
                        .join(" | ");
                    enqueue_one(pool, &w, v, Some(&fleet), Some(&why)).await?;
                }
            }

            other => warn!(webhook = %w.name, "webhook: unknown event {other}; ignoring"),
        }
    }
    Ok(())
}

/// Insert the delivery if no pod has already. Losing this race is the normal
/// case on every pod but one, and is not an error.
async fn enqueue_one(
    pool: &PgPool,
    w: &Webhook,
    version: i64,
    fleet: Option<&Fleet>,
    note: Option<&str>,
) -> AppResult<()> {
    let created: Option<Uuid> = sqlx::query(
        "INSERT INTO gtfs_webhook_delivery \
           (webhook_id, gtfs_id, event, feed_version, kind, status, next_attempt_at, \
            pod_count, pods, last_error) \
         VALUES ($1, $2, $3, $4, 'event', 'pending', now(), $5, $6::jsonb, $7) \
         ON CONFLICT (webhook_id, feed_version) WHERE kind = 'event' DO NOTHING \
         RETURNING delivery_id",
    )
    .bind(w.webhook_id)
    .bind(&w.gtfs_id)
    .bind(&w.event)
    .bind(version)
    .bind(fleet.map(|f| f.live.len() as i32))
    .bind(fleet.map(|f| f.pods_json().to_string()))
    .bind(note)
    .fetch_optional(pool)
    .await
    .map_err(db)?
    .map(|r| r.try_get("delivery_id"))
    .transpose()
    .map_err(db)?;
    if let Some(id) = created {
        info!(
            webhook = %w.name, feed = %w.gtfs_id, event = %w.event, version,
            delivery = %id, "webhook: queued a delivery"
        );
    }
    Ok(())
}

/// Record that a version will never be delivered, once. Same unique index, so
/// the abandonment is as exactly-once as a delivery would have been.
async fn abandon(pool: &PgPool, w: &Webhook, version: i64, why: &str) -> AppResult<()> {
    let created: Option<Uuid> = sqlx::query(
        "INSERT INTO gtfs_webhook_delivery \
           (webhook_id, gtfs_id, event, feed_version, kind, status, next_attempt_at, \
            last_error, completed_at) \
         VALUES ($1, $2, $3, $4, 'event', 'abandoned', NULL, $5, now()) \
         ON CONFLICT (webhook_id, feed_version) WHERE kind = 'event' DO NOTHING \
         RETURNING delivery_id",
    )
    .bind(w.webhook_id)
    .bind(&w.gtfs_id)
    .bind(&w.event)
    .bind(version)
    .bind(why)
    .fetch_optional(pool)
    .await
    .map_err(db)?
    .map(|r| r.try_get("delivery_id"))
    .transpose()
    .map_err(db)?;
    if created.is_some() {
        error!(
            webhook = %w.name, feed = %w.gtfs_id, version,
            "webhook: gave up waiting for the fleet ({why}); downstream caches will keep serving older data until this is retried"
        );
    }
    Ok(())
}

/// A delivery this pod has claimed and must now send.
struct Claim {
    delivery_id: Uuid,
    webhook_id: Uuid,
    gtfs_id: String,
    event: String,
    feed_version: i64,
    attempts: i32,
    pod_count: Option<i32>,
    target: Option<String>,
}

/// Claim one due delivery. `SKIP LOCKED` lets several pods drain a backlog in
/// parallel without two of them taking the same row.
async fn claim_one(pool: &PgPool, pod: &PodIdentity) -> AppResult<Option<Claim>> {
    let row = sqlx::query(
        "UPDATE gtfs_webhook_delivery d \
            SET status = 'in_flight', claimed_by = $1, claimed_at = now(), \
                attempts = d.attempts + 1 \
          WHERE d.delivery_id = ( \
                SELECT delivery_id FROM gtfs_webhook_delivery \
                 WHERE status = 'pending' AND next_attempt_at <= now() \
                 ORDER BY next_attempt_at \
                 FOR UPDATE SKIP LOCKED LIMIT 1) \
          RETURNING d.delivery_id, d.webhook_id, d.gtfs_id, d.event, d.feed_version, \
                    d.attempts, d.pod_count, d.target",
    )
    .bind(&pod.pod_id)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    row.map(|r| {
        Ok(Claim {
            delivery_id: r.try_get("delivery_id").map_err(db)?,
            webhook_id: r.try_get("webhook_id").map_err(db)?,
            gtfs_id: r.try_get("gtfs_id").map_err(db)?,
            event: r.try_get("event").map_err(db)?,
            feed_version: r.try_get("feed_version").map_err(db)?,
            attempts: r.try_get("attempts").map_err(db)?,
            pod_count: r.try_get("pod_count").map_err(db)?,
            target: r.try_get("target").map_err(db)?,
        })
    })
    .transpose()
}

async fn load_webhook(pool: &PgPool, id: Uuid) -> AppResult<Option<Webhook>> {
    let row = sqlx::query(
        "SELECT webhook_id, gtfs_id, name, event, url, method, headers, body, \
                stale_after_seconds, settle_seconds, give_up_after_seconds, \
                request_timeout_seconds, max_attempts \
           FROM gtfs_webhook WHERE webhook_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    row.map(|r| {
        Ok(Webhook {
            webhook_id: r.try_get("webhook_id").map_err(db)?,
            gtfs_id: r.try_get("gtfs_id").map_err(db)?,
            name: r.try_get("name").map_err(db)?,
            event: r.try_get("event").map_err(db)?,
            url: r.try_get("url").map_err(db)?,
            method: r.try_get("method").map_err(db)?,
            headers: r.try_get("headers").map_err(db)?,
            body: r.try_get("body").map_err(db)?,
            stale_after_seconds: r.try_get::<i32, _>("stale_after_seconds").map_err(db)? as i64,
            settle_seconds: r.try_get::<i32, _>("settle_seconds").map_err(db)? as i64,
            give_up_after_seconds: r.try_get::<i32, _>("give_up_after_seconds").map_err(db)? as i64,
            request_timeout_seconds: r.try_get::<i32, _>("request_timeout_seconds").map_err(db)?
                as u64,
            max_attempts: r.try_get("max_attempts").map_err(db)?,
        })
    })
    .transpose()
}

async fn send_due(
    pool: &PgPool,
    http: &reqwest::Client,
    pod: &PodIdentity,
    policy: &WebhookPolicy,
) -> AppResult<()> {
    for _ in 0..MAX_DELIVERIES_PER_TICK {
        let Some(claim) = claim_one(pool, pod).await? else {
            return Ok(());
        };
        let Some(w) = load_webhook(pool, claim.webhook_id).await? else {
            // the webhook was deleted between the queue and the claim
            finish(pool, &claim, None, Some("the webhook no longer exists"), 0).await?;
            continue;
        };
        let payload = event_payload(&claim, &w);
        match send_once(http, &w, policy, &payload).await {
            Ok(status) => {
                info!(
                    webhook = %w.name, feed = %claim.gtfs_id, version = claim.feed_version,
                    status, attempt = claim.attempts, "webhook: delivered"
                );
                finish(pool, &claim, Some(status), None, w.max_attempts).await?;
            }
            Err(e) => {
                warn!(
                    webhook = %w.name, feed = %claim.gtfs_id, version = claim.feed_version,
                    attempt = claim.attempts, "webhook: delivery failed: {}", e.message
                );
                finish(pool, &claim, e.status, Some(&e.message), w.max_attempts).await?;
            }
        }
    }
    Ok(())
}

/// What the receiver is told. Also the `${event:...}` namespace, so a Jenkins
/// job can take the feed and version as its own build parameters.
fn event_payload(claim: &Claim, w: &Webhook) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("event".into(), json!(claim.event));
    m.insert("gtfs_id".into(), json!(claim.gtfs_id));
    m.insert("feed_version".into(), json!(claim.feed_version));
    m.insert("delivery_id".into(), json!(claim.delivery_id));
    m.insert("webhook".into(), json!(w.name));
    m.insert("pod_count".into(), json!(claim.pod_count));
    m.insert("fired_at".into(), json!(Utc::now()));
    if let Some(t) = &claim.target {
        m.insert("target".into(), json!(t));
    }
    m
}

struct SendError {
    message: String,
    status: Option<i32>,
}

impl SendError {
    fn config(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
        }
    }
}

/// Build and send one request. The allow-list is rechecked here, not only when
/// the URL was saved, and the placeholders are resolved here, so a credential
/// exists only inside this function's request.
async fn send_once(
    http: &reqwest::Client,
    w: &Webhook,
    policy: &WebhookPolicy,
    payload: &Map<String, Value>,
) -> Result<i32, SendError> {
    let url = resolve(&w.url, payload).map_err(SendError::config)?;
    check_host(&url, &policy.allowed_hosts).map_err(SendError::config)?;

    let method = reqwest::Method::from_bytes(w.method.as_bytes())
        .map_err(|_| SendError::config(format!("{} is not an HTTP method", w.method)))?;
    let mut req = http
        .request(method, &url)
        .timeout(Duration::from_secs(w.request_timeout_seconds));

    if let Some(obj) = w.headers.as_object() {
        for (k, v) in obj {
            let Some(raw) = v.as_str() else {
                return Err(SendError::config(format!("header {k} must be a string")));
            };
            req = req.header(
                k.as_str(),
                resolve(raw, payload).map_err(SendError::config)?,
            );
        }
    }
    if w.method != "GET" {
        let body = match &w.body {
            Some(b) => resolve_json(b, payload).map_err(SendError::config)?,
            None => Value::Object(payload.clone()),
        };
        req = req.json(&body);
    }

    let res = req.send().await.map_err(|e| SendError {
        // the URL can carry a token in a query parameter, so report the error
        // without it: reqwest's Display includes the URL it was given
        message: format!("request failed: {}", scrub(&e.to_string(), &url)),
        status: None,
    })?;
    let status = res.status();
    let code = status.as_u16() as i32;
    if status.is_success() {
        return Ok(code);
    }
    let body = res
        .text()
        .await
        .unwrap_or_default()
        .chars()
        .take(500)
        .collect::<String>();
    Err(SendError {
        message: format!("the receiver answered {code}: {}", scrub(&body, &url)),
        status: Some(code),
    })
}

/// Remove any occurrence of the resolved URL from text that will be stored.
/// The URL may carry a secret in a query parameter, and an error message is
/// written to the delivery row, the log and the dashboard.
fn scrub(text: &str, url: &str) -> String {
    let mut out = text.replace(url, "<url>");
    if let Ok(parsed) = reqwest::Url::parse(url) {
        if let Some(q) = parsed.query() {
            out = out.replace(q, "<query>");
        }
    }
    out
}

/// Record the outcome: done, or due again after a backoff, or out of attempts.
async fn finish(
    pool: &PgPool,
    claim: &Claim,
    status: Option<i32>,
    error: Option<&str>,
    max_attempts: i32,
) -> AppResult<()> {
    let sql = match error {
        None => {
            "UPDATE gtfs_webhook_delivery SET status = 'succeeded', response_status = $2, \
                    last_error = NULL, next_attempt_at = NULL, completed_at = now() \
              WHERE delivery_id = $1"
        }
        Some(_) if claim.attempts >= max_attempts => {
            "UPDATE gtfs_webhook_delivery SET status = 'failed', response_status = $2, \
                    last_error = $3, next_attempt_at = NULL, completed_at = now() \
              WHERE delivery_id = $1"
        }
        Some(_) => {
            "UPDATE gtfs_webhook_delivery SET status = 'pending', response_status = $2, \
                    last_error = $3, next_attempt_at = now() + make_interval(secs => $4) \
              WHERE delivery_id = $1"
        }
    };
    let mut q = sqlx::query(sql).bind(claim.delivery_id).bind(status);
    if error.is_some() {
        q = q.bind(error);
        if claim.attempts < max_attempts {
            q = q.bind(backoff_seconds(claim.attempts) as f64);
        }
    }
    q.execute(pool).await.map_err(db)?;
    if error.is_some() && claim.attempts >= max_attempts {
        error!(
            feed = %claim.gtfs_id, version = claim.feed_version, delivery = %claim.delivery_id,
            "webhook: giving up after {} attempts", claim.attempts
        );
    }
    Ok(())
}

/// Send one delivery now, outside the event flow: the dashboard's Test button.
/// Recorded like any other delivery, but as `kind = 'test'`, so it neither
/// consumes nor satisfies the version's real delivery.
pub async fn enqueue_test(pool: &PgPool, webhook_id: Uuid, requested_by: &str) -> AppResult<Uuid> {
    let id: Uuid = sqlx::query(
        "INSERT INTO gtfs_webhook_delivery \
           (webhook_id, gtfs_id, event, feed_version, kind, status, next_attempt_at, requested_by) \
         SELECT w.webhook_id, w.gtfs_id, w.event, f.version, 'test', 'pending', now(), $2 \
           FROM gtfs_webhook w JOIN gtfs_feed f ON f.gtfs_id = w.gtfs_id \
          WHERE w.webhook_id = $1 \
         RETURNING delivery_id",
    )
    .bind(webhook_id)
    .bind(requested_by)
    .fetch_optional(pool)
    .await
    .map_err(db)?
    .ok_or_else(|| AppError::Internal(format!("no webhook {webhook_id}")))?
    .try_get("delivery_id")
    .map_err(db)?;
    Ok(id)
}

/// Queue the Release to Nandi request: one `kind = 'release'` delivery per
/// enabled `release_requested` webhook of the feed, for its committed version.
/// Runs in the caller's transaction, which holds the feed lock.
pub async fn enqueue_release(
    conn: &mut PgConnection,
    gtfs_id: &str,
    requested_by: &str,
    target: &str,
) -> AppResult<Vec<Uuid>> {
    let rows = sqlx::query(
        "INSERT INTO gtfs_webhook_delivery \
           (webhook_id, gtfs_id, event, feed_version, kind, status, next_attempt_at, requested_by, \
            target) \
         SELECT w.webhook_id, w.gtfs_id, w.event, f.version, 'release', 'pending', now(), $2, $3 \
           FROM gtfs_webhook w JOIN gtfs_feed f ON f.gtfs_id = w.gtfs_id \
          WHERE w.gtfs_id = $1 AND w.enabled AND w.event = 'release_requested' \
         RETURNING delivery_id",
    )
    .bind(gtfs_id)
    .bind(requested_by)
    .bind(target)
    .fetch_all(conn)
    .await
    .map_err(db)?;
    rows.iter()
        .map(|r| r.try_get("delivery_id").map_err(db))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    fn pod(id: &str, version: i64, loaded: i64, seen: i64) -> PodState {
        PodState {
            pod_id: id.to_string(),
            loaded_version: version,
            loaded_at: at(loaded),
            updated_at: at(seen),
            data_source: "db".to_string(),
            failing_version: None,
            last_error: None,
        }
    }

    // ------------------------------------------------------------ allow-list

    #[test]
    fn only_allowed_hosts_can_be_called() {
        let allow = vec![
            "jenkins.internal".to_string(),
            ".movingtech.net".to_string(),
        ];
        assert!(check_host("https://jenkins.internal/job/x", &allow).is_ok());
        assert!(check_host("https://a.b.movingtech.net/x", &allow).is_ok());
        // the bare suffix itself matches a `.suffix` entry
        assert!(check_host("https://movingtech.net/x", &allow).is_ok());
        for bad in [
            "https://evil.com/x",
            // a lookalike that merely ends in the same letters
            "https://notmovingtech.net/x",
            "ftp://jenkins.internal/x",
            "not a url",
        ] {
            assert!(check_host(bad, &allow).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn an_empty_allow_list_refuses_everything() {
        assert!(check_host("https://jenkins.internal/x", &[]).is_err());
        assert!(!WebhookPolicy {
            enabled: true,
            allowed_hosts: vec![],
        }
        .is_active());
    }

    // ------------------------------------------------------------ precedence

    fn seed() -> WebhookPolicy {
        WebhookPolicy {
            enabled: true,
            allowed_hosts: vec![".internal.svc.movingtech.net".to_string()],
        }
    }

    fn stored(enabled: bool, hosts: &[&str]) -> StoredPolicy {
        StoredPolicy {
            enabled,
            allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
            updated_at: at(0),
            updated_by: Some("admin@example.invalid".to_string()),
        }
    }

    #[test]
    fn with_no_row_the_deployments_own_values_are_in_force() {
        let e = resolve_policy(None, &seed());
        assert_eq!(e.policy, seed());
        assert_eq!(e.source, PolicySource::Config);
        assert_eq!(e.updated_at, None);
    }

    #[test]
    fn a_saved_row_wins_over_the_deployment_config() {
        let e = resolve_policy(Some(stored(true, &["jenkins.c2.example"])), &seed());
        assert_eq!(e.policy.allowed_hosts, vec!["jenkins.c2.example"]);
        assert_eq!(e.source, PolicySource::Database);
        assert_eq!(e.updated_by.as_deref(), Some("admin@example.invalid"));
    }

    #[test]
    fn a_row_can_take_away_what_the_config_allowed() {
        // the two halves the union would have got wrong: turning the feature
        // off, and emptying a list the deployment had filled
        assert!(
            !resolve_policy(Some(stored(false, &["jenkins.c2.example"])), &seed())
                .policy
                .enabled
        );
        let emptied = resolve_policy(Some(stored(true, &[])), &seed());
        assert!(emptied.policy.allowed_hosts.is_empty());
        assert!(
            !emptied.policy.is_active(),
            "on with an empty list stays legal and still fires nothing"
        );
        assert_eq!(emptied.source, PolicySource::Database);
    }

    // ------------------------------------------------------------ host input

    #[test]
    fn a_host_is_a_host_name_and_nothing_else() {
        assert_eq!(
            normalise_host("  Jenkins.Example.COM ").unwrap(),
            "jenkins.example.com"
        );
        assert_eq!(normalise_host(".example.com").unwrap(), ".example.com");
        assert_eq!(normalise_host("127.0.0.1").unwrap(), "127.0.0.1");
        assert_eq!(
            normalise_host("jenkins.c2.sso.internal.svc.movingtech.net").unwrap(),
            "jenkins.c2.sso.internal.svc.movingtech.net"
        );
        for bad in [
            "https://jenkins.example.com",
            "jenkins.example.com/job/x",
            "jenkins.example.com:8080",
            "user@jenkins.example.com",
            "jenkins example.com",
            "*.example.com",
            "jenkins..example.com",
            "-jenkins.example.com",
            "",
            "   ",
            ".",
        ] {
            assert!(normalise_host(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_host_list_is_de_duplicated_and_bounded() {
        assert_eq!(
            normalise_hosts(&[
                "Jenkins.Example.com".into(),
                "jenkins.example.com".into(),
                ".example.com".into(),
            ])
            .unwrap(),
            vec!["jenkins.example.com", ".example.com"]
        );
        // an empty list is a legal policy, not a mistake
        assert_eq!(normalise_hosts(&[]).unwrap(), Vec::<String>::new());
        assert!(normalise_hosts(&["ok.example.com".into(), " ".into()]).is_err());
        let too_many: Vec<String> = (0..=MAX_ALLOWED_HOSTS)
            .map(|i| format!("h{i}.example"))
            .collect();
        assert!(normalise_hosts(&too_many).is_err());
    }

    #[test]
    fn a_normalised_entry_is_one_check_host_can_match() {
        let hosts =
            normalise_hosts(&["  .Example.COM ".into(), "JENKINS.internal".into()]).unwrap();
        assert!(check_host("https://a.b.example.com/x", &hosts).is_ok());
        assert!(check_host("https://jenkins.internal/x", &hosts).is_ok());
        assert!(check_host("https://elsewhere.invalid/x", &hosts).is_err());
    }

    // ------------------------------------------------------------ placeholders

    #[test]
    fn placeholders_read_the_environment_and_the_event() {
        std::env::set_var("GIMS_TEST_WEBHOOK_TOKEN", "s3cret");
        let mut ev = Map::new();
        ev.insert("gtfs_id".into(), json!("chennai_bus"));
        ev.insert("feed_version".into(), json!(18));

        assert_eq!(
            resolve("https://j/build?token=${GIMS_TEST_WEBHOOK_TOKEN}", &ev).unwrap(),
            "https://j/build?token=s3cret"
        );
        assert_eq!(
            resolve("${env:GIMS_TEST_WEBHOOK_TOKEN}", &ev).unwrap(),
            "s3cret"
        );
        assert_eq!(
            resolve("feed=${event:gtfs_id}&v=${event:feed_version}", &ev).unwrap(),
            "feed=chennai_bus&v=18"
        );
        assert_eq!(resolve("no placeholders", &ev).unwrap(), "no placeholders");
        std::env::remove_var("GIMS_TEST_WEBHOOK_TOKEN");
    }

    #[test]
    fn an_unresolved_placeholder_fails_rather_than_sending_the_literal() {
        std::env::remove_var("GIMS_TEST_ABSENT");
        let ev = Map::new();
        let e = resolve("${GIMS_TEST_ABSENT}", &ev).unwrap_err();
        assert!(e.contains("GIMS_TEST_ABSENT"), "{e}");
        assert!(resolve("${event:nope}", &ev).is_err());
        assert!(resolve("${unterminated", &ev).is_err());
        assert!(resolve("${weird:x}", &ev).is_err());
    }

    #[test]
    fn a_json_body_resolves_in_every_string_leaf() {
        std::env::set_var("GIMS_TEST_T", "tok");
        let mut ev = Map::new();
        ev.insert("feed_version".into(), json!(7));
        let body = json!({
            "token": "${GIMS_TEST_T}",
            "params": {"VERSION": "${event:feed_version}", "keep": 5},
            "list": ["${GIMS_TEST_T}"],
        });
        assert_eq!(
            resolve_json(&body, &ev).unwrap(),
            json!({
                "token": "tok",
                "params": {"VERSION": "7", "keep": 5},
                "list": ["tok"],
            })
        );
        std::env::remove_var("GIMS_TEST_T");
    }

    #[test]
    fn an_error_message_never_carries_the_query_string() {
        let url = "https://jenkins.internal/job/x/build?token=s3cret";
        let text = format!("error sending request for url ({url})");
        let scrubbed = scrub(&text, url);
        assert!(!scrubbed.contains("s3cret"), "{scrubbed}");
    }

    // ------------------------------------------------------------ fleet

    #[test]
    fn a_silent_pod_drops_out_of_the_fleet() {
        let pods = vec![pod("a", 5, 0, 100), pod("b", 4, 0, 10)];
        // b last spoke 90s ago; with a 60s window it is gone, not a laggard
        let fleet = fleet_from(pods, at(100), 60);
        assert_eq!(fleet.live.len(), 1);
        assert_eq!(fleet.stale.len(), 1);
        assert_eq!(fleet.min_version(), Some(5));
    }

    #[test]
    fn a_pod_on_preprocessed_data_is_neither_live_nor_stale() {
        let mut p = pod("a", 5, 0, 100);
        p.data_source = "preprocessed".to_string();
        let fleet = fleet_from(vec![p, pod("b", 5, 0, 100)], at(100), 60);
        assert_eq!(fleet.live.len(), 1);
        assert!(fleet.stale.is_empty());
    }

    // ------------------------------------------------------------ verdict

    #[test]
    fn it_fires_only_once_every_pod_has_settled_on_the_version() {
        let fleet = fleet_from(
            vec![pod("a", 9, 100, 200), pod("b", 9, 150, 200)],
            at(200),
            60,
        );
        // the last pod arrived at t=150; a 30s settle window is not up at 170
        assert!(matches!(
            in_sync_verdict(&fleet, 9, at(90), at(170), 30, 1800),
            Verdict::Wait(_)
        ));
        assert_eq!(
            in_sync_verdict(&fleet, 9, at(90), at(200), 30, 1800),
            Verdict::Fire
        );
        // a zero settle window fires as soon as the last pod arrives
        assert_eq!(
            in_sync_verdict(&fleet, 9, at(90), at(150), 0, 1800),
            Verdict::Fire
        );
    }

    #[test]
    fn one_laggard_holds_the_whole_fleet_back() {
        let fleet = fleet_from(
            vec![pod("a", 9, 100, 200), pod("b", 8, 10, 200)],
            at(200),
            60,
        );
        match in_sync_verdict(&fleet, 9, at(90), at(200), 0, 1800) {
            Verdict::Wait(why) => {
                assert!(why.contains("b at v8"), "{why}");
                assert!(why.contains("1 of 2"), "{why}");
            }
            other => panic!("expected a wait, got {other:?}"),
        }
    }

    #[test]
    fn a_laggard_that_never_catches_up_is_given_up_on_not_fired_anyway() {
        let fleet = fleet_from(
            vec![pod("a", 9, 100, 5000), pod("b", 8, 10, 5000)],
            at(5000),
            60,
        );
        // the version appeared at t=90 and it is now t=5000: past a 1800s window
        match in_sync_verdict(&fleet, 9, at(90), at(5000), 0, 1800) {
            Verdict::GiveUp(why) => assert!(why.contains("b at v8"), "{why}"),
            other => panic!("expected a give-up, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_fleet_never_fires() {
        let fleet = fleet_from(vec![], at(200), 60);
        assert!(matches!(
            in_sync_verdict(&fleet, 9, at(190), at(200), 0, 1800),
            Verdict::Wait(_)
        ));
        // and eventually says so loudly rather than staying quiet forever
        assert!(matches!(
            in_sync_verdict(&fleet, 9, at(0), at(5000), 0, 1800),
            Verdict::GiveUp(_)
        ));
    }

    #[test]
    fn a_fleet_of_only_stale_pods_is_reported_as_such() {
        let fleet = fleet_from(vec![pod("a", 9, 0, 0)], at(5000), 60);
        match in_sync_verdict(&fleet, 9, at(0), at(5000), 0, 1800) {
            Verdict::GiveUp(why) => assert!(why.contains("1 last reported"), "{why}"),
            other => panic!("expected a give-up, got {other:?}"),
        }
    }

    // ------------------------------------------------------------ backoff

    #[test]
    fn the_backoff_grows_and_then_stops_growing() {
        assert_eq!(backoff_seconds(1), 30);
        assert_eq!(backoff_seconds(2), 60);
        assert_eq!(backoff_seconds(3), 120);
        assert_eq!(backoff_seconds(6), 900);
        assert_eq!(backoff_seconds(20), 900);
        // a nonsensical attempt count must not panic or overflow the shift
        assert_eq!(backoff_seconds(0), 30);
        assert_eq!(backoff_seconds(-5), 30);
    }
}
