//! Stages: a route as an ordered list of fare stages, each an ordered list of
//! stops, stored once and shared by every route that runs through it (see
//! docs/gtfs-editor.md section 18 and db/gtfs_editor/0022_stages.sql).
//!
//! `gtfs_route_stop` stays the flattened stop list every reader uses. A route
//! that has `gtfs_route_stage` rows is *built from stages*: its
//! `gtfs_route_stop` rows are rewritten from them, in the same transaction, by
//! every change to its stages - [`route_stages_replace`] for its own list, and
//! [`stage_update`] for every route using a stage it changes. Such a route's
//! stop list is never edited directly (`route_stops/replace` is refused). A
//! route without stages keeps being edited as before until it is given some.

use super::auth::Ctx;
use super::error::{EditorError, EditorResult};
use super::feed_lock::{lock_feed, retry_transient};
use super::service::{
    self as svc, blocks_apply, check_stops_usable, fail, json_col, like_pattern, load_route_rows,
    write_route_rows, ApplyError, Page,
};
use super::validation::{
    check_route_rows, check_stage_rows, flatten_stages, grade_against_live, mint_stage_id as mint,
    stage_numbers, stored_stage_rows, Finding, Level, RouteRow, StageLink, StageRow,
    UNSERVED_TYPES,
};
use super::EditorState;
use serde_json::{json, Value};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// How many route names a finding lists before it says "and N more".
const NAMED_ROUTES: usize = 20;

// ---------------------------------------------------------------- live rows

/// A stage as stored.
#[derive(Debug, Clone)]
pub struct LiveStage {
    pub name: String,
    /// Which way along its corridor it runs: "up", "down", or none.
    pub direction: Option<String>,
    pub description: Option<String>,
    pub deleted: bool,
    pub row_version: i32,
    pub rows: Vec<StageRow>,
}

/// Whether a route is built from stages.
pub async fn has_stages(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM gtfs_route_stage WHERE gtfs_id = $1 AND route_id = $2) AS has",
    )
    .bind(g)
    .bind(route_id)
    .fetch_one(&mut *conn)
    .await?
    .try_get("has")
}

const STAGE_ROW_COLS: &str =
    "stop_id, stop_type, marker_id, marker_name, marker_lat, marker_lon, stop_name_override";

fn stage_row_from(r: &PgRow) -> Result<StageRow, sqlx::Error> {
    Ok(StageRow {
        stop_id: r.try_get("stop_id")?,
        stop_type: r.try_get("stop_type")?,
        marker_id: r.try_get("marker_id")?,
        marker_name: r.try_get("marker_name")?,
        marker_lat: r.try_get("marker_lat")?,
        marker_lon: r.try_get("marker_lon")?,
        stop_name_override: r.try_get("stop_name_override")?,
    })
}

/// A stage and its rows; `lock` holds the stage row until the transaction ends.
pub async fn load_stage(
    conn: &mut PgConnection,
    g: &str,
    stage_id: &str,
    lock: bool,
) -> Result<Option<LiveStage>, sqlx::Error> {
    let Some(row) = sqlx::query(&format!(
        "SELECT name, direction, description, deleted, row_version FROM gtfs_stage \
         WHERE gtfs_id = $1 AND stage_id = $2{}",
        if lock { " FOR UPDATE" } else { "" }
    ))
    .bind(g)
    .bind(stage_id)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let rows = sqlx::query(&format!(
        "SELECT {STAGE_ROW_COLS} FROM gtfs_stage_stop WHERE gtfs_id = $1 AND stage_id = $2 ORDER BY position"
    ))
    .bind(g)
    .bind(stage_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(stage_row_from)
    .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(LiveStage {
        name: row.try_get("name")?,
        description: row.try_get("description")?,
        deleted: row.try_get("deleted")?,
        direction: row.try_get("direction")?,
        row_version: row.try_get("row_version")?,
        rows,
    }))
}

/// Which list the route is wearing: `None` is its normal one (section 19).
pub async fn active_variant(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT active_variant_id FROM gtfs_route WHERE gtfs_id = $1 AND route_id = $2",
    )
    .bind(g)
    .bind(route_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.and_then(|r| {
        r.try_get::<Option<String>, _>("active_variant_id")
            .ok()
            .flatten()
    }))
}

/// A route's stages in order, as `route_stages/replace` sends them. `variant` is
/// the list to read: `None` is the route's normal one.
pub async fn route_links(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    variant: Option<&str>,
) -> Result<Vec<StageLink>, sqlx::Error> {
    sqlx::query(
        "SELECT stage_id, stage_no FROM gtfs_route_stage \
         WHERE gtfs_id = $1 AND route_id = $2 \
           AND variant_id IS NOT DISTINCT FROM $3 ORDER BY position",
    )
    .bind(g)
    .bind(route_id)
    .bind(variant)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| {
        Ok(StageLink {
            stage_id: r.try_get("stage_id")?,
            stage_no: Some(r.try_get("stage_no")?),
        })
    })
    .collect()
}

/// Fingerprint of a route's stage list; a `route_stages` replace carries the one
/// it was based on. A route without stages hashes like an empty stop list.
pub fn links_hash(links: &[StageLink]) -> String {
    let numbered: Vec<StageLink> = links
        .iter()
        .zip(stage_numbers(links))
        .map(|(l, n)| StageLink {
            stage_id: l.stage_id.clone(),
            stage_no: Some(n),
        })
        .collect();
    super::crypto::sha256_hex(
        serde_json::to_string(&numbered)
            .expect("stage links serialize")
            .as_bytes(),
    )
}

pub async fn live_links_hash(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    variant: Option<&str>,
) -> Result<String, sqlx::Error> {
    Ok(links_hash(&route_links(conn, g, route_id, variant).await?))
}

/// A route's rows as one of its stage lists makes them; `None` is the normal
/// list. What the route serves is the list `active_variant` names.
pub async fn flatten_route(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    variant: Option<&str>,
) -> Result<Vec<RouteRow>, sqlx::Error> {
    sqlx::query(
        "SELECT rs.stage_no, st.name AS stage_name, ss.stop_id, ss.stop_type, ss.marker_id, \
                ss.marker_name, ss.marker_lat, ss.marker_lon, ss.stop_name_override \
         FROM gtfs_route_stage rs \
         JOIN gtfs_stage st ON st.gtfs_id = rs.gtfs_id AND st.stage_id = rs.stage_id \
         JOIN gtfs_stage_stop ss ON ss.gtfs_id = rs.gtfs_id AND ss.stage_id = rs.stage_id \
         WHERE rs.gtfs_id = $1 AND rs.route_id = $2 \
           AND rs.variant_id IS NOT DISTINCT FROM $3 \
         ORDER BY rs.position, ss.position",
    )
    .bind(g)
    .bind(route_id)
    .bind(variant)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| {
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
            provider_id: None,
            // a stage's stops carry no per-row GTFS fields of their own
            ..Default::default()
        })
    })
    .collect()
}

/// Whether two stop lists are the same but for the provider id, which is the
/// route's and not its stages'.
pub fn same_rows(a: &[RouteRow], b: &[RouteRow]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            RouteRow {
                provider_id: None,
                ..x.clone()
            } == RouteRow {
                provider_id: None,
                ..y.clone()
            }
        })
}

/// The live routes that use a stage, by short name: `(route_id, short_name)`.
pub async fn routes_using(
    conn: &mut PgConnection,
    g: &str,
    stage_id: &str,
) -> Result<Vec<(String, Option<String>)>, sqlx::Error> {
    sqlx::query(
        "SELECT DISTINCT r.route_id, r.short_name FROM gtfs_route_stage rs \
         JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
         WHERE rs.gtfs_id = $1 AND rs.stage_id = $2 \
           AND rs.variant_id IS NOT DISTINCT FROM r.active_variant_id \
         ORDER BY r.short_name, r.route_id",
    )
    .bind(g)
    .bind(stage_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| Ok((r.try_get("route_id")?, r.try_get("short_name")?)))
    .collect()
}

/// "21G (2614), 23C (118) and 3 more"
fn route_names(routes: &[(String, Option<String>)]) -> String {
    let mut names: Vec<String> = routes
        .iter()
        .take(NAMED_ROUTES)
        .map(|(id, short)| match short {
            Some(s) if !s.is_empty() && s != id => format!("{s} ({id})"),
            _ => id.clone(),
        })
        .collect();
    if routes.len() > NAMED_ROUTES {
        names.push(format!("{} more", routes.len() - NAMED_ROUTES));
    }
    match names.len() {
        0 => String::new(),
        1 => names.remove(0),
        n => {
            let last = names.remove(n - 1);
            format!("{} and {last}", names.join(", "))
        }
    }
}

pub(super) async fn write_stage_rows(
    conn: &mut PgConnection,
    g: &str,
    stage_id: &str,
    rows: &[StageRow],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM gtfs_stage_stop WHERE gtfs_id = $1 AND stage_id = $2")
        .bind(g)
        .bind(stage_id)
        .execute(&mut *conn)
        .await?;
    let pos: Vec<i32> = (1..=rows.len() as i32).collect();
    let col = |f: fn(&StageRow) -> Option<String>| rows.iter().map(f).collect::<Vec<_>>();
    sqlx::query(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, position, stop_id, stop_type, marker_id, \
                                      marker_name, marker_lat, marker_lon, stop_name_override) \
         SELECT $1, $2, u.pos, u.stop, u.typ, u.mid, u.mname, u.mlat, u.mlon, u.over \
         FROM UNNEST($3::int4[], $4::text[], $5::text[], $6::text[], $7::text[], $8::float8[], \
                     $9::float8[], $10::text[]) AS u(pos, stop, typ, mid, mname, mlat, mlon, over)",
    )
    .bind(g)
    .bind(stage_id)
    .bind(&pos)
    .bind(col(|r| r.stop_id.clone()))
    .bind(rows.iter().map(|r| r.stop_type.clone()).collect::<Vec<_>>())
    .bind(col(|r| r.marker_id.clone()))
    .bind(col(|r| r.marker_name.clone()))
    .bind(rows.iter().map(|r| r.marker_lat).collect::<Vec<_>>())
    .bind(rows.iter().map(|r| r.marker_lon).collect::<Vec<_>>())
    .bind(col(|r| r.stop_name_override.clone()))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub(super) async fn write_links(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    variant: Option<&str>,
    links: &[(String, i32)],
    actor: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM gtfs_route_stage WHERE gtfs_id = $1 AND route_id = $2 \
           AND variant_id IS NOT DISTINCT FROM $3",
    )
    .bind(g)
    .bind(route_id)
    .bind(variant)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO gtfs_route_stage \
             (gtfs_id, route_id, variant_id, position, stage_id, stage_no, updated_by) \
         SELECT $1, $2, $7, u.pos, u.stage, u.no, $6 \
         FROM UNNEST($3::int4[], $4::text[], $5::int4[]) AS u(pos, stage, no)",
    )
    .bind(g)
    .bind(route_id)
    .bind((1..=links.len() as i32).collect::<Vec<_>>())
    .bind(links.iter().map(|l| l.0.clone()).collect::<Vec<_>>())
    .bind(links.iter().map(|l| l.1).collect::<Vec<_>>())
    .bind(actor)
    .bind(variant)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A stage id nobody uses: not a stage of the feed (deleted ones included), and
/// not one an open draft of the feed creates.
pub async fn mint_stage_id(conn: &mut PgConnection, g: &str) -> EditorResult<String> {
    for _ in 0..8 {
        let id = mint();
        let used: bool = sqlx::query(
            "SELECT EXISTS (SELECT 1 FROM gtfs_stage WHERE gtfs_id = $1 AND stage_id = $2) \
                 OR EXISTS (SELECT 1 FROM gtfs_change ch \
                            JOIN gtfs_change_set cs ON cs.change_set_id = ch.change_set_id \
                            WHERE cs.gtfs_id = $1 AND cs.status NOT IN ('committed', 'discarded') \
                              AND ch.entity = 'stage' AND ch.op = 'create' AND ch.entity_key = $2) AS used",
        )
        .bind(g)
        .bind(&id)
        .fetch_one(&mut *conn)
        .await?
        .try_get("used")?;
        if !used {
            return Ok(id);
        }
    }
    Err(EditorError::internal("could not mint an unused stage id"))
}

// ---------------------------------------------------------------- apply

/// A stage change's `description`: absent leaves it, null or blank clears it.
/// `after.direction`: "up", "down", or null to clear it; absent leaves it.
fn direction_in(after: &Value) -> Option<Option<String>> {
    after.get("direction").map(|d| {
        d.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
    })
}

fn description_in(after: &Value) -> Option<Option<String>> {
    after.get("description").map(|d| {
        d.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn rows_in(after: &Value) -> Result<Option<Vec<StageRow>>, ApplyError> {
    after
        .get("rows")
        .map(|r| {
            serde_json::from_value::<Vec<StageRow>>(r.clone())
                .map_err(|e| fail("invalid_payload", format!("rows are not valid: {e}")))
        })
        .transpose()
}

/// Everything wrong with a stage's rows that no route could make right.
async fn stage_checks(
    conn: &mut PgConnection,
    g: &str,
    rows: &[StageRow],
) -> Result<Vec<Finding>, sqlx::Error> {
    let mut findings = check_stage_rows(rows);
    let ids: Vec<String> = rows
        .iter()
        .filter(|r| !r.is_marker())
        .filter_map(|r| r.stop_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    findings.extend(check_stops_usable(conn, g, &ids).await?);
    Ok(findings)
}

fn has_error(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.level == Level::Error)
}

pub(super) async fn stage_create(
    conn: &mut PgConnection,
    g: &str,
    key: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let name = after["name"].as_str().unwrap_or("").trim().to_string();
    let rows = stored_stage_rows(key, &rows_in(after)?.unwrap_or_default());
    let findings = stage_checks(conn, g, &rows).await?;
    if has_error(&findings) {
        return Err(ApplyError::Findings(findings));
    }
    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM gtfs_stage WHERE gtfs_id = $1 AND stage_id = $2) AS e",
    )
    .bind(g)
    .bind(key)
    .fetch_one(&mut *conn)
    .await?
    .try_get("e")?;
    if exists {
        return Err(fail("stage_exists", format!("stage {key} already exists")));
    }
    sqlx::query(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, name, direction, description, provenance, updated_by) \
         VALUES ($1, $2, $3, $4, $5, '{\"source\": \"editor\"}'::jsonb, $6)",
    )
    .bind(g)
    .bind(key)
    .bind(&name)
    .bind(direction_in(after).flatten())
    .bind(description_in(after).flatten())
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    write_stage_rows(conn, g, key, &rows).await?;
    Ok(findings)
}

/// Load a stage an update or delete acts on, locked, or the finding why not.
async fn stage_to_change(
    conn: &mut PgConnection,
    g: &str,
    key: &str,
) -> Result<LiveStage, ApplyError> {
    let stage = load_stage(conn, g, key, true)
        .await?
        .ok_or_else(|| fail("stage_not_found", format!("no stage {key}")))?;
    if stage.deleted {
        return Err(fail("stage_deleted", format!("stage {key} is deleted")));
    }
    Ok(stage)
}

/// Change a stage, then rewrite the stop list of every route that uses it.
/// Each route's fare and order rules are checked on its new rows: a problem the
/// route already had is a warning, a new one an error that (like a
/// `route_stops` change's) keeps the draft from being submitted.
pub(super) async fn stage_update(
    conn: &mut PgConnection,
    g: &str,
    key: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let live = stage_to_change(conn, g, key).await?;
    let name = after["name"]
        .as_str()
        .map(|n| n.trim().to_string())
        .unwrap_or_else(|| live.name.clone());
    let description = description_in(after).unwrap_or_else(|| live.description.clone());
    let direction = direction_in(after).unwrap_or_else(|| live.direction.clone());
    let rows = match rows_in(after)? {
        Some(rows) => stored_stage_rows(key, &rows),
        None => live.rows.clone(),
    };
    let mut findings = stage_checks(conn, g, &rows).await?;
    if has_error(&findings) {
        return Err(ApplyError::Findings(findings));
    }

    // Every route using the stage must be what its stages say before the change,
    // or rewriting it from them would undo an edit made some other way.
    let routes = routes_using(conn, g, key).await?;
    let mut before: Vec<Vec<RouteRow>> = Vec::with_capacity(routes.len());
    let mut stale = Vec::new();
    for (route_id, short) in &routes {
        let live_rows = load_route_rows(conn, g, route_id).await?;
        let worn = active_variant(conn, g, route_id).await?;
        if !same_rows(
            &flatten_route(conn, g, route_id, worn.as_deref()).await?,
            &live_rows,
        ) {
            stale.push((route_id.clone(), short.clone()));
        }
        before.push(live_rows);
    }
    if !stale.is_empty() {
        return Err(ApplyError::Findings(vec![Finding::error(
            "route_out_of_sync",
            key,
            format!(
                "the stop list of {} was changed outside its stages, so changing stage {key} would undo that; set that route's stages again first (a route_stages change), in this draft or an earlier one",
                route_names(&stale)
            ),
        )]));
    }

    sqlx::query(
        "UPDATE gtfs_stage SET name = $3, direction = $4, description = $5, updated_by = $6 \
         WHERE gtfs_id = $1 AND stage_id = $2",
    )
    .bind(g)
    .bind(key)
    .bind(&name)
    .bind(&direction)
    .bind(&description)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    write_stage_rows(conn, g, key, &rows).await?;

    let mut route_findings = Vec::new();
    for ((route_id, short), live_rows) in routes.iter().zip(&before) {
        let worn = active_variant(conn, g, route_id).await?;
        let rows = flatten_route(conn, g, route_id, worn.as_deref()).await?;
        let label = route_names(&[(route_id.clone(), short.clone())]);
        for mut f in grade_against_live(check_route_rows(&rows), &check_route_rows(live_rows)) {
            f.message = format!("route {label}: {}", f.message);
            f.key = format!("{route_id}|{}", f.key);
            route_findings.push(f);
        }
        write_route_rows(conn, g, route_id, &rows, live_rows, actor).await?;
    }
    if blocks_apply(&route_findings) {
        return Err(ApplyError::Findings(route_findings));
    }
    findings.extend(route_findings);
    if !routes.is_empty() {
        findings.push(Finding::warning(
            "stage_changes_routes",
            key,
            format!(
                "changing stage {name} ({key}) changes the stop list of {}: {}",
                plural(routes.len(), "route"),
                route_names(&routes)
            ),
        ));
    }
    Ok(findings)
}

fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// Soft-delete a stage no live route uses.
pub(super) async fn stage_delete(
    conn: &mut PgConnection,
    g: &str,
    key: &str,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    stage_to_change(conn, g, key).await?;
    let routes = routes_using(conn, g, key).await?;
    if !routes.is_empty() {
        return Err(fail(
            "stage_in_use",
            format!(
                "stage {key} is used by {}: {}; take it off those routes first",
                plural(routes.len(), "route"),
                route_names(&routes)
            ),
        ));
    }
    sqlx::query(
        "UPDATE gtfs_stage SET deleted = true, updated_by = $3 WHERE gtfs_id = $1 AND stage_id = $2",
    )
    .bind(g)
    .bind(key)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(vec![])
}

/// Set a route's stages and rewrite its stop list from them. A route without
/// stages becomes one built from stages; its old rows are replaced.
pub(super) async fn route_stages_replace(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let links: Vec<StageLink> = serde_json::from_value(after["stages"].clone())
        .map_err(|e| fail("invalid_payload", format!("stages are not valid: {e}")))?;
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

    let mut findings = Vec::new();
    let mut stages: HashMap<String, LiveStage> = HashMap::new();
    for l in &links {
        let id = l.stage_id.trim();
        if stages.contains_key(id) {
            continue;
        }
        match load_stage(conn, g, id, false).await? {
            None => findings.push(Finding::error(
                "stage_not_found",
                id,
                format!("stage {id} does not exist"),
            )),
            Some(s) if s.deleted => findings.push(Finding::error(
                "stage_deleted",
                id,
                format!("stage {id} is deleted"),
            )),
            Some(s) => {
                stages.insert(id.to_string(), s);
            }
        }
    }
    if has_error(&findings) {
        return Err(ApplyError::Findings(findings));
    }
    let numbers = stage_numbers(&links);
    let rows = flatten_stages(links.iter().zip(&numbers).map(|(l, n)| {
        let s = &stages[l.stage_id.trim()];
        (*n, s.name.as_str(), s.rows.as_slice())
    }));

    // a stage may still hold a stop deleted since it was written
    let ids: Vec<String> = rows
        .iter()
        .filter(|r| !r.is_marker())
        .filter_map(|r| r.stop_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    findings.extend(check_stops_usable(conn, g, &ids).await?);
    let live = load_route_rows(conn, g, route_id).await?;
    let worn = active_variant(conn, g, route_id).await?;
    if has_stages(conn, g, route_id).await?
        && worn.is_none()
        && !same_rows(&flatten_route(conn, g, route_id, None).await?, &live)
    {
        findings.push(Finding::warning(
            "route_out_of_sync",
            route_id,
            format!(
                "route {route_id}'s stop list was changed outside its stages; this replaces it with the stages' stops"
            ),
        ));
    }
    findings.extend(grade_against_live(
        check_route_rows(&rows),
        &check_route_rows(&live),
    ));
    if blocks_apply(&findings) {
        return Err(ApplyError::Findings(findings));
    }
    let numbered: Vec<(String, i32)> = links
        .iter()
        .zip(numbers)
        .map(|(l, n)| (l.stage_id.trim().to_string(), n))
        .collect();
    write_links(conn, g, route_id, None, &numbered, actor).await?;
    // a diverted route keeps serving its diversion: the normal list it is not
    // wearing changes underneath, and takes effect when it goes back to normal
    if worn.is_none() {
        write_route_rows(conn, g, route_id, &rows, &live, actor).await?;
    }
    Ok(findings)
}

/// A stop merge's step 1 for stages: every stage row of the stop that goes away
/// moves to the kept stop, keeping the stage's own spelling exactly as the
/// route rows do, so a route built from stages still matches them. The stages
/// touched move to a new version: a draft based on the old one conflicts.
pub(super) async fn merge_stop_into(
    conn: &mut PgConnection,
    g: &str,
    from: &str,
    into: &str,
    keep_from_name: Option<&str>,
    actor: &str,
) -> Result<(), sqlx::Error> {
    let touched: Vec<String> = sqlx::query(
        "UPDATE gtfs_stage_stop SET stop_id = $3, \
            stop_name_override = CASE WHEN stop_name_override IS NOT NULL THEN stop_name_override \
                                      ELSE $4 END \
         WHERE gtfs_id = $1 AND stop_id = $2 RETURNING stage_id",
    )
    .bind(g)
    .bind(from)
    .bind(into)
    .bind(keep_from_name)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| r.try_get("stage_id"))
    .collect::<Result<HashSet<String>, _>>()?
    .into_iter()
    .collect();
    if !touched.is_empty() {
        sqlx::query(
            "UPDATE gtfs_stage SET updated_by = $3 WHERE gtfs_id = $1 AND stage_id = ANY($2)",
        )
        .bind(g)
        .bind(&touched)
        .bind(actor)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Live stages, not deleted, that call at a stop: `(stage_id, name)`.
pub async fn stages_with_stop(
    conn: &mut PgConnection,
    g: &str,
    stop_id: &str,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query(
        "SELECT DISTINCT s.stage_id, s.name FROM gtfs_stage_stop ss \
         JOIN gtfs_stage s ON s.gtfs_id = ss.gtfs_id AND s.stage_id = ss.stage_id AND NOT s.deleted \
         WHERE ss.gtfs_id = $1 AND ss.stop_id = $2 ORDER BY s.name, s.stage_id",
    )
    .bind(g)
    .bind(stop_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| Ok((r.try_get("stage_id")?, r.try_get("name")?)))
    .collect()
}

// ---------------------------------------------------------------- reads

/// The rows of many stages in read shape (names and positions), in one query.
async fn stages_read_rows(
    conn: &mut PgConnection,
    g: &str,
    stage_ids: &[String],
) -> Result<HashMap<String, Vec<Value>>, sqlx::Error> {
    let mut out: HashMap<String, Vec<Value>> = HashMap::new();
    for r in sqlx::query(
        "SELECT ss.stage_id, ss.position, ss.stop_id, coalesce(ss.stop_name_override, s.name) AS stop_name, \
                s.lat, s.lon, s.deleted AS stop_deleted, s.unserviceable, s.parent_station, \
                ss.stop_type, ss.marker_id, \
                ss.marker_name, ss.marker_lat, ss.marker_lon, ss.stop_name_override \
         FROM gtfs_stage_stop ss \
         LEFT JOIN gtfs_stop s ON s.gtfs_id = ss.gtfs_id AND s.stop_id = ss.stop_id \
         WHERE ss.gtfs_id = $1 AND ss.stage_id = ANY($2) ORDER BY ss.stage_id, ss.position",
    )
    .bind(g)
    .bind(stage_ids)
    .fetch_all(&mut *conn)
    .await?
    {
        out.entry(r.try_get("stage_id")?).or_default().push(json!({
            "position": r.try_get::<i32, _>("position")?,
            "stop_id": r.try_get::<Option<String>, _>("stop_id")?,
            "stop_name": r.try_get::<Option<String>, _>("stop_name")?,
            "lat": r.try_get::<Option<f64>, _>("lat")?,
            "lon": r.try_get::<Option<f64>, _>("lon")?,
            "stop_deleted": r.try_get::<Option<bool>, _>("stop_deleted")?,
            // out of use: the stage still has the stop, no bus calls there (section 21)
            "unserviceable": r.try_get::<Option<bool>, _>("unserviceable")?.unwrap_or(false),
            "parent_station": r.try_get::<Option<String>, _>("parent_station")?,
            "stop_type": r.try_get::<String, _>("stop_type")?,
            "marker_id": r.try_get::<Option<String>, _>("marker_id")?,
            "marker_name": r.try_get::<Option<String>, _>("marker_name")?,
            "marker_lat": r.try_get::<Option<f64>, _>("marker_lat")?,
            "marker_lon": r.try_get::<Option<f64>, _>("marker_lon")?,
            "stop_name_override": r.try_get::<Option<String>, _>("stop_name_override")?,
        }));
    }
    Ok(out)
}

fn served_count(rows: &[Value]) -> usize {
    rows.iter()
        .filter(|r| {
            r["stop_type"]
                .as_str()
                .is_some_and(|t| !UNSERVED_TYPES.contains(&t))
        })
        .count()
}

const STAGE_COLS: &str =
    "stage_id, name, direction, description, provenance::text AS provenance, deleted, \
     row_version, created_at, updated_at, updated_by";

fn stage_json(r: &PgRow) -> Result<Value, sqlx::Error> {
    Ok(json!({
        "stage_id": r.try_get::<String, _>("stage_id")?,
        "name": r.try_get::<String, _>("name")?,
        "description": r.try_get::<Option<String>, _>("description")?,
        "direction": r.try_get::<Option<String>, _>("direction")?,
        "provenance": json_col(r, "provenance")?,
        "deleted": r.try_get::<bool, _>("deleted")?,
        "row_version": r.try_get::<i32, _>("row_version")?,
        "created_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?,
        "updated_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?,
        "updated_by": r.try_get::<Option<String>, _>("updated_by")?,
    }))
}

pub struct StageQuery {
    pub q: Option<String>,
    pub stop_id: Option<String>,
    pub route_id: Option<String>,
    /// "up" or "down": the two directions of one corridor share a name, so
    /// this is how a search tells them apart.
    pub direction: Option<String>,
    /// Only stages no live route uses.
    pub unused: bool,
}

/// `GET /feeds/{g}/stages`: stages by name or id, by a stop they call at, or by
/// a route that uses them; each with its first and last stop and how many stops
/// and routes it has.
pub async fn list_stages(
    state: &EditorState,
    g: &str,
    query: &StageQuery,
    page: &Page,
) -> EditorResult<Value> {
    let q = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let rows = sqlx::query(&format!(
        "SELECT {cols}, \
            (SELECT count(*) FROM gtfs_stage_stop ss WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id \
               AND ss.stop_type NOT IN ('ROUTE CORRECTION', 'JUMP STOP', 'HIDDEN STOP')) AS stop_count, \
            (SELECT count(*) FROM gtfs_stage_stop ss \
               JOIN gtfs_stop st ON st.gtfs_id = ss.gtfs_id AND st.stop_id = ss.stop_id \
              WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id AND st.unserviceable) AS out_of_use, \
            (SELECT count(DISTINCT rs.route_id) FROM gtfs_route_stage rs \
               JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
               WHERE rs.gtfs_id = s.gtfs_id AND rs.stage_id = s.stage_id) AS route_count, \
            f.stop_id AS first_stop_id, f.stop_name AS first_stop_name, \
            l.stop_id AS last_stop_id, l.stop_name AS last_stop_name \
         FROM gtfs_stage s \
         LEFT JOIN LATERAL (SELECT ss.stop_id, coalesce(ss.stop_name_override, st.name) AS stop_name \
                            FROM gtfs_stage_stop ss JOIN gtfs_stop st ON st.gtfs_id = ss.gtfs_id AND st.stop_id = ss.stop_id \
                            WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id \
                            ORDER BY ss.position LIMIT 1) f ON true \
         LEFT JOIN LATERAL (SELECT ss.stop_id, coalesce(ss.stop_name_override, st.name) AS stop_name \
                            FROM gtfs_stage_stop ss JOIN gtfs_stop st ON st.gtfs_id = ss.gtfs_id AND st.stop_id = ss.stop_id \
                            WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id \
                            ORDER BY ss.position DESC LIMIT 1) l ON true \
         WHERE s.gtfs_id = $1 AND NOT s.deleted \
           AND ($2::text IS NULL OR s.stage_id = $2 OR s.name ILIKE $3) \
           AND ($4::text IS NULL OR EXISTS (SELECT 1 FROM gtfs_stage_stop ss \
                WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id AND ss.stop_id = $4)) \
           AND ($5::text IS NULL OR EXISTS (SELECT 1 FROM gtfs_route_stage rs \
                WHERE rs.gtfs_id = s.gtfs_id AND rs.stage_id = s.stage_id AND rs.route_id = $5)) \
           AND (NOT $6 OR NOT EXISTS (SELECT 1 FROM gtfs_route_stage rs \
                JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
                WHERE rs.gtfs_id = s.gtfs_id AND rs.stage_id = s.stage_id)) \
           AND ($7::text IS NULL OR s.direction = $7) \
         ORDER BY (s.stage_id = $2 OR lower(s.name) = lower($2)) DESC NULLS LAST, s.name, s.direction, s.stage_id \
         LIMIT $8 OFFSET $9",
        cols = STAGE_COLS
            .split(", ")
            .map(|c| format!("s.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .bind(g)
    .bind(q)
    .bind(q.map(like_pattern))
    .bind(query.stop_id.as_deref().filter(|s| !s.is_empty()))
    .bind(query.route_id.as_deref().filter(|s| !s.is_empty()))
    .bind(query.unused)
    .bind(
        query
            .direction
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty()),
    )
    .bind(page.limit + 1)
    .bind(page.offset)
    .fetch_all(&state.pool)
    .await?;
    let items = rows
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            let mut v = stage_json(r)?;
            v["stop_count"] = json!(r.try_get::<i64, _>("stop_count")?);
            // how many of its stops nobody can board at now (section 21)
            v["out_of_use"] = json!(r.try_get::<i64, _>("out_of_use")?);
            v["route_count"] = json!(r.try_get::<i64, _>("route_count")?);
            v["first_stop"] = json!({
                "stop_id": r.try_get::<Option<String>, _>("first_stop_id")?,
                "name": r.try_get::<Option<String>, _>("first_stop_name")?,
            });
            v["last_stop"] = json!({
                "stop_id": r.try_get::<Option<String>, _>("last_stop_id")?,
                "name": r.try_get::<Option<String>, _>("last_stop_name")?,
            });
            Ok(v)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(page.wrap(items))
}

/// The stage row, or None.
async fn stage_row(conn: &mut PgConnection, g: &str, id: &str) -> EditorResult<Option<Value>> {
    let row = sqlx::query(&format!(
        "SELECT {STAGE_COLS} FROM gtfs_stage WHERE gtfs_id = $1 AND stage_id = $2"
    ))
    .bind(g)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(stage_json).transpose()?)
}

/// `GET /feeds/{g}/stages/{id}`: the stage, its stops in order, and every live
/// route that uses it (a route may use a stage more than once).
pub async fn stage_detail(conn: &mut PgConnection, g: &str, id: &str) -> EditorResult<Value> {
    let mut stage = stage_row(conn, g, id)
        .await?
        .ok_or_else(|| EditorError::not_found("stage_not_found", format!("no stage {id}")))?;
    let rows = stages_read_rows(conn, g, &[id.to_string()])
        .await?
        .remove(id)
        .unwrap_or_default();
    let routes = sqlx::query(
        "SELECT rs.route_id, r.short_name, r.long_name, rs.position, rs.stage_no, rs.variant_id, \
                (rs.variant_id IS NOT DISTINCT FROM r.active_variant_id) AS running \
         FROM gtfs_route_stage rs \
         JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
         WHERE rs.gtfs_id = $1 AND rs.stage_id = $2 \
         ORDER BY r.short_name, rs.route_id, rs.variant_id NULLS FIRST, rs.position",
    )
    .bind(g)
    .bind(id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        Ok(json!({
            "route_id": r.try_get::<String, _>("route_id")?,
            "short_name": r.try_get::<Option<String>, _>("short_name")?,
            "long_name": r.try_get::<Option<String>, _>("long_name")?,
            "position": r.try_get::<i32, _>("position")?,
            "stage_no": r.try_get::<i32, _>("stage_no")?,
            // which of the route's lists this is: null is its normal one, a
            // name is one of its temporary routes (section 20)
            "variant_id": r.try_get::<Option<String>, _>("variant_id")?,
            "running": r.try_get::<bool, _>("running")?,
        }))
    })
    .collect::<Result<Vec<_>, _>>()?;
    let distinct: HashSet<&str> = routes
        .iter()
        .filter_map(|r| r["route_id"].as_str())
        .collect();
    stage["route_count"] = json!(distinct.len());
    stage["stop_count"] = json!(served_count(&rows));
    stage["rows"] = json!(rows);
    stage["routes"] = json!(routes);
    Ok(stage)
}

/// `GET /feeds/{g}/routes/{route_id}/stages`: the route's stages in order, each
/// with its stops. `has_stages` false means the route is not built from stages
/// yet; `in_sync` false that its stop list no longer matches them.
pub async fn route_stages_detail(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
) -> EditorResult<Value> {
    route_stages_of(conn, g, route_id, None).await
}

/// One of a route's stage lists; `None` is its normal one, a temporary route's
/// id one of the others (section 19).
pub async fn route_stages_of(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    variant: Option<&str>,
) -> EditorResult<Value> {
    if svc::route_row(conn, g, route_id).await?.is_none() {
        return Err(EditorError::not_found(
            "route_not_found",
            format!("no route {route_id}"),
        ));
    }
    let links = sqlx::query(
        "SELECT rs.position, rs.stage_id, rs.stage_no, s.name, s.description, s.deleted, s.row_version, \
            (SELECT count(DISTINCT o.route_id) FROM gtfs_route_stage o \
               JOIN gtfs_route r ON r.gtfs_id = o.gtfs_id AND r.route_id = o.route_id AND NOT r.deleted \
               WHERE o.gtfs_id = rs.gtfs_id AND o.stage_id = rs.stage_id) AS route_count \
         FROM gtfs_route_stage rs JOIN gtfs_stage s ON s.gtfs_id = rs.gtfs_id AND s.stage_id = rs.stage_id \
         WHERE rs.gtfs_id = $1 AND rs.route_id = $2 \
           AND rs.variant_id IS NOT DISTINCT FROM $3 ORDER BY rs.position",
    )
    .bind(g)
    .bind(route_id)
    .bind(variant)
    .fetch_all(&mut *conn)
    .await?;
    let ids: Vec<String> = links
        .iter()
        .map(|r| r.try_get("stage_id"))
        .collect::<Result<HashSet<String>, _>>()?
        .into_iter()
        .collect();
    let mut rows = stages_read_rows(conn, g, &ids).await?;
    let stages = links
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            let id: String = r.try_get("stage_id")?;
            let stage_rows = rows.get(&id).cloned().unwrap_or_default();
            Ok(json!({
                "position": r.try_get::<i32, _>("position")?,
                "stage_id": id,
                "stage_no": r.try_get::<i32, _>("stage_no")?,
                "name": r.try_get::<String, _>("name")?,
                "description": r.try_get::<Option<String>, _>("description")?,
                "deleted": r.try_get::<bool, _>("deleted")?,
                "row_version": r.try_get::<i32, _>("row_version")?,
                "route_count": r.try_get::<i64, _>("route_count")?,
                "stop_count": served_count(&stage_rows),
                "rows": stage_rows,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    rows.clear();
    let has = !stages.is_empty();
    let worn = active_variant(conn, g, route_id).await?;
    let in_sync = has
        && worn.as_deref() == variant
        && same_rows(
            &flatten_route(conn, g, route_id, variant).await?,
            &load_route_rows(conn, g, route_id).await?,
        );
    Ok(json!({
        "route_id": route_id,
        "has_stages": has,
        "in_sync": in_sync,
        "stages_hash": live_links_hash(conn, g, route_id, variant).await?,
        "stages": stages,
    }))
}

/// A stage as a change's `before` shows it (and its version), or None.
pub(super) async fn stage_snapshot(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
) -> EditorResult<Option<(Value, i32)>> {
    if stage_row(conn, g, id).await?.is_none() {
        return Ok(None);
    }
    let detail = stage_detail(conn, g, id).await?;
    let version = detail["row_version"].as_i64().unwrap_or(0) as i32;
    Ok(Some((detail, version)))
}

/// A route's stage list as a change's `before` shows it.
pub(super) async fn route_stages_snapshot(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
) -> EditorResult<Value> {
    Ok(json!(sqlx::query(
        "SELECT rs.position, rs.stage_id, rs.stage_no, s.name FROM gtfs_route_stage rs \
         JOIN gtfs_stage s ON s.gtfs_id = rs.gtfs_id AND s.stage_id = rs.stage_id \
         WHERE rs.gtfs_id = $1 AND rs.route_id = $2 ORDER BY rs.position",
    )
    .bind(g)
    .bind(route_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<Value, sqlx::Error> {
        Ok(json!({
            "position": r.try_get::<i32, _>("position")?,
            "stage_id": r.try_get::<String, _>("stage_id")?,
            "stage_no": r.try_get::<i32, _>("stage_no")?,
            "name": r.try_get::<String, _>("name")?,
        }))
    })
    .collect::<Result<Vec<_>, _>>()?))
}

// ---------------------------------------------------------------- previews

/// A read of the live tables with a draft applied: the draft is evaluated in a
/// transaction that is rolled back, and `read` runs inside it. The draft's
/// findings and conflicts are added to what `read` returns.
async fn with_draft_applied<F>(
    state: &EditorState,
    ctx: &Ctx,
    set_id: Uuid,
    read: F,
) -> EditorResult<Value>
where
    F: for<'c> Fn(
        &'c mut PgConnection,
        &'c str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = EditorResult<Value>> + Send + 'c>,
    >,
{
    retry_transient(|| async {
        let mut tx = state.pool.begin().await?;
        let set = svc::load_set(&mut tx, set_id, false).await?;
        let changes = svc::load_changes(&mut tx, set_id).await?;
        lock_feed(&mut tx, &set.gtfs_id).await?;
        let ev = svc::evaluate(&mut tx, &set.gtfs_id, &changes, &ctx.user.email).await?;
        let out = read(&mut tx, &set.gtfs_id).await;
        tx.rollback().await?;
        let mut out = out?;
        out["validation"] = json!(ev.validation);
        out["conflicts"] = json!(ev.conflicts);
        Ok(out)
    })
    .await
}

/// `GET /change-sets/{id}/preview/stages/{stage_id}`
pub async fn preview_stage(
    state: &EditorState,
    ctx: &Ctx,
    set_id: Uuid,
    stage_id: &str,
) -> EditorResult<Value> {
    let stage_id = stage_id.to_string();
    with_draft_applied(state, ctx, set_id, |conn, g| {
        let stage_id = stage_id.clone();
        Box::pin(async move { stage_detail(conn, g, &stage_id).await })
    })
    .await
}

/// `GET /change-sets/{id}/preview/routes/{route_id}/stages`
pub async fn preview_route_stages(
    state: &EditorState,
    ctx: &Ctx,
    set_id: Uuid,
    route_id: &str,
) -> EditorResult<Value> {
    let route_id = route_id.to_string();
    with_draft_applied(state, ctx, set_id, |conn, g| {
        let route_id = route_id.clone();
        Box::pin(async move { route_stages_detail(conn, g, &route_id).await })
    })
    .await
}

/// A route's temporary routes with the draft applied (section 19).
pub async fn preview_route_variants(
    state: &EditorState,
    ctx: &Ctx,
    set_id: Uuid,
    route_id: &str,
) -> EditorResult<Value> {
    let route_id = route_id.to_string();
    with_draft_applied(state, ctx, set_id, |conn, g| {
        let route_id = route_id.clone();
        Box::pin(async move { Ok(super::variants::route_variants(conn, g, &route_id).await?) })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(stop: &str, typ: &str, no: i32, name: &str) -> RouteRow {
        RouteRow {
            stop_id: Some(stop.into()),
            stop_type: typ.into(),
            stage_no: no,
            stage_name: name.into(),
            marker_id: None,
            marker_name: None,
            marker_lat: None,
            marker_lon: None,
            stop_name_override: None,
            provider_id: None,
            ..Default::default()
        }
    }

    #[test]
    fn rows_compare_without_the_provider_id() {
        let a = vec![row("A", "NEW STOP", 1, "X")];
        let mut b = a.clone();
        b[0].provider_id = Some("75".into());
        assert!(same_rows(&a, &b));
        b[0].stage_no = 2;
        assert!(!same_rows(&a, &b));
        assert!(!same_rows(&a, &[]));
    }

    #[test]
    fn links_hash_numbers_links_before_hashing() {
        // no stages hashes like no rows, so a route that has neither has one base
        assert_eq!(
            links_hash(&[]),
            "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945"
        );
        let implicit = [
            StageLink {
                stage_id: "a".into(),
                stage_no: None,
            },
            StageLink {
                stage_id: "b".into(),
                stage_no: None,
            },
        ];
        let explicit = [
            StageLink {
                stage_id: "a".into(),
                stage_no: Some(1),
            },
            StageLink {
                stage_id: "b".into(),
                stage_no: Some(2),
            },
        ];
        assert_eq!(links_hash(&implicit), links_hash(&explicit));
    }

    #[test]
    fn route_names_read_as_a_list() {
        let r = |id: &str, s: Option<&str>| (id.to_string(), s.map(str::to_string));
        assert_eq!(route_names(&[r("2614", Some("21G"))]), "21G (2614)");
        assert_eq!(
            route_names(&[r("1", Some("1")), r("2", None), r("3", Some("5C"))]),
            "1, 2 and 5C (3)"
        );
        let many: Vec<_> = (0..25).map(|i| r(&i.to_string(), None)).collect();
        assert!(route_names(&many).ends_with("and 5 more"));
    }
}
