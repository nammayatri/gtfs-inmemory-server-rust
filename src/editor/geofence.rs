//! Stop geofences (docs/gtfs-editor.md section 24): a surveyed boundary
//! around a stop, read by gps-processor to detect a vehicle's arrival, dwell
//! and departure there. Defaults to a circle (`lat`, `lon`, `radius_m`); a
//! custom polygon (`polygon`, >= 3 points) is there for a depot fence or a
//! stop footprint a disc does not fit.
//!
//! One row per stop, keyed by `stop_id` like [`super::trips`]'s `service` is
//! by `service_id`: a stop has at most one surveyed boundary at a time. The
//! row simply does not exist until the field survey happens.

use super::error::EditorResult;
use super::service::{fail, json_col, ApplyError};
use super::validation::{valid_lat_lon, Finding};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

// ---------------------------------------------------------------- reads

const GEOFENCE_COLS: &str = "g.stop_id, s.name AS stop_name, g.lat, g.lon, g.shape, g.radius_m, \
     g.polygon_lat, g.polygon_lon, \
     g.row_version, g.created_at, g.created_by, g.updated_at, g.updated_by";

/// `polygon_lat`/`polygon_lon` as the API's `[{lat, lon}, ...]`, matching how
/// a shape's points already read (`draft_import::shape_points`).
fn polygon_points(lat: &Option<Vec<f64>>, lon: &Option<Vec<f64>>) -> Option<Value> {
    let (lat, lon) = (lat.as_ref()?, lon.as_ref()?);
    Some(json!(lat
        .iter()
        .zip(lon)
        .map(|(la, lo)| json!({"lat": la, "lon": lo}))
        .collect::<Vec<_>>()))
}

async fn geofences_json(
    conn: &mut PgConnection,
    g: &str,
    only: Option<&str>,
) -> EditorResult<Vec<Value>> {
    sqlx::query(&format!(
        "SELECT {GEOFENCE_COLS} FROM gtfs_geofence g JOIN gtfs_stop s \
            ON s.gtfs_id = g.gtfs_id AND s.stop_id = g.stop_id \
         WHERE g.gtfs_id = $1 AND ($2::text IS NULL OR g.stop_id = $2) \
         ORDER BY g.stop_id"
    ))
    .bind(g)
    .bind(only)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        let poly_lat: Option<Vec<f64>> = r.try_get("polygon_lat")?;
        let poly_lon: Option<Vec<f64>> = r.try_get("polygon_lon")?;
        Ok(json!({
            "stop_id": r.try_get::<String, _>("stop_id")?,
            "stop_name": r.try_get::<String, _>("stop_name")?,
            "lat": r.try_get::<f64, _>("lat")?,
            "lon": r.try_get::<f64, _>("lon")?,
            "shape": r.try_get::<String, _>("shape")?,
            "radius_m": r.try_get::<Option<i32>, _>("radius_m")?,
            "polygon": polygon_points(&poly_lat, &poly_lon),
            "row_version": r.try_get::<i32, _>("row_version")?,
            "created_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?,
            "created_by": r.try_get::<Option<String>, _>("created_by")?,
            "updated_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?,
            "updated_by": r.try_get::<Option<String>, _>("updated_by")?,
        }))
    })
    .collect::<Result<Vec<_>, _>>()
    .map_err(Into::into)
}

/// One stop's geofence in read shape, or `None` if it has none.
pub async fn geofence_json(
    conn: &mut PgConnection,
    g: &str,
    stop_id: &str,
) -> EditorResult<Option<Value>> {
    Ok(geofences_json(conn, g, Some(stop_id)).await?.pop())
}

/// `GET /feeds/{g}/geofences`.
pub async fn list_geofences(conn: &mut PgConnection, g: &str) -> EditorResult<Value> {
    Ok(json!({"items": geofences_json(conn, g, None).await?, "next_cursor": null}))
}

/// The `after` of the create, earlier in the set, that makes stop `key`'s
/// geofence.
pub(super) async fn geofence_created_in_set(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    key: &str,
) -> EditorResult<Option<Value>> {
    let row = sqlx::query(
        "SELECT after::text AS after FROM gtfs_change \
         WHERE change_set_id = $1 AND entity = 'geofence' AND op = 'create' AND entity_key = $2 \
         ORDER BY position LIMIT 1",
    )
    .bind(change_set_id)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|r| json_col(&r, "after")).transpose()?)
}

// ---------------------------------------------------------------- shape checks

fn invalid(key: &str, message: impl Into<String>) -> Finding {
    Finding::error("invalid_payload", key, message)
}

fn object<'a>(after: &'a Value, what: &str) -> Result<&'a Map<String, Value>, Finding> {
    after
        .as_object()
        .ok_or_else(|| invalid(what, format!("{what}: `after` must be an object")))
}

fn only(m: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), Finding> {
    match m.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(invalid(
            k,
            format!(
                "{what}: field {k:?} cannot be set (allowed: {})",
                allowed.join(", ")
            ),
        )),
        None => Ok(()),
    }
}

fn radius_m(m: &Map<String, Value>, required: bool, what: &str) -> Result<(), Finding> {
    match m.get("radius_m") {
        None if !required => Ok(()),
        Some(v) if v.as_i64().is_some_and(|n| n > 0) => Ok(()),
        _ => Err(invalid(
            "radius_m",
            format!("{what}: radius_m must be a positive whole number of metres"),
        )),
    }
}

fn lat_lon_pair(la: f64, lo: f64, what: &str) -> Result<(), Finding> {
    if !valid_lat_lon(la, lo) {
        return Err(invalid(
            "lat/lon",
            format!("{what}: position {la}, {lo} is out of range"),
        ));
    }
    Ok(())
}

fn lat_lon(m: &Map<String, Value>, required: bool, what: &str) -> Result<(), Finding> {
    let lat = m.get("lat");
    let lon = m.get("lon");
    if !required && lat.is_none() && lon.is_none() {
        return Ok(());
    }
    if lat.is_none() != lon.is_none() {
        return Err(invalid(
            "lat/lon",
            format!("{what}: lat and lon are given together"),
        ));
    }
    for (k, v) in [("lat", lat), ("lon", lon)] {
        if !matches!(v, Some(Value::Number(_))) {
            return Err(invalid(k, format!("{what}: {k} must be a number")));
        }
    }
    let la = lat.and_then(Value::as_f64).unwrap_or(0.0);
    let lo = lon.and_then(Value::as_f64).unwrap_or(0.0);
    lat_lon_pair(la, lo, what)
}

/// `shape`, defaulting to `"circle"` when absent - the plain-radius case is
/// the one that needs no choice made.
fn shape_of<'a>(m: &'a Map<String, Value>, what: &str) -> Result<&'a str, Finding> {
    match m.get("shape") {
        None => Ok("circle"),
        Some(Value::String(s)) if s == "circle" || s == "polygon" => Ok(s.as_str()),
        _ => Err(invalid(
            "shape",
            format!("{what}: shape must be \"circle\" or \"polygon\""),
        )),
    }
}

/// `polygon`: `[{lat, lon}, ...]`, at least 3 points, each in range.
fn polygon(m: &Map<String, Value>, required: bool, what: &str) -> Result<(), Finding> {
    let pts = match m.get("polygon") {
        None if !required => return Ok(()),
        None => return Err(invalid("polygon", format!("{what}: polygon is required"))),
        Some(Value::Array(pts)) => pts,
        _ => {
            return Err(invalid(
                "polygon",
                format!("{what}: polygon must be an array of points"),
            ))
        }
    };
    if pts.len() < 3 {
        return Err(invalid(
            "polygon",
            format!("{what}: polygon needs at least 3 points"),
        ));
    }
    for (i, p) in pts.iter().enumerate() {
        let o = p
            .as_object()
            .ok_or_else(|| invalid("polygon", format!("{what}: polygon[{i}] must be an object")))?;
        only(o, &["lat", "lon"], what)?;
        lat_lon(o, true, what)?;
    }
    Ok(())
}

/// `geofence/create` `{stop_id, lat, lon, shape?, radius_m, polygon}` -
/// `shape` defaults to `"circle"`, which then needs `radius_m`; `"polygon"`
/// needs `polygon` instead, not both. `geofence/update` (lat and lon given
/// together, any subset of `{lat, lon, shape, radius_m, polygon}`) and
/// `geofence/delete` (`null`).
pub fn check_geofence_payload(op: &str, key: &str, after: &Value) -> Result<(), Finding> {
    let what = format!("geofence/{op}");
    let what = what.as_str();
    if op == "delete" {
        return if after.is_null() {
            Ok(())
        } else {
            Err(invalid("", format!("{what}: `after` must be null")))
        };
    }
    let m = object(after, what)?;
    if op == "create" {
        only(
            m,
            &["stop_id", "lat", "lon", "shape", "radius_m", "polygon"],
            what,
        )?;
        let id = m
            .get("stop_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if id != key {
            return Err(invalid(
                id,
                format!("{what}: entity_key must equal stop_id"),
            ));
        }
        lat_lon(m, true, what)?;
        match shape_of(m, what)? {
            "polygon" => {
                if m.contains_key("radius_m") {
                    return Err(invalid(
                        "radius_m",
                        format!("{what}: radius_m cannot be set for a polygon"),
                    ));
                }
                polygon(m, true, what)?;
            }
            _ => {
                if m.contains_key("polygon") {
                    return Err(invalid(
                        "polygon",
                        format!("{what}: polygon cannot be set for a circle"),
                    ));
                }
                radius_m(m, true, what)?;
            }
        }
    } else {
        only(m, &["lat", "lon", "shape", "radius_m", "polygon"], what)?;
        if m.is_empty() {
            return Err(invalid("", format!("{what}: nothing to change")));
        }
        lat_lon(m, false, what)?;
        match m.get("shape") {
            // No shape switch: whichever of radius_m/polygon this update
            // touches must match the row's existing shape - checked against
            // the live row in `geofence_update`, which alone knows it.
            None => {
                if m.contains_key("radius_m") && m.contains_key("polygon") {
                    return Err(invalid(
                        "shape",
                        format!("{what}: cannot set both radius_m and polygon"),
                    ));
                }
                radius_m(m, false, what)?;
                polygon(m, false, what)?;
            }
            Some(Value::String(s)) if s == "circle" => {
                if m.contains_key("polygon") {
                    return Err(invalid(
                        "polygon",
                        format!("{what}: polygon cannot be set for a circle"),
                    ));
                }
                radius_m(m, true, what)?;
            }
            Some(Value::String(s)) if s == "polygon" => {
                if m.contains_key("radius_m") {
                    return Err(invalid(
                        "radius_m",
                        format!("{what}: radius_m cannot be set for a polygon"),
                    ));
                }
                polygon(m, true, what)?;
            }
            _ => {
                return Err(invalid(
                    "shape",
                    format!("{what}: shape must be \"circle\" or \"polygon\""),
                ))
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- apply

fn polygon_arrays(m: &Map<String, Value>) -> (Vec<f64>, Vec<f64>) {
    let pts = m["polygon"].as_array().expect("payload checked");
    (
        pts.iter().map(|p| p["lat"].as_f64().unwrap()).collect(),
        pts.iter().map(|p| p["lon"].as_f64().unwrap()).collect(),
    )
}

pub(super) async fn geofence_create(
    conn: &mut PgConnection,
    g: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let m = after.as_object().expect("payload checked");
    let id = m["stop_id"].as_str().expect("payload checked").trim();
    let deleted: bool =
        sqlx::query("SELECT deleted FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = $2")
            .bind(g)
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| fail("stop_not_found", format!("no stop {id}")))?
            .try_get("deleted")?;
    if deleted {
        return Err(fail("stop_deleted", format!("stop {id} is deleted")));
    }
    let lat = m["lat"].as_f64().expect("payload checked");
    let lon = m["lon"].as_f64().expect("payload checked");
    let shape = m.get("shape").and_then(Value::as_str).unwrap_or("circle");
    let (radius, poly_lat, poly_lon): (Option<i32>, Option<Vec<f64>>, Option<Vec<f64>>) =
        if shape == "polygon" {
            let (la, lo) = polygon_arrays(m);
            (None, Some(la), Some(lo))
        } else {
            (
                Some(m["radius_m"].as_i64().expect("payload checked") as i32),
                None,
                None,
            )
        };
    let created = sqlx::query(
        "INSERT INTO gtfs_geofence (gtfs_id, stop_id, lat, lon, shape, radius_m, polygon_lat, polygon_lon, created_by, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9) ON CONFLICT DO NOTHING RETURNING stop_id",
    )
    .bind(g)
    .bind(id)
    .bind(lat)
    .bind(lon)
    .bind(shape)
    .bind(radius)
    .bind(poly_lat)
    .bind(poly_lon)
    .bind(actor)
    .fetch_optional(&mut *conn)
    .await?;
    if created.is_none() {
        return Err(fail(
            "geofence_exists",
            format!("stop {id} already has a geofence"),
        ));
    }
    Ok(vec![])
}

pub(super) async fn geofence_update(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let m = after.as_object().expect("payload checked");
    let live = sqlx::query(
        "SELECT lat, lon, shape FROM gtfs_geofence WHERE gtfs_id = $1 AND stop_id = $2 FOR UPDATE",
    )
    .bind(g)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| fail("geofence_not_found", format!("stop {id} has no geofence")))?;
    let live_shape: String = live.try_get("shape")?;
    let new_shape = m
        .get("shape")
        .and_then(Value::as_str)
        .unwrap_or(live_shape.as_str());
    // The payload's own internal consistency (new_shape vs. the fields it
    // carries) is already checked in `check_geofence_payload`; what only the
    // live row can tell is whether an *unswitched* field matches it, and
    // whether a switch supplies what the new shape needs.
    if m.contains_key("radius_m") && new_shape != "circle" {
        return Err(fail(
            "invalid_payload",
            format!("stop {id}'s geofence is a polygon; radius_m does not apply"),
        ));
    }
    if m.contains_key("polygon") && new_shape != "polygon" {
        return Err(fail(
            "invalid_payload",
            format!("stop {id}'s geofence is a circle; polygon does not apply"),
        ));
    }
    if new_shape != live_shape {
        if new_shape == "circle" && !m.contains_key("radius_m") {
            return Err(fail(
                "invalid_payload",
                format!("switching stop {id}'s geofence to a circle needs radius_m"),
            ));
        }
        if new_shape == "polygon" && !m.contains_key("polygon") {
            return Err(fail(
                "invalid_payload",
                format!("switching stop {id}'s geofence to a polygon needs polygon"),
            ));
        }
    }
    let moved = m.contains_key("lat");
    let lat = m
        .get("lat")
        .and_then(Value::as_f64)
        .unwrap_or(live.try_get("lat")?);
    let lon = m
        .get("lon")
        .and_then(Value::as_f64)
        .unwrap_or(live.try_get("lon")?);
    let radius_given = m.contains_key("radius_m");
    let radius = m.get("radius_m").and_then(Value::as_i64).map(|v| v as i32);
    let polygon_given = m.contains_key("polygon");
    let (poly_lat, poly_lon): (Option<Vec<f64>>, Option<Vec<f64>>) = if polygon_given {
        let (la, lo) = polygon_arrays(m);
        (Some(la), Some(lo))
    } else {
        (None, None)
    };
    // `shape` decides what survives: a field that does not belong to the
    // target shape clears to NULL even when this change did not touch it, so
    // switching circle -> polygon (or back) cannot leave the other shape's
    // stale data sitting in the row.
    sqlx::query(
        "UPDATE gtfs_geofence SET \
            lat = CASE WHEN $3 THEN $4 ELSE lat END, \
            lon = CASE WHEN $3 THEN $5 ELSE lon END, \
            shape = $6, \
            radius_m = CASE WHEN $6 <> 'circle' THEN NULL \
                            WHEN $7 THEN $8 ELSE radius_m END, \
            polygon_lat = CASE WHEN $6 <> 'polygon' THEN NULL \
                                WHEN $9 THEN $10 ELSE polygon_lat END, \
            polygon_lon = CASE WHEN $6 <> 'polygon' THEN NULL \
                                WHEN $9 THEN $11 ELSE polygon_lon END, \
            updated_by = $12 \
         WHERE gtfs_id = $1 AND stop_id = $2",
    )
    .bind(g)
    .bind(id)
    .bind(moved)
    .bind(lat)
    .bind(lon)
    .bind(new_shape)
    .bind(radius_given)
    .bind(radius)
    .bind(polygon_given)
    .bind(poly_lat)
    .bind(poly_lon)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(vec![])
}

pub(super) async fn geofence_delete(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let found =
        sqlx::query("SELECT 1 FROM gtfs_geofence WHERE gtfs_id = $1 AND stop_id = $2 FOR UPDATE")
            .bind(g)
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    if found.is_none() {
        return Err(fail(
            "geofence_not_found",
            format!("stop {id} has no geofence"),
        ));
    }
    sqlx::query("DELETE FROM gtfs_geofence WHERE gtfs_id = $1 AND stop_id = $2")
        .bind(g)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(vec![])
}
