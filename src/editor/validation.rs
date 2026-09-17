//! Pure validation of editor changes. No database: the service layer supplies
//! the live state and turns these findings into per-change issues.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;

pub const STOP_TYPES: [&str; 5] = [
    "NEW STOP",
    "INTERMEDIATE STOP",
    "JUMP STOP",
    "ROUTE CORRECTION",
    "HIDDEN STOP",
];
/// Rows a passenger never boards at: shaping markers and fare-only jump stops.
pub const UNSERVED_TYPES: [&str; 3] = ["ROUTE CORRECTION", "JUMP STOP", "HIDDEN STOP"];

pub const MOVE_WARNING_METRES: f64 = 500.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    pub level: Level,
    pub code: String,
    pub message: String,
    /// Identifies the finding independently of row positions, so a finding
    /// that already existed before an edit can be told apart from a new one.
    #[serde(skip)]
    pub key: String,
}

impl Finding {
    pub fn error(code: &str, key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            code: code.to_string(),
            message: message.into(),
            key: key.into(),
        }
    }

    pub fn warning(code: &str, key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Warning,
            code: code.to_string(),
            message: message.into(),
            key: key.into(),
        }
    }
}

/// One row of a route's stop order, as sent in a `route_stops` replace and as
/// stored in `gtfs_route_stop`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRow {
    #[serde(default)]
    pub stop_id: Option<String>,
    pub stop_type: String,
    pub stage_no: i32,
    pub stage_name: String,
    #[serde(default)]
    pub marker_id: Option<String>,
    #[serde(default)]
    pub marker_name: Option<String>,
    #[serde(default)]
    pub marker_lat: Option<f64>,
    #[serde(default)]
    pub marker_lon: Option<f64>,
    #[serde(default)]
    pub stop_name_override: Option<String>,
    #[serde(default)]
    pub provider_id: Option<String>,
}

impl RouteRow {
    pub fn is_marker(&self) -> bool {
        self.stop_type == "ROUTE CORRECTION"
    }

    pub fn is_served(&self) -> bool {
        !UNSERVED_TYPES.contains(&self.stop_type.as_str())
    }
}

/// Structure and fare rules of a route's full stop order.
///
/// The fare invariant: an INTERMEDIATE STOP carries the stage number and name
/// of the NEW STOP before it. A stop on the wrong side of a stage boundary
/// changes what a passenger is charged.
pub fn check_route_rows(rows: &[RouteRow]) -> Vec<Finding> {
    let mut out = Vec::new();
    if rows.is_empty() {
        out.push(Finding::error(
            "route_empty",
            "",
            "a route needs at least one row",
        ));
        return out;
    }
    let mut last_stage: Option<(i32, &str)> = None; // most recent NEW STOP
    let mut max_stage: Option<i32> = None;
    let mut prev_served: Option<&str> = None;
    let mut first_served_seen = false;
    let mut served = 0;

    for (i, r) in rows.iter().enumerate() {
        let pos = i + 1;
        if !STOP_TYPES.contains(&r.stop_type.as_str()) {
            out.push(Finding::error(
                "unknown_stop_type",
                format!("{}|{}", r.stop_type, pos),
                format!("row {pos}: unknown stop type {:?}", r.stop_type),
            ));
            continue;
        }
        if r.stage_name.trim().is_empty() {
            out.push(Finding::error(
                "stage_name_missing",
                format!("{:?}|{}", r.stop_id, r.stage_no),
                format!("row {pos}: stage name is empty"),
            ));
        }
        if r.is_marker() {
            if r.stop_id.is_some() {
                out.push(Finding::error(
                    "marker_has_stop",
                    format!("{:?}", r.marker_id),
                    format!("row {pos}: a ROUTE CORRECTION row is a shaping point and cannot carry a stop id"),
                ));
            }
            match (r.marker_lat, r.marker_lon) {
                (Some(lat), Some(lon)) if valid_lat_lon(lat, lon) => {}
                _ => out.push(Finding::error(
                    "marker_position",
                    format!("{:?}", r.marker_id),
                    format!("row {pos}: a ROUTE CORRECTION row needs a valid marker position"),
                )),
            }
            if r.stop_name_override.is_some() {
                out.push(Finding::error(
                    "marker_name_override",
                    format!("{:?}", r.marker_id),
                    format!("row {pos}: use marker_name on a ROUTE CORRECTION row"),
                ));
            }
        } else if r.stop_id.as_deref().map(str::trim).unwrap_or("").is_empty() {
            out.push(Finding::error(
                "stop_missing",
                format!("{pos}"),
                format!("row {pos}: {} needs a stop id", r.stop_type),
            ));
            continue;
        }

        let sid = r.stop_id.as_deref().unwrap_or("");
        if !r.is_marker() {
            if let Some(max) = max_stage {
                if r.stage_no < max {
                    out.push(Finding::error(
                        "stage_decreases",
                        format!("{sid}|{max}->{}", r.stage_no),
                        format!(
                            "row {pos} ({sid}): stage {} comes after stage {max}",
                            r.stage_no
                        ),
                    ));
                }
            }
            max_stage = Some(max_stage.map_or(r.stage_no, |m| m.max(r.stage_no)));
        }

        match r.stop_type.as_str() {
            "NEW STOP" => {
                last_stage = Some((r.stage_no, r.stage_name.as_str()));
            }
            "INTERMEDIATE STOP" => match last_stage {
                None => out.push(Finding::error(
                    "intermediate_before_stage",
                    format!("{sid}|{}|{}", r.stage_no, r.stage_name),
                    format!("row {pos} ({sid}): an INTERMEDIATE STOP must follow a NEW STOP"),
                )),
                Some((no, name)) if no != r.stage_no || name != r.stage_name => {
                    out.push(Finding::error(
                        "fare_stage_mismatch",
                        format!("{sid}|{}|{}", r.stage_no, r.stage_name),
                        format!(
                            "row {pos} ({sid}): an INTERMEDIATE STOP must carry the preceding NEW STOP's stage {no} {name:?}, not {} {:?}",
                            r.stage_no, r.stage_name
                        ),
                    ))
                }
                _ => {}
            },
            _ => {}
        }

        if r.is_served() {
            served += 1;
            if !first_served_seen {
                first_served_seen = true;
                if r.stop_type != "NEW STOP" {
                    out.push(Finding::error(
                        "first_stop_not_stage",
                        sid.to_string(),
                        format!("row {pos} ({sid}): the first stop a passenger can board must be a NEW STOP"),
                    ));
                }
            }
            if prev_served == Some(sid) {
                out.push(Finding::error(
                    "stop_repeated",
                    sid.to_string(),
                    format!("row {pos} ({sid}): the same stop appears twice in a row"),
                ));
            }
            prev_served = Some(sid);
        }
    }
    if served < 2 {
        out.push(Finding::error(
            "too_few_stops",
            "",
            "a route needs at least two stops a passenger can board",
        ));
    }
    out
}

/// A finding that the live route already had is reported as a warning: an edit
/// must not make a route worse, but it should not be blocked by old problems it
/// did not introduce. Matching is by (code, key), counted, so adding a second
/// copy of an old problem is still an error.
pub fn grade_against_live(new: Vec<Finding>, live: &[Finding]) -> Vec<Finding> {
    let mut remaining: Vec<(String, String)> = live
        .iter()
        .filter(|f| f.level == Level::Error)
        .map(|f| (f.code.clone(), f.key.clone()))
        .collect();
    new.into_iter()
        .map(|mut f| {
            if f.level == Level::Error {
                if let Some(i) = remaining
                    .iter()
                    .position(|(c, k)| *c == f.code && *k == f.key)
                {
                    remaining.swap_remove(i);
                    f.level = Level::Warning;
                    f.message = format!("{} (already present before this edit)", f.message);
                }
            }
            f
        })
        .collect()
}

pub fn valid_lat_lon(lat: f64, lon: f64) -> bool {
    lat.is_finite()
        && lon.is_finite()
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon)
}

pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * a.sqrt().asin()
}

/// GIMS splits ids on ':' (`gtfs_id:code`), so an id must not contain one.
pub fn check_entity_id(field: &str, id: &str) -> Result<(), Finding> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(Finding::error(
            "invalid_id",
            id,
            format!("{field} must be 1-64 characters of letters, digits, '_', '-' or '.'"),
        ))
    }
}

fn is_hex_color(v: &str) -> bool {
    v.len() == 7 && v.starts_with('#') && v[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Decode a Google encoded polyline (precision 5). None if it is malformed or
/// any point is out of range.
pub fn decode_polyline(encoded: &str) -> Option<Vec<(f64, f64)>> {
    let bytes = encoded.as_bytes();
    let (mut i, mut lat, mut lon) = (0usize, 0i64, 0i64);
    let mut points = Vec::new();
    while i < bytes.len() {
        let mut coord = [0i64; 2];
        for c in coord.iter_mut() {
            let (mut shift, mut result) = (0u32, 0i64);
            loop {
                let b = *bytes.get(i)? as i64 - 63;
                if !(0..64).contains(&b) || shift > 60 {
                    return None;
                }
                i += 1;
                result |= (b & 0x1f) << shift;
                shift += 5;
                if b < 0x20 {
                    break;
                }
            }
            *c = if result & 1 != 0 {
                !(result >> 1)
            } else {
                result >> 1
            };
        }
        lat += coord[0];
        lon += coord[1];
        let (plat, plon) = (lat as f64 / 1e5, lon as f64 / 1e5);
        if !valid_lat_lon(plat, plon) {
            return None;
        }
        points.push((plat, plon));
    }
    Some(points)
}

fn obj<'a>(after: &'a Value, what: &str) -> Result<&'a Map<String, Value>, Finding> {
    after.as_object().ok_or_else(|| {
        Finding::error(
            "invalid_payload",
            what,
            format!("{what}: `after` must be an object"),
        )
    })
}

fn allow_only(map: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), Finding> {
    for k in map.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(Finding::error(
                "invalid_payload",
                k.as_str(),
                format!(
                    "{what}: field {k:?} cannot be set (allowed: {})",
                    allowed.join(", ")
                ),
            ));
        }
    }
    Ok(())
}

fn opt_string(map: &Map<String, Value>, key: &str, what: &str) -> Result<(), Finding> {
    match map.get(key) {
        None | Some(Value::Null) | Some(Value::String(_)) => Ok(()),
        _ => Err(Finding::error(
            "invalid_payload",
            key,
            format!("{what}: {key} must be a string or null"),
        )),
    }
}

fn req_string<'a>(map: &'a Map<String, Value>, key: &str, what: &str) -> Result<&'a str, Finding> {
    match map.get(key).and_then(Value::as_str).map(str::trim) {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(Finding::error(
            "invalid_payload",
            key,
            format!("{what}: {key} is required"),
        )),
    }
}

fn lat_lon(map: &Map<String, Value>, required: bool, what: &str) -> Result<(), Finding> {
    let lat = map.get("lat");
    let lon = map.get("lon");
    if !required && lat.is_none() && lon.is_none() {
        return Ok(());
    }
    for (k, v) in [("lat", lat), ("lon", lon)] {
        match v {
            Some(Value::Number(_)) => {}
            None if !required => {}
            _ => {
                return Err(Finding::error(
                    "invalid_payload",
                    k,
                    format!("{what}: {k} must be a number"),
                ))
            }
        }
    }
    let la = lat.and_then(Value::as_f64).unwrap_or(0.0);
    let lo = lon.and_then(Value::as_f64).unwrap_or(0.0);
    if !valid_lat_lon(la, lo) || (lat.is_some() && lon.is_some() && la == 0.0 && lo == 0.0) {
        return Err(Finding::error(
            "invalid_position",
            format!("{la},{lo}"),
            format!("{what}: position {la}, {lo} is out of range"),
        ));
    }
    Ok(())
}

/// Shape checks run when a change is added. Anything that needs live data
/// (does the stop exist, is it in use) is checked by the service.
pub fn check_payload(
    entity: &str,
    op: &str,
    entity_key: &str,
    after: &Value,
) -> Result<(), Finding> {
    let what = format!("{entity}/{op}");
    let what = what.as_str();
    match (entity, op) {
        ("stop", "update") => {
            let m = obj(after, what)?;
            let allowed = [
                "name",
                "lat",
                "lon",
                "platform_code",
                "cluster_id",
                "regional_name",
                "hindi_name",
            ];
            allow_only(m, &allowed, what)?;
            if m.is_empty() {
                return Err(Finding::error(
                    "invalid_payload",
                    "",
                    format!("{what}: nothing to change"),
                ));
            }
            if m.contains_key("name") {
                req_string(m, "name", what)?;
            }
            if m.contains_key("lat") != m.contains_key("lon") {
                return Err(Finding::error(
                    "invalid_payload",
                    "lat/lon",
                    format!("{what}: lat and lon are changed together"),
                ));
            }
            lat_lon(m, false, what)?;
            for k in ["platform_code", "cluster_id", "regional_name", "hindi_name"] {
                opt_string(m, k, what)?;
            }
            Ok(())
        }
        ("stop", "create") => {
            let m = obj(after, what)?;
            allow_only(
                m,
                &[
                    "stop_id",
                    "name",
                    "lat",
                    "lon",
                    "stop_code",
                    "platform_code",
                    "cluster_id",
                    "regional_name",
                    "hindi_name",
                ],
                what,
            )?;
            let id = req_string(m, "stop_id", what)?;
            check_entity_id("stop_id", id)?;
            if id != entity_key {
                return Err(Finding::error(
                    "invalid_payload",
                    id,
                    format!("{what}: entity_key must equal stop_id"),
                ));
            }
            req_string(m, "name", what)?;
            lat_lon(m, true, what)?;
            for k in [
                "stop_code",
                "platform_code",
                "cluster_id",
                "regional_name",
                "hindi_name",
            ] {
                opt_string(m, k, what)?;
            }
            Ok(())
        }
        ("stop", "delete") | ("station", "delete") => {
            if after.is_null() {
                Ok(())
            } else {
                Err(Finding::error(
                    "invalid_payload",
                    "",
                    format!("{what}: `after` must be null"),
                ))
            }
        }
        ("route", "update") => {
            let m = obj(after, what)?;
            allow_only(
                m,
                &[
                    "short_name",
                    "long_name",
                    "color",
                    "text_color",
                    "encoded_polyline",
                    "polyline_source",
                ],
                what,
            )?;
            if m.is_empty() {
                return Err(Finding::error(
                    "invalid_payload",
                    "",
                    format!("{what}: nothing to change"),
                ));
            }
            for k in [
                "short_name",
                "long_name",
                "color",
                "text_color",
                "encoded_polyline",
                "polyline_source",
            ] {
                opt_string(m, k, what)?;
            }
            for k in ["color", "text_color"] {
                if let Some(c) = m.get(k).and_then(Value::as_str) {
                    if !is_hex_color(c) {
                        return Err(Finding::error(
                            "invalid_color",
                            c,
                            format!("{what}: {k} must be #RRGGBB"),
                        ));
                    }
                }
            }
            if let Some(p) = m.get("encoded_polyline").and_then(Value::as_str) {
                match decode_polyline(p) {
                    Some(pts) if pts.len() >= 2 => {}
                    _ => {
                        return Err(Finding::error(
                            "invalid_polyline",
                            "",
                            format!(
                            "{what}: encoded_polyline does not decode to at least two valid points"
                        ),
                        ))
                    }
                }
            }
            if let Some(s) = m.get("polyline_source").and_then(Value::as_str) {
                if !["osrm", "manual", "imported"].contains(&s) {
                    return Err(Finding::error(
                        "invalid_payload",
                        s,
                        format!("{what}: polyline_source is osrm, manual or imported"),
                    ));
                }
            }
            Ok(())
        }
        ("route_stops", "replace") => {
            let m = obj(after, what)?;
            allow_only(m, &["rows", "base_rows_hash"], what)?;
            req_string(m, "base_rows_hash", what)?;
            let rows = m.get("rows").ok_or_else(|| {
                Finding::error(
                    "invalid_payload",
                    "rows",
                    format!("{what}: rows is required"),
                )
            })?;
            serde_json::from_value::<Vec<RouteRow>>(rows.clone()).map_err(|e| {
                Finding::error(
                    "invalid_payload",
                    "rows",
                    format!("{what}: rows are not valid: {e}"),
                )
            })?;
            Ok(())
        }
        ("station", "create") | ("station", "update") => {
            let m = obj(after, what)?;
            let create = op == "create";
            let allowed: &[&str] = if create {
                &["station_id", "name", "lat", "lon", "member_stop_ids"]
            } else {
                &["name", "lat", "lon", "member_stop_ids"]
            };
            allow_only(m, allowed, what)?;
            if create {
                let id = req_string(m, "station_id", what)?;
                check_entity_id("station_id", id)?;
                if id != entity_key {
                    return Err(Finding::error(
                        "invalid_payload",
                        id,
                        format!("{what}: entity_key must equal station_id"),
                    ));
                }
                req_string(m, "name", what)?;
                lat_lon(m, true, what)?;
            } else {
                if m.is_empty() {
                    return Err(Finding::error(
                        "invalid_payload",
                        "",
                        format!("{what}: nothing to change"),
                    ));
                }
                if m.contains_key("name") {
                    req_string(m, "name", what)?;
                }
                if m.contains_key("lat") != m.contains_key("lon") {
                    return Err(Finding::error(
                        "invalid_payload",
                        "lat/lon",
                        format!("{what}: lat and lon are changed together"),
                    ));
                }
                lat_lon(m, false, what)?;
            }
            if let Some(members) = m.get("member_stop_ids") {
                let list = members.as_array().ok_or_else(|| {
                    Finding::error(
                        "invalid_payload",
                        "member_stop_ids",
                        format!("{what}: member_stop_ids must be a list"),
                    )
                })?;
                let mut seen = HashSet::new();
                for v in list {
                    let s = v.as_str().ok_or_else(|| {
                        Finding::error(
                            "invalid_payload",
                            "member_stop_ids",
                            format!("{what}: member ids are strings"),
                        )
                    })?;
                    if s == entity_key {
                        return Err(Finding::error(
                            "invalid_payload",
                            s,
                            format!("{what}: a station cannot contain itself"),
                        ));
                    }
                    if !seen.insert(s) {
                        return Err(Finding::error(
                            "invalid_payload",
                            s,
                            format!("{what}: {s} is listed twice"),
                        ));
                    }
                }
            } else if create {
                return Err(Finding::error(
                    "invalid_payload",
                    "member_stop_ids",
                    format!("{what}: member_stop_ids is required"),
                ));
            }
            Ok(())
        }
        _ => Err(Finding::error(
            "invalid_change",
            format!("{entity}/{op}"),
            format!("unsupported change {entity}/{op}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(stop: &str, t: &str, no: i32, name: &str) -> RouteRow {
        RouteRow {
            stop_id: Some(stop.to_string()),
            stop_type: t.to_string(),
            stage_no: no,
            stage_name: name.to_string(),
            marker_id: None,
            marker_name: None,
            marker_lat: None,
            marker_lon: None,
            stop_name_override: None,
            provider_id: None,
        }
    }

    fn marker(no: i32, name: &str, pos: Option<(f64, f64)>) -> RouteRow {
        RouteRow {
            stop_id: None,
            stop_type: "ROUTE CORRECTION".into(),
            stage_no: no,
            stage_name: name.into(),
            marker_id: Some("m1".into()),
            marker_name: Some("ROUTE CORRECTION".into()),
            marker_lat: pos.map(|p| p.0),
            marker_lon: pos.map(|p| p.1),
            stop_name_override: None,
            provider_id: None,
        }
    }

    fn codes(f: &[Finding]) -> Vec<&str> {
        f.iter()
            .filter(|x| x.level == Level::Error)
            .map(|x| x.code.as_str())
            .collect()
    }

    fn good() -> Vec<RouteRow> {
        vec![
            row("A", "NEW STOP", 1, "ADYAR"),
            row("B", "INTERMEDIATE STOP", 1, "ADYAR"),
            marker(1, "ADYAR", Some((13.0, 80.2))),
            row("J", "JUMP STOP", 1, "ADYAR"),
            row("C", "NEW STOP", 2, "GUINDY"),
            row("D", "INTERMEDIATE STOP", 2, "GUINDY"),
        ]
    }

    #[test]
    fn a_well_formed_route_passes() {
        assert!(codes(&check_route_rows(&good())).is_empty());
    }

    #[test]
    fn intermediate_must_carry_the_preceding_stage() {
        let mut rows = good();
        rows[5].stage_no = 1;
        rows[5].stage_name = "ADYAR".into();
        let f = check_route_rows(&rows);
        assert!(codes(&f).contains(&"fare_stage_mismatch"));
        // stage number right but name wrong is still a mismatch
        let mut rows = good();
        rows[1].stage_name = "ADYAR B.T".into();
        assert_eq!(codes(&check_route_rows(&rows)), vec!["fare_stage_mismatch"]);
    }

    #[test]
    fn stage_numbers_never_decrease_and_first_stop_is_a_stage() {
        let mut rows = good();
        rows[4].stage_no = 0;
        rows[5].stage_no = 0;
        assert!(codes(&check_route_rows(&rows)).contains(&"stage_decreases"));
        let rows = vec![
            row("B", "INTERMEDIATE STOP", 1, "X"),
            row("A", "NEW STOP", 1, "X"),
        ];
        let c = check_route_rows(&rows);
        assert!(codes(&c).contains(&"first_stop_not_stage"));
        assert!(codes(&c).contains(&"intermediate_before_stage"));
    }

    #[test]
    fn repeated_stops_markers_and_missing_ids() {
        let mut rows = good();
        rows.insert(1, row("A", "INTERMEDIATE STOP", 1, "ADYAR"));
        assert_eq!(codes(&check_route_rows(&rows)), vec!["stop_repeated"]);
        let mut rows = good();
        rows[2] = marker(1, "ADYAR", None);
        assert_eq!(codes(&check_route_rows(&rows)), vec!["marker_position"]);
        let mut rows = good();
        rows[1].stop_id = None;
        assert_eq!(codes(&check_route_rows(&rows)), vec!["stop_missing"]);
        let mut rows = good();
        rows[2].stop_id = Some("A".into());
        assert!(codes(&check_route_rows(&rows)).contains(&"marker_has_stop"));
        assert!(
            codes(&check_route_rows(&[row("A", "NEW STOP", 1, "X")])).contains(&"too_few_stops")
        );
        let mut rows = good();
        rows[1].stop_type = "BUS STOP".into();
        assert!(codes(&check_route_rows(&rows)).contains(&"unknown_stop_type"));
    }

    #[test]
    fn old_problems_become_warnings_new_ones_stay_errors() {
        let mut live = good();
        live[5].stage_no = 1;
        live[5].stage_name = "ADYAR".into();
        let live_f = check_route_rows(&live);
        // an unrelated edit keeps the old mismatch: warning only
        let mut edited = live.clone();
        edited[0].stage_name = "ADYAR".into();
        let graded = grade_against_live(check_route_rows(&edited), &live_f);
        assert!(codes(&graded).is_empty());
        assert!(graded.iter().any(|f| f.level == Level::Warning));
        // a second, new mismatch is an error
        let mut worse = live.clone();
        worse[1].stage_no = 2;
        worse[1].stage_name = "GUINDY".into();
        let graded = grade_against_live(check_route_rows(&worse), &live_f);
        assert!(codes(&graded).contains(&"fare_stage_mismatch"));
    }

    #[test]
    fn payload_shapes() {
        assert!(check_payload("stop", "update", "A", &json!({"name": "ADYAR"})).is_ok());
        assert!(check_payload("stop", "update", "A", &json!({"lat": 13.0, "lon": 80.2})).is_ok());
        assert!(check_payload("stop", "update", "A", &json!({"lat": 13.0})).is_err());
        assert!(check_payload("stop", "update", "A", &json!({"lat": 95.0, "lon": 80.2})).is_err());
        assert!(check_payload("stop", "update", "A", &json!({"stop_type": "x"})).is_err());
        assert!(check_payload("stop", "update", "A", &json!({})).is_err());
        assert!(check_payload("stop", "update", "A", &json!({"name": "  "})).is_err());
        assert!(check_payload(
            "stop",
            "create",
            "N1",
            &json!({"stop_id": "N1", "name": "New", "lat": 13.0, "lon": 80.0})
        )
        .is_ok());
        assert!(check_payload(
            "stop",
            "create",
            "N1",
            &json!({"stop_id": "N:1", "name": "New", "lat": 13.0, "lon": 80.0})
        )
        .is_err());
        assert!(check_payload(
            "stop",
            "create",
            "N2",
            &json!({"stop_id": "N1", "name": "New", "lat": 13.0, "lon": 80.0})
        )
        .is_err());
        assert!(check_payload("stop", "delete", "A", &Value::Null).is_ok());
        assert!(check_payload("route", "update", "R", &json!({"color": "#00AA11"})).is_ok());
        assert!(check_payload("route", "update", "R", &json!({"color": "green"})).is_err());
        assert!(check_payload(
            "route",
            "update",
            "R",
            &json!({"encoded_polyline": "_p~iF~ps|U_ulLnnqC_mqNvxq`@"})
        )
        .is_ok());
        assert!(
            check_payload("route", "update", "R", &json!({"encoded_polyline": "!!!"})).is_err()
        );
        assert!(check_payload(
            "route_stops",
            "replace",
            "R",
            &json!({"base_rows_hash": "abc", "rows": [{"stop_id": "A", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "X"}]})
        )
        .is_ok());
        assert!(check_payload("route_stops", "replace", "R", &json!({"rows": []})).is_err());
        assert!(check_payload(
            "route_stops",
            "replace",
            "R",
            &json!({"base_rows_hash": "abc", "rows": [{"stop_id": "A", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "X", "bogus": 1}]})
        )
        .is_err());
        assert!(check_payload("station", "create", "S", &json!({"station_id": "S", "name": "Adyar", "lat": 13.0, "lon": 80.2, "member_stop_ids": ["A", "B"]})).is_ok());
        assert!(check_payload("station", "create", "S", &json!({"station_id": "S", "name": "Adyar", "lat": 13.0, "lon": 80.2, "member_stop_ids": ["A", "A"]})).is_err());
        assert!(check_payload("station", "create", "S", &json!({"station_id": "S", "name": "Adyar", "lat": 13.0, "lon": 80.2, "member_stop_ids": ["S"]})).is_err());
        assert!(
            check_payload("station", "update", "S", &json!({"member_stop_ids": ["A"]})).is_ok()
        );
        assert!(check_payload("trip", "update", "T", &json!({})).is_err());
    }

    #[test]
    fn polyline_decoding() {
        // Google's documented example
        let pts = decode_polyline("_p~iF~ps|U_ulLnnqC_mqNvxq`@").unwrap();
        assert_eq!(pts.len(), 3);
        assert!((pts[0].0 - 38.5).abs() < 1e-9 && (pts[0].1 + 120.2).abs() < 1e-9);
        assert!((pts[2].0 - 43.252).abs() < 1e-9 && (pts[2].1 + 126.453).abs() < 1e-9);
        assert!(decode_polyline("_p~iF~ps|U_").is_none());
    }

    #[test]
    fn distance() {
        let d = haversine_m(13.038743, 80.258956, 13.038566, 80.258869);
        assert!((d - 21.8).abs() < 1.0, "{d}");
    }
}
