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

trait EmptyToNone {
    fn into_option_when_not_empty(self) -> Option<String>;
}
impl EmptyToNone for String {
    fn into_option_when_not_empty(self) -> Option<String> {
        (!self.is_empty()).then_some(self)
    }
}

/// What names one stage: MTC's stop id for the fare-stage head, and which way
/// the route runs. Both are needed - 1,746 of MTC's 1,836 fare-stage stops are
/// used in both directions, and the stops after the boundary differ - so the
/// two make up the stage's primary key, and a `StageKey` is what every read and
/// write of a stage takes. A draft's change names its target with a single
/// `entity_key`, so the pair is written there as `<stage_id>|<direction>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct StageKey {
    pub stage_id: String,
    /// "up", "down", or "" for a stage that runs the same either way.
    pub direction: String,
}

impl StageKey {
    pub fn new(stage_id: &str, direction: Option<&str>) -> StageKey {
        StageKey {
            stage_id: stage_id.to_string(),
            direction: direction.unwrap_or("").to_string(),
        }
    }

    /// The pair as a draft's `entity_key`. The separator is `|`, which no stop
    /// id of MTC's holds.
    pub fn entity_key(&self) -> String {
        format!("{}|{}", self.stage_id, self.direction)
    }

    /// Read back what `entity_key` wrote. A key with no separator is a stage id
    /// on its own, from before direction joined the key.
    pub fn parse(entity_key: &str) -> StageKey {
        match entity_key.split_once('|') {
            Some((id, direction)) => StageKey::new(id, Some(direction)),
            None => StageKey::new(entity_key, None),
        }
    }

    /// What to show a person: the id, and the direction when it has one.
    pub fn label(&self) -> String {
        if self.direction.is_empty() {
            self.stage_id.clone()
        } else {
            format!("{} ({})", self.stage_id, self.direction)
        }
    }

    pub fn direction_opt(&self) -> Option<&str> {
        (!self.direction.is_empty()).then_some(self.direction.as_str())
    }
}

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

/// The stage a link names: its id and direction, however the link spells them.
pub fn link_key(l: &StageLink) -> StageKey {
    let mut key = StageKey::parse(l.stage_id.trim());
    if let Some(d) = l.direction.as_deref() {
        key.direction = d.trim().to_string();
    }
    key
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

/// The stage a path or a link names. `148|up` says which; a bare `148` is taken
/// as the only stage with that id, and says so when there is more than one.
pub async fn resolve_key(conn: &mut PgConnection, g: &str, raw: &str) -> EditorResult<StageKey> {
    if raw.contains('|') {
        return Ok(StageKey::parse(raw));
    }
    let rows = sqlx::query(
        "SELECT direction FROM gtfs_stage WHERE gtfs_id = $1 AND stage_id = $2 AND NOT deleted \
         ORDER BY direction",
    )
    .bind(g)
    .bind(raw)
    .fetch_all(&mut *conn)
    .await?;
    let ways: Vec<String> = rows
        .iter()
        .map(|r| r.try_get::<String, _>("direction"))
        .collect::<Result<_, _>>()?;
    match ways.len() {
        0 => Err(EditorError::not_found(
            "stage_not_found",
            format!("no stage {raw}"),
        )),
        1 => Ok(StageKey::new(raw, Some(&ways[0]))),
        _ => Err(EditorError::bad_request(
            "stage_direction_needed",
            format!(
                "stage {raw} runs both ways; name which one, as {}",
                ways.iter()
                    .map(|d| format!("{raw}|{d}"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
        )),
    }
}

/// The stage a link names. A link that states a direction, or carries it in its
/// id as `148|up`, says which; one that gives a bare id is taken as the only
/// stage with that id, as a path is. `Err` when the id names more than one and
/// the link does not say which.
pub async fn resolve_link(
    conn: &mut PgConnection,
    g: &str,
    l: &StageLink,
) -> EditorResult<StageKey> {
    let stated = link_key(l);
    if l.direction.is_some() || l.stage_id.contains('|') {
        return Ok(stated);
    }
    resolve_key(conn, g, &stated.stage_id).await
}

/// The stage a draft's change names. `148|up` says which; a bare id is resolved
/// when only one stage carries it, which is what a change written before
/// direction joined the key looks like, and what the dashboard sends for a stage
/// that runs one way only.
pub async fn key_of_change(conn: &mut PgConnection, g: &str, key: &str) -> StageKey {
    match resolve_key(conn, g, key).await {
        Ok(sk) => sk,
        Err(_) => StageKey::parse(key),
    }
}

/// A stage and its rows; `lock` holds the stage row until the transaction ends.
pub async fn load_stage(
    conn: &mut PgConnection,
    g: &str,
    key: &StageKey,
    lock: bool,
) -> Result<Option<LiveStage>, sqlx::Error> {
    let Some(row) = sqlx::query(&format!(
        "SELECT name, direction, description, deleted, row_version FROM gtfs_stage \
         WHERE gtfs_id = $1 AND stage_id = $2 AND direction = $3{}",
        if lock { " FOR UPDATE" } else { "" }
    ))
    .bind(g)
    .bind(&key.stage_id)
    .bind(&key.direction)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let rows = sqlx::query(&format!(
        "SELECT {STAGE_ROW_COLS} FROM gtfs_stage_stop \
         WHERE gtfs_id = $1 AND stage_id = $2 AND direction = $3 ORDER BY position"
    ))
    .bind(g)
    .bind(&key.stage_id)
    .bind(&key.direction)
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
        "SELECT stage_id, direction, stage_no FROM gtfs_route_stage \
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
            direction: Some(r.try_get::<String, _>("direction")?),
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
            direction: l.direction.clone(),
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
                           AND st.direction = rs.direction \
         JOIN gtfs_stage_stop ss ON ss.gtfs_id = rs.gtfs_id AND ss.stage_id = rs.stage_id \
                                AND ss.direction = rs.direction \
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
    key: &StageKey,
) -> Result<Vec<(String, Option<String>)>, sqlx::Error> {
    sqlx::query(
        "SELECT DISTINCT r.route_id, r.short_name FROM gtfs_route_stage rs \
         JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
         WHERE rs.gtfs_id = $1 AND rs.stage_id = $2 AND rs.direction = $3 \
           AND rs.variant_id IS NOT DISTINCT FROM r.active_variant_id \
         ORDER BY r.short_name, r.route_id",
    )
    .bind(g)
    .bind(&key.stage_id)
    .bind(&key.direction)
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
    key: &StageKey,
    rows: &[StageRow],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM gtfs_stage_stop WHERE gtfs_id = $1 AND stage_id = $2 AND direction = $3",
    )
    .bind(g)
    .bind(&key.stage_id)
    .bind(&key.direction)
    .execute(&mut *conn)
    .await?;
    let pos: Vec<i32> = (1..=rows.len() as i32).collect();
    let col = |f: fn(&StageRow) -> Option<String>| rows.iter().map(f).collect::<Vec<_>>();
    sqlx::query(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, direction, position, stop_id, stop_type, \
                                      marker_id, marker_name, marker_lat, marker_lon, \
                                      stop_name_override) \
         SELECT $1, $2, $3, u.pos, u.stop, u.typ, u.mid, u.mname, u.mlat, u.mlon, u.over \
         FROM UNNEST($4::int4[], $5::text[], $6::text[], $7::text[], $8::text[], $9::float8[], \
                     $10::float8[], $11::text[]) AS u(pos, stop, typ, mid, mname, mlat, mlon, over)",
    )
    .bind(g)
    .bind(&key.stage_id)
    .bind(&key.direction)
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
    links: &[(StageKey, i32)],
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
             (gtfs_id, route_id, variant_id, position, stage_id, direction, stage_no, updated_by) \
         SELECT $1, $2, $8, u.pos, u.stage, u.dir, u.no, $7 \
         FROM UNNEST($3::int4[], $4::text[], $5::text[], $6::int4[]) AS u(pos, stage, dir, no)",
    )
    .bind(g)
    .bind(route_id)
    .bind((1..=links.len() as i32).collect::<Vec<_>>())
    .bind(
        links
            .iter()
            .map(|l| l.0.stage_id.clone())
            .collect::<Vec<_>>(),
    )
    .bind(
        links
            .iter()
            .map(|l| l.0.direction.clone())
            .collect::<Vec<_>>(),
    )
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
                              AND ch.entity = 'stage' AND ch.op IN ('create', 'split') \
                              AND ch.entity_key = $2) AS used",
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
    // the change names its stage as <stage_id>|<direction>; a payload that
    // states a direction of its own must agree with it
    let mut sk = StageKey::parse(key);
    if let Some(d) = direction_in(after).flatten() {
        sk.direction = d;
    }
    let rows = stored_stage_rows(&sk.stage_id, &rows_in(after)?.unwrap_or_default());
    let findings = stage_checks(conn, g, &rows).await?;
    if has_error(&findings) {
        return Err(ApplyError::Findings(findings));
    }
    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM gtfs_stage \
         WHERE gtfs_id = $1 AND stage_id = $2 AND direction = $3) AS e",
    )
    .bind(g)
    .bind(&sk.stage_id)
    .bind(&sk.direction)
    .fetch_one(&mut *conn)
    .await?
    .try_get("e")?;
    if exists {
        return Err(fail(
            "stage_exists",
            format!("stage {} already exists", sk.label()),
        ));
    }
    sqlx::query(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, direction, name, description, provenance, updated_by) \
         VALUES ($1, $2, $3, $4, $5, '{\"source\": \"editor\"}'::jsonb, $6)",
    )
    .bind(g)
    .bind(&sk.stage_id)
    .bind(&sk.direction)
    .bind(&name)
    .bind(description_in(after).flatten())
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    write_stage_rows(conn, g, &sk, &rows).await?;
    Ok(findings)
}

/// Load a stage an update or delete acts on, locked, or the finding why not.
async fn stage_to_change(
    conn: &mut PgConnection,
    g: &str,
    key: &StageKey,
) -> Result<LiveStage, ApplyError> {
    let stage = load_stage(conn, g, key, true)
        .await?
        .ok_or_else(|| fail("stage_not_found", format!("no stage {}", key.label())))?;
    if stage.deleted {
        return Err(fail(
            "stage_deleted",
            format!("stage {} is deleted", key.label()),
        ));
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
    let sk = key_of_change(conn, g, key).await;
    let live = stage_to_change(conn, g, &sk).await?;
    let name = after["name"]
        .as_str()
        .map(|n| n.trim().to_string())
        .unwrap_or_else(|| live.name.clone());
    let description = description_in(after).unwrap_or_else(|| live.description.clone());
    // Direction is half the stage's key: changing it would not change this
    // stage, it would name a different one. Make a stage for the other
    // direction and point the routes at it instead.
    if let Some(d) = direction_in(after).flatten() {
        if d != sk.direction {
            return Err(fail(
                "stage_direction_fixed",
                format!(
                    "stage {} runs {}; direction is part of a stage's key, so it cannot be \
                     changed. Make the stage for the other direction and give it to the routes.",
                    sk.stage_id,
                    if sk.direction.is_empty() {
                        "either way"
                    } else {
                        &sk.direction
                    }
                ),
            ));
        }
    }
    let rows = match rows_in(after)? {
        Some(rows) => stored_stage_rows(&sk.stage_id, &rows),
        None => live.rows.clone(),
    };
    let mut findings = stage_checks(conn, g, &rows).await?;
    if has_error(&findings) {
        return Err(ApplyError::Findings(findings));
    }

    // Every route using the stage must be what its stages say before the change,
    // or rewriting it from them would undo an edit made some other way.
    let routes = routes_using(conn, g, &sk).await?;
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
        "UPDATE gtfs_stage SET name = $4, description = $5, updated_by = $6 \
         WHERE gtfs_id = $1 AND stage_id = $2 AND direction = $3",
    )
    .bind(g)
    .bind(&sk.stage_id)
    .bind(&sk.direction)
    .bind(&name)
    .bind(&description)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    write_stage_rows(conn, g, &sk, &rows).await?;

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

/// `stage/merge`: the stage `key` names goes away and every route using it is
/// pointed at `into_stage_id` instead.
///
/// **Only within one direction.** A stage's key is its stop and its direction,
/// and the two directions of a corridor hold different stops - the up stage runs
/// on one way, the down stage the other - so merging across them would hand a
/// route the other way's stops. Refused with `merge_across_directions`.
pub(super) async fn stage_merge(
    conn: &mut PgConnection,
    g: &str,
    key: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let gone = key_of_change(conn, g, key).await;
    let into = key_of_change(
        conn,
        g,
        after["into_stage_id"].as_str().unwrap_or("").trim(),
    )
    .await;
    if into.stage_id.is_empty() {
        return Err(fail(
            "invalid_payload",
            "into_stage_id names the stage to keep",
        ));
    }
    if gone == into {
        return Err(fail(
            "merge_same_stage",
            "a stage cannot be merged into itself",
        ));
    }
    if gone.direction != into.direction {
        return Err(fail(
            "merge_across_directions",
            format!(
                "{} runs {} and {} runs {}; the two directions of a corridor hold \
                 different stops, so a stage is only merged into one going the same way",
                gone.label(),
                if gone.direction.is_empty() {
                    "either way"
                } else {
                    &gone.direction
                },
                into.label(),
                if into.direction.is_empty() {
                    "either way"
                } else {
                    &into.direction
                }
            ),
        ));
    }
    let live = stage_to_change(conn, g, &gone).await?;
    let keeper = load_stage(conn, g, &into, true)
        .await?
        .ok_or_else(|| fail("stage_not_found", format!("no stage {}", into.label())))?;
    if keeper.deleted {
        return Err(fail(
            "stage_deleted",
            format!("stage {} is deleted", into.label()),
        ));
    }

    // Every route using either stage must match its stages before the change,
    // or rewriting it from them would undo an edit made some other way.
    let routes = routes_using(conn, g, &gone).await?;
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
        return Err(fail(
            "route_out_of_sync",
            format!(
                "the stop list of {} was changed outside its stages, so merging \
                 stage {} would undo that; set that route's stages again first",
                route_names(&stale),
                gone.label()
            ),
        ));
    }

    sqlx::query(
        "UPDATE gtfs_route_stage SET stage_id = $4, direction = $5, updated_by = $6 \
         WHERE gtfs_id = $1 AND stage_id = $2 AND direction = $3",
    )
    .bind(g)
    .bind(&gone.stage_id)
    .bind(&gone.direction)
    .bind(&into.stage_id)
    .bind(&into.direction)
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE gtfs_stage SET deleted = true, review = NULL, updated_by = $4 \
         WHERE gtfs_id = $1 AND stage_id = $2 AND direction = $3",
    )
    .bind(g)
    .bind(&gone.stage_id)
    .bind(&gone.direction)
    .bind(actor)
    .execute(&mut *conn)
    .await?;

    // Each route now calls at the kept stage's stops where it called at the
    // merged one's: say what that changed, and write it.
    let mut findings = Vec::new();
    for ((route_id, short), live_rows) in routes.iter().zip(&before) {
        let worn = active_variant(conn, g, route_id).await?;
        let rows = flatten_route(conn, g, route_id, worn.as_deref()).await?;
        let label = route_names(&[(route_id.clone(), short.clone())]);
        for mut f in grade_against_live(check_route_rows(&rows), &check_route_rows(live_rows)) {
            f.message = format!("route {label}: {}", f.message);
            f.key = format!("{route_id}|{}", f.key);
            findings.push(f);
        }
        write_route_rows(conn, g, route_id, &rows, live_rows, actor).await?;
    }
    if blocks_apply(&findings) {
        return Err(ApplyError::Findings(findings));
    }
    if !routes.is_empty() {
        findings.push(Finding::warning(
            "stage_merged",
            &gone.entity_key(),
            format!(
                "{} ({}) is merged into {} ({}), which changes the stop list of {}: {}",
                live.name,
                gone.label(),
                keeper.name,
                into.label(),
                plural(routes.len(), "route"),
                route_names(&routes)
            ),
        ));
    }
    Ok(findings)
}

/// Split some of a stage's routes off onto a stage of their own.
///
/// One change, however many routes: the reviewer has decided these routes are
/// not this stage at all, so they get a new stage carrying the stops they
/// actually give and come off the old one, which keeps every other route. The
/// dashboard used to do this as a stage create followed by a `route_stages`
/// replace per route - two calls per route, 95 of them for a stage 47 routes
/// run - and a half-finished split was a real outcome. Here it is one row in
/// the draft and one transaction at apply.
pub(super) async fn stage_split(
    conn: &mut PgConnection,
    g: &str,
    key: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let from = key_of_change(
        conn,
        g,
        after["from_stage_id"].as_str().unwrap_or("").trim(),
    )
    .await;
    if from.stage_id.is_empty() {
        return Err(fail(
            "invalid_payload",
            "from_stage_id names the stage these routes come off",
        ));
    }
    let routes: Vec<String> = after["routes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if routes.is_empty() {
        return Err(fail("invalid_payload", "routes names the routes to move"));
    }
    let old = load_stage(conn, g, &from, true)
        .await?
        .ok_or_else(|| fail("stage_not_found", format!("no stage {}", from.label())))?;
    if old.deleted {
        return Err(fail(
            "stage_deleted",
            format!("stage {} is deleted", from.label()),
        ));
    }

    // every named route must be on the stage it is being taken off
    let on_it: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT route_id FROM gtfs_route_stage \
          WHERE gtfs_id = $1 AND stage_id = $2 AND direction = $3 AND route_id = ANY($4)",
    )
    .bind(g)
    .bind(&from.stage_id)
    .bind(&from.direction)
    .bind(&routes)
    .fetch_all(&mut *conn)
    .await?;
    let missing: Vec<String> = routes
        .iter()
        .filter(|r| !on_it.contains(r))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(fail(
            "route_not_on_stage",
            format!(
                "{} does not run stage {}",
                route_names(
                    &missing
                        .iter()
                        .map(|r| (r.clone(), None))
                        .collect::<Vec<_>>()
                ),
                from.label()
            ),
        ));
    }
    // and none of them may have been edited outside its stages, or rewriting it
    // from them would undo that
    let mut before: Vec<Vec<RouteRow>> = Vec::with_capacity(routes.len());
    let mut stale = Vec::new();
    for route_id in &routes {
        let live_rows = load_route_rows(conn, g, route_id).await?;
        let worn = active_variant(conn, g, route_id).await?;
        if !same_rows(
            &flatten_route(conn, g, route_id, worn.as_deref()).await?,
            &live_rows,
        ) {
            stale.push((route_id.clone(), None));
        }
        before.push(live_rows);
    }
    if !stale.is_empty() {
        return Err(fail(
            "route_out_of_sync",
            format!(
                "the stop list of {} was changed outside its stages, so moving it \
                 would undo that; set that route's stages again first",
                route_names(&stale)
            ),
        ));
    }

    // the new stage: its own key, the direction it is asked for, or the one the
    // stage it comes off runs
    let mut made = StageKey::parse(key);
    made.direction = match direction_in(after).flatten() {
        Some(d) => d,
        None => from.direction.clone(),
    };
    let mut spawn = after.clone();
    spawn["direction"] = json!(made.direction);
    let findings = stage_create(conn, g, &made.entity_key(), &spawn, actor).await?;

    sqlx::query(
        "UPDATE gtfs_route_stage SET stage_id = $4, direction = $5, updated_by = $6 \
          WHERE gtfs_id = $1 AND stage_id = $2 AND direction = $3 AND route_id = ANY($7)",
    )
    .bind(g)
    .bind(&from.stage_id)
    .bind(&from.direction)
    .bind(&made.stage_id)
    .bind(&made.direction)
    .bind(actor)
    .bind(&routes)
    .execute(&mut *conn)
    .await?;

    // each moved route now calls at the new stage's stops: say what that
    // changed, and write it
    let mut findings = findings;
    for (route_id, live_rows) in routes.iter().zip(&before) {
        let worn = active_variant(conn, g, route_id).await?;
        let rows = flatten_route(conn, g, route_id, worn.as_deref()).await?;
        for mut f in grade_against_live(check_route_rows(&rows), &check_route_rows(live_rows)) {
            f.message = format!("route {route_id}: {}", f.message);
            f.key = format!("{route_id}|{}", f.key);
            findings.push(f);
        }
        write_route_rows(conn, g, route_id, &rows, live_rows, actor).await?;
    }
    if blocks_apply(&findings) {
        return Err(ApplyError::Findings(findings));
    }
    let left = routes_using(conn, g, &from).await?;
    findings.push(Finding::warning(
        "stage_split",
        &made.entity_key(),
        format!(
            "{} ({}) is a new stage for {}: {}. {} ({}) keeps {}",
            after["name"].as_str().unwrap_or("").trim(),
            made.label(),
            plural(routes.len(), "route"),
            route_names(&routes.iter().map(|r| (r.clone(), None)).collect::<Vec<_>>()),
            old.name,
            from.label(),
            plural(left.len(), "route"),
        ),
    ));
    Ok(findings)
}

/// Soft-delete a stage no live route uses.
pub(super) async fn stage_delete(
    conn: &mut PgConnection,
    g: &str,
    key: &str,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let sk = key_of_change(conn, g, key).await;
    stage_to_change(conn, g, &sk).await?;
    let routes = routes_using(conn, g, &sk).await?;
    if !routes.is_empty() {
        return Err(fail(
            "stage_in_use",
            format!(
                "stage {} is used by {}: {}; take it off those routes first",
                sk.label(),
                plural(routes.len(), "route"),
                route_names(&routes)
            ),
        ));
    }
    sqlx::query(
        "UPDATE gtfs_stage SET deleted = true, updated_by = $4 \
         WHERE gtfs_id = $1 AND stage_id = $2 AND direction = $3",
    )
    .bind(g)
    .bind(&sk.stage_id)
    .bind(&sk.direction)
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
    let mut stages: HashMap<StageKey, LiveStage> = HashMap::new();
    let mut resolved: Vec<StageKey> = Vec::with_capacity(links.len());
    for l in &links {
        let sk = match resolve_link(conn, g, l).await {
            Ok(sk) => sk,
            Err(e) => {
                findings.push(Finding::error(
                    "stage_not_found",
                    l.stage_id.trim(),
                    e.to_string(),
                ));
                resolved.push(link_key(l));
                continue;
            }
        };
        resolved.push(sk.clone());
        let id = sk.label();
        if stages.contains_key(&sk) {
            continue;
        }
        match load_stage(conn, g, &sk, false).await? {
            None => findings.push(Finding::error(
                "stage_not_found",
                &id,
                format!("stage {id} does not exist"),
            )),
            Some(s) if s.deleted => findings.push(Finding::error(
                "stage_deleted",
                &id,
                format!("stage {id} is deleted"),
            )),
            Some(s) => {
                stages.insert(sk, s);
            }
        }
    }
    if has_error(&findings) {
        return Err(ApplyError::Findings(findings));
    }
    let numbers = stage_numbers(&links);
    let rows = flatten_stages(resolved.iter().zip(&numbers).map(|(sk, n)| {
        let s = &stages[sk];
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
    let numbered: Vec<(StageKey, i32)> = resolved.iter().cloned().zip(numbers).collect();
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
        "SELECT DISTINCT s.stage_id, s.direction, s.name FROM gtfs_stage_stop ss \
         JOIN gtfs_stage s ON s.gtfs_id = ss.gtfs_id AND s.stage_id = ss.stage_id \
                          AND s.direction = ss.direction AND NOT s.deleted \
         WHERE ss.gtfs_id = $1 AND ss.stop_id = $2 ORDER BY s.name, s.stage_id, s.direction",
    )
    .bind(g)
    .bind(stop_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| {
        Ok((
            StageKey::new(r.try_get("stage_id")?, Some(r.try_get("direction")?)).entity_key(),
            r.try_get("name")?,
        ))
    })
    .collect()
}

// ---------------------------------------------------------------- reads

/// The rows of many stages in read shape (names and positions), in one query.
async fn stages_read_rows(
    conn: &mut PgConnection,
    g: &str,
    keys: &[StageKey],
) -> Result<HashMap<StageKey, Vec<Value>>, sqlx::Error> {
    let mut out: HashMap<StageKey, Vec<Value>> = HashMap::new();
    for r in sqlx::query(
        "SELECT ss.stage_id, ss.direction, ss.position, ss.stop_id, coalesce(ss.stop_name_override, s.name) AS stop_name, \
                s.lat, s.lon, s.deleted AS stop_deleted, s.unserviceable, s.parent_station, \
                ss.stop_type, ss.marker_id, \
                ss.marker_name, ss.marker_lat, ss.marker_lon, ss.stop_name_override \
         FROM gtfs_stage_stop ss \
         LEFT JOIN gtfs_stop s ON s.gtfs_id = ss.gtfs_id AND s.stop_id = ss.stop_id \
         WHERE ss.gtfs_id = $1 \
           AND (ss.stage_id, ss.direction) IN (SELECT * FROM UNNEST($2::text[], $3::text[])) \
         ORDER BY ss.stage_id, ss.direction, ss.position",
    )
    .bind(g)
    .bind(keys.iter().map(|k| k.stage_id.clone()).collect::<Vec<_>>())
    .bind(keys.iter().map(|k| k.direction.clone()).collect::<Vec<_>>())
    .fetch_all(&mut *conn)
    .await?
    {
        let key = StageKey::new(r.try_get("stage_id")?, Some(r.try_get("direction")?));
        out.entry(key).or_default().push(json!({
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
    "stage_id, name, direction, description, review, provenance::text AS provenance, deleted, \
     row_version, created_at, updated_at, updated_by";

fn stage_json(r: &PgRow) -> Result<Value, sqlx::Error> {
    Ok(json!({
        "stage_id": r.try_get::<String, _>("stage_id")?,
        // A stage is named by its id AND direction, so this is the one string
        // that names it: what a link, a draft's entity_key and the API path use.
        "stage_key": StageKey::new(
            r.try_get("stage_id")?,
            Some(r.try_get::<String, _>("direction")?.as_str()),
        )
        .entity_key(),
        "name": r.try_get::<String, _>("name")?,
        "description": r.try_get::<Option<String>, _>("description")?,
        // '' is stored for a stage that runs the same either way; the API says null
        "direction": r
            .try_get::<String, _>("direction")?
            .as_str()
            .to_owned()
            .into_option_when_not_empty(),
        // why somebody still has to look at this stage, or null (section 19.1)
        "review": r.try_get::<Option<String>, _>("review")?,
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
    /// Only stages still waiting on somebody: `any` for whatever the reason is,
    /// or one reason. `none` is the stages nobody need look at.
    pub review: Option<String>,
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
               AND ss.direction = s.direction \
               AND ss.stop_type NOT IN ('ROUTE CORRECTION', 'JUMP STOP', 'HIDDEN STOP')) AS stop_count, \
            (SELECT count(*) FROM gtfs_stage_stop ss \
               JOIN gtfs_stop st ON st.gtfs_id = ss.gtfs_id AND st.stop_id = ss.stop_id \
              WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id \
                AND ss.direction = s.direction AND st.unserviceable) AS out_of_use, \
            (SELECT count(DISTINCT rs.route_id) FROM gtfs_route_stage rs \
               JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
               WHERE rs.gtfs_id = s.gtfs_id AND rs.stage_id = s.stage_id \
                 AND rs.direction = s.direction) AS route_count, \
            f.stop_id AS first_stop_id, f.stop_name AS first_stop_name, \
            l.stop_id AS last_stop_id, l.stop_name AS last_stop_name \
         FROM gtfs_stage s \
         LEFT JOIN LATERAL (SELECT ss.stop_id, coalesce(ss.stop_name_override, st.name) AS stop_name \
                            FROM gtfs_stage_stop ss JOIN gtfs_stop st ON st.gtfs_id = ss.gtfs_id AND st.stop_id = ss.stop_id \
                            WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id \
                              AND ss.direction = s.direction \
                            ORDER BY ss.position LIMIT 1) f ON true \
         LEFT JOIN LATERAL (SELECT ss.stop_id, coalesce(ss.stop_name_override, st.name) AS stop_name \
                            FROM gtfs_stage_stop ss JOIN gtfs_stop st ON st.gtfs_id = ss.gtfs_id AND st.stop_id = ss.stop_id \
                            WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id \
                              AND ss.direction = s.direction \
                            ORDER BY ss.position DESC LIMIT 1) l ON true \
         WHERE s.gtfs_id = $1 AND NOT s.deleted \
           AND ($2::text IS NULL OR s.stage_id = $2 OR s.name ILIKE $3) \
           AND ($4::text IS NULL OR EXISTS (SELECT 1 FROM gtfs_stage_stop ss \
                WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id \
                  AND ss.direction = s.direction AND ss.stop_id = $4)) \
           AND ($5::text IS NULL OR EXISTS (SELECT 1 FROM gtfs_route_stage rs \
                WHERE rs.gtfs_id = s.gtfs_id AND rs.stage_id = s.stage_id \
                  AND rs.direction = s.direction AND rs.route_id = $5)) \
           AND (NOT $6 OR NOT EXISTS (SELECT 1 FROM gtfs_route_stage rs \
                JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
                WHERE rs.gtfs_id = s.gtfs_id AND rs.stage_id = s.stage_id \
                  AND rs.direction = s.direction)) \
           AND ($7::text IS NULL OR s.direction = $7) \
           AND ($10::text IS NULL \
                OR ($10 = 'any' AND s.review IS NOT NULL) \
                OR ($10 = 'none' AND s.review IS NULL) \
                OR s.review = $10) \
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
    .bind(
        query
            .review
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    )
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
async fn stage_row(
    conn: &mut PgConnection,
    g: &str,
    key: &StageKey,
) -> EditorResult<Option<Value>> {
    let row = sqlx::query(&format!(
        "SELECT {STAGE_COLS} FROM gtfs_stage \
         WHERE gtfs_id = $1 AND stage_id = $2 AND direction = $3"
    ))
    .bind(g)
    .bind(&key.stage_id)
    .bind(&key.direction)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(stage_json).transpose()?)
}

/// `GET /feeds/{g}/stages/{id}`: the stage, its stops in order, and every live
/// route that uses it (a route may use a stage more than once).
pub async fn stage_detail(conn: &mut PgConnection, g: &str, key: &StageKey) -> EditorResult<Value> {
    let mut stage = stage_row(conn, g, key).await?.ok_or_else(|| {
        EditorError::not_found("stage_not_found", format!("no stage {}", key.label()))
    })?;
    let rows = stages_read_rows(conn, g, std::slice::from_ref(key))
        .await?
        .remove(key)
        .unwrap_or_default();
    let routes = sqlx::query(
        "SELECT rs.route_id, r.short_name, r.long_name, rs.position, rs.stage_no, rs.variant_id, \
                (rs.variant_id IS NOT DISTINCT FROM r.active_variant_id) AS running \
         FROM gtfs_route_stage rs \
         JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
         WHERE rs.gtfs_id = $1 AND rs.stage_id = $2 AND rs.direction = $3 \
         ORDER BY r.short_name, rs.route_id, rs.variant_id NULLS FIRST, rs.position",
    )
    .bind(g)
    .bind(&key.stage_id)
    .bind(&key.direction)
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
        "SELECT rs.position, rs.stage_id, rs.direction, rs.stage_no, s.name, s.description, \
                s.deleted, s.row_version, \
            (SELECT count(DISTINCT o.route_id) FROM gtfs_route_stage o \
               JOIN gtfs_route r ON r.gtfs_id = o.gtfs_id AND r.route_id = o.route_id AND NOT r.deleted \
               WHERE o.gtfs_id = rs.gtfs_id AND o.stage_id = rs.stage_id \
                 AND o.direction = rs.direction) AS route_count \
         FROM gtfs_route_stage rs \
         JOIN gtfs_stage s ON s.gtfs_id = rs.gtfs_id AND s.stage_id = rs.stage_id \
                          AND s.direction = rs.direction \
         WHERE rs.gtfs_id = $1 AND rs.route_id = $2 \
           AND rs.variant_id IS NOT DISTINCT FROM $3 ORDER BY rs.position",
    )
    .bind(g)
    .bind(route_id)
    .bind(variant)
    .fetch_all(&mut *conn)
    .await?;
    let ids: Vec<StageKey> = links
        .iter()
        .map(|r| {
            Ok(StageKey::new(
                r.try_get("stage_id")?,
                Some(r.try_get("direction")?),
            ))
        })
        .collect::<Result<HashSet<StageKey>, sqlx::Error>>()?
        .into_iter()
        .collect();
    let mut rows = stages_read_rows(conn, g, &ids).await?;
    let stages = links
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            let key = StageKey::new(r.try_get("stage_id")?, Some(r.try_get("direction")?));
            let stage_rows = rows.get(&key).cloned().unwrap_or_default();
            Ok(json!({
                "position": r.try_get::<i32, _>("position")?,
                "stage_id": key.stage_id,
                "direction": key.direction_opt(),
                "stage_key": key.entity_key(),
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
    key: &StageKey,
) -> EditorResult<Option<(Value, i32)>> {
    if stage_row(conn, g, key).await?.is_none() {
        return Ok(None);
    }
    let detail = stage_detail(conn, g, key).await?;
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
                          AND s.direction = rs.direction \
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
    // resolved with the draft applied, so a stage the draft creates is found
    let raw = stage_id.to_string();
    with_draft_applied(state, ctx, set_id, |conn, g| {
        let raw = raw.clone();
        Box::pin(async move {
            let key = key_of_change(conn, g, &raw).await;
            stage_detail(conn, g, &key).await
        })
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
                direction: None,
                stage_no: None,
            },
            StageLink {
                stage_id: "b".into(),
                direction: None,
                stage_no: None,
            },
        ];
        let explicit = [
            StageLink {
                stage_id: "a".into(),
                direction: None,
                stage_no: Some(1),
            },
            StageLink {
                stage_id: "b".into(),
                direction: None,
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
