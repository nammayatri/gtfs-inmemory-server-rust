//! Database work for the editor: reads of the live tables, change sets, and the
//! one piece everything else leans on - [`evaluate`].
//!
//! `evaluate` applies a change set's changes, in order, inside the caller's
//! transaction, each in its own savepoint. A change the database or the rules
//! refuse is rolled back to its savepoint and reported against its change id;
//! the rest carry on. Viewing a draft, previewing a route and submitting all run
//! it in a transaction that is then rolled back, so validation always uses the
//! same code path - and the same constraints - as the real commit, which runs it
//! and commits.
//!
//! Every transaction that runs `evaluate`, or writes a draft, first takes the
//! feed's advisory lock (`feed_lock.rs`) so two of them never deadlock on the
//! live rows, and is retried by [`retry_transient`] should one still hit a
//! serialization failure - which is never reported as a change's own finding.

use super::auth::{self, Ctx};
use super::draft::DraftView;
use super::error::{EditorError, EditorResult};
use super::feed_lock::{is_transient, lock_feed, lock_feed_of_set, retry_transient};
use super::validation::{
    check_payload, check_polyline, check_route_rows, create_id_field, encode_polyline,
    grade_against_live, grade_repointed, haversine_m, merge_effect, mint_stop_id, polyline_change,
    polyline_length_finding, polyline_length_m, read_points, settle_create_key, station_members,
    Finding, Level, MemberSpec, PolylineChange, RouteRow, SequencedStop, MERGE_FAR_METRES,
    MOVE_WARNING_METRES, ROUTE_RULE_CODES,
};
use super::EditorState;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

// ---------------------------------------------------------------- paging

pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 500;

pub struct Page {
    pub limit: i64,
    pub offset: i64,
}

impl Page {
    pub fn parse(limit: Option<i64>, cursor: Option<&str>) -> EditorResult<Page> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(EditorError::bad_request(
                "invalid_limit",
                format!("limit must be between 1 and {MAX_LIMIT}"),
            ));
        }
        let offset = match cursor.filter(|c| !c.is_empty()) {
            None => 0,
            Some(c) => URL_SAFE_NO_PAD
                .decode(c)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .and_then(|s| s.strip_prefix("o:").and_then(|n| n.parse::<i64>().ok()))
                .filter(|o| *o >= 0)
                .ok_or_else(|| EditorError::bad_request("invalid_cursor", "cursor is not valid"))?,
        };
        Ok(Page { limit, offset })
    }

    /// Wrap `items` (fetched with `limit + 1`) into `{items, next_cursor}`.
    pub fn wrap(&self, mut items: Vec<Value>) -> Value {
        let more = items.len() as i64 > self.limit;
        items.truncate(self.limit as usize);
        let next = more.then(|| URL_SAFE_NO_PAD.encode(format!("o:{}", self.offset + self.limit)));
        json!({"items": items, "next_cursor": next})
    }
}

pub fn like_pattern(q: &str) -> String {
    let escaped = q
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

fn json_col(r: &PgRow, col: &str) -> Result<Value, sqlx::Error> {
    let raw: Option<String> = r.try_get(col)?;
    Ok(raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null))
}

// ---------------------------------------------------------------- feeds

pub async fn feed_version(conn: &mut PgConnection, gtfs_id: &str) -> EditorResult<i64> {
    let row = sqlx::query("SELECT version FROM gtfs_feed WHERE gtfs_id = $1")
        .bind(gtfs_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| EditorError::not_found("feed_not_found", format!("no feed {gtfs_id}")))?;
    Ok(row.try_get("version")?)
}

pub async fn feeds(state: &EditorState) -> EditorResult<Value> {
    let rows = sqlx::query(
        "SELECT gtfs_id, display_name, version, data_source, released_version, released_at, updated_at \
         FROM gtfs_feed ORDER BY gtfs_id",
    )
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            Ok(json!({
                "gtfs_id": r.try_get::<String, _>("gtfs_id")?,
                "display_name": r.try_get::<String, _>("display_name")?,
                "version": r.try_get::<i64, _>("version")?,
                "data_source": r.try_get::<String, _>("data_source")?,
                "released_version": r.try_get::<Option<i64>, _>("released_version")?,
                "released_at": r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("released_at")?,
                "updated_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"items": items, "next_cursor": null}))
}

/// `GET /feeds/{g}/config` (docs/gtfs-editor.md "Feed data source"): which data
/// source GIMS serves the feed from, and the open change sets that would change
/// it. Nothing writes it here: a switch is a `feed_config` change in a draft.
pub async fn feed_config(state: &EditorState, gtfs_id: &str) -> EditorResult<Value> {
    let row = sqlx::query("SELECT gtfs_id, data_source, version FROM gtfs_feed WHERE gtfs_id = $1")
        .bind(gtfs_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| EditorError::not_found("feed_not_found", format!("no feed {gtfs_id}")))?;
    let pending = sqlx::query(
        "SELECT cs.change_set_id, cs.title, cs.status, c.change_id, c.after->>'data_source' AS data_source \
         FROM gtfs_change_set cs \
         JOIN gtfs_change c ON c.change_set_id = cs.change_set_id \
         WHERE cs.gtfs_id = $1 AND cs.status IN ('draft', 'submitted', 'approved') \
           AND c.entity = 'feed_config' AND c.entity_key = $1 \
         ORDER BY cs.updated_at DESC, cs.change_set_id, c.position",
    )
    .bind(gtfs_id)
    .fetch_all(&state.pool)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        Ok(json!({
            "change_set_id": r.try_get::<Uuid, _>("change_set_id")?,
            "change_set_title": r.try_get::<String, _>("title")?,
            "status": r.try_get::<String, _>("status")?,
            "change_id": r.try_get::<i64, _>("change_id")?,
            "data_source": r.try_get::<Option<String>, _>("data_source")?,
        }))
    })
    .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "gtfs_id": row.try_get::<String, _>("gtfs_id")?,
        "data_source": row.try_get::<String, _>("data_source")?,
        "version": row.try_get::<i64, _>("version")?,
        "pending": pending,
    }))
}

/// The feed's row as a `feed_config` change snapshots it: `{gtfs_id,
/// data_source, version}`, the read shape of `GET /feeds/{g}/config`.
async fn feed_config_row(conn: &mut PgConnection, gtfs_id: &str) -> EditorResult<Value> {
    let row = sqlx::query("SELECT gtfs_id, data_source, version FROM gtfs_feed WHERE gtfs_id = $1")
        .bind(gtfs_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| EditorError::not_found("feed_not_found", format!("no feed {gtfs_id}")))?;
    Ok(json!({
        "gtfs_id": row.try_get::<String, _>("gtfs_id")?,
        "data_source": row.try_get::<String, _>("data_source")?,
        "version": row.try_get::<i64, _>("version")?,
    }))
}

// ---------------------------------------------------------------- stops

/// Distinct routes calling at a stop, for a query aliasing gtfs_stop as `s`.
const ROUTE_COUNT: &str = "(SELECT count(DISTINCT rs.route_id) FROM gtfs_route_stop rs \
     WHERE rs.gtfs_id = s.gtfs_id AND rs.stop_id = s.stop_id) AS route_count";

/// A station's live platforms, for the same query: the map ties a station to
/// its platforms, and this tells it whether it has them all (section 11).
const PLATFORM_COUNT: &str = "(SELECT count(*) FROM gtfs_stop c \
     WHERE c.gtfs_id = s.gtfs_id AND c.parent_station = s.stop_id AND NOT c.deleted) AS platform_count";

fn stop_json_counted(r: &PgRow) -> Result<Value, sqlx::Error> {
    let mut v = stop_json(r)?;
    v["route_count"] = json!(r.try_get::<i64, _>("route_count")?);
    v["platform_count"] = json!(r.try_get::<i64, _>("platform_count")?);
    Ok(v)
}

const STOP_COLS: &str = "stop_id, stop_code, name, lat, lon, location_type, parent_station, \
     platform_code, description, cluster_id, regional_name, hindi_name, \
     info_json::text AS info_json, position_source, provenance::text AS provenance, deleted, \
     row_version, updated_at, updated_by";

fn stop_json(r: &PgRow) -> Result<Value, sqlx::Error> {
    Ok(json!({
        "stop_id": r.try_get::<String, _>("stop_id")?,
        "stop_code": r.try_get::<Option<String>, _>("stop_code")?,
        "name": r.try_get::<String, _>("name")?,
        "lat": r.try_get::<f64, _>("lat")?,
        "lon": r.try_get::<f64, _>("lon")?,
        "location_type": r.try_get::<i16, _>("location_type")?,
        "parent_station": r.try_get::<Option<String>, _>("parent_station")?,
        "platform_code": r.try_get::<Option<String>, _>("platform_code")?,
        "description": r.try_get::<Option<String>, _>("description")?,
        "cluster_id": r.try_get::<Option<String>, _>("cluster_id")?,
        "regional_name": r.try_get::<Option<String>, _>("regional_name")?,
        "hindi_name": r.try_get::<Option<String>, _>("hindi_name")?,
        "info_json": json_col(r, "info_json")?,
        "position_source": r.try_get::<Option<String>, _>("position_source")?,
        "provenance": json_col(r, "provenance")?,
        "deleted": r.try_get::<bool, _>("deleted")?,
        "row_version": r.try_get::<i32, _>("row_version")?,
        "updated_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?,
        "updated_by": r.try_get::<Option<String>, _>("updated_by")?,
    }))
}

pub async fn stop_row(
    conn: &mut PgConnection,
    gtfs_id: &str,
    stop_id: &str,
) -> EditorResult<Option<Value>> {
    let row = sqlx::query(&format!(
        "SELECT {STOP_COLS} FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = $2"
    ))
    .bind(gtfs_id)
    .bind(stop_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(stop_json).transpose()?)
}

/// Stop rows in read shape by id, in one query (a bulk upload's `before`s).
pub async fn stop_rows(
    conn: &mut PgConnection,
    gtfs_id: &str,
    stop_ids: &[String],
) -> EditorResult<HashMap<String, Value>> {
    if stop_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(&format!(
        "SELECT {STOP_COLS} FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = ANY($2)"
    ))
    .bind(gtfs_id)
    .bind(stop_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in &rows {
        let row = stop_json(r)?;
        out.insert(row["stop_id"].as_str().unwrap_or("").to_string(), row);
    }
    Ok(out)
}

/// The live platforms of each station, `(stop_id, platform_code)` in id order:
/// what a station's `before` lists as `member_stop_ids` and `members`.
pub async fn stations_members(
    conn: &mut PgConnection,
    gtfs_id: &str,
    station_ids: &[String],
) -> EditorResult<HashMap<String, Vec<(String, Option<String>)>>> {
    let mut out: HashMap<String, Vec<(String, Option<String>)>> = HashMap::new();
    if station_ids.is_empty() {
        return Ok(out);
    }
    for r in sqlx::query(
        "SELECT parent_station, stop_id, platform_code FROM gtfs_stop \
         WHERE gtfs_id = $1 AND parent_station = ANY($2) AND NOT deleted ORDER BY stop_id",
    )
    .bind(gtfs_id)
    .bind(station_ids)
    .fetch_all(&mut *conn)
    .await?
    {
        out.entry(r.try_get("parent_station")?)
            .or_default()
            .push((r.try_get("stop_id")?, r.try_get("platform_code")?));
    }
    Ok(out)
}

/// Add a station's members to its row, as a station change's `before` has them.
pub fn with_members(row: &mut Value, members: &[(String, Option<String>)]) {
    row["member_stop_ids"] = json!(members.iter().map(|m| &m.0).collect::<Vec<_>>());
    row["members"] = json!(members
        .iter()
        .map(|(id, code)| json!({"stop_id": id, "platform_code": code}))
        .collect::<Vec<_>>());
}

pub struct StopQuery {
    pub q: Option<String>,
    pub bbox: Option<(f64, f64, f64, f64)>,
    pub station: Option<String>,
}

pub async fn list_stops(
    state: &EditorState,
    gtfs_id: &str,
    query: &StopQuery,
    page: &Page,
) -> EditorResult<Value> {
    let q = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let (min_lat, min_lon, max_lat, max_lon) = match query.bbox {
        Some((a, b, c, d)) => (Some(a), Some(b), Some(c), Some(d)),
        None => (None, None, None, None),
    };
    let rows = sqlx::query(&format!(
        "SELECT {STOP_COLS}, {ROUTE_COUNT}, {PLATFORM_COUNT} \
         FROM gtfs_stop s \
         WHERE s.gtfs_id = $1 AND NOT s.deleted \
           AND ($2::text IS NULL OR s.stop_id = $2 OR s.stop_code = $2 \
                OR s.name ILIKE $3 OR s.name % $2) \
           AND ($4::float8 IS NULL OR (s.lat BETWEEN $4 AND $6 AND s.lon BETWEEN $5 AND $7)) \
           AND ($8::text IS NULL OR ($8 = 'true' AND s.location_type = 1) OR s.parent_station = $8) \
         ORDER BY (s.stop_id = $2 OR s.stop_code = $2) DESC NULLS LAST, \
                  similarity(s.name, coalesce($2, '')) DESC, s.stop_id \
         LIMIT $9 OFFSET $10"
    ))
    .bind(gtfs_id)
    .bind(q)
    .bind(q.map(like_pattern))
    .bind(min_lat)
    .bind(min_lon)
    .bind(max_lat)
    .bind(max_lon)
    .bind(query.station.as_deref())
    .bind(page.limit + 1)
    .bind(page.offset)
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(stop_json_counted)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(page.wrap(items))
}

pub async fn stop_detail(
    conn: &mut PgConnection,
    gtfs_id: &str,
    stop_id: &str,
) -> EditorResult<Value> {
    let mut stop = stop_row(conn, gtfs_id, stop_id)
        .await?
        .ok_or_else(|| EditorError::not_found("stop_not_found", format!("no stop {stop_id}")))?;
    let routes = sqlx::query(
        "SELECT rs.route_id, r.short_name, r.long_name, rs.sequence, rs.stop_type, rs.stage_no \
         FROM gtfs_route_stop rs \
         JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id \
         WHERE rs.gtfs_id = $1 AND rs.stop_id = $2 ORDER BY rs.route_id, rs.sequence",
    )
    .bind(gtfs_id)
    .bind(stop_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        Ok(json!({
            "route_id": r.try_get::<String, _>("route_id")?,
            "short_name": r.try_get::<Option<String>, _>("short_name")?,
            "long_name": r.try_get::<Option<String>, _>("long_name")?,
            "sequence": r.try_get::<i32, _>("sequence")?,
            "stop_type": r.try_get::<String, _>("stop_type")?,
            "stage_no": r.try_get::<i32, _>("stage_no")?,
        }))
    })
    .collect::<Result<Vec<_>, _>>()?;
    let children = sqlx::query(&format!(
        "SELECT {STOP_COLS}, {ROUTE_COUNT}, {PLATFORM_COUNT} FROM gtfs_stop s \
         WHERE s.gtfs_id = $1 AND s.parent_station = $2 AND NOT s.deleted ORDER BY s.stop_id"
    ))
    .bind(gtfs_id)
    .bind(stop_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(stop_json_counted)
    .collect::<Result<Vec<_>, _>>()?;
    let parent = match stop["parent_station"].as_str() {
        Some(p) => stop_row(conn, gtfs_id, p).await?,
        None => None,
    };

    let (lat, lon) = (
        stop["lat"].as_f64().unwrap_or(0.0),
        stop["lon"].as_f64().unwrap_or(0.0),
    );
    let nearby = sqlx::query(&format!(
        "SELECT {STOP_COLS}, {ROUTE_COUNT}, {PLATFORM_COUNT} FROM gtfs_stop s \
         WHERE s.gtfs_id = $1 AND s.stop_id <> $2 AND NOT s.deleted \
           AND s.lat BETWEEN $3 - 0.0006 AND $3 + 0.0006 AND s.lon BETWEEN $4 - 0.0007 AND $4 + 0.0007"
    ))
    .bind(gtfs_id)
    .bind(stop_id)
    .bind(lat)
    .bind(lon)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(stop_json_counted)
    .collect::<Result<Vec<_>, _>>()?
    .into_iter()
    .filter_map(|mut s| {
        let d = haversine_m(lat, lon, s["lat"].as_f64()?, s["lon"].as_f64()?);
        (d <= 60.0).then(|| {
            s["distance_m"] = json!((d * 10.0).round() / 10.0);
            s
        })
    })
    .collect::<Vec<_>>();
    let mut nearby = nearby;
    nearby.sort_by(|a, b| {
        a["distance_m"]
            .as_f64()
            .partial_cmp(&b["distance_m"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    stop["route_count"] = json!(routes
        .iter()
        .filter_map(|r| r["route_id"].as_str())
        .collect::<HashSet<_>>()
        .len());
    stop["routes"] = json!(routes);
    stop["platform_count"] = json!(children.len());
    stop["children"] = json!(children);
    stop["nearby"] = json!(nearby);
    stop["parent"] = parent.unwrap_or(Value::Null);
    Ok(stop)
}

// ---------------------------------------------------------------- routes

const ROUTE_COLS: &str =
    "route_id, short_name, long_name, route_type, agency_id, color, text_color, \
     encoded_polyline, polyline_source, service_type, provenance::text AS provenance, deleted, \
     row_version, updated_at, updated_by";

fn route_json(r: &PgRow) -> Result<Value, sqlx::Error> {
    Ok(json!({
        "route_id": r.try_get::<String, _>("route_id")?,
        "short_name": r.try_get::<Option<String>, _>("short_name")?,
        "long_name": r.try_get::<Option<String>, _>("long_name")?,
        "route_type": r.try_get::<i16, _>("route_type")?,
        "agency_id": r.try_get::<Option<String>, _>("agency_id")?,
        "color": r.try_get::<Option<String>, _>("color")?,
        "text_color": r.try_get::<Option<String>, _>("text_color")?,
        "encoded_polyline": r.try_get::<Option<String>, _>("encoded_polyline")?,
        "polyline_source": r.try_get::<Option<String>, _>("polyline_source")?,
        "service_type": r.try_get::<Option<String>, _>("service_type")?,
        "provenance": json_col(r, "provenance")?,
        "deleted": r.try_get::<bool, _>("deleted")?,
        "row_version": r.try_get::<i32, _>("row_version")?,
        "updated_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?,
        "updated_by": r.try_get::<Option<String>, _>("updated_by")?,
    }))
}

pub async fn route_row(
    conn: &mut PgConnection,
    gtfs_id: &str,
    route_id: &str,
) -> EditorResult<Option<Value>> {
    let row = sqlx::query(&format!(
        "SELECT {ROUTE_COLS} FROM gtfs_route WHERE gtfs_id = $1 AND route_id = $2"
    ))
    .bind(gtfs_id)
    .bind(route_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(route_json).transpose()?)
}

/// [`route_row`] for many routes in one query, by route id; routes that do not
/// exist are absent. What an upload's `before` snapshots, one query for a file.
pub async fn route_rows(
    conn: &mut PgConnection,
    gtfs_id: &str,
    route_ids: &[String],
) -> EditorResult<HashMap<String, Value>> {
    let mut out = HashMap::new();
    if route_ids.is_empty() {
        return Ok(out);
    }
    for r in sqlx::query(&format!(
        "SELECT {ROUTE_COLS} FROM gtfs_route WHERE gtfs_id = $1 AND route_id = ANY($2)"
    ))
    .bind(gtfs_id)
    .bind(route_ids)
    .fetch_all(&mut *conn)
    .await?
    {
        out.insert(r.try_get("route_id")?, route_json(&r)?);
    }
    Ok(out)
}

/// The `after` of each route a draft creates, by route id - the `before` of a
/// change to a route that has no live row yet (docs section 5).
pub async fn routes_created_in_set(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    keys: &[String],
) -> EditorResult<HashMap<String, Value>> {
    let mut out = HashMap::new();
    if keys.is_empty() {
        return Ok(out);
    }
    for r in sqlx::query(
        "SELECT entity_key, after::text AS after FROM gtfs_change \
         WHERE change_set_id = $1 AND op = 'create' AND entity = 'route' \
           AND entity_key = ANY($2) ORDER BY position DESC",
    )
    .bind(change_set_id)
    .bind(keys)
    .fetch_all(&mut *conn)
    .await?
    {
        // position DESC: the earliest create of an id is the one that stays
        out.insert(r.try_get("entity_key")?, json_col(&r, "after")?);
    }
    Ok(out)
}

pub async fn list_routes(
    state: &EditorState,
    gtfs_id: &str,
    q: Option<&str>,
    polyline: Option<bool>,
    page: &Page,
) -> EditorResult<Value> {
    let q = q.map(str::trim).filter(|s| !s.is_empty());
    let rows = sqlx::query(&format!(
        "SELECT {ROUTE_COLS}, \
            (SELECT count(*) FROM gtfs_route_stop rs WHERE rs.gtfs_id = r.gtfs_id \
               AND rs.route_id = r.route_id \
               AND rs.stop_type NOT IN ('ROUTE CORRECTION', 'JUMP STOP', 'HIDDEN STOP')) AS stop_count \
         FROM gtfs_route r \
         WHERE r.gtfs_id = $1 AND NOT r.deleted \
           AND ($2::text IS NULL OR r.route_id = $2 OR r.short_name ILIKE $3 OR r.long_name ILIKE $3) \
           AND ($6::bool IS NULL \
                OR ($6 = (r.encoded_polyline IS NOT NULL AND r.encoded_polyline <> ''))) \
         ORDER BY (r.route_id = $2 OR lower(r.short_name) = lower($2)) DESC NULLS LAST, \
                  r.short_name, r.route_id \
         LIMIT $4 OFFSET $5"
    ))
    .bind(gtfs_id)
    .bind(q)
    .bind(q.map(like_pattern))
    .bind(page.limit + 1)
    .bind(page.offset)
    .bind(polyline)
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            let mut v = route_json(r)?;
            let has = v["encoded_polyline"]
                .as_str()
                .is_some_and(|p| !p.is_empty());
            if let Some(o) = v.as_object_mut() {
                o.remove("encoded_polyline");
            }
            v["has_polyline"] = json!(has);
            v["stop_count"] = json!(r.try_get::<i64, _>("stop_count")?);
            Ok(v)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(page.wrap(items))
}

const ROUTE_ROW_COLS: &str = "stop_id, stop_type, stage_no, stage_name, marker_id, marker_name, \
     marker_lat, marker_lon, stop_name_override, provider_id";

fn route_row_from(r: &PgRow) -> Result<RouteRow, sqlx::Error> {
    Ok(RouteRow {
        stop_id: r.try_get("stop_id")?,
        stop_type: r.try_get("stop_type")?,
        stage_no: r.try_get("stage_no")?,
        stage_name: r.try_get("stage_name")?,
        marker_id: r.try_get("marker_id")?,
        marker_name: r.try_get("marker_name")?,
        marker_lat: r.try_get("marker_lat")?,
        marker_lon: r.try_get("marker_lon")?,
        stop_name_override: r.try_get("stop_name_override")?,
        provider_id: r.try_get("provider_id")?,
    })
}

pub async fn load_route_rows(
    conn: &mut PgConnection,
    gtfs_id: &str,
    route_id: &str,
) -> EditorResult<Vec<RouteRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {ROUTE_ROW_COLS} FROM gtfs_route_stop WHERE gtfs_id = $1 AND route_id = $2 ORDER BY sequence"
    ))
    .bind(gtfs_id)
    .bind(route_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(route_row_from)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// [`load_route_rows`] for many routes in one query. Routes without rows are
/// absent.
pub async fn load_routes_rows(
    conn: &mut PgConnection,
    gtfs_id: &str,
    route_ids: &[String],
) -> EditorResult<HashMap<String, Vec<RouteRow>>> {
    let rows = sqlx::query(&format!(
        "SELECT route_id, {ROUTE_ROW_COLS} FROM gtfs_route_stop \
         WHERE gtfs_id = $1 AND route_id = ANY($2) ORDER BY route_id, sequence"
    ))
    .bind(gtfs_id)
    .bind(route_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: HashMap<String, Vec<RouteRow>> = HashMap::new();
    for r in &rows {
        out.entry(r.try_get("route_id")?)
            .or_default()
            .push(route_row_from(r)?);
    }
    Ok(out)
}

/// The rows of many routes in read shape (the route detail's `rows`), in one
/// query. Routes without rows are absent.
pub async fn load_routes_read_rows(
    conn: &mut PgConnection,
    gtfs_id: &str,
    route_ids: &[String],
) -> EditorResult<HashMap<String, Vec<Value>>> {
    let rows = sqlx::query(
        "SELECT rs.route_id, rs.sequence, rs.stop_id, coalesce(rs.stop_name_override, s.name) AS stop_name, \
                s.lat, s.lon, s.deleted AS stop_deleted, s.parent_station, \
                rs.stop_type, rs.stage_no, rs.stage_name, rs.marker_id, rs.marker_name, rs.marker_lat, \
                rs.marker_lon, rs.stop_name_override, rs.provider_id \
         FROM gtfs_route_stop rs \
         LEFT JOIN gtfs_stop s ON s.gtfs_id = rs.gtfs_id AND s.stop_id = rs.stop_id \
         WHERE rs.gtfs_id = $1 AND rs.route_id = ANY($2) ORDER BY rs.route_id, rs.sequence",
    )
    .bind(gtfs_id)
    .bind(route_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: HashMap<String, Vec<Value>> = HashMap::new();
    for r in &rows {
        out.entry(r.try_get("route_id")?).or_default().push(json!({
            "sequence": r.try_get::<i32, _>("sequence")?,
            "stop_id": r.try_get::<Option<String>, _>("stop_id")?,
            "stop_name": r.try_get::<Option<String>, _>("stop_name")?,
            "lat": r.try_get::<Option<f64>, _>("lat")?,
            "lon": r.try_get::<Option<f64>, _>("lon")?,
            "stop_deleted": r.try_get::<Option<bool>, _>("stop_deleted")?,
            "parent_station": r.try_get::<Option<String>, _>("parent_station")?,
            "stop_type": r.try_get::<String, _>("stop_type")?,
            "stage_no": r.try_get::<i32, _>("stage_no")?,
            "stage_name": r.try_get::<String, _>("stage_name")?,
            "marker_id": r.try_get::<Option<String>, _>("marker_id")?,
            "marker_name": r.try_get::<Option<String>, _>("marker_name")?,
            "marker_lat": r.try_get::<Option<f64>, _>("marker_lat")?,
            "marker_lon": r.try_get::<Option<f64>, _>("marker_lon")?,
            "stop_name_override": r.try_get::<Option<String>, _>("stop_name_override")?,
            "provider_id": r.try_get::<Option<String>, _>("provider_id")?,
        }));
    }
    Ok(out)
}

/// Fingerprint of a route's whole stop order; a `route_stops` replace carries
/// the one it was based on, and commit refuses it if the route moved on.
pub fn rows_hash(rows: &[RouteRow]) -> String {
    super::crypto::sha256_hex(
        serde_json::to_string(rows)
            .expect("route rows serialize")
            .as_bytes(),
    )
}

pub async fn route_detail(
    conn: &mut PgConnection,
    gtfs_id: &str,
    route_id: &str,
) -> EditorResult<Value> {
    let mut route = route_row(conn, gtfs_id, route_id)
        .await?
        .ok_or_else(|| EditorError::not_found("route_not_found", format!("no route {route_id}")))?;
    let rows = load_routes_read_rows(conn, gtfs_id, &[route_id.to_string()])
        .await?
        .remove(route_id)
        .unwrap_or_default();
    let hash = rows_hash(&load_route_rows(conn, gtfs_id, route_id).await?);
    let served = rows
        .iter()
        .filter(|r| {
            r["stop_type"]
                .as_str()
                .is_some_and(|t| !super::validation::UNSERVED_TYPES.contains(&t))
        })
        .count();
    route["stop_count"] = json!(served);
    route["rows"] = json!(rows);
    route["rows_hash"] = json!(hash);
    Ok(route)
}

/// Coordinates a polyline should pass through: every boarded stop and every
/// shaping marker, in order. Jump and hidden stops are not on the bus's path.
pub fn polyline_waypoints(detail: &Value) -> Vec<(f64, f64)> {
    detail["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| match r["stop_type"].as_str()? {
                    "ROUTE CORRECTION" => {
                        Some((r["marker_lat"].as_f64()?, r["marker_lon"].as_f64()?))
                    }
                    "JUMP STOP" | "HIDDEN STOP" => None,
                    _ => Some((r["lat"].as_f64()?, r["lon"].as_f64()?)),
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- route map lines

/// How far a route's stops are apart end to end, following the same points a
/// map line should pass through ([`polyline_waypoints`]), for many routes in
/// three queries - never a read per route, so a whole uploaded file costs the
/// same as one row. The draft is taken as applying, so a route whose stop list
/// this draft replaces is measured along the list it will have. Routes with
/// nothing to measure are absent.
pub async fn stop_chain_lengths(
    conn: &mut PgConnection,
    gtfs_id: &str,
    route_ids: &[String],
    draft: &DraftView,
) -> EditorResult<HashMap<String, f64>> {
    if route_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let live = load_routes_rows(conn, gtfs_id, route_ids).await?;
    let drafted: HashMap<&String, Vec<RouteRow>> = route_ids
        .iter()
        .map(|id| {
            let rows = draft.current_rows(id, live.get(id).map(Vec::as_slice).unwrap_or_default());
            (id, rows)
        })
        .collect();
    let wanted: Vec<String> = drafted
        .values()
        .flatten()
        .filter(|r| !r.is_marker() && r.is_served())
        .filter_map(|r| r.stop_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut at: HashMap<String, (f64, f64)> = HashMap::with_capacity(wanted.len());
    if !wanted.is_empty() {
        for r in sqlx::query(
            "SELECT stop_id, lat, lon FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = ANY($2)",
        )
        .bind(gtfs_id)
        .bind(&wanted)
        .fetch_all(&mut *conn)
        .await?
        {
            at.insert(
                r.try_get("stop_id")?,
                (r.try_get("lat")?, r.try_get("lon")?),
            );
        }
    }
    let mut out = HashMap::with_capacity(route_ids.len());
    for (id, rows) in drafted {
        let points: Vec<(f64, f64)> = rows
            .iter()
            .filter_map(|r| {
                if r.is_marker() {
                    return Some((r.marker_lat?, r.marker_lon?));
                }
                if !r.is_served() {
                    return None;
                }
                let stop = r.stop_id.as_deref()?;
                // a stop the draft moves is measured where the draft puts it
                draft.stop_position(stop).or_else(|| at.get(stop).copied())
            })
            .collect();
        if points.len() >= 2 {
            out.insert(id.clone(), polyline_length_m(&points));
        }
    }
    Ok(out)
}

/// A map line an operator wants on a route: an encoded line, the points of one,
/// or the road router's proposal for it. Exactly one of the three.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolylineRequest {
    #[serde(default)]
    pub encoded_polyline: Option<String>,
    #[serde(default)]
    pub points: Option<Value>,
    /// The line [`super::handlers::polyline_osrm`] last proposed, sent back to
    /// be kept. `polyline_source` then defaults to `osrm`.
    #[serde(default)]
    pub from_osrm: bool,
    #[serde(default)]
    pub polyline_source: Option<String>,
    /// Overwriting a line the route already has is asked for, never assumed.
    #[serde(default)]
    pub replace: bool,
}

/// Add the operator's map line to a draft as the `route/update` change it is.
///
/// Everything a line has to survive is here, in the order the operator meets
/// it: the shape (one source, points encoded to the one form the table holds),
/// [`check_polyline`], then what it would do to the route - and a line over a
/// line the route already has is refused unless `replace` says so, so the
/// overwrite is always somebody's decision. The change itself goes through
/// [`add_change_to`], so what lands in the draft is exactly what `POST
/// /change-sets/{id}/changes` would have stored, `before` and base version and
/// audit row included; the diff then shows the line it replaces.
pub async fn set_route_polyline(
    state: &EditorState,
    ctx: &Ctx,
    change_set_id: Uuid,
    route_id: &str,
    req: PolylineRequest,
) -> EditorResult<Value> {
    retry_transient(|| set_route_polyline_once(state, ctx, change_set_id, route_id, req.clone()))
        .await
}

async fn set_route_polyline_once(
    state: &EditorState,
    ctx: &Ctx,
    change_set_id: Uuid,
    route_id: &str,
    req: PolylineRequest,
) -> EditorResult<Value> {
    let what = format!("route/update: {route_id}");
    let bad = |f: Finding| {
        EditorError::bad_request("invalid_change", f.message.clone())
            .with_details(json!({"code": f.code}))
    };
    let given = [
        req.encoded_polyline.is_some(),
        req.points.is_some(),
        req.from_osrm,
    ]
    .iter()
    .filter(|g| **g)
    .count();
    if given != 1 {
        return Err(EditorError::bad_request(
            "invalid_change",
            "send exactly one of encoded_polyline, points or from_osrm",
        )
        .with_details(json!({"code": "invalid_payload"})));
    }
    // the road router runs before the write transaction, as the proposal
    // endpoint does: it is a call out to another service, and nothing that slow
    // belongs under the feed's lock
    let (line, source) = match (&req.encoded_polyline, &req.points) {
        (Some(encoded), _) => (encoded.trim().to_string(), "manual"),
        (_, Some(points)) => (
            encode_polyline(&read_points(points, &what).map_err(bad)?),
            "manual",
        ),
        // routed through the stops as this draft leaves them, and routed now, so
        // what is stored is a line for the route the draft describes
        _ => (
            osrm_line(state, ctx, change_set_id, route_id).await?,
            "osrm",
        ),
    };
    let source = req.polyline_source.as_deref().unwrap_or(source).to_string();

    let mut tx = state.pool.begin().await?;
    lock_feed_of_set(&mut tx, change_set_id).await?;
    let set = load_set(&mut tx, change_set_id, true).await?;
    editable(&set)?;
    let g = set.gtfs_id.clone();
    let route = route_row(&mut tx, &g, route_id)
        .await?
        .ok_or_else(|| EditorError::not_found("route_not_found", format!("no route {route_id}")))?;
    if route["deleted"] == json!(true) {
        return Err(EditorError::bad_request(
            "route_deleted",
            format!("route {route_id} is deleted"),
        ));
    }
    let draft = DraftView::load(&mut tx, change_set_id).await?;
    if draft.route_deleted(route_id) {
        return Err(EditorError::bad_request(
            "route_deleted",
            format!("route {route_id} is deleted in this draft"),
        ));
    }
    let points = check_polyline(&line, &what).map_err(bad)?;

    let current = draft.polyline_after(route_id, route["encoded_polyline"].as_str());
    let outcome = polyline_change(current, &line);
    if outcome == PolylineChange::Unchanged {
        return Err(EditorError::bad_request(
            "polyline_unchanged",
            format!("route {route_id} already has this map line"),
        ));
    }
    if outcome == PolylineChange::Replaced && !req.replace {
        let had = decode_current(current);
        return Err(EditorError::conflict(
            "polyline_exists",
            format!(
                "route {route_id} already has a map line; send replace: true to put this one over it"
            ),
        )
        .with_details(json!({
            "route_id": route_id,
            "polyline_source": route["polyline_source"],
            "points": had.len(),
            "length_m": polyline_length_m(&had).round(),
            "in_draft": draft.route_updated_by(route_id),
        })));
    }

    let after = json!({"encoded_polyline": line, "polyline_source": source});
    let change_id = add_change_to(
        &mut tx,
        ctx,
        &set,
        NewChange {
            entity: "route".into(),
            op: "update".into(),
            entity_key: route_id.to_string(),
            after,
            base_row_version: route["row_version"].as_i64().map(|v| v as i32),
        },
    )
    .await?;
    let chain = stop_chain_lengths(&mut tx, &g, &[route_id.to_string()], &draft)
        .await?
        .remove(route_id)
        .unwrap_or(0.0);
    tx.commit().await?;

    let length = polyline_length_m(&points);
    let warnings: Vec<Value> = polyline_length_finding(&what, length, chain)
        .into_iter()
        .map(|f| json!({"level": f.level, "code": f.code, "message": f.message}))
        .collect();
    let mut detail = set_detail(state, ctx, change_set_id).await?;
    detail["change_id"] = json!(change_id);
    detail["polyline"] = json!({
        "route_id": route_id,
        "encoded_polyline": line,
        "polyline_source": source,
        "points": points.len(),
        "length_m": length.round(),
        "stop_chain_m": chain.round(),
        "replaced": outcome == PolylineChange::Replaced,
        "warnings": warnings,
    });
    Ok(detail)
}

/// The points of the line a route has now, for saying how long it was in the
/// refusal to overwrite it. A line already in the table always decodes.
fn decode_current(current: Option<&str>) -> Vec<(f64, f64)> {
    current
        .and_then(super::validation::decode_polyline)
        .unwrap_or_default()
}

/// The road router's line through a route's stops, the draft applied - the same
/// proposal `POST /feeds/{g}/routes/{id}/polyline:osrm` returns.
pub async fn osrm_line(
    state: &EditorState,
    ctx: &Ctx,
    change_set_id: Uuid,
    route_id: &str,
) -> EditorResult<String> {
    if state.osrm_url.as_deref().unwrap_or("").is_empty() {
        return Err(EditorError::new(
            actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
            "osrm_unavailable",
            "no OSRM server is configured",
        ));
    }
    let detail = preview_route(state, ctx, change_set_id, route_id).await?;
    let waypoints = polyline_waypoints(&detail);
    crate::services::operator::osrm_route(state.osrm_url.as_deref(), &waypoints)
        .await
        .map(|(line, _)| line)
        .ok_or_else(|| {
            EditorError::new(
                actix_web::http::StatusCode::BAD_GATEWAY,
                "osrm_failed",
                "OSRM could not route through these stops",
            )
        })
}

// ---------------------------------------------------------------- change sets

#[derive(Debug, Clone)]
pub struct ChangeRow {
    pub change_id: i64,
    pub position: i32,
    pub entity: String,
    pub entity_key: String,
    pub op: String,
    pub base_row_version: Option<i32>,
    pub before: Value,
    pub after: Value,
    pub created_by: Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl ChangeRow {
    fn json(&self) -> Value {
        json!({
            "change_id": self.change_id,
            "position": self.position,
            "entity": self.entity,
            "entity_key": self.entity_key,
            "op": self.op,
            "base_row_version": self.base_row_version,
            "before": self.before,
            "after": self.after,
            "created_by": self.created_by,
            "created_at": self.created_at,
        })
    }
}

pub async fn load_changes(
    conn: &mut PgConnection,
    change_set_id: Uuid,
) -> EditorResult<Vec<ChangeRow>> {
    let rows = sqlx::query(
        "SELECT change_id, position, entity, entity_key, op, base_row_version, \
                before::text AS before, after::text AS after, created_by, created_at \
         FROM gtfs_change WHERE change_set_id = $1 ORDER BY position",
    )
    .bind(change_set_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| -> Result<ChangeRow, sqlx::Error> {
            Ok(ChangeRow {
                change_id: r.try_get("change_id")?,
                position: r.try_get("position")?,
                entity: r.try_get("entity")?,
                entity_key: r.try_get("entity_key")?,
                op: r.try_get("op")?,
                base_row_version: r.try_get("base_row_version")?,
                before: json_col(r, "before")?,
                after: json_col(r, "after")?,
                created_by: r.try_get("created_by")?,
                created_at: r.try_get("created_at")?,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[derive(Debug, Clone)]
pub struct ChangeSet {
    pub change_set_id: Uuid,
    pub gtfs_id: String,
    pub status: String,
    pub created_by: Uuid,
    pub submitted_by: Option<Uuid>,
    /// Approved by the admin who submitted it (docs/gtfs-editor.md section 2).
    pub self_approved: bool,
    pub json: Value,
}

const SET_SELECT: &str = "SELECT cs.change_set_id, cs.gtfs_id, cs.title, cs.description, cs.status, \
        cs.created_by, cu.email AS created_by_email, cs.created_at, cs.updated_at, \
        cs.submitted_by, su.email AS submitted_by_email, cs.submitted_at, \
        cs.reviewed_by, ru.email AS reviewed_by_email, cs.reviewed_at, cs.review_comment, \
        cs.self_approved, \
        cs.committed_by, mu.email AS committed_by_email, cs.committed_at, \
        cs.base_version, cs.committed_version, \
        (SELECT count(*) FROM gtfs_change c WHERE c.change_set_id = cs.change_set_id) AS change_count \
     FROM gtfs_change_set cs \
     LEFT JOIN gtfs_editor_user cu ON cu.user_id = cs.created_by \
     LEFT JOIN gtfs_editor_user su ON su.user_id = cs.submitted_by \
     LEFT JOIN gtfs_editor_user ru ON ru.user_id = cs.reviewed_by \
     LEFT JOIN gtfs_editor_user mu ON mu.user_id = cs.committed_by";

fn set_from_row(r: &PgRow) -> Result<ChangeSet, sqlx::Error> {
    type Ts = Option<chrono::DateTime<chrono::Utc>>;
    let json = json!({
        "change_set_id": r.try_get::<Uuid, _>("change_set_id")?,
        "gtfs_id": r.try_get::<String, _>("gtfs_id")?,
        "title": r.try_get::<String, _>("title")?,
        "description": r.try_get::<Option<String>, _>("description")?,
        "status": r.try_get::<String, _>("status")?,
        "created_by": r.try_get::<Uuid, _>("created_by")?,
        "created_by_email": r.try_get::<Option<String>, _>("created_by_email")?,
        "created_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?,
        "updated_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?,
        "submitted_by": r.try_get::<Option<Uuid>, _>("submitted_by")?,
        "submitted_by_email": r.try_get::<Option<String>, _>("submitted_by_email")?,
        "submitted_at": r.try_get::<Ts, _>("submitted_at")?,
        "reviewed_by": r.try_get::<Option<Uuid>, _>("reviewed_by")?,
        "reviewed_by_email": r.try_get::<Option<String>, _>("reviewed_by_email")?,
        "reviewed_at": r.try_get::<Ts, _>("reviewed_at")?,
        "review_comment": r.try_get::<Option<String>, _>("review_comment")?,
        "self_approved": r.try_get::<bool, _>("self_approved")?,
        "committed_by": r.try_get::<Option<Uuid>, _>("committed_by")?,
        "committed_by_email": r.try_get::<Option<String>, _>("committed_by_email")?,
        "committed_at": r.try_get::<Ts, _>("committed_at")?,
        "base_version": r.try_get::<i64, _>("base_version")?,
        "committed_version": r.try_get::<Option<i64>, _>("committed_version")?,
        "change_count": r.try_get::<i64, _>("change_count")?,
    });
    Ok(ChangeSet {
        change_set_id: r.try_get("change_set_id")?,
        gtfs_id: r.try_get("gtfs_id")?,
        status: r.try_get("status")?,
        created_by: r.try_get("created_by")?,
        submitted_by: r.try_get("submitted_by")?,
        self_approved: r.try_get("self_approved")?,
        json,
    })
}

pub async fn load_set(conn: &mut PgConnection, id: Uuid, lock: bool) -> EditorResult<ChangeSet> {
    if lock {
        sqlx::query("SELECT 1 FROM gtfs_change_set WHERE change_set_id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    }
    let row = sqlx::query(&format!("{SET_SELECT} WHERE cs.change_set_id = $1"))
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| EditorError::not_found("change_set_not_found", "no such change set"))?;
    Ok(set_from_row(&row)?)
}

pub async fn list_sets(
    state: &EditorState,
    gtfs_id: &str,
    status: Option<&str>,
    page: &Page,
) -> EditorResult<Value> {
    let rows = sqlx::query(&format!(
        "{SET_SELECT} WHERE cs.gtfs_id = $1 \
           AND ($2::text IS NULL OR cs.status = ANY(string_to_array($2, ','))) \
         ORDER BY cs.updated_at DESC, cs.change_set_id LIMIT $3 OFFSET $4"
    ))
    .bind(gtfs_id)
    .bind(status)
    .bind(page.limit + 1)
    .bind(page.offset)
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| set_from_row(r).map(|s| s.json))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(page.wrap(items))
}

pub async fn create_set(
    state: &EditorState,
    ctx: &Ctx,
    gtfs_id: &str,
    title: &str,
    description: Option<&str>,
) -> EditorResult<Value> {
    if title.trim().is_empty() {
        return Err(EditorError::bad_request(
            "title_required",
            "give the change set a title",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let version = feed_version(&mut tx, gtfs_id).await?;
    let id: Uuid = sqlx::query(
        "INSERT INTO gtfs_change_set (gtfs_id, title, description, created_by, base_version) \
         VALUES ($1, $2, $3, $4, $5) RETURNING change_set_id",
    )
    .bind(gtfs_id)
    .bind(title.trim())
    .bind(description)
    .bind(ctx.user.user_id)
    .bind(version)
    .fetch_one(&mut *tx)
    .await?
    .try_get("change_set_id")?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "change_set_created",
        Some(gtfs_id),
        Some(id),
        json!({"title": title.trim()}),
    )
    .await?;
    let set = load_set(&mut tx, id, false).await?;
    tx.commit().await?;
    Ok(set.json)
}

// ---------------------------------------------------------------- new rows

/// `n` stop ids nobody uses: not a row of the feed (deleted rows included), and
/// not the id of a stop or station an open draft of the feed creates.
pub async fn mint_stop_ids(
    conn: &mut PgConnection,
    gtfs_id: &str,
    n: usize,
) -> EditorResult<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(n);
    let mut seen: HashSet<String> = HashSet::with_capacity(n);
    for _ in 0..8 {
        let need = n - out.len();
        if need == 0 {
            break;
        }
        let candidates: Vec<String> = (0..need)
            .map(|_| mint_stop_id())
            .filter(|c| seen.insert(c.clone()))
            .collect();
        let used: HashSet<String> = sqlx::query(
            "SELECT c.id FROM UNNEST($2::text[]) AS c(id) \
             WHERE EXISTS (SELECT 1 FROM gtfs_stop s WHERE s.gtfs_id = $1 AND s.stop_id = c.id) \
                OR EXISTS (SELECT 1 FROM gtfs_change ch \
                           JOIN gtfs_change_set cs ON cs.change_set_id = ch.change_set_id \
                           WHERE cs.gtfs_id = $1 AND cs.status NOT IN ('committed', 'discarded') \
                             AND ch.entity IN ('stop', 'station') AND ch.op = 'create' \
                             AND ch.entity_key = c.id)",
        )
        .bind(gtfs_id)
        .bind(&candidates)
        .fetch_all(&mut *conn)
        .await?
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<_, _>>()?;
        out.extend(candidates.into_iter().filter(|c| !used.contains(c)));
    }
    if out.len() < n {
        return Err(EditorError::internal("could not mint unused stop ids"));
    }
    Ok(out)
}

/// The agency most of the feed's routes belong to: a new route's default.
pub async fn usual_agency(conn: &mut PgConnection, gtfs_id: &str) -> EditorResult<Option<String>> {
    Ok(sqlx::query(
        "SELECT agency_id FROM gtfs_route WHERE gtfs_id = $1 AND agency_id IS NOT NULL \
         GROUP BY agency_id ORDER BY count(*) DESC, agency_id LIMIT 1",
    )
    .bind(gtfs_id)
    .fetch_optional(&mut *conn)
    .await?
    .map(|r| r.try_get("agency_id"))
    .transpose()?)
}

/// A change to append to a set.
pub struct ChangeInsert {
    pub entity: String,
    pub op: String,
    pub entity_key: String,
    pub base_row_version: Option<i32>,
    pub before: Value,
    pub after: Value,
}

/// Append changes after the set's last position, in order, in one statement.
/// The caller holds the set's row lock. Returns the new change ids in order.
pub async fn insert_changes(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    created_by: Uuid,
    changes: &[ChangeInsert],
) -> EditorResult<Vec<i64>> {
    if changes.is_empty() {
        return Ok(vec![]);
    }
    let last: i32 = sqlx::query(
        "SELECT coalesce(max(position), 0) AS last FROM gtfs_change WHERE change_set_id = $1",
    )
    .bind(change_set_id)
    .fetch_one(&mut *conn)
    .await?
    .try_get("last")?;
    let positions: Vec<i32> = (1..=changes.len() as i32).map(|i| last + i).collect();
    let json_text = |v: &Value| (!v.is_null()).then(|| v.to_string());
    let rows = sqlx::query(
        "INSERT INTO gtfs_change (change_set_id, position, entity, entity_key, op, base_row_version, \
                                  before, after, created_by) \
         SELECT $1, u.pos, u.entity, u.key, u.op, u.base, u.before::jsonb, u.after::jsonb, $2 \
         FROM UNNEST($3::int4[], $4::text[], $5::text[], $6::text[], $7::int4[], $8::text[], $9::text[]) \
              AS u(pos, entity, key, op, base, before, after) \
         RETURNING change_id, position",
    )
    .bind(change_set_id)
    .bind(created_by)
    .bind(&positions)
    .bind(changes.iter().map(|c| c.entity.clone()).collect::<Vec<_>>())
    .bind(changes.iter().map(|c| c.entity_key.clone()).collect::<Vec<_>>())
    .bind(changes.iter().map(|c| c.op.clone()).collect::<Vec<_>>())
    .bind(changes.iter().map(|c| c.base_row_version).collect::<Vec<_>>())
    .bind(changes.iter().map(|c| json_text(&c.before)).collect::<Vec<_>>())
    .bind(changes.iter().map(|c| json_text(&c.after)).collect::<Vec<_>>())
    .fetch_all(&mut *conn)
    .await?;
    let by_position: HashMap<i32, i64> = rows
        .iter()
        .map(|r| -> Result<(i32, i64), sqlx::Error> {
            Ok((r.try_get("position")?, r.try_get("change_id")?))
        })
        .collect::<Result<_, _>>()?;
    positions
        .iter()
        .map(|p| {
            by_position
                .get(p)
                .copied()
                .ok_or_else(|| EditorError::internal("a change was not stored"))
        })
        .collect()
}

// ---------------------------------------------------------------- evaluate

#[derive(Debug, Default)]
pub struct Evaluation {
    pub validation: Vec<Value>,
    pub conflicts: Vec<Value>,
    /// One entry per stop merge that applied: `{change_id, from, into, routes,
    /// rows, keep_name, keep_position}`; commit audits them.
    pub merges: Vec<Value>,
    /// One entry per `feed_config` change that switched the data source:
    /// `{gtfs_id, from, to, change_id}`; commit audits them.
    pub feed_configs: Vec<Value>,
}

impl Evaluation {
    pub fn has_errors(&self) -> bool {
        self.validation.iter().any(|v| v["level"] == "error")
    }
}

enum ApplyError {
    Findings(Vec<Finding>),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for ApplyError {
    fn from(e: sqlx::Error) -> Self {
        ApplyError::Db(e)
    }
}

impl From<EditorError> for ApplyError {
    fn from(e: EditorError) -> Self {
        ApplyError::Findings(vec![Finding::error(e.code, "", e.message)])
    }
}

fn fail(code: &str, message: impl Into<String>) -> ApplyError {
    ApplyError::Findings(vec![Finding::error(code, "", message)])
}

fn finding_json(change_id: Option<i64>, f: &Finding) -> Value {
    json!({"change_id": change_id, "level": f.level, "code": f.code, "message": f.message})
}

/// What a database error inside a change's savepoint means: a finding against
/// the change when the database refused it (a constraint, a bad value); or, for
/// a deadlock or serialization failure, which says nothing about the change,
/// the error itself, 503 `try_again`, so the whole transaction is retried and
/// the change is never blamed.
fn db_failure(e: sqlx::Error) -> Result<Finding, EditorError> {
    if is_transient(&e) {
        return Err(e.into());
    }
    Ok(db_finding(&e))
}

fn db_finding(e: &sqlx::Error) -> Finding {
    match e.as_database_error() {
        Some(d) => {
            let code = match d.code().as_deref() {
                Some("23503") => "foreign_key_violation",
                Some("23505") => "unique_violation",
                Some("23514") => "check_violation",
                Some("23502") => "not_null_violation",
                _ => "database_rejected",
            };
            Finding::error(code, "", d.message().to_string())
        }
        None => {
            tracing::error!(tag = "[GTFS EDITOR APPLY]", error = %e);
            Finding::error("database_rejected", "", "the database refused this change")
        }
    }
}

fn field<'a>(m: &'a Map<String, Value>, key: &str) -> (bool, Option<&'a str>) {
    match m.get(key) {
        None => (false, None),
        Some(v) => (true, v.as_str()),
    }
}

/// A text a passenger reads (platform label, description): trimmed, and blank
/// clears it like `null` does.
fn text_field<'a>(m: &'a Map<String, Value>, key: &str) -> (bool, Option<&'a str>) {
    let (has, text) = field(m, key);
    (has, text.map(str::trim).filter(|t| !t.is_empty()))
}

fn conflict(c: &ChangeRow, reason: &str, expected: Value, actual: Value) -> Value {
    conflict_on(c, &c.entity_key, reason, expected, actual)
}

/// A conflict on `key`, which is the change's own row or - for a merge - the
/// stop it merges into.
fn conflict_on(c: &ChangeRow, key: &str, reason: &str, expected: Value, actual: Value) -> Value {
    let what = match c.entity.as_str() {
        "route_stops" => format!("The stop list of route {key}"),
        "route" => format!("Route {key}"),
        "station" => format!("Station {key}"),
        "feed_config" => format!("The data source of feed {key}"),
        _ if key != c.entity_key => format!("Stop {key} (kept by the merge of {})", c.entity_key),
        _ => format!("Stop {key}"),
    };
    let message = match reason {
        "missing" => format!("{what} no longer exists."),
        "exists" => format!("{what} was created by another commit after this edit was made."),
        _ => format!("{what} was changed by another commit after this edit was made."),
    };
    json!({
        "change_id": c.change_id, "entity": c.entity, "entity_key": key,
        "reason": reason, "expected": expected, "actual": actual, "message": message,
    })
}

async fn live_row_version(
    conn: &mut PgConnection,
    table: &str,
    id_col: &str,
    g: &str,
    key: &str,
) -> EditorResult<Option<i32>> {
    Ok(sqlx::query(&format!(
        "SELECT row_version FROM {table} WHERE gtfs_id = $1 AND {id_col} = $2"
    ))
    .bind(g)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?
    .map(|r| r.try_get("row_version"))
    .transpose()?)
}

fn version_conflict(
    c: &ChangeRow,
    key: &str,
    live: Option<i32>,
    base: Option<i32>,
) -> Option<Value> {
    match (live, base) {
        (None, _) => Some(conflict_on(c, key, "missing", json!(base), Value::Null)),
        (Some(v), Some(b)) if v != b => Some(conflict_on(c, key, "changed", json!(b), json!(v))),
        _ => None,
    }
}

/// Has the live data moved on since the change was made? `in_draft` says
/// whether an earlier change in the same set creates a row (`true` = a route):
/// such a row has no live version to be stale against.
async fn conflict_for(
    conn: &mut PgConnection,
    g: &str,
    c: &ChangeRow,
    in_draft: &dyn Fn(bool, &str) -> bool,
) -> EditorResult<Vec<Value>> {
    let key = c.entity_key.as_str();
    let mut out = Vec::new();
    match (c.entity.as_str(), c.op.as_str()) {
        ("stop", "update" | "delete") | ("station", "update" | "delete")
            if !in_draft(false, key) =>
        {
            let live = live_row_version(conn, "gtfs_stop", "stop_id", g, key).await?;
            out.extend(version_conflict(c, key, live, c.base_row_version));
        }
        ("route", "update" | "delete") if !in_draft(true, key) => {
            let live = live_row_version(conn, "gtfs_route", "route_id", g, key).await?;
            out.extend(version_conflict(c, key, live, c.base_row_version));
        }
        ("stop", "merge") => {
            if !in_draft(false, key) {
                let live = live_row_version(conn, "gtfs_stop", "stop_id", g, key).await?;
                out.extend(version_conflict(c, key, live, c.base_row_version));
            }
            if let Some(into) = c.after["into_stop_id"].as_str().map(str::trim) {
                if !in_draft(false, into) {
                    let base = c.after["into_row_version"].as_i64().map(|v| v as i32);
                    let live = live_row_version(conn, "gtfs_stop", "stop_id", g, into).await?;
                    out.extend(version_conflict(c, into, live, base));
                }
            }
        }
        ("stop" | "station" | "route", "create") => {
            let (table, id_col) = if c.entity == "route" {
                ("gtfs_route", "route_id")
            } else {
                ("gtfs_stop", "stop_id")
            };
            let exists = live_row_version(conn, table, id_col, g, key).await?;
            if exists.is_some() {
                out.push(conflict(c, "exists", Value::Null, json!(key)));
            }
        }
        ("feed_config", "update") => {
            // the feed's version moves with every commit, so the base is the
            // value itself: the data source the change was made against
            let expected = c.before["data_source"].as_str().unwrap_or("").to_string();
            let actual: Option<String> =
                sqlx::query("SELECT data_source FROM gtfs_feed WHERE gtfs_id = $1")
                    .bind(g)
                    .fetch_optional(&mut *conn)
                    .await?
                    .map(|r| r.try_get("data_source"))
                    .transpose()?;
            match actual {
                None => out.push(conflict(c, "missing", json!(expected), Value::Null)),
                Some(actual) if actual != expected => {
                    out.push(conflict(c, "changed", json!(expected), json!(actual)))
                }
                Some(_) => {}
            }
        }
        ("route_stops", "replace") => {
            // a route created in the draft has no rows: its base is the hash of []
            let expected = c.after["base_rows_hash"].as_str().unwrap_or("").to_string();
            let actual = rows_hash(&load_route_rows(conn, g, key).await?);
            if expected != actual {
                out.push(conflict(c, "changed", json!(expected), json!(actual)));
            }
        }
        _ => {}
    }
    Ok(out)
}

/// What applying one change needs to know about the rest of the set.
struct ApplyState<'a> {
    changes: &'a [ChangeRow],
    /// stop merged away by an applied change -> (the stop kept, that change)
    merged_away: HashMap<String, (String, i64)>,
    merges: Vec<Value>,
    feed_configs: Vec<Value>,
}

/// Apply every change in order inside `conn`'s transaction; see the module doc.
pub async fn evaluate(
    conn: &mut PgConnection,
    g: &str,
    changes: &[ChangeRow],
    actor: &str,
) -> EditorResult<Evaluation> {
    let mut ev = Evaluation::default();
    let mut created: HashSet<(bool, String)> = HashSet::new();
    for c in changes {
        let found = conflict_for(conn, g, c, &|routes, key| {
            created.contains(&(routes, key.to_string()))
        })
        .await?;
        ev.conflicts.extend(found);
        if c.op == "create" {
            created.insert((c.entity == "route", c.entity_key.clone()));
        }
    }
    let mut state = ApplyState {
        changes,
        merged_away: HashMap::new(),
        merges: Vec::new(),
        feed_configs: Vec::new(),
    };
    for c in changes {
        sqlx::query("SAVEPOINT editor_change")
            .execute(&mut *conn)
            .await?;
        match apply_change(conn, g, c, actor, &mut state).await {
            Ok(warnings) => {
                sqlx::query("RELEASE SAVEPOINT editor_change")
                    .execute(&mut *conn)
                    .await?;
                ev.validation
                    .extend(warnings.iter().map(|f| finding_json(Some(c.change_id), f)));
            }
            Err(err) => {
                sqlx::query("ROLLBACK TO SAVEPOINT editor_change")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("RELEASE SAVEPOINT editor_change")
                    .execute(&mut *conn)
                    .await?;
                let findings = match err {
                    ApplyError::Findings(f) => f,
                    ApplyError::Db(e) => vec![db_failure(e)?],
                };
                ev.validation
                    .extend(findings.iter().map(|f| finding_json(Some(c.change_id), f)));
            }
        }
    }
    ev.merges = state.merges;
    ev.feed_configs = state.feed_configs;
    // parent_station is DEFERRABLE: check it now rather than at COMMIT.
    sqlx::query("SAVEPOINT editor_constraints")
        .execute(&mut *conn)
        .await?;
    match sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *conn)
        .await
    {
        Ok(_) => {
            sqlx::query("SET CONSTRAINTS ALL DEFERRED")
                .execute(&mut *conn)
                .await?;
            sqlx::query("RELEASE SAVEPOINT editor_constraints")
                .execute(&mut *conn)
                .await?;
        }
        Err(e) => {
            sqlx::query("ROLLBACK TO SAVEPOINT editor_constraints")
                .execute(&mut *conn)
                .await?;
            ev.validation.push(finding_json(None, &db_failure(e)?));
        }
    }
    Ok(ev)
}

/// What [`evaluate`] would find for one more change, `after` on row `key`,
/// applied after `changes`: that change's findings as `{level, code, message}`,
/// in a savepoint that is rolled back. The caller is in a transaction. This is
/// how a merge is asked about before it is drafted (section 8.2) - the same
/// apply, so the same answer the draft would give once the change is in it.
pub async fn findings_for(
    conn: &mut PgConnection,
    g: &str,
    changes: &[ChangeRow],
    (entity, op, key): (&str, &str, &str),
    after: &Value,
    actor: &str,
) -> EditorResult<Vec<Value>> {
    const PROBE: i64 = 0;
    let mut all = changes.to_vec();
    all.push(ChangeRow {
        change_id: PROBE,
        position: i32::MAX,
        entity: entity.into(),
        entity_key: key.into(),
        op: op.into(),
        base_row_version: None,
        before: Value::Null,
        after: after.clone(),
        created_by: Uuid::nil(),
        created_at: chrono::Utc::now(),
    });
    sqlx::query("SAVEPOINT editor_probe")
        .execute(&mut *conn)
        .await?;
    let ev = evaluate(conn, g, &all, actor).await;
    sqlx::query("ROLLBACK TO SAVEPOINT editor_probe")
        .execute(&mut *conn)
        .await?;
    sqlx::query("RELEASE SAVEPOINT editor_probe")
        .execute(&mut *conn)
        .await?;
    Ok(ev?
        .validation
        .into_iter()
        .filter(|v| v["change_id"] == PROBE)
        .map(|v| json!({"level": v["level"], "code": v["code"], "message": v["message"]}))
        .collect())
}

/// Every stop id a change uses (not the rows it creates).
fn referenced_stops(c: &ChangeRow) -> Vec<String> {
    let mut ids: Vec<String> = match (c.entity.as_str(), c.op.as_str()) {
        ("stop", "update" | "delete") => vec![c.entity_key.clone()],
        ("stop", "merge") => {
            let mut v = vec![c.entity_key.clone()];
            v.extend(
                c.after["into_stop_id"]
                    .as_str()
                    .map(|s| s.trim().to_string()),
            );
            v
        }
        ("station", "create" | "update") => c
            .after
            .as_object()
            .and_then(station_members)
            .map(|m| m.into_iter().map(|m| m.stop_id).collect())
            .unwrap_or_default(),
        ("route_stops", "replace") => c.after["rows"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|r| r["stop_type"] != "ROUTE CORRECTION")
                    .filter_map(|r| r["stop_id"].as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    };
    ids.sort();
    ids.dedup();
    ids
}

async fn apply_change(
    conn: &mut PgConnection,
    g: &str,
    c: &ChangeRow,
    actor: &str,
    state: &mut ApplyState<'_>,
) -> Result<Vec<Finding>, ApplyError> {
    check_payload(&c.entity, &c.op, &c.entity_key, &c.after)
        .map_err(|f| ApplyError::Findings(vec![f]))?;
    // a stop an earlier change merged away is gone for every later change
    let gone: Vec<Finding> = referenced_stops(c)
        .iter()
        .filter_map(|id| {
            let (into, by) = state.merged_away.get(id)?;
            Some(Finding::error(
                "stop_merged_away",
                id.as_str(),
                format!("stop {id} is merged into {into} by change {by} earlier in this draft; use {into}"),
            ))
        })
        .collect();
    if !gone.is_empty() {
        return Err(ApplyError::Findings(gone));
    }
    let key = c.entity_key.as_str();
    match (c.entity.as_str(), c.op.as_str()) {
        ("stop", "update") => stop_update(conn, g, key, &c.after, actor).await,
        ("stop", "create") => stop_create(conn, g, &c.after, actor).await,
        ("stop", "delete") => stop_delete(conn, g, key, actor).await,
        ("stop", "merge") => stop_merge(conn, g, c, actor, state).await,
        ("route", "create") => route_create(conn, g, &c.after, actor).await,
        ("route", "update") => route_update(conn, g, key, &c.after, actor).await,
        ("route", "delete") => route_delete(conn, g, c, actor, state.changes).await,
        ("route_stops", "replace") => route_stops_replace(conn, g, key, &c.after, actor).await,
        ("station", "create") => station_create(conn, g, &c.after, actor).await,
        ("station", "update") => station_update(conn, g, key, &c.after, actor).await,
        ("station", "delete") => station_delete(conn, g, key, actor).await,
        ("feed_config", "update") => feed_config_update(conn, g, c, state).await,
        (e, o) => Err(fail(
            "invalid_change",
            format!("unsupported change {e}/{o}"),
        )),
    }
}

struct LiveStop {
    lat: f64,
    lon: f64,
    location_type: i16,
    deleted: bool,
    parent_station: Option<String>,
}

async fn live_stop(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
) -> Result<Option<LiveStop>, sqlx::Error> {
    sqlx::query(
        "SELECT lat, lon, location_type, deleted, parent_station FROM gtfs_stop \
         WHERE gtfs_id = $1 AND stop_id = $2 FOR UPDATE",
    )
    .bind(g)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .map(|r| -> Result<LiveStop, sqlx::Error> {
        Ok(LiveStop {
            lat: r.try_get("lat")?,
            lon: r.try_get("lon")?,
            location_type: r.try_get("location_type")?,
            deleted: r.try_get("deleted")?,
            parent_station: r.try_get("parent_station")?,
        })
    })
    .transpose()
}

async fn stop_update(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let live = live_stop(conn, g, id)
        .await?
        .ok_or_else(|| fail("stop_not_found", format!("no stop {id}")))?;
    if live.deleted {
        return Err(fail("stop_deleted", format!("stop {id} is deleted")));
    }
    if live.location_type == 1 {
        return Err(fail(
            "use_station_change",
            format!("{id} is a station; edit it as a station"),
        ));
    }
    let m = after.as_object().expect("payload checked");
    let mut warnings = Vec::new();
    let moved = m.contains_key("lat");
    let (lat, lon) = (
        m.get("lat").and_then(Value::as_f64).unwrap_or(live.lat),
        m.get("lon").and_then(Value::as_f64).unwrap_or(live.lon),
    );
    if moved {
        let d = haversine_m(live.lat, live.lon, lat, lon);
        if d > MOVE_WARNING_METRES {
            warnings.push(Finding::warning(
                "stop_moved_far",
                id,
                format!("stop {id} moves {:.0} m; check it is the same place", d),
            ));
        }
    }
    let (has_name, name) = field(m, "name");
    let (has_platform, platform) = text_field(m, "platform_code");
    let (has_description, description) = text_field(m, "description");
    let (has_cluster, cluster) = field(m, "cluster_id");
    let (has_regional, regional) = field(m, "regional_name");
    let (has_hindi, hindi) = field(m, "hindi_name");
    sqlx::query(
        "UPDATE gtfs_stop SET \
            name = CASE WHEN $3 THEN $4 ELSE name END, \
            lat = CASE WHEN $5 THEN $6 ELSE lat END, \
            lon = CASE WHEN $5 THEN $7 ELSE lon END, \
            platform_code = CASE WHEN $8 THEN $9 ELSE platform_code END, \
            cluster_id = CASE WHEN $10 THEN $11 ELSE cluster_id END, \
            info_json = CASE WHEN NOT $10 THEN info_json \
                             WHEN $11::text IS NULL THEN coalesce(info_json, '{}'::jsonb) - 'clusterId' \
                             ELSE jsonb_set(coalesce(info_json, '{}'::jsonb), '{clusterId}', to_jsonb($11::text)) END, \
            regional_name = CASE WHEN $12 THEN $13 ELSE regional_name END, \
            hindi_name = CASE WHEN $14 THEN $15 ELSE hindi_name END, \
            description = CASE WHEN $17 THEN $18 ELSE description END, \
            updated_by = $16 \
         WHERE gtfs_id = $1 AND stop_id = $2",
    )
    .bind(g)
    .bind(id)
    .bind(has_name)
    .bind(name.map(str::trim))
    .bind(moved)
    .bind(lat)
    .bind(lon)
    .bind(has_platform)
    .bind(platform)
    .bind(has_cluster)
    .bind(cluster)
    .bind(has_regional)
    .bind(regional)
    .bind(has_hindi)
    .bind(hindi)
    .bind(actor)
    .bind(has_description)
    .bind(description)
    .execute(&mut *conn)
    .await?;
    Ok(warnings)
}

async fn stop_create(
    conn: &mut PgConnection,
    g: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let m = after.as_object().expect("payload checked");
    let s = |k: &str| m.get(k).and_then(Value::as_str);
    let created = sqlx::query(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, platform_code, cluster_id, \
                                info_json, regional_name, hindi_name, position_source, updated_by, \
                                description) \
         VALUES ($1, $2, coalesce($3, $2), $4, $5, $6, $7, $8, \
                 CASE WHEN $8::text IS NULL THEN NULL ELSE jsonb_build_object('clusterId', $8::text) END, \
                 $9, $10, 'REVIEW', $11, $12) \
         ON CONFLICT DO NOTHING RETURNING stop_id",
    )
    .bind(g)
    .bind(s("stop_id"))
    .bind(s("stop_code"))
    .bind(s("name").map(str::trim))
    .bind(m.get("lat").and_then(Value::as_f64))
    .bind(m.get("lon").and_then(Value::as_f64))
    .bind(text_field(m, "platform_code").1)
    .bind(s("cluster_id"))
    .bind(s("regional_name"))
    .bind(s("hindi_name"))
    .bind(actor)
    .bind(text_field(m, "description").1)
    .fetch_optional(&mut *conn)
    .await?;
    if created.is_none() {
        return Err(fail(
            "stop_exists",
            format!("stop {} already exists", s("stop_id").unwrap_or("")),
        ));
    }
    Ok(vec![])
}

async fn stop_delete(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let live = live_stop(conn, g, id)
        .await?
        .ok_or_else(|| fail("stop_not_found", format!("no stop {id}")))?;
    if live.deleted {
        return Err(fail(
            "stop_deleted",
            format!("stop {id} is already deleted"),
        ));
    }
    if live.location_type == 1 {
        return Err(fail(
            "use_station_change",
            format!("{id} is a station; delete it as a station"),
        ));
    }
    let used: Vec<String> = sqlx::query(
        // a deleted route's rows stay, but nothing serves them
        "SELECT DISTINCT rs.route_id FROM gtfs_route_stop rs \
         JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
         WHERE rs.gtfs_id = $1 AND rs.stop_id = $2 ORDER BY rs.route_id",
    )
    .bind(g)
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| r.try_get("route_id"))
    .collect::<Result<_, _>>()?;
    if !used.is_empty() {
        let sample: Vec<&str> = used.iter().take(5).map(String::as_str).collect();
        return Err(fail(
            "stop_in_use",
            format!(
                "stop {id} is on {} route(s) ({}{}); take it off them first",
                used.len(),
                sample.join(", "),
                if used.len() > 5 { ", ..." } else { "" }
            ),
        ));
    }
    sqlx::query(
        "UPDATE gtfs_stop SET deleted = true, parent_station = NULL, updated_by = $3 \
         WHERE gtfs_id = $1 AND stop_id = $2",
    )
    .bind(g)
    .bind(id)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(vec![])
}

async fn route_update(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let deleted: bool = sqlx::query(
        "SELECT deleted FROM gtfs_route WHERE gtfs_id = $1 AND route_id = $2 FOR UPDATE",
    )
    .bind(g)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| fail("route_not_found", format!("no route {id}")))?
    .try_get("deleted")?;
    if deleted {
        return Err(fail("route_deleted", format!("route {id} is deleted")));
    }
    let m = after.as_object().expect("payload checked");
    let (has_short, short) = field(m, "short_name");
    let (has_long, long) = field(m, "long_name");
    let (has_color, color) = field(m, "color");
    let (has_text, text) = field(m, "text_color");
    let (has_poly, poly) = field(m, "encoded_polyline");
    let (has_src, src) = field(m, "polyline_source");
    sqlx::query(
        "UPDATE gtfs_route SET \
            short_name = CASE WHEN $3 THEN $4 ELSE short_name END, \
            long_name = CASE WHEN $5 THEN $6 ELSE long_name END, \
            color = CASE WHEN $7 THEN upper($8) ELSE color END, \
            text_color = CASE WHEN $9 THEN upper($10) ELSE text_color END, \
            encoded_polyline = CASE WHEN $11 THEN $12 ELSE encoded_polyline END, \
            polyline_source = CASE WHEN $13 THEN $14 \
                                   WHEN $11 AND $12::text IS NULL THEN NULL \
                                   WHEN $11 THEN 'manual' ELSE polyline_source END, \
            updated_by = $15 \
         WHERE gtfs_id = $1 AND route_id = $2",
    )
    .bind(g)
    .bind(id)
    .bind(has_short)
    .bind(short)
    .bind(has_long)
    .bind(long)
    .bind(has_color)
    .bind(color)
    .bind(has_text)
    .bind(text)
    .bind(has_poly)
    .bind(poly)
    .bind(has_src)
    .bind(src)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(vec![])
}

async fn route_create(
    conn: &mut PgConnection,
    g: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let m = after.as_object().expect("payload checked");
    let s = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let id = s("route_id").unwrap_or("");
    let created = sqlx::query(
        "INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type, agency_id, color, updated_by) \
         VALUES ($1, $2, $3, $4, coalesce($5, 3), \
                 coalesce($6, (SELECT agency_id FROM gtfs_route WHERE gtfs_id = $1 AND agency_id IS NOT NULL \
                               GROUP BY agency_id ORDER BY count(*) DESC, agency_id LIMIT 1)), \
                 upper($7), $8) \
         ON CONFLICT DO NOTHING RETURNING route_id",
    )
    .bind(g)
    .bind(id)
    .bind(s("short_name"))
    .bind(s("long_name"))
    .bind(m.get("route_type").and_then(Value::as_i64).map(|t| t as i16))
    .bind(s("agency_id"))
    .bind(s("color"))
    .bind(actor)
    .fetch_optional(&mut *conn)
    .await?;
    if created.is_none() {
        return Err(fail("route_exists", format!("route {id} already exists")));
    }
    Ok(vec![])
}

/// Soft delete: the route and its rows stay, marked deleted, and GIMS stops
/// serving it. Refused while another change in the set edits the route.
async fn route_delete(
    conn: &mut PgConnection,
    g: &str,
    c: &ChangeRow,
    actor: &str,
    changes: &[ChangeRow],
) -> Result<Vec<Finding>, ApplyError> {
    let id = c.entity_key.as_str();
    let deleted: bool = sqlx::query(
        "SELECT deleted FROM gtfs_route WHERE gtfs_id = $1 AND route_id = $2 FOR UPDATE",
    )
    .bind(g)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| fail("route_not_found", format!("no route {id}")))?
    .try_get("deleted")?;
    if deleted {
        return Err(fail(
            "route_deleted",
            format!("route {id} is already deleted"),
        ));
    }
    let others = other_route_changes(changes, id, c.change_id);
    if !others.is_empty() {
        return Err(fail(
            "route_has_pending_changes",
            format!(
                "route {id} is also edited by change(s) {} in this set; remove those or the delete",
                others
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    sqlx::query(
        "UPDATE gtfs_route SET deleted = true, updated_by = $3 WHERE gtfs_id = $1 AND route_id = $2",
    )
    .bind(g)
    .bind(id)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(vec![])
}

/// Changes in a set, other than `except`, that create or edit a route or its
/// stop list.
fn other_route_changes(changes: &[ChangeRow], route_id: &str, except: i64) -> Vec<i64> {
    changes
        .iter()
        .filter(|o| o.change_id != except && o.entity_key == route_id)
        .filter(|o| matches!(o.entity.as_str(), "route" | "route_stops"))
        .map(|o| o.change_id)
        .collect()
}

struct MergeStop {
    name: String,
    lat: f64,
    lon: f64,
    location_type: i16,
    deleted: bool,
    parent_station: Option<String>,
    platform_code: Option<String>,
}

/// Merge a duplicate stop (`entity_key`) into the stop that stays: every route
/// row moves to the kept id, the kept stop optionally takes the other's name or
/// position and its station, and the duplicate is soft-deleted.
async fn stop_merge(
    conn: &mut PgConnection,
    g: &str,
    c: &ChangeRow,
    actor: &str,
    state: &mut ApplyState<'_>,
) -> Result<Vec<Finding>, ApplyError> {
    let from = c.entity_key.as_str();
    let m = c.after.as_object().expect("payload checked");
    let into = m["into_stop_id"].as_str().expect("payload checked").trim();
    let keep_name_from = m.get("keep_name").and_then(Value::as_str) == Some("from");
    let keep_position_from = m.get("keep_position").and_then(Value::as_str) == Some("from");

    // both rows, locked in id order
    let found: HashMap<String, MergeStop> = sqlx::query(
        "SELECT stop_id, name, lat, lon, location_type, deleted, parent_station, platform_code \
         FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = ANY($2) ORDER BY stop_id FOR UPDATE",
    )
    .bind(g)
    .bind(vec![from.to_string(), into.to_string()])
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(String, MergeStop), sqlx::Error> {
        Ok((
            r.try_get("stop_id")?,
            MergeStop {
                name: r.try_get("name")?,
                lat: r.try_get("lat")?,
                lon: r.try_get("lon")?,
                location_type: r.try_get("location_type")?,
                deleted: r.try_get("deleted")?,
                parent_station: r.try_get("parent_station")?,
                platform_code: r.try_get("platform_code")?,
            },
        ))
    })
    .collect::<Result<_, _>>()?;
    let mut errors = Vec::new();
    for id in [from, into] {
        match found.get(id) {
            None => errors.push(Finding::error(
                "stop_not_found",
                id,
                format!("no stop {id}"),
            )),
            Some(s) if s.deleted => errors.push(Finding::error(
                "stop_deleted",
                id,
                format!("stop {id} is deleted"),
            )),
            Some(s) if s.location_type == 1 => errors.push(Finding::error(
                "stop_is_station",
                id,
                format!("{id} is a station; only stops are merged"),
            )),
            _ => {}
        }
    }
    if !errors.is_empty() {
        return Err(ApplyError::Findings(errors));
    }
    let (f, i) = (&found[from], &found[into]);

    // Every row of each live route that calls the stop going away, in order:
    // switching the id must not make a route call one stop twice in a row.
    let mut by_route: Vec<(String, Vec<SequencedStop>)> = Vec::new();
    for r in sqlx::query(
        "SELECT rs.route_id, rs.sequence, rs.stop_id, rs.stop_type FROM gtfs_route_stop rs \
         JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
         WHERE rs.gtfs_id = $1 \
           AND rs.route_id IN (SELECT route_id FROM gtfs_route_stop WHERE gtfs_id = $1 AND stop_id = $2) \
         ORDER BY rs.route_id, rs.sequence",
    )
    .bind(g)
    .bind(from)
    .fetch_all(&mut *conn)
    .await?
    {
        let route: String = r.try_get("route_id")?;
        let row = (
            r.try_get("sequence")?,
            r.try_get("stop_id")?,
            r.try_get("stop_type")?,
        );
        match by_route.last_mut() {
            Some((id, rows)) if *id == route => rows.push(row),
            _ => by_route.push((route, vec![row])),
        }
    }
    let mut warnings = Vec::new();
    for (route, rows) in &by_route {
        let effect = merge_effect(rows, from, into);
        for (a, b) in &effect.repeats {
            errors.push(Finding::error(
                "merge_would_repeat_stop",
                format!("{route}|{a}|{b}"),
                format!(
                    "route {route} would call {into} twice in a row (sequences {a} and {b}); fix that route's stop list first"
                ),
            ));
        }
        if effect.repeats.is_empty() && !effect.from_seqs.is_empty() && !effect.into_seqs.is_empty()
        {
            let seqs = |v: &[i32]| v.iter().map(i32::to_string).collect::<Vec<_>>().join(", ");
            warnings.push(Finding::warning(
                "merge_same_route_twice",
                route.as_str(),
                format!(
                    "route {route} calls both {from} (sequence {}) and {into} (sequence {}); after the merge it calls {into} at each",
                    seqs(&effect.from_seqs),
                    seqs(&effect.into_seqs)
                ),
            ));
        }
    }
    if !errors.is_empty() {
        return Err(ApplyError::Findings(errors));
    }
    let apart = haversine_m(f.lat, f.lon, i.lat, i.lon);
    if apart > MERGE_FAR_METRES {
        warnings.push(Finding::warning(
            "merge_far_apart",
            format!("{from}|{into}"),
            format!("{from} and {into} are {apart:.0} m apart"),
        ));
    }
    if f.name != i.name {
        let kept = if keep_name_from { &f.name } else { &i.name };
        warnings.push(Finding::warning(
            "merge_names_differ",
            format!("{from}|{into}"),
            format!(
                "{from} is named {:?} and {into} {:?}; the kept stop is named {kept:?}",
                f.name, i.name
            ),
        ));
    }

    // 1. the routes: moved rows keep the route's own spelling
    let moved: Vec<String> = sqlx::query(
        "UPDATE gtfs_route_stop SET stop_id = $3, \
            stop_name_override = CASE WHEN stop_name_override IS NOT NULL THEN stop_name_override \
                                      WHEN $4 THEN $5 ELSE NULL END, \
            provenance = coalesce(provenance, '{}'::jsonb) || jsonb_build_object('merged_from', $2::text), \
            updated_by = $6 \
         WHERE gtfs_id = $1 AND stop_id = $2 RETURNING route_id",
    )
    .bind(g)
    .bind(from)
    .bind(into)
    .bind(!keep_name_from && f.name != i.name)
    .bind(&f.name)
    .bind(actor)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| r.try_get("route_id"))
    .collect::<Result<_, _>>()?;
    // 2 and 3. the kept stop: name / position if asked, and the station the
    // duplicate was in when it has none
    let takes_station = i.parent_station.is_none() && f.parent_station.is_some();
    if keep_name_from || keep_position_from || takes_station {
        sqlx::query(
            "UPDATE gtfs_stop SET \
                name = CASE WHEN $3 THEN $4 ELSE name END, \
                lat = CASE WHEN $5 THEN $6 ELSE lat END, \
                lon = CASE WHEN $5 THEN $7 ELSE lon END, \
                parent_station = CASE WHEN $8 THEN $9 ELSE parent_station END, \
                platform_code = CASE WHEN $8 THEN coalesce($10, platform_code) ELSE platform_code END, \
                updated_by = $11 \
             WHERE gtfs_id = $1 AND stop_id = $2",
        )
        .bind(g)
        .bind(into)
        .bind(keep_name_from)
        .bind(&f.name)
        .bind(keep_position_from)
        .bind(f.lat)
        .bind(f.lon)
        .bind(takes_station)
        .bind(f.parent_station.as_deref())
        .bind(f.platform_code.as_deref())
        .bind(actor)
        .execute(&mut *conn)
        .await?;
    }
    // 4. the duplicate
    sqlx::query(
        "UPDATE gtfs_stop SET deleted = true, parent_station = NULL, \
            provenance = coalesce(provenance, '{}'::jsonb) || jsonb_build_object('merged_into', $3::text), \
            updated_by = $4 \
         WHERE gtfs_id = $1 AND stop_id = $2",
    )
    .bind(g)
    .bind(from)
    .bind(into)
    .bind(actor)
    .execute(&mut *conn)
    .await?;

    let routes = moved.iter().collect::<HashSet<_>>().len();
    state
        .merged_away
        .insert(from.to_string(), (into.to_string(), c.change_id));
    state.merges.push(json!({
        "change_id": c.change_id, "from": from, "into": into, "routes": routes, "rows": moved.len(),
        "keep_name": if keep_name_from { "from" } else { "into" },
        "keep_position": if keep_position_from { "from" } else { "into" },
    }));
    Ok(warnings)
}

#[derive(Deserialize)]
struct RouteStopsPayload {
    rows: Vec<RouteRow>,
    /// Set when the stop list was split off a reviewed stop (section 8.1).
    #[serde(default)]
    position_review_id: Option<i64>,
}

async fn route_stops_replace(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let payload: RouteStopsPayload = serde_json::from_value(after.clone())
        .map_err(|e| fail("invalid_payload", format!("rows are not valid: {e}")))?;
    let deleted: bool = sqlx::query(
        "SELECT deleted FROM gtfs_route WHERE gtfs_id = $1 AND route_id = $2 FOR UPDATE",
    )
    .bind(g)
    .bind(route_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| fail("route_not_found", format!("no route {route_id}")))?
    .try_get("deleted")?;
    if deleted {
        return Err(fail(
            "route_deleted",
            format!("route {route_id} is deleted"),
        ));
    }

    let mut rows = payload.rows;
    for r in rows.iter_mut() {
        r.stop_id = r
            .stop_id
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    let mut findings = Vec::new();
    let ids: Vec<String> = rows
        .iter()
        .filter(|r| !r.is_marker())
        .filter_map(|r| r.stop_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let known: HashMap<String, (i16, bool)> = sqlx::query(
        "SELECT stop_id, location_type, deleted FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = ANY($2)",
    )
    .bind(g)
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(String, (i16, bool)), sqlx::Error> {
        Ok((r.try_get("stop_id")?, (r.try_get("location_type")?, r.try_get("deleted")?)))
    })
    .collect::<Result<_, _>>()?;
    let mut ids_sorted = ids.clone();
    ids_sorted.sort();
    for id in &ids_sorted {
        match known.get(id) {
            None => findings.push(Finding::error(
                "unknown_stop",
                id.as_str(),
                format!("stop {id} does not exist"),
            )),
            Some((_, true)) => findings.push(Finding::error(
                "stop_deleted",
                id.as_str(),
                format!("stop {id} is deleted"),
            )),
            Some((1, _)) => findings.push(Finding::error(
                "stop_is_station",
                id.as_str(),
                format!("{id} is a station; a route calls at one of its platforms"),
            )),
            _ => {}
        }
    }
    let live = load_route_rows(conn, g, route_id).await?;
    if payload.position_review_id.is_some() {
        // a split points the reviewed stop's rows at a new stop: what those rows
        // already had wrong under the old stop is not the split's doing
        findings.extend(grade_repointed(&rows, &live));
    } else {
        findings.extend(grade_against_live(
            check_route_rows(&rows),
            &check_route_rows(&live),
        ));
    }
    // A stop that cannot be used, or a row the table refuses, stops the change
    // here. A broken fare or stop-order rule does not: the rows are written, so
    // the draft's preview (and every later change in it) sees the stop list as
    // drafted, while the error still blocks submit and commit.
    if findings
        .iter()
        .any(|f| f.level == Level::Error && !ROUTE_RULE_CODES.contains(&f.code.as_str()))
    {
        return Err(ApplyError::Findings(findings));
    }

    // What the live rows carry that an edit does not send: the cleanup's
    // per-row provenance (kept where the same stop or marker stays at the same
    // position) and the provider id (a new row takes the route's usual one).
    let live_meta: HashMap<i32, (Option<String>, Option<String>, Option<String>)> = sqlx::query(
        "SELECT sequence, stop_id, marker_id, provenance::text AS provenance \
         FROM gtfs_route_stop WHERE gtfs_id = $1 AND route_id = $2",
    )
    .bind(g)
    .bind(route_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(
        |r| -> Result<(i32, (Option<String>, Option<String>, Option<String>)), sqlx::Error> {
            Ok((
                r.try_get("sequence")?,
                (
                    r.try_get("stop_id")?,
                    r.try_get("marker_id")?,
                    r.try_get("provenance")?,
                ),
            ))
        },
    )
    .collect::<Result<_, _>>()?;
    let usual_provider = {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for r in &live {
            if let Some(p) = r.provider_id.as_deref() {
                *counts.entry(p).or_default() += 1;
            }
        }
        let mut v: Vec<_> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        v.first().map(|(p, _)| p.to_string())
    };

    let n = rows.len();
    let mut seq = Vec::with_capacity(n);
    let (mut stop, mut typ, mut stage_no, mut stage_name) = (vec![], vec![], vec![], vec![]);
    let (mut mid, mut mname, mut mlat, mut mlon, mut over, mut prov) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    let mut provenance: Vec<Option<String>> = Vec::with_capacity(n);
    for (i, r) in rows.iter().enumerate() {
        let s = i as i32 + 1;
        seq.push(s);
        stop.push(if r.is_marker() {
            None
        } else {
            r.stop_id.clone()
        });
        typ.push(r.stop_type.clone());
        stage_no.push(r.stage_no);
        stage_name.push(r.stage_name.clone());
        mid.push(if r.is_marker() {
            Some(
                r.marker_id
                    .clone()
                    .unwrap_or_else(|| format!("rc_{route_id}_{s}")),
            )
        } else {
            None
        });
        mname.push(if r.is_marker() {
            r.marker_name.clone()
        } else {
            None
        });
        mlat.push(if r.is_marker() { r.marker_lat } else { None });
        mlon.push(if r.is_marker() { r.marker_lon } else { None });
        over.push(if r.is_marker() {
            None
        } else {
            r.stop_name_override.clone()
        });
        prov.push(r.provider_id.clone().or_else(|| usual_provider.clone()));
        provenance.push(live_meta.get(&s).and_then(|(sid, mk, pv)| {
            let same = if r.is_marker() {
                mk.is_some() && mk == &r.marker_id
            } else {
                sid.is_some() && sid == &r.stop_id
            };
            if same {
                pv.clone()
            } else {
                None
            }
        }));
    }
    sqlx::query("DELETE FROM gtfs_route_stop WHERE gtfs_id = $1 AND route_id = $2")
        .bind(g)
        .bind(route_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, \
                                      marker_id, marker_name, marker_lat, marker_lon, stop_name_override, \
                                      provider_id, provenance, updated_by) \
         SELECT $1, $2, u.seq, u.stop, u.typ, u.no, u.name, u.mid, u.mname, u.mlat, u.mlon, u.over, u.prov, \
                u.provenance::jsonb, $14 \
         FROM UNNEST($3::int4[], $4::text[], $5::text[], $6::int4[], $7::text[], $8::text[], $9::text[], \
                     $10::float8[], $11::float8[], $12::text[], $13::text[], $15::text[]) \
              AS u(seq, stop, typ, no, name, mid, mname, mlat, mlon, over, prov, provenance)",
    )
    .bind(g)
    .bind(route_id)
    .bind(&seq)
    .bind(&stop)
    .bind(&typ)
    .bind(&stage_no)
    .bind(&stage_name)
    .bind(&mid)
    .bind(&mname)
    .bind(&mlat)
    .bind(&mlon)
    .bind(&over)
    .bind(&prov)
    .bind(actor)
    .bind(&provenance)
    .execute(&mut *conn)
    .await?;
    Ok(findings)
}

/// Put `members` under `station`, setting the platform label of each member
/// that sends one. Rows already in that state are left alone (no version bump).
async fn join_station(
    conn: &mut PgConnection,
    g: &str,
    station: &str,
    members: &[MemberSpec],
    actor: &str,
) -> Result<(), sqlx::Error> {
    if members.is_empty() {
        return Ok(());
    }
    let ids: Vec<&str> = members.iter().map(|m| m.stop_id.as_str()).collect();
    let has: Vec<bool> = members.iter().map(|m| m.platform_code.is_some()).collect();
    let codes: Vec<Option<&str>> = members
        .iter()
        .map(|m| m.platform_code.as_ref().and_then(|p| p.as_deref()))
        .collect();
    sqlx::query(
        "UPDATE gtfs_stop s SET parent_station = $2, \
            platform_code = CASE WHEN u.has THEN u.code ELSE s.platform_code END, \
            updated_by = $6 \
         FROM UNNEST($3::text[], $4::bool[], $5::text[]) AS u(id, has, code) \
         WHERE s.gtfs_id = $1 AND s.stop_id = u.id \
           AND (s.parent_station IS DISTINCT FROM $2 \
                OR (u.has AND s.platform_code IS DISTINCT FROM u.code))",
    )
    .bind(g)
    .bind(station)
    .bind(&ids)
    .bind(&has)
    .bind(&codes)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Every proposed member exists, is a live stop (not a station), and has no
/// other parent.
async fn check_members(
    conn: &mut PgConnection,
    g: &str,
    station_id: &str,
    members: &[String],
) -> Result<(), ApplyError> {
    let found: HashMap<String, LiveStop> = sqlx::query(
        "SELECT stop_id, lat, lon, location_type, deleted, parent_station FROM gtfs_stop \
         WHERE gtfs_id = $1 AND stop_id = ANY($2) FOR UPDATE",
    )
    .bind(g)
    .bind(members)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(String, LiveStop), sqlx::Error> {
        Ok((
            r.try_get("stop_id")?,
            LiveStop {
                lat: r.try_get("lat")?,
                lon: r.try_get("lon")?,
                location_type: r.try_get("location_type")?,
                deleted: r.try_get("deleted")?,
                parent_station: r.try_get("parent_station")?,
            },
        ))
    })
    .collect::<Result<_, _>>()?;
    let mut findings = Vec::new();
    for id in members {
        match found.get(id) {
            None => findings.push(Finding::error(
                "unknown_stop",
                id.as_str(),
                format!("stop {id} does not exist"),
            )),
            Some(s) if s.deleted => findings.push(Finding::error(
                "stop_deleted",
                id.as_str(),
                format!("stop {id} is deleted"),
            )),
            Some(s) if s.location_type == 1 => findings.push(Finding::error(
                "member_is_station",
                id.as_str(),
                format!("{id} is itself a station"),
            )),
            Some(s) if s.parent_station.as_deref().is_some_and(|p| p != station_id) => findings
                .push(Finding::error(
                    "member_has_parent",
                    id.as_str(),
                    format!(
                        "{id} already belongs to station {}",
                        s.parent_station.as_deref().unwrap_or("")
                    ),
                )),
            _ => {}
        }
    }
    if findings.is_empty() {
        Ok(())
    } else {
        Err(ApplyError::Findings(findings))
    }
}

async fn station_create(
    conn: &mut PgConnection,
    g: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let m = after.as_object().expect("payload checked");
    let id = m["station_id"].as_str().expect("payload checked").trim();
    let specs = station_members(m).unwrap_or_default();
    let members: Vec<String> = specs.iter().map(|s| s.stop_id.clone()).collect();
    check_members(conn, g, id, &members).await?;
    let created = sqlx::query(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, location_type, position_source, updated_by, \
                                description) \
         VALUES ($1, $2, $2, $3, $4, $5, 1, 'REVIEW', $6, $7) ON CONFLICT DO NOTHING RETURNING stop_id",
    )
    .bind(g)
    .bind(id)
    .bind(m.get("name").and_then(Value::as_str).map(str::trim))
    .bind(m.get("lat").and_then(Value::as_f64))
    .bind(m.get("lon").and_then(Value::as_f64))
    .bind(actor)
    .bind(text_field(m, "description").1)
    .fetch_optional(&mut *conn)
    .await?;
    if created.is_none() {
        return Err(fail(
            "station_exists",
            format!("a stop or station {id} already exists"),
        ));
    }
    // check_payload has made sure there are at least two members
    join_station(conn, g, id, &specs, actor).await?;
    Ok(vec![])
}

async fn live_station(conn: &mut PgConnection, g: &str, id: &str) -> Result<LiveStop, ApplyError> {
    let s = live_stop(conn, g, id)
        .await?
        .ok_or_else(|| fail("station_not_found", format!("no station {id}")))?;
    if s.location_type != 1 {
        return Err(fail(
            "not_a_station",
            format!("{id} is a stop, not a station"),
        ));
    }
    if s.deleted {
        return Err(fail("station_deleted", format!("station {id} is deleted")));
    }
    Ok(s)
}

async fn station_update(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let live = live_station(conn, g, id).await?;
    let m = after.as_object().expect("payload checked");
    let (has_name, name) = field(m, "name");
    let (has_description, description) = text_field(m, "description");
    let moved = m.contains_key("lat");
    sqlx::query(
        "UPDATE gtfs_stop SET name = CASE WHEN $3 THEN $4 ELSE name END, \
            lat = CASE WHEN $5 THEN $6 ELSE lat END, lon = CASE WHEN $5 THEN $7 ELSE lon END, \
            description = CASE WHEN $9 THEN $10 ELSE description END, \
            updated_by = $8 \
         WHERE gtfs_id = $1 AND stop_id = $2 AND ($3 OR $5 OR $9)",
    )
    .bind(g)
    .bind(id)
    .bind(has_name)
    .bind(name.map(str::trim))
    .bind(moved)
    .bind(m.get("lat").and_then(Value::as_f64).unwrap_or(live.lat))
    .bind(m.get("lon").and_then(Value::as_f64).unwrap_or(live.lon))
    .bind(actor)
    .bind(has_description)
    .bind(description)
    .execute(&mut *conn)
    .await?;
    // members, when sent, are at least two (check_payload)
    if let Some(specs) = station_members(m) {
        let members: Vec<String> = specs.iter().map(|s| s.stop_id.clone()).collect();
        check_members(conn, g, id, &members).await?;
        sqlx::query(
            "UPDATE gtfs_stop SET parent_station = NULL, updated_by = $4 \
             WHERE gtfs_id = $1 AND parent_station = $2 AND NOT (stop_id = ANY($3))",
        )
        .bind(g)
        .bind(id)
        .bind(&members)
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        join_station(conn, g, id, &specs, actor).await?;
    }
    Ok(vec![])
}

async fn station_delete(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    live_station(conn, g, id).await?;
    sqlx::query(
        "UPDATE gtfs_stop SET parent_station = NULL, updated_by = $3 WHERE gtfs_id = $1 AND parent_station = $2",
    )
    .bind(g)
    .bind(id)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE gtfs_stop SET deleted = true, updated_by = $3 WHERE gtfs_id = $1 AND stop_id = $2",
    )
    .bind(g)
    .bind(id)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(vec![])
}

/// Switch the data source GIMS serves the feed from. The feed's version is not
/// touched here: the commit's own bump is what every pod's poll notices. A
/// change to the value the feed already has - earlier changes of the set taken
/// as applied - switches nothing and says so.
async fn feed_config_update(
    conn: &mut PgConnection,
    g: &str,
    c: &ChangeRow,
    state: &mut ApplyState<'_>,
) -> Result<Vec<Finding>, ApplyError> {
    if c.entity_key != g {
        return Err(fail(
            "feed_mismatch",
            format!(
                "the change is for feed {} and the change set for feed {g}",
                c.entity_key
            ),
        ));
    }
    let to = c.after["data_source"].as_str().expect("payload checked");
    let from: String =
        sqlx::query("SELECT data_source FROM gtfs_feed WHERE gtfs_id = $1 FOR UPDATE")
            .bind(g)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| fail("feed_not_found", format!("no feed {g}")))?
            .try_get("data_source")?;
    if from == to {
        return Ok(vec![Finding::warning(
            "data_source_unchanged",
            g,
            format!("feed {g} is already served from '{to}'; this change switches nothing"),
        )]);
    }
    sqlx::query("UPDATE gtfs_feed SET data_source = $2 WHERE gtfs_id = $1")
        .bind(g)
        .bind(to)
        .execute(&mut *conn)
        .await?;
    state.feed_configs.push(json!({
        "gtfs_id": g, "from": from, "to": to, "change_id": c.change_id,
    }));
    Ok(vec![])
}

// ---------------------------------------------------------------- set detail

/// The set, its changes, and - unless it is finished - what applying it now
/// would find. Runs the changes in a transaction that is rolled back.
pub async fn set_detail(state: &EditorState, ctx: &Ctx, id: Uuid) -> EditorResult<Value> {
    retry_transient(|| set_detail_once(state, ctx, id)).await
}

async fn set_detail_once(state: &EditorState, ctx: &Ctx, id: Uuid) -> EditorResult<Value> {
    let mut tx = state.pool.begin().await?;
    let set = load_set(&mut tx, id, false).await?;
    let changes = load_changes(&mut tx, id).await?;
    let ev = if matches!(set.status.as_str(), "committed" | "discarded") {
        Evaluation::default()
    } else {
        lock_feed(&mut tx, &set.gtfs_id).await?;
        evaluate(&mut tx, &set.gtfs_id, &changes, &ctx.user.email).await?
    };
    tx.rollback().await?;
    // Stop-order changes carry ids only; give the diff the names of every stop
    // they reference (live names, so a stop added to a route reads as itself).
    let referenced: Vec<String> = changes
        .iter()
        .filter(|c| c.entity == "route_stops")
        .flat_map(|c| c.after["rows"].as_array().cloned().unwrap_or_default())
        .filter_map(|r| r["stop_id"].as_str().map(str::to_string))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let stop_names: Map<String, Value> = if referenced.is_empty() {
        Map::new()
    } else {
        sqlx::query("SELECT stop_id, name FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = ANY($2)")
            .bind(&set.gtfs_id)
            .bind(&referenced)
            .fetch_all(&state.pool)
            .await?
            .iter()
            .map(|r| -> Result<(String, Value), sqlx::Error> {
                Ok((
                    r.try_get("stop_id")?,
                    json!(r.try_get::<String, _>("name")?),
                ))
            })
            .collect::<Result<_, _>>()?
    };
    // a stop created in this draft has no live name yet: use the one it is given
    let mut stop_names = stop_names;
    let wanted: HashSet<&String> = referenced.iter().collect();
    for c in changes
        .iter()
        .filter(|c| c.entity == "stop" && c.op == "create")
    {
        if wanted.contains(&c.entity_key) && !stop_names.contains_key(&c.entity_key) {
            if let Some(name) = c.after["name"].as_str() {
                stop_names.insert(c.entity_key.clone(), json!(name.trim()));
            }
        }
    }
    let mut out = set.json;
    out["stop_names"] = Value::Object(stop_names);
    out["changes"] = json!(changes.iter().map(ChangeRow::json).collect::<Vec<_>>());
    out["validation"] = json!(ev.validation);
    out["conflicts"] = json!(ev.conflicts);
    out["can_submit"] = json!(
        set.status == "draft" && !changes.is_empty() && !ev.has_errors() && ev.conflicts.is_empty()
    );
    Ok(out)
}

pub async fn preview_route(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    route_id: &str,
) -> EditorResult<Value> {
    retry_transient(|| preview_route_once(state, ctx, id, route_id)).await
}

async fn preview_route_once(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    route_id: &str,
) -> EditorResult<Value> {
    let mut tx = state.pool.begin().await?;
    let set = load_set(&mut tx, id, false).await?;
    let changes = load_changes(&mut tx, id).await?;
    lock_feed(&mut tx, &set.gtfs_id).await?;
    let ev = evaluate(&mut tx, &set.gtfs_id, &changes, &ctx.user.email).await?;
    let route = route_detail(&mut tx, &set.gtfs_id, route_id).await;
    tx.rollback().await?;
    // the route in read shape, with the draft applied, plus what applying found
    let mut route = route?;
    route["validation"] = json!(ev.validation);
    route["conflicts"] = json!(ev.conflicts);
    Ok(route)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewChange {
    pub entity: String,
    pub op: String,
    /// May be left out for a create - or sent as `""` or `null`, which mean the
    /// same: it is taken from `after`, and a new stop without an id gets one
    /// minted.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub entity_key: String,
    #[serde(default)]
    pub after: Value,
    #[serde(default)]
    pub base_row_version: Option<i32>,
}

fn null_as_empty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

pub fn editable(set: &ChangeSet) -> EditorResult<()> {
    if set.status == "draft" {
        Ok(())
    } else {
        Err(EditorError::conflict(
            "change_set_not_draft",
            format!("the change set is {}; reopen it to edit", set.status),
        ))
    }
}

/// The `after` of the create, earlier in the set, that makes row `key` (a stop
/// or station for `routes = false`, a route otherwise).
async fn created_in_set(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    routes: bool,
    key: &str,
) -> EditorResult<Option<(String, Value)>> {
    let entities: &[&str] = if routes {
        &["route"]
    } else {
        &["stop", "station"]
    };
    let row = sqlx::query(
        "SELECT entity, after::text AS after FROM gtfs_change \
         WHERE change_set_id = $1 AND op = 'create' AND entity = ANY($2) AND entity_key = $3 \
         ORDER BY position LIMIT 1",
    )
    .bind(change_set_id)
    .bind(entities)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match row {
        Some(r) => Some((r.try_get("entity")?, json_col(&r, "after")?)),
        None => None,
    })
}

/// The `after` of the creates in the set that make the stops or stations
/// `keys`, as `(entity, after)` by id: [`created_in_set`] for a whole upload.
pub async fn stops_created_in_set(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    keys: &[String],
) -> EditorResult<HashMap<String, (String, Value)>> {
    let mut out = HashMap::new();
    if keys.is_empty() {
        return Ok(out);
    }
    for r in sqlx::query(
        "SELECT entity, entity_key, after::text AS after FROM gtfs_change \
         WHERE change_set_id = $1 AND op = 'create' AND entity IN ('stop', 'station') \
           AND entity_key = ANY($2) ORDER BY position DESC",
    )
    .bind(change_set_id)
    .bind(keys)
    .fetch_all(&mut *conn)
    .await?
    {
        // position DESC: the earliest create of an id is the one that stays
        out.insert(
            r.try_get("entity_key")?,
            (r.try_get("entity")?, json_col(&r, "after")?),
        );
    }
    Ok(out)
}

/// A stop row in read shape, or - for a stop created earlier in the set - what
/// its create gives it. The bool says whether it is a station.
async fn stop_or_created(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    g: &str,
    key: &str,
) -> EditorResult<Option<(Value, bool, Option<i32>)>> {
    if let Some(row) = stop_row(conn, g, key).await? {
        let station = row["location_type"] == 1;
        let version = row["row_version"].as_i64().map(|v| v as i32);
        return Ok(Some((row, station, version)));
    }
    Ok(created_in_set(conn, change_set_id, false, key)
        .await?
        .map(|(entity, after)| (after, entity == "station", None)))
}

/// Snapshot of what a change replaces, and the version it is based on. Rows an
/// earlier change in the same set creates count as existing (with no version).
/// May fill in `after` what the change leaves to the live data (a merge's
/// `into_row_version`).
async fn snapshot(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    g: &str,
    entity: &str,
    op: &str,
    key: &str,
    after: &mut Value,
) -> EditorResult<(Value, Option<i32>)> {
    let station_mismatch = |is_station: bool| {
        EditorError::bad_request(
            if is_station {
                "use_station_change"
            } else {
                "not_a_station"
            },
            format!(
                "{key} is {}",
                if is_station {
                    "a station"
                } else {
                    "a stop, not a station"
                }
            ),
        )
    };
    match (entity, op) {
        ("stop", "update" | "delete") | ("station", "update" | "delete") => {
            let (mut row, is_station, version) = stop_or_created(conn, change_set_id, g, key)
                .await?
                .ok_or_else(|| {
                    EditorError::not_found("entity_not_found", format!("no {entity} {key}"))
                })?;
            if (entity == "station") != is_station {
                return Err(station_mismatch(is_station));
            }
            if is_station && version.is_some() {
                let members = stations_members(conn, g, &[key.to_string()])
                    .await?
                    .remove(key)
                    .unwrap_or_default();
                with_members(&mut row, &members);
            }
            Ok((row, version))
        }
        ("stop", "merge") => {
            let into = after["into_stop_id"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_string();
            let side = |row: Option<(Value, bool, Option<i32>)>, id: &str| {
                let (row, is_station, version) = row.ok_or_else(|| {
                    EditorError::not_found("entity_not_found", format!("no stop {id}"))
                })?;
                if is_station {
                    return Err(EditorError::bad_request(
                        "stop_is_station",
                        format!("{id} is a station; only stops are merged"),
                    ));
                }
                Ok::<_, EditorError>((row, version))
            };
            let (from_row, from_version) =
                side(stop_or_created(conn, change_set_id, g, key).await?, key)?;
            let (into_row, into_version) =
                side(stop_or_created(conn, change_set_id, g, &into).await?, &into)?;
            if after["into_row_version"].is_null() {
                if let (Some(m), Some(v)) = (after.as_object_mut(), into_version) {
                    m.insert("into_row_version".into(), json!(v));
                }
            }
            let affected = sqlx::query(
                "SELECT rs.route_id, r.short_name, array_agg(rs.sequence ORDER BY rs.sequence) AS sequences \
                 FROM gtfs_route_stop rs \
                 JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
                 WHERE rs.gtfs_id = $1 AND rs.stop_id = $2 \
                 GROUP BY rs.route_id, r.short_name ORDER BY rs.route_id",
            )
            .bind(g)
            .bind(key)
            .fetch_all(&mut *conn)
            .await?
            .iter()
            .map(|r| -> Result<Value, sqlx::Error> {
                Ok(json!({
                    "route_id": r.try_get::<String, _>("route_id")?,
                    "short_name": r.try_get::<Option<String>, _>("short_name")?,
                    "sequences": r.try_get::<Vec<i32>, _>("sequences")?,
                }))
            })
            .collect::<Result<Vec<_>, _>>()?;
            Ok((
                json!({"from": from_row, "into": into_row, "affected": affected}),
                from_version,
            ))
        }
        ("route", "update" | "delete") => {
            if let Some(row) = route_row(conn, g, key).await? {
                let v = row["row_version"].as_i64().map(|v| v as i32);
                return Ok((row, v));
            }
            let (_, created) = created_in_set(conn, change_set_id, true, key)
                .await?
                .ok_or_else(|| {
                    EditorError::not_found("entity_not_found", format!("no route {key}"))
                })?;
            Ok((created, None))
        }
        ("feed_config", "update") => Ok((feed_config_row(conn, g).await?, None)),
        ("route_stops", "replace") => {
            if route_row(conn, g, key).await?.is_none() {
                if created_in_set(conn, change_set_id, true, key)
                    .await?
                    .is_some()
                {
                    // a route this set creates starts with no rows
                    return Ok((json!([]), None));
                }
                return Err(EditorError::not_found(
                    "entity_not_found",
                    format!("no route {key}"),
                ));
            }
            // the rows in read shape (names and positions), as the diff shows them
            let detail = route_detail(conn, g, key).await?;
            Ok((detail["rows"].clone(), None))
        }
        _ => Ok((Value::Null, None)),
    }
}

pub async fn add_change(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    change: NewChange,
) -> EditorResult<i64> {
    retry_transient(|| add_change_once(state, ctx, id, change.clone())).await
}

async fn add_change_once(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    change: NewChange,
) -> EditorResult<i64> {
    let mut tx = state.pool.begin().await?;
    lock_feed_of_set(&mut tx, id).await?;
    let set = load_set(&mut tx, id, true).await?;
    let change_id = add_change_to(&mut tx, ctx, &set, change).await?;
    tx.commit().await?;
    Ok(change_id)
}

/// Append one change to `set`, which the caller has locked, inside the caller's
/// transaction: the shape checks, a minted id, the defaults, the `before`
/// snapshot and base version, the row and its `change_added` audit. Everything
/// that adds a single change goes through here, so a change made for a review
/// (a merge, section 8.2) is exactly the change `POST /change-sets/{id}/changes`
/// would have stored.
pub async fn add_change_to(
    tx: &mut PgConnection,
    ctx: &Ctx,
    set: &ChangeSet,
    change: NewChange,
) -> EditorResult<i64> {
    let id = set.change_set_id;
    let NewChange {
        entity,
        op,
        entity_key,
        mut after,
        base_row_version,
    } = change;
    // which data source a feed is served from is an admin's call
    if entity == "feed_config" {
        ctx.require_role(auth::Role::Admin)?;
    }
    let mut key = entity_key.trim().to_string();
    settle_create_key(&entity, &op, &mut key, &mut after);
    let mint = entity == "stop" && op == "create" && key.is_empty() && after.is_object();
    let invalid = |f: Finding| {
        EditorError::bad_request("invalid_change", f.message.clone())
            .with_details(json!({"code": f.code}))
    };
    if key.is_empty() && !mint {
        if create_id_field(&entity, &op).is_some() {
            // a create without an id anywhere: say what is wrong with `after`
            check_payload(&entity, &op, &key, &after).map_err(invalid)?;
        }
        return Err(EditorError::bad_request(
            "entity_key_required",
            "entity_key is required",
        ));
    }
    if !mint {
        check_payload(&entity, &op, &key, &after).map_err(invalid)?;
    }
    editable(set)?;
    if entity == "feed_config" && key != set.gtfs_id {
        return Err(EditorError::bad_request(
            "invalid_change",
            format!(
                "feed_config/update: entity_key must be the change set's feed, {}",
                set.gtfs_id
            ),
        )
        .with_details(json!({"code": "feed_mismatch"})));
    }
    if mint {
        key = mint_stop_ids(&mut *tx, &set.gtfs_id, 1).await?.remove(0);
        after["stop_id"] = json!(key);
        check_payload(&entity, &op, &key, &after).map_err(invalid)?;
    }
    if entity == "route" && op == "create" {
        // defaults are written into the change, so a reviewer sees them
        if after["route_type"].is_null() {
            after["route_type"] = json!(3);
        }
        if after["agency_id"].is_null() {
            if let Some(agency) = usual_agency(&mut *tx, &set.gtfs_id).await? {
                after["agency_id"] = json!(agency);
            }
        }
    }
    if entity == "route" && op == "delete" {
        let others = other_route_changes(&load_changes(&mut *tx, id).await?, &key, 0);
        if !others.is_empty() {
            return Err(EditorError::conflict(
                "route_has_pending_changes",
                format!(
                    "change(s) {} in this set edit route {key}; remove them before deleting it",
                    others
                        .iter()
                        .map(i64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
            .with_details(json!({"change_ids": others})));
        }
    }
    let (before, live_version) =
        snapshot(&mut *tx, id, &set.gtfs_id, &entity, &op, &key, &mut after).await?;
    let base = base_row_version.or(live_version);
    let change_id: i64 = sqlx::query(
        "INSERT INTO gtfs_change (change_set_id, position, entity, entity_key, op, base_row_version, before, after, created_by) \
         VALUES ($1, (SELECT coalesce(max(position), 0) + 1 FROM gtfs_change WHERE change_set_id = $1), \
                 $2, $3, $4, $5, $6::jsonb, $7::jsonb, $8) RETURNING change_id",
    )
    .bind(id)
    .bind(&entity)
    .bind(&key)
    .bind(&op)
    .bind(base)
    .bind((!before.is_null()).then(|| before.to_string()))
    .bind((!after.is_null()).then(|| after.to_string()))
    .bind(ctx.user.user_id)
    .fetch_one(&mut *tx)
    .await?
    .try_get("change_id")?;
    sqlx::query("UPDATE gtfs_change_set SET updated_at = now() WHERE change_set_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "change_added",
        Some(&set.gtfs_id),
        Some(id),
        json!({"change_id": change_id, "entity": entity, "op": op, "entity_key": key}),
    )
    .await?;
    Ok(change_id)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeUpdate {
    #[serde(default)]
    pub after: Value,
    #[serde(default)]
    pub base_row_version: Option<i32>,
}

pub async fn update_change(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    change_id: i64,
    update: ChangeUpdate,
) -> EditorResult<()> {
    retry_transient(|| update_change_once(state, ctx, id, change_id, update.clone())).await
}

async fn update_change_once(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    change_id: i64,
    update: ChangeUpdate,
) -> EditorResult<()> {
    let mut tx = state.pool.begin().await?;
    lock_feed_of_set(&mut tx, id).await?;
    let set = load_set(&mut tx, id, true).await?;
    editable(&set)?;
    let row = sqlx::query(
        "SELECT entity, op, entity_key, after::text AS after FROM gtfs_change WHERE change_set_id = $1 AND change_id = $2",
    )
    .bind(id)
    .bind(change_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| EditorError::not_found("change_not_found", "no such change in this set"))?;
    let (entity, op, key): (String, String, String) = (
        row.try_get("entity")?,
        row.try_get("op")?,
        row.try_get("entity_key")?,
    );
    if entity == "feed_config" {
        ctx.require_role(auth::Role::Admin)?;
    }
    let mut update = update;
    // a create's id is fixed once the change exists; a merge keeps the kept
    // stop's version it was made against unless it names another stop
    let mut fixed_key = key.clone();
    settle_create_key(&entity, &op, &mut fixed_key, &mut update.after);
    if entity == "stop" && op == "merge" && update.after["into_row_version"].is_null() {
        let old: Value = json_col(&row, "after")?;
        let into = update.after["into_stop_id"].as_str().map(str::trim);
        let version = if old["into_stop_id"].as_str().map(str::trim) == into {
            old["into_row_version"].as_i64()
        } else {
            match into {
                Some(into) => live_row_version(&mut tx, "gtfs_stop", "stop_id", &set.gtfs_id, into)
                    .await?
                    .map(i64::from),
                None => None,
            }
        };
        if let (Some(m), Some(v)) = (update.after.as_object_mut(), version) {
            m.insert("into_row_version".into(), json!(v));
        }
    }
    // a change made for a coordinate review stays that review's
    super::position_reviews::keep_review_link(&json_col(&row, "after")?, &mut update.after)
        .map_err(|why| {
            EditorError::bad_request("invalid_change", why)
                .with_details(json!({"code": "position_review_mismatch"}))
        })?;
    check_payload(&entity, &op, &key, &update.after).map_err(|f| {
        EditorError::bad_request("invalid_change", f.message.clone())
            .with_details(json!({"code": f.code}))
    })?;
    sqlx::query(
        "UPDATE gtfs_change SET after = $3::jsonb, base_row_version = coalesce($4, base_row_version) \
         WHERE change_set_id = $1 AND change_id = $2",
    )
    .bind(id)
    .bind(change_id)
    .bind((!update.after.is_null()).then(|| update.after.to_string()))
    .bind(update.base_row_version)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE gtfs_change_set SET updated_at = now() WHERE change_set_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "change_updated",
        Some(&set.gtfs_id),
        Some(id),
        json!({"change_id": change_id}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn delete_change(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    change_id: i64,
) -> EditorResult<()> {
    retry_transient(|| delete_change_once(state, ctx, id, change_id)).await
}

async fn delete_change_once(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    change_id: i64,
) -> EditorResult<()> {
    let mut tx = state.pool.begin().await?;
    lock_feed_of_set(&mut tx, id).await?;
    let set = load_set(&mut tx, id, true).await?;
    editable(&set)?;
    let removed = sqlx::query(
        "DELETE FROM gtfs_change WHERE change_set_id = $1 AND change_id = $2 \
         RETURNING entity, op, entity_key, after->>'position_review_id' AS position_review_id",
    )
    .bind(id)
    .bind(change_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| EditorError::not_found("change_not_found", "no such change in this set"))?;
    let removed = super::position_reviews::RemovedChange {
        change_id,
        entity: removed.try_get("entity")?,
        op: removed.try_get("op")?,
        entity_key: removed.try_get("entity_key")?,
        position_review_id: removed
            .try_get::<Option<String>, _>("position_review_id")?
            .and_then(|v| v.parse().ok()),
    };
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "change_removed",
        Some(&set.gtfs_id),
        Some(id),
        json!({"change_id": change_id}),
    )
    .await?;
    super::proposals::return_to_pending(
        &mut tx,
        ctx,
        &set.gtfs_id,
        id,
        Some(change_id),
        "change_removed",
    )
    .await?;
    super::position_reviews::change_removed(&mut tx, ctx, &set.gtfs_id, id, &removed).await?;
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------- transitions

pub async fn submit(state: &EditorState, ctx: &Ctx, id: Uuid) -> EditorResult<()> {
    retry_transient(|| submit_once(state, ctx, id)).await
}

async fn submit_once(state: &EditorState, ctx: &Ctx, id: Uuid) -> EditorResult<()> {
    let mut tx = state.pool.begin().await?;
    lock_feed_of_set(&mut tx, id).await?;
    let set = load_set(&mut tx, id, true).await?;
    editable(&set)?;
    let changes = load_changes(&mut tx, id).await?;
    if changes.is_empty() {
        return Err(EditorError::bad_request(
            "change_set_empty",
            "add a change before submitting",
        ));
    }
    sqlx::query("SAVEPOINT editor_submit")
        .execute(&mut *tx)
        .await?;
    let ev = evaluate(&mut tx, &set.gtfs_id, &changes, &ctx.user.email).await?;
    sqlx::query("ROLLBACK TO SAVEPOINT editor_submit")
        .execute(&mut *tx)
        .await?;
    if !ev.conflicts.is_empty() {
        return Err(EditorError::conflict(
            "change_set_conflicts",
            "the live data changed since these edits were made",
        )
        .with_details(json!({"conflicts": ev.conflicts, "validation": ev.validation})));
    }
    if ev.has_errors() {
        return Err(EditorError::bad_request(
            "validation_failed",
            "fix the errors before submitting",
        )
        .with_details(json!({"validation": ev.validation})));
    }
    sqlx::query(
        "UPDATE gtfs_change_set SET status = 'submitted', submitted_by = $2, submitted_at = now(), \
            reviewed_by = NULL, reviewed_at = NULL, review_comment = NULL, self_approved = false \
         WHERE change_set_id = $1 AND status = 'draft'",
    )
    .bind(id)
    .bind(ctx.user.user_id)
    .execute(&mut *tx)
    .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "change_set_submitted",
        Some(&set.gtfs_id),
        Some(id),
        json!({"changes": changes.len(), "warnings": ev.validation.len()}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Maker-checker, enforced here as well as by the table's CHECK: nobody reviews
/// or commits a set they submitted, whatever their role - except an admin who
/// says so explicitly ([`review`]'s `self_approve`), which marks the set
/// `self_approved` and lets that admin commit it too. `details.can_self_approve`
/// tells the dashboard whether that override is open to the caller.
fn own_change_set(can_self_approve: bool) -> EditorError {
    EditorError::forbidden(
        "own_change_set",
        "a change set must be reviewed by someone other than the person who submitted it",
    )
    .with_details(json!({"can_self_approve": can_self_approve}))
}

pub async fn review(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    approve: bool,
    comment: Option<&str>,
    self_approve: bool,
) -> EditorResult<()> {
    let comment = comment.map(str::trim).filter(|c| !c.is_empty());
    if !approve && comment.is_none() {
        return Err(EditorError::bad_request(
            "comment_required",
            "say why the change set is rejected",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let set = load_set(&mut tx, id, true).await?;
    if set.status != "submitted" {
        return Err(EditorError::conflict(
            "change_set_not_submitted",
            format!("the change set is {}", set.status),
        ));
    }
    // the override is an approval, by an admin, asked for in so many words; on
    // someone else's set the flag means nothing
    let own = set.submitted_by == Some(ctx.user.user_id);
    let may_override = approve && ctx.user.role() >= auth::Role::Admin;
    if own && !(may_override && self_approve) {
        return Err(own_change_set(may_override));
    }
    sqlx::query(
        "UPDATE gtfs_change_set SET status = $2, reviewed_by = $3, reviewed_at = now(), review_comment = $4, \
            self_approved = $5 \
         WHERE change_set_id = $1",
    )
    .bind(id)
    .bind(if approve { "approved" } else { "rejected" })
    .bind(ctx.user.user_id)
    .bind(comment)
    .bind(own)
    .execute(&mut *tx)
    .await?;
    let (action, detail) = match (approve, own) {
        (true, true) => (
            "change_set_self_approved",
            json!({
                "submitted_by": set.submitted_by,
                "submitted_by_email": set.json["submitted_by_email"],
                "comment": comment,
            }),
        ),
        (true, false) => ("change_set_approved", json!({"comment": comment})),
        (false, _) => ("change_set_rejected", json!({"comment": comment})),
    };
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        action,
        Some(&set.gtfs_id),
        Some(id),
        detail,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn reopen(state: &EditorState, ctx: &Ctx, id: Uuid) -> EditorResult<()> {
    let mut tx = state.pool.begin().await?;
    let set = load_set(&mut tx, id, true).await?;
    if !matches!(set.status.as_str(), "submitted" | "rejected" | "approved") {
        return Err(EditorError::conflict(
            "cannot_reopen",
            format!("a {} change set cannot be reopened", set.status),
        ));
    }
    let is_author =
        set.created_by == ctx.user.user_id || set.submitted_by == Some(ctx.user.user_id);
    if !is_author && ctx.user.role() < auth::Role::Admin {
        return Err(EditorError::forbidden(
            "not_author",
            "only its author or an admin can reopen a change set",
        ));
    }
    sqlx::query(
        "UPDATE gtfs_change_set SET status = 'draft', submitted_by = NULL, submitted_at = NULL, \
            reviewed_by = NULL, reviewed_at = NULL, self_approved = false WHERE change_set_id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "change_set_reopened",
        Some(&set.gtfs_id),
        Some(id),
        json!({"from": set.status}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn discard(state: &EditorState, ctx: &Ctx, id: Uuid) -> EditorResult<()> {
    let mut tx = state.pool.begin().await?;
    let set = load_set(&mut tx, id, true).await?;
    if matches!(set.status.as_str(), "committed" | "discarded") {
        return Err(EditorError::conflict(
            "cannot_discard",
            format!("a {} change set cannot be discarded", set.status),
        ));
    }
    if set.created_by != ctx.user.user_id && ctx.user.role() < auth::Role::Admin {
        return Err(EditorError::forbidden(
            "not_author",
            "only its author or an admin can discard a change set",
        ));
    }
    sqlx::query("UPDATE gtfs_change_set SET status = 'discarded' WHERE change_set_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "change_set_discarded",
        Some(&set.gtfs_id),
        Some(id),
        json!({"from": set.status}),
    )
    .await?;
    super::proposals::return_to_pending(
        &mut tx,
        ctx,
        &set.gtfs_id,
        id,
        None,
        "change_set_discarded",
    )
    .await?;
    super::position_reviews::draft_discarded(&mut tx, ctx, &set.gtfs_id, id).await?;
    tx.commit().await?;
    Ok(())
}

/// Apply an approved set. One transaction: lock the feed, check every change
/// against the live rows, apply, bump the feed version, record it. Any conflict
/// or error rolls everything back - so a retry after a serialization failure
/// starts from nothing.
pub async fn commit(state: &EditorState, ctx: &Ctx, id: Uuid) -> EditorResult<Value> {
    retry_transient(|| commit_once(state, ctx, id)).await
}

async fn commit_once(state: &EditorState, ctx: &Ctx, id: Uuid) -> EditorResult<Value> {
    let mut tx = state.pool.begin().await?;
    // the feed's advisory lock first (every replay of the feed queues on it),
    // then its row, then the set: every commit on a feed takes locks in this order
    let gtfs_id = lock_feed_of_set(&mut tx, id).await?;
    sqlx::query("SELECT version FROM gtfs_feed WHERE gtfs_id = $1 FOR UPDATE")
        .bind(&gtfs_id)
        .fetch_one(&mut *tx)
        .await?;
    let set = load_set(&mut tx, id, true).await?;
    if set.status != "approved" {
        return Err(EditorError::conflict(
            "change_set_not_approved",
            format!(
                "the change set is {}; only an approved set can be committed",
                set.status
            ),
        ));
    }
    // maker-checker covers commit too: whoever submitted cannot put it live,
    // unless they are the admin who approved it themselves
    if set.submitted_by == Some(ctx.user.user_id)
        && !(set.self_approved && ctx.user.role() >= auth::Role::Admin)
    {
        return Err(own_change_set(false));
    }
    let changes = load_changes(&mut tx, id).await?;
    let ev = evaluate(&mut tx, &gtfs_id, &changes, &ctx.user.email).await?;
    if !ev.conflicts.is_empty() {
        tx.rollback().await?;
        return Err(EditorError::conflict(
            "change_set_conflicts",
            "the live data changed since this set was made; reopen it and rebase the edits",
        )
        .with_details(json!({"conflicts": ev.conflicts})));
    }
    if ev.has_errors() {
        tx.rollback().await?;
        return Err(EditorError::bad_request(
            "validation_failed",
            "the change set no longer applies cleanly",
        )
        .with_details(json!({"validation": ev.validation})));
    }
    let version: i64 = sqlx::query(
        "UPDATE gtfs_feed SET version = version + 1 WHERE gtfs_id = $1 RETURNING version",
    )
    .bind(&gtfs_id)
    .fetch_one(&mut *tx)
    .await?
    .try_get("version")?;
    sqlx::query(
        "UPDATE gtfs_change_set SET status = 'committed', committed_by = $2, committed_at = now(), \
            committed_version = $3 WHERE change_set_id = $1",
    )
    .bind(id)
    .bind(ctx.user.user_id)
    .bind(version)
    .execute(&mut *tx)
    .await?;
    let summary: Vec<Value> = changes
        .iter()
        .take(200)
        .map(|c| json!([c.entity, c.op, c.entity_key]))
        .collect();
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "change_set_committed",
        Some(&gtfs_id),
        Some(id),
        json!({
            "feed_version": version, "changes": changes.len(), "applied": summary,
            "self_approved": set.self_approved,
        }),
    )
    .await?;
    auth::audit_many(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "stop_merged",
        Some(&gtfs_id),
        Some(id),
        &ev.merges,
    )
    .await?;
    let switched: Vec<Value> = ev
        .feed_configs
        .iter()
        .cloned()
        .map(|mut d| {
            d["change_set_id"] = json!(id);
            d
        })
        .collect();
    auth::audit_many(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "feed_data_source_changed",
        Some(&gtfs_id),
        Some(id),
        &switched,
    )
    .await?;
    super::proposals::mark_committed(&mut tx, ctx, &gtfs_id, id, version).await?;
    super::position_reviews::mark_committed(&mut tx, ctx, &gtfs_id, id, version).await?;
    tx.commit().await?;
    Ok(json!({"change_set_id": id, "status": "committed", "feed_version": version}))
}

// ---------------------------------------------------------------- audit

pub async fn audit_list(
    state: &EditorState,
    gtfs_id: &str,
    change_set: Option<Uuid>,
    page: &Page,
) -> EditorResult<Value> {
    let rows = sqlx::query(
        "SELECT audit_id, at, actor, actor_email, action, gtfs_id, change_set_id, detail::text AS detail \
         FROM gtfs_audit_log WHERE gtfs_id = $1 AND ($2::uuid IS NULL OR change_set_id = $2) \
         ORDER BY at DESC, audit_id DESC LIMIT $3 OFFSET $4",
    )
    .bind(gtfs_id)
    .bind(change_set)
    .bind(page.limit + 1)
    .bind(page.offset)
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            Ok(json!({
                "audit_id": r.try_get::<i64, _>("audit_id")?,
                "at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("at")?,
                "actor": r.try_get::<Option<Uuid>, _>("actor")?,
                "actor_email": r.try_get::<Option<String>, _>("actor_email")?,
                "action": r.try_get::<String, _>("action")?,
                "gtfs_id": r.try_get::<Option<String>, _>("gtfs_id")?,
                "change_set_id": r.try_get::<Option<Uuid>, _>("change_set_id")?,
                "detail": json_col(r, "detail")?,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(page.wrap(items))
}

// ---------------------------------------------------------------- users

pub async fn users(state: &EditorState) -> EditorResult<Value> {
    let rows = sqlx::query(
        "SELECT user_id, email, display_name, role, totp_enabled, totp_last_step, NULL::bytea AS totp_secret_enc, \
                status, created_at, last_login_at FROM gtfs_editor_user ORDER BY lower(email)",
    )
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            Ok(json!({
                "user_id": r.try_get::<Uuid, _>("user_id")?,
                "email": r.try_get::<String, _>("email")?,
                "display_name": r.try_get::<Option<String>, _>("display_name")?,
                "role": r.try_get::<String, _>("role")?,
                "status": r.try_get::<String, _>("status")?,
                "totp_enabled": r.try_get::<bool, _>("totp_enabled")?,
                "created_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?,
                "last_login_at": r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_login_at")?,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"items": items, "next_cursor": null}))
}

pub async fn create_user(
    state: &EditorState,
    ctx: &Ctx,
    email: &str,
    display_name: Option<&str>,
    role: &str,
) -> EditorResult<Value> {
    let email = email.trim().to_ascii_lowercase();
    if !email.contains('@') || email.len() > 254 {
        return Err(EditorError::bad_request(
            "invalid_email",
            "email is not valid",
        ));
    }
    if auth::Role::parse(role).is_none() {
        return Err(EditorError::bad_request(
            "invalid_role",
            "role is viewer, editor, approver or admin",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "INSERT INTO gtfs_editor_user (email, display_name, role) VALUES ($1, $2, $3) \
         ON CONFLICT DO NOTHING RETURNING user_id",
    )
    .bind(&email)
    .bind(display_name.map(str::trim).filter(|d| !d.is_empty()))
    .bind(role)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| {
        EditorError::conflict("user_exists", format!("{email} already has an account"))
    })?;
    let user_id: Uuid = row.try_get("user_id")?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "user_created",
        None,
        None,
        json!({"user_id": user_id, "email": email, "role": role}),
    )
    .await?;
    tx.commit().await?;
    Ok(
        json!({"user_id": user_id, "email": email, "role": role, "status": "active", "totp_enabled": false}),
    )
}

pub async fn update_user(
    state: &EditorState,
    ctx: &Ctx,
    user_id: Uuid,
    role: Option<&str>,
    status: Option<&str>,
) -> EditorResult<()> {
    if role.is_none() && status.is_none() {
        return Err(EditorError::bad_request(
            "nothing_to_change",
            "send role or status",
        ));
    }
    if let Some(r) = role {
        if auth::Role::parse(r).is_none() {
            return Err(EditorError::bad_request(
                "invalid_role",
                "role is viewer, editor, approver or admin",
            ));
        }
    }
    if let Some(s) = status {
        if !["active", "disabled"].contains(&s) {
            return Err(EditorError::bad_request(
                "invalid_status",
                "status is active or disabled",
            ));
        }
    }
    if user_id == ctx.user.user_id
        && (role.is_some_and(|r| r != "admin") || status.is_some_and(|s| s != "active"))
    {
        return Err(EditorError::bad_request(
            "cannot_change_self",
            "an admin cannot demote or disable their own account",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let n = sqlx::query(
        "UPDATE gtfs_editor_user SET role = coalesce($2, role), status = coalesce($3, status) WHERE user_id = $1",
    )
    .bind(user_id)
    .bind(role)
    .bind(status)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(EditorError::not_found("user_not_found", "no such user"));
    }
    if status == Some("disabled") {
        sqlx::query("DELETE FROM gtfs_editor_session WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "user_updated",
        None,
        None,
        json!({"user_id": user_id, "role": role, "status": status}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn reset_totp(state: &EditorState, ctx: &Ctx, user_id: Uuid) -> EditorResult<()> {
    let mut tx = state.pool.begin().await?;
    let n = sqlx::query(
        "UPDATE gtfs_editor_user SET totp_secret_enc = NULL, totp_enabled = false, totp_last_step = NULL \
         WHERE user_id = $1",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(EditorError::not_found("user_not_found", "no such user"));
    }
    sqlx::query("DELETE FROM gtfs_editor_session WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "user_totp_reset",
        None,
        None,
        json!({"user_id": user_id}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trip_and_limits() {
        let p = Page::parse(Some(2), None).unwrap();
        let wrapped = p.wrap(vec![json!(1), json!(2), json!(3)]);
        assert_eq!(wrapped["items"].as_array().unwrap().len(), 2);
        let next = wrapped["next_cursor"].as_str().unwrap().to_string();
        let p2 = Page::parse(Some(2), Some(&next)).unwrap();
        assert_eq!(p2.offset, 2);
        assert!(p2.wrap(vec![json!(3)])["next_cursor"].is_null());
        assert!(Page::parse(Some(0), None).is_err());
        assert!(Page::parse(Some(501), None).is_err());
        assert!(Page::parse(None, Some("garbage!")).is_err());
    }

    #[test]
    fn like_patterns_escape_wildcards() {
        assert_eq!(like_pattern("50%_off"), "%50\\%\\_off%");
    }

    /// A deadlock inside a change's savepoint is the transaction's to retry,
    /// never the change's `database_rejected` finding; a constraint still is.
    #[test]
    fn a_deadlock_in_a_savepoint_is_a_retry_not_a_finding() {
        use super::super::feed_lock::{tests::db_error, TRY_AGAIN};
        let e = db_failure(db_error("40P01", "deadlock detected")).unwrap_err();
        assert_eq!((e.status.as_u16(), e.code), (503, TRY_AGAIN));
        let e = db_failure(db_error("40001", "could not serialize access")).unwrap_err();
        assert_eq!((e.status.as_u16(), e.code), (503, TRY_AGAIN));
        let f = db_failure(db_error("23503", "violates foreign key")).unwrap();
        assert_eq!(
            (f.code.as_str(), f.message.as_str()),
            ("foreign_key_violation", "violates foreign key")
        );
        let f = db_failure(db_error("22P02", "invalid input syntax")).unwrap();
        assert_eq!(f.code, "database_rejected");
    }

    #[test]
    fn a_new_routes_base_hash_is_the_hash_of_an_empty_list() {
        // documented in docs/gtfs-editor.md section 5
        assert_eq!(
            rows_hash(&[]),
            "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
        );
    }

    fn change(id: i64, entity: &str, op: &str, key: &str, after: Value) -> ChangeRow {
        ChangeRow {
            change_id: id,
            position: id as i32,
            entity: entity.into(),
            entity_key: key.into(),
            op: op.into(),
            base_row_version: None,
            before: Value::Null,
            after,
            created_by: Uuid::nil(),
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn stops_a_change_uses() {
        let c = change(
            1,
            "route_stops",
            "replace",
            "R1",
            json!({"base_rows_hash": "x", "rows": [
            {"stop_id": " B ", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "X"},
            {"stop_id": "A", "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "X"},
            {"stop_type": "ROUTE CORRECTION", "stage_no": 1, "stage_name": "X"},
            {"stop_id": "B", "stop_type": "NEW STOP", "stage_no": 2, "stage_name": "Y"}]}),
        );
        assert_eq!(referenced_stops(&c), vec!["A", "B"]);
        let c = change(2, "stop", "merge", "A", json!({"into_stop_id": "B"}));
        assert_eq!(referenced_stops(&c), vec!["A", "B"]);
        let c = change(
            3,
            "station",
            "update",
            "S",
            json!({"members": [{"stop_id": "C"}]}),
        );
        assert_eq!(referenced_stops(&c), vec!["C"]);
        assert!(referenced_stops(&change(4, "stop", "create", "N", json!({}))).is_empty());
    }

    #[test]
    fn route_changes_that_block_a_delete() {
        let changes = vec![
            change(1, "route", "update", "R1", json!({"color": "#000000"})),
            change(2, "route_stops", "replace", "R2", json!({})),
            change(3, "route", "delete", "R1", Value::Null),
            change(4, "stop", "update", "R1", json!({"name": "x"})),
        ];
        assert_eq!(other_route_changes(&changes, "R1", 3), vec![1]);
        assert_eq!(other_route_changes(&changes, "R2", 0), vec![2]);
        assert!(other_route_changes(&changes, "R3", 0).is_empty());
    }

    #[test]
    fn rows_hash_is_order_sensitive() {
        let a = RouteRow {
            stop_id: Some("A".into()),
            stop_type: "NEW STOP".into(),
            stage_no: 1,
            stage_name: "X".into(),
            marker_id: None,
            marker_name: None,
            marker_lat: None,
            marker_lon: None,
            stop_name_override: None,
            provider_id: None,
        };
        let mut b = a.clone();
        b.stop_id = Some("B".into());
        assert_ne!(
            rows_hash(&[a.clone(), b.clone()]),
            rows_hash(&[b, a.clone()])
        );
        assert_eq!(rows_hash(std::slice::from_ref(&a)), rows_hash(&[a]));
    }
}
