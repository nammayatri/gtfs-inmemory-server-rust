//! Route reviews (docs/gtfs-editor.md section 14): the routes most worth an
//! operator's time, queued in `gtfs_route_review` by nandi's
//! `load_route_reviews.py`, worst-ranked first.
//!
//! # Why it is ordered by use, not by defect
//!
//! Coordinate reviews (section 8) start from a defect and ask whether a stop is
//! wrong. This queue starts from the other end. A route nobody rides can be
//! wrong for years and cost nothing; a defect on the busiest route is felt by
//! thousands of passengers a day. So the order is [`Review::queue_rank`] - 1 is
//! the route with the most bookings - and the `reasons` say what the load found
//! wrong with it, so an operator sees both why the route matters and what to
//! look at when they open it.
//!
//! [`measure`] names what was counted, because it will not always be the same
//! thing. A deployment with ticket sales says `bookings`; one with only
//! operations says `trips_operated`. Passing off the second as the first would
//! make the queue lie about the very thing it is sorted by.
//!
//! # Nothing here writes the feed
//!
//! A fix is made with the ordinary route editor - a `route` or `route_stops`
//! change in a draft, graded by the same validation as any other. [`fix`] only
//! records which draft the fix went into; it refuses a draft that holds no such
//! change, so the link cannot claim a fix that was never made. From there the
//! review follows its draft exactly as a coordinate review does: the change
//! leaves, and it is pending again; the draft is committed, and it is committed.
//!
//! # What looks wrong, twice
//!
//! `reasons` is the load-time snapshot, kept so the queue can be filtered and
//! counted without reading the whole feed. [`problems`] recomputes the same
//! judgements from the live rows when a review is opened, so an operator is
//! never sent after something that has since been fixed. The two use the same
//! codes ([`REASON_CODES`]) on purpose: a filter and a detail page that disagree
//! about what "short stop list" means would be worse than either alone.
//!
//! [`measure`]: Review::measure

use super::auth::{self, Ctx};
use super::context;
use super::error::{EditorError, EditorResult};
use super::feed_lock::{lock_feed_of_set, retry_transient};
use super::position_reviews::detour_m;
use super::proposals::{parse_status_list, Problem};
use super::service::{self, Page};
use super::validation::UNSERVED_TYPES;
use super::EditorState;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub const STATUSES: [&str; 6] = [
    "pending",
    "approved",
    "committed",
    "confirmed",
    "rejected",
    "superseded",
];

/// The codes a load may put in `reasons`, and the codes [`problems`] answers
/// with. The list filter and the summary count these, so a code the loader
/// invents is filterable but uncounted - which is the right way round: a new
/// signal should not be silently dropped from the queue.
pub const REASON_CODES: [&str; 6] = [
    "no_polyline",
    "too_few_stops",
    "short_stop_list",
    "repeated_stop",
    "stops_under_position_review",
    "worst_detour",
];

/// Fewer served stops than this and the route cannot be ridden from anywhere to
/// anywhere: a defect, not a short route. Measured on master (2026-09-19): 78 of
/// 5,567 routes have exactly one.
pub const MIN_SERVED_STOPS: usize = 2;
/// Short enough to be worth a look. Master's distribution is flat from two stops
/// upwards (224 / 129 / 221 / 183 routes at 2 / 3 / 4 / 5), so this is a place to
/// start looking, never evidence on its own - which is why it is a warning.
pub const SHORT_STOP_LIST: usize = 5;

const SELECT: &str = "SELECT r.review_id, r.gtfs_id, r.batch, r.route_id, r.route_short_name, \
        r.route_long_name, r.queue_rank, r.measure, r.measure_value, r.measure_window, \
        r.reasons::text AS reasons, r.evidence::text AS evidence, r.status, r.change_set_id, \
        r.change_id, cs.title AS change_set_title, u.email AS reviewed_by_email, r.reviewed_at, \
        r.review_note, r.created_at, r.updated_at \
     FROM gtfs_route_review r \
     LEFT JOIN gtfs_change_set cs ON cs.change_set_id = r.change_set_id \
     LEFT JOIN gtfs_editor_user u ON u.user_id = r.reviewed_by";

pub struct Review {
    pub review_id: i64,
    pub gtfs_id: String,
    pub route_id: String,
    pub queue_rank: i32,
    pub measure: String,
    pub status: String,
    pub change_set_id: Option<Uuid>,
    pub json: Value,
}

fn from_row(r: &PgRow) -> Result<Review, sqlx::Error> {
    type Ts = Option<chrono::DateTime<chrono::Utc>>;
    let jsonb = |col: &str| -> Result<Value, sqlx::Error> {
        Ok(r.try_get::<Option<String>, _>(col)?
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .unwrap_or_else(|| json!({})))
    };
    let review = Review {
        review_id: r.try_get("review_id")?,
        gtfs_id: r.try_get("gtfs_id")?,
        route_id: r.try_get("route_id")?,
        queue_rank: r.try_get("queue_rank")?,
        measure: r.try_get("measure")?,
        status: r.try_get("status")?,
        change_set_id: r.try_get("change_set_id")?,
        json: Value::Null,
    };
    let json = json!({
        "review_id": review.review_id,
        "gtfs_id": review.gtfs_id,
        "batch": r.try_get::<String, _>("batch")?,
        "route_id": review.route_id,
        "route_short_name": r.try_get::<Option<String>, _>("route_short_name")?,
        "route_long_name": r.try_get::<Option<String>, _>("route_long_name")?,
        "queue_rank": review.queue_rank,
        "measure": review.measure,
        "measure_value": r.try_get::<f64, _>("measure_value")?,
        "measure_window": r.try_get::<Option<String>, _>("measure_window")?,
        "reasons": jsonb("reasons")?,
        "evidence": jsonb("evidence")?,
        "status": review.status,
        "change_set_id": review.change_set_id,
        "change_id": r.try_get::<Option<i64>, _>("change_id")?,
        "change_set_title": r.try_get::<Option<String>, _>("change_set_title")?,
        "reviewed_by_email": r.try_get::<Option<String>, _>("reviewed_by_email")?,
        "reviewed_at": r.try_get::<Ts, _>("reviewed_at")?,
        "review_note": r.try_get::<Option<String>, _>("review_note")?,
        "created_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?,
        "updated_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?,
    });
    Ok(Review { json, ..review })
}

async fn load(conn: &mut PgConnection, id: i64, lock: bool) -> EditorResult<Review> {
    let sql = format!(
        "{SELECT} WHERE r.review_id = $1{}",
        if lock { " FOR UPDATE OF r" } else { "" }
    );
    let row = sqlx::query(&sql)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| {
            EditorError::not_found("review_not_found", format!("no route review {id}"))
        })?;
    Ok(from_row(&row)?)
}

// ---------------------------------------------------------------- statuses

/// What happens to a route review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// a reviewer ties the draft holding their fix to the review
    Fix,
    /// a reviewer walked the route and it is right as it stands
    Confirm,
    /// not worth fixing, or not a real problem
    Reject,
    /// a closed review is opened again
    Reopen,
    /// the review's change left its draft: removed, or the draft discarded
    Return,
    /// the draft carrying the review's change was committed
    Commit,
}

/// The status a review in `status` takes on `t`; `None` when `t` does not apply
/// to a review in that status. Unlike a coordinate review, a rejected review can
/// be reopened: "not worth fixing" is a judgement about priority, and priority
/// changes.
pub fn next_status(status: &str, t: Transition) -> Option<&'static str> {
    match (status, t) {
        ("pending" | "approved", Transition::Fix) => Some("approved"),
        ("pending", Transition::Confirm) => Some("confirmed"),
        ("pending", Transition::Reject) => Some("rejected"),
        ("confirmed" | "rejected", Transition::Reopen) => Some("pending"),
        ("approved", Transition::Return) => Some("pending"),
        ("approved", Transition::Commit) => Some("committed"),
        _ => None,
    }
}

/// Why a fix cannot be tied to a draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// the review is closed: committed, confirmed, rejected or superseded
    NotOpen,
    /// it is already tied to another draft
    InOtherDraft(Uuid),
}

/// Whether draft `into` may be tied to a review in `status` whose fix is in
/// `draft`. A review follows ONE draft: a route fixed in two drafts at once
/// would have no answer to "what happened to it", and the second draft would
/// silently take the first one's place.
pub fn may_tie(status: &str, draft: Option<Uuid>, into: Uuid) -> Result<(), Refusal> {
    match (status, draft) {
        ("pending", _) => Ok(()),
        ("approved", Some(d)) if d == into => Ok(()),
        ("approved", Some(d)) => Err(Refusal::InOtherDraft(d)),
        _ => Err(Refusal::NotOpen),
    }
}

// ---------------------------------------------------------------- what looks wrong

/// One served position on the route, as [`problems`] reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub sequence: i32,
    pub stop_id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

/// Metres as responses give them: to a tenth.
fn metres(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// What looks wrong with the route now: no shape to draw, a stop list too short
/// to ride, a stop listed twice running, stops whose coordinate is itself under
/// review, and the worst way round the route takes to call somewhere.
///
/// `served` is the route's calls in sequence order with shaping markers, jump
/// and hidden stops already left out - the same rows a detour is measured over
/// anywhere else in the editor. Pure, so the judgements are tested without a
/// database, and so the loader's snapshot can be read against the same rules.
pub fn problems(served: &[Call], has_polyline: bool, stops_in_review: usize) -> Vec<Problem> {
    let mut out = Vec::new();
    if !has_polyline {
        out.push(Problem::warning(
            "no_polyline",
            None,
            "the route has no shape, so it cannot be drawn on a map".to_string(),
        ));
    }
    if served.len() < MIN_SERVED_STOPS {
        out.push(Problem::error(
            "too_few_stops",
            None,
            format!(
                "the route has {} served stop(s); a bus cannot be ridden from anywhere to anywhere",
                served.len()
            ),
        ));
    } else if served.len() < SHORT_STOP_LIST {
        out.push(Problem::warning(
            "short_stop_list",
            None,
            format!(
                "the route has only {} served stops; check whether any are missing",
                served.len()
            ),
        ));
    }
    // the same stop twice running is a bus stopping where it already is: either a
    // duplicated row or two ids for one place that were never merged
    let repeated: Vec<&Call> = served
        .windows(2)
        .filter(|w| w[0].stop_id == w[1].stop_id)
        .map(|w| &w[1])
        .collect();
    if let Some(first) = repeated.first() {
        out.push(Problem::warning(
            "repeated_stop",
            Some(&first.stop_id),
            format!(
                "{} stop(s) are listed twice running, the first at sequence {} ({})",
                repeated.len(),
                first.sequence,
                first.name
            ),
        ));
    }
    if stops_in_review > 0 {
        out.push(Problem::warning(
            "stops_under_position_review",
            None,
            format!(
                "{stops_in_review} stop(s) on this route have a coordinate review open; \
                 the route's shape may be wrong because a stop is"
            ),
        ));
    }
    // the worst single call: how far the bus goes out of its way to reach it,
    // measured exactly as the cleanup context measures it (section 9)
    let worst = served
        .windows(3)
        .map(|w| {
            let here = (w[1].lat, w[1].lon);
            // a neighbour that is this stop is no way round
            let place = |n: &Call| {
                if n.stop_id == w[1].stop_id {
                    here
                } else {
                    (n.lat, n.lon)
                }
            };
            (detour_m(place(&w[0]), here, place(&w[2])), &w[1])
        })
        .filter(|(d, _)| *d > context::WORST_DETOUR_METRES)
        .max_by(|a, b| a.0.total_cmp(&b.0));
    if let Some((d, call)) = worst {
        out.push(Problem::warning(
            "worst_detour",
            Some(&call.stop_id),
            format!(
                "the bus goes {} m out of its way to call at {} (sequence {})",
                metres(d),
                call.name,
                call.sequence
            ),
        ));
    }
    out
}

/// The codes a stored `reasons` array names, in the order it names them. A
/// reason that is not an object, or carries no `code`, is skipped rather than
/// failing the read: the column is written by a loader outside this codebase.
pub fn reason_codes(reasons: &Value) -> Vec<String> {
    reasons
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r.get("code")?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The route's served calls, in sequence order.
async fn calls(conn: &mut PgConnection, g: &str, route_id: &str) -> EditorResult<Vec<Call>> {
    Ok(sqlx::query(
        "SELECT rs.sequence, rs.stop_id, s.name, s.lat, s.lon FROM gtfs_route_stop rs \
         JOIN gtfs_stop s ON s.gtfs_id = rs.gtfs_id AND s.stop_id = rs.stop_id \
         WHERE rs.gtfs_id = $1 AND rs.route_id = $2 AND rs.stop_type <> ALL($3) \
         ORDER BY rs.sequence",
    )
    .bind(g)
    .bind(route_id)
    .bind(&UNSERVED_TYPES[..])
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Call, sqlx::Error> {
        Ok(Call {
            sequence: r.try_get("sequence")?,
            stop_id: r.try_get("stop_id")?,
            name: r.try_get("name")?,
            lat: r.try_get("lat")?,
            lon: r.try_get("lon")?,
        })
    })
    .collect::<Result<Vec<_>, _>>()?)
}

// ---------------------------------------------------------------- reads

pub async fn list(
    state: &EditorState,
    gtfs_id: &str,
    status: Option<&str>,
    q: Option<&str>,
    reason: Option<&str>,
    page: &Page,
) -> EditorResult<Value> {
    let statuses = parse_status_list(status, &STATUSES)?;
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    if reason.is_some_and(|r| !REASON_CODES.contains(&r)) {
        return Err(EditorError::bad_request(
            "invalid_reason",
            format!("reason is one of {}", REASON_CODES.join(", ")),
        ));
    }
    let q = q.map(str::trim).filter(|s| !s.is_empty());
    let rows = sqlx::query(&format!(
        "{SELECT} \
         WHERE r.gtfs_id = $1 AND r.status = ANY($2) \
           AND ($3::text IS NULL OR r.route_id = $3 \
                OR r.route_short_name ILIKE $4 OR r.route_long_name ILIKE $4 \
                OR r.route_short_name % $3) \
           AND ($5::text IS NULL OR r.reasons @> $5::jsonb) \
         ORDER BY (r.route_id = $3) DESC NULLS LAST, \
                  array_position($6::text[], r.status), r.queue_rank, r.review_id \
         LIMIT $7 OFFSET $8"
    ))
    .bind(gtfs_id)
    .bind(&statuses)
    .bind(q)
    .bind(q.map(service::like_pattern))
    .bind(reason.map(|r| json!([{"code": r}]).to_string()))
    .bind(&STATUSES[..])
    .bind(page.limit + 1)
    .bind(page.offset)
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| from_row(r).map(|x| x.json))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(page.wrap(items))
}

/// Counts by status, the reasons the waiting reviews were queued for, and what
/// the queue is measured in - a dashboard that says "1,200 bookings" without
/// saying bookings of what would be inventing a unit.
pub async fn summary(state: &EditorState, gtfs_id: &str) -> EditorResult<Value> {
    let mut out =
        json!({"pending": 0, "approved": 0, "committed": 0, "confirmed": 0, "rejected": 0});
    for r in sqlx::query(
        "SELECT status, count(*) AS n FROM gtfs_route_review WHERE gtfs_id = $1 GROUP BY status",
    )
    .bind(gtfs_id)
    .fetch_all(&state.pool)
    .await?
    .iter()
    {
        let status: String = r.try_get("status")?;
        if out.get(&status).is_some() {
            out[status] = json!(r.try_get::<i64, _>("n")?);
        }
    }
    let mut reasons = serde_json::Map::new();
    for code in REASON_CODES {
        reasons.insert(code.into(), json!(0));
    }
    for r in sqlx::query(
        "SELECT x->>'code' AS code, count(*) AS n FROM gtfs_route_review r, \
              LATERAL jsonb_array_elements(CASE WHEN jsonb_typeof(r.reasons) = 'array' \
                                                THEN r.reasons ELSE '[]'::jsonb END) x \
         WHERE r.gtfs_id = $1 AND r.status = 'pending' AND x->>'code' IS NOT NULL GROUP BY 1",
    )
    .bind(gtfs_id)
    .fetch_all(&state.pool)
    .await?
    .iter()
    {
        let code: String = r.try_get("code")?;
        if reasons.contains_key(&code) {
            reasons.insert(code, json!(r.try_get::<i64, _>("n")?));
        }
    }
    out["reasons"] = Value::Object(reasons);
    let batch = sqlx::query(
        "SELECT batch, measure, measure_window, count(*) AS n FROM gtfs_route_review \
         WHERE gtfs_id = $1 GROUP BY batch, measure, measure_window ORDER BY max(review_id) DESC LIMIT 1",
    )
    .bind(gtfs_id)
    .fetch_optional(&state.pool)
    .await?;
    out["batch"] = match batch {
        Some(b) => json!({
            "batch": b.try_get::<String, _>("batch")?,
            "measure": b.try_get::<String, _>("measure")?,
            "measure_window": b.try_get::<Option<String>, _>("measure_window")?,
            "reviews": b.try_get::<i64, _>("n")?,
        }),
        None => Value::Null,
    };
    Ok(out)
}

/// The review, the route as it is now, what looks wrong with it TODAY (not at
/// load time), and the cleanup context the route page already shows - its worst
/// detours, the stops on it under coordinate review, its history and the open
/// drafts that touch it. A deleted or missing route still answers: the review
/// and why it was queued are the point, and "the route is gone" is an answer.
pub async fn detail(conn: &mut PgConnection, id: i64) -> EditorResult<Value> {
    let r = load(conn, id, false).await?;
    let route = service::route_row(conn, &r.gtfs_id, &r.route_id).await?;
    let live = route
        .as_ref()
        .is_some_and(|x| !x["deleted"].as_bool().unwrap_or(false));
    let served = if live {
        calls(conn, &r.gtfs_id, &r.route_id).await?
    } else {
        vec![]
    };
    let has_polyline = route.as_ref().is_some_and(|x| {
        x["encoded_polyline"]
            .as_str()
            .is_some_and(|p| !p.trim().is_empty())
    });
    let ctx = if route.is_some() {
        context::route(conn, &r.gtfs_id, &r.route_id).await?
    } else {
        Value::Null
    };
    let in_review = ctx["stops_with_reviews"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| matches!(x["status"].as_str(), Some("pending" | "approved")))
                .filter_map(|x| x["stop_id"].as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        })
        .unwrap_or(0);

    let mut out = r.json;
    out["route"] = route.unwrap_or(Value::Null);
    out["stop_count"] = json!(served.len());
    out["has_polyline"] = json!(has_polyline);
    out["stops"] = json!(served
        .iter()
        .map(
            |c| json!({"sequence": c.sequence, "stop_id": c.stop_id, "name": c.name,
                        "lat": c.lat, "lon": c.lon})
        )
        .collect::<Vec<_>>());
    out["problems"] = json!(if live {
        problems(&served, has_polyline, in_review)
    } else {
        vec![Problem::error(
            "route_missing",
            None,
            format!("route {} is gone from the feed", r.route_id),
        )]
    });
    out["context"] = ctx;
    Ok(out)
}

// ---------------------------------------------------------------- actions

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixBody {
    pub change_set_id: Uuid,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NoteBody {
    #[serde(default)]
    pub note: Option<String>,
}

fn note_text(note: Option<&str>) -> Option<String> {
    note.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

fn not_open(r: &Review) -> EditorError {
    EditorError::conflict(
        "review_not_open",
        format!("route review {} is {}", r.review_id, r.status),
    )
    .with_details(json!({"status": r.status, "change_set_id": r.json["change_set_id"]}))
}

/// Tie the draft holding the operator's fix to the review. The fix itself was
/// made with the ordinary route editor, so this only looks for it: the draft
/// must already hold a `route` or `route_stops` change keyed on this route, or
/// there is nothing to record and the call is refused. The review's `change_id`
/// is the earliest such change.
pub async fn fix(state: &EditorState, ctx: &Ctx, id: i64, body: FixBody) -> EditorResult<Value> {
    let note = note_text(body.note.as_deref());
    retry_transient(|| fix_once(state, ctx, id, body.change_set_id, note.as_deref())).await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id).await
}

async fn fix_once(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    change_set_id: Uuid,
    note: Option<&str>,
) -> EditorResult<()> {
    let mut tx = state.pool.begin().await?;
    // the feed's advisory lock first, as in every transaction that touches a
    // draft, so this never collides with a commit
    lock_feed_of_set(&mut tx, change_set_id).await?;
    let set = service::load_set(&mut tx, change_set_id, true).await?;
    let r = load(&mut tx, id, true).await?;
    if r.gtfs_id != set.gtfs_id {
        return Err(EditorError::bad_request(
            "feed_mismatch",
            format!(
                "the draft is for feed {} and the review for feed {}",
                set.gtfs_id, r.gtfs_id
            ),
        ));
    }
    service::editable(&set)?;
    next_status(&r.status, Transition::Fix).ok_or_else(|| not_open(&r))?;
    may_tie(&r.status, r.change_set_id, set.change_set_id).map_err(|refusal| match refusal {
        Refusal::InOtherDraft(other) => EditorError::conflict(
            "review_in_other_draft",
            format!(
                "route review {id} already follows draft {other}; add the fix to that draft, \
                 or take its changes out of it first"
            ),
        )
        .with_details(json!({"change_set_id": other})),
        Refusal::NotOpen => not_open(&r),
    })?;
    let change_id: Option<i64> = sqlx::query(
        "SELECT change_id FROM gtfs_change WHERE change_set_id = $1 \
           AND entity IN ('route', 'route_stops') AND entity_key = $2 \
         ORDER BY position LIMIT 1",
    )
    .bind(set.change_set_id)
    .bind(&r.route_id)
    .fetch_optional(&mut *tx)
    .await?
    .map(|row| row.try_get("change_id"))
    .transpose()?;
    let Some(change_id) = change_id else {
        return Err(EditorError::conflict(
            "no_change_for_route",
            format!(
                "draft {} does not change route {}; make the fix on the route page first, \
                 then record it here",
                set.change_set_id, r.route_id
            ),
        )
        .with_details(json!({"route_id": r.route_id, "change_set_id": set.change_set_id})));
    };
    sqlx::query(
        "UPDATE gtfs_route_review SET status = 'approved', change_set_id = $2, change_id = $3, \
            reviewed_by = $4, reviewed_at = now(), \
            review_note = CASE WHEN status = 'approved' THEN coalesce($5, review_note) ELSE $5 END \
         WHERE review_id = $1",
    )
    .bind(id)
    .bind(set.change_set_id)
    .bind(change_id)
    .bind(ctx.user.user_id)
    .bind(note)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE gtfs_change_set SET updated_at = now() WHERE change_set_id = $1")
        .bind(set.change_set_id)
        .execute(&mut *tx)
        .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "route_review_fixed",
        Some(&r.gtfs_id),
        Some(set.change_set_id),
        json!({"review_id": id, "route_id": r.route_id, "change_id": change_id,
               "queue_rank": r.queue_rank, "measure": r.measure, "note": note}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Close a pending review with no change: `Confirm` when the route is right as
/// it stands, `Reject` when it is not worth fixing.
async fn close(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    t: Transition,
    action: &str,
    note: Option<&str>,
) -> EditorResult<Value> {
    let note = note_text(note);
    let mut tx = state.pool.begin().await?;
    let r = load(&mut tx, id, true).await?;
    let status = next_status(&r.status, t).ok_or_else(|| not_open(&r))?;
    sqlx::query(
        "UPDATE gtfs_route_review SET status = $2, reviewed_by = $3, reviewed_at = now(), \
            review_note = $4 WHERE review_id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(ctx.user.user_id)
    .bind(&note)
    .execute(&mut *tx)
    .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        action,
        Some(&r.gtfs_id),
        None,
        json!({"review_id": id, "route_id": r.route_id, "queue_rank": r.queue_rank, "note": note}),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id).await
}

pub async fn confirm(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    note: Option<&str>,
) -> EditorResult<Value> {
    close(
        state,
        ctx,
        id,
        Transition::Confirm,
        "route_review_confirmed",
        note,
    )
    .await
}

pub async fn reject(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    note: Option<&str>,
) -> EditorResult<Value> {
    close(
        state,
        ctx,
        id,
        Transition::Reject,
        "route_review_rejected",
        note,
    )
    .await
}

/// A confirmed or rejected review is pending again, its review fields cleared.
pub async fn reopen(state: &EditorState, ctx: &Ctx, id: i64) -> EditorResult<Value> {
    let mut tx = state.pool.begin().await?;
    let r = load(&mut tx, id, true).await?;
    let status = next_status(&r.status, Transition::Reopen).ok_or_else(|| {
        EditorError::conflict(
            "review_not_closed",
            format!(
                "route review {id} is {}; only a confirmed or rejected review is reopened",
                r.status
            ),
        )
    })?;
    let reopened = sqlx::query(
        "UPDATE gtfs_route_review SET status = $2, reviewed_by = NULL, reviewed_at = NULL, \
            review_note = NULL WHERE review_id = $1",
    )
    .bind(id)
    .bind(status)
    .execute(&mut *tx)
    .await;
    match reopened {
        Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") => {
            return Err(EditorError::conflict(
                "review_superseded",
                format!("another open review already covers route {}", r.route_id),
            ));
        }
        other => {
            other?;
        }
    }
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "route_review_reopened",
        Some(&r.gtfs_id),
        None,
        json!({"review_id": id, "route_id": r.route_id, "was": r.status,
               "note": r.json["review_note"]}),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id).await
}

/// What the operator found, written down without closing anything. A walked
/// route often needs a second visit - a driver to ask, a stop to check on the
/// ground - and that belongs on the review rather than in someone's head.
pub async fn note(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    note: Option<&str>,
) -> EditorResult<Value> {
    let note = note_text(note);
    let mut tx = state.pool.begin().await?;
    let r = load(&mut tx, id, true).await?;
    if r.status == "superseded" {
        return Err(not_open(&r));
    }
    sqlx::query("UPDATE gtfs_route_review SET review_note = $2 WHERE review_id = $1")
        .bind(id)
        .bind(&note)
        .execute(&mut *tx)
        .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "route_review_noted",
        Some(&r.gtfs_id),
        r.change_set_id,
        json!({"review_id": id, "route_id": r.route_id, "note": note}),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id).await
}

// ---------------------------------------------------------------- lifecycle

/// What pending again means for a review: no draft, no change, no reviewer.
const BACK_TO_PENDING: &str =
    "UPDATE gtfs_route_review SET status = 'pending', change_set_id = NULL, change_id = NULL, \
        reviewed_by = NULL, reviewed_at = NULL, review_note = NULL";

/// A change left an open draft. A review whose draft no longer changes its route
/// is pending again; one whose draft still does keeps following it, with
/// `change_id` moved on to whatever is now earliest.
pub async fn change_removed(
    conn: &mut PgConnection,
    ctx: &Ctx,
    gtfs_id: &str,
    change_set_id: Uuid,
    entity: &str,
    entity_key: &str,
) -> Result<(), sqlx::Error> {
    if !matches!(entity, "route" | "route_stops") {
        return Ok(());
    }
    let reviews: Vec<(i64, Option<i64>)> = sqlx::query(
        "SELECT review_id, change_id FROM gtfs_route_review \
         WHERE change_set_id = $1 AND status = 'approved' AND route_id = $2 \
         ORDER BY review_id FOR UPDATE",
    )
    .bind(change_set_id)
    .bind(entity_key)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(i64, Option<i64>), sqlx::Error> {
        Ok((r.try_get("review_id")?, r.try_get("change_id")?))
    })
    .collect::<Result<_, _>>()?;
    for (review_id, change_id) in reviews {
        let left: Option<i64> = sqlx::query(
            "SELECT change_id FROM gtfs_change WHERE change_set_id = $1 \
               AND entity IN ('route', 'route_stops') AND entity_key = $2 \
             ORDER BY position LIMIT 1",
        )
        .bind(change_set_id)
        .bind(entity_key)
        .fetch_optional(&mut *conn)
        .await?
        .map(|r| r.try_get("change_id"))
        .transpose()?;
        match left {
            None => {
                sqlx::query(&format!("{BACK_TO_PENDING} WHERE review_id = $1"))
                    .bind(review_id)
                    .execute(&mut *conn)
                    .await?;
                auth::audit(
                    &mut *conn,
                    Some(ctx.user.user_id),
                    Some(&ctx.user.email),
                    "route_review_returned",
                    Some(gtfs_id),
                    Some(change_set_id),
                    json!({"review_id": review_id, "route_id": entity_key,
                           "reason": "change_removed"}),
                )
                .await?;
            }
            Some(first) if Some(first) != change_id => {
                sqlx::query("UPDATE gtfs_route_review SET change_id = $2 WHERE review_id = $1")
                    .bind(review_id)
                    .bind(first)
                    .execute(&mut *conn)
                    .await?;
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// A discarded draft returns every review that followed it to pending.
pub async fn draft_discarded(
    conn: &mut PgConnection,
    ctx: &Ctx,
    gtfs_id: &str,
    change_set_id: Uuid,
) -> Result<(), sqlx::Error> {
    let details = sqlx::query(&format!(
        "{BACK_TO_PENDING} WHERE change_set_id = $1 AND status = 'approved' \
         RETURNING review_id, route_id"
    ))
    .bind(change_set_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        Ok(json!({
            "review_id": r.try_get::<i64, _>("review_id")?,
            "route_id": r.try_get::<String, _>("route_id")?,
            "reason": "change_set_discarded",
        }))
    })
    .collect::<Result<Vec<_>, _>>()?;
    auth::audit_many(
        &mut *conn,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "route_review_returned",
        Some(gtfs_id),
        Some(change_set_id),
        &details,
    )
    .await
}

/// The draft carrying these reviews' fixes is committed.
pub async fn mark_committed(
    conn: &mut PgConnection,
    ctx: &Ctx,
    gtfs_id: &str,
    change_set_id: Uuid,
    feed_version: i64,
) -> Result<(), sqlx::Error> {
    let details = sqlx::query(
        "UPDATE gtfs_route_review SET status = 'committed' \
         WHERE change_set_id = $1 AND status = 'approved' \
         RETURNING review_id, route_id, change_id",
    )
    .bind(change_set_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        Ok(json!({
            "review_id": r.try_get::<i64, _>("review_id")?,
            "route_id": r.try_get::<String, _>("route_id")?,
            "change_id": r.try_get::<Option<i64>, _>("change_id")?,
            "feed_version": feed_version,
        }))
    })
    .collect::<Result<Vec<_>, _>>()?;
    auth::audit_many(
        &mut *conn,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "route_review_committed",
        Some(gtfs_id),
        Some(change_set_id),
        &details,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::validation::Level;

    fn call(sequence: i32, stop_id: &str, lat: f64, lon: f64) -> Call {
        Call {
            sequence,
            stop_id: stop_id.into(),
            name: format!("STOP {stop_id}"),
            lat,
            lon,
        }
    }

    /// A straight run of well-spaced stops with a shape is a route nobody needs
    /// to look at, and must produce nothing at all.
    fn straight() -> Vec<Call> {
        (0..6)
            .map(|i| call(i + 1, &format!("S{i}"), 13.0 + f64::from(i) * 0.01, 80.2))
            .collect()
    }

    fn codes(found: &[Problem]) -> Vec<&str> {
        found.iter().map(|p| p.code).collect()
    }

    #[test]
    fn a_healthy_route_has_nothing_wrong_with_it() {
        assert!(problems(&straight(), true, 0).is_empty());
    }

    #[test]
    fn a_route_without_a_shape_cannot_be_drawn() {
        assert_eq!(codes(&problems(&straight(), false, 0)), ["no_polyline"]);
    }

    /// One stop cannot be ridden from anywhere to anywhere: that is an error,
    /// while merely short is a place to start looking.
    #[test]
    fn too_few_stops_is_an_error_and_short_is_a_warning() {
        let one = problems(&straight()[..1], true, 0);
        assert_eq!(codes(&one), ["too_few_stops"]);
        assert_eq!(one[0].level, Level::Error);
        let four = problems(&straight()[..4], true, 0);
        assert_eq!(codes(&four), ["short_stop_list"]);
        assert_eq!(four[0].level, Level::Warning);
        // exactly SHORT_STOP_LIST stops is not short
        assert!(problems(&straight()[..SHORT_STOP_LIST], true, 0).is_empty());
    }

    /// An empty stop list is too few stops, not a panic on `windows(2)`.
    #[test]
    fn a_route_with_no_stops_at_all_still_answers() {
        assert_eq!(codes(&problems(&[], true, 0)), ["too_few_stops"]);
    }

    #[test]
    fn a_stop_listed_twice_running_is_reported_once_with_a_count() {
        let mut stops = straight();
        stops[2] = call(3, "S1", 13.01, 80.2);
        stops[4] = call(5, "S3", 13.03, 80.2);
        let found = problems(&stops, true, 0);
        assert_eq!(codes(&found), ["repeated_stop"]);
        assert!(found[0].message.contains("2 stop(s)"), "{found:?}");
        assert_eq!(found[0].stop_id.as_deref(), Some("S1"));
    }

    /// The same stop twice but NOT running is an out-and-back spur, which this
    /// data has 153 of; it is not a defect.
    #[test]
    fn the_same_stop_twice_apart_is_a_spur_not_a_repeat() {
        let mut stops = straight();
        stops[4] = call(5, "S0", 13.0, 80.2);
        assert!(!codes(&problems(&stops, true, 0)).contains(&"repeated_stop"));
    }

    #[test]
    fn the_worst_detour_is_the_single_worst_call_not_every_one() {
        let mut stops = straight();
        stops[1] = call(2, "S1", 13.01, 80.25); // ~5 km off the line
        stops[3] = call(4, "S3", 13.03, 80.21); // ~1 km off the line
        let found = problems(&stops, true, 0);
        assert_eq!(codes(&found), ["worst_detour"]);
        assert_eq!(found[0].stop_id.as_deref(), Some("S1"));
    }

    /// A stop that is its own neighbour moves with it, so a spur's repeated call
    /// is not also reported as a vast detour.
    #[test]
    fn a_neighbour_that_is_this_stop_is_no_way_round() {
        let stops = vec![
            call(1, "A", 13.0, 80.2),
            call(2, "B", 13.0, 80.2),
            call(3, "B", 13.05, 80.2),
        ];
        assert!(!codes(&problems(&stops, true, 0)).contains(&"worst_detour"));
    }

    #[test]
    fn stops_under_coordinate_review_are_a_reason_to_look() {
        let found = problems(&straight(), true, 3);
        assert_eq!(codes(&found), ["stops_under_position_review"]);
        assert!(found[0].message.contains('3'), "{found:?}");
    }

    /// Every code `problems` can answer with has to be filterable and countable,
    /// or the queue's filter and its detail page mean different things.
    #[test]
    fn every_problem_code_is_a_known_reason_code() {
        let mut stops = straight();
        stops[2] = call(3, "S1", 13.01, 80.25);
        let (short, one) = (straight(), straight());
        let found = [
            problems(&stops, false, 1),
            problems(&one[..1], true, 0),
            problems(&short[..3], true, 0),
        ];
        let seen: Vec<&str> = found.iter().flat_map(|f| codes(f)).collect();
        for code in &seen {
            assert!(REASON_CODES.contains(code), "{code} is not a reason code");
        }
        // and nothing is only theoretical: every reason code is reachable
        for code in REASON_CODES {
            assert!(seen.contains(&code), "{code} was never produced");
        }
    }

    #[test]
    fn a_reasons_array_names_its_codes_and_survives_rubbish() {
        assert_eq!(
            reason_codes(
                &json!([{"code": "no_polyline"}, {"code": "worst_detour", "detour_m": 12}])
            ),
            ["no_polyline", "worst_detour"]
        );
        assert!(reason_codes(&json!([])).is_empty());
        assert!(reason_codes(&json!({"code": "no_polyline"})).is_empty());
        assert!(reason_codes(&json!([1, "x", {"detail": "no code"}, null])).is_empty());
    }

    #[test]
    fn a_review_follows_one_draft_at_a_time() {
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        assert_eq!(may_tie("pending", None, a), Ok(()));
        assert_eq!(may_tie("approved", Some(a), a), Ok(()));
        assert_eq!(
            may_tie("approved", Some(a), b),
            Err(Refusal::InOtherDraft(a))
        );
        for closed in ["committed", "confirmed", "rejected", "superseded"] {
            assert_eq!(may_tie(closed, None, a), Err(Refusal::NotOpen));
        }
    }

    #[test]
    fn the_statuses_go_where_section_14_says() {
        use Transition::*;
        assert_eq!(next_status("pending", Fix), Some("approved"));
        assert_eq!(next_status("approved", Fix), Some("approved"));
        assert_eq!(next_status("pending", Confirm), Some("confirmed"));
        assert_eq!(next_status("pending", Reject), Some("rejected"));
        assert_eq!(next_status("approved", Return), Some("pending"));
        assert_eq!(next_status("approved", Commit), Some("committed"));
        // a judgement about priority can be revisited; a committed fix cannot
        assert_eq!(next_status("rejected", Reopen), Some("pending"));
        assert_eq!(next_status("confirmed", Reopen), Some("pending"));
        assert_eq!(next_status("committed", Reopen), None);
        assert_eq!(next_status("superseded", Reopen), None);
        // a closed review takes no action
        for closed in ["committed", "confirmed", "rejected", "superseded"] {
            assert_eq!(next_status(closed, Fix), None);
            assert_eq!(next_status(closed, Confirm), None);
            assert_eq!(next_status(closed, Reject), None);
        }
        assert_eq!(next_status("pending", Reopen), None);
    }

    /// Every status the table admits has to be a status the list can be filtered
    /// by, or a review can be in a state nothing shows.
    #[test]
    fn the_status_list_matches_the_migration() {
        assert_eq!(
            STATUSES.len(),
            6,
            "0014_route_reviews.sql admits six statuses"
        );
        for s in [
            "pending",
            "approved",
            "committed",
            "confirmed",
            "rejected",
            "superseded",
        ] {
            assert!(STATUSES.contains(&s));
        }
    }
}
