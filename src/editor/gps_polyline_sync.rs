//! Weekly drafts of routes' map lines from GPS (docs/gtfs-editor.md section 19).
//!
//! Once a week every route's line is rebuilt from the last days of bus pings and
//! compared with the live one; the routes whose line changed, or had none, go
//! into drafts of `draft_size` routes for a person to check and submit. The
//! pings are read once for the whole feed ([`gps_line::batch`]), by the route id
//! each trip was assigned, a group of routes at a time. The Kubernetes CronJob
//! runs it as a process of its own ([`run_job`]), outside the GIMS pods.

use super::auth::{self, Ctx};
use super::error::{EditorError, EditorResult};
use super::gps_line::{self, batch, GpsLine, RouteQuery, Stop};
use super::service::{self, NewChange};
use super::EditorState;
use crate::environment::{AppConfig, GpsPolylineSyncConfig};
use crate::services::osrm::{self, seg_dist, Planar};
use chrono::{DateTime, Datelike, FixedOffset, Utc};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
use uuid::Uuid;

const HAND_DRAWN: [&str; 2] = ["manual", "upload"];
const OSRM_BUDGET: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub min_runs: f64,
    pub max_missed_stops: usize,
    pub min_matched_share: f64,
    pub step_m: f64,
    pub off_m: f64,
    pub longest_off_m: f64,
    pub p95_m: f64,
    pub length_ratio: f64,
    pub coverage_m: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_runs: 3.0,
            max_missed_stops: 5,
            min_matched_share: 0.9,
            step_m: 10.0,
            off_m: 50.0,
            longest_off_m: 300.0,
            p95_m: 40.0,
            length_ratio: 0.05,
            coverage_m: 30.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncSettings {
    pub author_email: String,
    pub feeds: Vec<String>,
    /// Routes per draft.
    pub draft_size: usize,
    /// Bus-days read per route.
    pub bus_days_per_route: usize,
    /// Routes are read and built this many groups at a time.
    pub groups: usize,
    /// Routes built (and snapped by OSRM) at once.
    pub osrm_parallel: usize,
    /// Only these route ids; all when `None`.
    pub route_ids: Option<Vec<String>>,
    pub run_budget: Duration,
    pub thresholds: Thresholds,
}

impl SyncSettings {
    pub fn from_config(c: &GpsPolylineSyncConfig) -> Result<Self, String> {
        if c.author_email.trim().is_empty() {
            return Err("author_email is empty".into());
        }
        Ok(Self {
            author_email: c.author_email.trim().to_string(),
            feeds: c
                .feeds
                .clone()
                .unwrap_or_else(|| vec!["chennai_bus".to_string()]),
            draft_size: c.draft_size.unwrap_or(100).max(1) as usize,
            bus_days_per_route: c.bus_days_per_route.unwrap_or(16).max(1) as usize,
            groups: c.groups.unwrap_or(8).max(1) as usize,
            osrm_parallel: c.osrm_parallel.unwrap_or(4).max(1) as usize,
            route_ids: c.route_ids.clone().filter(|n| !n.is_empty()),
            run_budget: Duration::from_secs(c.run_budget_minutes.unwrap_or(360).max(1) as u64 * 60),
            thresholds: {
                let d = Thresholds::default();
                Thresholds {
                    min_runs: c.min_runs.map_or(d.min_runs, |n| n.max(1) as f64),
                    max_missed_stops: c
                        .max_missed_stops
                        .map_or(d.max_missed_stops, |n| n as usize),
                    ..d
                }
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Scheduled,
    Manual,
}

#[derive(Debug)]
pub enum Outcome {
    /// Another pod holds the run.
    Busy,
    /// This week's scheduled draft exists already.
    AlreadyDone,
    NoChange {
        checked: usize,
        partial: bool,
    },
    Drafted {
        change_set_ids: Vec<Uuid>,
        routes: usize,
        checked: usize,
        partial: bool,
    },
}

/// A route whose line goes into a draft: its encoded line and why
/// (`changed` or `missing`).
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub route_id: String,
    pub line: String,
    pub kind: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Propose(&'static str),
    Skip(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Diff {
    pub p95_m: f64,
    pub longest_off_m: f64,
    pub length_ratio: f64,
}

#[derive(sqlx::FromRow)]
struct RouteRow {
    route_id: String,
    short_name: Option<String>,
    encoded_polyline: Option<String>,
    polyline_source: Option<String>,
}

impl RouteRow {
    fn hand_drawn(&self) -> bool {
        self.polyline_source
            .as_deref()
            .is_some_and(|s| HAND_DRAWN.contains(&s))
    }
}

/// A week's proposals as drafts: changed lines first, then routes without
/// one, each kind in route id order (numbers by value), `size` routes a draft.
pub fn batches(mut proposals: Vec<Proposal>, size: usize) -> Vec<Vec<Proposal>> {
    let order = |id: &str| (id.parse::<u64>().map_or(u64::MAX, |n| n), id.to_string());
    proposals.sort_by(|a, b| {
        (a.kind != "changed", order(&a.route_id)).cmp(&(b.kind != "changed", order(&b.route_id)))
    });
    let mut out: Vec<Vec<Proposal>> = vec![];
    let mut rest = proposals.into_iter().peekable();
    while let Some(first) = rest.next() {
        let mut draft = vec![first];
        while draft.len() < size.max(1) {
            match rest.peek() {
                Some(next) if next.kind == draft[0].kind => {
                    draft.push(rest.next().expect("peeked"))
                }
                _ => break,
            }
        }
        out.push(draft);
    }
    out
}

/// Draft `k` of `n` of a run titled `base`; one draft keeps the plain title.
fn batch_title(base: &str, k: usize, n: usize) -> String {
    if n == 1 {
        base.to_string()
    } else {
        format!("{base} ({k}/{n})")
    }
}

/// Whether `title` is one of the drafts of the run titled `base`.
fn is_batch_of(title: &str, base: &str) -> bool {
    title == base
        || title
            .strip_prefix(base)
            .and_then(|r| r.strip_prefix(" ("))
            .and_then(|r| r.strip_suffix(')'))
            .and_then(|r| r.split_once('/'))
            .is_some_and(|(k, n)| k.parse::<usize>().is_ok() && n.parse::<usize>().is_ok())
}

fn ist() -> FixedOffset {
    FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("IST offset")
}

fn title_for(now: DateTime<Utc>, trigger: Trigger) -> String {
    let w = now.with_timezone(&ist()).date_naive().iso_week();
    let (year, week) = (w.year(), w.week());
    match trigger {
        Trigger::Scheduled => format!("GPS polylines {year}-W{week:02}"),
        Trigger::Manual => format!(
            "GPS polylines {year}-W{week:02} (manual {})",
            now.with_timezone(&ist()).format("%a %H:%M")
        ),
    }
}

/// Each point of `points` to the nearest point of `line`, both planar.
fn distances(points: &[(f64, f64)], line: &[(f64, f64)]) -> Vec<f64> {
    points
        .iter()
        .map(|&p| {
            line.windows(2)
                .map(|w| seg_dist(p, w[0], w[1]).0)
                .fold(f64::INFINITY, f64::min)
        })
        .collect()
}

fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * q).round() as usize]
}

fn longest_run_over(values: &[f64], limit: f64) -> usize {
    let (mut best, mut run) = (0, 0);
    for &d in values {
        run = if d > limit { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

/// The part of `line` between where the route's first and last served stops
/// fall on it: a depot or terminus approach at either end is not the route.
/// The whole line when the stops do not fall on it in order (a loop).
fn between_end_stops(line: &[(f64, f64)], stops: &[Stop]) -> Vec<(f64, f64)> {
    let (Some(first), Some(last)) = (stops.first(), stops.last()) else {
        return line.to_vec();
    };
    if line.len() < 2 || stops.len() < 2 {
        return line.to_vec();
    }
    let pl = Planar::around(line[0].0, line[0].1);
    let xy: Vec<(f64, f64)> = line.iter().map(|&p| pl.xy(p)).collect();
    let at = |s: &Stop| -> (usize, f64) {
        let p = pl.xy((s.lat, s.lon));
        xy.windows(2)
            .enumerate()
            .map(|(i, w)| {
                let (d, t) = seg_dist(p, w[0], w[1]);
                (d, i, t)
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, i, t)| (i, t))
            .unwrap_or((0, 0.0))
    };
    let (a, b) = (at(first), at(last));
    if (b.0, b.1) <= (a.0, a.1) {
        return line.to_vec();
    }
    let point = |(i, t): (usize, f64)| {
        let (p, q) = (line[i], line[i + 1]);
        (p.0 + t * (q.0 - p.0), p.1 + t * (q.1 - p.1))
    };
    let mut out = vec![point(a)];
    out.extend_from_slice(&line[a.0 + 1..=b.0]);
    out.push(point(b));
    out
}

/// How far apart two lines of (lat, lon) are, both ways.
pub fn compare(old: &[(f64, f64)], new: &[(f64, f64)], t: &Thresholds) -> Diff {
    if old.len() < 2 || new.len() < 2 {
        return Diff::default();
    }
    let pl = Planar::around(new[0].0, new[0].1);
    let planar = |line: &[(f64, f64)]| -> Vec<(f64, f64)> {
        osrm::resample(line, t.step_m)
            .into_iter()
            .map(|p| pl.xy(p))
            .collect()
    };
    let (o, n) = (planar(old), planar(new));
    let (fwd, back) = (distances(&n, &o), distances(&o, &n));
    let old_len = osrm::length_m(old);
    Diff {
        p95_m: percentile(&fwd, 0.95).max(percentile(&back, 0.95)),
        longest_off_m: longest_run_over(&fwd, t.off_m).max(longest_run_over(&back, t.off_m)) as f64
            * t.step_m,
        length_ratio: if old_len > 0.0 {
            (osrm::length_m(new) - old_len).abs() / old_len
        } else {
            f64::INFINITY
        },
    }
}

fn stop_coverage(line: &[(f64, f64)], stops: &[Stop], t: &Thresholds) -> f64 {
    let Some(first) = stops.first() else {
        return 0.0;
    };
    let pl = Planar::around(first.lat, first.lon);
    let line_xy: Vec<(f64, f64)> = line.iter().map(|&p| pl.xy(p)).collect();
    let stops_xy: Vec<(f64, f64)> = stops.iter().map(|s| pl.xy((s.lat, s.lon))).collect();
    gps_line::stop_coverage(&line_xy, &stops_xy, t.coverage_m)
}

/// Served stops more than `coverage_m` from the line.
fn missed_stops(line: &[(f64, f64)], stops: &[Stop], t: &Thresholds) -> usize {
    let n = stops.len();
    n.saturating_sub((stop_coverage(line, stops, t) * n as f64).round() as usize)
}

/// Whether a GPS answer is good enough to propose at all.
pub fn gate(ev: &Value, t: &Thresholds) -> Result<(), String> {
    let num = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let checks = [
        ("runs_used", num("runs_used") >= t.min_runs),
        (
            "matched",
            matches!(ev["matched"].as_str(), Some("osrm" | "partial"))
                && num("matched_share") >= t.min_matched_share,
        ),
        ("truncated", ev["truncated"] != json!(true)),
        ("stopped", ev["stopped"] != json!("budget")),
    ];
    match checks.iter().find(|(_, ok)| !ok) {
        Some((what, _)) => {
            let seen = match *what {
                "matched" => format!("{} {}", ev["matched"], ev["matched_share"]),
                other => ev[other].to_string(),
            };
            Err(format!("low_evidence: {what} {seen}"))
        }
        None => Ok(()),
    }
}

/// Propose the answer's line for a route whose live line is `current`, or not.
pub fn decide(
    current: Option<&str>,
    answer: &Value,
    stops: &[Stop],
    t: &Thresholds,
) -> (Decision, Option<Diff>) {
    let Some(new) = answer["encoded_polyline"]
        .as_str()
        .and_then(osrm::decode_polyline)
        .filter(|l| l.len() >= 2)
    else {
        return (Decision::Skip("no_line".into()), None);
    };
    let old = current
        .filter(|c| !c.trim().is_empty())
        .and_then(osrm::decode_polyline)
        .filter(|l| l.len() >= 2);
    // measured even when not proposed, so a skip's log says how far apart the lines are;
    // only between the end stops
    let measured = old.as_ref().map(|o| {
        compare(
            &between_end_stops(o, stops),
            &between_end_stops(&new, stops),
            t,
        )
    });
    if let Err(why) = gate(&answer["evidence"], t) {
        return (Decision::Skip(why), measured);
    }
    let (Some(old), Some(diff)) = (old, measured) else {
        return (Decision::Propose("missing"), None);
    };
    let changed = diff.longest_off_m >= t.longest_off_m
        || diff.p95_m >= t.p95_m
        || diff.length_ratio >= t.length_ratio;
    if !changed {
        return (Decision::Skip("unchanged".into()), Some(diff));
    }
    // either is enough: few stops off the new line, or no more than off the live one
    let (new_missed, old_missed) = (missed_stops(&new, stops, t), missed_stops(&old, stops, t));
    if new_missed > t.max_missed_stops && new_missed > old_missed {
        return (
            Decision::Skip(format!(
                "misses_stops: {new_missed} of {} (live {old_missed})",
                stops.len()
            )),
            Some(diff),
        );
    }
    (Decision::Propose("changed"), Some(diff))
}

struct Checked {
    decision: Decision,
    diff: Option<Diff>,
    line: Option<String>,
    evidence: Value,
}

fn skipped_with(why: impl Into<String>, evidence: Value) -> Checked {
    Checked {
        decision: Decision::Skip(why.into()),
        diff: None,
        line: None,
        evidence,
    }
}

/// One route from its own pings: steps 3 and 4 off the async thread, then
/// OSRM, then [`decide`].
#[allow(clippy::too_many_arguments)]
async fn check_route(
    state: &EditorState,
    gps: &GpsLine,
    s: &SyncSettings,
    gtfs_id: &str,
    index: &batch::WeekIndex,
    read: &batch::GroupRead,
    key: &str,
    r: &RouteRow,
    stops: Vec<Stop>,
) -> EditorResult<Checked> {
    if stops.len() < 2 {
        return Ok(skipped_with(
            format!("not_enough_stops ({})", stops.len()),
            Value::Null,
        ));
    }
    // the route's pings move to the blocking thread as a pointer, not a copy
    let tracks = read.tracks.get(key).cloned().unwrap_or_default();
    let base = batch::evidence_for(index, read, key, stops.len());
    let (params, at) = (gps.settings.params.clone(), stops.clone());
    let built = tokio::task::spawn_blocking(move || batch::build(&tracks, &at, &params, base))
        .await
        .map_err(|e| EditorError::internal(format!("building the line: {e}")))?;
    let (path, evidence) = match built {
        Ok(found) => found,
        Err(evidence) => return Ok(skipped_with("not_enough_runs", evidence)),
    };
    let short_name = r.short_name.as_deref().unwrap_or("");
    let q = RouteQuery {
        gtfs_id,
        route_id: &r.route_id,
        short_name,
        stops: &stops,
    };
    let answer = gps
        .finish(state.osrm_url.as_deref(), &q, &path, evidence, OSRM_BUDGET)
        .await;
    // the comparison is points x segments: off the async thread, like the build
    let (current, thresholds) = (r.encoded_polyline.clone(), s.thresholds);
    let (decision, diff, answer) = tokio::task::spawn_blocking(move || {
        let (decision, diff) = decide(current.as_deref(), &answer, &stops, &thresholds);
        (decision, diff, answer)
    })
    .await
    .map_err(|e| EditorError::internal(format!("comparing the line: {e}")))?;
    Ok(Checked {
        decision,
        diff,
        line: answer["encoded_polyline"].as_str().map(str::to_string),
        evidence: answer["evidence"].clone(),
    })
}

/// One run for one feed. Only one pod at a time gets past the lock.
pub async fn run_once(
    state: &EditorState,
    s: &SyncSettings,
    gtfs_id: &str,
    trigger: Trigger,
) -> EditorResult<Outcome> {
    let gps = state
        .gps_line
        .clone()
        .filter(|g| g.serves(gtfs_id))
        .ok_or_else(|| {
            EditorError::bad_request("gps_unavailable", format!("no GPS for {gtfs_id}"))
        })?;
    let author = auth::user_for_email(state, &s.author_email)
        .await?
        .filter(|u| u.status == "active")
        .ok_or_else(|| {
            EditorError::internal(format!(
                "editor account {} is missing or not active",
                s.author_email
            ))
        })?;
    let ctx = Ctx { user: author };
    let key = format!("gps-polyline-sync:{gtfs_id}");
    let mut lock = state.pool.acquire().await?;
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
        .bind(&key)
        .fetch_one(&mut *lock)
        .await?;
    if !got {
        return Ok(Outcome::Busy);
    }
    let outcome = run_locked(state, s, &gps, &ctx, gtfs_id, trigger).await;
    if let Err(e) = sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
        .bind(&key)
        .execute(&mut *lock)
        .await
    {
        warn!("gps polyline sync: unlock failed for {gtfs_id}: {e}");
    }
    outcome
}

async fn run_locked(
    state: &EditorState,
    s: &SyncSettings,
    gps: &GpsLine,
    ctx: &Ctx,
    gtfs_id: &str,
    trigger: Trigger,
) -> EditorResult<Outcome> {
    let now = Utc::now();
    let title = title_for(now, trigger);
    let author = ctx.user.user_id;
    if trigger == Trigger::Scheduled {
        let titles: Vec<String> = sqlx::query_scalar(
            "SELECT title FROM gtfs_change_set \
             WHERE gtfs_id = $1 AND created_by = $2 AND starts_with(title, $3)",
        )
        .bind(gtfs_id)
        .bind(author)
        .bind(&title)
        .fetch_all(&state.pool)
        .await?;
        if titles.iter().any(|t| is_batch_of(t, &title)) {
            return Ok(Outcome::AlreadyDone);
        }
    }

    let routes: Vec<RouteRow> = sqlx::query_as(
        "SELECT route_id, short_name, encoded_polyline, polyline_source FROM gtfs_route \
         WHERE gtfs_id = $1 AND NOT deleted \
         AND ($2::text[] IS NULL OR route_id = ANY($2)) ORDER BY route_id",
    )
    .bind(gtfs_id)
    .bind(s.route_ids.as_deref())
    .fetch_all(&state.pool)
    .await?;
    // a route in this account's submitted or approved set waits for its review; one
    // in an earlier draft is checked again (and moved here if proposed again)
    let held: HashSet<String> = sqlx::query_scalar(
        "SELECT c.entity_key FROM gtfs_change c \
         JOIN gtfs_change_set s ON s.change_set_id = c.change_set_id \
         WHERE s.gtfs_id = $1 AND s.created_by = $2 \
         AND s.status IN ('submitted', 'approved') AND c.entity = 'route'",
    )
    .bind(gtfs_id)
    .bind(author)
    .fetch_all(&state.pool)
    .await?
    .into_iter()
    .collect();
    let todo: Vec<&RouteRow> = routes
        .iter()
        .filter(|r| !held.contains(&r.route_id) && !r.hand_drawn())
        .collect();
    info!(
        "gps polyline sync {gtfs_id}: checking {} of {} routes",
        todo.len(),
        routes.len()
    );

    let deadline = Instant::now() + s.run_budget;
    let read_at = now.timestamp();
    let index = gps.read_week_index(read_at).await;
    let chosen = batch::choose(
        &index,
        s.bus_days_per_route,
        u64::from(gps.settings.params.min_bus_day_pings),
    );
    info!(
        days_read = index.days_read,
        days_unread = index.days_unread,
        routes = index.by_route.len(),
        queries = index.queries,
        "gps polyline sync {gtfs_id}: week index"
    );
    // a route with no buses would look like a quiet week: stop, so it reads as a failure
    if !index.complete() {
        return Err(EditorError::internal(format!(
            "gps polyline sync {gtfs_id}: week index incomplete ({} of {} days unread)",
            index.days_unread, index.days
        )));
    }
    let mut proposals: Vec<Proposal> = vec![];
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    let (mut checked, mut partial) = (0, false);
    // each route by its own id: no bus ran it this week, nothing to read
    let mut by_route: BTreeMap<String, &RouteRow> = BTreeMap::new();
    for r in todo {
        match batch::route_key(&r.route_id) {
            Some(k) if chosen.contains_key(&k) => {
                by_route.insert(k, r);
            }
            _ => {
                checked += 1;
                *skipped.entry("no_buses".into()).or_default() += 1;
            }
        }
    }
    for g in 0..s.groups {
        let routes: Vec<String> = by_route
            .keys()
            .filter(|k| batch::group_of(k, s.groups) == g)
            .cloned()
            .collect();
        if routes.is_empty() {
            continue;
        }
        if Instant::now() >= deadline {
            partial = true;
            break;
        }
        let read = gps.read_group(&chosen, &routes, read_at).await;
        info!(
            group = g,
            routes = routes.len(),
            queries = read.queries,
            seconds = read.read_seconds,
            complete = read.complete,
            "gps polyline sync {gtfs_id}: group read"
        );
        let jobs: Vec<(&str, &RouteRow)> =
            routes.iter().map(|k| (k.as_str(), by_route[k])).collect();
        // the group's stop lists in one query, as the route page reads them
        let keys: Vec<(String, i16)> = jobs
            .iter()
            .map(|(_, r)| (r.route_id.clone(), service::FIRST_PATTERN))
            .collect();
        let mut rows = {
            let mut conn = state.pool.acquire().await?;
            service::load_patterns_read_rows(&mut conn, gtfs_id, &keys).await?
        };
        let (index, read) = (&index, &read);
        let results: Vec<(String, EditorResult<Checked>)> = futures::stream::iter(jobs)
            .map(|(n, r)| {
                let stops = gps_line::served_stops(&json!({
                    "rows": rows.remove(&(r.route_id.clone(), service::FIRST_PATTERN)).unwrap_or_default()
                }));
                async move {
                    let c = check_route(state, gps, s, gtfs_id, index, read, n, r, stops).await;
                    (r.route_id.clone(), c)
                }
            })
            .buffer_unordered(s.osrm_parallel)
            .collect()
            .await;
        for (route_id, result) in results {
            checked += 1;
            match result {
                Ok(c) => {
                    info!(
                        route_id = %route_id,
                        decision = ?c.decision,
                        diff = ?c.diff,
                        evidence = %c.evidence,
                        "gps polyline sync {gtfs_id}"
                    );
                    match (c.decision, c.line) {
                        (Decision::Propose(kind), Some(line)) => proposals.push(Proposal {
                            route_id,
                            line,
                            kind,
                        }),
                        (Decision::Skip(why), _) => {
                            let reason = why.split(':').next().unwrap_or("").to_string();
                            *skipped.entry(reason).or_default() += 1;
                        }
                        (Decision::Propose(_), None) => {}
                    }
                }
                Err(e) => {
                    warn!(route_id = %route_id, "gps polyline sync {gtfs_id}: {e:?}");
                    *skipped.entry("error".into()).or_default() += 1;
                }
            }
        }
    }
    info!(
        "gps polyline sync {gtfs_id}: {checked} checked, {} to propose, skipped {skipped:?}{}",
        proposals.len(),
        if partial { " (run budget spent)" } else { "" }
    );
    if proposals.is_empty() {
        return Ok(Outcome::NoChange { checked, partial });
    }

    let (from, to) = (
        index.from.map(|d| d.to_string()).unwrap_or_default(),
        index.to.map(|d| d.to_string()).unwrap_or_default(),
    );
    let total = proposals.len();
    let drafts = batches(proposals, s.draft_size);
    let n = drafts.len();
    let (mut created, mut added): (Vec<Uuid>, Vec<String>) = (vec![], vec![]);
    for (k, draft) in drafts.into_iter().enumerate() {
        // one line: each route's comparison is on its own card
        let what = if draft[0].kind == "changed" {
            "whose line changed"
        } else {
            "without a line"
        };
        let description = format!(
            "From the buses' GPS, {from} to {to}: draft {} of {n}, {} routes {what}; {total} routes in all.",
            k + 1,
            draft.len(),
        );
        let title = batch_title(&title, k + 1, n);
        let set = service::create_set(state, ctx, gtfs_id, &title, Some(&description)).await?;
        let id: Uuid = set["change_set_id"]
            .as_str()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| EditorError::internal("create_set answered without an id"))?;
        let before = added.len();
        for p in draft {
            let change = NewChange {
                entity: "route".into(),
                op: "update".into(),
                entity_key: p.route_id.clone(),
                after: json!({"encoded_polyline": p.line, "polyline_source": "gps"}),
                base_row_version: None,
            };
            match service::add_change(state, ctx, id, change).await {
                Ok(_) => added.push(p.route_id),
                Err(e) => warn!(
                    route_id = %p.route_id,
                    "gps polyline sync {gtfs_id}: change not added: {e:?}"
                ),
            }
        }
        if added.len() == before {
            service::discard(state, ctx, id).await?;
        } else {
            created.push(id);
        }
    }
    if created.is_empty() {
        return Ok(Outcome::NoChange { checked, partial });
    }
    replace_in_earlier_drafts(state, ctx, gtfs_id, &created, &added).await?;
    Ok(Outcome::Drafted {
        change_set_ids: created,
        routes: added.len(),
        checked,
        partial,
    })
}

/// Takes the routes now in this run's drafts (`new`) out of this account's
/// earlier drafts, and discards an earlier draft left with nothing in it. Other
/// routes stay where they are.
async fn replace_in_earlier_drafts(
    state: &EditorState,
    ctx: &Ctx,
    gtfs_id: &str,
    new: &[Uuid],
    routes: &[String],
) -> EditorResult<()> {
    let old: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT c.change_set_id, c.change_id FROM gtfs_change c \
         JOIN gtfs_change_set s ON s.change_set_id = c.change_set_id \
         WHERE s.gtfs_id = $1 AND s.created_by = $2 AND s.status = 'draft' \
         AND NOT (s.change_set_id = ANY($3)) AND c.entity = 'route' AND c.entity_key = ANY($4)",
    )
    .bind(gtfs_id)
    .bind(ctx.user.user_id)
    .bind(new)
    .bind(routes)
    .fetch_all(&state.pool)
    .await?;
    let mut sets: Vec<Uuid> = vec![];
    for (set, change) in old {
        service::delete_change(state, ctx, set, change).await?;
        if !sets.contains(&set) {
            sets.push(set);
        }
    }
    for set in sets {
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM gtfs_change WHERE change_set_id = $1")
                .bind(set)
                .fetch_one(&state.pool)
                .await?;
        if left == 0 {
            service::discard(state, ctx, set).await?;
            info!("gps polyline sync {gtfs_id}: draft {set} emptied by this run, discarded");
        } else {
            info!("gps polyline sync {gtfs_id}: routes moved from draft {set} to this run's");
        }
    }
    Ok(())
}

/// The weekly run as a process of its own: `gtfs-routes-service
/// --gps-polyline-sync`, which the Kubernetes CronJob starts in a pod of its
/// own. No HTTP server and no GTFS feed are loaded, so the run uses only that
/// pod's memory. Every configured feed is run once; `Err` when the sync cannot
/// start or a feed's run failed (the Job then shows as failed). Drafted, no
/// change, already done this week and busy elsewhere are all fine.
pub async fn run_job(config: &AppConfig) -> Result<(), String> {
    let state = EditorState::init(config)
        .await
        .ok_or("the GTFS editor is not enabled or its config is incomplete")?;
    let settings = state
        .gps_polyline_sync
        .clone()
        .ok_or("gtfs_gps_polyline_sync is not configured, or gtfs_gps is missing")?;
    let mut failed = vec![];
    for g in &settings.feeds {
        match run_once(&state, &settings, g, Trigger::Scheduled).await {
            Ok(outcome) => info!("gps polyline sync {g}: {outcome:?}"),
            Err(e) => {
                error!("gps polyline sync {g}: {e:?}");
                failed.push(g.clone());
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("gps polyline sync failed for {failed:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(from: (f64, f64), to: (f64, f64), n: usize) -> Vec<(f64, f64)> {
        (0..=n)
            .map(|i| {
                let f = i as f64 / n as f64;
                (from.0 + (to.0 - from.0) * f, from.1 + (to.1 - from.1) * f)
            })
            .collect()
    }

    fn east(p: (f64, f64), m: f64) -> (f64, f64) {
        (p.0, p.1 + m / (111_320.0 * p.0.to_radians().cos()))
    }

    fn straight() -> Vec<(f64, f64)> {
        line((13.00, 80.20), (13.05, 80.20), 50)
    }

    fn good_answer(new: &[(f64, f64)], stop_coverage: f64) -> Value {
        json!({
            "encoded_polyline": osrm::encode_polyline(new),
            "evidence": {
                "runs_used": 20, "buses": 5, "stop_coverage": stop_coverage,
                "matched": "osrm", "matched_share": 0.99, "truncated": false, "stopped": "enough"
            }
        })
    }

    #[test]
    fn a_depot_approach_before_the_first_stop_is_not_a_change() {
        let route = straight();
        let mut old = line(east(route[0], -3000.0), route[0], 30);
        old.pop();
        old.extend(route.iter().copied());
        let (d, diff) = decide(
            Some(&osrm::encode_polyline(&old)),
            &good_answer(&route, 1.0),
            &stops_on(&route),
            &Thresholds::default(),
        );
        assert_eq!(d, Decision::Skip("unchanged".into()), "{diff:?}");
    }

    fn stops_on(l: &[(f64, f64)]) -> Vec<Stop> {
        l.iter()
            .step_by(10)
            .enumerate()
            .map(|(i, &(lat, lon))| Stop {
                stop_id: i.to_string(),
                lat,
                lon,
            })
            .collect()
    }

    #[test]
    fn identical_lines_do_not_differ() {
        let d = compare(&straight(), &straight(), &Thresholds::default());
        assert!(d.p95_m < 1.0 && d.longest_off_m == 0.0 && d.length_ratio < 1e-9);
    }

    #[test]
    fn a_small_shift_is_unchanged() {
        let old = straight();
        let new: Vec<_> = old.iter().map(|&p| east(p, 8.0)).collect();
        let t = Thresholds::default();
        let (d, _) = decide(
            Some(&osrm::encode_polyline(&old)),
            &good_answer(&new, 1.0),
            &stops_on(&old),
            &t,
        );
        assert_eq!(d, Decision::Skip("unchanged".into()));
    }

    #[test]
    fn a_diversion_is_a_change() {
        let old = straight();
        let new: Vec<_> = old
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                if (20..=30).contains(&i) {
                    east(p, 200.0)
                } else {
                    p
                }
            })
            .collect();
        let t = Thresholds::default();
        let diff = compare(&old, &new, &t);
        assert!(diff.longest_off_m >= t.longest_off_m, "{diff:?}");
        let (d, _) = decide(
            Some(&osrm::encode_polyline(&old)),
            &good_answer(&new, 1.0),
            &stops_on(&old),
            &t,
        );
        assert_eq!(d, Decision::Propose("changed"));
    }

    #[test]
    fn a_longer_line_is_a_change() {
        let old = straight();
        let new = line((13.00, 80.20), (13.0535, 80.20), 50);
        let diff = compare(&old, &new, &Thresholds::default());
        assert!(diff.length_ratio >= 0.05, "{diff:?}");
    }

    #[test]
    fn a_change_that_misses_stops_is_skipped() {
        let old = straight();
        let new: Vec<_> = old.iter().map(|&p| east(p, 300.0)).collect();
        let (d, _) = decide(
            Some(&osrm::encode_polyline(&old)),
            &good_answer(&new, 0.0),
            &stops_on(&old),
            &Thresholds::default(),
        );
        assert_eq!(d, Decision::Skip("misses_stops: 6 of 6 (live 0)".into()));
    }

    #[test]
    fn missing_no_more_stops_than_the_live_line_is_enough() {
        let route = straight();
        let old: Vec<_> = route.iter().map(|&p| east(p, 300.0)).collect();
        let new: Vec<_> = route.iter().map(|&p| east(p, 200.0)).collect();
        let (d, _) = decide(
            Some(&osrm::encode_polyline(&old)),
            &good_answer(&new, 0.0),
            &stops_on(&route),
            &Thresholds::default(),
        );
        assert_eq!(d, Decision::Propose("changed"));
    }

    #[test]
    fn a_missing_line_is_proposed_whatever_its_stops() {
        let (d, _) = decide(
            None,
            &good_answer(&straight(), 0.6),
            &[],
            &Thresholds::default(),
        );
        assert_eq!(d, Decision::Propose("missing"));
    }

    #[test]
    fn no_live_line_is_proposed_as_missing() {
        let (d, diff) = decide(
            None,
            &good_answer(&straight(), 1.0),
            &[],
            &Thresholds::default(),
        );
        assert_eq!(d, Decision::Propose("missing"));
        assert!(diff.is_none());
    }

    #[test]
    fn thin_evidence_is_never_proposed() {
        let mut a = good_answer(&straight(), 1.0);
        a["evidence"]["runs_used"] = json!(2);
        let (d, _) = decide(None, &a, &[], &Thresholds::default());
        assert_eq!(d, Decision::Skip("low_evidence: runs_used 2".into()));
    }

    #[test]
    fn dhall_block_parses_into_settings() {
        let c: GpsPolylineSyncConfig = serde_dhall::from_str(
            r#"{ author_email = "gps-polyline-sync@example.com", feeds = Some [ "chennai_bus" ],
                 groups = Some 4, run_budget_minutes = None Natural }"#,
        )
        .parse()
        .unwrap();
        let s = SyncSettings::from_config(&c).unwrap();
        assert_eq!(
            (
                s.groups,
                s.bus_days_per_route,
                s.osrm_parallel,
                s.draft_size
            ),
            (4, 16, 4, 100)
        );
    }

    #[test]
    fn proposals_go_into_drafts_by_kind_and_route_id() {
        let p = |id: &str, kind: &'static str| Proposal {
            route_id: id.into(),
            line: format!("line-{id}"),
            kind,
        };
        let all = vec![
            p("761", "missing"),
            p("1987", "changed"),
            p("5", "changed"),
            p("40", "missing"),
            p("212", "changed"),
            p("x9", "changed"),
        ];
        let ids = |d: &Vec<Proposal>| d.iter().map(|q| q.route_id.clone()).collect::<Vec<_>>();
        let drafts = batches(all.clone(), 2);
        // changed first, numbers by value, then the routes without a line
        assert_eq!(
            drafts.iter().map(ids).collect::<Vec<_>>(),
            vec![vec!["5", "212"], vec!["1987", "x9"], vec!["40", "761"]]
        );
        // a draft never mixes the two kinds
        let drafts = batches(all.clone(), 3);
        assert_eq!(
            drafts.iter().map(ids).collect::<Vec<_>>(),
            vec![vec!["5", "212", "1987"], vec!["x9"], vec!["40", "761"]]
        );
        assert_eq!(batches(all.clone(), 100).len(), 2);
        assert_eq!(batches(all, 0).len(), 6, "size 0 is 1");
        assert!(batches(vec![], 100).is_empty());
    }

    #[test]
    fn a_weeks_drafts_are_found_by_their_titles() {
        let base = "GPS polylines 2026-W41";
        assert_eq!(batch_title(base, 1, 1), base);
        assert_eq!(batch_title(base, 3, 22), "GPS polylines 2026-W41 (3/22)");
        assert!(is_batch_of(base, base));
        assert!(is_batch_of("GPS polylines 2026-W41 (3/22)", base));
        // a manual run the same week is not this week's scheduled run
        assert!(!is_batch_of(
            "GPS polylines 2026-W41 (manual Mon 14:05)",
            base
        ));
        assert!(!is_batch_of(
            "GPS polylines 2026-W41 (manual Mon 14:05) (1/2)",
            base
        ));
        assert!(!is_batch_of("GPS polylines 2026-W41 (run A)", base));
        assert!(!is_batch_of("GPS polylines 2026-W42", base));
    }

    #[test]
    fn dev_config_has_the_sync_off() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/dhall-configs/dev/gtfs_in_memory_server_rust.dhall"
        );
        let config = crate::environment::read_dhall_config(path).unwrap();
        assert!(config.gtfs_gps_polyline_sync.is_none());
    }
}
