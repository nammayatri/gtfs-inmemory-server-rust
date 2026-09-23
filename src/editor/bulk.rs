//! Bulk import (docs/gtfs-editor.md sections 5 and 11): uploaded rows of stops,
//! routes or route stop lists become create / replace changes in a draft, and
//! rows of stop details (platform label, description, name) become updates. A
//! dry run previews every row; a real run appends all the changes in one
//! transaction, and only when no row has an error.
//!
//! Validation is batched. A few queries fetch every id the upload references
//! and the routes' rows; the draft's own changes are read once
//! ([`DraftView`]); then each row is checked in memory with the rules a single
//! change runs: [`check_payload`] for its shape, ids unused or existing, and
//! for a route's stop list [`check_route_rows`] graded against the route's
//! rows once the draft applies.

use super::auth::{self, Ctx};
use super::draft::{DraftView, StopTexts};
use super::error::{EditorError, EditorResult};
use super::feed_lock::{lock_feed_of_set, retry_transient};
use super::service::{self, rows_hash, ChangeInsert};
use super::validation::{
    check_payload, check_route_rows, check_route_rows_labelled, grade_against_live, Finding, Level,
    RouteRow,
};
use super::EditorState;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, Row};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub const MAX_ROWS: usize = 5000;

/// Stand-in id for checking the shape of a stop row whose id the server mints.
const PROVISIONAL_STOP_ID: &str = "ed_0000000000";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BulkRequest {
    pub kind: String,
    pub rows: Vec<Value>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Stops,
    Routes,
    RouteStops,
    StopUpdates,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "stops" => Some(Kind::Stops),
            "routes" => Some(Kind::Routes),
            "route_stops" => Some(Kind::RouteStops),
            "stop_updates" => Some(Kind::StopUpdates),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Stops => "stops",
            Kind::Routes => "routes",
            Kind::RouteStops => "route_stops",
            Kind::StopUpdates => "stop_updates",
        }
    }

    pub fn columns(self) -> &'static [&'static str] {
        match self {
            Kind::Stops => &["action", "stop_id", "name", "lat", "lon", "platform_code"],
            Kind::Routes => &["action", "route_id", "short_name", "long_name", "color"],
            Kind::RouteStops => &[
                "action",
                "route_id",
                "sequence",
                "stop_id",
                "stop_type",
                "stage_no",
                "stage_name",
            ],
            Kind::StopUpdates => &["action", "stop_id", "platform_code", "description", "name"],
        }
    }

    /// What `action` may say for this kind, and why not, for the rest.
    fn refuses(self, action: Action) -> Option<String> {
        match (self, action) {
            (Kind::StopUpdates, Action::Add) => Some(
                "the stop details file only updates stops; add one with the stops file, which carries its position"
                    .into(),
            ),
            (Kind::StopUpdates, Action::Delete) => Some(
                "the stop details file only updates stops; delete one with the stops file".into(),
            ),
            (Kind::RouteStops, Action::Delete) => Some(
                "a route's stop list is uploaded whole: upload it with action update, leaving out the stops it should not have"
                    .into(),
            ),
            _ => None,
        }
    }
}

/// What a row asks for. Every row says which, in its `action` column.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Action {
    Add,
    Update,
    Delete,
}

impl Action {
    fn parse(s: &str) -> Option<Action> {
        match s.trim().to_ascii_lowercase().as_str() {
            "add" => Some(Action::Add),
            "update" => Some(Action::Update),
            "delete" => Some(Action::Delete),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Action::Add => "add",
            Action::Update => "update",
            Action::Delete => "delete",
        }
    }
}

/// The row's `action`, or why it cannot be read. Blank is an error, never a
/// default: an upload says what it does to every row.
fn cell_action(m: &Map<String, Value>, kind: Kind) -> Result<Action, String> {
    let given = match m.get("action") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err("action must be text: add, update or delete".into()),
    };
    let Some(given) = given else {
        return Err("action is required: add, update or delete".into());
    };
    let Some(action) = Action::parse(&given) else {
        return Err(format!(
            "action {given:?} is not one of add, update or delete"
        ));
    };
    match kind.refuses(action) {
        Some(why) => Err(format!("action {}: {why}", action.name())),
        None => Ok(action),
    }
}

struct Planned {
    entity: &'static str,
    op: &'static str,
    /// `None`: a stop whose id the apply mints.
    key: Option<String>,
    after: Value,
    before: Value,
    /// The live row's version an update is based on; a create has none.
    base: Option<i32>,
}

#[derive(Default)]
struct Outcome {
    findings: Vec<Finding>,
    /// Index into `Plan::changes` of the change this row is part of.
    change: Option<usize>,
    /// `stop_updates`: the stop the row names as it is now (the draft applied),
    /// so the preview can show what changes, and where, without a read per row.
    stop: Option<Value>,
}

struct Plan {
    rows: Vec<Outcome>,
    changes: Vec<Planned>,
}

impl Plan {
    fn new(n: usize) -> Plan {
        Plan {
            rows: (0..n).map(|_| Outcome::default()).collect(),
            changes: Vec::new(),
        }
    }

    fn has_errors(&self) -> bool {
        self.rows
            .iter()
            .any(|r| r.findings.iter().any(|f| f.level == Level::Error))
    }
}

// ---------------------------------------------------------------- cells

fn invalid_row(message: impl Into<String>) -> Finding {
    Finding::error("invalid_row", "", message)
}

/// The row as an object with only its kind's columns.
pub fn row_object(v: &Value, kind: Kind) -> Result<&Map<String, Value>, Finding> {
    let m = v
        .as_object()
        .ok_or_else(|| invalid_row("a row is an object of column: value"))?;
    if let Some(k) = m.keys().find(|k| !kind.columns().contains(&k.as_str())) {
        return Err(invalid_row(format!(
            "{k:?} is not a column of a {} row (columns: {})",
            kind.name(),
            kind.columns().join(", ")
        )));
    }
    Ok(m)
}

/// Text cell: absent, null and blank are all "not given". A number is taken as
/// its text (a spreadsheet may type an id as one).
pub fn cell_text(m: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => Err(format!("{key} must be text")),
    }
}

/// Number cell: a JSON number or a numeric string (CSV cells are text).
pub fn cell_f64(m: &Map<String, Value>, key: &str) -> Result<Option<f64>, String> {
    let bad = || format!("{key} must be a number");
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_f64().map(Some).ok_or_else(bad),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite())
            .map(Some)
            .ok_or_else(bad),
        Some(_) => Err(bad()),
    }
}

/// Whole-number cell (`12`, `"12"`, `12.0`), within i32.
pub fn cell_i32(m: &Map<String, Value>, key: &str) -> Result<Option<i32>, String> {
    let bad = || format!("{key} must be a whole number");
    let whole = |x: f64| {
        (x.is_finite() && x.fract() == 0.0 && x >= i32::MIN as f64 && x <= i32::MAX as f64)
            .then_some(x as i32)
    };
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_f64().and_then(whole).map(Some).ok_or_else(bad),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(whole)
            .map(Some)
            .ok_or_else(bad),
        Some(_) => Err(bad()),
    }
}

/// Mark every row of each group with more than one member as a duplicate.
fn mark_duplicates<K: std::hash::Hash + Eq>(
    plan: &mut Plan,
    groups: HashMap<K, Vec<usize>>,
    what: impl Fn(&K) -> String,
) {
    for (k, rows) in groups {
        if rows.len() < 2 {
            continue;
        }
        let mut sorted = rows.clone();
        sorted.sort_unstable();
        let listed = sorted
            .iter()
            .map(|i| (i + 1).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        for i in sorted {
            plan.rows[i].findings.push(Finding::error(
                "duplicate_in_upload",
                "",
                format!("{} is on rows {listed} of this upload", what(&k)),
            ));
        }
    }
}

// ---------------------------------------------------------------- stops

async fn plan_stops(
    conn: &mut PgConnection,
    g: &str,
    rows: &[Value],
    draft: &DraftView,
) -> EditorResult<Plan> {
    let mut plan = Plan::new(rows.len());
    let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_place: HashMap<(String, i64, i64), Vec<usize>> = HashMap::new();
    // rows that create, and rows that change a stop that is already there
    let mut adds: Vec<(usize, String)> = Vec::new();
    let mut touches: Vec<(usize, String, Action, Map<String, Value>)> = Vec::new();
    for (i, v) in rows.iter().enumerate() {
        let m = match row_object(v, Kind::Stops) {
            Ok(m) => m,
            Err(f) => {
                plan.rows[i].findings.push(f);
                continue;
            }
        };
        let action = match cell_action(m, Kind::Stops) {
            Ok(a) => a,
            Err(e) => {
                plan.rows[i].findings.push(invalid_row(e));
                continue;
            }
        };
        let (id, name, lat, lon, platform) = (
            cell_text(m, "stop_id"),
            cell_text(m, "name"),
            cell_f64(m, "lat"),
            cell_f64(m, "lon"),
            cell_text(m, "platform_code"),
        );
        let bad: Vec<String> = [
            id.as_ref().err(),
            name.as_ref().err(),
            platform.as_ref().err(),
        ]
        .into_iter()
        .chain([lat.as_ref().err(), lon.as_ref().err()])
        .flatten()
        .cloned()
        .collect();
        if !bad.is_empty() {
            plan.rows[i]
                .findings
                .extend(bad.into_iter().map(invalid_row));
            continue;
        }
        let (id, name, lat, lon, platform) = (
            id.unwrap_or_default(),
            name.unwrap_or_default(),
            lat.unwrap_or_default(),
            lon.unwrap_or_default(),
            platform.unwrap_or_default(),
        );
        let mut after = Map::new();
        if let Some(id) = &id {
            after.insert("stop_id".into(), json!(id));
        }
        if let Some(name) = &name {
            after.insert("name".into(), json!(name));
        }
        if let Some(lat) = lat {
            after.insert("lat".into(), json!(lat));
        }
        if let Some(lon) = lon {
            after.insert("lon".into(), json!(lon));
        }
        if let Some(p) = &platform {
            after.insert("platform_code".into(), json!(p));
        }
        if let Some(id) = &id {
            by_id.entry(id.clone()).or_default().push(i);
        }

        if action == Action::Add {
            // exactly the single change's shape check; a stand-in id where the
            // server will mint one
            let key = id
                .clone()
                .unwrap_or_else(|| PROVISIONAL_STOP_ID.to_string());
            let mut probe = after.clone();
            probe.insert("stop_id".into(), json!(key));
            if let Err(f) = check_payload("stop", "create", &key, &Value::Object(probe)) {
                plan.rows[i].findings.push(f);
            }
            if let Some(id) = &id {
                adds.push((i, id.clone()));
            }
            if let (Some(name), Some(lat), Some(lon)) = (&name, lat, lon) {
                let place = (
                    name.to_lowercase(),
                    (lat * 1e6).round() as i64,
                    (lon * 1e6).round() as i64,
                );
                by_place.entry(place).or_default().push(i);
            }
            plan.changes.push(Planned {
                entity: "stop",
                op: "create",
                key: id,
                after: Value::Object(after),
                before: Value::Null,
                base: None,
            });
            plan.rows[i].change = Some(plan.changes.len() - 1);
            continue;
        }

        // update and delete both name a stop that already exists
        let Some(stop_id) = id else {
            plan.rows[i].findings.push(invalid_row(format!(
                "stop_id is required to {} a stop",
                action.name()
            )));
            continue;
        };
        let mut fields = after;
        fields.remove("stop_id");
        if action == Action::Update && fields.is_empty() {
            plan.rows[i].findings.push(Finding::error(
                "nothing_to_update",
                stop_id.as_str(),
                format!("the row for stop {stop_id} gives no name, lat and lon, or platform_code"),
            ));
            continue;
        }
        if action == Action::Delete && !fields.is_empty() {
            // deleting takes the id alone; a filled cell means the row was meant
            // to change something
            plan.rows[i].findings.push(invalid_row(format!(
                "action delete takes stop_id alone; this row also fills in {}",
                fields.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
            continue;
        }
        if action == Action::Update {
            if let Err(f) =
                check_payload("stop", "update", &stop_id, &Value::Object(fields.clone()))
            {
                plan.rows[i].findings.push(f);
                continue;
            }
        }
        touches.push((i, stop_id, action, fields));
    }
    mark_duplicates(&mut plan, by_id, |id| format!("stop_id {id}"));
    mark_duplicates(&mut plan, by_place, |(name, _, _)| {
        format!("a stop named {name:?} at the same position")
    });

    // the live rows behind every id the upload names: an add must not find one,
    // an update or a delete must
    let mut wanted: Vec<String> = adds.iter().map(|(_, id)| id.clone()).collect();
    wanted.extend(touches.iter().map(|(_, id, _, _)| id.clone()));
    wanted.sort_unstable();
    wanted.dedup();
    let mut live: HashMap<String, LiveStopRow> = HashMap::with_capacity(wanted.len());
    if !wanted.is_empty() {
        for r in sqlx::query(
            "SELECT stop_id, lat, lon, location_type, deleted, row_version \
             FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = ANY($2)",
        )
        .bind(g)
        .bind(&wanted)
        .fetch_all(&mut *conn)
        .await?
        {
            live.insert(
                r.try_get("stop_id")?,
                LiveStopRow {
                    lat: r.try_get("lat")?,
                    lon: r.try_get("lon")?,
                    location_type: r.try_get("location_type")?,
                    deleted: r.try_get("deleted")?,
                    row_version: r.try_get("row_version")?,
                },
            );
        }
    }
    for (i, id) in adds {
        if live.get(&id).is_some_and(|s| !s.deleted) {
            plan.rows[i].findings.push(Finding::error(
                "stop_exists",
                id.as_str(),
                format!("stop {id} already exists; action update changes it"),
            ));
        } else if live.contains_key(&id) {
            plan.rows[i].findings.push(Finding::error(
                "stop_exists",
                id.as_str(),
                format!("stop {id} existed and was deleted; its id cannot be used again"),
            ));
        } else if let Some(c) = draft.created_stop(&id) {
            plan.rows[i].findings.push(Finding::error(
                "stop_exists",
                id.as_str(),
                format!(
                    "stop {id} is already created in this draft (change {})",
                    c.change_id
                ),
            ));
        }
    }
    for (i, id, action, fields) in touches {
        if let Some((into, by)) = draft.merged_into(&id) {
            plan.rows[i].findings.push(Finding::error(
                "stop_merged_away",
                id.as_str(),
                format!("stop {id} is merged into {into} by change {by} earlier in this draft; use {into}"),
            ));
            continue;
        }
        if draft.stop_deleted(&id) {
            plan.rows[i].findings.push(Finding::error(
                "stop_deleted",
                id.as_str(),
                format!("stop {id} is deleted earlier in this draft"),
            ));
            continue;
        }
        let in_draft = draft.created_stop(&id);
        let base = match (live.get(&id), in_draft) {
            (Some(s), _) if s.deleted => {
                plan.rows[i].findings.push(Finding::error(
                    "stop_deleted",
                    id.as_str(),
                    format!("stop {id} is deleted"),
                ));
                continue;
            }
            (Some(s), _) if s.location_type == 1 => {
                plan.rows[i].findings.push(Finding::error(
                    "stop_is_station",
                    id.as_str(),
                    format!("{id} is a station; stations are edited in the dashboard, not here"),
                ));
                continue;
            }
            (Some(s), _) => Some(s.row_version),
            // a stop this draft creates: the change it carries has no live version
            (None, Some(_)) if action == Action::Delete => {
                plan.rows[i].findings.push(Finding::error(
                    "stop_not_live",
                    id.as_str(),
                    format!(
                        "stop {id} is created in this draft; remove that change instead of deleting it"
                    ),
                ));
                continue;
            }
            (None, Some(_)) => None,
            (None, None) => {
                plan.rows[i].findings.push(Finding::error(
                    "stop_not_found",
                    id.as_str(),
                    format!("no stop {id}; action add creates one"),
                ));
                continue;
            }
        };
        plan.changes.push(Planned {
            entity: "stop",
            op: if action == Action::Delete {
                "delete"
            } else {
                "update"
            },
            key: Some(id),
            after: if action == Action::Delete {
                Value::Null
            } else {
                Value::Object(fields)
            },
            before: Value::Null,
            base,
        });
        plan.rows[i].change = Some(plan.changes.len() - 1);
    }
    Ok(plan)
}

// ---------------------------------------------------------------- routes

async fn plan_routes(
    conn: &mut PgConnection,
    g: &str,
    rows: &[Value],
    draft: &DraftView,
) -> EditorResult<Plan> {
    let mut plan = Plan::new(rows.len());
    let agency = service::usual_agency(conn, g).await?;
    let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
    let mut adds: Vec<(usize, String)> = Vec::new();
    let mut touches: Vec<(usize, String, Action, Map<String, Value>)> = Vec::new();
    for (i, v) in rows.iter().enumerate() {
        let m = match row_object(v, Kind::Routes) {
            Ok(m) => m,
            Err(f) => {
                plan.rows[i].findings.push(f);
                continue;
            }
        };
        let action = match cell_action(m, Kind::Routes) {
            Ok(a) => a,
            Err(e) => {
                plan.rows[i].findings.push(invalid_row(e));
                continue;
            }
        };
        let cells = ["route_id", "short_name", "long_name", "color"].map(|k| cell_text(m, k));
        let bad: Vec<String> = cells
            .iter()
            .filter_map(|c| c.as_ref().err())
            .cloned()
            .collect();
        if !bad.is_empty() {
            plan.rows[i]
                .findings
                .extend(bad.into_iter().map(invalid_row));
            continue;
        }
        let [id, short, long, color] = cells.map(|c| c.unwrap_or_default());
        if let Some(id) = &id {
            by_id.entry(id.clone()).or_default().push(i);
        }
        let mut fields = Map::new();
        for (k, v) in [
            ("short_name", &short),
            ("long_name", &long),
            ("color", &color),
        ] {
            if let Some(v) = v {
                fields.insert(k.into(), json!(v));
            }
        }

        if action == Action::Add {
            let mut after = fields.clone();
            if let Some(id) = &id {
                after.insert("route_id".into(), json!(id));
            }
            after.insert("route_type".into(), json!(3));
            if let Some(a) = &agency {
                after.insert("agency_id".into(), json!(a));
            }
            let after = Value::Object(after);
            let key = id.clone().unwrap_or_default();
            if let Err(f) = check_payload("route", "create", &key, &after) {
                plan.rows[i].findings.push(f);
            }
            if let Some(id) = &id {
                adds.push((i, id.clone()));
            }
            plan.changes.push(Planned {
                entity: "route",
                op: "create",
                key: Some(key),
                after,
                before: Value::Null,
                base: None,
            });
            plan.rows[i].change = Some(plan.changes.len() - 1);
            continue;
        }

        let Some(route_id) = id else {
            plan.rows[i].findings.push(invalid_row(format!(
                "route_id is required to {} a route",
                action.name()
            )));
            continue;
        };
        if action == Action::Update && fields.is_empty() {
            plan.rows[i].findings.push(Finding::error(
                "nothing_to_update",
                route_id.as_str(),
                format!("the row for route {route_id} gives no short_name, long_name or color"),
            ));
            continue;
        }
        if action == Action::Delete && !fields.is_empty() {
            plan.rows[i].findings.push(invalid_row(format!(
                "action delete takes route_id alone; this row also fills in {}",
                fields.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
            continue;
        }
        if action == Action::Update {
            if let Err(f) =
                check_payload("route", "update", &route_id, &Value::Object(fields.clone()))
            {
                plan.rows[i].findings.push(f);
                continue;
            }
        }
        touches.push((i, route_id, action, fields));
    }

    let mut wanted: Vec<String> = by_id.keys().cloned().collect();
    wanted.sort_unstable();
    let mut live: HashMap<String, (bool, i32)> = HashMap::with_capacity(wanted.len());
    if !wanted.is_empty() {
        for r in sqlx::query(
            "SELECT route_id, deleted, row_version FROM gtfs_route \
             WHERE gtfs_id = $1 AND route_id = ANY($2)",
        )
        .bind(g)
        .bind(&wanted)
        .fetch_all(&mut *conn)
        .await?
        {
            live.insert(
                r.try_get("route_id")?,
                (r.try_get("deleted")?, r.try_get("row_version")?),
            );
        }
    }
    for (i, id) in adds {
        let found = if live.contains_key(&id) {
            Some(format!(
                "route {id} already exists; action update changes it"
            ))
        } else {
            draft
                .created_route(&id)
                .map(|cid| format!("route {id} is already created in this draft (change {cid})"))
        };
        if let Some(message) = found {
            plan.rows[i]
                .findings
                .push(Finding::error("route_exists", id.as_str(), message));
        }
    }
    for (i, id, action, fields) in touches {
        let base = match (live.get(&id), draft.created_route(&id)) {
            (Some((true, _)), _) => {
                plan.rows[i].findings.push(Finding::error(
                    "route_deleted",
                    id.as_str(),
                    format!("route {id} is deleted"),
                ));
                continue;
            }
            (Some((_, version)), _) => Some(*version),
            (None, Some(_)) if action == Action::Delete => {
                plan.rows[i].findings.push(Finding::error(
                    "route_not_live",
                    id.as_str(),
                    format!(
                        "route {id} is created in this draft; remove that change instead of deleting it"
                    ),
                ));
                continue;
            }
            (None, Some(_)) => None,
            (None, None) => {
                plan.rows[i].findings.push(Finding::error(
                    "route_not_found",
                    id.as_str(),
                    format!("no route {id}; action add creates one"),
                ));
                continue;
            }
        };
        plan.changes.push(Planned {
            entity: "route",
            op: if action == Action::Delete {
                "delete"
            } else {
                "update"
            },
            key: Some(id),
            after: if action == Action::Delete {
                Value::Null
            } else {
                Value::Object(fields)
            },
            before: Value::Null,
            base,
        });
        plan.rows[i].change = Some(plan.changes.len() - 1);
    }
    mark_duplicates(&mut plan, by_id, |id| format!("route_id {id}"));
    Ok(plan)
}

// ---------------------------------------------------------------- route stop lists

struct StopRow {
    upload: usize,
    sequence: i32,
    row: RouteRow,
}

async fn plan_route_stops(
    conn: &mut PgConnection,
    g: &str,
    rows: &[Value],
    draft: &DraftView,
    with_before: bool,
) -> EditorResult<Plan> {
    let mut plan = Plan::new(rows.len());
    let mut routes: Vec<(String, Vec<StopRow>)> = Vec::new();
    let mut route_at: HashMap<String, usize> = HashMap::new();
    let mut by_seq: HashMap<(String, i32), Vec<usize>> = HashMap::new();
    // what each route's rows say to do, and where they said it
    let mut route_action: HashMap<String, Vec<(usize, Action)>> = HashMap::new();
    for (i, v) in rows.iter().enumerate() {
        let m = match row_object(v, Kind::RouteStops) {
            Ok(m) => m,
            Err(f) => {
                plan.rows[i].findings.push(f);
                continue;
            }
        };
        let action = match cell_action(m, Kind::RouteStops) {
            Ok(a) => a,
            Err(e) => {
                plan.rows[i].findings.push(invalid_row(e));
                continue;
            }
        };
        let mut bad = Vec::new();
        let mut text = |k: &str| match cell_text(m, k) {
            Ok(Some(v)) => Some(v),
            Ok(None) => {
                bad.push(format!("{k} is required"));
                None
            }
            Err(e) => {
                bad.push(e);
                None
            }
        };
        let (route_id, stop_id, stop_type) = (text("route_id"), text("stop_id"), text("stop_type"));
        // an empty stage name is the fare check's to report, like a single change
        let stage_name = match m.get("stage_name") {
            None | Some(Value::Null) => {
                bad.push("stage_name is required".into());
                None
            }
            Some(Value::String(s)) => Some(s.trim().to_string()),
            Some(_) => {
                bad.push("stage_name must be text".into());
                None
            }
        };
        let mut whole = |k: &str| match cell_i32(m, k) {
            Ok(Some(v)) => Some(v),
            Ok(None) => {
                bad.push(format!("{k} is required"));
                None
            }
            Err(e) => {
                bad.push(e);
                None
            }
        };
        let (sequence, stage_no) = (whole("sequence"), whole("stage_no"));
        if sequence.is_some_and(|s| s < 1) {
            bad.push("sequence must be 1 or more".into());
        }
        let (
            Some(route_id),
            Some(stop_id),
            Some(stop_type),
            Some(stage_name),
            Some(sequence),
            Some(stage_no),
        ) = (route_id, stop_id, stop_type, stage_name, sequence, stage_no)
        else {
            plan.rows[i]
                .findings
                .extend(bad.into_iter().map(invalid_row));
            continue;
        };
        if !bad.is_empty() {
            plan.rows[i]
                .findings
                .extend(bad.into_iter().map(invalid_row));
            continue;
        }
        by_seq
            .entry((route_id.clone(), sequence))
            .or_default()
            .push(i);
        route_action
            .entry(route_id.clone())
            .or_default()
            .push((i, action));
        let at = *route_at.entry(route_id.clone()).or_insert_with(|| {
            routes.push((route_id.clone(), Vec::new()));
            routes.len() - 1
        });
        routes[at].1.push(StopRow {
            upload: i,
            sequence,
            row: RouteRow {
                stop_id: Some(stop_id),
                stop_type,
                stage_no,
                stage_name,
                marker_id: None,
                marker_name: None,
                marker_lat: None,
                marker_lon: None,
                stop_name_override: None,
                provider_id: None,
            },
        });
    }
    mark_duplicates(&mut plan, by_seq, |(route, seq)| {
        format!("sequence {seq} of route {route}")
    });
    for (_, list) in routes.iter_mut() {
        list.sort_by_key(|r| (r.sequence, r.upload));
    }

    // everything the upload references, in a few queries
    let route_ids: Vec<String> = routes.iter().map(|(id, _)| id.clone()).collect();
    let stop_ids: Vec<String> = routes
        .iter()
        .flat_map(|(_, list)| list.iter().filter_map(|r| r.row.stop_id.clone()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let live_routes: HashMap<String, bool> = sqlx::query(
        "SELECT route_id, deleted FROM gtfs_route WHERE gtfs_id = $1 AND route_id = ANY($2)",
    )
    .bind(g)
    .bind(&route_ids)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(String, bool), sqlx::Error> {
        Ok((r.try_get("route_id")?, r.try_get("deleted")?))
    })
    .collect::<Result<_, _>>()?;
    let live_stops: HashMap<String, (i16, bool)> = sqlx::query(
        "SELECT stop_id, location_type, deleted FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = ANY($2)",
    )
    .bind(g)
    .bind(&stop_ids)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(String, (i16, bool)), sqlx::Error> {
        Ok((
            r.try_get("stop_id")?,
            (r.try_get("location_type")?, r.try_get("deleted")?),
        ))
    })
    .collect::<Result<_, _>>()?;
    let live_rows = service::load_routes_rows(conn, g, &route_ids).await?;
    let mut read_rows = if with_before {
        service::load_routes_read_rows(conn, g, &route_ids).await?
    } else {
        HashMap::new()
    };

    for (route_id, list) in routes {
        let uploads: Vec<usize> = list.iter().map(|r| r.upload).collect();
        let rows: Vec<RouteRow> = list.iter().map(|r| r.row.clone()).collect();
        let live = live_rows.get(&route_id).cloned().unwrap_or_default();
        let current = draft.current_rows(&route_id, &live);
        let mut findings: Vec<(Option<usize>, Finding)> = Vec::new();

        // a route's stop list is uploaded whole, so its rows agree on the action
        let said = route_action.get(&route_id).cloned().unwrap_or_default();
        let action = said.first().map(|(_, a)| *a).unwrap_or(Action::Update);
        let mixed: Vec<usize> = said
            .iter()
            .filter(|(_, a)| *a != action)
            .map(|(i, _)| *i)
            .collect();
        let has_rows = !current.is_empty();
        let route_state = match live_routes.get(&route_id) {
            Some(true) => Some(("route_deleted", format!("route {route_id} is deleted"))),
            _ if draft.route_deleted(&route_id) => Some((
                "route_deleted",
                format!("route {route_id} is deleted in this draft"),
            )),
            None if draft.created_route(&route_id).is_none() => {
                Some(("route_not_found", format!("no route {route_id}")))
            }
            _ if !mixed.is_empty() => Some((
                "mixed_action",
                format!(
                    "route {route_id} has rows saying {} and rows saying something else; a route's whole stop list is one action",
                    action.name()
                ),
            )),
            _ if action == Action::Add && has_rows => Some((
                "route_stops_exist",
                format!(
                    "route {route_id} already has a stop list of {} stops; action update replaces it",
                    current.len()
                ),
            )),
            _ if action == Action::Update && !has_rows => Some((
                "route_stops_missing",
                format!("route {route_id} has no stop list yet; action add gives it one"),
            )),
            _ => None,
        };
        if let Some((code, message)) = route_state {
            findings.push((None, Finding::error(code, route_id.as_str(), message)));
        } else {
            // the stops: once the draft applies, each exists, is live and is not
            // a station; a stop merged away in the draft is gone for good
            let mut merged = false;
            let mut ids: Vec<&str> = list
                .iter()
                .filter(|r| !r.row.is_marker())
                .filter_map(|r| r.row.stop_id.as_deref())
                .collect();
            ids.sort_unstable();
            ids.dedup();
            let mut stop_findings = Vec::new();
            for id in ids {
                let at: Vec<usize> = list
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.row.stop_id.as_deref() == Some(id))
                    .map(|(k, _)| k)
                    .collect();
                let problem = if let Some((into, by)) = draft.merged_into(id) {
                    merged = true;
                    Some((
                        "stop_merged_away",
                        format!("stop {id} is merged into {into} by change {by} earlier in this draft; use {into}"),
                    ))
                } else {
                    let created = draft.created_stop(id).map(|c| (c.location_type, false));
                    match live_stops.get(id).copied().or(created) {
                        None => Some(("unknown_stop", format!("stop {id} does not exist"))),
                        Some((_, deleted)) if deleted || draft.stop_deleted(id) => {
                            Some(("stop_deleted", format!("stop {id} is deleted")))
                        }
                        Some((1, _)) => Some((
                            "stop_is_station",
                            format!("{id} is a station; a route calls at one of its platforms"),
                        )),
                        _ => None,
                    }
                };
                if let Some((code, message)) = problem {
                    for k in at {
                        stop_findings.push((Some(k), Finding::error(code, id, message.clone())));
                    }
                }
            }
            if merged {
                // like a single change, a stop list using a merged-away stop is
                // refused before anything else is checked
                findings.extend(
                    stop_findings
                        .into_iter()
                        .filter(|(_, f)| f.code == "stop_merged_away"),
                );
            } else {
                findings.extend(stop_findings);
                let label = |k: usize| format!("sequence {}", list[k].sequence);
                findings.extend(
                    grade_against_live(
                        check_route_rows_labelled(&rows, &label),
                        &check_route_rows(&current),
                    )
                    .into_iter()
                    .map(|f| (f.row, f)),
                );
            }
        }
        let markers = current.iter().filter(|r| r.is_marker()).count();
        if markers > 0 {
            findings.push((
                Some(0),
                Finding::warning(
                    "markers_dropped",
                    route_id.as_str(),
                    format!(
                        "route {route_id} has {markers} ROUTE CORRECTION row(s); this list replaces them"
                    ),
                ),
            ));
        }
        if let Some(cid) = draft.replaced_by(&route_id) {
            findings.push((
                Some(0),
                Finding::warning(
                    "route_already_in_draft",
                    route_id.as_str(),
                    format!("change {cid} in this draft already replaces route {route_id}'s stop list; this one replaces it again"),
                ),
            ));
        }

        // the route's own spelling of a stop it already calls at is kept
        let mut spelling: HashMap<&str, &str> = HashMap::new();
        for r in current.iter().filter(|r| !r.is_marker()) {
            if let (Some(id), Some(o)) = (r.stop_id.as_deref(), r.stop_name_override.as_deref()) {
                spelling.entry(id).or_insert(o);
            }
        }
        let after_rows: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut o = json!({
                    "stop_id": r.stop_id, "stop_type": r.stop_type,
                    "stage_no": r.stage_no, "stage_name": r.stage_name,
                });
                if let Some(s) = r.stop_id.as_deref().and_then(|id| spelling.get(id)) {
                    o["stop_name_override"] = json!(s);
                }
                o
            })
            .collect();
        let after = json!({"base_rows_hash": rows_hash(&live), "rows": after_rows});
        if let Err(f) = check_payload("route_stops", "replace", &route_id, &after) {
            findings.push((None, f));
        }
        plan.changes.push(Planned {
            entity: "route_stops",
            op: "replace",
            key: Some(route_id.clone()),
            before: json!(read_rows.remove(&route_id).unwrap_or_default()),
            after,
            base: None,
        });
        let change = plan.changes.len() - 1;
        for i in &uploads {
            plan.rows[*i].change = Some(change);
        }
        for (at, f) in findings {
            match at {
                Some(k) => plan.rows[uploads[k]].findings.push(f),
                None => {
                    for i in &uploads {
                        plan.rows[*i].findings.push(f.clone());
                    }
                }
            }
        }
    }
    Ok(plan)
}

// ---------------------------------------------------------------- stop details

/// One readable row of a `stop_updates` upload.
struct StopUpdate {
    upload: usize,
    stop_id: String,
    /// `(column, value)` for each cell that is given, in column order.
    given: Vec<(&'static str, String)>,
}

/// A stop as the upload needs it: what is live, before the draft.
struct LiveStopRow {
    lat: f64,
    lon: f64,
    location_type: i16,
    deleted: bool,
    row_version: i32,
}

/// `{stop_id, platform_code?, description?, name?}` rows: one `stop/update`
/// (a station: `station/update`) per row, with exactly the cells given. The
/// whole upload is checked against one read of its stops and one replay of the
/// draft ([`DraftView::texts_after`]) - never the draft once per row.
async fn plan_stop_updates(
    conn: &mut PgConnection,
    g: &str,
    change_set_id: Uuid,
    rows: &[Value],
    draft: &DraftView,
    with_before: bool,
) -> EditorResult<Plan> {
    let mut plan = Plan::new(rows.len());
    let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
    let mut updates: Vec<StopUpdate> = Vec::new();
    for (i, v) in rows.iter().enumerate() {
        let m = match row_object(v, Kind::StopUpdates) {
            Ok(m) => m,
            Err(f) => {
                plan.rows[i].findings.push(f);
                continue;
            }
        };
        if let Err(e) = cell_action(m, Kind::StopUpdates) {
            plan.rows[i].findings.push(invalid_row(e));
            continue;
        }
        let cells = ["stop_id", "platform_code", "description", "name"].map(|k| cell_text(m, k));
        let bad: Vec<String> = cells
            .iter()
            .filter_map(|c| c.as_ref().err())
            .cloned()
            .collect();
        if !bad.is_empty() {
            plan.rows[i]
                .findings
                .extend(bad.into_iter().map(invalid_row));
            continue;
        }
        let [id, platform, description, name] = cells.map(|c| c.unwrap_or_default());
        let Some(stop_id) = id else {
            plan.rows[i]
                .findings
                .push(invalid_row("stop_id is required"));
            continue;
        };
        by_id.entry(stop_id.clone()).or_default().push(i);
        let given: Vec<(&'static str, String)> = [
            ("platform_code", platform),
            ("description", description),
            ("name", name),
        ]
        .into_iter()
        .filter_map(|(k, v)| Some((k, v?)))
        .collect();
        if given.is_empty() {
            plan.rows[i].findings.push(Finding::error(
                "nothing_to_update",
                stop_id.as_str(),
                format!("the row for stop {stop_id} gives no platform_code, description or name"),
            ));
            continue;
        }
        updates.push(StopUpdate {
            upload: i,
            stop_id,
            given,
        });
    }
    mark_duplicates(&mut plan, by_id, |id| format!("stop_id {id}"));

    // every stop the upload names, and the stops a merge in the draft takes a
    // name or a label from, in one query
    let mut wanted: Vec<String> = updates.iter().map(|u| u.stop_id.clone()).collect();
    wanted.extend(draft.merge_sources());
    wanted.sort_unstable();
    wanted.dedup();
    let mut live: HashMap<String, LiveStopRow> = HashMap::with_capacity(wanted.len());
    let mut live_texts: HashMap<String, StopTexts> = HashMap::with_capacity(wanted.len());
    if !wanted.is_empty() {
        for r in sqlx::query(
            "SELECT stop_id, name, lat, lon, platform_code, description, parent_station, \
                    location_type, deleted, row_version \
             FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = ANY($2)",
        )
        .bind(g)
        .bind(&wanted)
        .fetch_all(&mut *conn)
        .await?
        {
            let id: String = r.try_get("stop_id")?;
            live_texts.insert(
                id.clone(),
                StopTexts {
                    name: r.try_get("name")?,
                    platform_code: r.try_get("platform_code")?,
                    description: r.try_get("description")?,
                    parent_station: r.try_get("parent_station")?,
                },
            );
            live.insert(
                id,
                LiveStopRow {
                    lat: r.try_get("lat")?,
                    lon: r.try_get("lon")?,
                    location_type: r.try_get("location_type")?,
                    deleted: r.try_get("deleted")?,
                    row_version: r.try_get("row_version")?,
                },
            );
        }
    }
    let texts = draft.texts_after(&live_texts);

    let mut stations: Vec<String> = Vec::new();
    let mut created: Vec<String> = Vec::new();
    for u in &updates {
        let (i, id) = (u.upload, u.stop_id.as_str());
        if let Some((into, by)) = draft.merged_into(id) {
            plan.rows[i].findings.push(Finding::error(
                "stop_merged_away",
                id,
                format!("stop {id} is merged into {into} by change {by} earlier in this draft; use {into}"),
            ));
            continue;
        }
        let in_draft = draft.created_stop(id);
        let place = draft
            .stop_position(id)
            .or_else(|| live.get(id).map(|s| (s.lat, s.lon)));
        if let (Some(t), Some((lat, lon))) = (texts.get(id), place) {
            plan.rows[i].stop = Some(json!({
                "name": t.name, "lat": lat, "lon": lon, "platform_code": t.platform_code,
                "description": t.description, "parent_station": t.parent_station,
            }));
        }
        let (location_type, base) = match (live.get(id), in_draft) {
            (Some(s), _) if s.deleted => {
                plan.rows[i].findings.push(Finding::error(
                    "stop_deleted",
                    id,
                    format!("stop {id} is deleted"),
                ));
                continue;
            }
            (Some(s), _) => (s.location_type, Some(s.row_version)),
            (None, Some(c)) => (c.location_type, None),
            (None, None) => {
                plan.rows[i].findings.push(Finding::error(
                    "stop_not_found",
                    id,
                    format!("no stop {id}"),
                ));
                continue;
            }
        };
        if draft.stop_deleted(id) {
            plan.rows[i].findings.push(Finding::error(
                "stop_deleted",
                id,
                format!("stop {id} is deleted in this draft"),
            ));
            continue;
        }
        let is_station = location_type == 1;
        if is_station && u.given.iter().any(|(k, _)| *k == "platform_code") {
            plan.rows[i].findings.push(Finding::error(
                "platform_code_on_station",
                id,
                format!("{id} is a station; a platform label belongs to one of its stops"),
            ));
            continue;
        }
        let entity = if is_station { "station" } else { "stop" };
        let after: Map<String, Value> = u
            .given
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect();
        let after = Value::Object(after);
        // exactly the single change's shape check (the lengths)
        if let Err(f) = check_payload(entity, "update", id, &after) {
            plan.rows[i].findings.push(f);
        }
        if !plan.rows[i].findings.is_empty() {
            // a duplicate or a bad length: the row is an error whatever it says
            continue;
        }
        let now = texts.get(id);
        let same = u.given.iter().all(|(k, v)| {
            let current = now.and_then(|t| match *k {
                "platform_code" => t.platform_code.as_deref(),
                "description" => t.description.as_deref(),
                _ => Some(t.name.as_str()),
            });
            current.map(str::trim) == Some(v.as_str())
        });
        if same {
            plan.rows[i].findings.push(Finding::warning(
                "unchanged",
                id,
                format!(
                    "{entity} {id} already has {}; this row changes nothing",
                    if u.given.len() == 1 {
                        "this value"
                    } else {
                        "these values"
                    }
                ),
            ));
            continue;
        }
        if let Some(cid) = draft.updated_by(id) {
            plan.rows[i].findings.push(Finding::warning(
                "stop_already_in_draft",
                id,
                format!("change {cid} in this draft already updates {entity} {id}; this one applies after it"),
            ));
        }
        if is_station && base.is_some() {
            stations.push(id.to_string());
        }
        if base.is_none() {
            created.push(id.to_string());
        }
        plan.changes.push(Planned {
            entity,
            op: "update",
            key: Some(id.to_string()),
            after,
            before: Value::Null,
            base,
        });
        plan.rows[i].change = Some(plan.changes.len() - 1);
    }

    if with_before {
        // `before` as the single change snapshots it: the row in read shape (a
        // station with its members), or the create's `after` for a row of the draft
        let ids: Vec<String> = plan.changes.iter().filter_map(|c| c.key.clone()).collect();
        let mut read = service::stop_rows(conn, g, &ids).await?;
        let mut members = service::stations_members(conn, g, &stations).await?;
        let mut drafted = service::stops_created_in_set(conn, change_set_id, &created).await?;
        for c in plan.changes.iter_mut() {
            let id = c.key.as_deref().unwrap_or_default();
            c.before = match read.remove(id) {
                Some(mut row) => {
                    if c.entity == "station" {
                        service::with_members(&mut row, &members.remove(id).unwrap_or_default());
                    }
                    row
                }
                None => drafted
                    .remove(id)
                    .map(|(_, after)| after)
                    .unwrap_or(Value::Null),
            };
        }
    }
    Ok(plan)
}

// ---------------------------------------------------------------- run

fn respond(dry_run: bool, kind: Kind, plan: &Plan, change_ids: &[i64]) -> Value {
    let (mut ok, mut warnings, mut errors, mut unchanged) = (0, 0, 0, 0);
    let rows: Vec<Value> = plan
        .rows
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let status = if o.findings.iter().any(|f| f.level == Level::Error) {
                errors += 1;
                "error"
            } else if o.findings.is_empty() {
                ok += 1;
                "ok"
            } else {
                warnings += 1;
                if o.findings.iter().any(|f| f.code == "unchanged") {
                    unchanged += 1;
                }
                "warning"
            };
            let change = o.change.map(|k| {
                let c = &plan.changes[k];
                let mut v = json!({"entity": c.entity, "op": c.op, "entity_key": c.key});
                if let Some(id) = change_ids.get(k) {
                    v["change_id"] = json!(id);
                }
                v
            });
            let mut row = json!({
                "row": i + 1,
                "status": status,
                "messages": o.findings.iter().map(|f| json!({"level": f.level, "code": f.code, "message": f.message})).collect::<Vec<_>>(),
                "change": change,
            });
            if let Some(stop) = &o.stop {
                row["stop"] = stop.clone();
            }
            row
        })
        .collect();
    let mut out = json!({
        "dry_run": dry_run,
        "kind": kind.name(),
        "summary": {"rows": plan.rows.len(), "ok": ok, "warnings": warnings, "errors": errors, "changes": plan.changes.len()},
        "rows": rows,
        "changes_preview": plan.changes.iter().map(|c| json!({
            "entity": c.entity, "op": c.op, "entity_key": c.key, "after": c.after,
        })).collect::<Vec<_>>(),
    });
    // only an upload of updates can find a row already true
    if kind == Kind::StopUpdates {
        out["summary"]["unchanged"] = json!(unchanged);
    }
    out
}

pub async fn run(
    state: &EditorState,
    ctx: &Ctx,
    change_set_id: Uuid,
    req: BulkRequest,
) -> EditorResult<Value> {
    let kind = Kind::parse(&req.kind).ok_or_else(|| {
        EditorError::bad_request(
            "invalid_kind",
            "kind is stops, routes, route_stops or stop_updates",
        )
    })?;
    if req.rows.is_empty() {
        return Err(EditorError::bad_request(
            "rows_required",
            "the upload has no rows",
        ));
    }
    if req.rows.len() > MAX_ROWS {
        return Err(EditorError::bad_request(
            "too_many_rows",
            format!(
                "an upload has at most {MAX_ROWS} rows; this one has {}",
                req.rows.len()
            ),
        )
        .with_details(json!({"max_rows": MAX_ROWS, "rows": req.rows.len()})));
    }
    let (mut out, with_detail) =
        retry_transient(|| run_once(state, ctx, change_set_id, kind, &req)).await?;
    if with_detail {
        out["change_set"] = service::set_detail(state, ctx, change_set_id).await?;
    }
    Ok(out)
}

/// The upload's one transaction: plan the rows against the live tables and the
/// draft, and - unless it is a dry run or nothing changes - store the changes.
/// On the feed's lock like every other draft write, so it never collides with a
/// commit; retried whole after a serialization failure. The bool says whether
/// the response carries the set detail (a dry run's does not).
async fn run_once(
    state: &EditorState,
    ctx: &Ctx,
    change_set_id: Uuid,
    kind: Kind,
    req: &BulkRequest,
) -> EditorResult<(Value, bool)> {
    let mut tx = state.pool.begin().await?;
    lock_feed_of_set(&mut tx, change_set_id).await?;
    let set = service::load_set(&mut tx, change_set_id, !req.dry_run).await?;
    service::editable(&set)?;
    let g = set.gtfs_id.clone();
    let draft = DraftView::load(&mut tx, change_set_id).await?;
    let mut plan = match kind {
        Kind::Stops => plan_stops(&mut tx, &g, &req.rows, &draft).await?,
        Kind::Routes => plan_routes(&mut tx, &g, &req.rows, &draft).await?,
        Kind::RouteStops => plan_route_stops(&mut tx, &g, &req.rows, &draft, !req.dry_run).await?,
        Kind::StopUpdates => {
            plan_stop_updates(&mut tx, &g, change_set_id, &req.rows, &draft, !req.dry_run).await?
        }
    };
    if req.dry_run {
        tx.rollback().await?;
        return Ok((respond(true, kind, &plan, &[]), false));
    }
    if plan.has_errors() {
        tx.rollback().await?;
        let out = respond(false, kind, &plan, &[]);
        return Err(EditorError::bad_request(
            "bulk_has_errors",
            format!(
                "{} row(s) have errors; fix them and preview again",
                out["summary"]["errors"]
            ),
        )
        .with_details(out));
    }
    if plan.changes.is_empty() {
        // every row is `unchanged`: uploading the same file again adds nothing,
        // so nothing is written - not the draft, not the audit log
        tx.rollback().await?;
        return Ok((respond(false, kind, &plan, &[]), true));
    }
    let unnamed: Vec<usize> = plan
        .changes
        .iter()
        .enumerate()
        .filter(|(_, c)| c.key.is_none())
        .map(|(k, _)| k)
        .collect();
    let minted = service::mint_stop_ids(&mut tx, &g, unnamed.len()).await?;
    for (k, id) in unnamed.into_iter().zip(minted) {
        let c = &mut plan.changes[k];
        c.after["stop_id"] = json!(id);
        c.key = Some(id);
    }
    let inserts: Vec<ChangeInsert> = plan
        .changes
        .iter()
        .map(|c| ChangeInsert {
            entity: c.entity.into(),
            op: c.op.into(),
            entity_key: c.key.clone().unwrap_or_default(),
            base_row_version: c.base,
            before: c.before.clone(),
            after: c.after.clone(),
        })
        .collect();
    let change_ids =
        service::insert_changes(&mut tx, change_set_id, ctx.user.user_id, &inserts).await?;
    sqlx::query("UPDATE gtfs_change_set SET updated_at = now() WHERE change_set_id = $1")
        .bind(change_set_id)
        .execute(&mut *tx)
        .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "bulk_imported",
        Some(&g),
        Some(change_set_id),
        json!({
            "kind": kind.name(), "rows": plan.rows.len(), "changes": change_ids.len(),
            "first_change_id": change_ids.first(), "last_change_id": change_ids.last(),
        }),
    )
    .await?;
    tx.commit().await?;
    Ok((respond(false, kind, &plan, &change_ids), true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_accept_csv_text() {
        let m = json!({"a": " 13.05 ", "b": 80.2, "c": "", "d": "x", "e": "12", "f": 3.0, "g": "2.5", "h": 7, "i": true})
            .as_object()
            .cloned()
            .unwrap();
        assert_eq!(cell_f64(&m, "a"), Ok(Some(13.05)));
        assert_eq!(cell_f64(&m, "b"), Ok(Some(80.2)));
        assert_eq!(cell_f64(&m, "c"), Ok(None));
        assert_eq!(cell_f64(&m, "missing"), Ok(None));
        assert!(cell_f64(&m, "d").is_err());
        assert_eq!(cell_i32(&m, "e"), Ok(Some(12)));
        assert_eq!(cell_i32(&m, "f"), Ok(Some(3)));
        assert!(cell_i32(&m, "g").is_err());
        assert_eq!(cell_text(&m, "c"), Ok(None));
        assert_eq!(cell_text(&m, "h"), Ok(Some("7".into())));
        assert!(cell_text(&m, "i").is_err());
    }

    #[test]
    fn every_row_says_what_it_does() {
        let row = |v: Value| {
            let m = v.as_object().unwrap().clone();
            m
        };
        // add, update and delete, however they are typed
        for (given, want) in [
            ("add", Action::Add),
            ("UPDATE", Action::Update),
            (" delete ", Action::Delete),
        ] {
            assert_eq!(
                cell_action(&row(json!({"action": given})), Kind::Stops),
                Ok(want),
                "{given}"
            );
        }
        // blank is never a default: the upload says what it does to every row
        for blank in [
            json!({}),
            json!({"action": ""}),
            json!({"action": "  "}),
            json!({"action": null}),
        ] {
            let e = cell_action(&row(blank), Kind::Stops).unwrap_err();
            assert!(e.contains("action is required"), "{e}");
        }
        let e = cell_action(&row(json!({"action": "upsert"})), Kind::Stops).unwrap_err();
        assert!(e.contains("not one of add, update or delete"), "{e}");
        assert!(cell_action(&row(json!({"action": 1})), Kind::Stops).is_err());
        // what each kind can be asked for
        assert_eq!(
            cell_action(&row(json!({"action": "delete"})), Kind::Routes),
            Ok(Action::Delete)
        );
        for (kind, action, says) in [
            (Kind::StopUpdates, "add", "carries its position"),
            (
                Kind::StopUpdates,
                "delete",
                "delete one with the stops file",
            ),
            (Kind::RouteStops, "delete", "leaving out the stops"),
        ] {
            let e = cell_action(&row(json!({"action": action})), kind).unwrap_err();
            assert!(e.contains(says), "{kind:?} {action}: {e}");
        }
        assert_eq!(
            cell_action(&row(json!({"action": "update"})), Kind::RouteStops),
            Ok(Action::Update)
        );
        assert_eq!(
            cell_action(&row(json!({"action": "update"})), Kind::StopUpdates),
            Ok(Action::Update)
        );
        // and it is a column of every kind
        for kind in [
            Kind::Stops,
            Kind::Routes,
            Kind::RouteStops,
            Kind::StopUpdates,
        ] {
            assert_eq!(kind.columns()[0], "action", "{kind:?}");
        }
    }

    #[test]
    fn rows_only_carry_their_kinds_columns() {
        assert!(row_object(&json!({"name": "A", "lat": 1, "lon": 2}), Kind::Stops).is_ok());
        let f = row_object(&json!({"name": "A", "notes": "x"}), Kind::Stops).unwrap_err();
        assert_eq!(f.code, "invalid_row");
        assert!(f.message.contains("notes"));
        assert!(row_object(&json!(["A"]), Kind::Routes).is_err());
        assert!(row_object(
            &json!({"route_id": "R", "sequence": 1, "stop_id": "S", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "X"}),
            Kind::RouteStops
        )
        .is_ok());
        assert!(Kind::parse("stations").is_none());
        assert_eq!(Kind::parse("stop_updates"), Some(Kind::StopUpdates));
        assert!(row_object(
            &json!({"stop_id": "S", "platform_code": "Towards X", "description": "d", "name": "n"}),
            Kind::StopUpdates
        )
        .is_ok());
        assert!(row_object(&json!({"stop_id": "S", "lat": 13.0}), Kind::StopUpdates).is_err());
    }

    #[test]
    fn duplicates_mark_every_row_involved() {
        let mut plan = Plan::new(4);
        let groups: HashMap<String, Vec<usize>> =
            [("A".to_string(), vec![3, 0]), ("B".to_string(), vec![2])]
                .into_iter()
                .collect();
        mark_duplicates(&mut plan, groups, |k| format!("stop_id {k}"));
        assert_eq!(plan.rows[0].findings[0].code, "duplicate_in_upload");
        assert!(plan.rows[0].findings[0].message.contains("rows 1, 4"));
        assert_eq!(plan.rows[3].findings.len(), 1);
        assert!(plan.rows[1].findings.is_empty() && plan.rows[2].findings.is_empty());
        assert!(plan.has_errors());
    }
}
