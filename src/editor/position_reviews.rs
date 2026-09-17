//! Coordinate reviews (docs/gtfs-editor.md sections 8 and 8.1): stops whose
//! position is suspected wrong, queued in `gtfs_position_review` by nandi's
//! `load_position_reviews.py`. A person checks each one here and either
//!
//!   - moves the stop: a `stop/update` of its position goes into a draft,
//!   - splits some routes off it: a new stop at another point, and those routes'
//!     stop lists pointed at it, go into a draft,
//!   - merges it into another stop of the same name (section 8.2): a `stop/merge`
//!     goes into a draft, by the same code path any merge is added by, or
//!   - confirms that the position is right, which closes the review and changes
//!     nothing.
//!
//! A draft goes live only when someone else approves and commits it. A stop can
//! need several of these at once - two splits and a move of the stop itself - so
//! a review collects any number of actions, all in one draft. The server keeps
//! the lifecycle, as for station proposals: removing a split's new stop from the
//! draft takes that split's stop lists with it; a review whose draft holds none
//! of its changes any more, or whose draft is discarded, is pending again;
//! committing the draft marks it committed. Every action is audited.
//!
//! A position is judged by the routes through it: for every call at the stop,
//! the served stops either side and how far the bus goes out of its way to call
//! there ([`detour_m`]). That context is one query however many routes call, so
//! a stop on 300 routes costs the same round trips as a stop on one.

use super::auth::{self, Ctx};
use super::draft::DraftView;
use super::error::{EditorError, EditorResult};
use super::proposals::{parse_status_list, ListQuery, Problem};
use super::service::{self, ChangeInsert, Page};
use super::validation::{check_payload, haversine_m, valid_lat_lon, Finding, Level, RouteRow};
use super::EditorState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use std::collections::{BTreeSet, HashSet};
use uuid::Uuid;

pub const STATUSES: [&str; 5] = [
    "pending",
    "approved",
    "committed",
    "confirmed",
    "superseded",
];
/// A stop further than this from where its review was loaded has moved since:
/// a warning (the evidence may be stale), never a reason to refuse a move.
pub const MOVED_METRES: f64 = 25.0;
/// A move shorter than this leaves the stop where it is.
pub const SAME_POINT_METRES: f64 = 0.5;

const SELECT: &str =
    "SELECT r.review_id, r.gtfs_id, r.batch, r.stop_id, r.original_stop_id, r.stop_name, r.reason, \
        r.lat, r.lon, r.raw_lat, r.raw_lon, r.suggested_lat, r.suggested_lon, r.suggested_source, \
        r.evidence::text AS evidence, r.status, r.change_set_id, r.change_id, \
        cs.title AS change_set_title, u.email AS reviewed_by_email, r.reviewed_at, r.review_note, \
        r.created_at, r.updated_at \
     FROM gtfs_position_review r \
     LEFT JOIN gtfs_change_set cs ON cs.change_set_id = r.change_set_id \
     LEFT JOIN gtfs_editor_user u ON u.user_id = r.reviewed_by";

pub struct Review {
    pub review_id: i64,
    pub gtfs_id: String,
    pub stop_id: String,
    pub stop_name: String,
    /// Where the stop was when the review was loaded.
    pub lat: f64,
    pub lon: f64,
    pub status: String,
    pub change_set_id: Option<Uuid>,
    pub change_id: Option<i64>,
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
        stop_id: r.try_get("stop_id")?,
        stop_name: r.try_get("stop_name")?,
        lat: r.try_get("lat")?,
        lon: r.try_get("lon")?,
        status: r.try_get("status")?,
        change_set_id: r.try_get("change_set_id")?,
        change_id: r.try_get("change_id")?,
        json: Value::Null,
    };
    let json = json!({
        "review_id": review.review_id,
        "gtfs_id": review.gtfs_id,
        "batch": r.try_get::<String, _>("batch")?,
        "stop_id": review.stop_id,
        "original_stop_id": r.try_get::<String, _>("original_stop_id")?,
        "stop_name": review.stop_name,
        "reason": r.try_get::<String, _>("reason")?,
        "lat": review.lat,
        "lon": review.lon,
        "raw_lat": r.try_get::<Option<f64>, _>("raw_lat")?,
        "raw_lon": r.try_get::<Option<f64>, _>("raw_lon")?,
        "suggested_lat": r.try_get::<Option<f64>, _>("suggested_lat")?,
        "suggested_lon": r.try_get::<Option<f64>, _>("suggested_lon")?,
        "suggested_source": r.try_get::<Option<String>, _>("suggested_source")?,
        "evidence": evidence,
        "status": review.status,
        "change_set_id": review.change_set_id,
        "change_id": review.change_id,
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
            EditorError::not_found("review_not_found", format!("no position review {id}"))
        })?;
    Ok(from_row(&row)?)
}

// ---------------------------------------------------------------- statuses

/// What happens to a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// a reviewer adds a move of the stop to a draft
    Move,
    /// a reviewer adds a split of some routes onto a new stop to a draft
    Split,
    /// a reviewer adds a merge of the stop into another stop to a draft
    Merge,
    /// a reviewer says the position is right
    Confirm,
    /// a confirmed review is opened again
    Reopen,
    /// the review's change left its draft: removed, or the draft discarded
    Return,
    /// the draft carrying the review's change was committed
    Commit,
}

/// The status a review in `status` takes on `t`; `None` when `t` does not apply
/// to a review in that status. An approved review takes further actions only in
/// its own draft ([`may_add_action`]). Return and commit act on the approved
/// reviews of a draft, in [`change_removed`], [`draft_discarded`] and
/// [`mark_committed`].
pub fn next_status(status: &str, t: Transition) -> Option<&'static str> {
    match (status, t) {
        ("pending" | "approved", Transition::Move | Transition::Split | Transition::Merge) => {
            Some("approved")
        }
        ("pending", Transition::Confirm) => Some("confirmed"),
        ("confirmed", Transition::Reopen) => Some("pending"),
        ("approved", Transition::Return) => Some("pending"),
        ("approved", Transition::Commit) => Some("committed"),
        _ => None,
    }
}

/// Why a move or split cannot go into a draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// the review is closed: committed, confirmed or superseded
    NotPending,
    /// its actions are in another draft
    InOtherDraft(Uuid),
}

/// Whether a move or split may go into draft `into` for a review in `status`
/// whose actions are in `draft`: a pending review takes its first action, an
/// approved one more actions in the same draft only.
pub fn may_add_action(status: &str, draft: Option<Uuid>, into: Uuid) -> Result<(), Refusal> {
    match (status, draft) {
        ("pending", _) => Ok(()),
        ("approved", Some(d)) if d == into => Ok(()),
        ("approved", Some(d)) => Err(Refusal::InOtherDraft(d)),
        _ => Err(Refusal::NotPending),
    }
}

// ---------------------------------------------------------------- a review's actions in its draft

/// A change a draft holds for a review, as its actions are read from it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewChange {
    pub change_id: i64,
    pub entity: String,
    pub op: String,
    pub entity_key: String,
    /// the point a stop update or create sets; for a merge, where the stop it
    /// merges into is
    pub at: Option<(f64, f64)>,
    /// the stops a stop list's rows call at
    pub row_stops: Vec<String>,
    /// the stop a merge merges into
    pub into_stop_id: Option<String>,
    /// the routes a merge moves to that stop (its `before.affected`)
    pub merged_routes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionKind {
    /// the reviewed stop moves to the point
    Move,
    /// a new stop at the point takes these routes off the reviewed stop
    Split {
        new_stop_id: String,
        route_ids: Vec<String>,
    },
    /// the reviewed stop goes away: its routes call at this stop, at the point
    Merge {
        into_stop_id: String,
        route_ids: Vec<String>,
    },
}

/// One move, split or merge a review has in its draft.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub change_id: i64,
    pub at: (f64, f64),
    pub kind: ActionKind,
}

/// A review's actions, in change order: each stop update is a move, each stop
/// create a split whose routes are the stop lists that call at its new stop, a
/// stop merge a merge (the only action of a review that has one).
pub fn actions(changes: &[ReviewChange]) -> Vec<Action> {
    changes
        .iter()
        .filter_map(|c| {
            let at = c.at?;
            let kind = match (c.entity.as_str(), c.op.as_str()) {
                ("stop", "update") => ActionKind::Move,
                ("stop", "create") => ActionKind::Split {
                    new_stop_id: c.entity_key.clone(),
                    route_ids: changes
                        .iter()
                        .filter(|x| {
                            x.entity == "route_stops" && x.row_stops.contains(&c.entity_key)
                        })
                        .map(|x| x.entity_key.clone())
                        .collect(),
                },
                ("stop", "merge") => ActionKind::Merge {
                    into_stop_id: c.into_stop_id.clone()?,
                    route_ids: c.merged_routes.clone(),
                },
                _ => return None,
            };
            Some(Action {
                change_id: c.change_id,
                at,
                kind,
            })
        })
        .collect()
}

/// The change a review is known by in its draft: its earliest action, or its
/// earliest change when only stop lists are left; `None` when nothing is.
pub fn first_change(changes: &[ReviewChange]) -> Option<i64> {
    changes
        .iter()
        .find(|c| c.entity == "stop" && matches!(c.op.as_str(), "update" | "create" | "merge"))
        .or(changes.first())
        .map(|c| c.change_id)
}

/// The routes a review's splits in the draft take off its stop.
pub fn split_off(changes: &[ReviewChange]) -> Vec<String> {
    let mut routes: Vec<String> = changes
        .iter()
        .filter(|c| c.entity == "route_stops")
        .map(|c| c.entity_key.clone())
        .collect();
    routes.sort();
    routes.dedup();
    routes
}

/// The changes draft `change_set_id` holds for review `review_id`, in change
/// order. A merge's point is the live position of the stop it merges into, or
/// the one the draft creates that stop at.
async fn review_changes(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    review_id: i64,
) -> Result<Vec<ReviewChange>, sqlx::Error> {
    sqlx::query(
        "SELECT c.change_id, c.entity, c.op, c.entity_key, \
                coalesce((c.after->>'lat')::float8, i.lat, (k.after->>'lat')::float8) AS lat, \
                coalesce((c.after->>'lon')::float8, i.lon, (k.after->>'lon')::float8) AS lon, \
                CASE WHEN c.op = 'merge' THEN btrim(c.after->>'into_stop_id') END AS into_stop_id, \
                ARRAY(SELECT DISTINCT x->>'stop_id' FROM jsonb_array_elements( \
                          CASE WHEN jsonb_typeof(c.after->'rows') = 'array' THEN c.after->'rows' \
                               ELSE '[]'::jsonb END) x \
                      WHERE x->>'stop_id' IS NOT NULL) AS row_stops, \
                ARRAY(SELECT x->>'route_id' FROM jsonb_array_elements( \
                          CASE WHEN c.op = 'merge' AND jsonb_typeof(c.before->'affected') = 'array' \
                               THEN c.before->'affected' ELSE '[]'::jsonb END) x \
                      WHERE x->>'route_id' IS NOT NULL) AS merged_routes \
         FROM gtfs_change c \
         JOIN gtfs_change_set cs ON cs.change_set_id = c.change_set_id \
         LEFT JOIN gtfs_stop i ON c.op = 'merge' AND i.gtfs_id = cs.gtfs_id \
              AND i.stop_id = btrim(c.after->>'into_stop_id') \
         LEFT JOIN gtfs_change k ON c.op = 'merge' AND k.change_set_id = c.change_set_id \
              AND k.entity = 'stop' AND k.op = 'create' \
              AND k.entity_key = btrim(c.after->>'into_stop_id') \
         WHERE c.change_set_id = $1 AND c.after->>'position_review_id' = $2 ORDER BY c.position",
    )
    .bind(change_set_id)
    .bind(review_id.to_string())
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<ReviewChange, sqlx::Error> {
        let (lat, lon): (Option<f64>, Option<f64>) = (r.try_get("lat")?, r.try_get("lon")?);
        Ok(ReviewChange {
            change_id: r.try_get("change_id")?,
            entity: r.try_get("entity")?,
            op: r.try_get("op")?,
            entity_key: r.try_get("entity_key")?,
            at: lat.zip(lon),
            row_stops: r.try_get("row_stops")?,
            into_stop_id: r.try_get("into_stop_id")?,
            merged_routes: r.try_get("merged_routes")?,
        })
    })
    .collect()
}

// ---------------------------------------------------------------- problems

/// The reviewed stop as it is now.
#[derive(Debug, Clone, PartialEq)]
pub struct StopNow {
    pub lat: f64,
    pub lon: f64,
    pub location_type: i64,
    pub deleted: bool,
    /// The stop a committed merge moved this one's routes to.
    pub merged_into: Option<String>,
    pub row_version: i64,
}

impl StopNow {
    /// From the stop row in read shape ([`service::stop_row`]).
    pub fn from_row(stop: &Value) -> Option<StopNow> {
        let deleted = stop["deleted"].as_bool()?;
        Some(StopNow {
            lat: stop["lat"].as_f64()?,
            lon: stop["lon"].as_f64()?,
            location_type: stop["location_type"].as_i64()?,
            deleted,
            merged_into: stop["provenance"]["merged_into"]
                .as_str()
                .filter(|_| deleted)
                .map(str::to_string),
            row_version: stop["row_version"].as_i64()?,
        })
    }
}

/// What stands in the way of changing the review's stop now. Errors: the stop
/// no longer exists, is deleted, was merged into another stop (by a commit, or
/// by `draft`, which is taken as applying), or is a station. Warning: it is more
/// than [`MOVED_METRES`] from where the review was loaded - not reported once
/// the review's own change is committed.
pub fn problems(
    stop_id: &str,
    loaded: (f64, f64),
    committed: bool,
    stop: Option<&StopNow>,
    draft: Option<&DraftView>,
) -> Vec<Problem> {
    let refuse = |code: &'static str, message: String| vec![Problem::error(code, None, message)];
    let Some(s) = stop else {
        return refuse("stop_missing", format!("stop {stop_id} no longer exists"));
    };
    if let Some(into) = &s.merged_into {
        return refuse(
            "stop_merged_away",
            format!("stop {stop_id} was merged into {into}; its routes call at {into} now"),
        );
    }
    if s.deleted {
        return refuse("stop_deleted", format!("stop {stop_id} is deleted"));
    }
    if s.location_type == 1 {
        return refuse(
            "stop_is_station",
            format!("{stop_id} is a station, not a stop"),
        );
    }
    if let Some((into, by)) = draft.and_then(|d| d.merged_into(stop_id)) {
        return refuse(
            "stop_merged_away",
            format!("stop {stop_id} is merged into {into} by change {by} in the draft"),
        );
    }
    if draft.is_some_and(|d| d.stop_deleted(stop_id)) {
        return refuse(
            "stop_deleted",
            format!("stop {stop_id} is deleted in the draft"),
        );
    }
    let (lat, lon) = draft
        .and_then(|d| d.stop_position(stop_id))
        .unwrap_or((s.lat, s.lon));
    let moved = haversine_m(loaded.0, loaded.1, lat, lon);
    if !committed && moved > MOVED_METRES {
        return vec![Problem::warning(
            "moved_since_load",
            None,
            format!("stop {stop_id} is {moved:.0} m from where the review found it"),
        )];
    }
    vec![]
}

fn has_errors(found: &[Problem]) -> bool {
    found.iter().any(|p| p.level == Level::Error)
}

fn review_has_problems(found: Vec<Problem>) -> EditorError {
    EditorError::bad_request(
        "review_has_problems",
        "the stop cannot be changed now; see the problems",
    )
    .with_details(json!({"problems": found}))
}

// ---------------------------------------------------------------- routes and detours

/// A served stop just before or after the reviewed stop on a route.
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbour {
    pub stop_id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

/// One call of a route at the reviewed stop, with the served stops either side
/// (shaping markers, jump and hidden stops are skipped).
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub route_id: String,
    pub short_name: Option<String>,
    pub sequence: i32,
    pub stop_type: String,
    pub prev: Option<Neighbour>,
    pub next: Option<Neighbour>,
}

/// How much further a bus travels to call at `stop` on its way from `prev` to
/// `next`: d(prev, stop) + d(stop, next) - d(prev, next), in metres. A stop on
/// the straight way costs nothing.
pub fn detour_m(prev: (f64, f64), stop: (f64, f64), next: (f64, f64)) -> f64 {
    let d = |a: (f64, f64), b: (f64, f64)| haversine_m(a.0, a.1, b.0, b.1);
    (d(prev, stop) + d(stop, next) - d(prev, next)).max(0.0)
}

impl Call {
    /// This call's detour with stop `stop_id` at `at`; `None` at either end of
    /// the route. A neighbour that is the same stop moves with it.
    pub fn detour_at(&self, stop_id: &str, at: (f64, f64)) -> Option<f64> {
        let place = |n: &Neighbour| {
            if n.stop_id == stop_id {
                at
            } else {
                (n.lat, n.lon)
            }
        };
        Some(detour_m(
            place(self.prev.as_ref()?),
            at,
            place(self.next.as_ref()?),
        ))
    }
}

pub fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    })
}

/// The median detour over the calls with a stop on both sides - of the routes
/// in `only`, when given - with stop `stop_id` at `at`; `None` when no call has
/// one.
pub fn median_detour(
    calls: &[Call],
    only: Option<&[String]>,
    stop_id: &str,
    at: (f64, f64),
) -> Option<f64> {
    median(
        calls
            .iter()
            .filter(|c| only.is_none_or(|ids| ids.contains(&c.route_id)))
            .filter_map(|c| c.detour_at(stop_id, at))
            .collect(),
    )
}

/// Metres as responses give them: to a tenth.
fn metres(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Every call a live (not deleted) route makes at `stop_id`, with the served
/// stops either side. One statement; per call, key lookups only: the route, and
/// one index range scan and one stop per side. The laterals keep it so on a
/// feed the planner has no statistics for yet (a plain join there scanned the
/// whole feed's stops for every call: 309 ms against 8 ms at 319 calls).
pub async fn calls(conn: &mut PgConnection, g: &str, stop_id: &str) -> EditorResult<Vec<Call>> {
    let rows = sqlx::query(
        "SELECT t.route_id, r.short_name, t.sequence, t.stop_type, \
                p.stop_id AS prev_id, p.name AS prev_name, p.lat AS prev_lat, p.lon AS prev_lon, \
                n.stop_id AS next_id, n.name AS next_name, n.lat AS next_lat, n.lon AS next_lon \
         FROM gtfs_route_stop t \
         CROSS JOIN LATERAL ( \
             SELECT r.short_name FROM gtfs_route r \
             WHERE r.gtfs_id = t.gtfs_id AND r.route_id = t.route_id AND NOT r.deleted LIMIT 1) r \
         LEFT JOIN LATERAL ( \
             SELECT x.stop_id, s.name, s.lat, s.lon FROM gtfs_route_stop x \
             JOIN gtfs_stop s ON s.gtfs_id = x.gtfs_id AND s.stop_id = x.stop_id \
             WHERE x.gtfs_id = t.gtfs_id AND x.route_id = t.route_id AND x.sequence < t.sequence \
               AND x.stop_type NOT IN ('ROUTE CORRECTION', 'JUMP STOP', 'HIDDEN STOP') \
             ORDER BY x.sequence DESC LIMIT 1) p ON true \
         LEFT JOIN LATERAL ( \
             SELECT x.stop_id, s.name, s.lat, s.lon FROM gtfs_route_stop x \
             JOIN gtfs_stop s ON s.gtfs_id = x.gtfs_id AND s.stop_id = x.stop_id \
             WHERE x.gtfs_id = t.gtfs_id AND x.route_id = t.route_id AND x.sequence > t.sequence \
               AND x.stop_type NOT IN ('ROUTE CORRECTION', 'JUMP STOP', 'HIDDEN STOP') \
             ORDER BY x.sequence LIMIT 1) n ON true \
         WHERE t.gtfs_id = $1 AND t.stop_id = $2 \
         ORDER BY t.route_id, t.sequence",
    )
    .bind(g)
    .bind(stop_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| -> Result<Call, sqlx::Error> {
            let side = |p: &str| -> Result<Option<Neighbour>, sqlx::Error> {
                let col = |c: &str| format!("{p}_{c}");
                let id: Option<String> = r.try_get(col("id").as_str())?;
                let name: Option<String> = r.try_get(col("name").as_str())?;
                let lat: Option<f64> = r.try_get(col("lat").as_str())?;
                let lon: Option<f64> = r.try_get(col("lon").as_str())?;
                Ok(match (id, name, lat, lon) {
                    (Some(stop_id), Some(name), Some(lat), Some(lon)) => Some(Neighbour {
                        stop_id,
                        name,
                        lat,
                        lon,
                    }),
                    _ => None,
                })
            };
            Ok(Call {
                route_id: r.try_get("route_id")?,
                short_name: r.try_get("short_name")?,
                sequence: r.try_get("sequence")?,
                stop_type: r.try_get("stop_type")?,
                prev: side("prev")?,
                next: side("next")?,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn neighbour_json(n: Option<&Neighbour>) -> Value {
    match n {
        None => Value::Null,
        Some(n) => json!({"stop_id": n.stop_id, "name": n.name, "lat": n.lat, "lon": n.lon}),
    }
}

// ---------------------------------------------------------------- reads

/// What nandi's advisory tool suggests for a review, in
/// `evidence.auto_fix.action` (section 8.2); the list filters on it and the
/// summary counts it.
pub const AUTO_FIX_ACTIONS: [&str; 4] = ["merge", "move", "choose", "none"];

pub async fn list(
    state: &EditorState,
    gtfs_id: &str,
    query: &ListQuery,
    auto_fix: Option<&str>,
    page: &Page,
) -> EditorResult<Value> {
    let statuses = parse_status_list(query.status.as_deref(), &STATUSES)?;
    let auto_fix = auto_fix.map(str::trim).filter(|a| !a.is_empty());
    if auto_fix.is_some_and(|a| !AUTO_FIX_ACTIONS.contains(&a)) {
        return Err(EditorError::bad_request(
            "invalid_auto_fix",
            format!("auto_fix is one of {}", AUTO_FIX_ACTIONS.join(", ")),
        ));
    }
    let q = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (min_lat, min_lon, max_lat, max_lon) = match query.bbox {
        Some((a, b, c, d)) => (Some(a), Some(b), Some(c), Some(d)),
        None => (None, None, None, None),
    };
    let rows = sqlx::query(&format!(
        "{SELECT} \
         WHERE r.gtfs_id = $1 AND r.status = ANY($2) \
           AND ($3::float8 IS NULL OR (r.lat BETWEEN $3 AND $5 AND r.lon BETWEEN $4 AND $6)) \
           AND ($7::text IS NULL OR r.stop_id = $7 OR r.original_stop_id = $7 \
                OR r.stop_name ILIKE $8 OR r.stop_name % $7) \
           AND ($12::text IS NULL OR r.evidence->'auto_fix'->>'action' = $12) \
         ORDER BY (r.stop_id = $7 OR r.original_stop_id = $7) DESC NULLS LAST, \
                  similarity(r.stop_name, coalesce($7, '')) DESC, \
                  array_position($9::text[], r.status), r.review_id \
         LIMIT $10 OFFSET $11"
    ))
    .bind(gtfs_id)
    .bind(&statuses)
    .bind(min_lat)
    .bind(min_lon)
    .bind(max_lat)
    .bind(max_lon)
    .bind(q)
    .bind(q.map(service::like_pattern))
    .bind(&STATUSES[..])
    .bind(page.limit + 1)
    .bind(page.offset)
    .bind(auto_fix)
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| from_row(r).map(|x| x.json))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(page.wrap(items))
}

pub async fn summary(state: &EditorState, gtfs_id: &str) -> EditorResult<Value> {
    let rows = sqlx::query(
        "SELECT status, count(*) AS n FROM gtfs_position_review WHERE gtfs_id = $1 GROUP BY status",
    )
    .bind(gtfs_id)
    .fetch_all(&state.pool)
    .await?;
    let mut out = json!({"pending": 0, "approved": 0, "committed": 0, "confirmed": 0});
    for r in &rows {
        let status: String = r.try_get("status")?;
        if out.get(&status).is_some() {
            out[status] = json!(r.try_get::<i64, _>("n")?);
        }
    }
    // what the advisory tool suggests for the reviews still waiting
    let rows = sqlx::query(
        "SELECT evidence->'auto_fix'->>'action' AS action, count(*) AS n FROM gtfs_position_review \
         WHERE gtfs_id = $1 AND status = 'pending' AND evidence->'auto_fix'->>'action' IS NOT NULL \
         GROUP BY 1",
    )
    .bind(gtfs_id)
    .fetch_all(&state.pool)
    .await?;
    let mut auto_fix = json!({"merge": 0, "move": 0, "choose": 0, "none": 0});
    for r in &rows {
        let action: String = r.try_get("action")?;
        if auto_fix.get(&action).is_some() {
            auto_fix[action] = json!(r.try_get::<i64, _>("n")?);
        }
    }
    out["auto_fix"] = auto_fix;
    Ok(out)
}

/// What to ask the detour of before drafting anything.
#[derive(Debug, Clone, PartialEq)]
pub enum WhatIf {
    /// the stop at `at`, over the calls of `route_ids` when given, otherwise
    /// over every call: a move, or a split
    Point {
        at: (f64, f64),
        route_ids: Option<Vec<String>>,
    },
    /// the stop merged into `stop_id`: every call measured where that stop is,
    /// and what the `stop/merge` validation says - on top of draft
    /// `change_set`, when given
    MergeInto {
        stop_id: String,
        change_set: Option<Uuid>,
    },
}

/// The `after` of the merge of a reviewed stop into `into`, as `/merge` drafts
/// it and as the dry question asks about it.
fn merge_after(into: &str, into_row_version: Option<i64>, keep_name: &str, id: i64) -> Value {
    json!({
        "into_stop_id": into, "into_row_version": into_row_version, "keep_name": keep_name,
        "keep_position": "into", "position_review_id": id,
    })
}

/// Where stop `id` is once `draft` applies: where the draft puts it, else where
/// it is now; `None` when there is no such stop.
async fn stop_point(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
    draft: Option<&DraftView>,
) -> EditorResult<Option<(f64, f64)>> {
    if let Some(at) = draft.and_then(|d| d.stop_position(id)) {
        return Ok(Some(at));
    }
    Ok(service::stop_row(conn, g, id)
        .await?
        .and_then(|s| Some((s["lat"].as_f64()?, s["lon"].as_f64()?))))
}

/// The review with its stop as it is now, every call a route makes there with
/// its detour, the median detour, what stands in the way of changing it, and
/// its actions in its draft (`draft_actions`, in change order), each with the
/// detour it gives: a split over its own routes at its new stop's point, a move
/// over the routes the review's splits leave at the stop, a merge over every
/// call, at the point of the stop it merges into. The singular `new_position`,
/// `new_stop_id`, `split_route_ids`, `merge_into_stop_id` and `detour_m_after`
/// are the latest action's; `what_if`, when given, answers `detour_m_after`
/// instead - and, for a merge, `merge_problems`. `actor` is who asks.
pub async fn detail(
    conn: &mut PgConnection,
    id: i64,
    what_if: Option<&WhatIf>,
    actor: &str,
) -> EditorResult<Value> {
    let r = load(conn, id, false).await?;
    let stop = service::stop_row(conn, &r.gtfs_id, &r.stop_id).await?;
    let now = stop.as_ref().and_then(StopNow::from_row);
    let calls = calls(conn, &r.gtfs_id, &r.stop_id).await?;
    let changes = match r.change_set_id {
        Some(set) => review_changes(conn, set, r.review_id).await?,
        None => vec![],
    };
    let found = problems(
        &r.stop_id,
        (r.lat, r.lon),
        r.status == "committed",
        now.as_ref(),
        None,
    );
    let here = now.as_ref().map(|s| (s.lat, s.lon));
    let routes: Vec<Value> = calls
        .iter()
        .map(|c| {
            json!({
                "route_id": c.route_id,
                "short_name": c.short_name,
                "sequence": c.sequence,
                "stop_type": c.stop_type,
                "prev": neighbour_json(c.prev.as_ref()),
                "next": neighbour_json(c.next.as_ref()),
                "detour_m": here.and_then(|p| c.detour_at(&r.stop_id, p)).map(metres),
            })
        })
        .collect();
    let detour = here.and_then(|p| median_detour(&calls, None, &r.stop_id, p));

    let gone = split_off(&changes);
    let kept: Vec<Call> = calls
        .iter()
        .filter(|c| !gone.contains(&c.route_id))
        .cloned()
        .collect();
    let mut draft_actions = Vec::new();
    // (new_position, new_stop_id, split_route_ids, merge_into_stop_id, detour after)
    let mut latest = (Value::Null, Value::Null, Value::Null, Value::Null, None);
    for a in actions(&changes) {
        let mut entry =
            json!({"kind": "move", "change_id": a.change_id, "lat": a.at.0, "lon": a.at.1});
        let at = json!({"lat": a.at.0, "lon": a.at.1});
        let after = match &a.kind {
            ActionKind::Move => {
                latest = (at, Value::Null, Value::Null, Value::Null, None);
                median_detour(&kept, None, &r.stop_id, a.at)
            }
            ActionKind::Split {
                new_stop_id,
                route_ids,
            } => {
                entry["kind"] = json!("split");
                entry["new_stop_id"] = json!(new_stop_id);
                entry["route_ids"] = json!(route_ids);
                latest = (at, json!(new_stop_id), json!(route_ids), Value::Null, None);
                if r.status == "committed" {
                    // the split routes call at the new stop now
                    let there = self::calls(conn, &r.gtfs_id, new_stop_id).await?;
                    median_detour(&there, Some(route_ids.as_slice()), new_stop_id, a.at)
                } else {
                    median_detour(&calls, Some(route_ids.as_slice()), &r.stop_id, a.at)
                }
            }
            ActionKind::Merge {
                into_stop_id,
                route_ids,
            } => {
                entry["kind"] = json!("merge");
                entry["into_stop_id"] = json!(into_stop_id);
                latest = (at, Value::Null, Value::Null, json!(into_stop_id), None);
                if r.status == "committed" {
                    // the merged stop's routes call at the kept stop now
                    let there = self::calls(conn, &r.gtfs_id, into_stop_id).await?;
                    median_detour(&there, Some(route_ids.as_slice()), into_stop_id, a.at)
                } else {
                    median_detour(&calls, None, &r.stop_id, a.at)
                }
            }
        };
        entry["detour_m_after"] = json!(after.map(metres));
        latest.4 = after;
        draft_actions.push(entry);
    }
    let (new_position, new_stop_id, split_route_ids, merge_into_stop_id, latest_after) = latest;
    let mut merge_problems = Value::Null;
    let detour_after = match what_if {
        Some(WhatIf::Point { at, route_ids }) => {
            median_detour(&calls, route_ids.as_deref(), &r.stop_id, *at)
        }
        Some(WhatIf::MergeInto {
            stop_id: into,
            change_set,
        }) => {
            // asked of the real validator, in a transaction that is rolled back
            let mut tx = sqlx::Connection::begin(&mut *conn).await?;
            let (draft, drafted) = match change_set {
                Some(set) => {
                    let set = service::load_set(&mut tx, *set, false).await?;
                    if set.gtfs_id != r.gtfs_id {
                        return Err(feed_mismatch(&set.gtfs_id, &r.gtfs_id));
                    }
                    (
                        Some(DraftView::load(&mut tx, set.change_set_id).await?),
                        service::load_changes(&mut tx, set.change_set_id).await?,
                    )
                }
                None => (None, vec![]),
            };
            let at = stop_point(&mut tx, &r.gtfs_id, into, draft.as_ref()).await?;
            merge_problems = json!(
                service::findings_for(
                    &mut tx,
                    &r.gtfs_id,
                    &drafted,
                    ("stop", "merge", &r.stop_id),
                    &merge_after(into, None, "into", id),
                    actor,
                )
                .await?
            );
            tx.rollback().await?;
            at.and_then(|p| median_detour(&calls, None, &r.stop_id, p))
        }
        None => latest_after,
    };
    let mut out = r.json;
    out["stop"] = stop.unwrap_or(Value::Null);
    out["routes"] = json!(routes);
    out["detour_m"] = json!(detour.map(metres));
    out["new_position"] = new_position;
    out["new_stop_id"] = new_stop_id;
    out["split_route_ids"] = split_route_ids;
    out["merge_into_stop_id"] = merge_into_stop_id;
    out["detour_m_after"] = json!(detour_after.map(metres));
    out["draft_actions"] = json!(draft_actions);
    out["problems"] = json!(found);
    out["merge_problems"] = merge_problems;
    Ok(out)
}

// ---------------------------------------------------------------- move, confirm, reopen

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveBody {
    pub change_set_id: Uuid,
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub note: Option<String>,
}

fn note_text(note: Option<&str>) -> Option<String> {
    note.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
}

fn not_pending(r: &Review) -> EditorError {
    EditorError::conflict(
        "review_not_pending",
        format!("position review {} is {}", r.review_id, r.status),
    )
    .with_details(json!({"status": r.status, "change_set_id": r.json["change_set_id"]}))
}

fn feed_mismatch(draft: &str, review: &str) -> EditorError {
    EditorError::bad_request(
        "feed_mismatch",
        format!("the draft is for feed {draft} and the review for feed {review}"),
    )
}

fn invalid_change(f: Finding) -> EditorError {
    EditorError::bad_request("invalid_change", f.message.clone())
        .with_details(json!({"code": f.code}))
}

/// The draft a reviewer's action goes into, and the review, both locked: the
/// same feed, the draft still a draft, the review pending or approved in that
/// same draft. Also the changes the draft already holds for the review.
async fn open_for_change(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    id: i64,
    t: Transition,
) -> EditorResult<(service::ChangeSet, Review, Vec<ReviewChange>)> {
    let set = service::load_set(conn, change_set_id, true).await?;
    let r = load(conn, id, true).await?;
    if r.gtfs_id != set.gtfs_id {
        return Err(feed_mismatch(&set.gtfs_id, &r.gtfs_id));
    }
    service::editable(&set)?;
    next_status(&r.status, t).ok_or_else(|| not_pending(&r))?;
    may_add_action(&r.status, r.change_set_id, set.change_set_id).map_err(
        |refusal| match refusal {
            Refusal::InOtherDraft(other) => EditorError::conflict(
                "review_in_other_draft",
                format!(
                    "position review {id} already has changes in draft {other}; add to that \
                     draft, or take them out of it first"
                ),
            )
            .with_details(json!({"change_set_id": other})),
            Refusal::NotPending => not_pending(&r),
        },
    )?;
    let changes = review_changes(conn, set.change_set_id, id).await?;
    Ok((set, r, changes))
}

/// A review's merge in the draft takes its stop away: nothing else - a move, a
/// split - can be added to the review beside it.
fn merged_in_draft(r: &Review, changes: &[ReviewChange]) -> Result<(), EditorError> {
    let merges: Vec<i64> = changes
        .iter()
        .filter(|c| c.entity == "stop" && c.op == "merge")
        .map(|c| c.change_id)
        .collect();
    if merges.is_empty() {
        return Ok(());
    }
    Err(draft_conflict(
        merges,
        format!(
            "merge stop {} into another stop for this review; remove that change first",
            r.stop_id
        ),
    ))
}

fn draft_conflict(change_ids: Vec<i64>, what: String) -> EditorError {
    EditorError::conflict(
        "draft_conflict",
        format!(
            "change(s) {} in this draft already {what}",
            change_ids
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
    .with_details(json!({"change_ids": change_ids}))
}

/// Mark a review approved with an action in `change_set_id`. Its first action
/// makes `change_id` the review's change; a later one keeps the earlier change,
/// and the earlier note unless it brings its own.
async fn mark_approved(
    conn: &mut PgConnection,
    ctx: &Ctx,
    id: i64,
    change_set_id: Uuid,
    change_id: i64,
    note: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE gtfs_position_review SET status = 'approved', change_set_id = $2, \
            change_id = CASE WHEN status = 'approved' THEN change_id ELSE $3 END, \
            reviewed_by = $4, reviewed_at = now(), \
            review_note = CASE WHEN status = 'approved' THEN coalesce($5, review_note) ELSE $5 END \
         WHERE review_id = $1",
    )
    .bind(id)
    .bind(change_set_id)
    .bind(change_id)
    .bind(ctx.user.user_id)
    .bind(note)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE gtfs_change_set SET updated_at = now() WHERE change_set_id = $1")
        .bind(change_set_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Move the review's stop into a draft: one `stop/update` of its position,
/// based on the stop's current row version. The review is `approved` until that
/// draft is committed, or the move leaves it.
pub async fn move_stop(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    body: MoveBody,
) -> EditorResult<Value> {
    if !valid_lat_lon(body.lat, body.lon) || (body.lat, body.lon) == (0.0, 0.0) {
        return Err(EditorError::bad_request(
            "invalid_position",
            "lat and lon must be a valid position",
        ));
    }
    let note = note_text(body.note.as_deref());
    let mut tx = state.pool.begin().await?;
    let (set, r, changes) =
        open_for_change(&mut tx, body.change_set_id, id, Transition::Move).await?;
    merged_in_draft(&r, &changes)?;
    // one move per review per draft: a second point is an edit of the first
    let moves: Vec<i64> = changes
        .iter()
        .filter(|c| c.entity == "stop" && c.op == "update")
        .map(|c| c.change_id)
        .collect();
    if !moves.is_empty() {
        return Err(draft_conflict(
            moves,
            format!(
                "move stop {} for this review; edit that change instead",
                r.stop_id
            ),
        ));
    }
    let row = service::stop_row(&mut tx, &r.gtfs_id, &r.stop_id).await?;
    let now = row.as_ref().and_then(StopNow::from_row);
    let draft = DraftView::load(&mut tx, set.change_set_id).await?;
    let found = problems(
        &r.stop_id,
        (r.lat, r.lon),
        false,
        now.as_ref(),
        Some(&draft),
    );
    let (before, now) = match (row, now) {
        (Some(row), Some(now)) if !has_errors(&found) => (row, now),
        _ => return Err(review_has_problems(found)),
    };
    let moved_m = haversine_m(now.lat, now.lon, body.lat, body.lon);
    if moved_m < SAME_POINT_METRES {
        return Err(EditorError::bad_request(
            "position_unchanged",
            format!(
                "stop {} is already at that point; if its position is right, confirm the review",
                r.stop_id
            ),
        ));
    }
    let after = json!({"lat": body.lat, "lon": body.lon, "position_review_id": id});
    check_payload("stop", "update", &r.stop_id, &after).map_err(invalid_change)?;
    // the stop keeps the routes the review's splits in this draft leave it
    let gone = split_off(&changes);
    let route_calls: Vec<Call> = calls(&mut tx, &r.gtfs_id, &r.stop_id)
        .await?
        .into_iter()
        .filter(|c| !gone.contains(&c.route_id))
        .collect();
    let change_id = service::insert_changes(
        &mut tx,
        set.change_set_id,
        ctx.user.user_id,
        &[ChangeInsert {
            entity: "stop".into(),
            op: "update".into(),
            entity_key: r.stop_id.clone(),
            base_row_version: Some(now.row_version as i32),
            before,
            after,
        }],
    )
    .await?[0];
    mark_approved(
        &mut tx,
        ctx,
        id,
        set.change_set_id,
        change_id,
        note.as_deref(),
    )
    .await?;
    let detour = |p: (f64, f64)| median_detour(&route_calls, None, &r.stop_id, p).map(metres);
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "position_review_moved",
        Some(&r.gtfs_id),
        Some(set.change_set_id),
        json!({
            "review_id": id, "stop_id": r.stop_id, "change_id": change_id,
            "from": {"lat": now.lat, "lon": now.lon}, "to": {"lat": body.lat, "lon": body.lon},
            "moved_m": metres(moved_m), "detour_m": detour((now.lat, now.lon)),
            "detour_m_after": detour((body.lat, body.lon)), "note": note,
        }),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id, None, &ctx.user.email).await
}

/// The position is right: close the review without changing anything.
pub async fn confirm(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    note: Option<&str>,
) -> EditorResult<Value> {
    let note = note_text(note);
    let mut tx = state.pool.begin().await?;
    let r = load(&mut tx, id, true).await?;
    let status = next_status(&r.status, Transition::Confirm).ok_or_else(|| not_pending(&r))?;
    let position = service::stop_row(&mut tx, &r.gtfs_id, &r.stop_id)
        .await?
        .map(|s| json!({"lat": s["lat"], "lon": s["lon"]}));
    sqlx::query(
        "UPDATE gtfs_position_review SET status = $2, reviewed_by = $3, reviewed_at = now(), \
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
        "position_review_confirmed",
        Some(&r.gtfs_id),
        None,
        json!({"review_id": id, "stop_id": r.stop_id, "position": position, "note": note}),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id, None, &ctx.user.email).await
}

/// A confirmed review is pending again, its review fields cleared.
pub async fn reopen(state: &EditorState, ctx: &Ctx, id: i64) -> EditorResult<Value> {
    let mut tx = state.pool.begin().await?;
    let r = load(&mut tx, id, true).await?;
    let status = next_status(&r.status, Transition::Reopen).ok_or_else(|| {
        EditorError::conflict(
            "review_not_confirmed",
            format!(
                "position review {id} is {}; only a confirmed review is reopened",
                r.status
            ),
        )
    })?;
    let reopened = sqlx::query(
        "UPDATE gtfs_position_review SET status = $2, reviewed_by = NULL, reviewed_at = NULL, \
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
                format!("another open review already covers stop {}", r.stop_id),
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
        "position_review_reopened",
        Some(&r.gtfs_id),
        None,
        json!({"review_id": id, "stop_id": r.stop_id, "note": r.json["review_note"]}),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id, None, &ctx.user.email).await
}

// ---------------------------------------------------------------- split

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitBody {
    pub change_set_id: Uuid,
    pub route_ids: Vec<String>,
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// One reason a split cannot be made, as `invalid_split` lists them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SplitProblem {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
}

impl SplitProblem {
    fn new(code: &'static str, route_id: Option<&str>, message: String) -> SplitProblem {
        SplitProblem {
            code,
            message,
            route_id: route_id.map(str::to_string),
        }
    }
}

fn invalid_split(found: Vec<SplitProblem>) -> EditorError {
    EditorError::bad_request(
        "invalid_split",
        "the routes cannot be split off this way; see the problems",
    )
    .with_details(json!({"problems": found}))
}

/// What is wrong with a split request on its face: no routes, a route listed
/// twice (once per route), a point a new stop cannot have.
pub fn split_request_problems(route_ids: &[String], lat: f64, lon: f64) -> Vec<SplitProblem> {
    let mut out = Vec::new();
    if route_ids.is_empty() {
        out.push(SplitProblem::new(
            "route_ids_required",
            None,
            "name the routes to split off the stop".into(),
        ));
    }
    let mut seen = HashSet::new();
    let mut reported = HashSet::new();
    for id in route_ids {
        if !seen.insert(id.as_str()) && reported.insert(id.as_str()) {
            out.push(SplitProblem::new(
                "route_listed_twice",
                Some(id),
                format!("route {id} is listed more than once"),
            ));
        }
    }
    if !valid_lat_lon(lat, lon) || (lat, lon) == (0.0, 0.0) {
        out.push(SplitProblem::new(
            "invalid_position",
            None,
            format!("position {lat}, {lon} is out of range"),
        ));
    }
    out
}

/// What stands in the way of splitting `route_ids` off stop `stop_id`, which
/// the live routes `calling` call at: a route that does not call there (a
/// deleted route never does), and taking away every route the review's earlier
/// splits in the draft (`gone`) leave it - that is a move.
pub fn split_route_problems(
    stop_id: &str,
    route_ids: &[String],
    calling: &BTreeSet<&str>,
    gone: &[String],
) -> Vec<SplitProblem> {
    // a route this review already split off in this draft is not at the stop
    // any more either; in practice `split_conflicts` refuses re-splitting it
    // first (its route_stops change is already there), but this is the rule if
    // that check is ever bypassed
    let mut out: Vec<SplitProblem> = route_ids
        .iter()
        .filter(|id| !calling.contains(id.as_str()) || gone.iter().any(|g| g == *id))
        .map(|id| {
            SplitProblem::new(
                "route_not_at_stop",
                Some(id),
                format!("route {id} does not call at stop {stop_id}"),
            )
        })
        .collect();
    let left: Vec<&str> = calling
        .iter()
        .copied()
        .filter(|c| !gone.iter().any(|g| g == c))
        .collect();
    if !left.is_empty() && left.iter().all(|c| route_ids.iter().any(|r| r == c)) {
        out.push(SplitProblem::new(
            "would_empty_stop",
            None,
            format!("every route calling at stop {stop_id} would leave it; move the stop instead"),
        ));
    }
    out
}

/// A route's rows with every row at stop `from` pointed at `to`; nothing else
/// in any row changes.
pub fn split_rows(rows: &[RouteRow], from: &str, to: &str) -> Vec<RouteRow> {
    rows.iter()
        .map(|r| {
            let mut r = r.clone();
            if r.stop_id.as_deref() == Some(from) {
                r.stop_id = Some(to.to_string());
            }
            r
        })
        .collect()
}

/// Changes in the draft a split would collide with: a stop list change of one
/// of the routes - a route this review already split off included - or a
/// change to the reviewed stop (its update, delete or merge, or a merge into
/// it) that was not made for this review.
async fn split_conflicts(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    review_id: i64,
    stop_id: &str,
    route_ids: &[String],
) -> EditorResult<Vec<i64>> {
    Ok(sqlx::query(
        "SELECT change_id FROM gtfs_change WHERE change_set_id = $1 AND ( \
             (entity = 'route_stops' AND entity_key = ANY($2)) \
             OR (entity = 'stop' AND entity_key = $3 \
                 AND (after->>'position_review_id') IS DISTINCT FROM $4) \
             OR (entity = 'stop' AND op = 'merge' AND btrim(after->>'into_stop_id') = $3)) \
         ORDER BY position",
    )
    .bind(change_set_id)
    .bind(route_ids)
    .bind(stop_id)
    .bind(review_id.to_string())
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| r.try_get("change_id"))
    .collect::<Result<_, _>>()?)
}

/// Split some routes off the review's stop into a draft: a `stop/create` at the
/// new point (the id minted), then per route, in the order given, a
/// `route_stops/replace` of its current rows with every row at the stop pointed
/// at the new one. The review is `approved`, its change the create.
pub async fn split(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    body: SplitBody,
) -> EditorResult<Value> {
    let route_ids: Vec<String> = body
        .route_ids
        .iter()
        .map(|r| r.trim().to_string())
        .collect();
    let found = split_request_problems(&route_ids, body.lat, body.lon);
    if !found.is_empty() {
        return Err(invalid_split(found));
    }
    let name = note_text(body.name.as_deref());
    let note = note_text(body.note.as_deref());
    let mut tx = state.pool.begin().await?;
    let (set, r, changes) =
        open_for_change(&mut tx, body.change_set_id, id, Transition::Split).await?;
    merged_in_draft(&r, &changes)?;
    let conflicts = split_conflicts(&mut tx, set.change_set_id, id, &r.stop_id, &route_ids).await?;
    if !conflicts.is_empty() {
        return Err(draft_conflict(
            conflicts,
            format!("change stop {} or these routes' stop lists", r.stop_id),
        ));
    }
    let row = service::stop_row(&mut tx, &r.gtfs_id, &r.stop_id).await?;
    let now = row.as_ref().and_then(StopNow::from_row);
    let found = problems(&r.stop_id, (r.lat, r.lon), false, now.as_ref(), None);
    let stop = match row {
        Some(row) if now.is_some() && !has_errors(&found) => row,
        _ => return Err(review_has_problems(found)),
    };
    let route_calls = calls(&mut tx, &r.gtfs_id, &r.stop_id).await?;
    let calling: BTreeSet<&str> = route_calls.iter().map(|c| c.route_id.as_str()).collect();
    let found = split_route_problems(&r.stop_id, &route_ids, &calling, &split_off(&changes));
    if !found.is_empty() {
        return Err(invalid_split(found));
    }

    let new_stop_id = service::mint_stop_ids(&mut tx, &r.gtfs_id, 1)
        .await?
        .remove(0);
    let name = name.unwrap_or_else(|| stop["name"].as_str().unwrap_or(&r.stop_name).to_string());
    let create = json!({
        "stop_id": new_stop_id, "name": name, "lat": body.lat, "lon": body.lon,
        "position_review_id": id,
    });
    check_payload("stop", "create", &new_stop_id, &create).map_err(invalid_change)?;
    let mut changes = vec![ChangeInsert {
        entity: "stop".into(),
        op: "create".into(),
        entity_key: new_stop_id.clone(),
        base_row_version: None,
        before: Value::Null,
        after: create,
    }];
    let live_rows = service::load_routes_rows(&mut tx, &r.gtfs_id, &route_ids).await?;
    let mut read_rows = service::load_routes_read_rows(&mut tx, &r.gtfs_id, &route_ids).await?;
    for route_id in &route_ids {
        let live = live_rows.get(route_id).map(Vec::as_slice).unwrap_or(&[]);
        let after = json!({
            "base_rows_hash": service::rows_hash(live),
            "rows": split_rows(live, &r.stop_id, &new_stop_id),
            "position_review_id": id,
        });
        check_payload("route_stops", "replace", route_id, &after).map_err(invalid_change)?;
        changes.push(ChangeInsert {
            entity: "route_stops".into(),
            op: "replace".into(),
            entity_key: route_id.clone(),
            base_row_version: None,
            before: json!(read_rows.remove(route_id).unwrap_or_default()),
            after,
        });
    }
    let change_ids =
        service::insert_changes(&mut tx, set.change_set_id, ctx.user.user_id, &changes).await?;
    mark_approved(
        &mut tx,
        ctx,
        id,
        set.change_set_id,
        change_ids[0],
        note.as_deref(),
    )
    .await?;
    let detour_after = median_detour(
        &route_calls,
        Some(route_ids.as_slice()),
        &r.stop_id,
        (body.lat, body.lon),
    );
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "position_review_split",
        Some(&r.gtfs_id),
        Some(set.change_set_id),
        json!({
            "review_id": id, "stop_id": r.stop_id, "new_stop_id": new_stop_id,
            "route_ids": route_ids, "change_set_id": set.change_set_id,
            "change_id": change_ids[0], "route_change_ids": &change_ids[1..],
            "name": name, "lat": body.lat, "lon": body.lon,
            "detour_m_after": detour_after.map(metres), "note": note,
        }),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id, None, &ctx.user.email).await
}

// ---------------------------------------------------------------- merge

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeBody {
    pub change_set_id: Uuid,
    pub into_stop_id: String,
    #[serde(default)]
    pub keep_name: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Changes in the draft a merge of `stop_id` into `into` would collide with:
/// any change to the reviewed stop (a stop change keyed on it, or a merge into
/// it - the review's own move or split included, which `changes` lists), and a
/// change that moves, deletes or merges away the stop it would merge into. One
/// more merge into that same stop is no conflict: several duplicates are merged
/// into one stop in one draft.
async fn merge_conflicts(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    changes: &[ReviewChange],
    stop_id: &str,
    into: &str,
) -> EditorResult<Vec<i64>> {
    let mut ids: Vec<i64> = sqlx::query(
        "SELECT change_id FROM gtfs_change WHERE change_set_id = $1 AND entity = 'stop' AND ( \
             entity_key = $2 \
             OR (op = 'merge' AND btrim(after->>'into_stop_id') = $2) \
             OR (entity_key = $3 AND op IN ('update', 'delete', 'merge'))) \
         ORDER BY position",
    )
    .bind(change_set_id)
    .bind(stop_id)
    .bind(into)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| r.try_get("change_id"))
    .collect::<Result<_, _>>()?;
    ids.extend(changes.iter().map(|c| c.change_id));
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// Merge the review's stop into another stop, into a draft: one `stop/merge`,
/// added by [`service::add_change_to`] - the code path every merge is added by -
/// after [`service::findings_for`] has asked the merge's own validation what it
/// would say. An error there refuses the merge; its warnings come back in the
/// response. The review is `approved`, and it is the review's only action: a
/// merge takes the stop away, so nothing else can be done to it beside one.
pub async fn merge(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    body: MergeBody,
) -> EditorResult<Value> {
    let into = body.into_stop_id.trim().to_string();
    let keep_name = body.keep_name.as_deref().unwrap_or("into");
    if into.is_empty() || !["into", "from"].contains(&keep_name) {
        return Err(EditorError::bad_request(
            "invalid_merge",
            "into_stop_id is required, and keep_name is \"into\" or \"from\"",
        ));
    }
    let note = note_text(body.note.as_deref());
    let mut tx = state.pool.begin().await?;
    let (set, r, changes) =
        open_for_change(&mut tx, body.change_set_id, id, Transition::Merge).await?;
    let conflicts =
        merge_conflicts(&mut tx, set.change_set_id, &changes, &r.stop_id, &into).await?;
    if !conflicts.is_empty() {
        return Err(draft_conflict(
            conflicts,
            format!("change stop {} or stop {into}", r.stop_id),
        ));
    }
    let row = service::stop_row(&mut tx, &r.gtfs_id, &r.stop_id).await?;
    let now = row.as_ref().and_then(StopNow::from_row);
    let draft = DraftView::load(&mut tx, set.change_set_id).await?;
    let found = problems(
        &r.stop_id,
        (r.lat, r.lon),
        false,
        now.as_ref(),
        Some(&draft),
    );
    let now = match now {
        Some(now) if !has_errors(&found) => now,
        _ => return Err(review_has_problems(found)),
    };
    // what the merge's own validation says, before anything is stored
    let drafted = service::load_changes(&mut tx, set.change_set_id).await?;
    let mut after = merge_after(&into, None, keep_name, id);
    let findings = service::findings_for(
        &mut tx,
        &r.gtfs_id,
        &drafted,
        ("stop", "merge", &r.stop_id),
        &after,
        &ctx.user.email,
    )
    .await?;
    if findings.iter().any(|f| f["level"] == "error") {
        return Err(EditorError::bad_request(
            "review_has_problems",
            "the stop cannot be merged into that stop; see the problems",
        )
        .with_details(json!({"problems": findings})));
    }
    let into_at = stop_point(&mut tx, &r.gtfs_id, &into, Some(&draft)).await?;
    let route_calls = calls(&mut tx, &r.gtfs_id, &r.stop_id).await?;
    // the live version is filled in by the add, as for any merge
    if let Some(m) = after.as_object_mut() {
        m.remove("into_row_version");
    }
    let change_id = service::add_change_to(
        &mut tx,
        ctx,
        &set,
        service::NewChange {
            entity: "stop".into(),
            op: "merge".into(),
            entity_key: r.stop_id.clone(),
            after,
            base_row_version: Some(now.row_version as i32),
        },
    )
    .await?;
    mark_approved(
        &mut tx,
        ctx,
        id,
        set.change_set_id,
        change_id,
        note.as_deref(),
    )
    .await?;
    let detour = |p: (f64, f64)| median_detour(&route_calls, None, &r.stop_id, p).map(metres);
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "position_review_merged",
        Some(&r.gtfs_id),
        Some(set.change_set_id),
        json!({
            "review_id": id, "stop_id": r.stop_id, "into_stop_id": into,
            "change_id": change_id, "change_set_id": set.change_set_id,
            "detour_m": detour((now.lat, now.lon)),
            "detour_m_after": into_at.and_then(detour),
            "moved_m": into_at.map(|p| metres(haversine_m(now.lat, now.lon, p.0, p.1))),
            "note": note,
        }),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    let mut out = detail(&mut conn, id, None, &ctx.user.email).await?;
    out["warnings"] = json!(findings);
    Ok(out)
}

// ---------------------------------------------------------------- lifecycle

/// A change taken out of an open draft, as the review lifecycle needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemovedChange {
    pub change_id: i64,
    pub entity: String,
    pub op: String,
    pub entity_key: String,
    pub position_review_id: Option<i64>,
}

/// What pending again means for a review: no draft, no change, no reviewer.
const BACK_TO_PENDING: &str =
    "UPDATE gtfs_position_review SET status = 'pending', change_set_id = NULL, change_id = NULL, \
        reviewed_by = NULL, reviewed_at = NULL, review_note = NULL";

/// A change left an open draft. A split's new stop takes that split's stop
/// lists (the ones calling at it) with it. A review whose draft then holds none
/// of its changes is pending again; otherwise it stays approved, known by its
/// earliest remaining action.
pub async fn change_removed(
    conn: &mut PgConnection,
    ctx: &Ctx,
    gtfs_id: &str,
    change_set_id: Uuid,
    removed: &RemovedChange,
) -> Result<(), sqlx::Error> {
    let reviews: Vec<(i64, String, Option<i64>)> = sqlx::query(
        "SELECT review_id, stop_id, change_id FROM gtfs_position_review \
         WHERE change_set_id = $1 AND status = 'approved' AND (change_id = $2 OR review_id = $3) \
         ORDER BY review_id FOR UPDATE",
    )
    .bind(change_set_id)
    .bind(removed.change_id)
    .bind(removed.position_review_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(i64, String, Option<i64>), sqlx::Error> {
        Ok((
            r.try_get("review_id")?,
            r.try_get("stop_id")?,
            r.try_get("change_id")?,
        ))
    })
    .collect::<Result<_, _>>()?;
    for (review_id, stop_id, change_id) in reviews {
        let mut cascaded: Vec<i64> = Vec::new();
        if removed.position_review_id == Some(review_id)
            && removed.entity == "stop"
            && removed.op == "create"
        {
            cascaded = sqlx::query(
                "DELETE FROM gtfs_change c WHERE c.change_set_id = $1 AND c.entity = 'route_stops' \
                   AND c.after->>'position_review_id' = $2 \
                   AND EXISTS (SELECT 1 FROM jsonb_array_elements(c.after->'rows') x \
                               WHERE x->>'stop_id' = $3) \
                 RETURNING c.change_id",
            )
            .bind(change_set_id)
            .bind(review_id.to_string())
            .bind(&removed.entity_key)
            .fetch_all(&mut *conn)
            .await?
            .iter()
            .map(|r| r.try_get("change_id"))
            .collect::<Result<_, _>>()?;
            cascaded.sort_unstable();
            let details: Vec<Value> = cascaded
                .iter()
                .map(|cid| {
                    json!({"change_id": cid, "review_id": review_id,
                           "new_stop_id": removed.entity_key, "reason": "split_removed"})
                })
                .collect();
            auth::audit_many(
                &mut *conn,
                Some(ctx.user.user_id),
                Some(&ctx.user.email),
                "change_removed",
                Some(gtfs_id),
                Some(change_set_id),
                &details,
            )
            .await?;
        }
        let left = review_changes(conn, change_set_id, review_id).await?;
        match first_change(&left) {
            None => {
                sqlx::query(&format!("{BACK_TO_PENDING} WHERE review_id = $1"))
                    .bind(review_id)
                    .execute(&mut *conn)
                    .await?;
                let mut detail =
                    json!({"review_id": review_id, "stop_id": stop_id, "reason": "change_removed"});
                if !cascaded.is_empty() {
                    detail["removed_change_ids"] = json!(cascaded);
                }
                auth::audit(
                    &mut *conn,
                    Some(ctx.user.user_id),
                    Some(&ctx.user.email),
                    "position_review_returned",
                    Some(gtfs_id),
                    Some(change_set_id),
                    detail,
                )
                .await?;
            }
            Some(first) if Some(first) != change_id => {
                sqlx::query("UPDATE gtfs_position_review SET change_id = $2 WHERE review_id = $1")
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

/// A discarded draft returns every review approved in it to pending. Its
/// changes stay as they were: a discarded draft never applies.
pub async fn draft_discarded(
    conn: &mut PgConnection,
    ctx: &Ctx,
    gtfs_id: &str,
    change_set_id: Uuid,
) -> Result<(), sqlx::Error> {
    let details = sqlx::query(&format!(
        "{BACK_TO_PENDING} WHERE change_set_id = $1 AND status = 'approved' RETURNING review_id, stop_id"
    ))
    .bind(change_set_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        Ok(json!({
            "review_id": r.try_get::<i64, _>("review_id")?,
            "stop_id": r.try_get::<String, _>("stop_id")?,
            "reason": "change_set_discarded",
        }))
    })
    .collect::<Result<Vec<_>, _>>()?;
    auth::audit_many(
        &mut *conn,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "position_review_returned",
        Some(gtfs_id),
        Some(change_set_id),
        &details,
    )
    .await
}

/// The draft carrying these reviews' changes is committed.
pub async fn mark_committed(
    conn: &mut PgConnection,
    ctx: &Ctx,
    gtfs_id: &str,
    change_set_id: Uuid,
    feed_version: i64,
) -> Result<(), sqlx::Error> {
    let committed = sqlx::query(
        "UPDATE gtfs_position_review SET status = 'committed' \
         WHERE change_set_id = $1 AND status = 'approved' RETURNING review_id, stop_id, change_id",
    )
    .bind(change_set_id)
    .fetch_all(&mut *conn)
    .await?;
    let details = committed
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            Ok(json!({
                "review_id": r.try_get::<i64, _>("review_id")?,
                "stop_id": r.try_get::<String, _>("stop_id")?,
                "change_id": r.try_get::<Option<i64>, _>("change_id")?,
                "feed_version": feed_version,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    auth::audit_many(
        &mut *conn,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "position_review_committed",
        Some(gtfs_id),
        Some(change_set_id),
        &details,
    )
    .await
}

/// A draft edit of a change keeps the review it was made for: `old` is the
/// stored `after`, `new` the edit's. `position_review_id` is put back when the
/// edit leaves it out; naming another review is refused (the message says why).
pub fn keep_review_link(old: &Value, new: &mut Value) -> Result<(), String> {
    let Some(review_id) = old.get("position_review_id").and_then(Value::as_i64) else {
        return Ok(());
    };
    let Some(m) = new.as_object_mut() else {
        return Ok(());
    };
    match m.get("position_review_id") {
        None | Some(Value::Null) => {
            m.insert("position_review_id".into(), json!(review_id));
            Ok(())
        }
        Some(v) if v.as_i64() == Some(review_id) => Ok(()),
        Some(_) => Err(format!(
            "this change was made for position review {review_id}; remove it from the draft \
             to take it back"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::draft::DraftChange;

    fn stop(lat: f64, lon: f64) -> StopNow {
        StopNow {
            lat,
            lon,
            location_type: 0,
            deleted: false,
            merged_into: None,
            row_version: 3,
        }
    }

    fn codes(p: &[Problem]) -> Vec<(&str, Level)> {
        p.iter().map(|x| (x.code, x.level)).collect()
    }

    fn at(id: &str, lat: f64, lon: f64) -> Neighbour {
        Neighbour {
            stop_id: id.into(),
            name: id.into(),
            lat,
            lon,
        }
    }

    fn call(route: &str, prev: Option<Neighbour>, next: Option<Neighbour>) -> Call {
        Call {
            route_id: route.into(),
            short_name: Some(route.into()),
            sequence: 2,
            stop_type: "NEW STOP".into(),
            prev,
            next,
        }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn statuses_follow_the_lifecycle() {
        use Transition::*;
        let all = [Move, Split, Merge, Confirm, Reopen, Return, Commit];
        let allowed: Vec<(&str, Transition, &str)> = STATUSES
            .iter()
            .flat_map(|s| all.iter().map(move |t| (*s, *t)))
            .filter_map(|(s, t)| next_status(s, t).map(|n| (s, t, n)))
            .collect();
        assert_eq!(
            allowed,
            vec![
                ("pending", Move, "approved"),
                ("pending", Split, "approved"),
                ("pending", Merge, "approved"),
                ("pending", Confirm, "confirmed"),
                ("approved", Move, "approved"),
                ("approved", Split, "approved"),
                ("approved", Merge, "approved"),
                ("approved", Return, "pending"),
                ("approved", Commit, "committed"),
                ("confirmed", Reopen, "pending"),
            ]
        );
        // closed reviews stay closed; a superseded one is never acted on
        for t in all {
            assert_eq!(next_status("committed", t), None, "{t:?}");
            assert_eq!(next_status("superseded", t), None, "{t:?}");
        }
    }

    #[test]
    fn a_merge_is_an_action_and_the_change_a_review_is_known_by() {
        let change = |id: i64, entity: &str, op: &str, key: &str| ReviewChange {
            change_id: id,
            entity: entity.into(),
            op: op.into(),
            entity_key: key.into(),
            at: None,
            row_stops: vec![],
            into_stop_id: None,
            merged_routes: vec![],
        };
        let merge = ReviewChange {
            at: Some((13.0, 80.2)),
            into_stop_id: Some("C".into()),
            merged_routes: ids(&["R1", "R2"]),
            ..change(4, "stop", "merge", "S")
        };
        assert_eq!(
            actions(std::slice::from_ref(&merge)),
            vec![Action {
                change_id: 4,
                at: (13.0, 80.2),
                kind: ActionKind::Merge {
                    into_stop_id: "C".into(),
                    route_ids: ids(&["R1", "R2"]),
                },
            }]
        );
        assert_eq!(first_change(std::slice::from_ref(&merge)), Some(4));
        // a merge into a stop that is nowhere has no point to show, and is no action
        let nowhere = ReviewChange { at: None, ..merge };
        assert!(actions(&[nowhere]).is_empty());
        assert!(split_off(&[change(5, "stop", "merge", "S")]).is_empty());
    }

    #[test]
    fn detour_is_the_extra_way_to_the_stop() {
        // A and B 222 m apart north-south; the stop halfway is on the way
        let (a, b) = ((13.0, 80.2), (13.002, 80.2));
        assert!(detour_m(a, (13.001, 80.2), b) < 0.01);
        // 542 m east of the middle: out and back, less the straight way (884 m)
        let east = (13.001, 80.205);
        let off = detour_m(a, east, b);
        let d = |p: (f64, f64), q: (f64, f64)| haversine_m(p.0, p.1, q.0, q.1);
        let expected = d(a, east) + d(east, b) - d(a, b);
        assert!(
            (off - expected).abs() < 1e-6 && (883.0..884.0).contains(&off),
            "{off}"
        );
        // never negative, even where rounding would make it so
        assert_eq!(detour_m(a, a, a), 0.0);
        // a call at either end of its route has no detour
        let middle = (13.001, 80.205);
        assert_eq!(
            call("R", None, Some(at("B", 13.002, 80.2))).detour_at("S", middle),
            None
        );
        assert_eq!(
            call("R", Some(at("A", 13.0, 80.2)), None).detour_at("S", middle),
            None
        );
        // a neighbour that is the stop itself moves with it
        let back_to_back = call("R", Some(at("S", 13.0, 80.2)), Some(at("B", 13.002, 80.2)));
        assert!(back_to_back.detour_at("S", (13.001, 80.2)).unwrap() < 0.01);
    }

    #[test]
    fn median_over_the_calls_with_both_neighbours() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![5.0]), Some(5.0));
        assert_eq!(median(vec![9.0, 1.0, 5.0]), Some(5.0));
        assert_eq!(median(vec![10.0, 1.0, 3.0, 5.0]), Some(4.0));
        let (a, b) = (at("A", 13.0, 80.2), at("B", 13.002, 80.2));
        // R2 comes the other way along a road 542 m east
        let (c, d) = (at("C", 13.0, 80.205), at("D", 13.002, 80.205));
        let calls = vec![
            call("R1", Some(a.clone()), Some(b.clone())),
            call("R1", Some(a.clone()), Some(b.clone())),
            call("R2", Some(d), Some(c)),
            call("R3", None, Some(b.clone())),
        ];
        let west = (13.001, 80.2);
        // two of the three measurable calls pass the west road
        assert!(median_detour(&calls, None, "S", west).unwrap() < 0.01);
        // only R2's: 884 m out of its way at the west road, nothing on its own
        let r2 = ids(&["R2"]);
        let r2_west = median_detour(&calls, Some(r2.as_slice()), "S", west).unwrap();
        assert!((883.0..884.0).contains(&r2_west), "{r2_west}");
        assert!(median_detour(&calls, Some(r2.as_slice()), "S", (13.001, 80.205)).unwrap() < 0.01);
        // an even count takes the middle two
        let even = median_detour(&calls[1..3], None, "S", west).unwrap();
        assert!((441.0..442.5).contains(&even), "{even}");
        assert_eq!(median_detour(&calls[3..], None, "S", west), None);
        assert_eq!(metres(884.84), 884.8);
    }

    #[test]
    fn problems_against_live_data() {
        let loaded = (13.0, 80.2);
        assert!(problems("S", loaded, false, Some(&stop(13.0, 80.2)), None).is_empty());
        assert_eq!(
            codes(&problems("S", loaded, false, None, None)),
            vec![("stop_missing", Level::Error)]
        );
        let merged = StopNow {
            deleted: true,
            merged_into: Some("K".into()),
            ..stop(13.0, 80.2)
        };
        let found = problems("S", loaded, false, Some(&merged), None);
        assert_eq!(codes(&found), vec![("stop_merged_away", Level::Error)]);
        assert_eq!(
            found[0].message,
            "stop S was merged into K; its routes call at K now"
        );
        let deleted = StopNow {
            deleted: true,
            ..stop(13.0, 80.2)
        };
        assert_eq!(
            codes(&problems("S", loaded, false, Some(&deleted), None)),
            vec![("stop_deleted", Level::Error)]
        );
        let station = StopNow {
            location_type: 1,
            ..stop(13.0, 80.2)
        };
        assert_eq!(
            codes(&problems("S", loaded, false, Some(&station), None)),
            vec![("stop_is_station", Level::Error)]
        );
        // 33 m north of where it was loaded: a warning, not once committed
        let moved = stop(13.0003, 80.2);
        let found = problems("S", loaded, false, Some(&moved), None);
        assert_eq!(codes(&found), vec![("moved_since_load", Level::Warning)]);
        assert_eq!(
            found[0].message,
            "stop S is 33 m from where the review found it"
        );
        assert!(problems("S", loaded, true, Some(&moved), None).is_empty());
        // 22 m is within the evidence's tolerance
        assert!(problems("S", loaded, false, Some(&stop(13.0002, 80.2)), None).is_empty());
    }

    #[test]
    fn problems_see_the_draft() {
        let ch = |id, op: &str, key: &str, after: Value| DraftChange {
            change_id: id,
            entity: "stop".into(),
            op: op.into(),
            entity_key: key.into(),
            after,
        };
        let draft = DraftView::from_changes(&[
            ch(4, "merge", "M", json!({"into_stop_id": "K"})),
            ch(5, "delete", "D", Value::Null),
            ch(6, "update", "U", json!({"lat": 13.001, "lon": 80.2})),
        ]);
        let live = stop(13.0, 80.2);
        let with_draft = |id| problems(id, (13.0, 80.2), false, Some(&live), Some(&draft));
        let found = with_draft("M");
        assert_eq!(codes(&found), vec![("stop_merged_away", Level::Error)]);
        assert_eq!(
            found[0].message,
            "stop M is merged into K by change 4 in the draft"
        );
        assert_eq!(
            codes(&with_draft("D")),
            vec![("stop_deleted", Level::Error)]
        );
        // the draft already moves U 111 m from where the review found it
        assert_eq!(
            codes(&with_draft("U")),
            vec![("moved_since_load", Level::Warning)]
        );
        assert!(with_draft("S").is_empty());
    }

    #[test]
    fn a_stop_row_reads_as_now() {
        let row = json!({"stop_id": "S", "lat": 13.0, "lon": 80.2, "location_type": 0, "deleted": true,
                         "provenance": {"merged_into": "K"}, "row_version": 4});
        let now = StopNow::from_row(&row).unwrap();
        assert_eq!(
            (now.merged_into.as_deref(), now.row_version, now.deleted),
            (Some("K"), 4, true)
        );
        // provenance naming a merge counts only on a stop the merge deleted
        let mut live = row.clone();
        live["deleted"] = json!(false);
        assert_eq!(StopNow::from_row(&live).unwrap().merged_into, None);
        let mut no_provenance = row;
        no_provenance["provenance"] = Value::Null;
        assert_eq!(StopNow::from_row(&no_provenance).unwrap().merged_into, None);
    }

    #[test]
    fn split_requests_and_routes() {
        let code = |p: &[SplitProblem]| -> Vec<(&str, Option<String>)> {
            p.iter().map(|x| (x.code, x.route_id.clone())).collect()
        };
        assert!(split_request_problems(&ids(&["R1"]), 13.0, 80.2).is_empty());
        assert_eq!(
            code(&split_request_problems(&[], 95.0, 80.2)),
            vec![("route_ids_required", None), ("invalid_position", None)]
        );
        assert_eq!(
            code(&split_request_problems(
                &ids(&["R1", "R2", "R1", "R1"]),
                0.0,
                0.0
            )),
            vec![
                ("route_listed_twice", Some("R1".into())),
                ("invalid_position", None)
            ]
        );
        let calling: BTreeSet<&str> = ["R1", "R2", "R3"].into_iter().collect();
        let none: Vec<String> = vec![];
        assert!(split_route_problems("S", &ids(&["R1", "R3"]), &calling, &none).is_empty());
        assert_eq!(
            code(&split_route_problems(
                "S",
                &ids(&["R1", "R9"]),
                &calling,
                &none
            )),
            vec![("route_not_at_stop", Some("R9".into()))]
        );
        let every = split_route_problems("S", &ids(&["R3", "R2", "R1"]), &calling, &none);
        assert_eq!(code(&every), vec![("would_empty_stop", None)]);
        assert_eq!(
            every[0].message,
            "every route calling at stop S would leave it; move the stop instead"
        );
        // a stop no route calls at has nothing to empty; every route named is wrong
        assert_eq!(
            code(&split_route_problems(
                "S",
                &ids(&["R1"]),
                &BTreeSet::new(),
                &none
            )),
            vec![("route_not_at_stop", Some("R1".into()))]
        );
        // a route this review already split off in the draft is gone: splitting
        // the last remaining route would empty the stop too
        let gone = ids(&["R1", "R2"]);
        let found = split_route_problems("S", &ids(&["R3"]), &calling, &gone);
        assert_eq!(code(&found), vec![("would_empty_stop", None)]);
        // asking to split an already-gone route again is refused (in practice
        // split_conflicts catches this first, with its own change already there)
        assert_eq!(
            code(&split_route_problems("S", &ids(&["R2"]), &calling, &gone)),
            vec![("route_not_at_stop", Some("R2".into()))]
        );
    }

    #[test]
    fn split_rows_point_every_call_at_the_new_stop() {
        let row = |stop: Option<&str>, t: &str| RouteRow {
            stop_id: stop.map(str::to_string),
            stop_type: t.into(),
            stage_no: 1,
            stage_name: "X".into(),
            marker_id: stop.is_none().then(|| "m1".to_string()),
            marker_name: None,
            marker_lat: stop.is_none().then_some(13.0),
            marker_lon: stop.is_none().then_some(80.2),
            stop_name_override: stop.map(|s| format!("{s} ON THIS ROUTE")),
            provider_id: Some("7".into()),
        };
        let live = vec![
            row(Some("S"), "NEW STOP"),
            row(Some("A"), "INTERMEDIATE STOP"),
            row(None, "ROUTE CORRECTION"),
            row(Some("S"), "JUMP STOP"),
        ];
        let split = split_rows(&live, "S", "ed_new");
        let stops: Vec<Option<&str>> = split.iter().map(|r| r.stop_id.as_deref()).collect();
        assert_eq!(stops, vec![Some("ed_new"), Some("A"), None, Some("ed_new")]);
        // everything else is the route's own
        for (a, b) in live.iter().zip(&split) {
            assert_eq!(
                (
                    &a.stop_type,
                    &a.stop_name_override,
                    &a.marker_id,
                    &a.provider_id
                ),
                (
                    &b.stop_type,
                    &b.stop_name_override,
                    &b.marker_id,
                    &b.provider_id
                )
            );
        }
    }

    #[test]
    fn draft_edits_keep_the_review() {
        let old = json!({"lat": 13.0, "lon": 80.2, "position_review_id": 7});
        let mut left_out = json!({"lat": 13.1, "lon": 80.3});
        keep_review_link(&old, &mut left_out).unwrap();
        assert_eq!(left_out["position_review_id"], 7);
        let mut null = json!({"lat": 13.1, "lon": 80.3, "position_review_id": null});
        keep_review_link(&old, &mut null).unwrap();
        assert_eq!(null["position_review_id"], 7);
        let mut same = json!({"lat": 13.1, "lon": 80.3, "position_review_id": 7});
        assert!(keep_review_link(&old, &mut same).is_ok());
        let mut other = json!({"lat": 13.1, "lon": 80.3, "position_review_id": 8});
        assert!(keep_review_link(&old, &mut other).is_err());
        // a change made without a review is edited as it always was
        let mut plain = json!({"name": "X"});
        keep_review_link(&json!({"name": "Y"}), &mut plain).unwrap();
        assert_eq!(plain, json!({"name": "X"}));
    }
}
