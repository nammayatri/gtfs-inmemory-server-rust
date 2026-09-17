//! Station proposals (docs/gtfs-editor.md section 6): stations nandi's
//! `build_stations.py` suggests, queued in `gtfs_station_proposal`. A person
//! reviews each one here. Approving adds a `station/create` change to a draft;
//! only committing that draft, through the normal maker-checker flow, creates
//! the station.
//!
//! The server keeps the lifecycle: removing the change from its draft, or
//! discarding the draft, puts the proposal back to pending; committing the draft
//! marks it committed. Every transition is audited.

use super::auth::{self, Ctx};
use super::draft::DraftView;
use super::error::{EditorError, EditorResult};
use super::service::{self, ChangeInsert, Page};
use super::validation::{
    check_payload, haversine_m, too_few_members, valid_lat_lon, Level, PLATFORM_CODE_MAX_CHARS,
};
use super::EditorState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub const STATUSES: [&str; 5] = ["pending", "approved", "rejected", "committed", "superseded"];
/// A member that moved further than this since the proposal was built is
/// reported (a warning: it does not make the station invalid).
pub const MOVED_METRES: f64 = 100.0;
/// Most proposals one bulk approval takes.
pub const MAX_BULK: usize = 5000;

const SELECT: &str =
    "SELECT p.proposal_id, p.gtfs_id, p.batch, p.station_id, p.name, p.lat, p.lon, \
        p.members::text AS members, p.spread_m, p.status, p.change_set_id, p.change_id, \
        cs.title AS change_set_title, u.email AS reviewed_by_email, p.reviewed_at, p.review_note, \
        p.created_at, p.updated_at \
     FROM gtfs_station_proposal p \
     LEFT JOIN gtfs_change_set cs ON cs.change_set_id = p.change_set_id \
     LEFT JOIN gtfs_editor_user u ON u.user_id = p.reviewed_by";

pub struct Proposal {
    pub proposal_id: i64,
    pub gtfs_id: String,
    pub station_id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub status: String,
    /// As recorded by the build: `[{stop_id, name, lat, lon, platform_code, route_count}]`
    pub members: Vec<Value>,
    pub json: Value,
}

fn from_row(r: &PgRow) -> Result<Proposal, sqlx::Error> {
    type Ts = Option<chrono::DateTime<chrono::Utc>>;
    let members: Vec<Value> = r
        .try_get::<String, _>("members")
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let p = Proposal {
        proposal_id: r.try_get("proposal_id")?,
        gtfs_id: r.try_get("gtfs_id")?,
        station_id: r.try_get("station_id")?,
        name: r.try_get("name")?,
        lat: r.try_get("lat")?,
        lon: r.try_get("lon")?,
        status: r.try_get("status")?,
        json: Value::Null,
        members,
    };
    let json = json!({
        "proposal_id": p.proposal_id,
        "gtfs_id": p.gtfs_id,
        "batch": r.try_get::<String, _>("batch")?,
        "station_id": p.station_id,
        "name": p.name,
        "lat": p.lat,
        "lon": p.lon,
        "spread_m": r.try_get::<Option<i32>, _>("spread_m")?,
        "members": p.members,
        "status": p.status,
        "change_set_id": r.try_get::<Option<Uuid>, _>("change_set_id")?,
        "change_id": r.try_get::<Option<i64>, _>("change_id")?,
        "change_set_title": r.try_get::<Option<String>, _>("change_set_title")?,
        "reviewed_by_email": r.try_get::<Option<String>, _>("reviewed_by_email")?,
        "reviewed_at": r.try_get::<Ts, _>("reviewed_at")?,
        "review_note": r.try_get::<Option<String>, _>("review_note")?,
        "created_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?,
        "updated_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?,
    });
    Ok(Proposal { json, ..p })
}

fn member_id(m: &Value) -> &str {
    m["stop_id"].as_str().unwrap_or("")
}

fn member_position(m: &Value) -> Option<(f64, f64)> {
    Some((m["lat"].as_f64()?, m["lon"].as_f64()?))
}

async fn load(conn: &mut PgConnection, id: i64, lock: bool) -> EditorResult<Proposal> {
    let sql = format!(
        "{SELECT} WHERE p.proposal_id = $1{}",
        if lock { " FOR UPDATE OF p" } else { "" }
    );
    let row = sqlx::query(&sql)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| EditorError::not_found("proposal_not_found", format!("no proposal {id}")))?;
    Ok(from_row(&row)?)
}

// ---------------------------------------------------------------- problems

/// A stop as it is now.
#[derive(Debug, Clone)]
pub struct StopNow {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub location_type: i16,
    pub parent_station: Option<String>,
    pub platform_code: Option<String>,
    pub deleted: bool,
    pub row_version: i32,
    pub route_count: Option<i64>,
}

async fn stops_now(
    conn: &mut PgConnection,
    g: &str,
    ids: &[String],
    with_routes: bool,
) -> EditorResult<HashMap<String, StopNow>> {
    let rows = sqlx::query(&format!(
        "SELECT s.stop_id, s.name, s.lat, s.lon, s.location_type, s.parent_station, s.platform_code, \
                s.deleted, s.row_version, {} AS route_count \
         FROM gtfs_stop s WHERE s.gtfs_id = $1 AND s.stop_id = ANY($2)",
        if with_routes {
            "(SELECT count(DISTINCT rs.route_id) FROM gtfs_route_stop rs \
              WHERE rs.gtfs_id = s.gtfs_id AND rs.stop_id = s.stop_id)"
        } else {
            "NULL::int8"
        }
    ))
    .bind(g)
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| -> Result<(String, StopNow), sqlx::Error> {
            Ok((
                r.try_get("stop_id")?,
                StopNow {
                    name: r.try_get("name")?,
                    lat: r.try_get("lat")?,
                    lon: r.try_get("lon")?,
                    location_type: r.try_get("location_type")?,
                    parent_station: r.try_get("parent_station")?,
                    platform_code: r.try_get("platform_code")?,
                    deleted: r.try_get("deleted")?,
                    row_version: r.try_get("row_version")?,
                    route_count: r.try_get("route_count")?,
                },
            ))
        })
        .collect::<Result<_, _>>()
        .map_err(Into::into)
}

/// Which of `ids` are already a row of the feed's stop table.
async fn ids_taken(
    conn: &mut PgConnection,
    g: &str,
    ids: &[String],
) -> EditorResult<HashSet<String>> {
    Ok(
        sqlx::query("SELECT stop_id FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = ANY($2)")
            .bind(g)
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?
            .iter()
            .map(|r| r.try_get("stop_id"))
            .collect::<Result<_, _>>()?,
    )
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Problem {
    pub level: Level,
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_id: Option<String>,
    pub message: String,
}

impl Problem {
    pub fn error(code: &'static str, stop_id: Option<&str>, message: String) -> Problem {
        Problem {
            level: Level::Error,
            code,
            stop_id: stop_id.map(str::to_string),
            message,
        }
    }

    pub fn warning(code: &'static str, stop_id: Option<&str>, message: String) -> Problem {
        Problem {
            level: Level::Warning,
            ..Problem::error(code, stop_id, message)
        }
    }
}

/// A member to check: its id and the position the build recorded for it.
pub struct MemberCheck<'a> {
    pub stop_id: &'a str,
    pub recorded: Option<(f64, f64)>,
}

/// What stands in the way of creating station `station_id` with `members` now:
/// errors make the station invalid (a member is gone, deleted, a station, or in
/// another station; the id is taken; nothing or only one stop is left), a
/// warning flags evidence that went stale (a member moved). `live` holds the
/// members' live rows (and, with a draft, the stops its merges take a station
/// from); `draft` applies the draft the station would join on top of them.
pub fn problems(
    station_id: &str,
    committed: bool,
    station_taken: bool,
    members: &[MemberCheck],
    live: &HashMap<String, StopNow>,
    draft: Option<&DraftView>,
) -> Vec<Problem> {
    let mut out = Vec::new();
    if members.is_empty() {
        out.push(Problem::error(
            "no_members",
            None,
            format!("station {station_id} would have no stops"),
        ));
    } else if !committed {
        if let Some(why) = too_few_members(station_id, members.len()) {
            out.push(Problem::error("too_few_members", None, why));
        }
    }
    if station_taken && !committed {
        out.push(Problem::error(
            "station_exists",
            None,
            format!("a stop or station {station_id} already exists"),
        ));
    }
    let live_parents: HashMap<String, Option<String>> = members
        .iter()
        .map(|m| m.stop_id.to_string())
        .chain(draft.map(DraftView::merge_sources).unwrap_or_default())
        .map(|id| {
            let parent = live.get(&id).and_then(|s| s.parent_station.clone());
            (id, parent)
        })
        .collect();
    let parents = match draft {
        Some(d) => d.parents_after(&live_parents),
        None => live_parents,
    };
    for m in members {
        let id = m.stop_id;
        if let Some((into, by)) = draft.and_then(|d| d.merged_into(id)) {
            out.push(Problem::error(
                "stop_merged_away",
                Some(id),
                format!("stop {id} is merged into {into} by change {by} in the draft"),
            ));
            continue;
        }
        let created = draft.and_then(|d| d.created_stop(id));
        let (location_type, deleted, mut position) = match (live.get(id), created) {
            (Some(s), _) => (s.location_type, s.deleted, (s.lat, s.lon)),
            (None, Some(c)) => (c.location_type, false, (c.lat, c.lon)),
            (None, None) => {
                out.push(Problem::error(
                    "member_missing",
                    Some(id),
                    format!("stop {id} no longer exists"),
                ));
                continue;
            }
        };
        if deleted || draft.is_some_and(|d| d.stop_deleted(id)) {
            out.push(Problem::error(
                "member_deleted",
                Some(id),
                format!("stop {id} is deleted"),
            ));
            continue;
        }
        if location_type == 1 {
            out.push(Problem::error(
                "member_is_station",
                Some(id),
                format!("{id} is itself a station"),
            ));
            continue;
        }
        if let Some(Some(parent)) = parents.get(id) {
            if parent != station_id {
                out.push(Problem::error(
                    "member_has_parent",
                    Some(id),
                    format!("stop {id} already belongs to station {parent}"),
                ));
            }
        }
        if let Some(p) = draft.and_then(|d| d.stop_position(id)) {
            position = p;
        }
        if let Some((lat, lon)) = m.recorded {
            let moved = haversine_m(lat, lon, position.0, position.1);
            if moved > MOVED_METRES {
                out.push(Problem {
                    level: Level::Warning,
                    code: "member_moved",
                    stop_id: Some(id.to_string()),
                    message: format!("stop {id} moved {moved:.0} m since the proposal was built"),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------- reads

fn parse_statuses(status: Option<&str>) -> EditorResult<Vec<String>> {
    parse_status_list(status, &STATUSES)
}

/// A list's `status` filter: a comma list of `allowed` values, `pending` when
/// none is given (400 `invalid_status` for any other value).
pub fn parse_status_list(status: Option<&str>, allowed: &[&str]) -> EditorResult<Vec<String>> {
    let mut list: Vec<String> = status
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if list.is_empty() {
        list.push("pending".into());
    }
    if let Some(bad) = list.iter().find(|s| !allowed.contains(&s.as_str())) {
        return Err(EditorError::bad_request(
            "invalid_status",
            format!("unknown status {bad:?} (statuses: {})", allowed.join(", ")),
        ));
    }
    Ok(list)
}

/// The station proposal list's filters; the position review list takes the same.
pub struct ListQuery {
    pub status: Option<String>,
    pub bbox: Option<(f64, f64, f64, f64)>,
    pub q: Option<String>,
}

pub async fn list(
    state: &EditorState,
    gtfs_id: &str,
    query: &ListQuery,
    page: &Page,
) -> EditorResult<Value> {
    let statuses = parse_statuses(query.status.as_deref())?;
    let q = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (min_lat, min_lon, max_lat, max_lon) = match query.bbox {
        Some((a, b, c, d)) => (Some(a), Some(b), Some(c), Some(d)),
        None => (None, None, None, None),
    };
    let like = q.map(|q| {
        format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    });
    let rows = sqlx::query(&format!(
        "{SELECT} \
         WHERE p.gtfs_id = $1 AND p.status = ANY($2) \
           AND ($3::float8 IS NULL OR (p.lat BETWEEN $3 AND $5 AND p.lon BETWEEN $4 AND $6)) \
           AND ($7::text IS NULL OR p.station_id = $7 OR p.name ILIKE $8 OR p.name % $7 \
                OR p.members @> jsonb_build_array(jsonb_build_object('stop_id', $7::text))) \
         ORDER BY (p.station_id = $7 \
                   OR p.members @> jsonb_build_array(jsonb_build_object('stop_id', $7::text))) DESC NULLS LAST, \
                  similarity(p.name, coalesce($7, '')) DESC, p.proposal_id \
         LIMIT $9 OFFSET $10"
    ))
    .bind(gtfs_id)
    .bind(&statuses)
    .bind(min_lat)
    .bind(min_lon)
    .bind(max_lat)
    .bind(max_lon)
    .bind(q)
    .bind(like)
    .bind(page.limit + 1)
    .bind(page.offset)
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| from_row(r).map(|p| p.json))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(page.wrap(items))
}

pub async fn summary(state: &EditorState, gtfs_id: &str) -> EditorResult<Value> {
    let rows = sqlx::query(
        "SELECT status, count(*) AS n FROM gtfs_station_proposal WHERE gtfs_id = $1 GROUP BY status",
    )
    .bind(gtfs_id)
    .fetch_all(&state.pool)
    .await?;
    let mut out = json!({"pending": 0, "approved": 0, "rejected": 0, "committed": 0});
    for r in &rows {
        let status: String = r.try_get("status")?;
        if out.get(&status).is_some() {
            out[status] = json!(r.try_get::<i64, _>("n")?);
        }
    }
    Ok(out)
}

/// The proposal with each member's current stop row and what stands in the way
/// of it now (against the live data).
pub async fn detail(conn: &mut PgConnection, id: i64) -> EditorResult<Value> {
    let p = load(conn, id, false).await?;
    let ids: Vec<String> = p.members.iter().map(|m| member_id(m).to_string()).collect();
    let live = stops_now(conn, &p.gtfs_id, &ids, true).await?;
    let taken = !ids_taken(conn, &p.gtfs_id, std::slice::from_ref(&p.station_id))
        .await?
        .is_empty();
    let checks: Vec<MemberCheck> = p
        .members
        .iter()
        .map(|m| MemberCheck {
            stop_id: member_id(m),
            recorded: member_position(m),
        })
        .collect();
    let found = problems(
        &p.station_id,
        p.status == "committed",
        taken,
        &checks,
        &live,
        None,
    );
    let members: Vec<Value> = p
        .members
        .iter()
        .map(|m| {
            let mut m = m.clone();
            m["current"] = match live.get(member_id(&m)) {
                None => Value::Null,
                Some(s) => json!({
                    "name": s.name, "lat": s.lat, "lon": s.lon,
                    "parent_station": s.parent_station, "platform_code": s.platform_code,
                    "location_type": s.location_type, "deleted": s.deleted,
                    "row_version": s.row_version, "route_count": s.route_count,
                    "moved_m": member_position(&m)
                        .map(|(lat, lon)| (haversine_m(lat, lon, s.lat, s.lon) * 10.0).round() / 10.0),
                }),
            };
            m
        })
        .collect();
    let mut out = p.json;
    out["members"] = json!(members);
    out["problems"] = json!(found);
    Ok(out)
}

// ---------------------------------------------------------------- approve

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveBody {
    pub change_set_id: Uuid,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    #[serde(default)]
    pub members: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BulkApproveBody {
    pub change_set_id: Uuid,
    pub proposal_ids: Vec<i64>,
}

/// A member of the station an approval creates. `platform_code`: `None` leaves
/// the stop's label as it is, `Some(None)` clears it.
struct Planned<'a> {
    stop_id: String,
    platform_code: Option<Option<String>>,
    recorded: Option<&'a Value>,
}

/// The members the station gets: the proposal's, or the reviewer's subset with
/// their labels (a member sent without `platform_code` keeps the proposal's).
fn planned_members<'a>(p: &'a Proposal, edits: Option<&[Value]>) -> EditorResult<Vec<Planned<'a>>> {
    let recorded_code = |m: &Value| {
        m["platform_code"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Some(s.to_string()))
    };
    let Some(edits) = edits else {
        return Ok(p
            .members
            .iter()
            .map(|m| Planned {
                stop_id: member_id(m).to_string(),
                platform_code: recorded_code(m),
                recorded: Some(m),
            })
            .collect());
    };
    let invalid = |msg: String| EditorError::bad_request("invalid_members", msg);
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(edits.len());
    for v in edits {
        let o = v
            .as_object()
            .ok_or_else(|| invalid("each member is {stop_id, platform_code}".into()))?;
        if let Some(k) = o
            .keys()
            .find(|k| !["stop_id", "platform_code"].contains(&k.as_str()))
        {
            return Err(invalid(format!("a member has no field {k:?}")));
        }
        let id = o
            .get("stop_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("each member needs a stop_id".into()))?;
        let rec =
            p.members
                .iter()
                .find(|m| member_id(m) == id)
                .ok_or_else(|| {
                    EditorError::bad_request(
                "member_not_in_proposal",
                format!("{id} is not a member of this proposal; a reviewer can only drop members"),
            )
            .with_details(json!({"stop_id": id}))
                })?;
        if !seen.insert(id) {
            return Err(invalid(format!("{id} is listed twice")));
        }
        let platform_code = match o.get("platform_code") {
            None => recorded_code(rec),
            Some(Value::Null) => Some(None),
            Some(Value::String(s)) => {
                let s = s.trim();
                if s.chars().count() > PLATFORM_CODE_MAX_CHARS {
                    return Err(EditorError::bad_request(
                        "invalid_platform_code",
                        format!("the platform label of {id} is longer than {PLATFORM_CODE_MAX_CHARS} characters"),
                    ));
                }
                Some((!s.is_empty()).then(|| s.to_string()))
            }
            Some(_) => {
                return Err(invalid(format!(
                    "the platform_code of {id} is text or null"
                )))
            }
        };
        out.push(Planned {
            stop_id: id.to_string(),
            platform_code,
            recorded: Some(rec),
        });
    }
    Ok(out)
}

fn station_after(p: &Proposal, name: &str, lat: f64, lon: f64, members: &[Planned]) -> Value {
    json!({
        "station_id": p.station_id,
        "name": name,
        "lat": lat,
        "lon": lon,
        "members": members.iter().map(|m| {
            let mut o = json!({"stop_id": m.stop_id});
            if let Some(code) = &m.platform_code {
                o["platform_code"] = json!(code);
            }
            o
        }).collect::<Vec<_>>(),
        "proposal_id": p.proposal_id,
    })
}

async fn mark_approved(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    reviewer: Uuid,
    approved: &[(i64, i64)],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE gtfs_station_proposal p SET status = 'approved', change_set_id = $1, change_id = u.cid, \
            reviewed_by = $2, reviewed_at = now(), review_note = NULL \
         FROM UNNEST($3::int8[], $4::int8[]) AS u(pid, cid) WHERE p.proposal_id = u.pid",
    )
    .bind(change_set_id)
    .bind(reviewer)
    .bind(approved.iter().map(|a| a.0).collect::<Vec<_>>())
    .bind(approved.iter().map(|a| a.1).collect::<Vec<_>>())
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE gtfs_change_set SET updated_at = now() WHERE change_set_id = $1")
        .bind(change_set_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

fn not_pending(p: &Proposal) -> EditorError {
    EditorError::conflict(
        "proposal_not_pending",
        format!("proposal {} is {}", p.proposal_id, p.status),
    )
    .with_details(json!({"status": p.status, "change_set_id": p.json["change_set_id"]}))
}

/// Approve one proposal, as it is or with the reviewer's edits, into a draft.
pub async fn approve(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    body: ApproveBody,
) -> EditorResult<Value> {
    let name = match body.name.as_deref().map(str::trim) {
        Some("") => {
            return Err(EditorError::bad_request(
                "name_required",
                "the station needs a name",
            ))
        }
        other => other.map(str::to_string),
    };
    match (body.lat, body.lon) {
        (None, None) => {}
        (Some(lat), Some(lon)) if valid_lat_lon(lat, lon) && (lat, lon) != (0.0, 0.0) => {}
        _ => {
            return Err(EditorError::bad_request(
                "invalid_position",
                "lat and lon are sent together and must be a valid position",
            ))
        }
    }
    let mut tx = state.pool.begin().await?;
    let set = service::load_set(&mut tx, body.change_set_id, true).await?;
    let p = load(&mut tx, id, true).await?;
    if p.gtfs_id != set.gtfs_id {
        return Err(EditorError::bad_request(
            "feed_mismatch",
            format!(
                "the draft is for feed {} and the proposal for feed {}",
                set.gtfs_id, p.gtfs_id
            ),
        ));
    }
    service::editable(&set)?;
    if p.status != "pending" {
        return Err(not_pending(&p));
    }
    let members = planned_members(&p, body.members.as_deref())?;
    let draft = DraftView::load(&mut tx, set.change_set_id).await?;
    let mut ids: Vec<String> = members.iter().map(|m| m.stop_id.clone()).collect();
    ids.extend(draft.merge_sources());
    let live = stops_now(&mut tx, &p.gtfs_id, &ids, false).await?;
    let taken = !ids_taken(&mut tx, &p.gtfs_id, std::slice::from_ref(&p.station_id))
        .await?
        .is_empty()
        || draft.created_stop(&p.station_id).is_some();
    let checks: Vec<MemberCheck> = members
        .iter()
        .map(|m| MemberCheck {
            stop_id: &m.stop_id,
            recorded: m.recorded.and_then(member_position),
        })
        .collect();
    let found = problems(&p.station_id, false, taken, &checks, &live, Some(&draft));
    if found.iter().any(|x| x.level == Level::Error) {
        return Err(EditorError::bad_request(
            "proposal_has_problems",
            "the station would not be valid now; see the problems",
        )
        .with_details(json!({"problems": found})));
    }
    let renamed = name.as_deref().is_some_and(|n| n != p.name);
    let moved = body.lat.is_some();
    let after = station_after(
        &p,
        name.as_deref().unwrap_or(&p.name),
        body.lat.unwrap_or(p.lat),
        body.lon.unwrap_or(p.lon),
        &members,
    );
    check_payload("station", "create", &p.station_id, &after).map_err(|f| {
        EditorError::bad_request("invalid_change", f.message.clone())
            .with_details(json!({"code": f.code}))
    })?;
    let change_id = service::insert_changes(
        &mut tx,
        set.change_set_id,
        ctx.user.user_id,
        &[ChangeInsert {
            entity: "station".into(),
            op: "create".into(),
            entity_key: p.station_id.clone(),
            base_row_version: None,
            before: Value::Null,
            after,
        }],
    )
    .await?[0];
    mark_approved(
        &mut tx,
        set.change_set_id,
        ctx.user.user_id,
        &[(id, change_id)],
    )
    .await?;
    let kept: HashSet<&str> = members.iter().map(|m| m.stop_id.as_str()).collect();
    let relabelled = members
        .iter()
        .filter(|m| {
            let recorded = m.recorded.and_then(|r| r["platform_code"].as_str());
            // `None` leaves the label as the proposal has it
            m.platform_code
                .as_ref()
                .is_some_and(|code| code.as_deref() != recorded)
        })
        .count();
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "station_proposal_approved",
        Some(&p.gtfs_id),
        Some(set.change_set_id),
        json!({
            "proposal_id": id, "station_id": p.station_id, "change_id": change_id,
            "renamed": renamed, "moved": moved, "members": members.len(),
            "dropped": p.members.iter().map(member_id).filter(|m| !kept.contains(m)).collect::<Vec<_>>(),
            "relabelled": relabelled,
        }),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id).await
}

/// Approve several proposals unchanged into a draft, in one transaction. A
/// proposal that is not pending or has any problem is skipped, not fatal.
pub async fn approve_many(
    state: &EditorState,
    ctx: &Ctx,
    gtfs_id: &str,
    body: BulkApproveBody,
) -> EditorResult<Value> {
    let mut seen = HashSet::new();
    let ids: Vec<i64> = body
        .proposal_ids
        .iter()
        .copied()
        .filter(|id| seen.insert(*id))
        .collect();
    if ids.is_empty() {
        return Err(EditorError::bad_request(
            "proposal_ids_required",
            "send the proposals to approve",
        ));
    }
    if ids.len() > MAX_BULK {
        return Err(EditorError::bad_request(
            "too_many_proposals",
            format!("approve at most {MAX_BULK} proposals at once"),
        ));
    }
    let mut tx = state.pool.begin().await?;
    let set = service::load_set(&mut tx, body.change_set_id, true).await?;
    if set.gtfs_id != gtfs_id {
        return Err(EditorError::bad_request(
            "feed_mismatch",
            format!("the draft is for feed {}, not {gtfs_id}", set.gtfs_id),
        ));
    }
    service::editable(&set)?;
    let by_id: HashMap<i64, Proposal> = sqlx::query(&format!(
        "{SELECT} WHERE p.proposal_id = ANY($1) AND p.gtfs_id = $2 ORDER BY p.proposal_id FOR UPDATE OF p"
    ))
    .bind(&ids)
    .bind(gtfs_id)
    .fetch_all(&mut *tx)
    .await?
    .iter()
    .map(|r| from_row(r).map(|p| (p.proposal_id, p)))
    .collect::<Result<_, _>>()?;
    let draft = DraftView::load(&mut tx, set.change_set_id).await?;
    let pending = || by_id.values().filter(|p| p.status == "pending");
    let mut stop_ids: Vec<String> = pending()
        .flat_map(|p| p.members.iter().map(|m| member_id(m).to_string()))
        .collect();
    stop_ids.extend(draft.merge_sources());
    let live = stops_now(&mut tx, gtfs_id, &stop_ids, false).await?;
    let station_ids: Vec<String> = pending().map(|p| p.station_id.clone()).collect();
    let taken = ids_taken(&mut tx, gtfs_id, &station_ids).await?;

    let mut claimed_stops: HashMap<String, i64> = HashMap::new();
    let mut claimed_stations: HashMap<String, i64> = HashMap::new();
    let mut results: Vec<Value> = Vec::with_capacity(ids.len());
    let mut planned: Vec<(i64, usize, String, ChangeInsert)> = Vec::new();
    for pid in &ids {
        let skip =
            |problems: Vec<Problem>| json!({"proposal_id": pid, "ok": false, "problems": problems});
        let Some(p) = by_id.get(pid) else {
            results.push(skip(vec![Problem::error(
                "proposal_not_found",
                None,
                format!("no proposal {pid} for feed {gtfs_id}"),
            )]));
            continue;
        };
        if p.status != "pending" {
            results.push(skip(vec![Problem::error(
                "proposal_not_pending",
                None,
                format!("proposal {pid} is {}", p.status),
            )]));
            continue;
        }
        let members = planned_members(p, None)?;
        let checks: Vec<MemberCheck> = members
            .iter()
            .map(|m| MemberCheck {
                stop_id: &m.stop_id,
                recorded: m.recorded.and_then(member_position),
            })
            .collect();
        let station_taken =
            taken.contains(&p.station_id) || draft.created_stop(&p.station_id).is_some();
        let mut found = problems(
            &p.station_id,
            false,
            station_taken,
            &checks,
            &live,
            Some(&draft),
        );
        for m in &members {
            if let Some(other) = claimed_stops.get(&m.stop_id) {
                found.push(Problem::error(
                    "member_has_parent",
                    Some(&m.stop_id),
                    format!(
                        "stop {} is in proposal {other}, approved in this request",
                        m.stop_id
                    ),
                ));
            }
        }
        if let Some(other) = claimed_stations.get(&p.station_id) {
            found.push(Problem::error(
                "station_exists",
                None,
                format!(
                    "proposal {other} in this request creates station {}",
                    p.station_id
                ),
            ));
        }
        let after = station_after(p, &p.name, p.lat, p.lon, &members);
        if let Err(f) = check_payload("station", "create", &p.station_id, &after) {
            found.push(Problem::error("invalid_change", None, f.message));
        }
        if !found.is_empty() {
            results.push(skip(found));
            continue;
        }
        for m in &members {
            claimed_stops.insert(m.stop_id.clone(), *pid);
        }
        claimed_stations.insert(p.station_id.clone(), *pid);
        results.push(json!({"proposal_id": pid, "ok": true}));
        planned.push((
            *pid,
            results.len() - 1,
            p.station_id.clone(),
            ChangeInsert {
                entity: "station".into(),
                op: "create".into(),
                entity_key: p.station_id.clone(),
                base_row_version: None,
                before: Value::Null,
                after,
            },
        ));
    }
    let (inserts, meta): (Vec<ChangeInsert>, Vec<(i64, usize, String)>) = planned
        .into_iter()
        .map(|(pid, at, station, c)| (c, (pid, at, station)))
        .unzip();
    let change_ids =
        service::insert_changes(&mut tx, set.change_set_id, ctx.user.user_id, &inserts).await?;
    if !change_ids.is_empty() {
        let pairs: Vec<(i64, i64)> = meta
            .iter()
            .zip(&change_ids)
            .map(|((pid, _, _), cid)| (*pid, *cid))
            .collect();
        mark_approved(&mut tx, set.change_set_id, ctx.user.user_id, &pairs).await?;
        let details: Vec<Value> = meta
            .iter()
            .zip(&change_ids)
            .map(|((pid, _, station), cid)| {
                json!({"proposal_id": pid, "station_id": station, "change_id": cid, "bulk": true})
            })
            .collect();
        auth::audit_many(
            &mut *tx,
            Some(ctx.user.user_id),
            Some(&ctx.user.email),
            "station_proposal_approved",
            Some(gtfs_id),
            Some(set.change_set_id),
            &details,
        )
        .await?;
        for ((_, at, _), cid) in meta.iter().zip(&change_ids) {
            results[*at]["change_id"] = json!(cid);
        }
    }
    tx.commit().await?;
    let approved = change_ids.len();
    Ok(json!({
        "results": results,
        "approved": approved,
        "skipped": ids.len() - approved,
        "change_set_id": set.change_set_id,
    }))
}

// ---------------------------------------------------------------- reject, reopen

pub async fn reject(
    state: &EditorState,
    ctx: &Ctx,
    id: i64,
    note: Option<&str>,
) -> EditorResult<Value> {
    let note = note
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            EditorError::bad_request("note_required", "say why the proposal is rejected")
        })?;
    let mut tx = state.pool.begin().await?;
    let p = load(&mut tx, id, true).await?;
    if p.status != "pending" {
        return Err(not_pending(&p));
    }
    sqlx::query(
        "UPDATE gtfs_station_proposal SET status = 'rejected', reviewed_by = $2, reviewed_at = now(), \
            review_note = $3 WHERE proposal_id = $1",
    )
    .bind(id)
    .bind(ctx.user.user_id)
    .bind(note)
    .execute(&mut *tx)
    .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "station_proposal_rejected",
        Some(&p.gtfs_id),
        None,
        json!({"proposal_id": id, "station_id": p.station_id, "note": note}),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id).await
}

pub async fn reopen(state: &EditorState, ctx: &Ctx, id: i64) -> EditorResult<Value> {
    let mut tx = state.pool.begin().await?;
    let p = load(&mut tx, id, true).await?;
    if p.status != "rejected" {
        return Err(EditorError::conflict(
            "proposal_not_rejected",
            format!(
                "proposal {id} is {}; only a rejected proposal is reopened",
                p.status
            ),
        ));
    }
    let reopened = sqlx::query(
        "UPDATE gtfs_station_proposal SET status = 'pending', reviewed_by = NULL, reviewed_at = NULL, \
            review_note = NULL WHERE proposal_id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await;
    match reopened {
        Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505") => {
            return Err(EditorError::conflict(
                "proposal_superseded",
                format!(
                    "another open proposal already suggests station {}",
                    p.station_id
                ),
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
        "station_proposal_reopened",
        Some(&p.gtfs_id),
        None,
        json!({"proposal_id": id, "station_id": p.station_id}),
    )
    .await?;
    tx.commit().await?;
    let mut conn = state.pool.acquire().await?;
    detail(&mut conn, id).await
}

// ---------------------------------------------------------------- lifecycle

/// A proposal whose change left its draft is pending again: every proposal
/// approved into `change_set_id` (only the one behind `change_id`, if given).
pub async fn return_to_pending(
    conn: &mut PgConnection,
    ctx: &Ctx,
    gtfs_id: &str,
    change_set_id: Uuid,
    change_id: Option<i64>,
    reason: &str,
) -> Result<(), sqlx::Error> {
    let returned = sqlx::query(
        "UPDATE gtfs_station_proposal SET status = 'pending', change_set_id = NULL, change_id = NULL, \
            reviewed_by = NULL, reviewed_at = NULL \
         WHERE change_set_id = $1 AND status = 'approved' AND ($2::int8 IS NULL OR change_id = $2) \
         RETURNING proposal_id, station_id",
    )
    .bind(change_set_id)
    .bind(change_id)
    .fetch_all(&mut *conn)
    .await?;
    let details = returned
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            Ok(json!({
                "proposal_id": r.try_get::<i64, _>("proposal_id")?,
                "station_id": r.try_get::<String, _>("station_id")?,
                "reason": reason,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    auth::audit_many(
        &mut *conn,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "station_proposal_returned",
        Some(gtfs_id),
        Some(change_set_id),
        &details,
    )
    .await
}

/// The draft carrying these proposals is committed.
pub async fn mark_committed(
    conn: &mut PgConnection,
    ctx: &Ctx,
    gtfs_id: &str,
    change_set_id: Uuid,
    feed_version: i64,
) -> Result<(), sqlx::Error> {
    let committed = sqlx::query(
        "UPDATE gtfs_station_proposal SET status = 'committed' \
         WHERE change_set_id = $1 AND status = 'approved' RETURNING proposal_id, station_id, change_id",
    )
    .bind(change_set_id)
    .fetch_all(&mut *conn)
    .await?;
    let details = committed
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            Ok(json!({
                "proposal_id": r.try_get::<i64, _>("proposal_id")?,
                "station_id": r.try_get::<String, _>("station_id")?,
                "change_id": r.try_get::<Option<i64>, _>("change_id")?,
                "feed_version": feed_version,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    auth::audit_many(
        &mut *conn,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "station_proposal_committed",
        Some(gtfs_id),
        Some(change_set_id),
        &details,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::draft::DraftChange;

    fn stop(lat: f64, lon: f64, parent: Option<&str>) -> StopNow {
        StopNow {
            name: "X".into(),
            lat,
            lon,
            location_type: 0,
            parent_station: parent.map(str::to_string),
            platform_code: None,
            deleted: false,
            row_version: 1,
            route_count: None,
        }
    }

    fn codes(p: &[Problem]) -> Vec<(&str, Option<&str>)> {
        p.iter().map(|x| (x.code, x.stop_id.as_deref())).collect()
    }

    #[test]
    fn problems_against_live_data() {
        let mut live = HashMap::new();
        live.insert("A".to_string(), stop(13.0, 80.0, None));
        live.insert("B".to_string(), stop(13.0, 80.0, Some("stn_other")));
        live.insert(
            "C".to_string(),
            StopNow {
                deleted: true,
                ..stop(13.0, 80.0, None)
            },
        );
        live.insert(
            "D".to_string(),
            StopNow {
                location_type: 1,
                ..stop(13.0, 80.0, None)
            },
        );
        live.insert("E".to_string(), stop(13.002, 80.0, Some("stn_1")));
        let m = |id, lat| MemberCheck {
            stop_id: id,
            recorded: Some((lat, 80.0)),
        };
        let members = [
            m("A", 13.0),
            m("B", 13.0),
            m("C", 13.0),
            m("D", 13.0),
            m("E", 13.0),
            m("Z", 13.0),
        ];
        let found = problems("stn_1", false, true, &members, &live, None);
        assert_eq!(
            codes(&found),
            vec![
                ("station_exists", None),
                ("member_has_parent", Some("B")),
                ("member_deleted", Some("C")),
                ("member_is_station", Some("D")),
                // E is already in this very station: not a problem; but it moved ~222 m
                ("member_moved", Some("E")),
                ("member_missing", Some("Z")),
            ]
        );
        assert_eq!(found[4].level, Level::Warning);
        // a committed proposal's own station is expected to exist
        let found = problems("stn_1", true, true, &members[..1], &live, None);
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(
            codes(&problems("stn_1", false, false, &[], &live, None)),
            vec![("no_members", None)]
        );
        // one stop is not a station
        let found = problems("stn_1", false, false, &members[..1], &live, None);
        assert_eq!(codes(&found), vec![("too_few_members", None)]);
        assert_eq!(
            found[0].message,
            "a station groups at least two stops, and stn_1 would have only one"
        );
    }

    #[test]
    fn problems_see_the_draft() {
        let mut live = HashMap::new();
        live.insert("A".to_string(), stop(13.0, 80.0, None));
        live.insert("B".to_string(), stop(13.0, 80.0, None));
        live.insert("C".to_string(), stop(13.0, 80.0, None));
        live.insert("M".to_string(), stop(13.0, 80.0, Some("stn_x")));
        let ch = |id, entity: &str, op: &str, key: &str, after: Value| DraftChange {
            change_id: id,
            entity: entity.into(),
            op: op.into(),
            entity_key: key.into(),
            after,
        };
        let draft = DraftView::from_changes(&[
            ch(
                1,
                "station",
                "create",
                "stn_9",
                json!({"station_id": "stn_9", "name": "n", "lat": 1.0, "lon": 1.0, "members": [{"stop_id": "A"}]}),
            ),
            ch(2, "stop", "delete", "B", Value::Null),
            ch(3, "stop", "merge", "M", json!({"into_stop_id": "C"})),
            ch(
                4,
                "stop",
                "create",
                "ed_new",
                json!({"stop_id": "ed_new", "name": "n", "lat": 13.0, "lon": 80.0}),
            ),
        ]);
        let members: Vec<MemberCheck> = ["A", "B", "C", "M", "ed_new"]
            .iter()
            .map(|id| MemberCheck {
                stop_id: id,
                recorded: Some((13.0, 80.0)),
            })
            .collect();
        let found = problems("stn_1", false, false, &members, &live, Some(&draft));
        assert_eq!(
            codes(&found),
            vec![
                ("member_has_parent", Some("A")),
                ("member_deleted", Some("B")),
                // C takes stn_x from the stop merged into it
                ("member_has_parent", Some("C")),
                ("stop_merged_away", Some("M")),
            ]
        );
    }

    #[test]
    fn statuses_default_to_pending_and_are_checked() {
        assert_eq!(parse_statuses(None).unwrap(), vec!["pending"]);
        assert_eq!(
            parse_statuses(Some("pending, approved")).unwrap(),
            vec!["pending", "approved"]
        );
        assert!(parse_statuses(Some("pending,bogus")).is_err());
    }
}
