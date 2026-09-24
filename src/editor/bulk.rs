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
use super::records::{ROUTE_GTFS_FIELDS, STOP_GTFS_FIELDS};
use super::service::{self, rows_hash, ChangeInsert, FIRST_PATTERN};
use super::trips::{self, check_trips, TripRules, TripSpec};
use super::validation::{
    check_payload, check_route_rows_for, check_route_rows_labelled, grade_against_live, Finding,
    Level, RouteRow,
};
use super::EditorState;
use crate::gtfs::spec;
use crate::services::gtfs_timing::{Offsets, MAX_TIME_S};
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
    /// kind `records`: the file the rows are of (`pathways.txt`, section 18).
    #[serde(default)]
    pub file: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Stops,
    Routes,
    RouteStops,
    StopUpdates,
    RouteTrips,
    TimingProfiles,
    Services,
    /// Rows of any file the editor keeps as records (section 18).
    Records,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "stops" => Some(Kind::Stops),
            "routes" => Some(Kind::Routes),
            "route_stops" => Some(Kind::RouteStops),
            "stop_updates" => Some(Kind::StopUpdates),
            "route_trips" => Some(Kind::RouteTrips),
            "timing_profiles" => Some(Kind::TimingProfiles),
            "services" => Some(Kind::Services),
            "records" => Some(Kind::Records),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Stops => "stops",
            Kind::Routes => "routes",
            Kind::RouteStops => "route_stops",
            Kind::StopUpdates => "stop_updates",
            Kind::RouteTrips => "route_trips",
            Kind::TimingProfiles => "timing_profiles",
            Kind::Services => "services",
            Kind::Records => "records",
        }
    }

    pub fn columns(self) -> &'static [&'static str] {
        match self {
            // then records::STOP_GTFS_FIELDS, by their stops.txt names
            Kind::Stops => &[
                "action",
                "stop_id",
                "name",
                "lat",
                "lon",
                "platform_code",
                "tts_stop_name",
                "zone_id",
                "stop_url",
                "stop_timezone",
                "wheelchair_boarding",
                "level_id",
                "stop_access",
                "location_type",
                "parent_station",
            ],
            // then records::ROUTE_GTFS_FIELDS and the route's agency and type
            Kind::Routes => &[
                "action",
                "route_id",
                "short_name",
                "long_name",
                "color",
                "route_desc",
                "route_url",
                "route_sort_order",
                "continuous_pickup",
                "continuous_drop_off",
                "network_id",
                "agency_id",
                "route_type",
            ],
            Kind::RouteStops => &[
                "action",
                "route_id",
                "sequence",
                "stop_id",
                "stop_type",
                "stage_no",
                "stage_name",
                "pattern_key",
                "stop_headsign",
                "pickup_type",
                "drop_off_type",
                "timepoint",
                "stop_sequence",
                "continuous_pickup",
                "continuous_drop_off",
                "shape_dist_traveled",
                "pickup_booking_rule_id",
                "drop_off_booking_rule_id",
            ],
            Kind::StopUpdates => &["action", "stop_id", "platform_code", "description", "name"],
            Kind::RouteTrips => &[
                "action",
                "route_id",
                "trip_id",
                "pattern_key",
                "profile_key",
                "service_id",
                "direction_id",
                "start_time",
                "headsign",
                "short_name",
                "block_id",
                "shape_id",
                "wheelchair_accessible",
                "bikes_allowed",
                "cars_allowed",
                "frequencies",
                "source_ref",
            ],
            Kind::TimingProfiles => &[
                "action",
                "route_id",
                "pattern_key",
                "profile_key",
                "stop_sequence",
                "arrival_offset",
                "departure_offset",
                "label",
            ],
            Kind::Services => &[
                "action",
                "service_id",
                "monday",
                "tuesday",
                "wednesday",
                "thursday",
                "friday",
                "saturday",
                "sunday",
                "start_date",
                "end_date",
                "date",
                "exception_type",
                "label",
            ],
            // the file's own fields, checked by plan_records
            Kind::Records => &[],
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
            (Kind::RouteTrips, Action::Delete) => Some(
                "a route's trips are uploaded whole: upload them with action update, leaving out the trips it should not have"
                    .into(),
            ),
            (Kind::TimingProfiles, Action::Delete) => Some(
                "a timing profile is uploaded whole; delete one with a timing_profile/delete change".into(),
            ),
            (Kind::Services, Action::Delete) => Some(
                "a service is uploaded whole; delete one with a service/delete change".into(),
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

/// The GTFS `fields` of `file` a row fills in, into `into` as a change sends
/// them: each cell read as the file writes it (`2`, `https://…`), blank being
/// not given.
fn gtfs_cells(
    m: &Map<String, Value>,
    file: &str,
    fields: &[&str],
    into: &mut Map<String, Value>,
    bad: &mut Vec<String>,
) {
    let fspec = spec::file(file).expect("a file of the reference");
    for f in fields {
        let text = match cell_text(m, f) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                bad.push(e);
                continue;
            }
        };
        let fs = fspec.field(f).expect("a field of the file");
        match spec::from_text(fs, &text) {
            Ok(v) => {
                into.insert(f.to_string(), spec::to_api(fs, &v));
            }
            Err(why) => bad.push(format!("{f} {why}")),
        }
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
        let mut bad = Vec::new();
        gtfs_cells(m, "stops.txt", STOP_GTFS_FIELDS, &mut after, &mut bad);
        if !bad.is_empty() {
            plan.rows[i]
                .findings
                .extend(bad.into_iter().map(invalid_row));
            continue;
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
                format!("the row for stop {stop_id} gives no name, lat and lon, platform_code or other stops.txt field"),
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
        let mut bad = Vec::new();
        let gtfs: Vec<&str> = ROUTE_GTFS_FIELDS
            .iter()
            .copied()
            .chain(["agency_id", "route_type"])
            .collect();
        gtfs_cells(m, "routes.txt", &gtfs, &mut fields, &mut bad);
        if !bad.is_empty() {
            plan.rows[i]
                .findings
                .extend(bad.into_iter().map(invalid_row));
            continue;
        }
        if let Some(id) = &id {
            by_id.entry(id.clone()).or_default().push(i);
        }

        if action == Action::Add {
            let mut after = fields.clone();
            if let Some(id) = &id {
                after.insert("route_id".into(), json!(id));
            }
            // a bus of the feed's usual agency, unless the row says otherwise
            after.entry("route_type").or_insert(json!(3));
            if let Some(a) = &agency {
                after.entry("agency_id").or_insert(json!(a));
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
                format!("the row for route {route_id} gives no short_name, long_name, color or other routes.txt field"),
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

/// A route's stop order, as the upload groups its rows.
type PatternKey = (String, i16);

async fn plan_route_stops(
    conn: &mut PgConnection,
    g: &str,
    rows: &[Value],
    draft: &DraftView,
    with_before: bool,
) -> EditorResult<Plan> {
    let mut plan = Plan::new(rows.len());
    let mut routes: Vec<(PatternKey, Vec<StopRow>)> = Vec::new();
    let mut route_at: HashMap<PatternKey, usize> = HashMap::new();
    let mut by_seq: HashMap<(String, i16, i32), Vec<usize>> = HashMap::new();
    // what each stop order's rows say to do, and where they said it
    let mut route_action: HashMap<PatternKey, Vec<(usize, Action)>> = HashMap::new();
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
        // which stop order (section 16), and the GTFS fields a row may carry
        let pattern = optional_small(m, "pattern_key", 1..=i16::MAX as i64, &mut bad).unwrap_or(1);
        let small = |k: &str, max: i64, bad: &mut Vec<String>| optional_small(m, k, 0..=max, bad);
        let (pickup, dropoff, timepoint) = (
            small("pickup_type", 3, &mut bad),
            small("drop_off_type", 3, &mut bad),
            small("timepoint", 1, &mut bad),
        );
        let headsign = cell_text(m, "stop_headsign").unwrap_or_else(|e| {
            bad.push(e);
            None
        });
        let (continuous_pickup, continuous_drop_off) = (
            small("continuous_pickup", 3, &mut bad),
            small("continuous_drop_off", 3, &mut bad),
        );
        let stop_sequence = cell_i32(m, "stop_sequence").unwrap_or_else(|e| {
            bad.push(e);
            None
        });
        let shape_dist_traveled = cell_f64(m, "shape_dist_traveled").unwrap_or_else(|e| {
            bad.push(e);
            None
        });
        let [pickup_rule, drop_off_rule] = ["pickup_booking_rule_id", "drop_off_booking_rule_id"]
            .map(|k| {
                cell_text(m, k).unwrap_or_else(|e| {
                    bad.push(e);
                    None
                })
            });
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
            .entry((route_id.clone(), pattern, sequence))
            .or_default()
            .push(i);
        let key = (route_id.clone(), pattern);
        route_action
            .entry(key.clone())
            .or_default()
            .push((i, action));
        let at = *route_at.entry(key.clone()).or_insert_with(|| {
            routes.push((key, Vec::new()));
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
                pickup_type: pickup,
                drop_off_type: dropoff,
                timepoint,
                stop_headsign: headsign,
                stop_sequence,
                continuous_pickup,
                continuous_drop_off,
                shape_dist_traveled,
                pickup_booking_rule_id: pickup_rule,
                drop_off_booking_rule_id: drop_off_rule,
            },
        });
    }
    mark_duplicates(&mut plan, by_seq, |(route, pattern, seq)| {
        if *pattern == FIRST_PATTERN {
            format!("sequence {seq} of route {route}")
        } else {
            format!("sequence {seq} of pattern {pattern} of route {route}")
        }
    });
    for (_, list) in routes.iter_mut() {
        list.sort_by_key(|r| (r.sequence, r.upload));
    }

    // everything the upload references, in a few queries
    let keys: Vec<PatternKey> = routes.iter().map(|(k, _)| k.clone()).collect();
    let route_ids: Vec<String> = keys
        .iter()
        .map(|(r, _)| r.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let stop_ids: Vec<String> = routes
        .iter()
        .flat_map(|(_, list)| list.iter().filter_map(|r| r.row.stop_id.clone()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let live_routes = live_route_states(conn, g, &route_ids).await?;
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
    let live_rows = service::load_patterns_rows(conn, g, &keys).await?;
    let fare_stages = service::has_fare_stages(conn, g).await?;
    let mut read_rows = if with_before {
        service::load_patterns_read_rows(conn, g, &keys).await?
    } else {
        HashMap::new()
    };

    for ((route_id, pattern), list) in routes {
        let uploads: Vec<usize> = list.iter().map(|r| r.upload).collect();
        let rows: Vec<RouteRow> = list.iter().map(|r| r.row.clone()).collect();
        let key = (route_id.clone(), pattern);
        let live = live_rows.get(&key).cloned().unwrap_or_default();
        let current = draft.current_pattern_rows(&route_id, pattern, &live);
        let mut findings: Vec<(Option<usize>, Finding)> = Vec::new();

        // a stop order is uploaded whole, so its rows agree on the action
        let said = route_action.get(&key).cloned().unwrap_or_default();
        let what = if pattern == service::FIRST_PATTERN {
            format!("route {route_id}")
        } else {
            format!("stop order {pattern} of route {route_id}")
        };
        let route_state = route_problem(&route_id, &live_routes, draft).or_else(|| {
            let action = said.first().map(|(_, a)| *a).unwrap_or(Action::Update);
            group_action_problem(&said, !current.is_empty(), &what, "stop list").map(
                |(code, message)| {
                    let code = match code {
                        GroupProblem::Mixed => "mixed_action",
                        GroupProblem::Exists => "route_stops_exist",
                        GroupProblem::Missing => "route_stops_missing",
                    };
                    let message = match (code, action) {
                        ("route_stops_exist", _) => format!(
                            "{what} already has a stop list of {} stops; action update replaces it",
                            current.len()
                        ),
                        _ => message,
                    };
                    (code, message)
                },
            )
        });
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
                        Some((t, _)) if t != 0 => Some((
                            "not_a_stop",
                            format!(
                                "{id} is {} (location_type {t}); a route calls at a stop or platform",
                                service::stop_kind(t)
                            ),
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
                        check_route_rows_labelled(&rows, fare_stages, &label),
                        &check_route_rows_for(&current, fare_stages),
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
        if let Some(cid) = draft.pattern_replaced_by(&route_id, pattern) {
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
                for (k, v) in [
                    ("pickup_type", r.pickup_type),
                    ("drop_off_type", r.drop_off_type),
                    ("timepoint", r.timepoint),
                    ("continuous_pickup", r.continuous_pickup),
                    ("continuous_drop_off", r.continuous_drop_off),
                ] {
                    if let Some(v) = v {
                        o[k] = json!(v);
                    }
                }
                for (k, v) in [
                    ("stop_headsign", &r.stop_headsign),
                    ("pickup_booking_rule_id", &r.pickup_booking_rule_id),
                    ("drop_off_booking_rule_id", &r.drop_off_booking_rule_id),
                ] {
                    if let Some(v) = v {
                        o[k] = json!(v);
                    }
                }
                if let Some(n) = r.stop_sequence {
                    o["stop_sequence"] = json!(n);
                }
                if let Some(d) = r.shape_dist_traveled {
                    o["shape_dist_traveled"] = json!(d);
                }
                o
            })
            .collect();
        let mut after = json!({"base_rows_hash": rows_hash(&live), "rows": after_rows});
        if pattern != FIRST_PATTERN {
            after["pattern_key"] = json!(pattern);
        }
        if let Err(f) = check_payload("route_stops", "replace", &route_id, &after) {
            findings.push((None, f));
        }
        plan.changes.push(Planned {
            entity: "route_stops",
            op: "replace",
            key: Some(route_id.clone()),
            before: json!(read_rows.remove(&key).unwrap_or_default()),
            after,
            base: None,
        });
        let change = plan.changes.len() - 1;
        spread(&mut plan, &uploads, change, findings);
    }
    Ok(plan)
}

/// Attach a change and its findings to the upload rows it came from: a finding
/// about one row (`Some(k)`, an index into `uploads`) to that row, one about the
/// whole change to every row.
fn spread(
    plan: &mut Plan,
    uploads: &[usize],
    change: usize,
    findings: Vec<(Option<usize>, Finding)>,
) {
    for i in uploads {
        plan.rows[*i].change = Some(change);
    }
    for (at, f) in findings {
        match at {
            Some(k) => plan.rows[uploads[k]].findings.push(f),
            None => {
                for i in uploads {
                    plan.rows[*i].findings.push(f.clone());
                }
            }
        }
    }
}

/// An optional whole-number cell within `range`: blank is not given.
fn optional_small(
    m: &Map<String, Value>,
    key: &str,
    range: std::ops::RangeInclusive<i64>,
    bad: &mut Vec<String>,
) -> Option<i16> {
    match cell_i32(m, key) {
        Ok(None) => None,
        Ok(Some(v)) if range.contains(&(v as i64)) => Some(v as i16),
        Ok(Some(_)) => {
            bad.push(format!(
                "{key} must be from {} to {}",
                range.start(),
                range.end()
            ));
            None
        }
        Err(e) => {
            bad.push(e);
            None
        }
    }
}

/// Each route's `deleted`, for the routes an upload names.
async fn live_route_states(
    conn: &mut PgConnection,
    g: &str,
    route_ids: &[String],
) -> EditorResult<HashMap<String, bool>> {
    Ok(sqlx::query(
        "SELECT route_id, deleted FROM gtfs_route WHERE gtfs_id = $1 AND route_id = ANY($2)",
    )
    .bind(g)
    .bind(route_ids)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(String, bool), sqlx::Error> {
        Ok((r.try_get("route_id")?, r.try_get("deleted")?))
    })
    .collect::<Result<_, _>>()?)
}

/// Why a route cannot take a change once the draft applies, if it cannot.
fn route_problem(
    route_id: &str,
    live: &HashMap<String, bool>,
    draft: &DraftView,
) -> Option<(&'static str, String)> {
    match live.get(route_id) {
        Some(true) => Some(("route_deleted", format!("route {route_id} is deleted"))),
        _ if draft.route_deleted(route_id) => Some((
            "route_deleted",
            format!("route {route_id} is deleted in this draft"),
        )),
        None if draft.created_route(route_id).is_none() => {
            Some(("route_not_found", format!("no route {route_id}")))
        }
        _ => None,
    }
}

/// Why a group's action does not fit, for [`group_action_problem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupProblem {
    /// The group's rows say different actions.
    Mixed,
    /// `add` for something that is already there.
    Exists,
    /// `update` for something that is not there yet.
    Missing,
}

/// The one action the rows of a group say together - a stop order, a route's
/// trips, a timing profile and a service are each uploaded whole - checked
/// against whether `what` is already there: `add` gives it one that is not
/// yet, `update` replaces the one it has. `None` when the rows agree and the
/// action fits.
fn group_action_problem(
    said: &[(usize, Action)],
    exists: bool,
    what: &str,
    thing: &str,
) -> Option<(GroupProblem, String)> {
    let action = said.first().map(|(_, a)| *a)?;
    if said.iter().any(|(_, a)| *a != action) {
        return Some((
            GroupProblem::Mixed,
            format!(
                "{what} has rows saying {} and rows saying something else; a {thing} is one action",
                action.name()
            ),
        ));
    }
    match action {
        Action::Add if exists => Some((
            GroupProblem::Exists,
            format!("{what} already has its {thing}; action update replaces it"),
        )),
        Action::Update if !exists => Some((
            GroupProblem::Missing,
            format!("{what} has no {thing} yet; action add gives it one"),
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------- trips (section 16.5)

/// A JSON cell: a JSON value, or JSON written as text (a CSV cell).
fn cell_json(m: &Map<String, Value>, key: &str) -> Result<Option<Value>, String> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => serde_json::from_str(s)
            .map(Some)
            .map_err(|_| format!("{key} must be JSON")),
        Some(v) => Ok(Some(v.clone())),
    }
}

/// `{route_id, trip_id?, pattern_key, profile_key?, service_id, direction_id?,
/// start_time, ...}` rows: one `route_trips/replace` per route, its trips in
/// upload order. Checked with the single change's rules ([`check_trips`])
/// against the route once the draft applies; a trip id another route holds
/// then - live, or in the draft's trip list for it - is taken.
async fn plan_route_trips(
    conn: &mut PgConnection,
    g: &str,
    rows: &[Value],
    draft: &DraftView,
    with_before: bool,
) -> EditorResult<Plan> {
    let mut plan = Plan::new(rows.len());
    let mut routes: Vec<(String, Vec<(usize, TripSpec, Value)>)> = Vec::new();
    let mut route_at: HashMap<String, usize> = HashMap::new();
    let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
    let mut route_action: HashMap<String, Vec<(usize, Action)>> = HashMap::new();
    for (i, v) in rows.iter().enumerate() {
        let m = match row_object(v, Kind::RouteTrips) {
            Ok(m) => m,
            Err(f) => {
                plan.rows[i].findings.push(f);
                continue;
            }
        };
        let action = match cell_action(m, Kind::RouteTrips) {
            Ok(a) => a,
            Err(e) => {
                plan.rows[i].findings.push(invalid_row(e));
                continue;
            }
        };
        let mut bad = Vec::new();
        let mut trip = Map::new();
        let mut route_id = None;
        for k in [
            "route_id",
            "trip_id",
            "service_id",
            "start_time",
            "headsign",
            "short_name",
            "block_id",
            "shape_id",
        ] {
            match cell_text(m, k) {
                Ok(Some(t)) if k == "route_id" => route_id = Some(t),
                Ok(Some(t)) => {
                    trip.insert(k.into(), json!(t));
                }
                Ok(None) if ["route_id", "service_id", "start_time"].contains(&k) => {
                    bad.push(format!("{k} is required"))
                }
                Ok(None) => {}
                Err(e) => bad.push(e),
            }
        }
        match cell_i32(m, "pattern_key") {
            Ok(Some(p)) => {
                trip.insert("pattern_key".into(), json!(p));
            }
            Ok(None) => bad.push("pattern_key is required".into()),
            Err(e) => bad.push(e),
        }
        for k in [
            "profile_key",
            "direction_id",
            "wheelchair_accessible",
            "bikes_allowed",
            "cars_allowed",
        ] {
            match cell_i32(m, k) {
                Ok(Some(n)) => {
                    trip.insert(k.into(), json!(n));
                }
                Ok(None) => {}
                Err(e) => bad.push(e),
            }
        }
        for k in ["frequencies", "source_ref"] {
            match cell_json(m, k) {
                Ok(Some(j)) => {
                    trip.insert(k.into(), j);
                }
                Ok(None) => {}
                Err(e) => bad.push(e),
            }
        }
        let Some(route_id) = route_id.filter(|_| bad.is_empty()) else {
            plan.rows[i]
                .findings
                .extend(bad.into_iter().map(invalid_row));
            continue;
        };
        // a new trip an import brings is the import's; one already on the
        // route keeps its own source when the change applies
        trip.insert("source".into(), json!("import"));
        let trip = Value::Object(trip);
        // exactly the single change's shape check, on this one trip
        let probe = json!({"base_trips_hash": trips::empty_hash(), "trips": [trip.clone()]});
        if let Err(f) = check_payload("route_trips", "replace", &route_id, &probe) {
            let mut f = f;
            f.message = f.message.replace("route_trips/replace: trip 1: ", "");
            plan.rows[i].findings.push(f);
            continue;
        }
        let spec: TripSpec = match serde_json::from_value(trip.clone()) {
            Ok(s) => s,
            Err(e) => {
                plan.rows[i].findings.push(invalid_row(e.to_string()));
                continue;
            }
        };
        if let Some(id) = spec.id() {
            by_id.entry(id.to_string()).or_default().push(i);
        }
        route_action
            .entry(route_id.clone())
            .or_default()
            .push((i, action));
        let at = *route_at.entry(route_id.clone()).or_insert_with(|| {
            routes.push((route_id.clone(), Vec::new()));
            routes.len() - 1
        });
        routes[at].1.push((i, spec, trip));
    }
    mark_duplicates(&mut plan, by_id, |id| format!("trip_id {id}"));

    // the routes' patterns, profiles and trips, the services, and who holds the
    // ids, in a few queries
    let route_ids: Vec<String> = routes.iter().map(|(r, _)| r.clone()).collect();
    let live_routes = live_route_states(conn, g, &route_ids).await?;
    let mut live_patterns: HashMap<String, HashSet<i16>> = HashMap::new();
    for r in sqlx::query(
        "SELECT route_id, pattern_key FROM gtfs_pattern WHERE gtfs_id = $1 AND route_id = ANY($2)",
    )
    .bind(g)
    .bind(&route_ids)
    .fetch_all(&mut *conn)
    .await?
    {
        live_patterns
            .entry(r.try_get("route_id")?)
            .or_default()
            .insert(r.try_get("pattern_key")?);
    }
    let mut live_firsts: HashMap<String, HashMap<(i16, i32), i32>> = HashMap::new();
    for r in sqlx::query(
        "SELECT route_id, pattern_key, profile_key, arrival_s[1] AS first FROM gtfs_timing_profile \
         WHERE gtfs_id = $1 AND route_id = ANY($2)",
    )
    .bind(g)
    .bind(&route_ids)
    .fetch_all(&mut *conn)
    .await?
    {
        live_firsts
            .entry(r.try_get("route_id")?)
            .or_default()
            .insert(
                (r.try_get("pattern_key")?, r.try_get("profile_key")?),
                r.try_get::<Option<i32>, _>("first")?.unwrap_or(0),
            );
    }
    let service_ids: Vec<String> = routes
        .iter()
        .flat_map(|(_, list)| list.iter().map(|(_, t, _)| t.service_id.trim().to_string()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let live_services: HashSet<String> = sqlx::query(
        "SELECT service_id FROM gtfs_service WHERE gtfs_id = $1 AND service_id = ANY($2)",
    )
    .bind(g)
    .bind(&service_ids)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| r.try_get("service_id"))
    .collect::<Result<_, _>>()?;
    let trip_ids: Vec<String> = routes
        .iter()
        .flat_map(|(_, list)| {
            list.iter()
                .filter_map(|(_, t, _)| t.id().map(str::to_string))
        })
        .collect();
    let live_owners: HashMap<String, String> = sqlx::query(
        "SELECT trip_id, route_id FROM gtfs_trip WHERE gtfs_id = $1 AND trip_id = ANY($2)",
    )
    .bind(g)
    .bind(&trip_ids)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(String, String), sqlx::Error> {
        Ok((r.try_get("trip_id")?, r.try_get("route_id")?))
    })
    .collect::<Result<_, _>>()?;
    let uploaded: HashSet<&str> = route_ids.iter().map(String::as_str).collect();
    // who holds a trip id once the draft applies, among the routes this upload
    // does not replace: the draft's trip list for a route, else its live trips
    let mut owners: HashMap<String, String> = HashMap::new();
    for (id, route) in &live_owners {
        if uploaded.contains(route.as_str()) {
            continue;
        }
        let kept = match draft.route_trips(route) {
            Some((_, ids)) => ids.iter().any(|t| t == id),
            None => true,
        };
        if kept {
            owners.insert(id.clone(), route.clone());
        }
    }
    for (route, ids) in draft.trip_lists() {
        if uploaded.contains(route.as_str()) {
            continue;
        }
        for id in ids {
            owners.insert(id.clone(), route.clone());
        }
    }
    let mut live_trips = trips::load_routes_trips(conn, g, &route_ids).await?;

    for (route_id, list) in routes {
        let uploads: Vec<usize> = list.iter().map(|(i, _, _)| *i).collect();
        let specs: Vec<TripSpec> = list.iter().map(|(_, t, _)| t.clone()).collect();
        let live = live_trips.remove(&route_id).unwrap_or_default();
        let mut findings: Vec<(Option<usize>, Finding)> = Vec::new();
        let has_trips = draft
            .route_trips(&route_id)
            .map(|(_, ids)| !ids.is_empty())
            .unwrap_or(!live.is_empty());
        let said = route_action.get(&route_id).cloned().unwrap_or_default();
        let action_problem =
            group_action_problem(&said, has_trips, &format!("route {route_id}"), "trip list").map(
                |(problem, message)| {
                    let code = match problem {
                        GroupProblem::Mixed => "mixed_action",
                        GroupProblem::Exists => "route_trips_exist",
                        GroupProblem::Missing => "route_trips_missing",
                    };
                    (code, message)
                },
            );
        if let Some((code, message)) =
            route_problem(&route_id, &live_routes, draft).or(action_problem)
        {
            findings.push((None, Finding::error(code, route_id.as_str(), message)));
        } else {
            let live_keys = live_patterns.get(&route_id).cloned().unwrap_or_default();
            let named: HashSet<i16> = specs
                .iter()
                .map(|t| t.pattern_key)
                .chain(live_keys.iter().copied())
                .collect();
            let patterns: HashSet<i16> = named
                .into_iter()
                .filter(|k| draft.pattern_exists(&route_id, *k, live_keys.contains(k)))
                .collect();
            let firsts_live = live_firsts.get(&route_id).cloned().unwrap_or_default();
            let mut firsts: HashMap<(i16, i32), i32> = HashMap::new();
            for t in &specs {
                if let Some(p) = t.profile_key {
                    let live = firsts_live.get(&(t.pattern_key, p)).copied();
                    if let Some(first) =
                        draft.profile_first_arrival(&route_id, t.pattern_key, p, live)
                    {
                        firsts.insert((t.pattern_key, p), first);
                    }
                }
            }
            let service_exists = |s: &str| draft.service_exists(s, live_services.contains(s));
            let taken: HashMap<String, String> = specs
                .iter()
                .filter_map(|t| {
                    let id = t.id()?;
                    owners
                        .get(id)
                        .filter(|r| **r != route_id)
                        .map(|r| (id.to_string(), r.clone()))
                })
                .collect();
            let rules = TripRules {
                route_id: &route_id,
                patterns: &patterns,
                profiles: &firsts,
                service_exists: &service_exists,
                taken: &taken,
            };
            let label = |k: usize| format!("row {}", uploads[k] + 1);
            let (new, _) = check_trips(&specs, &rules, &label);
            let live_label = |k: usize| format!("trip {}", live[k].trip_id);
            let (old, _) = check_trips(&trips::specs_of(&live, &firsts_live), &rules, &live_label);
            findings.extend(
                grade_against_live(new, &old)
                    .into_iter()
                    .map(|f| (f.row, f)),
            );
        }
        if let Some((cid, _)) = draft.route_trips(&route_id) {
            findings.push((
                Some(0),
                Finding::warning(
                    "route_already_in_draft",
                    route_id.as_str(),
                    format!("change {cid} in this draft already replaces route {route_id}'s trips; this one replaces them again"),
                ),
            ));
        }
        let firsts_live = live_firsts.get(&route_id).cloned().unwrap_or_default();
        let before = if with_before {
            json!(live
                .iter()
                .map(|t| {
                    let first = t
                        .profile_key
                        .and_then(|p| firsts_live.get(&(t.pattern_key, p)).copied())
                        .unwrap_or(0);
                    trips::trip_json(t, first)
                })
                .collect::<Vec<_>>())
        } else {
            Value::Null
        };
        let after = json!({
            "base_trips_hash": trips::trips_hash(&live),
            "trips": list.iter().map(|(_, _, t)| t.clone()).collect::<Vec<_>>(),
        });
        plan.changes.push(Planned {
            entity: "route_trips",
            op: "replace",
            key: Some(route_id.clone()),
            before,
            after,
            base: None,
        });
        let change = plan.changes.len() - 1;
        spread(&mut plan, &uploads, change, findings);
    }
    Ok(plan)
}

/// `{route_id, pattern_key, profile_key, stop_sequence, arrival_offset,
/// departure_offset, label?}` rows, one stop of one profile each: one
/// `timing_profile/replace` per profile. `stop_sequence` is the stop's position
/// among the pattern's served stops, 1 to n, as the pattern stands once the
/// draft applies.
async fn plan_timing_profiles(
    conn: &mut PgConnection,
    g: &str,
    rows: &[Value],
    draft: &DraftView,
    with_before: bool,
) -> EditorResult<Plan> {
    type ProfileKey = (String, i16, i32);
    let mut plan = Plan::new(rows.len());
    let mut profiles: Vec<(ProfileKey, Vec<(usize, i32, i32, i32)>, Option<String>)> = Vec::new();
    let mut profile_at: HashMap<ProfileKey, usize> = HashMap::new();
    let mut by_seq: HashMap<(String, i16, i32, i32), Vec<usize>> = HashMap::new();
    let mut profile_action: HashMap<ProfileKey, Vec<(usize, Action)>> = HashMap::new();
    for (i, v) in rows.iter().enumerate() {
        let m = match row_object(v, Kind::TimingProfiles) {
            Ok(m) => m,
            Err(f) => {
                plan.rows[i].findings.push(f);
                continue;
            }
        };
        let action = match cell_action(m, Kind::TimingProfiles) {
            Ok(a) => a,
            Err(e) => {
                plan.rows[i].findings.push(invalid_row(e));
                continue;
            }
        };
        let mut bad = Vec::new();
        let route_id = match cell_text(m, "route_id") {
            Ok(Some(r)) => Some(r),
            Ok(None) => {
                bad.push("route_id is required".into());
                None
            }
            Err(e) => {
                bad.push(e);
                None
            }
        };
        let mut whole = |k: &str, min: i32| match cell_i32(m, k) {
            Ok(Some(n)) if n >= min => Some(n),
            Ok(Some(_)) => {
                bad.push(format!("{k} must be {min} or more"));
                None
            }
            Ok(None) => {
                bad.push(format!("{k} is required"));
                None
            }
            Err(e) => {
                bad.push(e);
                None
            }
        };
        let (pattern, profile, seq) = (
            whole("pattern_key", 1),
            whole("profile_key", 1),
            whole("stop_sequence", 1),
        );
        let (arrival, departure) = (
            whole("arrival_offset", -MAX_TIME_S),
            whole("departure_offset", -MAX_TIME_S),
        );
        let label = cell_text(m, "label").unwrap_or_else(|e| {
            bad.push(e);
            None
        });
        let (
            Some(route_id),
            Some(pattern),
            Some(profile),
            Some(seq),
            Some(arrival),
            Some(departure),
        ) = (route_id, pattern, profile, seq, arrival, departure)
        else {
            plan.rows[i]
                .findings
                .extend(bad.into_iter().map(invalid_row));
            continue;
        };
        if pattern > i16::MAX as i32 {
            plan.rows[i]
                .findings
                .push(invalid_row("pattern_key is too large"));
            continue;
        }
        let pattern = pattern as i16;
        by_seq
            .entry((route_id.clone(), pattern, profile, seq))
            .or_default()
            .push(i);
        let key = (route_id, pattern, profile);
        profile_action
            .entry(key.clone())
            .or_default()
            .push((i, action));
        let at = *profile_at.entry(key.clone()).or_insert_with(|| {
            profiles.push((key, Vec::new(), None));
            profiles.len() - 1
        });
        profiles[at].1.push((i, seq, arrival, departure));
        if label.is_some() {
            profiles[at].2 = label;
        }
    }
    mark_duplicates(&mut plan, by_seq, |(route, pattern, profile, seq)| {
        format!("stop_sequence {seq} of profile {profile} of pattern {pattern} of route {route}")
    });

    let keys: Vec<PatternKey> = profiles
        .iter()
        .map(|((r, p, _), _, _)| (r.clone(), *p))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let route_ids: Vec<String> = keys
        .iter()
        .map(|(r, _)| r.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let live_routes = live_route_states(conn, g, &route_ids).await?;
    let live_patterns: HashSet<PatternKey> = sqlx::query(
        "SELECT route_id, pattern_key FROM gtfs_pattern WHERE gtfs_id = $1 AND route_id = ANY($2)",
    )
    .bind(g)
    .bind(&route_ids)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<PatternKey, sqlx::Error> {
        Ok((r.try_get("route_id")?, r.try_get("pattern_key")?))
    })
    .collect::<Result<_, _>>()?;
    let live_rows = service::load_patterns_rows(conn, g, &keys).await?;
    let mut live_profiles: HashMap<ProfileKey, trips::StoredProfile> = HashMap::new();
    for r in &route_ids {
        for p in trips::load_profiles(conn, g, r, None).await? {
            live_profiles.insert((r.clone(), p.pattern_key, p.profile_key), p);
        }
    }
    // the served stops of every pattern once the draft applies, and where they are
    let served: HashMap<PatternKey, Vec<String>> = keys
        .iter()
        .map(|k| {
            let live = live_rows.get(k).cloned().unwrap_or_default();
            let rows = draft.current_pattern_rows(&k.0, k.1, &live);
            (k.clone(), trips::served_ids(&rows))
        })
        .collect();
    let stop_ids: Vec<String> = served
        .values()
        .flatten()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let positions: HashMap<String, (f64, f64)> = sqlx::query(
        "SELECT stop_id, lat, lon FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = ANY($2)",
    )
    .bind(g)
    .bind(&stop_ids)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(String, (f64, f64)), sqlx::Error> {
        Ok((
            r.try_get("stop_id")?,
            (r.try_get("lat")?, r.try_get("lon")?),
        ))
    })
    .collect::<Result<_, _>>()?;
    let at = |id: &str| {
        draft
            .stop_position(id)
            .or_else(|| positions.get(id).copied())
    };

    for ((route_id, pattern, profile), mut list, label) in profiles {
        list.sort_by_key(|(i, seq, _, _)| (*seq, *i));
        let uploads: Vec<usize> = list.iter().map(|(i, _, _, _)| *i).collect();
        let offsets = Offsets {
            arrival: list.iter().map(|(_, _, a, _)| *a).collect(),
            departure: list.iter().map(|(_, _, _, d)| *d).collect(),
        };
        let key = (route_id.clone(), pattern, profile);
        let mut findings: Vec<(Option<usize>, Finding)> = Vec::new();
        let seqs: Vec<i32> = list.iter().map(|(_, s, _, _)| *s).collect();
        let pk = (route_id.clone(), pattern);
        let profile_there = live_profiles.contains_key(&key)
            || draft
                .written_profiles(&route_id)
                .iter()
                .any(|(p, k, _)| *p == pattern && *k == profile);
        let said = profile_action.get(&key).cloned().unwrap_or_default();
        let action_problem = group_action_problem(
            &said,
            profile_there,
            &format!("pattern {pattern} of route {route_id}"),
            &format!("profile {profile}"),
        )
        .map(|(problem, message)| {
            let code = match problem {
                GroupProblem::Mixed => "mixed_action",
                GroupProblem::Exists => "profile_exists",
                GroupProblem::Missing => "profile_missing",
            };
            (code, message)
        });
        if let Some((code, message)) =
            route_problem(&route_id, &live_routes, draft).or(action_problem)
        {
            findings.push((None, Finding::error(code, route_id.as_str(), message)));
        } else if !draft.pattern_exists(&route_id, pattern, live_patterns.contains(&pk)) {
            findings.push((
                None,
                Finding::error(
                    "pattern_not_found",
                    route_id.as_str(),
                    format!("route {route_id} has no pattern {pattern}"),
                ),
            ));
        } else {
            let stops = served.get(&pk).cloned().unwrap_or_default();
            let expected: Vec<i32> = (1..=stops.len() as i32).collect();
            if seqs != expected {
                findings.push((
                    None,
                    Finding::error(
                        "profile_length_mismatch",
                        format!("{route_id}|{pattern}|{profile}"),
                        format!(
                            "profile {profile} of pattern {pattern} of route {route_id}: stop_sequence runs 1 to {} over the pattern's served stops, and the upload gives {}",
                            stops.len(),
                            seqs.iter().map(i32::to_string).collect::<Vec<_>>().join(", ")
                        ),
                    ),
                ));
            } else {
                let points: Vec<(f64, f64)> = stops.iter().filter_map(|s| at(s)).collect();
                if points.len() == stops.len() {
                    findings.extend(
                        trips::implausible_warning(
                            &format!("profile {profile} of pattern {pattern} of route {route_id}"),
                            &format!("{pattern}|{profile}"),
                            &offsets,
                            &points,
                        )
                        .map(|f| (Some(0), f)),
                    );
                }
            }
        }
        if draft
            .written_profiles(&route_id)
            .iter()
            .any(|(p, k, _)| *p == pattern && *k == profile)
        {
            findings.push((
                Some(0),
                Finding::warning(
                    "profile_already_in_draft",
                    format!("{route_id}|{pattern}|{profile}"),
                    format!("this draft already writes profile {profile} of pattern {pattern} of route {route_id}; this one replaces it again"),
                ),
            ));
        }
        let live = live_profiles.get(&key);
        let mut after = json!({
            "pattern_key": pattern, "profile_key": profile,
            "arrival_s": offsets.arrival, "departure_s": offsets.departure,
            "base_hash": trips::profile_hash(live), "source": "import",
        });
        if let Some(l) = &label {
            after["label"] = json!(l);
        }
        if let Err(f) = check_payload("timing_profile", "replace", &route_id, &after) {
            findings.push((None, f));
        }
        let before = match (with_before, live) {
            (true, Some(_)) => trips::profile_read(conn, g, &route_id, pattern, profile)
                .await?
                .map(|(v, _)| v)
                .unwrap_or(Value::Null),
            _ => Value::Null,
        };
        plan.changes.push(Planned {
            entity: "timing_profile",
            op: "replace",
            key: Some(route_id.clone()),
            before,
            after,
            base: None,
        });
        let change = plan.changes.len() - 1;
        spread(&mut plan, &uploads, change, findings);
    }
    Ok(plan)
}

/// A yes/no cell: `1`, `0`, `true`, `false` (a CSV cell is text); blank is no.
fn cell_bool(m: &Map<String, Value>, key: &str) -> Result<Option<bool>, String> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(Value::Number(n)) if n.as_i64() == Some(0) || n.as_i64() == Some(1) => {
            Ok(Some(n.as_i64() == Some(1)))
        }
        Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "" => Ok(None),
            "1" | "true" => Ok(Some(true)),
            "0" | "false" => Ok(Some(false)),
            _ => Err(format!("{key} must be 1 or 0")),
        },
        Some(_) => Err(format!("{key} must be 1 or 0")),
    }
}

/// `{service_id, monday..sunday, start_date?, end_date?, date?,
/// exception_type?, label?}` rows, one per added or removed date, each
/// repeating its service's days: one `service/create` (or `update`, for a
/// service there is) per service. The upload describes a service whole: its
/// dates replace the list.
async fn plan_services(
    conn: &mut PgConnection,
    g: &str,
    rows: &[Value],
    draft: &DraftView,
    with_before: bool,
) -> EditorResult<Plan> {
    struct Service {
        uploads: Vec<usize>,
        days: Option<[bool; 7]>,
        range: Option<(Option<String>, Option<String>)>,
        label: Option<String>,
        dates: Vec<(String, i32, usize)>,
        disagree: Option<String>,
    }
    let mut plan = Plan::new(rows.len());
    let mut services: Vec<(String, Service)> = Vec::new();
    let mut service_at: HashMap<String, usize> = HashMap::new();
    let mut by_date: HashMap<(String, String), Vec<usize>> = HashMap::new();
    let mut service_action: HashMap<String, Vec<(usize, Action)>> = HashMap::new();
    for (i, v) in rows.iter().enumerate() {
        let m = match row_object(v, Kind::Services) {
            Ok(m) => m,
            Err(f) => {
                plan.rows[i].findings.push(f);
                continue;
            }
        };
        let action = match cell_action(m, Kind::Services) {
            Ok(a) => a,
            Err(e) => {
                plan.rows[i].findings.push(invalid_row(e));
                continue;
            }
        };
        let mut bad = Vec::new();
        let mut text = |k: &str| {
            cell_text(m, k).unwrap_or_else(|e| {
                bad.push(e);
                None
            })
        };
        let (id, start, end, date, label) = (
            text("service_id"),
            text("start_date"),
            text("end_date"),
            text("date"),
            text("label"),
        );
        let mut days = [false; 7];
        for (d, name) in trips::DAYS.iter().enumerate() {
            match cell_bool(m, name) {
                Ok(v) => days[d] = v.unwrap_or(false),
                Err(e) => bad.push(e),
            }
        }
        let kind = match cell_i32(m, "exception_type") {
            Ok(k) => k,
            Err(e) => {
                bad.push(e);
                None
            }
        };
        if date.is_some() != kind.is_some() {
            bad.push("date and exception_type are given together".into());
        }
        if id.is_none() {
            bad.push("service_id is required".into());
        }
        let Some(id) = id.filter(|_| bad.is_empty()) else {
            plan.rows[i]
                .findings
                .extend(bad.into_iter().map(invalid_row));
            continue;
        };
        service_action
            .entry(id.clone())
            .or_default()
            .push((i, action));
        let at = *service_at.entry(id.clone()).or_insert_with(|| {
            services.push((
                id.clone(),
                Service {
                    uploads: vec![],
                    days: None,
                    range: None,
                    label: None,
                    dates: vec![],
                    disagree: None,
                },
            ));
            services.len() - 1
        });
        let s = &mut services[at].1;
        s.uploads.push(i);
        // every row repeats its service's days, range and label
        let range = (start, end);
        if s.days.is_some_and(|d| d != days)
            || s.range.as_ref().is_some_and(|r| *r != range)
            || (label.is_some() && s.label.is_some() && s.label != label)
        {
            s.disagree = Some(format!(
                "row {} gives service {id} other days, dates or label than its row {}",
                i + 1,
                s.uploads[0] + 1
            ));
        }
        s.days.get_or_insert(days);
        s.range.get_or_insert(range);
        if label.is_some() {
            s.label = label;
        }
        if let (Some(d), Some(k)) = (date, kind) {
            by_date.entry((id.clone(), d.clone())).or_default().push(i);
            s.dates.push((d, k, i));
        }
    }
    mark_duplicates(&mut plan, by_date, |(id, date)| {
        format!("date {date} of service {id}")
    });

    let ids: Vec<String> = services.iter().map(|(id, _)| id.clone()).collect();
    let live: HashMap<String, i32> = sqlx::query(
        "SELECT service_id, row_version FROM gtfs_service WHERE gtfs_id = $1 AND service_id = ANY($2)",
    )
    .bind(g)
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| -> Result<(String, i32), sqlx::Error> {
        Ok((r.try_get("service_id")?, r.try_get("row_version")?))
    })
    .collect::<Result<_, _>>()?;
    for (id, s) in services {
        let mut findings: Vec<(Option<usize>, Finding)> = Vec::new();
        if let Some(why) = &s.disagree {
            findings.push((
                None,
                Finding::error("invalid_row", id.as_str(), why.clone()),
            ));
        }
        let days = s.days.unwrap_or([false; 7]);
        let mut day_map = Map::new();
        for (d, name) in trips::DAYS.iter().enumerate() {
            day_map.insert(name.to_string(), json!(days[d]));
        }
        let (start, end) = s.range.clone().unwrap_or((None, None));
        let dates: Vec<Value> = s
            .dates
            .iter()
            .map(|(d, k, _)| json!({"date": d, "exception_type": k}))
            .collect();
        let exists = draft.service_exists(&id, live.contains_key(&id));
        let said = service_action.get(&id).cloned().unwrap_or_default();
        if let Some((problem, message)) =
            group_action_problem(&said, exists, &format!("service {id}"), "calendar")
        {
            let code = match problem {
                GroupProblem::Mixed => "mixed_action",
                GroupProblem::Exists => "service_exists",
                GroupProblem::Missing => "service_not_found",
            };
            findings.push((None, Finding::error(code, id.as_str(), message)));
        }
        let mut after = json!({
            "days": day_map, "start_date": start, "end_date": end, "dates": dates,
        });
        if let Some(l) = &s.label {
            after["label"] = json!(l);
        }
        let op = if exists { "update" } else { "create" };
        if !exists {
            after["service_id"] = json!(id);
        }
        if let Err(f) = check_payload("service", op, &id, &after) {
            findings.push((None, f));
        }
        let runs = days.iter().any(|d| *d) || s.dates.iter().any(|(_, k, _)| *k == 1);
        if !runs {
            findings.push((
                Some(0),
                Finding::warning(
                    "service_never_runs",
                    id.as_str(),
                    format!("service {id} runs on no day of the week and no added date; its trips never run"),
                ),
            ));
        }
        let before = if with_before && exists {
            trips::service_json(conn, g, &id)
                .await?
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        plan.changes.push(Planned {
            entity: "service",
            op,
            key: Some(id.clone()),
            before,
            after,
            base: if exists { live.get(&id).copied() } else { None },
        });
        let change = plan.changes.len() - 1;
        spread(&mut plan, &s.uploads, change, findings);
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

// ---------------------------------------------------------------- records (section 18)

/// The record file a `records` upload names.
fn records_file(file: Option<&str>) -> EditorResult<&'static crate::gtfs::spec::FileSpec> {
    let file = file
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .ok_or_else(|| {
            EditorError::bad_request(
                "file_required",
                "a records upload says which file it is: file",
            )
        })?;
    let fspec = crate::gtfs::spec::file(file).ok_or_else(|| {
        EditorError::bad_request(
            "invalid_file",
            format!("{file} is not a file of the GTFS reference"),
        )
    })?;
    if fspec.table().is_none() {
        return Err(EditorError::bad_request(
            "invalid_file",
            format!(
                "{} has an upload of its own: stops, routes, route_stops, route_trips, timing_profiles or services",
                fspec.name
            ),
        ));
    }
    Ok(fspec)
}

/// `records` rows: one row of `fspec`'s file each, by its own field names, with
/// an `action`: `add` creates the row, `update` sets the cells given (a blank
/// cell is not given), `delete` removes it. A file with no id of its own names
/// an existing row by `row_id`. One change per row, each found exactly as the
/// draft would find it: the rows are replayed after the draft's own changes, in
/// a savepoint that is rolled back.
async fn plan_records(
    conn: &mut PgConnection,
    g: &str,
    change_set_id: Uuid,
    fspec: &'static crate::gtfs::spec::FileSpec,
    rows: &[Value],
    actor: &str,
) -> EditorResult<Plan> {
    use crate::gtfs::spec::Key;
    let entity = fspec.entity().expect("a record file");
    let is_shape = entity == "shape";
    let mut plan = Plan::new(rows.len());
    let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, v) in rows.iter().enumerate() {
        let Some(m) = v.as_object() else {
            plan.rows[i]
                .findings
                .push(invalid_row("a row is an object of column: value"));
            continue;
        };
        let known = |k: &str| {
            k == "action"
                || (k == "row_id" && matches!(fspec.key(), Some(Key::Minted { .. })))
                || (is_shape && k == "points")
                || fspec.field(k).is_some()
        };
        if let Some(k) = m.keys().find(|k| !known(k)) {
            plan.rows[i].findings.push(invalid_row(format!(
                "{k:?} is not a column of {} (columns: action, {})",
                fspec.name,
                fspec
                    .fields
                    .iter()
                    .map(|f| f.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
            continue;
        }
        let action = match cell_action(m, Kind::Records) {
            Ok(a) => a,
            Err(e) => {
                plan.rows[i].findings.push(invalid_row(e));
                continue;
            }
        };
        let mut after = Map::new();
        for (k, v) in m {
            if k == "action" || k == "row_id" {
                continue;
            }
            match v {
                Value::Null => {}
                Value::String(s) if s.trim().is_empty() => {}
                // a CSV cell of points is JSON written as text
                Value::String(s) if k == "points" => match serde_json::from_str::<Value>(s) {
                    Ok(p) => {
                        after.insert(k.clone(), p);
                    }
                    Err(_) => {
                        after.insert(k.clone(), v.clone());
                    }
                },
                _ => {
                    after.insert(k.clone(), v.clone());
                }
            }
        }
        let named = |f: &str| {
            m.get(f).and_then(|v| match v {
                Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
        };
        let key = match fspec.key() {
            Some(Key::Field(f)) => named(f),
            Some(Key::Minted { .. }) if action == Action::Add => {
                if named("row_id").is_some() {
                    plan.rows[i].findings.push(invalid_row(
                        "a new row gets its row_id when it is added; leave row_id blank",
                    ));
                    continue;
                }
                Some(super::records::mint_row_id())
            }
            Some(Key::Minted { .. }) => named("row_id"),
            _ => Some(g.to_string()),
        };
        let Some(key) = key else {
            let field = match fspec.key() {
                Some(Key::Field(f)) => f,
                _ => "row_id",
            };
            plan.rows[i].findings.push(invalid_row(format!(
                "{field} is required to {}",
                action.name()
            )));
            continue;
        };
        let (op, after) = match action {
            Action::Add => ("create", Value::Object(after)),
            Action::Update => ("update", Value::Object(after)),
            Action::Delete => ("delete", Value::Null),
        };
        if let Err(f) = super::records::check_payload(fspec, op, &key, &after) {
            plan.rows[i].findings.push(f);
            continue;
        }
        by_key.entry(key.clone()).or_default().push(i);
        plan.rows[i].change = Some(plan.changes.len());
        plan.changes.push(Planned {
            entity,
            op,
            key: Some(key),
            after,
            before: Value::Null,
            base: None,
        });
    }
    mark_duplicates(&mut plan, by_key, |k| format!("{} {k}", fspec.name));

    // an update or a delete is based on the live row, as a single change is
    let keys: Vec<String> = plan
        .changes
        .iter()
        .filter(|c| c.op != "create")
        .filter_map(|c| c.key.clone())
        .collect();
    let live = super::records::live_rows(conn, g, fspec, &keys).await?;
    for c in plan.changes.iter_mut().filter(|c| c.op != "create") {
        if let Some((before, version)) = c.key.as_ref().and_then(|k| live.get(k)) {
            c.before = before.clone();
            c.base = Some(*version);
        }
    }

    // what the draft would find: its own changes, then these, replayed and
    // rolled back
    let mut all = service::load_changes_to_apply(conn, change_set_id).await?;
    let next = all.iter().map(|c| c.position).max().unwrap_or(0);
    let probe_id = |k: usize| -(k as i64) - 1;
    for (k, c) in plan.changes.iter().enumerate() {
        all.push(service::ChangeRow {
            change_id: probe_id(k),
            position: next + 1 + k as i32,
            entity: c.entity.into(),
            entity_key: c.key.clone().unwrap_or_default(),
            op: c.op.into(),
            base_row_version: c.base,
            before: Value::Null,
            after: c.after.clone(),
            created_by: Uuid::nil(),
            created_at: chrono::Utc::now(),
        });
    }
    sqlx::query("SAVEPOINT bulk_records")
        .execute(&mut *conn)
        .await?;
    let ev = service::evaluate(conn, g, &all, actor).await;
    sqlx::query("ROLLBACK TO SAVEPOINT bulk_records")
        .execute(&mut *conn)
        .await?;
    sqlx::query("RELEASE SAVEPOINT bulk_records")
        .execute(&mut *conn)
        .await?;
    let ev = ev?;
    let row_of: HashMap<i64, Vec<usize>> = {
        let mut m: HashMap<i64, Vec<usize>> = HashMap::new();
        for (i, o) in plan.rows.iter().enumerate() {
            if let Some(k) = o.change {
                m.entry(probe_id(k)).or_default().push(i);
            }
        }
        m
    };
    for v in ev.validation.iter().chain(ev.conflicts.iter()) {
        let Some(rows) = v["change_id"].as_i64().and_then(|id| row_of.get(&id)) else {
            continue;
        };
        let code = v["code"]
            .as_str()
            .or(v["reason"].as_str())
            .unwrap_or("conflict");
        let message = v["message"].as_str().unwrap_or("").to_string();
        let f = if v["level"] == "warning" {
            Finding::warning(code, "", message)
        } else {
            Finding::error(code, "", message)
        };
        for i in rows {
            plan.rows[*i].findings.push(f.clone());
        }
    }
    Ok(plan)
}

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
            "kind is stops, routes, route_stops, stop_updates, route_trips, timing_profiles, services, or records with a file",
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
/// The upload's rows, planned against the draft as it stands.
async fn plan_kind(
    conn: &mut PgConnection,
    g: &str,
    change_set_id: Uuid,
    kind: Kind,
    req: &BulkRequest,
    draft: &DraftView,
    actor: &str,
) -> EditorResult<Plan> {
    let (rows, with_before) = (&req.rows, !req.dry_run);
    Ok(match kind {
        Kind::Stops => plan_stops(conn, g, rows, draft).await?,
        Kind::Routes => plan_routes(conn, g, rows, draft).await?,
        Kind::RouteStops => plan_route_stops(conn, g, rows, draft, with_before).await?,
        Kind::StopUpdates => {
            plan_stop_updates(conn, g, change_set_id, rows, draft, with_before).await?
        }
        Kind::RouteTrips => plan_route_trips(conn, g, rows, draft, with_before).await?,
        Kind::TimingProfiles => plan_timing_profiles(conn, g, rows, draft, with_before).await?,
        Kind::Services => plan_services(conn, g, rows, draft, with_before).await?,
        Kind::Records => {
            let fspec = records_file(req.file.as_deref())?;
            plan_records(conn, g, change_set_id, fspec, rows, actor).await?
        }
    })
}

/// Append a plan's changes to the draft, minting the stop and trip ids its
/// rows left blank as single changes would. Returns the new change ids.
async fn insert_plan(
    conn: &mut PgConnection,
    g: &str,
    change_set_id: Uuid,
    user_id: Uuid,
    plan: &mut Plan,
) -> EditorResult<Vec<i64>> {
    let unnamed: Vec<usize> = plan
        .changes
        .iter()
        .enumerate()
        .filter(|(_, c)| c.key.is_none())
        .map(|(k, _)| k)
        .collect();
    let minted = service::mint_stop_ids(conn, g, unnamed.len()).await?;
    for (k, id) in unnamed.into_iter().zip(minted) {
        let c = &mut plan.changes[k];
        c.after["stop_id"] = json!(id);
        c.key = Some(id);
    }
    // a trip uploaded without an id gets one, as a single change's would
    for c in plan
        .changes
        .iter_mut()
        .filter(|c| c.entity == "route_trips")
    {
        let route = c.key.clone().unwrap_or_default();
        service::mint_missing_trip_ids(conn, g, &route, &mut c.after).await?;
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
    service::insert_changes(conn, change_set_id, user_id, &inserts).await
}

/// An upload an import builds itself (section 18): `rows` of `kind` planned
/// against the draft as it stands, exactly as an upload of them would be, and
/// appended to it when no row has an error. Returns each row's findings and
/// the new change ids (none when a row has an error).
pub(super) async fn plan_into_set(
    conn: &mut PgConnection,
    g: &str,
    change_set_id: Uuid,
    (user_id, actor): (Uuid, &str),
    kind: Kind,
    rows: Vec<Value>,
) -> EditorResult<(Vec<Vec<Finding>>, Vec<i64>)> {
    let req = BulkRequest {
        kind: kind.name().to_string(),
        rows,
        dry_run: false,
        file: None,
    };
    let draft = DraftView::load(conn, change_set_id).await?;
    let mut plan = plan_kind(conn, g, change_set_id, kind, &req, &draft, actor).await?;
    let findings: Vec<Vec<Finding>> = plan.rows.iter().map(|o| o.findings.clone()).collect();
    if plan.has_errors() {
        return Ok((findings, vec![]));
    }
    let ids = insert_plan(conn, g, change_set_id, user_id, &mut plan).await?;
    Ok((findings, ids))
}

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
    let mut plan = plan_kind(
        &mut tx,
        &g,
        change_set_id,
        kind,
        req,
        &draft,
        &ctx.user.email,
    )
    .await?;
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
    let change_ids = insert_plan(&mut tx, &g, change_set_id, ctx.user.user_id, &mut plan).await?;
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
    fn stops_and_routes_take_every_gtfs_field_their_changes_take() {
        for f in STOP_GTFS_FIELDS {
            assert!(Kind::Stops.columns().contains(f), "{f}");
        }
        for f in ROUTE_GTFS_FIELDS.iter().chain(&["agency_id", "route_type"]) {
            assert!(Kind::Routes.columns().contains(f), "{f}");
        }
    }

    #[test]
    fn gtfs_cells_read_a_cell_as_the_file_writes_it() {
        let m = json!({"wheelchair_boarding": "1", "stop_url": " ", "zone_id": 7})
            .as_object()
            .unwrap()
            .clone();
        let (mut into, mut bad) = (Map::new(), Vec::new());
        gtfs_cells(&m, "stops.txt", STOP_GTFS_FIELDS, &mut into, &mut bad);
        assert!(bad.is_empty(), "{bad:?}");
        assert_eq!(
            into,
            *json!({"wheelchair_boarding": 1, "zone_id": "7"})
                .as_object()
                .unwrap()
        );
        let m = json!({"route_type": "bus"}).as_object().unwrap().clone();
        gtfs_cells(&m, "routes.txt", &["route_type"], &mut into, &mut bad);
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert!(bad[0].starts_with("route_type"));
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
