//! What a draft does to the live tables, read from its changes without applying
//! them.
//!
//! [`super::service::evaluate`] is the authority on whether a draft applies: it
//! runs every change against the database, one savepoint each. A bulk import or
//! a batch of station proposals needs a batch answer instead - does this stop
//! exist once the draft applies, is it a station, which station is it in, what
//! are this route's rows - for thousands of rows at once. This reads the
//! draft's changes in position order and takes each one as applying cleanly (a
//! draft whose changes do not apply cannot be submitted, and its own validation
//! says why).

use super::error::EditorResult;
use super::validation::{station_members, MemberSpec, RouteRow};
use serde_json::Value;
use sqlx::{PgConnection, Row};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// A stop or station an earlier change in the draft creates.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatedStop {
    pub change_id: i64,
    pub location_type: i16,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone)]
enum StationOp {
    /// station/create or station/update: `members` join the station; an update
    /// with a member list also releases the station's other stops. `labels`
    /// holds the platform label of each member that sends one.
    Members {
        station: String,
        members: HashSet<String>,
        releases_others: bool,
        labels: HashMap<String, Option<String>>,
    },
    /// station/delete releases every member.
    Delete { station: String },
    /// stop/delete clears the stop's parent.
    StopDelete { stop: String },
    /// stop/merge: the kept stop takes the parent the merged stop had when it
    /// has none (and then its platform label); the merged stop leaves its
    /// station. `keep_name_from`: the kept stop takes the merged stop's name.
    Merge {
        from: String,
        into: String,
        keep_name_from: bool,
    },
    /// station/merge: every platform of the station that goes moves onto the
    /// one that stays, keeping its own label; the station that goes is deleted.
    /// `keep_name_from`: the kept station takes the other's name.
    StationMerge {
        from: String,
        into: String,
        keep_name_from: bool,
    },
    /// A create or update of a stop or station: the texts it sets. A station
    /// never sets a platform label.
    Texts {
        stop: String,
        creates: bool,
        name: Option<String>,
        platform_code: Option<Option<String>>,
        description: Option<Option<String>>,
    },
}

/// The texts of a stop a passenger reads, and its station: what
/// [`DraftView::texts_after`] follows through a draft.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StopTexts {
    pub name: String,
    pub platform_code: Option<String>,
    pub description: Option<String>,
    pub parent_station: Option<String>,
}

/// A label or description as the apply stores it: trimmed, blank is none.
fn stored_text(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn texts_op(stop: &str, creates: bool, a: &Value) -> StationOp {
    let given = |k: &str| a.get(k).map(stored_text);
    StationOp::Texts {
        stop: stop.to_string(),
        creates,
        name: a
            .get("name")
            .and_then(Value::as_str)
            .map(|n| n.trim().to_string()),
        platform_code: given("platform_code"),
        description: given("description"),
    }
}

fn member_labels(members: &[MemberSpec]) -> HashMap<String, Option<String>> {
    members
        .iter()
        .filter_map(|m| Some((m.stop_id.clone(), m.platform_code.clone()?)))
        .collect()
}

#[derive(Debug, Default)]
pub struct DraftView {
    created_stops: HashMap<String, CreatedStop>,
    deleted_stops: HashSet<String>,
    moved_stops: HashMap<String, (f64, f64)>,
    merged_away: HashMap<String, (String, i64)>,
    /// stop or station -> the draft's last update of it
    updated_stops: HashMap<String, i64>,
    created_routes: HashMap<String, i64>,
    deleted_routes: HashSet<String>,
    /// route -> (index of the change, change id, rows as the apply stores them)
    replaces: HashMap<String, (usize, i64, Vec<RouteRow>)>,
    /// (index of the change, from, into), in order
    merges: Vec<(usize, String, String)>,
    station_ops: Vec<StationOp>,
}

/// One change as the view needs it.
pub struct DraftChange {
    pub change_id: i64,
    pub entity: String,
    pub op: String,
    pub entity_key: String,
    pub after: Value,
}

/// A route_stops row list as the replace stores it: trimmed stop ids, markers
/// without a stop and with an id, stops without marker fields.
pub fn stored_rows(route_id: &str, rows: &[RouteRow]) -> Vec<RouteRow> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let mut r = r.clone();
            r.stop_id = r
                .stop_id
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if r.is_marker() {
                r.stop_id = None;
                r.stop_name_override = None;
                if r.marker_id.is_none() {
                    r.marker_id = Some(format!("rc_{route_id}_{}", i + 1));
                }
            } else {
                r.marker_id = None;
                r.marker_name = None;
                r.marker_lat = None;
                r.marker_lon = None;
            }
            r
        })
        .collect()
}

impl DraftView {
    pub async fn load(conn: &mut PgConnection, change_set_id: Uuid) -> EditorResult<DraftView> {
        let rows = sqlx::query(
            "SELECT change_id, entity, op, entity_key, after::text AS after \
             FROM gtfs_change WHERE change_set_id = $1 ORDER BY position",
        )
        .bind(change_set_id)
        .fetch_all(&mut *conn)
        .await?;
        let changes = rows
            .iter()
            .map(|r| -> Result<DraftChange, sqlx::Error> {
                let after: Option<String> = r.try_get("after")?;
                Ok(DraftChange {
                    change_id: r.try_get("change_id")?,
                    entity: r.try_get("entity")?,
                    op: r.try_get("op")?,
                    entity_key: r.try_get("entity_key")?,
                    after: after
                        .and_then(|a| serde_json::from_str(&a).ok())
                        .unwrap_or(Value::Null),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(DraftView::from_changes(&changes))
    }

    pub fn from_changes(changes: &[DraftChange]) -> DraftView {
        let mut v = DraftView::default();
        for (idx, c) in changes.iter().enumerate() {
            let key = c.entity_key.as_str();
            let a = &c.after;
            let num = |k: &str| a.get(k).and_then(Value::as_f64);
            match (c.entity.as_str(), c.op.as_str()) {
                ("stop", "create") | ("station", "create") => {
                    let is_station = c.entity == "station";
                    v.created_stops
                        .entry(key.to_string())
                        .or_insert_with(|| CreatedStop {
                            change_id: c.change_id,
                            location_type: if is_station { 1 } else { 0 },
                            name: a
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .trim()
                                .to_string(),
                            lat: num("lat").unwrap_or(0.0),
                            lon: num("lon").unwrap_or(0.0),
                        });
                    v.station_ops.push(texts_op(key, true, a));
                    if is_station {
                        if let Some(members) = a.as_object().and_then(station_members) {
                            v.station_ops.push(StationOp::Members {
                                station: key.to_string(),
                                labels: member_labels(&members),
                                members: members.into_iter().map(|m| m.stop_id).collect(),
                                releases_others: false,
                            });
                        }
                    }
                }
                ("stop", "update") => {
                    if let (Some(lat), Some(lon)) = (num("lat"), num("lon")) {
                        v.moved_stops.insert(key.to_string(), (lat, lon));
                    }
                    v.updated_stops.insert(key.to_string(), c.change_id);
                    v.station_ops.push(texts_op(key, false, a));
                }
                ("station", "update") => {
                    v.updated_stops.insert(key.to_string(), c.change_id);
                    v.station_ops.push(texts_op(key, false, a));
                    if let Some(members) = a.as_object().and_then(station_members) {
                        v.station_ops.push(StationOp::Members {
                            station: key.to_string(),
                            labels: member_labels(&members),
                            members: members.into_iter().map(|m| m.stop_id).collect(),
                            releases_others: true,
                        });
                    }
                }
                ("stop", "delete") => {
                    v.deleted_stops.insert(key.to_string());
                    v.station_ops.push(StationOp::StopDelete {
                        stop: key.to_string(),
                    });
                }
                ("station", "delete") => {
                    v.deleted_stops.insert(key.to_string());
                    v.station_ops.push(StationOp::Delete {
                        station: key.to_string(),
                    });
                }
                ("stop", "merge") => {
                    let Some(into) = a.get("into_stop_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let into = into.trim().to_string();
                    v.deleted_stops.insert(key.to_string());
                    v.merged_away
                        .insert(key.to_string(), (into.clone(), c.change_id));
                    v.merges.push((idx, key.to_string(), into.clone()));
                    v.station_ops.push(StationOp::Merge {
                        from: key.to_string(),
                        into,
                        keep_name_from: a.get("keep_name").and_then(Value::as_str) == Some("from"),
                    });
                }
                ("station", "merge") => {
                    let Some(into) = a.get("into_station_id").and_then(Value::as_str) else {
                        continue;
                    };
                    let into = into.trim().to_string();
                    v.deleted_stops.insert(key.to_string());
                    v.merged_away
                        .insert(key.to_string(), (into.clone(), c.change_id));
                    v.station_ops.push(StationOp::StationMerge {
                        from: key.to_string(),
                        into,
                        keep_name_from: a.get("keep_name").and_then(Value::as_str) == Some("from"),
                    });
                }
                ("route", "create") => {
                    v.created_routes
                        .entry(key.to_string())
                        .or_insert(c.change_id);
                }
                ("route", "delete") => {
                    v.deleted_routes.insert(key.to_string());
                }
                ("route_stops", "replace") => {
                    let rows = a
                        .get("rows")
                        .cloned()
                        .and_then(|r| serde_json::from_value::<Vec<RouteRow>>(r).ok());
                    if let Some(rows) = rows {
                        v.replaces
                            .insert(key.to_string(), (idx, c.change_id, stored_rows(key, &rows)));
                    }
                }
                _ => {}
            }
        }
        v
    }

    pub fn created_stop(&self, id: &str) -> Option<&CreatedStop> {
        self.created_stops.get(id)
    }

    /// Deleted, or merged into another stop, by the draft.
    pub fn stop_deleted(&self, id: &str) -> bool {
        self.deleted_stops.contains(id)
    }

    /// `(into, change_id)` when the draft merges `id` into another stop.
    pub fn merged_into(&self, id: &str) -> Option<(&str, i64)> {
        self.merged_away
            .get(id)
            .map(|(into, cid)| (into.as_str(), *cid))
    }

    /// The position the draft last gives a stop, if it creates or moves it.
    pub fn stop_position(&self, id: &str) -> Option<(f64, f64)> {
        self.moved_stops
            .get(id)
            .copied()
            .or_else(|| self.created_stops.get(id).map(|c| (c.lat, c.lon)))
    }

    /// The change id of the draft's last update of a stop or station.
    pub fn updated_by(&self, id: &str) -> Option<i64> {
        self.updated_stops.get(id).copied()
    }

    pub fn created_route(&self, id: &str) -> Option<i64> {
        self.created_routes.get(id).copied()
    }

    pub fn route_deleted(&self, id: &str) -> bool {
        self.deleted_routes.contains(id)
    }

    /// The change id of the draft's last stop-list replace of a route.
    pub fn replaced_by(&self, route_id: &str) -> Option<i64> {
        self.replaces.get(route_id).map(|(_, cid, _)| *cid)
    }

    /// A route's rows once the draft applies, given its live rows: the last
    /// replace (or the live rows), with every later merge's stop switched.
    pub fn current_rows(&self, route_id: &str, live: &[RouteRow]) -> Vec<RouteRow> {
        let (after, mut rows) = match self.replaces.get(route_id) {
            Some((idx, _, rows)) => (Some(*idx), rows.clone()),
            None => (None, live.to_vec()),
        };
        for (idx, from, into) in &self.merges {
            if after.is_some_and(|a| *idx < a) {
                continue;
            }
            for r in rows.iter_mut() {
                if !r.is_marker() && r.stop_id.as_deref() == Some(from.as_str()) {
                    r.stop_id = Some(into.clone());
                }
            }
        }
        rows
    }

    /// Stops a merge in the draft takes a station from; a parent simulation
    /// needs their live parents too.
    pub fn merge_sources(&self) -> Vec<String> {
        self.merges
            .iter()
            .map(|(_, from, _)| from.clone())
            .collect()
    }

    /// Each stop's parent station once the draft applies, starting from `live`
    /// (stop id -> live parent). Only stops in `live` are followed.
    pub fn parents_after(
        &self,
        live: &HashMap<String, Option<String>>,
    ) -> HashMap<String, Option<String>> {
        let mut p = live.clone();
        for op in &self.station_ops {
            match op {
                StationOp::Members {
                    station,
                    members,
                    releases_others,
                    ..
                } => {
                    for (id, parent) in p.iter_mut() {
                        if members.contains(id) {
                            *parent = Some(station.clone());
                        } else if *releases_others && parent.as_deref() == Some(station.as_str()) {
                            *parent = None;
                        }
                    }
                }
                StationOp::Delete { station } => {
                    for parent in p.values_mut() {
                        if parent.as_deref() == Some(station.as_str()) {
                            *parent = None;
                        }
                    }
                }
                StationOp::StopDelete { stop } => {
                    if let Some(parent) = p.get_mut(stop) {
                        *parent = None;
                    }
                }
                StationOp::Merge { from, into, .. } => {
                    let taken = p.get(from).cloned().flatten();
                    if let Some(parent) = p.get_mut(into) {
                        if parent.is_none() {
                            *parent = taken;
                        }
                    }
                    if let Some(parent) = p.get_mut(from) {
                        *parent = None;
                    }
                }
                StationOp::StationMerge { from, into, .. } => {
                    for parent in p.values_mut() {
                        if parent.as_deref() == Some(from.as_str()) {
                            *parent = Some(into.clone());
                        }
                    }
                    if let Some(parent) = p.get_mut(from) {
                        *parent = None;
                    }
                }
                StationOp::Texts { .. } => {}
            }
        }
        p
    }

    /// Each stop's name, platform label, description and station once the
    /// draft applies, starting from `live`. Only stops in `live` are followed,
    /// and the stops and stations the draft creates; a merge needs the merged
    /// stop ([`DraftView::merge_sources`]) in `live` to hand anything over.
    pub fn texts_after(&self, live: &HashMap<String, StopTexts>) -> HashMap<String, StopTexts> {
        let mut t = live.clone();
        for op in &self.station_ops {
            match op {
                StationOp::Texts {
                    stop,
                    creates,
                    name,
                    platform_code,
                    description,
                } => {
                    if *creates && !t.contains_key(stop) {
                        t.insert(stop.clone(), StopTexts::default());
                    }
                    let Some(s) = t.get_mut(stop) else { continue };
                    if let Some(name) = name {
                        s.name = name.clone();
                    }
                    if let Some(code) = platform_code {
                        s.platform_code = code.clone();
                    }
                    if let Some(d) = description {
                        s.description = d.clone();
                    }
                }
                StationOp::Members {
                    station,
                    members,
                    releases_others,
                    labels,
                } => {
                    // a join costs its members, a release is one pass: a draft
                    // of many stations stays linear in what it touches
                    for id in members {
                        if let Some(s) = t.get_mut(id) {
                            s.parent_station = Some(station.clone());
                            if let Some(code) = labels.get(id) {
                                s.platform_code = code.clone();
                            }
                        }
                    }
                    if *releases_others {
                        for (id, s) in t.iter_mut() {
                            if !members.contains(id)
                                && s.parent_station.as_deref() == Some(station.as_str())
                            {
                                s.parent_station = None;
                            }
                        }
                    }
                }
                StationOp::Delete { station } => {
                    for s in t.values_mut() {
                        if s.parent_station.as_deref() == Some(station.as_str()) {
                            s.parent_station = None;
                        }
                    }
                }
                StationOp::StopDelete { stop } => {
                    if let Some(s) = t.get_mut(stop) {
                        s.parent_station = None;
                    }
                }
                StationOp::Merge {
                    from,
                    into,
                    keep_name_from,
                } => {
                    let gone = t.get(from).cloned();
                    if let (Some(gone), Some(kept)) = (gone, t.get_mut(into)) {
                        if *keep_name_from {
                            kept.name = gone.name;
                        }
                        if kept.parent_station.is_none() && gone.parent_station.is_some() {
                            kept.parent_station = gone.parent_station;
                            if gone.platform_code.is_some() {
                                kept.platform_code = gone.platform_code;
                            }
                        }
                    }
                    if let Some(s) = t.get_mut(from) {
                        s.parent_station = None;
                    }
                }
                StationOp::StationMerge {
                    from,
                    into,
                    keep_name_from,
                } => {
                    if *keep_name_from {
                        let gone = t.get(from).map(|s| s.name.clone());
                        if let (Some(name), Some(kept)) = (gone, t.get_mut(into)) {
                            kept.name = name;
                        }
                    }
                    // the platforms keep their own labels, only the parent moves
                    for s in t.values_mut() {
                        if s.parent_station.as_deref() == Some(from.as_str()) {
                            s.parent_station = Some(into.clone());
                        }
                    }
                    if let Some(s) = t.get_mut(from) {
                        s.parent_station = None;
                    }
                }
            }
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ch(id: i64, entity: &str, op: &str, key: &str, after: Value) -> DraftChange {
        DraftChange {
            change_id: id,
            entity: entity.into(),
            op: op.into(),
            entity_key: key.into(),
            after,
        }
    }

    fn row(stop: &str, t: &str, no: i32) -> RouteRow {
        RouteRow {
            stop_id: Some(stop.into()),
            stop_type: t.into(),
            stage_no: no,
            stage_name: format!("S{no}"),
            marker_id: None,
            marker_name: None,
            marker_lat: None,
            marker_lon: None,
            stop_name_override: None,
            provider_id: None,
        }
    }

    #[test]
    fn creates_deletes_and_merges() {
        let v = DraftView::from_changes(&[
            ch(
                1,
                "stop",
                "create",
                "ed_1",
                json!({"stop_id": "ed_1", "name": " New ", "lat": 13.0, "lon": 80.0}),
            ),
            ch(
                2,
                "station",
                "create",
                "ST",
                json!({"station_id": "ST", "name": "St", "lat": 13.0, "lon": 80.0,
                "members": [{"stop_id": "A", "platform_code": "Towards X"}, {"stop_id": "ed_1"}]}),
            ),
            ch(
                3,
                "route",
                "create",
                "R9",
                json!({"route_id": "R9", "short_name": "9"}),
            ),
            ch(4, "stop", "delete", "D", Value::Null),
            ch(5, "stop", "merge", "M", json!({"into_stop_id": "K"})),
            ch(6, "route", "delete", "R1", Value::Null),
            ch(7, "stop", "update", "A", json!({"lat": 13.5, "lon": 80.5})),
        ]);
        let created = v.created_stop("ed_1").unwrap();
        assert_eq!(
            (
                created.change_id,
                created.location_type,
                created.name.as_str()
            ),
            (1, 0, "New")
        );
        assert_eq!(v.created_stop("ST").unwrap().location_type, 1);
        assert_eq!(v.created_route("R9"), Some(3));
        assert!(v.stop_deleted("D") && v.stop_deleted("M") && !v.stop_deleted("K"));
        assert_eq!(v.merged_into("M"), Some(("K", 5)));
        assert!(v.route_deleted("R1") && !v.route_deleted("R9"));
        assert_eq!(v.stop_position("A"), Some((13.5, 80.5)));
        assert_eq!(v.stop_position("ed_1"), Some((13.0, 80.0)));
        assert_eq!(v.merge_sources(), vec!["M".to_string()]);
    }

    #[test]
    fn current_rows_follow_replaces_then_later_merges() {
        let live = vec![row("A", "NEW STOP", 1), row("M", "INTERMEDIATE STOP", 1)];
        // a merge before the replace does not touch the replace's rows
        let v = DraftView::from_changes(&[
            ch(1, "stop", "merge", "M", json!({"into_stop_id": "K"})),
            ch(
                2,
                "route_stops",
                "replace",
                "R1",
                json!({"base_rows_hash": "x", "rows": [
                {"stop_id": " B ", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "S1"},
                {"stop_type": "ROUTE CORRECTION", "stage_no": 1, "stage_name": "S1", "marker_lat": 13.0, "marker_lon": 80.0},
                {"stop_id": "M", "stop_type": "INTERMEDIATE STOP", "stage_no": 1, "stage_name": "S1"}]}),
            ),
        ]);
        let rows = v.current_rows("R1", &live);
        assert_eq!(rows[0].stop_id.as_deref(), Some("B"));
        assert_eq!(rows[1].marker_id.as_deref(), Some("rc_R1_2"));
        assert_eq!(rows[2].stop_id.as_deref(), Some("M"));
        assert_eq!(v.replaced_by("R1"), Some(2));
        // a route the draft does not replace gets the merge applied to its live rows
        let rows = v.current_rows("R2", &live);
        assert_eq!(rows[1].stop_id.as_deref(), Some("K"));
        // a merge after the replace switches the replaced rows
        let v = DraftView::from_changes(&[
            ch(
                1,
                "route_stops",
                "replace",
                "R1",
                json!({"base_rows_hash": "x", "rows": [
                {"stop_id": "M", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "S1"}]}),
            ),
            ch(2, "stop", "merge", "M", json!({"into_stop_id": "K"})),
        ]);
        assert_eq!(v.current_rows("R1", &[])[0].stop_id.as_deref(), Some("K"));
    }

    #[test]
    fn parents_follow_station_changes_in_order() {
        let v = DraftView::from_changes(&[
            ch(
                1,
                "station",
                "create",
                "ST1",
                json!({"station_id": "ST1", "name": "a", "lat": 1.0, "lon": 1.0, "member_stop_ids": ["A", "B"]}),
            ),
            ch(
                2,
                "station",
                "update",
                "ST0",
                json!({"members": [{"stop_id": "C"}]}),
            ),
            ch(3, "station", "delete", "ST9", Value::Null),
            ch(4, "stop", "merge", "E", json!({"into_stop_id": "F"})),
            ch(5, "stop", "delete", "G", Value::Null),
        ]);
        let live: HashMap<String, Option<String>> = [
            ("A", None),
            ("B", Some("ST0")),
            ("C", None),
            ("D", Some("ST0")),
            ("E", Some("ST9")),
            ("F", None),
            ("G", Some("ST5")),
            ("H", Some("ST9")),
        ]
        .into_iter()
        .map(|(k, p)| (k.to_string(), p.map(str::to_string)))
        .collect();
        let p = v.parents_after(&live);
        assert_eq!(p["A"].as_deref(), Some("ST1"));
        // B moved to ST1 before ST0's update, which then releases D but not B
        assert_eq!(p["B"].as_deref(), Some("ST1"));
        assert_eq!(p["C"].as_deref(), Some("ST0"));
        assert_eq!(p["D"], None);
        // ST9 was deleted before the merge, so F takes nothing
        assert_eq!(p["E"], None);
        assert_eq!(p["F"], None);
        assert_eq!(p["G"], None);
        assert_eq!(p["H"], None);
        // a merge takes the station of a stop that still has one
        let v =
            DraftView::from_changes(&[ch(1, "stop", "merge", "E", json!({"into_stop_id": "F"}))]);
        let p = v.parents_after(&live);
        assert_eq!((p["E"].clone(), p["F"].as_deref()), (None, Some("ST9")));
    }

    #[test]
    fn texts_follow_updates_labels_and_merges_in_order() {
        let v = DraftView::from_changes(&[
            ch(
                1,
                "stop",
                "update",
                "A",
                json!({"platform_code": " Towards X ", "description": "by the gate"}),
            ),
            ch(2, "stop", "update", "A", json!({"description": " "})),
            ch(
                3,
                "station",
                "create",
                "ST",
                json!({"station_id": "ST", "name": " Hub ", "lat": 1.0, "lon": 1.0, "description": "the hub",
                "members": [{"stop_id": "A"}, {"stop_id": "B", "platform_code": "Towards Y"}, {"stop_id": "C", "platform_code": null}]}),
            ),
            ch(
                4,
                "stop",
                "merge",
                "B",
                json!({"into_stop_id": "K", "keep_name": "from"}),
            ),
            ch(5, "station", "update", "ST", json!({"name": "Hub 2"})),
        ]);
        let stop = |name: &str, code: Option<&str>| StopTexts {
            name: name.into(),
            platform_code: code.map(str::to_string),
            description: None,
            parent_station: None,
        };
        let live: HashMap<String, StopTexts> = [
            ("A", stop("a", None)),
            ("B", stop("b", Some("old"))),
            ("C", stop("c", Some("old"))),
            ("K", stop("k", Some("own"))),
        ]
        .into_iter()
        .map(|(k, s)| (k.to_string(), s))
        .collect();
        let t = v.texts_after(&live);
        // the later update cleared the description the earlier one set
        assert_eq!(t["A"].platform_code.as_deref(), Some("Towards X"));
        assert_eq!(t["A"].description, None);
        assert_eq!(t["A"].parent_station.as_deref(), Some("ST"));
        assert_eq!(t["C"].platform_code, None);
        // the kept stop takes the station, and with it the merged stop's label
        assert_eq!(t["K"].name, "b");
        assert_eq!(t["K"].platform_code.as_deref(), Some("Towards Y"));
        assert_eq!(t["K"].parent_station.as_deref(), Some("ST"));
        assert_eq!(t["B"].parent_station, None);
        // a station the draft creates is followed too
        assert_eq!(t["ST"].name, "Hub 2");
        assert_eq!(t["ST"].description.as_deref(), Some("the hub"));
        assert_eq!(v.updated_by("A"), Some(2));
        assert_eq!(v.updated_by("K"), None);
    }
}
