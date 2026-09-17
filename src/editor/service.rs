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

use super::auth::{self, Ctx};
use super::error::{EditorError, EditorResult};
use super::validation::{
    check_payload, check_route_rows, grade_against_live, haversine_m, Finding, Level, RouteRow,
    MOVE_WARNING_METRES,
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

fn like_pattern(q: &str) -> String {
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
    Ok(json!({"items": items}))
}

// ---------------------------------------------------------------- stops

const STOP_COLS: &str = "stop_id, stop_code, name, lat, lon, location_type, parent_station, \
     platform_code, cluster_id, regional_name, hindi_name, info_json::text AS info_json, \
     position_source, provenance::text AS provenance, deleted, row_version, updated_at, updated_by";

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
        "SELECT {STOP_COLS}, \
            (SELECT count(DISTINCT rs.route_id) FROM gtfs_route_stop rs \
              WHERE rs.gtfs_id = s.gtfs_id AND rs.stop_id = s.stop_id) AS route_count \
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
        .map(|r| -> Result<Value, sqlx::Error> {
            let mut v = stop_json(r)?;
            v["route_count"] = json!(r.try_get::<i64, _>("route_count")?);
            Ok(v)
        })
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
        "SELECT {STOP_COLS} FROM gtfs_stop \
         WHERE gtfs_id = $1 AND parent_station = $2 AND NOT deleted ORDER BY stop_id"
    ))
    .bind(gtfs_id)
    .bind(stop_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(stop_json)
    .collect::<Result<Vec<_>, _>>()?;

    let (lat, lon) = (
        stop["lat"].as_f64().unwrap_or(0.0),
        stop["lon"].as_f64().unwrap_or(0.0),
    );
    let nearby = sqlx::query(&format!(
        "SELECT {STOP_COLS} FROM gtfs_stop \
         WHERE gtfs_id = $1 AND stop_id <> $2 AND NOT deleted \
           AND lat BETWEEN $3 - 0.0006 AND $3 + 0.0006 AND lon BETWEEN $4 - 0.0007 AND $4 + 0.0007"
    ))
    .bind(gtfs_id)
    .bind(stop_id)
    .bind(lat)
    .bind(lon)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(stop_json)
    .collect::<Result<Vec<_>, _>>()?
    .into_iter()
    .filter_map(|mut s| {
        let d = haversine_m(lat, lon, s["lat"].as_f64()?, s["lon"].as_f64()?);
        (d <= 60.0).then(|| {
            s["metres"] = json!((d * 10.0).round() / 10.0);
            s
        })
    })
    .collect::<Vec<_>>();
    stop["routes"] = json!(routes);
    stop["children"] = json!(children);
    stop["nearby"] = json!(nearby);
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

pub async fn list_routes(
    state: &EditorState,
    gtfs_id: &str,
    q: Option<&str>,
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
         ORDER BY (r.route_id = $2 OR lower(r.short_name) = lower($2)) DESC NULLS LAST, \
                  r.short_name, r.route_id \
         LIMIT $4 OFFSET $5"
    ))
    .bind(gtfs_id)
    .bind(q)
    .bind(q.map(like_pattern))
    .bind(page.limit + 1)
    .bind(page.offset)
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

pub async fn load_route_rows(
    conn: &mut PgConnection,
    gtfs_id: &str,
    route_id: &str,
) -> EditorResult<Vec<RouteRow>> {
    let rows = sqlx::query(
        "SELECT stop_id, stop_type, stage_no, stage_name, marker_id, marker_name, marker_lat, \
                marker_lon, stop_name_override, provider_id \
         FROM gtfs_route_stop WHERE gtfs_id = $1 AND route_id = $2 ORDER BY sequence",
    )
    .bind(gtfs_id)
    .bind(route_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| -> Result<RouteRow, sqlx::Error> {
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
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
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
    let rows = sqlx::query(
        "SELECT rs.sequence, rs.stop_id, s.name AS stop_name, s.lat, s.lon, s.deleted AS stop_deleted, \
                rs.stop_type, rs.stage_no, rs.stage_name, rs.marker_id, rs.marker_name, rs.marker_lat, \
                rs.marker_lon, rs.stop_name_override, rs.provider_id \
         FROM gtfs_route_stop rs \
         LEFT JOIN gtfs_stop s ON s.gtfs_id = rs.gtfs_id AND s.stop_id = rs.stop_id \
         WHERE rs.gtfs_id = $1 AND rs.route_id = $2 ORDER BY rs.sequence",
    )
    .bind(gtfs_id)
    .bind(route_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        Ok(json!({
            "sequence": r.try_get::<i32, _>("sequence")?,
            "stop_id": r.try_get::<Option<String>, _>("stop_id")?,
            "stop_name": r.try_get::<Option<String>, _>("stop_name")?,
            "lat": r.try_get::<Option<f64>, _>("lat")?,
            "lon": r.try_get::<Option<f64>, _>("lon")?,
            "stop_deleted": r.try_get::<Option<bool>, _>("stop_deleted")?,
            "stop_type": r.try_get::<String, _>("stop_type")?,
            "stage_no": r.try_get::<i32, _>("stage_no")?,
            "stage_name": r.try_get::<String, _>("stage_name")?,
            "marker_id": r.try_get::<Option<String>, _>("marker_id")?,
            "marker_name": r.try_get::<Option<String>, _>("marker_name")?,
            "marker_lat": r.try_get::<Option<f64>, _>("marker_lat")?,
            "marker_lon": r.try_get::<Option<f64>, _>("marker_lon")?,
            "stop_name_override": r.try_get::<Option<String>, _>("stop_name_override")?,
            "provider_id": r.try_get::<Option<String>, _>("provider_id")?,
        }))
    })
    .collect::<Result<Vec<_>, _>>()?;
    let hash = rows_hash(&load_route_rows(conn, gtfs_id, route_id).await?);
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
    pub json: Value,
}

const SET_SELECT: &str = "SELECT cs.change_set_id, cs.gtfs_id, cs.title, cs.description, cs.status, \
        cs.created_by, cu.email AS created_by_email, cs.created_at, cs.updated_at, \
        cs.submitted_by, su.email AS submitted_by_email, cs.submitted_at, \
        cs.reviewed_by, ru.email AS reviewed_by_email, cs.reviewed_at, cs.review_comment, \
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
        "{SET_SELECT} WHERE cs.gtfs_id = $1 AND ($2::text IS NULL OR cs.status = $2) \
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

// ---------------------------------------------------------------- evaluate

#[derive(Debug, Default)]
pub struct Evaluation {
    pub validation: Vec<Value>,
    pub conflicts: Vec<Value>,
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

fn conflict(c: &ChangeRow, reason: &str, expected: Value, actual: Value) -> Value {
    json!({
        "change_id": c.change_id, "entity": c.entity, "entity_key": c.entity_key,
        "reason": reason, "expected": expected, "actual": actual,
    })
}

/// Has the live row moved on since the change was made?
async fn conflict_for(
    conn: &mut PgConnection,
    g: &str,
    c: &ChangeRow,
) -> EditorResult<Option<Value>> {
    let version_of = |table: &'static str, id_col: &'static str| {
        format!("SELECT row_version FROM {table} WHERE gtfs_id = $1 AND {id_col} = $2")
    };
    Ok(match (c.entity.as_str(), c.op.as_str()) {
        ("stop", "update" | "delete") | ("station", "update" | "delete") | ("route", "update") => {
            let sql = if c.entity == "route" {
                version_of("gtfs_route", "route_id")
            } else {
                version_of("gtfs_stop", "stop_id")
            };
            let live: Option<i32> = sqlx::query(&sql)
                .bind(g)
                .bind(&c.entity_key)
                .fetch_optional(&mut *conn)
                .await?
                .map(|r| r.try_get("row_version"))
                .transpose()?;
            match (live, c.base_row_version) {
                (None, _) => Some(conflict(
                    c,
                    "missing",
                    json!(c.base_row_version),
                    Value::Null,
                )),
                (Some(v), Some(b)) if v != b => Some(conflict(c, "changed", json!(b), json!(v))),
                _ => None,
            }
        }
        ("stop", "create") | ("station", "create") => {
            let exists = sqlx::query("SELECT 1 FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = $2")
                .bind(g)
                .bind(&c.entity_key)
                .fetch_optional(&mut *conn)
                .await?
                .is_some();
            exists.then(|| conflict(c, "exists", Value::Null, json!(c.entity_key)))
        }
        ("route_stops", "replace") => {
            let expected = c.after["base_rows_hash"].as_str().unwrap_or("").to_string();
            let actual = rows_hash(&load_route_rows(conn, g, &c.entity_key).await?);
            (expected != actual).then(|| conflict(c, "changed", json!(expected), json!(actual)))
        }
        _ => None,
    })
}

/// Apply every change in order inside `conn`'s transaction; see the module doc.
pub async fn evaluate(
    conn: &mut PgConnection,
    g: &str,
    changes: &[ChangeRow],
    actor: &str,
) -> EditorResult<Evaluation> {
    let mut ev = Evaluation::default();
    for c in changes {
        if let Some(x) = conflict_for(conn, g, c).await? {
            ev.conflicts.push(x);
        }
    }
    for c in changes {
        sqlx::query("SAVEPOINT editor_change")
            .execute(&mut *conn)
            .await?;
        match apply_change(conn, g, c, actor).await {
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
                    ApplyError::Db(e) => vec![db_finding(&e)],
                };
                ev.validation
                    .extend(findings.iter().map(|f| finding_json(Some(c.change_id), f)));
            }
        }
    }
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
            ev.validation.push(finding_json(None, &db_finding(&e)));
        }
    }
    Ok(ev)
}

async fn apply_change(
    conn: &mut PgConnection,
    g: &str,
    c: &ChangeRow,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    check_payload(&c.entity, &c.op, &c.entity_key, &c.after)
        .map_err(|f| ApplyError::Findings(vec![f]))?;
    let key = c.entity_key.as_str();
    match (c.entity.as_str(), c.op.as_str()) {
        ("stop", "update") => stop_update(conn, g, key, &c.after, actor).await,
        ("stop", "create") => stop_create(conn, g, &c.after, actor).await,
        ("stop", "delete") => stop_delete(conn, g, key, actor).await,
        ("route", "update") => route_update(conn, g, key, &c.after, actor).await,
        ("route_stops", "replace") => route_stops_replace(conn, g, key, &c.after, actor).await,
        ("station", "create") => station_create(conn, g, &c.after, actor).await,
        ("station", "update") => station_update(conn, g, key, &c.after, actor).await,
        ("station", "delete") => station_delete(conn, g, key, actor).await,
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
    let (has_platform, platform) = field(m, "platform_code");
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
                                info_json, regional_name, hindi_name, position_source, updated_by) \
         VALUES ($1, $2, coalesce($3, $2), $4, $5, $6, $7, $8, \
                 CASE WHEN $8::text IS NULL THEN NULL ELSE jsonb_build_object('clusterId', $8::text) END, \
                 $9, $10, 'REVIEW', $11) \
         ON CONFLICT DO NOTHING RETURNING stop_id",
    )
    .bind(g)
    .bind(s("stop_id"))
    .bind(s("stop_code"))
    .bind(s("name").map(str::trim))
    .bind(m.get("lat").and_then(Value::as_f64))
    .bind(m.get("lon").and_then(Value::as_f64))
    .bind(s("platform_code"))
    .bind(s("cluster_id"))
    .bind(s("regional_name"))
    .bind(s("hindi_name"))
    .bind(actor)
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
        "SELECT DISTINCT route_id FROM gtfs_route_stop WHERE gtfs_id = $1 AND stop_id = $2 ORDER BY route_id",
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

#[derive(Deserialize)]
struct RouteStopsPayload {
    rows: Vec<RouteRow>,
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
    findings.extend(grade_against_live(
        check_route_rows(&rows),
        &check_route_rows(&live),
    ));
    if findings.iter().any(|f| f.level == Level::Error) {
        return Err(ApplyError::Findings(findings));
    }

    let n = rows.len();
    let mut seq = Vec::with_capacity(n);
    let (mut stop, mut typ, mut stage_no, mut stage_name) = (vec![], vec![], vec![], vec![]);
    let (mut mid, mut mname, mut mlat, mut mlon, mut over, mut prov) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
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
        prov.push(r.provider_id.clone());
    }
    sqlx::query("DELETE FROM gtfs_route_stop WHERE gtfs_id = $1 AND route_id = $2")
        .bind(g)
        .bind(route_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name, \
                                      marker_id, marker_name, marker_lat, marker_lon, stop_name_override, \
                                      provider_id, updated_by) \
         SELECT $1, $2, u.* , $14 FROM UNNEST($3::int4[], $4::text[], $5::text[], $6::int4[], $7::text[], \
                $8::text[], $9::text[], $10::float8[], $11::float8[], $12::text[], $13::text[]) AS u",
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
    .execute(&mut *conn)
    .await?;
    Ok(findings)
}

fn members_of(m: &Map<String, Value>) -> Option<Vec<String>> {
    m.get("member_stop_ids")?.as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    })
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
    let members = members_of(m).unwrap_or_default();
    check_members(conn, g, id, &members).await?;
    let created = sqlx::query(
        "INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, location_type, position_source, updated_by) \
         VALUES ($1, $2, $2, $3, $4, $5, 1, 'REVIEW', $6) ON CONFLICT DO NOTHING RETURNING stop_id",
    )
    .bind(g)
    .bind(id)
    .bind(m.get("name").and_then(Value::as_str).map(str::trim))
    .bind(m.get("lat").and_then(Value::as_f64))
    .bind(m.get("lon").and_then(Value::as_f64))
    .bind(actor)
    .fetch_optional(&mut *conn)
    .await?;
    if created.is_none() {
        return Err(fail(
            "station_exists",
            format!("a stop or station {id} already exists"),
        ));
    }
    sqlx::query(
        "UPDATE gtfs_stop SET parent_station = $3, updated_by = $4 WHERE gtfs_id = $1 AND stop_id = ANY($2)",
    )
    .bind(g)
    .bind(&members)
    .bind(id)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(if members.is_empty() {
        vec![Finding::warning(
            "station_empty",
            id,
            format!("station {id} has no stops"),
        )]
    } else {
        vec![]
    })
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
    let moved = m.contains_key("lat");
    sqlx::query(
        "UPDATE gtfs_stop SET name = CASE WHEN $3 THEN $4 ELSE name END, \
            lat = CASE WHEN $5 THEN $6 ELSE lat END, lon = CASE WHEN $5 THEN $7 ELSE lon END, \
            updated_by = $8 \
         WHERE gtfs_id = $1 AND stop_id = $2 AND ($3 OR $5)",
    )
    .bind(g)
    .bind(id)
    .bind(has_name)
    .bind(name.map(str::trim))
    .bind(moved)
    .bind(m.get("lat").and_then(Value::as_f64).unwrap_or(live.lat))
    .bind(m.get("lon").and_then(Value::as_f64).unwrap_or(live.lon))
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    let mut warnings = Vec::new();
    if let Some(members) = members_of(m) {
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
        sqlx::query(
            "UPDATE gtfs_stop SET parent_station = $2, updated_by = $4 \
             WHERE gtfs_id = $1 AND stop_id = ANY($3) AND parent_station IS DISTINCT FROM $2",
        )
        .bind(g)
        .bind(id)
        .bind(&members)
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        if members.is_empty() {
            warnings.push(Finding::warning(
                "station_empty",
                id,
                format!("station {id} has no stops"),
            ));
        }
    }
    Ok(warnings)
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

// ---------------------------------------------------------------- set detail

/// The set, its changes, and - unless it is finished - what applying it now
/// would find. Runs the changes in a transaction that is rolled back.
pub async fn set_detail(state: &EditorState, ctx: &Ctx, id: Uuid) -> EditorResult<Value> {
    let mut tx = state.pool.begin().await?;
    let set = load_set(&mut tx, id, false).await?;
    let changes = load_changes(&mut tx, id).await?;
    let ev = if matches!(set.status.as_str(), "committed" | "discarded") {
        Evaluation::default()
    } else {
        evaluate(&mut tx, &set.gtfs_id, &changes, &ctx.user.email).await?
    };
    tx.rollback().await?;
    let mut out = set.json;
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
    let mut tx = state.pool.begin().await?;
    let set = load_set(&mut tx, id, false).await?;
    let changes = load_changes(&mut tx, id).await?;
    let ev = evaluate(&mut tx, &set.gtfs_id, &changes, &ctx.user.email).await?;
    let route = route_detail(&mut tx, &set.gtfs_id, route_id).await;
    tx.rollback().await?;
    Ok(json!({"route": route?, "validation": ev.validation, "conflicts": ev.conflicts}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewChange {
    pub entity: String,
    pub op: String,
    pub entity_key: String,
    #[serde(default)]
    pub after: Value,
    #[serde(default)]
    pub base_row_version: Option<i32>,
}

fn editable(set: &ChangeSet) -> EditorResult<()> {
    if set.status == "draft" {
        Ok(())
    } else {
        Err(EditorError::conflict(
            "change_set_not_draft",
            format!("the change set is {}; reopen it to edit", set.status),
        ))
    }
}

/// Snapshot of what a change replaces, and the version it is based on.
async fn snapshot(
    conn: &mut PgConnection,
    g: &str,
    entity: &str,
    op: &str,
    key: &str,
) -> EditorResult<(Value, Option<i32>)> {
    match (entity, op) {
        ("stop", "update" | "delete") | ("station", "update" | "delete") => {
            let row = stop_row(conn, g, key).await?.ok_or_else(|| {
                EditorError::not_found("entity_not_found", format!("no {entity} {key}"))
            })?;
            let is_station = row["location_type"] == 1;
            if (entity == "station") != is_station {
                return Err(EditorError::bad_request(
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
                ));
            }
            let v = row["row_version"].as_i64().map(|v| v as i32);
            Ok((row, v))
        }
        ("route", "update") => {
            let row = route_row(conn, g, key).await?.ok_or_else(|| {
                EditorError::not_found("entity_not_found", format!("no route {key}"))
            })?;
            let v = row["row_version"].as_i64().map(|v| v as i32);
            Ok((row, v))
        }
        ("route_stops", "replace") => {
            if route_row(conn, g, key).await?.is_none() {
                return Err(EditorError::not_found(
                    "entity_not_found",
                    format!("no route {key}"),
                ));
            }
            let rows = load_route_rows(conn, g, key).await?;
            let hash = rows_hash(&rows);
            Ok((json!({"rows": rows, "rows_hash": hash}), None))
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
    let key = change.entity_key.trim().to_string();
    if key.is_empty() {
        return Err(EditorError::bad_request(
            "entity_key_required",
            "entity_key is required",
        ));
    }
    check_payload(&change.entity, &change.op, &key, &change.after).map_err(|f| {
        EditorError::bad_request("invalid_change", f.message.clone())
            .with_details(json!({"code": f.code}))
    })?;
    let mut tx = state.pool.begin().await?;
    let set = load_set(&mut tx, id, true).await?;
    editable(&set)?;
    let (before, live_version) =
        snapshot(&mut tx, &set.gtfs_id, &change.entity, &change.op, &key).await?;
    let base = change.base_row_version.or(live_version);
    let change_id: i64 = sqlx::query(
        "INSERT INTO gtfs_change (change_set_id, position, entity, entity_key, op, base_row_version, before, after, created_by) \
         VALUES ($1, (SELECT coalesce(max(position), 0) + 1 FROM gtfs_change WHERE change_set_id = $1), \
                 $2, $3, $4, $5, $6::jsonb, $7::jsonb, $8) RETURNING change_id",
    )
    .bind(id)
    .bind(&change.entity)
    .bind(&key)
    .bind(&change.op)
    .bind(base)
    .bind((!before.is_null()).then(|| before.to_string()))
    .bind((!change.after.is_null()).then(|| change.after.to_string()))
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
        json!({"change_id": change_id, "entity": change.entity, "op": change.op, "entity_key": key}),
    )
    .await?;
    tx.commit().await?;
    Ok(change_id)
}

#[derive(Debug, Deserialize)]
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
    let mut tx = state.pool.begin().await?;
    let set = load_set(&mut tx, id, true).await?;
    editable(&set)?;
    let row = sqlx::query(
        "SELECT entity, op, entity_key FROM gtfs_change WHERE change_set_id = $1 AND change_id = $2",
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
    let mut tx = state.pool.begin().await?;
    let set = load_set(&mut tx, id, true).await?;
    editable(&set)?;
    let n = sqlx::query("DELETE FROM gtfs_change WHERE change_set_id = $1 AND change_id = $2")
        .bind(id)
        .bind(change_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(EditorError::not_found(
            "change_not_found",
            "no such change in this set",
        ));
    }
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
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------- transitions

pub async fn submit(state: &EditorState, ctx: &Ctx, id: Uuid) -> EditorResult<()> {
    let mut tx = state.pool.begin().await?;
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
            reviewed_by = NULL, reviewed_at = NULL, review_comment = NULL \
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
/// a set they submitted, whatever their role.
fn not_own(set: &ChangeSet, ctx: &Ctx) -> EditorResult<()> {
    if set.submitted_by == Some(ctx.user.user_id) {
        Err(EditorError::forbidden(
            "own_change_set",
            "a change set must be reviewed by someone other than the person who submitted it",
        ))
    } else {
        Ok(())
    }
}

pub async fn review(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    approve: bool,
    comment: Option<&str>,
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
    not_own(&set, ctx)?;
    sqlx::query(
        "UPDATE gtfs_change_set SET status = $2, reviewed_by = $3, reviewed_at = now(), review_comment = $4 \
         WHERE change_set_id = $1",
    )
    .bind(id)
    .bind(if approve { "approved" } else { "rejected" })
    .bind(ctx.user.user_id)
    .bind(comment)
    .execute(&mut *tx)
    .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        if approve {
            "change_set_approved"
        } else {
            "change_set_rejected"
        },
        Some(&set.gtfs_id),
        Some(id),
        json!({"comment": comment}),
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
            reviewed_by = NULL, reviewed_at = NULL WHERE change_set_id = $1",
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
    tx.commit().await?;
    Ok(())
}

/// Apply an approved set. One transaction: lock the feed, check every change
/// against the live rows, apply, bump the feed version, record it. Any conflict
/// or error rolls everything back.
pub async fn commit(state: &EditorState, ctx: &Ctx, id: Uuid) -> EditorResult<Value> {
    let mut tx = state.pool.begin().await?;
    let gtfs_id: String =
        sqlx::query("SELECT gtfs_id FROM gtfs_change_set WHERE change_set_id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| EditorError::not_found("change_set_not_found", "no such change set"))?
            .try_get("gtfs_id")?;
    // feed first, then the set: every commit on a feed takes locks in this order
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
        json!({"feed_version": version, "changes": changes.len(), "applied": summary}),
    )
    .await?;
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
    Ok(json!({"items": items}))
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
