//! A node's versions, and rolling a node back to one of them (docs/gtfs-editor.md
//! section 26).
//!
//! A **node** is one thing a draft edits: a stop, a station, a route, one of a
//! route's stop orders (its stop list), a stage, a route's stage list, or a row
//! of a record file (an agency, a fare, a pathway, ...).
//!
//! Its **versions** are the committed changes that touched it, each labelled by
//! the feed version it went live in. Its own changes (`direct`) record the whole
//! node: a change's `before` is the node as it was when the change was made, and
//! the commit refuses a change whose base moved, so `before` is exactly the node
//! just before that version, and `before` with the change's `after` laid over it
//! the node just after. A change that touched it as a side effect - a stop merge
//! moving a route's rows, a stage split, a station taking a stop as a platform -
//! is listed too, but records only what it moved, so it is no version to restore.
//! Where a version's node and the next version's `before` disagree with nothing
//! listed between them, something this history does not see changed it (a stop
//! merge editing a stage's rows, a seed, a script), and the version says so.
//!
//! **A rollback never writes the feed.** It adds one ordinary change to a draft -
//! the node's update or replace, carrying the old values of exactly the fields
//! that differ from the live node - through [`service::add_change_to`], so it is
//! shape-checked, based on the live row, validated (the fare rules included) and
//! audited like any other change, and goes live only when a second person
//! approves and commits the draft. Fields that change type cannot set (a stop's
//! station, a stage's direction) are listed as `not_restored`, never dropped
//! silently.

use super::auth::{self, Ctx, Role};
use super::error::{EditorError, EditorResult};
use super::feed_lock::{lock_feed, retry_transient};
use super::service::{self, ChangeRow, ChangeSet, NewChange, FIRST_PATTERN};
use super::stages;
use super::validation::{stage_numbers, StageLink};
use super::EditorState;
use crate::gtfs::spec::{FileSpec, Key};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, Row as _};
use uuid::Uuid;

/// The most versions one read lists, newest first.
pub const MAX_VERSIONS: usize = 200;

/// What a change carries besides the node's fields: never a field to compare or
/// restore.
const CONTROL: &[&str] = &[
    "base_rows_hash",
    "base_stages_hash",
    "base_trips_hash",
    "base_hash",
    "into_row_version",
    "position_review_id",
    "proposal_id",
    "pattern_key",
    "profile_key",
];

/// A stop's fields a rollback restores, through `stop/update`.
const STOP_FIELDS: &[&str] = &[
    "name",
    "lat",
    "lon",
    "platform_code",
    "description",
    "cluster_id",
    "regional_name",
    "hindi_name",
    "unserviceable",
    "tts_stop_name",
    "zone_id",
    "stop_url",
    "stop_timezone",
    "wheelchair_boarding",
    "level_id",
    "stop_access",
];

/// A stop's fields no `stop/update` of a rollback sets: which station it is a
/// platform of moves with the station's members, and a stop does not become a
/// station. Reported when they differ.
const STOP_NOT_RESTORED: &[(&str, &str)] = &[
    (
        "parent_station",
        "a stop's station is restored on the station's history",
    ),
    (
        "location_type",
        "a stop does not become a station, nor back",
    ),
];

const STATION_FIELDS: &[&str] = &["name", "lat", "lon", "description", "member_stop_ids"];

/// A route's fields a rollback restores, through `route/update`.
const ROUTE_FIELDS: &[&str] = &[
    "short_name",
    "long_name",
    "color",
    "text_color",
    "encoded_polyline",
    "polyline_source",
    "schedule_source",
    "agency_id",
    "route_type",
    "active",
    "route_tag",
    "route_desc",
    "route_url",
    "route_sort_order",
    "continuous_pickup",
    "continuous_drop_off",
    "network_id",
];

/// Fields a change refuses to set to null: an old snapshot holding null for one
/// is left as it is rather than sent.
const NEVER_NULL: &[&str] = &[
    "name",
    "schedule_source",
    "route_type",
    "active",
    "unserviceable",
];

/// The fields of a stop list row a `route_stops/replace` takes.
const STOP_LIST_ROW_FIELDS: &[&str] = &[
    "stop_id",
    "stop_type",
    "stage_no",
    "stage_name",
    "marker_id",
    "marker_name",
    "marker_lat",
    "marker_lon",
    "stop_name_override",
    "provider_id",
];

/// The fields of a stage row a `stage/update` takes.
const STAGE_ROW_FIELDS: &[&str] = &[
    "stop_id",
    "stop_type",
    "marker_id",
    "marker_name",
    "marker_lat",
    "marker_lon",
    "stop_name_override",
];

/// What a record's read carries besides its fields.
const RECORD_META: &[&str] = &["row_id", "row_version", "feed_id"];

#[derive(Clone, Copy)]
enum Kind {
    Stop,
    Station,
    Route,
    RouteStops,
    Stage,
    RouteStages,
    Record(&'static FileSpec),
}

/// One node, as its history and its rollback name it.
pub struct Node {
    kind: Kind,
    /// The change entity that edits it.
    entity: String,
    /// The change entity_key: a stage as `<id>|<direction>` when it has one.
    key: String,
    /// A stop list's stop order (section 16); 1 for everything else.
    pattern: i16,
    /// A stage's direction, "" for one that runs the same either way.
    direction: String,
}

impl Node {
    fn json(&self) -> Value {
        let mut v = json!({"entity": self.entity, "entity_key": self.key});
        if matches!(self.kind, Kind::RouteStops) {
            v["pattern_key"] = json!(self.pattern);
        }
        v
    }

    /// What the dashboard calls it: "stop CHNS00012", "route 2614's stop list".
    fn label(&self) -> String {
        match self.kind {
            Kind::Stop => format!("stop {}", self.key),
            Kind::Station => format!("station {}", self.key),
            Kind::Route => format!("route {}", self.key),
            Kind::RouteStops if self.pattern == FIRST_PATTERN => {
                format!("route {}'s stop list", self.key)
            }
            Kind::RouteStops => format!("route {}'s stop order {}", self.key, self.pattern),
            Kind::Stage => format!("stage {}", self.key),
            Kind::RouteStages => format!("route {}'s stages", self.key),
            Kind::Record(_) => format!("{} {}", self.entity, self.key),
        }
    }

    /// The op a rollback's change is.
    fn op(&self) -> &'static str {
        match self.kind {
            Kind::RouteStops | Kind::RouteStages => "replace",
            _ => "update",
        }
    }
}

/// The node a history or rollback names: `entity` is a change entity that edits
/// a node, `key` its id. A stage's bare id is resolved to its direction when only
/// one stage carries it.
pub async fn node(
    conn: &mut PgConnection,
    g: &str,
    entity: &str,
    key: &str,
    pattern: Option<i16>,
) -> EditorResult<Node> {
    let key = key.trim();
    if key.is_empty() {
        return Err(EditorError::bad_request(
            "entity_key_required",
            "name the node: its id",
        ));
    }
    let kind = match entity {
        "stop" => Kind::Stop,
        "station" => Kind::Station,
        "route" => Kind::Route,
        "route_stops" => Kind::RouteStops,
        "stage" => Kind::Stage,
        "route_stages" => Kind::RouteStages,
        other => match super::records::spec_for(other) {
            Some(fspec) => Kind::Record(fspec),
            None => {
                return Err(EditorError::bad_request(
                    "not_a_node",
                    format!(
                        "{other:?} has no versions here: a node is a stop, station, route, \
                         route_stops, stage, route_stages or a record file's row"
                    ),
                ))
            }
        },
    };
    let (key, direction) = match kind {
        Kind::Stage => {
            let sk = stages::key_of_change(conn, g, key).await;
            let full = if sk.direction.is_empty() {
                sk.stage_id.clone()
            } else {
                format!("{}|{}", sk.stage_id, sk.direction)
            };
            (full, sk.direction)
        }
        _ => (key.to_string(), String::new()),
    };
    Ok(Node {
        kind,
        entity: entity.to_string(),
        key,
        pattern: match kind {
            Kind::RouteStops => pattern.unwrap_or(FIRST_PATTERN),
            _ => FIRST_PATTERN,
        },
        direction,
    })
}

/// One committed change that touched a node.
struct Version {
    change_id: i64,
    position: i32,
    change_set_id: Uuid,
    title: String,
    feed_version: i64,
    committed_at: Option<chrono::DateTime<chrono::Utc>>,
    committed_by: Option<String>,
    entity: String,
    entity_key: String,
    op: String,
    before: Value,
    after: Value,
    direct: bool,
}

impl Version {
    fn from_row(r: &sqlx::postgres::PgRow, direct: bool) -> Result<Version, sqlx::Error> {
        Ok(Version {
            change_id: r.try_get("change_id")?,
            position: r.try_get("position")?,
            change_set_id: r.try_get("change_set_id")?,
            title: r.try_get("title")?,
            feed_version: r.try_get("committed_version")?,
            committed_at: r.try_get("committed_at")?,
            committed_by: r.try_get("committed_by")?,
            entity: r.try_get("entity")?,
            entity_key: r.try_get("entity_key")?,
            op: r.try_get("op")?,
            before: service::json_col(r, "before")?,
            after: service::json_col(r, "after")?,
            direct,
        })
    }
}

const VERSION_COLS: &str = "c.change_id, c.position, c.entity, c.entity_key, c.op, \
     c.before::text AS before, c.after::text AS after, s.change_set_id, s.title, \
     s.committed_version, s.committed_at, u.email AS committed_by";

const VERSION_FROM: &str = "FROM gtfs_change c \
     JOIN gtfs_change_set s ON s.change_set_id = c.change_set_id \
     LEFT JOIN gtfs_editor_user u ON u.user_id = s.committed_by \
     WHERE s.gtfs_id = $1 AND s.status = 'committed' AND s.committed_version IS NOT NULL";

/// Whether a change of the node's own entity and key is this node's: the right
/// stop order of a route, the right direction of a stage.
fn is_own(node: &Node, entity: &str, key: &str, before: &Value, after: &Value) -> bool {
    if entity != node.entity {
        return false;
    }
    match node.kind {
        Kind::RouteStops => key == node.key && service::pattern_of(after) == node.pattern,
        Kind::Stage => {
            let id = node.key.split('|').next().unwrap_or("");
            if key.contains('|') {
                return key == node.key;
            }
            if key != id {
                return false;
            }
            // a bare id: the stage it named then, by the direction it recorded
            let recorded = before["direction"]
                .as_str()
                .or_else(|| before["from"]["direction"].as_str())
                .or_else(|| after["direction"].as_str())
                .unwrap_or("");
            recorded == node.direction || recorded.is_empty()
        }
        _ => key == node.key,
    }
}

/// The SQL condition (on `c`, with the node's key as `$2`) of the changes that
/// touched a node without being its own, and the words a version uses for one.
fn indirect_condition(node: &Node) -> Option<&'static str> {
    match node.kind {
        Kind::Stop | Kind::Station => Some(
            "((c.entity IN ('stop', 'station') AND c.op = 'merge' AND c.entity_key <> $2 \
               AND (c.after ->> 'into_stop_id' = $2 OR c.after ->> 'into_station_id' = $2)) \
              OR (c.entity = 'station' AND c.entity_key <> $2 \
               AND (jsonb_exists(coalesce(c.after -> 'member_stop_ids', '[]'), $2) \
                    OR jsonb_exists(coalesce(c.before -> 'member_stop_ids', '[]'), $2) \
                    OR EXISTS (SELECT 1 FROM jsonb_array_elements( \
                                  CASE WHEN jsonb_typeof(c.after -> 'members') = 'array' \
                                       THEN c.after -> 'members' ELSE '[]' END) m \
                               WHERE m ->> 'stop_id' = $2 OR m #>> '{}' = $2))))",
        ),
        Kind::RouteStops => Some(
            "((c.entity = 'stop' AND c.op = 'merge' \
               AND EXISTS (SELECT 1 FROM jsonb_array_elements( \
                              CASE WHEN jsonb_typeof(c.before -> 'affected') = 'array' \
                                   THEN c.before -> 'affected' ELSE '[]' END) a \
                           WHERE a ->> 'route_id' = $2)) \
              OR (c.entity IN ('route_variant', 'route_stages') AND c.entity_key = $2) \
              OR (c.entity = 'stage' AND c.op = 'split' \
               AND jsonb_exists(coalesce(c.after -> 'routes', '[]'), $2)))",
        ),
        Kind::Stage => Some(
            "(c.entity = 'stage' AND c.op IN ('merge', 'split') \
              AND (split_part(c.after ->> 'into_stage_id', '|', 1) = split_part($2, '|', 1) \
                   OR split_part(c.after ->> 'from_stage_id', '|', 1) = split_part($2, '|', 1)) \
              AND split_part(c.entity_key, '|', 1) <> split_part($2, '|', 1))",
        ),
        Kind::RouteStages => Some(
            "((c.entity = 'stage' AND c.op = 'split' \
               AND jsonb_exists(coalesce(c.after -> 'routes', '[]'), $2)) \
              OR (c.entity = 'route_variant' AND c.entity_key = $2))",
        ),
        Kind::Route | Kind::Record(_) => None,
    }
}

/// Every committed change that touched the node, oldest first.
async fn versions(conn: &mut PgConnection, g: &str, node: &Node) -> EditorResult<Vec<Version>> {
    // its own: by the (entity, entity_key) index; a stage's bare-id changes too
    let id_only = node.key.split('|').next().unwrap_or("").to_string();
    let own_sql = format!(
        "SELECT {VERSION_COLS} {VERSION_FROM} \
         AND c.entity = $2 AND (c.entity_key = $3 OR c.entity_key = $4)"
    );
    let mut out: Vec<Version> = Vec::new();
    for r in sqlx::query(&own_sql)
        .bind(g)
        .bind(&node.entity)
        .bind(&node.key)
        .bind(&id_only)
        .fetch_all(&mut *conn)
        .await?
    {
        let v = Version::from_row(&r, true)?;
        if is_own(node, &v.entity, &v.entity_key, &v.before, &v.after) {
            out.push(v);
        }
    }
    if let Some(cond) = indirect_condition(node) {
        let sql = format!("SELECT {VERSION_COLS} {VERSION_FROM} AND {cond}");
        for r in sqlx::query(&sql)
            .bind(g)
            .bind(&node.key)
            .fetch_all(&mut *conn)
            .await?
        {
            let v = Version::from_row(&r, false)?;
            if !out.iter().any(|o| o.change_id == v.change_id) {
                out.push(v);
            }
        }
    }
    out.sort_by_key(|v| (v.feed_version, v.position, v.change_id));
    Ok(out)
}

// ---------------------------------------------------------------- states

/// A station's members as sorted ids, from either shape a change gives them in.
fn member_ids(v: &Value) -> Option<Value> {
    let ids: Option<Vec<String>> = v["member_stop_ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .or_else(|| {
            v["members"].as_array().map(|a| {
                a.iter()
                    .filter_map(|m| {
                        m.as_str()
                            .map(str::to_string)
                            .or_else(|| m["stop_id"].as_str().map(str::to_string))
                    })
                    .collect()
            })
        });
    ids.map(|mut ids| {
        ids.sort();
        json!(ids)
    })
}

/// A field of a node as a comparison sees it: the field itself, or under `gtfs`
/// where a stop or route keeps the rest of its GTFS fields; a station's members
/// as sorted ids. None when the snapshot does not carry it.
fn field(snap: &Value, f: &str) -> Option<Value> {
    if f == "member_stop_ids" {
        return member_ids(snap);
    }
    snap.get(f)
        .or_else(|| snap.get("gtfs").and_then(|g| g.get(f)))
        .cloned()
}

/// Lay a change's `after` over the node it was made to: the node just after it.
fn overlay(base: &Value, after: &Value) -> Value {
    let mut out = base.clone();
    if let (Some(o), Some(a)) = (out.as_object_mut(), after.as_object()) {
        for (k, v) in a {
            if CONTROL.contains(&k.as_str()) {
                continue;
            }
            if k == "members" || k == "member_stop_ids" {
                if let Some(ids) = member_ids(after) {
                    o.insert("member_stop_ids".into(), ids);
                }
                continue;
            }
            o.insert(k.clone(), v.clone());
            if let Some(g) = o.get_mut("gtfs").and_then(Value::as_object_mut) {
                if g.contains_key(k) {
                    g.insert(k.clone(), v.clone());
                }
            }
        }
    }
    out
}

/// The node just before version `v`, when the version is its own and records it.
fn state_before(node: &Node, v: &Version) -> Option<Value> {
    if !v.direct || v.op == "create" || v.before.is_null() {
        return None;
    }
    match node.kind {
        Kind::RouteStops => v.before.as_array().map(|rows| json!({"rows": rows})),
        Kind::RouteStages => v.before.as_array().map(|s| json!({"stages": s})),
        // a stop or station merge records both sides; the node is the one merged away
        _ if v.before["from"].is_object() => Some(v.before["from"].clone()),
        _ => v.before.is_object().then(|| v.before.clone()),
    }
}

/// The node just after version `v`, or why the version is none to restore to.
fn state_after(node: &Node, v: &Version) -> Result<Value, String> {
    if !v.direct {
        return Err(match (v.entity.as_str(), v.op.as_str()) {
            ("stop" | "station", "merge") => format!(
                "{} {} was merged in this version; its change records only what moved",
                v.entity, v.entity_key
            ),
            ("station", _) => format!(
                "station {} took or let go of this stop as a platform",
                v.entity_key
            ),
            ("stage", "split") => format!("stage {} was split in this version", v.entity_key),
            ("stage", "merge") => format!("stage {} was merged in this version", v.entity_key),
            ("route_variant", _) => "a diversion was switched in this version".to_string(),
            ("route_stages", _) => {
                "the route's stages changed in this version; restore them on the route's stages"
                    .to_string()
            }
            (e, o) => format!("changed by a {e}/{o} change, which does not record it whole"),
        });
    }
    match (node.kind, v.op.as_str()) {
        (_, "delete") => Err("deleted in this version".to_string()),
        (_, "merge") => Err(format!(
            "merged into {} in this version",
            v.after["into_stop_id"]
                .as_str()
                .or_else(|| v.after["into_station_id"].as_str())
                .or_else(|| v.after["into_stage_id"].as_str())
                .unwrap_or("another")
        )),
        (_, "split") => Err("split in this version".to_string()),
        (Kind::RouteStops, "replace") => match v.after["rows"].as_array() {
            Some(rows) => Ok(json!({"rows": rows})),
            None => Err("the change records no rows".to_string()),
        },
        (Kind::RouteStages, "replace") => match v.after["stages"].as_array() {
            Some(s) => Ok(json!({"stages": s})),
            None => Err("the change records no stages".to_string()),
        },
        (_, "create") if v.after.is_object() => {
            let mut made = v.after.clone();
            if let Some(ids) = member_ids(&v.after) {
                made["member_stop_ids"] = ids;
            }
            Ok(made)
        }
        (_, "update") => match state_before(node, v) {
            Some(base) => Ok(overlay(&base, &v.after)),
            None => Err("the change records no node to lay it over".to_string()),
        },
        (_, op) => Err(format!("a {op} change is no version to restore")),
    }
}

// ---------------------------------------------------------------- comparing

/// One field a rollback cannot set, and why.
fn not_restored(f: &str, why: &str, target: Value, live: Value) -> Value {
    json!({"field": f, "reason": why, "version": target, "live": live})
}

/// A row of a list, picked to the fields a change of it takes, without nulls.
fn pick_row(row: &Value, fields: &[&str]) -> Value {
    let mut out = Map::new();
    for f in fields {
        if let Some(v) = row.get(*f).filter(|v| !v.is_null()) {
            out.insert(f.to_string(), v.clone());
        }
    }
    Value::Object(out)
}

/// Whether two lists of rows are the same on every field the old one carries (a
/// snapshot older than a field cannot say what it was).
fn rows_same(old: &[Value], live: &[Value], fields: &[&str]) -> bool {
    old.len() == live.len()
        && old.iter().zip(live).all(|(o, l)| {
            fields.iter().all(|f| match o.get(*f) {
                None => true,
                Some(v) => v == l.get(*f).unwrap_or(&Value::Null),
            })
        })
}

/// A stage list, as links, from either shape a version holds it in: a change's
/// `stages`, or a `before`'s `{position, stage_id, stage_no, name}`.
fn links_of(stages: &[Value]) -> Vec<StageLink> {
    stages
        .iter()
        .map(|s| {
            let raw = s["stage_id"].as_str().unwrap_or("").to_string();
            let (id, dir) = match raw.split_once('|') {
                Some((id, d)) => (id.to_string(), Some(d.to_string())),
                None => (raw.clone(), None),
            };
            StageLink {
                stage_id: id,
                direction: s["direction"]
                    .as_str()
                    .map(str::to_string)
                    .or(dir)
                    .filter(|d| !d.is_empty()),
                stage_no: s["stage_no"].as_i64().map(|n| n as i32),
            }
        })
        .collect()
}

/// Whether an old stage list is the live one: the same stages in the same order
/// with the same fare stage numbers, and the same direction wherever the old one
/// says which.
fn links_same(old: &[StageLink], live: &[StageLink]) -> bool {
    let (no_old, no_live) = (stage_numbers(old), stage_numbers(live));
    old.len() == live.len()
        && old.iter().zip(live).enumerate().all(|(i, (o, l))| {
            o.stage_id == l.stage_id
                && no_old[i] == no_live[i]
                && match (&o.direction, &l.direction) {
                    (Some(a), Some(b)) => a == b,
                    (Some(a), None) => a.is_empty(),
                    _ => true,
                }
        })
}

/// What restoring `target` over `live` changes: the change's `after` (only what
/// differs; empty when nothing does), and the fields it cannot set. `both`
/// compares only fields both sides carry - for telling whether two snapshots of
/// different ages agree, rather than for writing.
fn difference(
    node: &Node,
    target: &Value,
    live: &Value,
    both: bool,
) -> (Map<String, Value>, Vec<Value>) {
    let mut after = Map::new();
    let mut skipped = Vec::new();
    let fields_of = |fields: &[&str], after: &mut Map<String, Value>| {
        for f in fields {
            let Some(want) = field(target, f) else {
                continue;
            };
            let have = field(live, f);
            if both && have.is_none() {
                continue;
            }
            if want.is_null() && NEVER_NULL.contains(f) {
                continue;
            }
            if Some(&want) != have.as_ref() {
                after.insert(f.to_string(), want);
            }
        }
    };
    match node.kind {
        Kind::Stop => {
            fields_of(STOP_FIELDS, &mut after);
            // a position is moved whole
            if after.contains_key("lat") || after.contains_key("lon") {
                for f in ["lat", "lon"] {
                    if let Some(v) = field(target, f) {
                        after.insert(f.into(), v);
                    }
                }
            }
            for (f, why) in STOP_NOT_RESTORED {
                if let (Some(want), Some(have)) = (field(target, f), field(live, f)) {
                    if want != have {
                        skipped.push(not_restored(f, why, want, have));
                    }
                }
            }
        }
        Kind::Station => {
            fields_of(STATION_FIELDS, &mut after);
            if after.contains_key("lat") || after.contains_key("lon") {
                for f in ["lat", "lon"] {
                    if let Some(v) = field(target, f) {
                        after.insert(f.into(), v);
                    }
                }
            }
        }
        Kind::Route => fields_of(ROUTE_FIELDS, &mut after),
        Kind::RouteStops => {
            let old: Vec<Value> = target["rows"]
                .as_array()
                .map(|r| {
                    r.iter()
                        .map(|r| pick_row(r, STOP_LIST_ROW_FIELDS))
                        .collect()
                })
                .unwrap_or_default();
            let now: Vec<Value> = live["rows"]
                .as_array()
                .map(|r| {
                    r.iter()
                        .map(|r| pick_row(r, STOP_LIST_ROW_FIELDS))
                        .collect()
                })
                .unwrap_or_default();
            if !rows_same(&old, &now, STOP_LIST_ROW_FIELDS) {
                after.insert("rows".into(), json!(old));
            }
        }
        Kind::Stage => {
            for f in ["name", "description"] {
                let Some(want) = field(target, f) else {
                    continue;
                };
                if want.is_null() && f == "name" {
                    continue;
                }
                let have = field(live, f);
                if (both && have.is_none()) || Some(&want) == have.as_ref() {
                    continue;
                }
                after.insert(f.into(), want);
            }
            if let Some(rows) = target["rows"].as_array() {
                let old: Vec<Value> = rows.iter().map(|r| pick_row(r, STAGE_ROW_FIELDS)).collect();
                let now: Vec<Value> = live["rows"]
                    .as_array()
                    .map(|r| r.iter().map(|r| pick_row(r, STAGE_ROW_FIELDS)).collect())
                    .unwrap_or_default();
                // two snapshots compare rows only when both carry them
                let unknown = both && live["rows"].is_null();
                if !(unknown || rows_same(&old, &now, STAGE_ROW_FIELDS)) {
                    after.insert("rows".into(), json!(old));
                }
            }
            if let (Some(want), Some(have)) =
                (target["direction"].as_str(), live["direction"].as_str())
            {
                if want != have {
                    skipped.push(not_restored(
                        "direction",
                        "a stage keeps its direction: it is part of its key",
                        json!(want),
                        json!(have),
                    ));
                }
            }
        }
        Kind::RouteStages => {
            let old = links_of(
                target["stages"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            );
            let now = links_of(live["stages"].as_array().map(Vec::as_slice).unwrap_or(&[]));
            if !links_same(&old, &now) {
                after.insert(
                    "stages".into(),
                    serde_json::to_value(&old).unwrap_or(Value::Null),
                );
            }
        }
        Kind::Record(fspec) => {
            let key_field = match fspec.key() {
                Some(Key::Field(f)) => Some(f),
                _ => None,
            };
            let mut names: Vec<&str> = fspec.fields.iter().map(|f| f.name).collect();
            names.push("points"); // a shape's points, replaced whole
            for f in names {
                if Some(f) == key_field || RECORD_META.contains(&f) {
                    continue;
                }
                let Some(want) = target.get(f) else { continue };
                let have = live.get(f);
                if both && have.is_none() {
                    continue;
                }
                if Some(want) != have {
                    after.insert(f.to_string(), want.clone());
                }
            }
        }
    }
    (after, skipped)
}

/// The node as it is now, in the shape its versions are, and whether it is
/// still there to restore (a deleted stop or route is not).
async fn live(
    conn: &mut PgConnection,
    set_id: Option<Uuid>,
    g: &str,
    node: &Node,
) -> EditorResult<(Value, bool)> {
    match node.kind {
        Kind::RouteStops => {
            // the rows in read shape, as the versions' `before` holds them
            let mut after = json!({"pattern_key": node.pattern});
            if !route_live(conn, g, &node.key).await? {
                return Ok((Value::Null, false));
            }
            let (rows, _) = service::snapshot(
                conn,
                set_id.unwrap_or(Uuid::nil()),
                g,
                "route_stops",
                "replace",
                &node.key,
                &mut after,
            )
            .await?;
            Ok((json!({"rows": rows}), true))
        }
        Kind::RouteStages => {
            if !route_live(conn, g, &node.key).await? {
                return Ok((Value::Null, false));
            }
            let links = stages::route_links(conn, g, &node.key, None).await?;
            Ok((json!({"stages": links}), true))
        }
        _ => {
            let mut after = Value::Null;
            let found = service::snapshot(
                conn,
                set_id.unwrap_or(Uuid::nil()),
                g,
                &node.entity,
                "update",
                &node.key,
                &mut after,
            )
            .await;
            match found {
                Ok((row, _)) => {
                    let gone = row["deleted"].as_bool().unwrap_or(false);
                    Ok((row, !gone))
                }
                Err(e) if e.code == "entity_not_found" => Ok((Value::Null, false)),
                Err(e) => Err(e),
            }
        }
    }
}

/// Whether route `id` is there and not deleted.
async fn route_live(conn: &mut PgConnection, g: &str, id: &str) -> EditorResult<bool> {
    Ok(service::route_row(conn, g, id)
        .await?
        .is_some_and(|r| !r["deleted"].as_bool().unwrap_or(false)))
}

// ---------------------------------------------------------------- the read

/// `GET /feeds/{g}/history/{entity}/{key}`: the node now, and every committed
/// change that touched it, newest first, each with the node just after it where
/// it records that, and whether it can be restored.
pub async fn history(
    state: &EditorState,
    g: &str,
    entity: &str,
    key: &str,
    pattern: Option<i16>,
) -> EditorResult<Value> {
    let mut conn = state.pool.acquire().await?;
    let node = node(&mut conn, g, entity, key, pattern).await?;
    let all = versions(&mut conn, g, &node).await?;
    let (now, exists) = live(&mut conn, None, g, &node).await?;

    let mut listed = Vec::with_capacity(all.len());
    for (i, v) in all.iter().enumerate() {
        let made = state_after(&node, v);
        // the node this version left, set beside what the next version found
        // (or the live node, after the last): a difference with nothing listed
        // between them is a change this history does not see
        let unrecorded = match (&made, all.get(i + 1)) {
            (Ok(s), Some(next)) => state_before(&node, next).is_some_and(|b| {
                let (d, _) = difference(&node, s, &b, true);
                !d.is_empty()
            }),
            (Ok(s), None) if exists => {
                let (d, _) = difference(&node, s, &now, true);
                !d.is_empty()
            }
            _ => false,
        };
        let restorable = made.is_ok() && exists;
        listed.push(json!({
            "change_id": v.change_id,
            "change_set_id": v.change_set_id,
            "title": v.title,
            "feed_version": v.feed_version,
            "committed_at": v.committed_at,
            "committed_by": v.committed_by,
            "entity": v.entity,
            "entity_key": v.entity_key,
            "op": v.op,
            "direct": v.direct,
            "before": v.before,
            "after": v.after,
            "state": made.as_ref().ok(),
            "restorable": restorable,
            "why_not": match (&made, exists) {
                (Err(why), _) => Some(why.clone()),
                (Ok(_), false) => Some("the node is gone: deleted or merged away".to_string()),
                _ => None,
            },
            "is_live": made.as_ref().is_ok_and(|s| exists && difference(&node, s, &now, false).0.is_empty()),
            "unrecorded_change_after": unrecorded,
        }));
    }
    // as it was before its first recorded edit, where that edit records it
    let original = all
        .iter()
        .find(|v| v.direct)
        .and_then(|v| state_before(&node, v).map(|s| (v, s)))
        .map(|(v, s)| {
            json!({
                "change_id": v.change_id,
                "feed_version": v.feed_version,
                "state": s,
                "restorable": exists,
                "is_live": exists && difference(&node, &s, &now, false).0.is_empty(),
            })
        });
    let total = listed.len();
    listed.reverse();
    listed.truncate(MAX_VERSIONS);
    Ok(json!({
        "node": node.json(),
        "label": node.label(),
        "exists": exists,
        "live": now,
        "versions": listed,
        "total": total,
        "truncated": total > MAX_VERSIONS,
        "original": original,
    }))
}

// ---------------------------------------------------------------- rollback

#[derive(Debug, Clone, Deserialize)]
pub struct RollbackRequest {
    /// The version: a committed change that is the node's own.
    pub change_id: i64,
    /// `after` (the default): the node as that version left it; `before`: as
    /// it was just before it - undoing that one version.
    #[serde(default)]
    pub side: Option<String>,
    /// A draft of the feed to add the change to; a new draft when left out.
    #[serde(default)]
    pub change_set_id: Option<Uuid>,
    /// The new draft's title; "Roll back <node> to v<version>" when left out.
    #[serde(default)]
    pub title: Option<String>,
    /// A stop list's stop order; 1 when left out.
    #[serde(default)]
    pub pattern_key: Option<i16>,
}

/// `POST /feeds/{g}/history/{entity}/{key}/rollback`: one change into a draft
/// that puts the node back as a version had it. Editor of the feed.
pub async fn rollback(
    state: &EditorState,
    ctx: &Ctx,
    g: &str,
    entity: &str,
    key: &str,
    req: RollbackRequest,
) -> EditorResult<Value> {
    ctx.require_feed_role(g, Role::Editor)?;
    retry_transient(|| rollback_once(state, ctx, g, entity, key, req.clone())).await
}

async fn rollback_once(
    state: &EditorState,
    ctx: &Ctx,
    g: &str,
    entity: &str,
    key: &str,
    req: RollbackRequest,
) -> EditorResult<Value> {
    let side = req.side.as_deref().unwrap_or("after");
    if side != "after" && side != "before" {
        return Err(EditorError::bad_request(
            "invalid_side",
            "side is \"after\" (the node as the version left it) or \"before\" (as it was just before it)",
        ));
    }
    let mut tx = state.pool.begin().await?;
    lock_feed(&mut tx, g).await?;
    let node = node(&mut tx, g, entity, key, req.pattern_key).await?;

    // the version, and the node it had
    let all = versions(&mut tx, g, &node).await?;
    let Some(v) = all.iter().find(|v| v.change_id == req.change_id) else {
        return Err(EditorError::not_found(
            "version_not_found",
            format!(
                "change {} is no committed version of {}",
                req.change_id,
                node.label()
            ),
        ));
    };
    let target = if side == "before" {
        state_before(&node, v).ok_or_else(|| {
            EditorError::conflict(
                "not_restorable",
                format!(
                    "v{} of {} records nothing from before it (it made the {} or touched it in passing)",
                    v.feed_version,
                    node.label(),
                    node.entity
                ),
            )
        })?
    } else {
        state_after(&node, v).map_err(|why| {
            EditorError::conflict(
                "not_restorable",
                format!(
                    "v{} of {} cannot be restored: {why}",
                    v.feed_version,
                    node.label()
                ),
            )
        })?
    };
    let (version, change_set_id) = (v.feed_version, v.change_set_id);

    // the draft: one given, or a new one
    let (set, created) = match req.change_set_id {
        Some(id) => {
            let set = service::load_set(&mut tx, id, true).await?;
            if set.gtfs_id != g {
                return Err(EditorError::not_found(
                    "change_set_not_found",
                    format!("no draft {id} of feed {g}"),
                ));
            }
            service::editable(&set)?;
            (set, false)
        }
        None => {
            let title = req
                .title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if side == "before" {
                        format!("Roll back {} to before v{version}", node.label())
                    } else {
                        format!("Roll back {} to v{version}", node.label())
                    }
                });
            (
                new_set(&mut tx, ctx, g, &title, &node, version).await?,
                true,
            )
        }
    };
    // one change per node in a draft: a second would be based on the first
    let pending: Vec<ChangeRow> = service::load_changes(&mut tx, set.change_set_id)
        .await?
        .into_iter()
        .filter(|c| is_own(&node, &c.entity, &c.entity_key, &c.before, &c.after))
        .collect();
    if let Some(c) = pending.first() {
        return Err(EditorError::conflict(
            "node_in_draft",
            format!(
                "this draft already changes {} (change {}); remove that change or use another draft",
                node.label(),
                c.change_id
            ),
        )
        .with_details(json!({"change_id": c.change_id})));
    }

    let (now, exists) = live(&mut tx, Some(set.change_set_id), g, &node).await?;
    if !exists {
        return Err(EditorError::conflict(
            "node_gone",
            format!(
                "{} is deleted or merged away; a rollback edits a live node",
                node.label()
            ),
        ));
    }
    let (mut after, skipped) = difference(&node, &target, &now, false);
    if after.is_empty() {
        return Err(EditorError::conflict(
            "nothing_to_roll_back",
            format!(
                "{} already has what v{version} {} it{}",
                node.label(),
                if side == "before" { "found" } else { "left" },
                if skipped.is_empty() {
                    String::new()
                } else {
                    " in every field a rollback sets".to_string()
                }
            ),
        )
        .with_details(json!({"not_restored": skipped})));
    }
    let restored: Vec<String> = after.keys().cloned().collect();
    // what each kind's change needs besides the fields: the base it replaces
    match node.kind {
        Kind::RouteStops => {
            let rows = service::load_pattern_rows(&mut tx, g, &node.key, node.pattern).await?;
            after.insert("base_rows_hash".into(), json!(service::rows_hash(&rows)));
            if node.pattern != FIRST_PATTERN {
                after.insert("pattern_key".into(), json!(node.pattern));
            }
        }
        Kind::RouteStages => {
            let hash = stages::live_links_hash(&mut tx, g, &node.key, None).await?;
            after.insert("base_stages_hash".into(), json!(hash));
        }
        _ => {}
    }
    let change_id = service::add_change_to(
        &mut tx,
        ctx,
        &set,
        NewChange {
            entity: node.entity.clone(),
            op: node.op().to_string(),
            entity_key: node.key.clone(),
            after: Value::Object(after),
            base_row_version: None,
        },
    )
    .await?;
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "node_rolled_back",
        Some(g),
        Some(set.change_set_id),
        json!({
            "change_id": change_id,
            "node": node.json(),
            "to_change_id": req.change_id,
            "to_change_set_id": change_set_id,
            "to_feed_version": version,
            "side": side,
            "fields": restored,
            "not_restored": skipped,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(json!({
        "change_set_id": set.change_set_id,
        "created_set": created,
        "change_id": change_id,
        "node": node.json(),
        "to_feed_version": version,
        "side": side,
        "fields": restored,
        "not_restored": skipped,
    }))
}

/// A new draft of the caller's for a rollback, based on the feed as it is.
async fn new_set(
    conn: &mut PgConnection,
    ctx: &Ctx,
    g: &str,
    title: &str,
    node: &Node,
    version: i64,
) -> EditorResult<ChangeSet> {
    let base = service::feed_version(conn, g).await?;
    let description = format!(
        "Puts {} back as feed version {version} had it. Made from its history.",
        node.label()
    );
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO gtfs_change_set (gtfs_id, title, description, created_by, base_version) \
         VALUES ($1, $2, $3, $4, $5) RETURNING change_set_id",
    )
    .bind(g)
    .bind(title)
    .bind(&description)
    .bind(ctx.user.user_id)
    .bind(base)
    .fetch_one(&mut *conn)
    .await?;
    auth::audit(
        &mut *conn,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "change_set_created",
        Some(g),
        Some(id),
        json!({"title": title}),
    )
    .await?;
    service::load_set(conn, id, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop_node() -> Node {
        Node {
            kind: Kind::Stop,
            entity: "stop".into(),
            key: "S1".into(),
            pattern: 1,
            direction: String::new(),
        }
    }

    #[test]
    fn a_stop_update_lays_over_its_before() {
        let before = json!({"stop_id": "S1", "name": "A", "lat": 13.0, "lon": 80.0,
                            "gtfs": {"zone_id": "z1"}});
        let after = json!({"name": "B", "zone_id": "z2", "position_review_id": 7});
        let s = overlay(&before, &after);
        assert_eq!(s["name"], "B");
        assert_eq!(field(&s, "zone_id"), Some(json!("z2")));
        assert!(s.get("position_review_id").is_none());
    }

    #[test]
    fn a_rollback_sends_only_what_differs_and_moves_a_position_whole() {
        let n = stop_node();
        let target = json!({"name": "A", "lat": 13.0, "lon": 80.0, "description": null});
        let live = json!({"name": "A", "lat": 13.5, "lon": 80.0, "description": null,
                          "parent_station": "ST1"});
        let (after, skipped) = difference(&n, &target, &live, false);
        assert_eq!(after.len(), 2);
        assert_eq!(after["lat"], json!(13.0));
        assert_eq!(after["lon"], json!(80.0));
        assert!(
            skipped.is_empty(),
            "the old snapshot says nothing of its station"
        );
    }

    #[test]
    fn a_field_the_old_snapshot_lacks_is_left_alone() {
        let n = stop_node();
        let target = json!({"name": "A"});
        let live = json!({"name": "A", "gtfs": {"zone_id": "z9"}});
        assert!(difference(&n, &target, &live, false).0.is_empty());
    }

    #[test]
    fn a_stops_station_is_reported_not_restored() {
        let n = stop_node();
        let target = json!({"name": "A", "parent_station": null});
        let live = json!({"name": "A", "parent_station": "ST1"});
        let (after, skipped) = difference(&n, &target, &live, false);
        assert!(after.is_empty());
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["field"], "parent_station");
    }

    #[test]
    fn stop_lists_compare_on_the_fields_the_old_rows_carry() {
        let n = Node {
            kind: Kind::RouteStops,
            entity: "route_stops".into(),
            key: "R1".into(),
            pattern: 1,
            direction: String::new(),
        };
        // an old snapshot without provider_id, the live rows with one
        let target = json!({"rows": [
            {"stop_id": "A", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "A", "stop_name": "Aa"},
            {"stop_id": "B", "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "A"}]});
        let same = json!({"rows": [
            {"stop_id": "A", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "A", "provider_id": "p"},
            {"stop_id": "B", "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "A", "provider_id": "p"}]});
        assert!(difference(&n, &target, &same, false).0.is_empty());
        let moved = json!({"rows": [
            {"stop_id": "A", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "A"},
            {"stop_id": "C", "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "A"}]});
        let (after, _) = difference(&n, &target, &moved, false);
        let rows = after["rows"].as_array().unwrap();
        assert_eq!(rows[1]["stop_id"], "B");
        assert!(
            rows[0].get("stop_name").is_none(),
            "read-only fields are not sent"
        );
    }

    #[test]
    fn stage_lists_compare_by_numbered_stages() {
        let n = Node {
            kind: Kind::RouteStages,
            entity: "route_stages".into(),
            key: "R1".into(),
            pattern: 1,
            direction: String::new(),
        };
        // a before snapshot: no direction; the live links: directions
        let target = json!({"stages": [
            {"position": 1, "stage_id": "10", "stage_no": 1, "name": "X"},
            {"position": 2, "stage_id": "11", "stage_no": 2, "name": "Y"}]});
        let live = json!({"stages": [
            {"stage_id": "10", "direction": "up", "stage_no": 1},
            {"stage_id": "11", "direction": "up", "stage_no": 2}]});
        assert!(difference(&n, &target, &live, false).0.is_empty());
        let other = json!({"stages": [
            {"stage_id": "10", "direction": "up", "stage_no": 1},
            {"stage_id": "12", "direction": "up", "stage_no": 2}]});
        assert!(difference(&n, &target, &other, false)
            .0
            .contains_key("stages"));
    }

    #[test]
    fn a_station_update_with_members_sets_sorted_ids() {
        let before = json!({"stop_id": "ST", "name": "S", "member_stop_ids": ["b", "a"]});
        let after = json!({"members": [{"stop_id": "c"}, {"stop_id": "a"}]});
        let s = overlay(&before, &after);
        assert_eq!(s["member_stop_ids"], json!(["a", "c"]));
    }
}
