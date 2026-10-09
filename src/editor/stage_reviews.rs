//! Stage reviews (docs/gtfs-editor.md section 19.1): stage names whose routes
//! do not agree about what the name means, queued in `gtfs_stage_review` by the
//! stage backfill (`scripts/backfill_stages.py`). The operations team works
//! through them here, exactly as it works through coordinate reviews
//! ([`super::position_reviews`], section 8).
//!
//! One row is one **stage name and direction**, not one stage. When 36 stages
//! carry the name M.G.R.CENTRAL because 36 routes disagree about where it
//! begins, that is one decision to make. The row names no stage ids of its own:
//! the stages it covers are looked up live by name and direction, so a stage
//! renamed or deleted under an open review does not leave the row pointing at
//! nothing.
//!
//! A person either
//!
//!   - **fixes it**, through the editing that already exists - renaming the
//!     stages apart (`stage/update`), merging duplicate stops (`stop/merge`,
//!     section 8.2), or correcting a stage's stops - and closes the review
//!     naming the draft the fix went into, or
//!   - **confirms it**: the routes really do differ, nothing is to be changed,
//!     and the note says why.
//!
//! Either way the review is closed and `gtfs_stage.review` is cleared on every
//! stage of the name, in the same transaction, so the flag and the queue never
//! disagree. Reopening puts both back. There is deliberately no bespoke "fix"
//! action here: a stage review is closed by a person who has already made the
//! change in a draft, and that draft goes live the way every other one does,
//! by somebody else approving it.

use super::auth::{self, Ctx};
use super::error::{EditorError, EditorResult};
use super::proposals::{parse_status_list, ListQuery};
use super::service::{self, Page};
use super::stages::{self, StageKey};
use super::EditorState;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub const STATUSES: [&str; 4] = ["pending", "fixed", "confirmed", "superseded"];

/// Why a name is in the queue. The same vocabulary as `gtfs_stage.review`, and
/// the order the list puts them in: the ones that need a decision about what a
/// name means first, the tidying last.
pub const REASONS: [&str; 4] = [
    "head_differs",
    "head_duplicate_stops",
    "stretch_differs",
    // not a disagreement: every route gave the stage the same stops, and it is
    // raised so the team can look over what was mapped and say it is right
    "agreed",
];

/// A review changing fewer stop calls than this is small. Still raised, still in
/// the queue, just behind the ones that matter: on chennai_bus 776 of 2,037 rows
/// are at or above it and they hold 92% of the difference, which is what makes
/// the queue finishable.
pub const WORTH_A_LOOK: i32 = 20;

/// A draft in one of these never goes live: a review closed as fixed naming it
/// says fixed for a fix that is not coming. Kept as SQL, for the queries.
pub const LOST_DRAFT: &str = "('discarded', 'rejected')";

/// `side`: `settle` is the reviews somebody has to decide (every reason but
/// `agreed`), `verify` the ones that mapped cleanly and only want looking over:
/// the split the summary counts in. As SQL, whether `reason = 'agreed'`.
pub fn parse_side(side: Option<&str>) -> EditorResult<Option<bool>> {
    match side.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some("settle") => Ok(Some(false)),
        Some("verify") => Ok(Some(true)),
        Some(_) => Err(EditorError::bad_request(
            "invalid_side",
            "side is settle or verify",
        )),
    }
}

/// The last committed draft that changed the stage `id` (SQL), or merged a
/// stage into it, or split one off it, after `since` (SQL): an open review of a
/// stage somebody has since fixed without saying so. `dir` (SQL) narrows it to
/// one direction; without it, either. A jsonb object, or NULL.
fn changed_since_sql(id: &str, dir: Option<&str>, since: &str) -> String {
    let keys = match dir {
        Some(d) => format!("ARRAY[{id} || '|' || {d}]"),
        None => format!("ARRAY[{id} || '|up', {id} || '|down', {id} || '|', {id}]"),
    };
    format!(
        "(SELECT jsonb_build_object('change_set_id', cs.change_set_id, 'title', cs.title, \
                 'committed_at', cs.committed_at) \
            FROM gtfs_change ch \
            JOIN gtfs_change_set cs ON cs.change_set_id = ch.change_set_id \
           WHERE cs.gtfs_id = $1 AND cs.status = 'committed' AND ch.entity = 'stage' \
             AND (ch.entity_key = ANY({keys}) \
                  OR ch.after->>'into_stage_id' = ANY({keys}) \
                  OR ch.after->>'from_stage_id' = ANY({keys})) \
             AND cs.committed_at > {since} \
           ORDER BY cs.committed_at DESC LIMIT 1)"
    )
}

const SELECT: &str = "SELECT r.review_id, r.gtfs_id, r.batch, r.name, r.name_key, r.direction, \
        r.reason, r.impact, r.evidence::text AS evidence, r.status, r.change_set_id, \
        cs.title AS change_set_title, cs.status AS change_set_status, \
        u.email AS reviewed_by_email, r.reviewed_at, r.review_note, \
        r.created_at, r.updated_at \
     FROM gtfs_stage_review r \
     LEFT JOIN gtfs_change_set cs ON cs.change_set_id = r.change_set_id \
     LEFT JOIN gtfs_editor_user u ON u.user_id = r.reviewed_by";

pub struct Review {
    pub review_id: i64,
    pub gtfs_id: String,
    pub name: String,
    pub name_key: String,
    pub direction: Option<String>,
    pub reason: String,
    pub status: String,
    pub json: Value,
}

fn from_row(r: &PgRow) -> Result<Review, sqlx::Error> {
    type Ts = Option<chrono::DateTime<chrono::Utc>>;
    let evidence = r
        .try_get::<Option<String>, _>("evidence")?
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| json!({}));
    let review = Review {
        review_id: r.try_get("review_id")?,
        gtfs_id: r.try_get("gtfs_id")?,
        name: r.try_get("name")?,
        name_key: r.try_get("name_key")?,
        direction: r.try_get("direction")?,
        reason: r.try_get("reason")?,
        status: r.try_get("status")?,
        json: Value::Null,
    };
    let json = json!({
        "review_id": review.review_id,
        "gtfs_id": review.gtfs_id,
        "batch": r.try_get::<String, _>("batch")?,
        "name": review.name,
        "direction": review.direction,
        "reason": review.reason,
        // how many stop calls the backfill's guess gets wrong here
        "impact": r.try_get::<i32, _>("impact")?,
        "evidence": evidence,
        "status": review.status,
        "change_set_id": r.try_get::<Option<Uuid>, _>("change_set_id")?,
        "change_set_title": r.try_get::<Option<String>, _>("change_set_title")?,
        "change_set_status": r.try_get::<Option<String>, _>("change_set_status")?,
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
            EditorError::not_found("review_not_found", format!("no stage review {id}"))
        })?;
    Ok(from_row(&row)?)
}

/// The live stages the review is about: every stage of that name and direction.
/// Looked up rather than stored, so the row keeps up with the editing done under
/// it - which is the whole point of the review.
async fn stage_ids(
    conn: &mut PgConnection,
    g: &str,
    name_key: &str,
    direction: Option<&str>,
) -> EditorResult<Vec<StageKey>> {
    // The review names one stage: the backfill writes its id into name_key, and
    // direction is the other half of its key. An older row, raised before a
    // stage was identified by MTC's stop id, holds the folded name instead, so
    // that is tried too.
    let rows = sqlx::query(
        "SELECT stage_id, direction FROM gtfs_stage \
         WHERE gtfs_id = $1 AND NOT deleted AND direction = coalesce($3, '') \
           AND (stage_id = $2 \
                OR upper(btrim(regexp_replace(name, '\\s+', ' ', 'g'))) = $2) \
         ORDER BY stage_id",
    )
    .bind(g)
    .bind(name_key)
    .bind(direction)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(StageKey::new(
                r.try_get("stage_id")?,
                Some(r.try_get("direction")?),
            ))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(Into::into)
}

// ---------------------------------------------------------------- reading

/// `GET /feeds/{g}/stage-reviews?group=stage`: one row per STAGE ID, with its
/// two directions inside it.
///
/// A stage id is one of MTC's fare-stage stops, and it is nearly always used
/// both ways - so a row each would show every stage twice over. Grouped by id,
/// the team sees the stop once and steps between up and down from inside.
///
/// Names are NOT grouped: MTC gives 160 names to more than one stop, and those
/// really are different places. GOVT ESTATE METRO R.S is stops 143, 148, 170 and
/// 1834, so it is four rows here - whether any of them are the same place is
/// what the merge is for.
#[allow(clippy::too_many_arguments)]
pub async fn list_by_stage(
    state: &EditorState,
    gtfs_id: &str,
    query: &ListQuery,
    reason: Option<&str>,
    side: Option<bool>,
    min_impact: Option<i32>,
    lost: bool,
    page: &Page,
) -> EditorResult<Value> {
    let statuses = parse_status_list(query.status.as_deref(), &STATUSES)?;
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    if reason.is_some_and(|r| !REASONS.contains(&r)) {
        return Err(EditorError::bad_request(
            "invalid_reason",
            format!("reason is one of {}", REASONS.join(", ")),
        ));
    }
    let q = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    // what has happened to the stage since its open reviews were raised
    let changed = changed_since_sql(
        "r.name_key",
        None,
        "min(r.created_at) FILTER (WHERE r.status = 'pending')",
    );
    let rows = sqlx::query(&format!(
        "SELECT r.name_key AS stage_id, max(r.name) AS name, \
                count(*) AS parts, \
                count(*) FILTER (WHERE r.status = 'pending') AS left_to_do, \
                count(*) FILTER (WHERE r.status = 'fixed' AND cs.status IN {LOST_DRAFT}) AS lost, \
                coalesce(sum(r.impact), 0)::int AS impact, \
                min(r.review_id) FILTER (WHERE r.status = 'pending') AS next_id, \
                min(r.review_id) FILTER (WHERE r.status = 'fixed' AND cs.status IN {LOST_DRAFT}) \
                    AS lost_id, \
                min(r.review_id) AS any_id, \
                array_agg(DISTINCT r.reason) AS reasons, \
                array_agg(DISTINCT nullif(r.direction, '')) AS directions, \
                (SELECT coalesce(jsonb_agg(jsonb_build_object( \
                            'direction', nullif(d.direction, ''), 'status', d.status) \
                          ORDER BY d.direction), '[]'::jsonb) \
                   FROM (SELECT DISTINCT ON (x.direction) x.direction, x.status \
                           FROM gtfs_stage_review x \
                          WHERE x.gtfs_id = $1 AND x.name_key = r.name_key \
                            AND x.status IN ('fixed', 'confirmed') \
                            AND NOT EXISTS (SELECT 1 FROM gtfs_stage_review p \
                                 WHERE p.gtfs_id = x.gtfs_id AND p.name_key = x.name_key \
                                   AND p.direction = x.direction AND p.status = 'pending') \
                          ORDER BY x.direction, x.reviewed_at DESC NULLS LAST) d) AS closed_ways, \
                {changed} AS changed \
         FROM gtfs_stage_review r \
         LEFT JOIN gtfs_change_set cs ON cs.change_set_id = r.change_set_id \
         WHERE r.gtfs_id = $1 AND r.status = ANY($2) \
           AND ($3::text IS NULL OR r.reason = $3) \
           AND ($4::text IS NULL OR r.name ILIKE $5 OR r.name % $4 \
                OR lower(r.name_key) = lower($4)) \
           AND ($8::int IS NULL OR r.impact >= $8) \
           AND (NOT $9 OR (r.status = 'fixed' AND cs.status IN {LOST_DRAFT})) \
           AND ($10::bool IS NULL OR (r.reason = 'agreed') = $10) \
         GROUP BY r.name_key \
         ORDER BY ($4::text IS NOT NULL \
                   AND NOT bool_or(r.name ILIKE $5 OR lower(r.name_key) = lower($4))), \
                  coalesce(sum(r.impact), 0) DESC, max(r.name), r.name_key \
         LIMIT $6 OFFSET $7"
    ))
    .bind(gtfs_id)
    .bind(&statuses)
    .bind(reason)
    .bind(q)
    .bind(q.map(service::like_pattern))
    .bind(page.limit + 1)
    .bind(page.offset)
    .bind(min_impact)
    .bind(lost)
    .bind(side)
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            let left: i64 = r.try_get("left_to_do")?;
            let directions: Vec<Option<String>> = r.try_get("directions")?;
            Ok(json!({
                // the other ways of this stage, closed with nothing left open:
                // up fixed while down still waits is one row, and says so
                "closed_ways": r.try_get::<Value, _>("closed_ways")?,
                // the last committed draft to change the stage after its open
                // reviews were raised, which may have fixed it unannounced
                "changed": r.try_get::<Option<Value>, _>("changed")?,
                "stage_id": r.try_get::<String, _>("stage_id")?,
                "name": r.try_get::<Option<String>, _>("name")?,
                // how many of this stop's directions are in the queue, and how
                // many still want a person
                "parts": r.try_get::<i64, _>("parts")?,
                "left_to_do": left,
                "done": r.try_get::<i64, _>("parts")? - left,
                "impact": r.try_get::<i32, _>("impact")?,
                // closed as fixed naming a draft that was discarded or
                // rejected: the fix never went live
                "lost": r.try_get::<i64, _>("lost")?,
                // the one to open: the first still waiting, else the first
                // whose fix was lost, else the first of them
                "review_id": r
                    .try_get::<Option<i64>, _>("next_id")?
                    .or(r.try_get::<Option<i64>, _>("lost_id")?)
                    .or(r.try_get::<Option<i64>, _>("any_id")?),
                "reasons": r.try_get::<Vec<String>, _>("reasons")?,
                "directions": directions.into_iter().flatten().collect::<Vec<_>>(),
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(page.wrap(items))
}

/// `GET /feeds/{g}/stage-reviews`. `reason` narrows to one reason, `side` to
/// one side of the work ([`parse_side`]); `q` matches the name, or the stage id
/// exactly.
#[allow(clippy::too_many_arguments)]
pub async fn list(
    state: &EditorState,
    gtfs_id: &str,
    query: &ListQuery,
    reason: Option<&str>,
    side: Option<bool>,
    min_impact: Option<i32>,
    lost: bool,
    page: &Page,
) -> EditorResult<Value> {
    let statuses = parse_status_list(query.status.as_deref(), &STATUSES)?;
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    if reason.is_some_and(|r| !REASONS.contains(&r)) {
        return Err(EditorError::bad_request(
            "invalid_reason",
            format!("reason is one of {}", REASONS.join(", ")),
        ));
    }
    let q = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let rows = sqlx::query(&format!(
        "{SELECT} \
         WHERE r.gtfs_id = $1 AND r.status = ANY($2) \
           AND ($3::text IS NULL OR r.reason = $3) \
           AND ($4::text IS NULL OR r.name ILIKE $5 OR r.name % $4 \
                OR lower(r.name_key) = lower($4)) \
           AND ($10::int IS NULL OR r.impact >= $10) \
           AND (NOT $11 OR (r.status = 'fixed' AND cs.status IN {LOST_DRAFT})) \
           AND ($12::bool IS NULL OR (r.reason = 'agreed') = $12) \
         ORDER BY ($4::text IS NOT NULL \
                   AND NOT (r.name ILIKE $5 OR lower(r.name_key) = lower($4))), \
                  array_position($6::text[], r.status), r.impact DESC, \
                  array_position($7::text[], r.reason), r.name, r.review_id \
         LIMIT $8 OFFSET $9"
    ))
    .bind(gtfs_id)
    .bind(&statuses)
    .bind(reason)
    .bind(q)
    .bind(q.map(service::like_pattern))
    .bind(&STATUSES[..])
    .bind(&REASONS[..])
    .bind(page.limit + 1)
    .bind(page.offset)
    .bind(min_impact)
    .bind(lost)
    .bind(side)
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| from_row(r).map(|x| x.json))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(page.wrap(items))
}

/// `GET /feeds/{g}/stage-reviews/summary`: how much is left, and of what.
pub async fn summary(state: &EditorState, gtfs_id: &str) -> EditorResult<Value> {
    // Everything here is counted in STAGE IDS, because that is what the list
    // shows: one row per MTC fare-stage stop, with its two directions inside.
    // Counting review rows instead would put 4,041 beside a list of half that.
    //
    // A stop is counted under a status when either of its directions is in it,
    // and under a reason when either carries it - so the reasons do not sum to
    // the total, and a stop with one direction done is in both `pending` and
    // `fixed`.
    let side = |agreed: bool| async move {
        let rows = sqlx::query(&format!(
            "SELECT count(DISTINCT name_key) FILTER (WHERE r.status = 'pending') AS pending, \
                    count(DISTINCT name_key) FILTER (WHERE r.status = 'fixed') AS fixed, \
                    count(DISTINCT name_key) FILTER (WHERE r.status = 'fixed' \
                        AND cs.status IN {LOST_DRAFT}) AS fixed_lost, \
                    count(DISTINCT name_key) FILTER (WHERE r.status = 'confirmed') AS confirmed, \
                    count(DISTINCT name_key) FILTER (WHERE r.status = 'superseded') AS superseded, \
                    count(DISTINCT name_key) FILTER (WHERE r.status = 'pending' AND impact >= $3) \
                        AS worth_a_look, \
                    coalesce(sum(impact) FILTER (WHERE r.status = 'pending' AND impact >= $3), 0) \
                        AS big_impact, \
                    coalesce(sum(impact) FILTER (WHERE r.status = 'pending'), 0) AS all_impact \
             FROM gtfs_stage_review r \
             LEFT JOIN gtfs_change_set cs ON cs.change_set_id = r.change_set_id \
             WHERE r.gtfs_id = $1 AND (reason = 'agreed') = $2"
        ))
        .bind(gtfs_id)
        .bind(agreed)
        .bind(WORTH_A_LOOK)
        .fetch_one(&state.pool)
        .await?;
        let pending: i64 = rows.try_get("pending")?;
        let big: i64 = rows.try_get("worth_a_look")?;
        let big_impact: i64 = rows.try_get("big_impact")?;
        let all_impact: i64 = rows.try_get("all_impact")?;
        Ok::<Value, sqlx::Error>(json!({
            "pending": pending,
            "fixed": rows.try_get::<i64, _>("fixed")?,
            // of those, the ones whose draft was discarded or rejected
            "fixed_lost": rows.try_get::<i64, _>("fixed_lost")?,
            "confirmed": rows.try_get::<i64, _>("confirmed")?,
            "superseded": rows.try_get::<i64, _>("superseded")?,
            "worth_a_look": {
                "threshold": WORTH_A_LOOK,
                "names": big,
                "small": pending - big,
                "share_of_difference": if all_impact > 0 { (100 * big_impact) / all_impact } else { 0 },
            },
        }))
    };
    let settle = side(false).await?;
    let verify = side(true).await?;

    // names having at least one stage raised for each reason, still pending
    let rows = sqlx::query(
        "SELECT reason, count(DISTINCT name_key) AS n FROM gtfs_stage_review \
         WHERE gtfs_id = $1 AND status = 'pending' GROUP BY reason",
    )
    .bind(gtfs_id)
    .fetch_all(&state.pool)
    .await?;
    let mut reasons = json!({});
    for reason in REASONS {
        reasons[reason] = json!(0);
    }
    for r in &rows {
        let reason: String = r.try_get("reason")?;
        if reasons.get(&reason).is_some() {
            reasons[reason] = json!(r.try_get::<i64, _>("n")?);
        }
    }

    Ok(json!({
        "settle": settle,
        "verify": verify,
        // by reason, in names, for the chips on the settle side
        "reason": reasons,
    }))
}

/// `GET /stage-reviews/{id}`: the review, and every stage of its name in full -
/// its stops and the routes that use it - which is what the two sides of the
/// disagreement look like.
pub async fn detail(conn: &mut PgConnection, id: i64) -> EditorResult<Value> {
    let r = load(conn, id, false).await?;
    let ids = stage_ids(conn, &r.gtfs_id, &r.name_key, r.direction.as_deref()).await?;
    let mut stages_json = Vec::with_capacity(ids.len());
    for stage_id in &ids {
        stages_json.push(stages::stage_detail(conn, &r.gtfs_id, stage_id).await?);
    }
    // the biggest list first: it is the one the others are judged against
    stages_json.sort_by_key(|s| {
        (
            -(s["rows"].as_array().map_or(0, |a| a.len()) as i64),
            s["stage_id"].as_str().unwrap_or("").to_string(),
        )
    });
    // Stages of the same name going the SAME way: what this one may be merged
    // into. MTC gives 160 names more than one stop, so a name like GOVT ESTATE
    // METRO R.S is several stages each way; whether they are really one place is
    // a person's call. The other direction is never offered - the two hold
    // different stops, and merging across them would hand a route the wrong ones.
    let mut siblings = Vec::new();
    if let Some(mine) = stages_json.first() {
        let name = mine["name"].as_str().unwrap_or_default();
        let rows = sqlx::query(
            "SELECT s.stage_id, s.direction, s.name, s.review, \
                    (SELECT count(*) FROM gtfs_stage_stop ss \
                      WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id \
                        AND ss.direction = s.direction) AS stop_count, \
                    (SELECT count(DISTINCT rs.route_id) FROM gtfs_route_stage rs \
                      WHERE rs.gtfs_id = s.gtfs_id AND rs.stage_id = s.stage_id \
                        AND rs.direction = s.direction) AS route_count \
             FROM gtfs_stage s \
             WHERE s.gtfs_id = $1 AND NOT s.deleted AND s.name = $2 \
               AND s.direction = $3 AND s.stage_id <> $4 \
             ORDER BY s.stage_id",
        )
        .bind(&r.gtfs_id)
        .bind(name)
        .bind(r.direction.clone().unwrap_or_default())
        .bind(mine["stage_id"].as_str().unwrap_or_default())
        .fetch_all(&mut *conn)
        .await?;
        for row in &rows {
            let key = StageKey::new(row.try_get("stage_id")?, Some(row.try_get("direction")?));
            siblings.push(json!({
                "stage_id": row.try_get::<String, _>("stage_id")?,
                "stage_key": key.entity_key(),
                "direction": key.direction_opt(),
                "name": row.try_get::<String, _>("name")?,
                "review": row.try_get::<Option<String>, _>("review")?,
                "stop_count": row.try_get::<i64, _>("stop_count")?,
                "route_count": row.try_get::<i64, _>("route_count")?,
            }));
        }
    }

    // The other ways THIS stop is run: a fare stage is nearly always both up and
    // down, and somebody settling it wants to step between them without going
    // back to the list. Other stops of the same name are not here - those are
    // different places, and separate rows in the list.
    let fam = sqlx::query(
        "SELECT review_id, name_key, direction, reason, impact, status, \
                reviewed_by IS NOT NULL AS looked_at \
         FROM gtfs_stage_review \
         WHERE gtfs_id = $1 AND name_key = $2 AND status <> 'superseded' \
         ORDER BY direction, impact DESC",
    )
    .bind(&r.gtfs_id)
    .bind(&r.name_key)
    .fetch_all(&mut *conn)
    .await?;
    let family: Vec<Value> = fam
        .iter()
        .map(|row| -> Result<Value, sqlx::Error> {
            let direction: String = row.try_get("direction")?;
            Ok(json!({
                "review_id": row.try_get::<i64, _>("review_id")?,
                "stage_id": row.try_get::<String, _>("name_key")?,
                "direction": (!direction.is_empty()).then_some(direction),
                "reason": row.try_get::<String, _>("reason")?,
                "impact": row.try_get::<i32, _>("impact")?,
                "status": row.try_get::<String, _>("status")?,
                "looked_at": row.try_get::<bool, _>("looked_at")?,
            }))
        })
        .collect::<Result<_, _>>()?;

    // The routes the evidence names, by number: it holds route ids, which mean
    // nothing to the people working the queue, who know a route as 21G.
    let mut named: Vec<String> = r.json["evidence"]["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|c| c["routes"].as_array().into_iter().flatten())
        .chain(
            r.json["evidence"]["routes"]
                .as_array()
                .into_iter()
                .flatten(),
        )
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    named.sort();
    named.dedup();
    let mut route_names = serde_json::Map::new();
    if !named.is_empty() {
        for row in sqlx::query(
            "SELECT route_id, short_name FROM gtfs_route WHERE gtfs_id = $1 AND route_id = ANY($2)",
        )
        .bind(&r.gtfs_id)
        .bind(&named)
        .fetch_all(&mut *conn)
        .await?
        {
            if let Some(short) = row.try_get::<Option<String>, _>("short_name")? {
                route_names.insert(row.try_get("route_id")?, json!(short));
            }
        }
    }

    // A draft that changed this stage after the review was raised, and is live:
    // somebody may have fixed it without saying so, and the page offers to
    // close it naming that draft.
    let changed: Option<Value> = if r.status == "pending" {
        sqlx::query_scalar(&format!(
            "SELECT {}",
            changed_since_sql(
                "$2::text",
                Some("$3::text"),
                "(SELECT created_at FROM gtfs_stage_review WHERE review_id = $4)",
            )
        ))
        .bind(&r.gtfs_id)
        .bind(&r.name_key)
        .bind(r.direction.clone().unwrap_or_default())
        .bind(id)
        .fetch_one(&mut *conn)
        .await?
    } else {
        None
    };

    let mut out = r.json;
    out["changed"] = json!(changed);
    // route id -> its number, for every route the evidence names
    out["route_names"] = Value::Object(route_names);
    // the other reviews of this name, so the page can switch between them
    out["family"] = json!(family);
    out["stages"] = json!(stages_json);
    out["stage_count"] = json!(ids.len());
    // other stages of this name going the same way, which this may merge with
    out["siblings"] = json!(siblings);
    Ok(out)
}

/// The open reviews a change to these stages touches: each stage's own stage
/// review, and the open routes to review of `routes` (section 19.2). A merge or
/// a replacement settles a stage's review once its stage is gone, and may
/// settle a route's - though only a person can say the route's whole list is
/// right now - so the page offers to close them with the same draft.
pub async fn open_about(
    conn: &mut PgConnection,
    g: &str,
    keys: &[StageKey],
    routes: &[String],
) -> EditorResult<Value> {
    let reviews = sqlx::query(
        "SELECT review_id, name, name_key, direction, reason, impact FROM gtfs_stage_review \
         WHERE gtfs_id = $1 AND status = 'pending' \
           AND (name_key, direction) IN (SELECT * FROM UNNEST($2::text[], $3::text[])) \
         ORDER BY name, direction, review_id",
    )
    .bind(g)
    .bind(keys.iter().map(|k| k.stage_id.clone()).collect::<Vec<_>>())
    .bind(keys.iter().map(|k| k.direction.clone()).collect::<Vec<_>>())
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        let stage_id: String = r.try_get("name_key")?;
        let direction: String = r.try_get("direction")?;
        Ok(json!({
            "review_id": r.try_get::<i64, _>("review_id")?,
            "name": r.try_get::<String, _>("name")?,
            "stage_id": stage_id,
            "stage_key": StageKey::new(&stage_id, Some(&direction)).entity_key(),
            "direction": (!direction.is_empty()).then_some(direction),
            "reason": r.try_get::<String, _>("reason")?,
            "impact": r.try_get::<i32, _>("impact")?,
        }))
    })
    .collect::<Result<Vec<_>, _>>()?;
    let issues = sqlx::query(
        "SELECT i.issue_id, i.route_id, coalesce(r.short_name, i.short_name) AS short_name, \
                i.issue, i.stages_unkeyed \
         FROM gtfs_route_stage_issue i \
         LEFT JOIN gtfs_route r ON r.gtfs_id = i.gtfs_id AND r.route_id = i.route_id \
         WHERE i.gtfs_id = $1 AND i.status = 'pending' AND i.route_id = ANY($2) \
         ORDER BY coalesce(r.short_name, i.short_name), i.route_id",
    )
    .bind(g)
    .bind(routes)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        Ok(json!({
            "issue_id": r.try_get::<i64, _>("issue_id")?,
            "route_id": r.try_get::<String, _>("route_id")?,
            "short_name": r.try_get::<Option<String>, _>("short_name")?,
            "issue": r.try_get::<String, _>("issue")?,
            "stages_unkeyed": r.try_get::<i32, _>("stages_unkeyed")?,
        }))
    })
    .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "stage_reviews": reviews, "route_issues": issues }))
}

/// `GET /feeds/{g}/open-reviews?stages=`: [`open_about`] the stages named, and
/// the routes that run any of them today.
pub async fn open_about_stages(
    conn: &mut PgConnection,
    g: &str,
    keys: &[StageKey],
) -> EditorResult<Value> {
    let routes: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT rs.route_id FROM gtfs_route_stage rs \
         JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
         WHERE rs.gtfs_id = $1 \
           AND (rs.stage_id, rs.direction) IN (SELECT * FROM UNNEST($2::text[], $3::text[]))",
    )
    .bind(g)
    .bind(keys.iter().map(|k| k.stage_id.clone()).collect::<Vec<_>>())
    .bind(keys.iter().map(|k| k.direction.clone()).collect::<Vec<_>>())
    .fetch_all(&mut *conn)
    .await?;
    open_about(conn, g, keys, &routes).await
}

// ---------------------------------------------------------------- closing

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseBody {
    /// `fixed` (the change is in a draft) or `confirmed` (nothing to change).
    pub decision: String,
    /// Why. Required for `confirmed`: a reviewer who says the routes really do
    /// differ has to say how they know.
    pub note: Option<String>,
    /// The draft the fix went into, for `fixed`.
    pub change_set: Option<Uuid>,
}

fn note_text(note: Option<&str>) -> Option<String> {
    note.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

fn not_pending(r: &Review) -> EditorError {
    EditorError::conflict(
        "review_not_pending",
        format!(
            "stage review {} is {}; only a pending review is closed",
            r.review_id, r.status
        ),
    )
}

/// `POST /stage-reviews/{id}/close`.
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
            "a confirmed review says in its note why the routes really do differ",
        ));
    }
    if decision == "confirmed" && body.change_set.is_some() {
        return Err(EditorError::bad_request(
            "invalid_change_set",
            "a confirmed review changes nothing, so it names no draft",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let r = load(&mut tx, id, true).await?;
    if r.status != "pending" {
        return Err(not_pending(&r));
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
    let ids = stage_ids(&mut tx, &r.gtfs_id, &r.name_key, r.direction.as_deref()).await?;
    sqlx::query(
        "UPDATE gtfs_stage_review SET status = $2, reviewed_by = $3, reviewed_at = now(), \
            review_note = $4, change_set_id = $5 WHERE review_id = $1",
    )
    .bind(id)
    .bind(decision)
    .bind(ctx.user.user_id)
    .bind(&note)
    .bind(body.change_set)
    .execute(&mut *tx)
    .await?;
    // the flag and the queue say the same thing or the queue is worthless
    clear_flag(&mut tx, &r.gtfs_id, &ids, None, &ctx.user.email).await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "stage_review_closed",
        Some(&r.gtfs_id),
        body.change_set,
        json!({
            "review_id": id,
            "name": r.name,
            "direction": r.direction,
            "reason": r.reason,
            "decision": decision,
            "stages": ids.iter().map(|k| k.entity_key()).collect::<Vec<_>>(),
            "note": note,
        }),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id).await
}

/// `POST /stage-reviews/{id}/reopen`: pending again, and the flag back on its
/// stages.
pub async fn reopen(state: &EditorState, ctx: &Ctx, id: i64) -> EditorResult<Value> {
    let mut tx = state.pool.begin().await?;
    let r = load(&mut tx, id, true).await?;
    if r.status == "pending" {
        return Err(EditorError::conflict(
            "review_pending",
            format!("stage review {id} is already pending"),
        ));
    }
    if r.status == "superseded" {
        return Err(EditorError::conflict(
            "review_superseded",
            format!("stage review {id} was superseded by a later batch; reopen that one instead"),
        ));
    }
    let ids = stage_ids(&mut tx, &r.gtfs_id, &r.name_key, r.direction.as_deref()).await?;
    let reopened = sqlx::query(
        "UPDATE gtfs_stage_review SET status = 'pending', reviewed_by = NULL, \
            reviewed_at = NULL, review_note = NULL, change_set_id = NULL WHERE review_id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await;
    match reopened {
        Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") => {
            return Err(EditorError::conflict(
                "review_superseded",
                format!(
                    "another open review already covers the stage name {}",
                    r.name
                ),
            ));
        }
        other => {
            other?;
        }
    }
    clear_flag(&mut tx, &r.gtfs_id, &ids, Some(&r.reason), &ctx.user.email).await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "stage_review_reopened",
        Some(&r.gtfs_id),
        None,
        json!({
            "review_id": id,
            "name": r.name,
            "direction": r.direction,
            "reason": r.reason,
            "was": r.status,
            "stages": ids.iter().map(|k| k.entity_key()).collect::<Vec<_>>(),
        }),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id).await
}

/// The open reviews a committed draft settles by itself: those about a stage it
/// merged away or deleted, with no stage of the review's key left. Closed as
/// fixed naming the draft and whoever made it, in the commit's transaction, so
/// the queue never offers a stage that is not there any more. A review that
/// names a stage by its folded name closes only once the last stage of that
/// name and direction is gone. Returns what it closed, for the audit.
pub async fn close_settled(
    conn: &mut PgConnection,
    g: &str,
    set: Uuid,
    title: &str,
    author: Uuid,
) -> EditorResult<Vec<Value>> {
    let rows = sqlx::query(
        "WITH gone AS ( \
            SELECT s.stage_id, s.direction, ch.op, \
                   split_part(ch.after->>'into_stage_id', '|', 1) AS into_id, \
                   upper(btrim(regexp_replace(s.name, '\\s+', ' ', 'g'))) AS folded \
              FROM gtfs_change ch \
              JOIN gtfs_stage s ON s.gtfs_id = $1 AND s.deleted \
               AND s.stage_id = split_part(ch.entity_key, '|', 1) \
               AND s.direction = split_part(ch.entity_key, '|', 2) \
             WHERE ch.change_set_id = $2 AND ch.entity = 'stage' \
               AND ch.op IN ('merge', 'delete')) \
         UPDATE gtfs_stage_review r \
            SET status = 'fixed', reviewed_by = $3, reviewed_at = now(), change_set_id = $2, \
                review_note = CASE WHEN g.op = 'merge' \
                    THEN format('Closed on its own: draft “%s” merged stage %s into %s.', \
                                $4::text, g.stage_id, g.into_id) \
                    ELSE format('Closed on its own: draft “%s” deleted stage %s.', \
                                $4::text, g.stage_id) END \
           FROM gone g \
          WHERE r.gtfs_id = $1 AND r.status = 'pending' AND r.direction = g.direction \
            AND (r.name_key = g.stage_id OR r.name_key = g.folded) \
            AND NOT EXISTS (SELECT 1 FROM gtfs_stage s \
                 WHERE s.gtfs_id = r.gtfs_id AND NOT s.deleted AND s.direction = r.direction \
                   AND (s.stage_id = r.name_key \
                        OR upper(btrim(regexp_replace(s.name, '\\s+', ' ', 'g'))) = r.name_key)) \
         RETURNING r.review_id, r.name, r.direction, r.reason, g.op, g.stage_id, g.into_id",
    )
    .bind(g)
    .bind(set)
    .bind(author)
    .bind(title)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|row| -> Result<Value, sqlx::Error> {
            let direction: String = row.try_get("direction")?;
            let op: String = row.try_get("op")?;
            Ok(json!({
                "review_id": row.try_get::<i64, _>("review_id")?,
                "name": row.try_get::<String, _>("name")?,
                "direction": (!direction.is_empty()).then_some(direction),
                "reason": row.try_get::<String, _>("reason")?,
                "decision": "fixed",
                // nobody pressed Mark fixed: the draft took the stage away
                "automatic": true,
                "stage": row.try_get::<String, _>("stage_id")?,
                "merged_into": (op == "merge")
                    .then(|| row.try_get::<String, _>("into_id"))
                    .transpose()?,
            }))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Set `gtfs_stage.review` on the stages a review covers: `None` clears it,
/// `Some(reason)` puts it back. Untouched rows are left alone so a reopen does
/// not bump a stage's `row_version` for nothing.
async fn clear_flag(
    conn: &mut PgConnection,
    g: &str,
    keys: &[StageKey],
    reason: Option<&str>,
    actor: &str,
) -> EditorResult<()> {
    if keys.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE gtfs_stage SET review = $4, updated_by = $5 \
         WHERE gtfs_id = $1 \
           AND (stage_id, direction) IN (SELECT * FROM UNNEST($2::text[], $3::text[])) \
           AND review IS DISTINCT FROM $4",
    )
    .bind(g)
    .bind(keys.iter().map(|k| k.stage_id.clone()).collect::<Vec<_>>())
    .bind(keys.iter().map(|k| k.direction.clone()).collect::<Vec<_>>())
    .bind(reason)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
