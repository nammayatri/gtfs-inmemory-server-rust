//! Routes to review (docs/gtfs-editor.md section 19.2): routes whose fare
//! stages do not line up with MTC's own route definition, queued in
//! `gtfs_route_stage_issue` by the stage backfill
//! (`scripts/backfill_stages.py`), and worked through exactly as the stage
//! reviews are ([`super::stage_reviews`], section 19.1).
//!
//! The backfill names a fare stage by MTC's `bus_stop_id` for its head, which it
//! gets by lining our fare stages up against the replica's **by position**, and
//! only when the two lists are the same length. Where they are not, it pairs
//! nothing for that route rather than guess - a guess slides the shorter list
//! against the longer one and hands every stage after the divergence the wrong
//! id, silently and for good. Those stages then fall back to being named by
//! their own names, which is where a stage id like `nm_SAIDAPET` comes from, and
//! the same real fare stage ends up as two stages: one MTC-keyed, one not.
//!
//! So a row here is one **route**, and it carries both lists. Nothing is missing
//! from a table: both sides are complete and they disagree, usually because MTC
//! has changed the route since our feed was built. Only somebody who knows the
//! route can say which side is right, which is what this queue is for.
//!
//! A person either
//!
//!   - **fixes it**: changes the route's stages so they say what the route
//!     really is (the ordinary stages editor, section 18), and closes the row
//!     naming the draft, or
//!   - **confirms it**: our feed is right and MTC's replica is behind, or the
//!     other way about and it is not ours to change; the note says which.
//!
//! Nothing here edits anything itself. The fix goes into a draft and goes live
//! the way every other one does, by somebody else approving it.

use super::auth::{self, Ctx};
use super::error::{EditorError, EditorResult};
use super::proposals::{parse_status_list, ListQuery};
use super::service::{self, Page};
use super::EditorState;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub const STATUSES: [&str; 4] = ["pending", "fixed", "confirmed", "superseded"];

/// What is wrong with the route, and the order the list puts them in.
pub const ISSUES: [&str; 7] = [
    // our fare stages and MTC's are a different number, so the stages that did
    // not match could not be given an MTC id
    "count_differs",
    // neither the same set of stages nor the same order
    "set_differs",
    // the names repeat on one side, so no stage could be identified safely
    "ambiguous_names",
    // MTC's replica does not carry the route at all
    "absent",
    // the replica carries it and our feed does not
    "missing_internal",
    // the same stages in another order
    "order_differs",
    // the same stages in the same places, spelled differently. Much the
    // commonest and much the least urgent: 92% of these are one place written
    // two ways (M.G.R.KOYAMBEDU against M.G.R.KOYAMBEDU B.T), so the queue
    // shows them last.
    "name_differs",
];

const SELECT: &str = "SELECT i.issue_id, i.gtfs_id, i.batch, i.route_id, i.short_name, \
        i.issue, i.ours::text AS ours, i.theirs::text AS theirs, i.stages_unkeyed, \
        i.status, i.change_set_id, \
        cs.title AS change_set_title, cs.status AS change_set_status, \
        u.email AS reviewed_by_email, i.reviewed_at, i.review_note, \
        i.created_at, i.updated_at, \
        r.short_name AS live_short_name, r.long_name AS live_long_name, r.deleted \
     FROM gtfs_route_stage_issue i \
     LEFT JOIN gtfs_change_set cs ON cs.change_set_id = i.change_set_id \
     LEFT JOIN gtfs_editor_user u ON u.user_id = i.reviewed_by \
     LEFT JOIN gtfs_route r ON r.gtfs_id = i.gtfs_id AND r.route_id = i.route_id";

pub struct Issue {
    pub issue_id: i64,
    pub gtfs_id: String,
    pub route_id: String,
    pub issue: String,
    pub status: String,
    pub json: Value,
}

fn list_of(r: &PgRow, col: &str) -> Result<Value, sqlx::Error> {
    Ok(r.try_get::<Option<String>, _>(col)?
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!([])))
}

fn from_row(r: &PgRow) -> Result<Issue, sqlx::Error> {
    type Ts = Option<chrono::DateTime<chrono::Utc>>;
    let issue = Issue {
        issue_id: r.try_get("issue_id")?,
        gtfs_id: r.try_get("gtfs_id")?,
        route_id: r.try_get("route_id")?,
        issue: r.try_get("issue")?,
        status: r.try_get("status")?,
        json: Value::Null,
    };
    let ours = list_of(r, "ours")?;
    let theirs = list_of(r, "theirs")?;
    let json = json!({
        "issue_id": issue.issue_id,
        "gtfs_id": issue.gtfs_id,
        "batch": r.try_get::<String, _>("batch")?,
        "route_id": issue.route_id,
        // what the route was called when it was raised, and what it is called
        // now: a route renamed since then still reads right in the queue
        "short_name": r.try_get::<Option<String>, _>("live_short_name")?
            .or(r.try_get::<Option<String>, _>("short_name")?),
        "long_name": r.try_get::<Option<String>, _>("live_long_name")?,
        "route_deleted": r.try_get::<Option<bool>, _>("deleted")?.unwrap_or(false),
        "issue": issue.issue,
        "ours": ours,
        "theirs": theirs,
        "stages_unkeyed": r.try_get::<i32, _>("stages_unkeyed")?,
        "status": issue.status,
        "change_set_id": r.try_get::<Option<Uuid>, _>("change_set_id")?,
        "change_set_title": r.try_get::<Option<String>, _>("change_set_title")?,
        "change_set_status": r.try_get::<Option<String>, _>("change_set_status")?,
        "reviewed_by_email": r.try_get::<Option<String>, _>("reviewed_by_email")?,
        "reviewed_at": r.try_get::<Ts, _>("reviewed_at")?,
        "review_note": r.try_get::<Option<String>, _>("review_note")?,
        "created_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?,
        "updated_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?,
    });
    Ok(Issue { json, ..issue })
}

async fn load(conn: &mut PgConnection, id: i64, lock: bool) -> EditorResult<Issue> {
    let sql = format!(
        "{SELECT} WHERE i.issue_id = $1{}",
        if lock { " FOR UPDATE OF i" } else { "" }
    );
    let row = sqlx::query(&sql)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| {
            EditorError::not_found("route_issue_not_found", format!("no route issue {id}"))
        })?;
    Ok(from_row(&row)?)
}

/// `GET /feeds/{g}/route-issues`: the queue, heaviest first.
pub async fn list(
    state: &EditorState,
    gtfs_id: &str,
    query: &ListQuery,
    issue: Option<&str>,
    page: &Page,
) -> EditorResult<Value> {
    let statuses = parse_status_list(query.status.as_deref(), &STATUSES)?;
    let issue = issue.map(str::trim).filter(|r| !r.is_empty());
    if issue.is_some_and(|r| !ISSUES.contains(&r)) {
        return Err(EditorError::bad_request(
            "invalid_issue",
            format!("issue is one of {}", ISSUES.join(", ")),
        ));
    }
    let q = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let rows = sqlx::query(&format!(
        "{SELECT} \
         WHERE i.gtfs_id = $1 AND i.status = ANY($2) \
           AND ($3::text IS NULL OR i.issue = $3) \
           AND ($4::text IS NULL OR i.route_id = $4 OR i.short_name ILIKE $5 \
                OR r.short_name ILIKE $5) \
         ORDER BY array_position($6::text[], i.status), i.stages_unkeyed DESC, \
                  array_position($7::text[], i.issue), \
                  CASE WHEN i.route_id ~ '^[0-9]+$' THEN 0 ELSE 1 END, \
                  CASE WHEN i.route_id ~ '^[0-9]+$' THEN i.route_id::bigint END, \
                  i.route_id \
         LIMIT $8 OFFSET $9"
    ))
    .bind(gtfs_id)
    .bind(&statuses)
    .bind(issue)
    .bind(q)
    .bind(q.map(service::like_pattern))
    .bind(&STATUSES[..])
    .bind(&ISSUES[..])
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

/// `GET /feeds/{g}/route-issues/summary`: how much is left, and of what.
pub async fn summary(state: &EditorState, gtfs_id: &str) -> EditorResult<Value> {
    let row = sqlx::query(
        "SELECT count(*) FILTER (WHERE status = 'pending') AS pending, \
                count(*) FILTER (WHERE status = 'fixed') AS fixed, \
                count(*) FILTER (WHERE status = 'confirmed') AS confirmed, \
                count(*) FILTER (WHERE status = 'superseded') AS superseded, \
                coalesce(sum(stages_unkeyed) FILTER (WHERE status = 'pending'), 0) AS stages_unkeyed, \
                count(*) FILTER (WHERE status = 'pending' AND issue = 'count_differs') AS count_differs, \
                count(*) FILTER (WHERE status = 'pending' AND issue = 'set_differs') AS set_differs, \
                count(*) FILTER (WHERE status = 'pending' AND issue = 'ambiguous_names') AS ambiguous_names, \
                count(*) FILTER (WHERE status = 'pending' AND issue = 'absent') AS absent, \
                count(*) FILTER (WHERE status = 'pending' AND issue = 'missing_internal') AS missing_internal, \
                count(*) FILTER (WHERE status = 'pending' AND issue = 'order_differs') AS order_differs, \
                count(*) FILTER (WHERE status = 'pending' AND issue = 'name_differs') AS name_differs \
         FROM gtfs_route_stage_issue WHERE gtfs_id = $1",
    )
    .bind(gtfs_id)
    .fetch_one(&state.pool)
    .await?;
    let n = |c: &str| -> Result<i64, sqlx::Error> { row.try_get(c) };
    Ok(json!({
        "gtfs_id": gtfs_id,
        "pending": n("pending")?,
        "fixed": n("fixed")?,
        "confirmed": n("confirmed")?,
        "superseded": n("superseded")?,
        // how many stages carry a name-keyed id because of the routes still open
        "stages_unkeyed": row.try_get::<i64, _>("stages_unkeyed")?,
        "issues": {
            "count_differs": n("count_differs")?,
            "set_differs": n("set_differs")?,
            "ambiguous_names": n("ambiguous_names")?,
            "absent": n("absent")?,
            "missing_internal": n("missing_internal")?,
            "order_differs": n("order_differs")?,
            "name_differs": n("name_differs")?,
        },
    }))
}

/// `GET /route-issues/{id}`: the row, the two lists, and the stages this route
/// is on that are still named by name rather than by MTC's id.
pub async fn detail(conn: &mut PgConnection, id: i64) -> EditorResult<Value> {
    let issue = load(conn, id, false).await?;
    let stages = sqlx::query(
        "SELECT rs.stage_no, rs.stage_id, rs.direction, s.name, \
                (SELECT count(*) FROM gtfs_stage_stop ss \
                  WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id \
                    AND ss.direction = s.direction) AS stop_count, \
                (SELECT count(DISTINCT o.route_id) FROM gtfs_route_stage o \
                  WHERE o.gtfs_id = rs.gtfs_id AND o.stage_id = rs.stage_id \
                    AND o.direction = rs.direction) AS route_count, \
                EXISTS (SELECT 1 FROM gtfs_stage t \
                         WHERE t.gtfs_id = s.gtfs_id AND t.name = s.name \
                           AND t.direction = s.direction AND NOT t.deleted \
                           AND t.stage_id <> s.stage_id \
                           AND t.stage_id NOT LIKE 'nm\\_%') AS twin \
           FROM gtfs_route_stage rs \
           JOIN gtfs_stage s ON s.gtfs_id = rs.gtfs_id AND s.stage_id = rs.stage_id \
                            AND s.direction = rs.direction \
          WHERE rs.gtfs_id = $1 AND rs.route_id = $2 AND rs.variant_id IS NULL \
          ORDER BY rs.position",
    )
    .bind(&issue.gtfs_id)
    .bind(&issue.route_id)
    .fetch_all(&mut *conn)
    .await?;
    let stages = stages
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            let stage_id: String = r.try_get("stage_id")?;
            let direction: String = r.try_get("direction")?;
            Ok(json!({
                "stage_no": r.try_get::<i32, _>("stage_no")?,
                "stage_id": stage_id,
                "direction": (!direction.is_empty()).then(|| direction.clone()),
                "stage_key": format!("{stage_id}|{direction}"),
                "name": r.try_get::<String, _>("name")?,
                "stop_count": r.try_get::<i64, _>("stop_count")?,
                "route_count": r.try_get::<i64, _>("route_count")?,
                // named by its own name, for want of an MTC id
                "name_keyed": stage_id.starts_with("nm_"),
                // and there is an MTC-keyed stage of the same name and
                // direction, so this one is a twin of it
                "twin": r.try_get::<bool, _>("twin")?,
            }))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    let mut out = issue.json;
    out["stages"] = json!(stages);
    Ok(out)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseBody {
    /// `fixed` (the change is in a draft) or `confirmed` (nothing to change).
    pub decision: String,
    /// Why. Required for `confirmed`: saying the two really do differ and that
    /// is right means saying how you know.
    pub note: Option<String>,
    /// The draft the fix went into, for `fixed`.
    pub change_set: Option<Uuid>,
}

fn note_text(note: Option<&str>) -> Option<String> {
    note.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

/// `POST /route-issues/{id}/close`.
pub async fn close(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    body: &CloseBody,
) -> EditorResult<Value> {
    let decision = body.decision.trim();
    if !["fixed", "confirmed"].contains(&decision) {
        return Err(EditorError::bad_request(
            "invalid_decision",
            "decision is fixed (the change is in a draft) or confirmed (nothing to change)",
        ));
    }
    let note = note_text(body.note.as_deref());
    if decision == "confirmed" && note.is_none() {
        return Err(EditorError::bad_request(
            "note_required",
            "a confirmed row says in its note why the route is right as it stands",
        ));
    }
    if decision == "confirmed" && body.change_set.is_some() {
        return Err(EditorError::bad_request(
            "invalid_change_set",
            "a confirmed row changes nothing, so it names no draft",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let r = load(&mut tx, id, true).await?;
    if r.status != "pending" {
        return Err(EditorError::conflict(
            "route_issue_not_pending",
            format!(
                "route {} is {}; only a pending row is closed",
                r.route_id, r.status
            ),
        ));
    }
    if let Some(set) = body.change_set {
        let row = sqlx::query("SELECT gtfs_id FROM gtfs_change_set WHERE change_set_id = $1")
            .bind(set)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| {
                EditorError::not_found("change_set_not_found", format!("no change set {set}"))
            })?;
        if row.try_get::<String, _>("gtfs_id")? != r.gtfs_id {
            return Err(EditorError::bad_request(
                "feed_mismatch",
                format!("change set {set} is not on feed {}", r.gtfs_id),
            ));
        }
    }
    sqlx::query(
        "UPDATE gtfs_route_stage_issue SET status = $2, reviewed_by = $3, \
            reviewed_at = now(), review_note = $4, change_set_id = $5, updated_at = now() \
         WHERE issue_id = $1",
    )
    .bind(id)
    .bind(decision)
    .bind(ctx.user.user_id)
    .bind(&note)
    .bind(body.change_set)
    .execute(&mut *tx)
    .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "route_issue_closed",
        Some(&r.gtfs_id),
        body.change_set,
        json!({
            "issue_id": id,
            "route_id": r.route_id,
            "issue": r.issue,
            "decision": decision,
            "note": note,
        }),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id).await
}

/// `POST /route-issues/{id}/reopen`: pending again.
pub async fn reopen(state: &EditorState, ctx: &Ctx, id: i64) -> EditorResult<Value> {
    let mut tx = state.pool.begin().await?;
    let r = load(&mut tx, id, true).await?;
    if r.status == "pending" {
        return Err(EditorError::conflict(
            "route_issue_pending",
            format!("route {} is already pending", r.route_id),
        ));
    }
    if r.status == "superseded" {
        return Err(EditorError::conflict(
            "route_issue_superseded",
            format!(
                "route {} was raised again by a later run; work on that row",
                r.route_id
            ),
        ));
    }
    // the partial unique index allows one pending row per route, so a reopen
    // that would make two says so rather than failing on the constraint
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gtfs_route_stage_issue \
          WHERE gtfs_id = $1 AND route_id = $2 AND status = 'pending'",
    )
    .bind(&r.gtfs_id)
    .bind(&r.route_id)
    .fetch_one(&mut *tx)
    .await?;
    if open > 0 {
        return Err(EditorError::conflict(
            "route_issue_pending",
            format!("route {} already has a row open", r.route_id),
        ));
    }
    sqlx::query(
        "UPDATE gtfs_route_stage_issue SET status = 'pending', reviewed_by = NULL, \
            reviewed_at = NULL, review_note = NULL, change_set_id = NULL, updated_at = now() \
         WHERE issue_id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "route_issue_reopened",
        Some(&r.gtfs_id),
        None,
        json!({"issue_id": id, "route_id": r.route_id, "was": r.status}),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id).await
}
