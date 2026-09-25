//! Cleanup context (docs/gtfs-editor.md section 9): what a person cleaning up a
//! stop or a route wants beside it - how far its routes go out of their way to
//! call there, the coordinate reviews it has had, the other stops of the same
//! name nearby, what the audit log says happened to it, and the open drafts
//! that already touch it. Reads only, viewer+.
//!
//! Every part is a key or index lookup: the stop's calls
//! ([`position_reviews::calls`], the detour code coordinate reviews use), its
//! reviews (`gtfs_position_review_stop_idx`), a lat/lon box for the same-named
//! stops (`gtfs_stop_latlon_idx`), the audit rows that name it
//! (`0011_context_indexes.sql`), and the changes of the feed's open drafts.

use super::error::{EditorError, EditorResult};
use super::position_reviews::{self, detour_m, median_detour};
use super::service;
use super::validation::{haversine_m, UNSERVED_TYPES};
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// Same-named stops are looked for this far from the stop.
pub const SAME_NAME_METRES: f64 = 5000.0;
/// A name this similar (pg_trgm) counts as the same name.
pub const SAME_NAME_SIMILARITY: f64 = 0.6;
pub const SAME_NAME_MAX: usize = 20;
/// A call further out of its route's way than this is one of a route's worst.
pub const WORST_DETOUR_METRES: f64 = 300.0;
pub const WORST_DETOURS_MAX: usize = 5;
const AUDIT_MAX: i64 = 10;
/// The statuses a stop's reviews are counted in; a superseded review was never
/// looked at and is left out.
const REVIEW_STATUSES: [&str; 4] = ["pending", "approved", "committed", "confirmed"];

fn metres(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn audit_json(r: &PgRow) -> Result<Value, sqlx::Error> {
    let detail: Option<String> = r.try_get("detail")?;
    Ok(json!({
        "audit_id": r.try_get::<i64, _>("audit_id")?,
        "at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("at")?,
        "actor_email": r.try_get::<Option<String>, _>("actor_email")?,
        "action": r.try_get::<String, _>("action")?,
        "change_set_id": r.try_get::<Option<Uuid>, _>("change_set_id")?,
        "detail": detail.and_then(|d| serde_json::from_str::<Value>(&d).ok()).unwrap_or(Value::Null),
    }))
}

fn open_draft_json(r: &PgRow) -> Result<Value, sqlx::Error> {
    Ok(json!({
        "change_set_id": r.try_get::<Uuid, _>("change_set_id")?,
        "title": r.try_get::<String, _>("title")?,
        "status": r.try_get::<String, _>("status")?,
        "change_id": r.try_get::<i64, _>("change_id")?,
        "entity": r.try_get::<String, _>("entity")?,
        "op": r.try_get::<String, _>("op")?,
    }))
}

/// The latest audit rows about a stop: every row naming it in `detail.stop_id`
/// (the coordinate review actions), the changes drafted on it, the merges it
/// was either side of, and the commits of the change sets that changed it.
async fn stop_audit(conn: &mut PgConnection, g: &str, stop_id: &str) -> EditorResult<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT a.audit_id, a.at, a.actor_email, a.action, a.change_set_id, a.detail::text AS detail \
         FROM gtfs_audit_log a WHERE a.audit_id IN ( \
             (SELECT audit_id FROM gtfs_audit_log WHERE gtfs_id = $1 AND detail ? 'stop_id' \
                 AND detail->>'stop_id' = $2 ORDER BY audit_id DESC LIMIT $3) \
             UNION \
             (SELECT audit_id FROM gtfs_audit_log WHERE gtfs_id = $1 AND detail ? 'entity_key' \
                 AND detail->>'entity_key' = $2 AND detail->>'entity' IN ('stop', 'station') \
               ORDER BY audit_id DESC LIMIT $3) \
             UNION \
             (SELECT audit_id FROM gtfs_audit_log WHERE gtfs_id = $1 AND action = 'stop_merged' \
                 AND detail->>'from' = $2 ORDER BY audit_id DESC LIMIT $3) \
             UNION \
             (SELECT audit_id FROM gtfs_audit_log WHERE gtfs_id = $1 AND action = 'stop_merged' \
                 AND detail->>'into' = $2 ORDER BY audit_id DESC LIMIT $3) \
             UNION \
             (SELECT x.audit_id FROM gtfs_change c \
              JOIN gtfs_audit_log x ON x.change_set_id = c.change_set_id \
                 AND x.action = 'change_set_committed' AND x.gtfs_id = $1 \
              WHERE c.entity IN ('stop', 'station') AND c.entity_key = $2 \
              ORDER BY x.audit_id DESC LIMIT $3)) \
         ORDER BY a.audit_id DESC LIMIT $3",
    )
    .bind(g)
    .bind(stop_id)
    .bind(AUDIT_MAX)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.iter().map(audit_json).collect::<Result<_, _>>()?)
}

/// The latest audit rows about a route: the changes drafted on it or its stop
/// list, the commits of the change sets that changed either, and the
/// coordinate review splits that took it off a stop.
async fn route_audit(conn: &mut PgConnection, g: &str, route_id: &str) -> EditorResult<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT a.audit_id, a.at, a.actor_email, a.action, a.change_set_id, a.detail::text AS detail \
         FROM gtfs_audit_log a WHERE a.audit_id IN ( \
             (SELECT audit_id FROM gtfs_audit_log WHERE gtfs_id = $1 AND detail ? 'entity_key' \
                 AND detail->>'entity_key' = $2 AND detail->>'entity' IN ('route', 'route_stops') \
               ORDER BY audit_id DESC LIMIT $3) \
             UNION \
             (SELECT x.audit_id FROM gtfs_change c \
              JOIN gtfs_audit_log x ON x.change_set_id = c.change_set_id AND x.gtfs_id = $1 \
                 AND (x.action = 'change_set_committed' \
                      OR (x.action = 'position_review_split' AND x.detail->'route_ids' ? $2)) \
              WHERE c.entity IN ('route', 'route_stops') AND c.entity_key = $2 \
              ORDER BY x.audit_id DESC LIMIT $3)) \
         ORDER BY a.audit_id DESC LIMIT $3",
    )
    .bind(g)
    .bind(route_id)
    .bind(AUDIT_MAX)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.iter().map(audit_json).collect::<Result<_, _>>()?)
}

/// `GET /feeds/{g}/stops/{stop_id}/context`.
pub async fn stop(conn: &mut PgConnection, g: &str, stop_id: &str) -> EditorResult<Value> {
    let row = service::stop_row(conn, g, stop_id)
        .await?
        .ok_or_else(|| EditorError::not_found("stop_not_found", format!("no stop {stop_id}")))?;
    let (lat, lon) = (
        row["lat"].as_f64().unwrap_or(0.0),
        row["lon"].as_f64().unwrap_or(0.0),
    );
    let name = row["name"].as_str().unwrap_or("").to_string();

    // the same detour coordinate reviews judge a position by
    let calls = position_reviews::calls(conn, g, stop_id).await?;
    let measured = calls
        .iter()
        .filter(|c| c.detour_at(stop_id, (lat, lon)).is_some())
        .count();
    let detour = median_detour(&calls, None, stop_id, (lat, lon));

    let mut reviews = json!({"pending": 0, "approved": 0, "committed": 0, "confirmed": 0});
    let mut items = Vec::new();
    for r in sqlx::query(
        "SELECT review_id, status, reason FROM gtfs_position_review \
         WHERE gtfs_id = $1 AND stop_id = $2 AND status = ANY($3) ORDER BY review_id DESC",
    )
    .bind(g)
    .bind(stop_id)
    .bind(&REVIEW_STATUSES[..])
    .fetch_all(&mut *conn)
    .await?
    {
        let status: String = r.try_get("status")?;
        reviews[status.as_str()] = json!(reviews[status.as_str()].as_i64().unwrap_or(0) + 1);
        items.push(json!({
            "review_id": r.try_get::<i64, _>("review_id")?,
            "status": status,
            "reason": r.try_get::<String, _>("reason")?,
        }));
    }
    reviews["items"] = json!(items);

    // a box a little over the radius each way, then the true distance
    let dlat = SAME_NAME_METRES / 111_000.0;
    let dlon = dlat / lat.to_radians().cos().abs().max(0.01);
    let mut same_name: Vec<(f64, Value)> = sqlx::query(
        "SELECT s.stop_id, s.name, s.lat, s.lon, s.parent_station, s.platform_code, s.description, \
                similarity(s.name, $3)::float8 AS similarity, \
                (SELECT count(DISTINCT rs.route_id) FROM gtfs_route_stop rs \
                  WHERE rs.gtfs_id = s.gtfs_id AND rs.stop_id = s.stop_id) AS route_count \
         FROM gtfs_stop s \
         WHERE s.gtfs_id = $1 AND NOT s.deleted AND s.location_type = 0 AND s.stop_id <> $2 \
           AND s.lat BETWEEN $4 - $6 AND $4 + $6 AND s.lon BETWEEN $5 - $7 AND $5 + $7 \
           AND (similarity(s.name, $3) >= $8 \
                OR regexp_replace(lower(s.name), '[^[:alnum:]]+', '', 'g') \
                 = regexp_replace(lower($3), '[^[:alnum:]]+', '', 'g'))",
    )
    .bind(g)
    .bind(stop_id)
    .bind(&name)
    .bind(lat)
    .bind(lon)
    .bind(dlat * 1.05)
    .bind(dlon * 1.05)
    .bind(SAME_NAME_SIMILARITY as f32)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(f64, Value), sqlx::Error> {
        let (slat, slon): (f64, f64) = (r.try_get("lat")?, r.try_get("lon")?);
        let d = haversine_m(lat, lon, slat, slon);
        Ok((
            d,
            json!({
                "stop_id": r.try_get::<String, _>("stop_id")?,
                "name": r.try_get::<String, _>("name")?,
                "lat": slat,
                "lon": slon,
                "distance_m": metres(d),
                "route_count": r.try_get::<i64, _>("route_count")?,
                "parent_station": r.try_get::<Option<String>, _>("parent_station")?,
                "platform_code": r.try_get::<Option<String>, _>("platform_code")?,
                "description": r.try_get::<Option<String>, _>("description")?,
                "similarity": (r.try_get::<f64, _>("similarity")? * 100.0).round() / 100.0,
            }),
        ))
    })
    .collect::<Result<_, _>>()?;
    same_name.retain(|(d, _)| *d <= SAME_NAME_METRES);
    same_name.sort_by(|a, b| {
        a.0.total_cmp(&b.0)
            .then_with(|| a.1["stop_id"].as_str().cmp(&b.1["stop_id"].as_str()))
    });
    same_name.truncate(SAME_NAME_MAX);

    // changes of the feed's open drafts on this stop: keyed on it (a stop or
    // station change), merging another stop into it, or taking it into a station
    let open_drafts = sqlx::query(
        "SELECT cs.change_set_id, cs.title, cs.status, c.change_id, c.entity, c.op \
         FROM gtfs_change_set cs \
         JOIN gtfs_change c ON c.change_set_id = cs.change_set_id \
         WHERE cs.gtfs_id = $1 AND cs.status IN ('draft', 'submitted', 'approved') \
           AND c.entity IN ('stop', 'station') \
           AND (c.entity_key = $2 \
                OR (c.op = 'merge' AND btrim(c.after->>'into_stop_id') = $2) \
                OR (c.entity = 'station' AND c.op IN ('create', 'update') \
                    AND (c.after->'member_stop_ids' ? $2 \
                         OR c.after->'members' @> jsonb_build_array(jsonb_build_object('stop_id', $2::text))))) \
         ORDER BY cs.updated_at DESC, cs.change_set_id, c.position",
    )
    .bind(g)
    .bind(stop_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(open_draft_json)
    .collect::<Result<Vec<_>, _>>()?;

    Ok(json!({
        "stop_id": stop_id,
        "detour_m": detour.map(metres),
        "routes_measured": measured,
        "position_reviews": reviews,
        "same_name": same_name.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "audit": stop_audit(conn, g, stop_id).await?,
        "open_drafts": open_drafts,
    }))
}

/// `GET /feeds/{g}/routes/{route_id}/context`.
pub async fn route(conn: &mut PgConnection, g: &str, route_id: &str) -> EditorResult<Value> {
    service::route_row(conn, g, route_id)
        .await?
        .ok_or_else(|| EditorError::not_found("route_not_found", format!("no route {route_id}")))?;

    let stops_with_reviews = sqlx::query(
        "SELECT rs.stop_id, rs.sequence, r.review_id, r.status \
         FROM gtfs_route_stop rs \
         JOIN gtfs_position_review r ON r.gtfs_id = rs.gtfs_id AND r.stop_id = rs.stop_id \
         WHERE rs.gtfs_id = $1 AND rs.route_id = $2 AND rs.pattern_key = 1 AND r.status = ANY($3) \
         ORDER BY rs.sequence, r.review_id",
    )
    .bind(g)
    .bind(route_id)
    .bind(&REVIEW_STATUSES[..])
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        Ok(json!({
            "stop_id": r.try_get::<String, _>("stop_id")?,
            "sequence": r.try_get::<i32, _>("sequence")?,
            "review_id": r.try_get::<i64, _>("review_id")?,
            "status": r.try_get::<String, _>("status")?,
        }))
    })
    .collect::<Result<Vec<_>, _>>()?;

    // each served call's own detour, between the served stops either side
    let served: Vec<(i32, String, String, f64, f64)> = sqlx::query(
        "SELECT rs.sequence, rs.stop_id, s.name, s.lat, s.lon FROM gtfs_route_stop rs \
         JOIN gtfs_stop s ON s.gtfs_id = rs.gtfs_id AND s.stop_id = rs.stop_id \
         WHERE rs.gtfs_id = $1 AND rs.route_id = $2 AND rs.pattern_key = 1 AND rs.stop_type <> ALL($3) \
         ORDER BY rs.sequence",
    )
    .bind(g)
    .bind(route_id)
    .bind(&UNSERVED_TYPES[..])
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(
        |r| -> Result<(i32, String, String, f64, f64), sqlx::Error> {
            Ok((
                r.try_get("sequence")?,
                r.try_get("stop_id")?,
                r.try_get("name")?,
                r.try_get("lat")?,
                r.try_get("lon")?,
            ))
        },
    )
    .collect::<Result<_, _>>()?;
    let mut worst: Vec<(f64, Value)> = served
        .windows(3)
        .filter_map(|w| {
            let (prev, (sequence, stop_id, name, lat, lon), next) = (&w[0], &w[1], &w[2]);
            // a neighbour that is the same stop is no way round
            let place = |n: &(i32, String, String, f64, f64)| {
                if n.1 == *stop_id {
                    (*lat, *lon)
                } else {
                    (n.3, n.4)
                }
            };
            let d = detour_m(place(prev), (*lat, *lon), place(next));
            (d > WORST_DETOUR_METRES).then(|| {
                (
                    d,
                    json!({"stop_id": stop_id, "name": name, "sequence": sequence, "detour_m": metres(d)}),
                )
            })
        })
        .collect();
    worst.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1["sequence"].as_i64().cmp(&b.1["sequence"].as_i64()))
    });
    worst.truncate(WORST_DETOURS_MAX);

    let open_drafts = sqlx::query(
        "SELECT cs.change_set_id, cs.title, cs.status, c.change_id, c.entity, c.op \
         FROM gtfs_change_set cs \
         JOIN gtfs_change c ON c.change_set_id = cs.change_set_id \
         WHERE cs.gtfs_id = $1 AND cs.status IN ('draft', 'submitted', 'approved') \
           AND c.entity IN ('route', 'route_stops') AND c.entity_key = $2 \
         ORDER BY cs.updated_at DESC, cs.change_set_id, c.position",
    )
    .bind(g)
    .bind(route_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(open_draft_json)
    .collect::<Result<Vec<_>, _>>()?;

    Ok(json!({
        "route_id": route_id,
        "stops_with_reviews": stops_with_reviews,
        "worst_detours": worst.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "audit": route_audit(conn, g, route_id).await?,
        "open_drafts": open_drafts,
    }))
}
