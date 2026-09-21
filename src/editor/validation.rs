//! Pure validation of editor changes. No database: the service layer supplies
//! the live state and turns these findings into per-change issues.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

pub const STOP_TYPES: [&str; 5] = [
    "NEW STOP",
    "INTERMEDIATE STOP",
    "JUMP STOP",
    "ROUTE CORRECTION",
    "HIDDEN STOP",
];
/// Rows a passenger never boards at: shaping markers and fare-only jump stops.
pub const UNSERVED_TYPES: [&str; 3] = ["ROUTE CORRECTION", "JUMP STOP", "HIDDEN STOP"];
/// Findings of [`check_route_rows`] about fares and stop order. Rows that break
/// only these can still be stored (no table constraint refuses them), so a draft
/// shows its stop list as drafted while the errors block submit and commit. The
/// other codes are row shapes the table refuses (a marker with a stop, a stop row
/// without one, an unknown stop type).
pub const ROUTE_RULE_CODES: [&str; 8] = [
    "fare_stage_mismatch",
    "first_stop_not_stage",
    "intermediate_before_stage",
    "route_empty",
    "stage_decreases",
    "stage_name_missing",
    "stop_repeated",
    "too_few_stops",
];

pub const MOVE_WARNING_METRES: f64 = 500.0;
/// A merge of stops further apart than this is a warning.
pub const MERGE_FAR_METRES: f64 = 150.0;
/// A merge of **stations** further apart than this is a warning. Looser than
/// [`MERGE_FAR_METRES`] on purpose: that one is about two kerbs, which are the
/// same place or they are not, while a station point is the centroid of its
/// platforms. `build_stations` already groups same-named stops within a 500 m
/// diameter (section 6), so two stations that are really one place have their
/// points inside that same 500 m - anything wider is a grouping the builder
/// deliberately did not make. See the doc's "Settled while implementing" note
/// for the measurement behind the number.
pub const STATION_MERGE_FAR_METRES: f64 = 500.0;
/// Longest platform label a stop may carry.
pub const PLATFORM_CODE_MAX_CHARS: usize = 120;
/// Longest description a stop or station may carry.
pub const DESCRIPTION_MAX_CHARS: usize = 500;
/// A station groups the stops of one place; fewer than this is not a station.
pub const STATION_MIN_MEMBERS: usize = 2;
/// Stop ids the server mints: `ed_` + 10 lower-case hex digits.
pub const MINTED_STOP_PREFIX: &str = "ed_";
/// The two `gtfs_feed.data_source` values a `feed_config` change may set.
pub const DATA_SOURCES: [&str; 2] = ["db", "preprocessed"];

/// Why a station of `members` stops is too small to be one, if it is.
pub fn too_few_members(station_id: &str, members: usize) -> Option<String> {
    (members < STATION_MIN_MEMBERS).then(|| {
        format!(
            "a station groups at least two stops, and {station_id} would have {}",
            if members == 0 { "none" } else { "only one" }
        )
    })
}

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
    /// The row a route-order finding is about (index into the rows checked).
    #[serde(skip)]
    pub row: Option<usize>,
}

impl Finding {
    pub fn error(code: &str, key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            code: code.to_string(),
            message: message.into(),
            key: key.into(),
            row: None,
        }
    }

    pub fn warning(code: &str, key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Warning,
            code: code.to_string(),
            message: message.into(),
            key: key.into(),
            row: None,
        }
    }

    pub fn at(mut self, row: usize) -> Self {
        self.row = Some(row);
        self
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
    check_route_rows_labelled(rows, &|i| format!("row {}", i + 1))
}

/// [`check_route_rows`], naming row `i` with `label(i)` in messages (a bulk
/// import names rows by their uploaded sequence). Every finding about one row
/// carries its index in [`Finding::row`]; keys never depend on the label.
pub fn check_route_rows_labelled(
    rows: &[RouteRow],
    label: &dyn Fn(usize) -> String,
) -> Vec<Finding> {
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
        let n = i + 1;
        let pos = label(i);
        if !STOP_TYPES.contains(&r.stop_type.as_str()) {
            out.push(
                Finding::error(
                    "unknown_stop_type",
                    format!("{}|{}", r.stop_type, n),
                    format!("{pos}: unknown stop type {:?}", r.stop_type),
                )
                .at(i),
            );
            continue;
        }
        if r.stage_name.trim().is_empty() {
            out.push(
                Finding::error(
                    "stage_name_missing",
                    format!("{:?}|{}", r.stop_id, r.stage_no),
                    format!("{pos}: stage name is empty"),
                )
                .at(i),
            );
        }
        if r.is_marker() {
            if r.stop_id.is_some() {
                out.push(Finding::error(
                    "marker_has_stop",
                    format!("{:?}", r.marker_id),
                    format!("{pos}: a ROUTE CORRECTION row is a shaping point and cannot carry a stop id"),
                ).at(i));
            }
            match (r.marker_lat, r.marker_lon) {
                (Some(lat), Some(lon)) if valid_lat_lon(lat, lon) => {}
                _ => out.push(
                    Finding::error(
                        "marker_position",
                        format!("{:?}", r.marker_id),
                        format!("{pos}: a ROUTE CORRECTION row needs a valid marker position"),
                    )
                    .at(i),
                ),
            }
            if r.stop_name_override.is_some() {
                out.push(
                    Finding::error(
                        "marker_name_override",
                        format!("{:?}", r.marker_id),
                        format!("{pos}: use marker_name on a ROUTE CORRECTION row"),
                    )
                    .at(i),
                );
            }
        } else if r.stop_id.as_deref().map(str::trim).unwrap_or("").is_empty() {
            out.push(
                Finding::error(
                    "stop_missing",
                    format!("{n}"),
                    format!("{pos}: {} needs a stop id", r.stop_type),
                )
                .at(i),
            );
            continue;
        }

        let sid = r.stop_id.as_deref().unwrap_or("");
        if !r.is_marker() {
            if let Some(max) = max_stage {
                if r.stage_no < max {
                    out.push(
                        Finding::error(
                            "stage_decreases",
                            format!("{sid}|{max}->{}", r.stage_no),
                            format!(
                                "{pos} ({sid}): stage {} comes after stage {max}",
                                r.stage_no
                            ),
                        )
                        .at(i),
                    );
                }
            }
            max_stage = Some(max_stage.map_or(r.stage_no, |m| m.max(r.stage_no)));
        }

        match r.stop_type.as_str() {
            "NEW STOP" => {
                last_stage = Some((r.stage_no, r.stage_name.as_str()));
            }
            "INTERMEDIATE STOP" => match last_stage {
                None => out.push(
                    Finding::error(
                        "intermediate_before_stage",
                        format!("{sid}|{}|{}", r.stage_no, r.stage_name),
                        format!("{pos} ({sid}): an INTERMEDIATE STOP must follow a NEW STOP"),
                    )
                    .at(i),
                ),
                Some((no, name)) if no != r.stage_no || name != r.stage_name => out.push(
                    Finding::error(
                        "fare_stage_mismatch",
                        format!("{sid}|{}|{}", r.stage_no, r.stage_name),
                        format!(
                            "{pos} ({sid}): an INTERMEDIATE STOP must carry the preceding NEW STOP's stage {no} {name:?}, not {} {:?}",
                            r.stage_no, r.stage_name
                        ),
                    )
                    .at(i),
                ),
                _ => {}
            },
            _ => {}
        }

        if r.is_served() {
            served += 1;
            if !first_served_seen {
                first_served_seen = true;
                if r.stop_type != "NEW STOP" {
                    out.push(
                        Finding::error(
                            "first_stop_not_stage",
                            sid.to_string(),
                            format!("{pos} ({sid}): the first stop a passenger can board must be a NEW STOP"),
                        )
                        .at(i),
                    );
                }
            }
            if prev_served == Some(sid) {
                out.push(
                    Finding::error(
                        "stop_repeated",
                        sid.to_string(),
                        format!("{pos} ({sid}): the same stop appears twice in a row"),
                    )
                    .at(i),
                );
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

/// Stops an edit re-points and nothing else: new stop id -> the stop the live
/// row had, for rows that differ from the live row at the same place only in
/// their stop. Empty unless both lists are the same length; a new id the live
/// list already calls at, or one standing for two live stops, is left out.
pub fn repointed_stops(live: &[RouteRow], rows: &[RouteRow]) -> HashMap<String, String> {
    if live.len() != rows.len() {
        return HashMap::new();
    }
    let called: HashSet<&str> = live.iter().filter_map(|l| l.stop_id.as_deref()).collect();
    let mut seen: HashMap<String, Option<String>> = HashMap::new();
    for (l, r) in live.iter().zip(rows) {
        let (Some(old), Some(new)) = (l.stop_id.as_deref(), r.stop_id.as_deref()) else {
            continue;
        };
        if called.contains(new) {
            continue;
        }
        let same_row = RouteRow {
            stop_id: None,
            ..l.clone()
        } == RouteRow {
            stop_id: None,
            ..r.clone()
        };
        if old == new || !same_row {
            continue;
        }
        let entry = seen.entry(new.to_string()).or_insert(Some(old.to_string()));
        if entry.as_deref() != Some(old) {
            *entry = None;
        }
    }
    seen.into_iter()
        .filter_map(|(new, old)| Some((new, old?)))
        .collect()
}

/// [`grade_against_live`] for a stop list whose re-pointed rows (see
/// [`repointed_stops`]) are the same rows as before: a problem such a row
/// already had under its old stop counts as already present, as a split of a
/// stop's routes onto a new stop needs. Keys name the row's stop first, as
/// `id`, `id|...` or `Some("id")|...`.
pub fn grade_repointed(rows: &[RouteRow], live: &[RouteRow]) -> Vec<Finding> {
    let renames = repointed_stops(live, rows);
    let rekey = |key: &str| -> Option<String> {
        let (head, tail) = match key.split_once('|') {
            Some((h, t)) => (h, Some(t)),
            None => (key, None),
        };
        let old = match renames.get(head) {
            Some(old) => old.clone(),
            None => {
                let id = head.strip_prefix("Some(\"")?.strip_suffix("\")")?;
                format!("Some({:?})", renames.get(id)?)
            }
        };
        Some(match tail {
            Some(t) => format!("{old}|{t}"),
            None => old,
        })
    };
    let mut new = check_route_rows(rows);
    for f in new.iter_mut() {
        if let Some(key) = rekey(&f.key) {
            f.key = key;
        }
    }
    grade_against_live(new, &check_route_rows(live))
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

/// A fresh stop id: `ed_` + 10 lower-case hex digits (40 random bits). The
/// caller checks it is unused.
pub fn mint_stop_id() -> String {
    format!(
        "{MINTED_STOP_PREFIX}{}",
        hex::encode(super::crypto::random_bytes(5))
    )
}

/// GTFS route_type: the basic types and Google's extended range.
pub fn valid_route_type(t: i64) -> bool {
    matches!(t, 0..=7 | 11 | 12 | 100..=1702)
}

/// One member of a station change. `platform_code` is `None` when the change
/// leaves the stop's label alone, `Some(None)` when it clears it.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberSpec {
    pub stop_id: String,
    pub platform_code: Option<Option<String>>,
}

/// A station change's members, from `members: [{stop_id, platform_code?}]` or
/// the older `member_stop_ids: [...]`. `None` when it sends neither.
pub fn station_members(m: &Map<String, Value>) -> Option<Vec<MemberSpec>> {
    if let Some(list) = m.get("members").and_then(Value::as_array) {
        return Some(
            list.iter()
                .filter_map(|v| {
                    let o = v.as_object()?;
                    Some(MemberSpec {
                        stop_id: o.get("stop_id")?.as_str()?.trim().to_string(),
                        platform_code: o.get("platform_code").map(|p| {
                            p.as_str()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                        }),
                    })
                })
                .collect(),
        );
    }
    m.get("member_stop_ids")?.as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str())
            .map(|s| MemberSpec {
                stop_id: s.to_string(),
                platform_code: None,
            })
            .collect()
    })
}

/// What merging stop `from` into `into` does to one route, given its rows in
/// sequence order as `(sequence, stop_id, stop_type)`.
#[derive(Debug, Default, PartialEq)]
pub struct MergeEffect {
    /// Consecutive served rows that would both call `into` because of the
    /// merge (one was `from`, the other `into`), as sequence pairs.
    pub repeats: Vec<(i32, i32)>,
    /// Sequences of the rows calling `from` and `into` (markers excluded).
    pub from_seqs: Vec<i32>,
    pub into_seqs: Vec<i32>,
}

/// A route row as `(sequence, stop_id, stop_type)`.
pub type SequencedStop = (i32, Option<String>, String);

pub fn merge_effect(rows: &[SequencedStop], from: &str, into: &str) -> MergeEffect {
    let served: Vec<(i32, &str)> = rows
        .iter()
        .filter(|(_, s, t)| s.is_some() && !UNSERVED_TYPES.contains(&t.as_str()))
        .map(|(q, s, _)| (*q, s.as_deref().unwrap_or("")))
        .collect();
    let after = |s: &str| {
        if s == from {
            into.to_string()
        } else {
            s.to_string()
        }
    };
    let mut out = MergeEffect::default();
    for w in served.windows(2) {
        let ((qa, a), (qb, b)) = (w[0], w[1]);
        // a pair that was already the same stop is not the merge's doing
        if a != b && after(a) == after(b) {
            out.repeats.push((qa, qb));
        }
    }
    for (q, s, t) in rows {
        if t == "ROUTE CORRECTION" {
            continue;
        }
        match s.as_deref() {
            Some(x) if x == from => out.from_seqs.push(*q),
            Some(x) if x == into => out.into_seqs.push(*q),
            _ => {}
        }
    }
    out
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

/// A stop's own text fields that have a length: the platform label and the
/// description, each a string or null, counted in characters once trimmed.
fn stop_texts(map: &Map<String, Value>, id: &str, what: &str) -> Result<(), Finding> {
    for (key, code, max, noun) in [
        (
            "platform_code",
            "invalid_platform_code",
            PLATFORM_CODE_MAX_CHARS,
            "platform label",
        ),
        (
            "description",
            "description_too_long",
            DESCRIPTION_MAX_CHARS,
            "description",
        ),
    ] {
        opt_string(map, key, what)?;
        if let Some(text) = map.get(key).and_then(Value::as_str) {
            if text.trim().chars().count() > max {
                return Err(Finding::error(
                    code,
                    id,
                    format!("{what}: the {noun} of {id} is longer than {max} characters"),
                ));
            }
        }
    }
    Ok(())
}

/// The coordinate review a change was made for (docs/gtfs-editor.md section 8):
/// absent or null, or a positive whole number. The apply ignores it.
fn position_review_id(map: &Map<String, Value>, what: &str) -> Result<Option<i64>, Finding> {
    match map.get("position_review_id") {
        None | Some(Value::Null) => Ok(None),
        Some(v) if v.as_i64().is_some_and(|n| n > 0) => Ok(v.as_i64()),
        _ => Err(Finding::error(
            "invalid_payload",
            "position_review_id",
            format!("{what}: position_review_id must be a positive whole number"),
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

/// The field of `after` that names the row a create makes.
pub fn create_id_field(entity: &str, op: &str) -> Option<&'static str> {
    match (entity, op) {
        ("stop", "create") => Some("stop_id"),
        ("route", "create") => Some("route_id"),
        ("station", "create") => Some("station_id"),
        _ => None,
    }
}

/// A create names its row twice, as `entity_key` and in `after`; either may be
/// left out and is filled from the other. Both empty (a stop whose id the server
/// mints) leaves both empty. A mismatch is left for [`check_payload`] to refuse.
pub fn settle_create_key(entity: &str, op: &str, key: &mut String, after: &mut Value) {
    let Some(field) = create_id_field(entity, op) else {
        return;
    };
    let Some(m) = after.as_object_mut() else {
        return;
    };
    let given = m
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    match given {
        Some(id) => {
            if key.is_empty() {
                *key = id.clone();
            }
            m.insert(field.to_string(), Value::String(id));
        }
        None if !key.is_empty() => {
            m.insert(field.to_string(), Value::String(key.clone()));
        }
        None => {
            m.remove(field);
        }
    }
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
                "description",
                "cluster_id",
                "regional_name",
                "hindi_name",
                "position_review_id",
            ];
            allow_only(m, &allowed, what)?;
            if m.keys().all(|k| k == "position_review_id") {
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
            stop_texts(m, entity_key, what)?;
            for k in ["cluster_id", "regional_name", "hindi_name"] {
                opt_string(m, k, what)?;
            }
            // a coordinate review's stop update is its move, never anything else
            if position_review_id(m, what)?.is_some() && !m.contains_key("lat") {
                return Err(Finding::error(
                    "invalid_payload",
                    "position_review_id",
                    format!("{what}: a change for a position review moves the stop (lat and lon)"),
                ));
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
                    "description",
                    "cluster_id",
                    "regional_name",
                    "hindi_name",
                    "position_review_id",
                ],
                what,
            )?;
            position_review_id(m, what)?;
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
            stop_texts(m, id, what)?;
            for k in ["stop_code", "cluster_id", "regional_name", "hindi_name"] {
                opt_string(m, k, what)?;
            }
            Ok(())
        }
        ("stop", "delete") | ("station", "delete") | ("route", "delete") => {
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
        ("stop", "merge") => {
            let m = obj(after, what)?;
            allow_only(
                m,
                &[
                    "into_stop_id",
                    "into_row_version",
                    "keep_name",
                    "keep_position",
                    "position_review_id",
                ],
                what,
            )?;
            position_review_id(m, what)?;
            let into = req_string(m, "into_stop_id", what)?;
            if into == entity_key.trim() {
                return Err(Finding::error(
                    "merge_same_stop",
                    into,
                    format!("{what}: a stop cannot be merged into itself"),
                ));
            }
            if entity_key.starts_with("prm_") || into.starts_with("prm_") {
                return Err(Finding::error(
                    "merge_prm_stop",
                    format!("{entity_key}->{into}"),
                    format!("{what}: prm_ stops are never merged"),
                ));
            }
            match m.get("into_row_version") {
                None | Some(Value::Null) => {}
                Some(v)
                    if v.as_i64()
                        .is_some_and(|n| (1..=i32::MAX as i64).contains(&n)) => {}
                _ => {
                    return Err(Finding::error(
                        "invalid_payload",
                        "into_row_version",
                        format!("{what}: into_row_version must be a positive whole number"),
                    ))
                }
            }
            for k in ["keep_name", "keep_position"] {
                match m.get(k) {
                    None | Some(Value::Null) => {}
                    Some(Value::String(v)) if v == "into" || v == "from" => {}
                    _ => {
                        return Err(Finding::error(
                            "invalid_payload",
                            k,
                            format!("{what}: {k} is \"into\" or \"from\""),
                        ))
                    }
                }
            }
            Ok(())
        }
        ("route", "create") => {
            let m = obj(after, what)?;
            allow_only(
                m,
                &[
                    "route_id",
                    "short_name",
                    "long_name",
                    "route_type",
                    "color",
                    "agency_id",
                ],
                what,
            )?;
            let id = req_string(m, "route_id", what)?;
            check_entity_id("route_id", id)?;
            if id != entity_key {
                return Err(Finding::error(
                    "invalid_payload",
                    id,
                    format!("{what}: entity_key must equal route_id"),
                ));
            }
            req_string(m, "short_name", what)?;
            for k in ["long_name", "color", "agency_id"] {
                opt_string(m, k, what)?;
            }
            if let Some(c) = m.get("color").and_then(Value::as_str) {
                if !is_hex_color(c) {
                    return Err(Finding::error(
                        "invalid_color",
                        c,
                        format!("{what}: color must be #RRGGBB"),
                    ));
                }
            }
            match m.get("route_type") {
                None | Some(Value::Null) => {}
                Some(v) if v.as_i64().is_some_and(valid_route_type) => {}
                Some(v) => {
                    return Err(Finding::error(
                        "invalid_route_type",
                        v.to_string(),
                        format!("{what}: route_type {v} is not a GTFS route type"),
                    ))
                }
            }
            Ok(())
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
                if !["osrm", "gps", "manual", "imported"].contains(&s) {
                    return Err(Finding::error(
                        "invalid_payload",
                        s,
                        format!("{what}: polyline_source is osrm, gps, manual or imported"),
                    ));
                }
            }
            Ok(())
        }
        ("route_stops", "replace") => {
            let m = obj(after, what)?;
            allow_only(m, &["rows", "base_rows_hash", "position_review_id"], what)?;
            position_review_id(m, what)?;
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
        ("station", "merge") => {
            let m = obj(after, what)?;
            allow_only(
                m,
                &[
                    "into_station_id",
                    "into_row_version",
                    "keep_name",
                    "keep_position",
                ],
                what,
            )?;
            let into = req_string(m, "into_station_id", what)?;
            if into == entity_key.trim() {
                return Err(Finding::error(
                    "merge_same_station",
                    into,
                    format!("{what}: a station cannot be merged into itself"),
                ));
            }
            match m.get("into_row_version") {
                None | Some(Value::Null) => {}
                Some(v)
                    if v.as_i64()
                        .is_some_and(|n| (1..=i32::MAX as i64).contains(&n)) => {}
                _ => {
                    return Err(Finding::error(
                        "invalid_payload",
                        "into_row_version",
                        format!("{what}: into_row_version must be a positive whole number"),
                    ))
                }
            }
            for k in ["keep_name", "keep_position"] {
                match m.get(k) {
                    None | Some(Value::Null) => {}
                    Some(Value::String(v)) if v == "into" || v == "from" => {}
                    _ => {
                        return Err(Finding::error(
                            "invalid_payload",
                            k,
                            format!("{what}: {k} is \"into\" or \"from\""),
                        ))
                    }
                }
            }
            Ok(())
        }
        ("station", "create") | ("station", "update") => {
            let m = obj(after, what)?;
            let create = op == "create";
            let allowed: &[&str] = if create {
                &[
                    "station_id",
                    "name",
                    "lat",
                    "lon",
                    "description",
                    "member_stop_ids",
                    "members",
                    "proposal_id",
                ]
            } else {
                &[
                    "name",
                    "lat",
                    "lon",
                    "description",
                    "member_stop_ids",
                    "members",
                    "proposal_id",
                ]
            };
            allow_only(m, allowed, what)?;
            // a station has a description like any stop, but never a platform label
            stop_texts(m, entity_key, what)?;
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
                if m.keys().all(|k| k == "proposal_id") {
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
            match m.get("proposal_id") {
                None | Some(Value::Null) => {}
                Some(v) if v.as_i64().is_some_and(|n| n > 0) => {}
                _ => {
                    return Err(Finding::error(
                        "invalid_payload",
                        "proposal_id",
                        format!("{what}: proposal_id must be a positive whole number"),
                    ))
                }
            }
            if m.contains_key("member_stop_ids") && m.contains_key("members") {
                return Err(Finding::error(
                    "invalid_payload",
                    "members",
                    format!("{what}: send members or member_stop_ids, not both"),
                ));
            }
            let mut seen = HashSet::new();
            let mut member = |s: &str| -> Result<(), Finding> {
                if s == entity_key {
                    return Err(Finding::error(
                        "invalid_payload",
                        s,
                        format!("{what}: a station cannot contain itself"),
                    ));
                }
                if !seen.insert(s.to_string()) {
                    return Err(Finding::error(
                        "invalid_payload",
                        s,
                        format!("{what}: {s} is listed twice"),
                    ));
                }
                Ok(())
            };
            let mut count = None;
            if let Some(members) = m.get("member_stop_ids") {
                let list = members.as_array().ok_or_else(|| {
                    Finding::error(
                        "invalid_payload",
                        "member_stop_ids",
                        format!("{what}: member_stop_ids must be a list"),
                    )
                })?;
                for v in list {
                    let s = v.as_str().ok_or_else(|| {
                        Finding::error(
                            "invalid_payload",
                            "member_stop_ids",
                            format!("{what}: member ids are strings"),
                        )
                    })?;
                    member(s)?;
                }
                count = Some(list.len());
            } else if let Some(members) = m.get("members") {
                let list = members.as_array().ok_or_else(|| {
                    Finding::error(
                        "invalid_payload",
                        "members",
                        format!("{what}: members must be a list of {{stop_id, platform_code?}}"),
                    )
                })?;
                for v in list {
                    let o = v.as_object().ok_or_else(|| {
                        Finding::error(
                            "invalid_payload",
                            "members",
                            format!("{what}: each member is {{stop_id, platform_code?}}"),
                        )
                    })?;
                    allow_only(o, &["stop_id", "platform_code"], what)?;
                    let s = req_string(o, "stop_id", what)?;
                    member(s)?;
                    opt_string(o, "platform_code", what)?;
                    if let Some(p) = o.get("platform_code").and_then(Value::as_str) {
                        if p.trim().chars().count() > PLATFORM_CODE_MAX_CHARS {
                            return Err(Finding::error(
                                "invalid_platform_code",
                                s,
                                format!(
                                    "{what}: the platform label of {s} is longer than {PLATFORM_CODE_MAX_CHARS} characters"
                                ),
                            ));
                        }
                    }
                }
                count = Some(list.len());
            } else if create {
                return Err(Finding::error(
                    "invalid_payload",
                    "members",
                    format!("{what}: members (or member_stop_ids) is required"),
                ));
            }
            // to empty or shrink a station below two stops, dissolve it instead
            if let Some(why) = count.and_then(|n| too_few_members(entity_key, n)) {
                return Err(Finding::error(
                    "too_few_members",
                    entity_key,
                    format!("{what}: {why}"),
                ));
            }
            Ok(())
        }
        ("feed_config", "update") => {
            let m = obj(after, what)?;
            allow_only(m, &["data_source"], what)?;
            match m.get("data_source").and_then(Value::as_str) {
                Some(s) if DATA_SOURCES.contains(&s) => Ok(()),
                _ => Err(Finding::error(
                    "invalid_data_source",
                    "data_source",
                    format!("{what}: data_source is 'db' or 'preprocessed'"),
                )),
            }
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

    #[test]
    fn feed_config_payloads() {
        let check = |after: Value| check_payload("feed_config", "update", "feed", &after);
        assert!(check(json!({"data_source": "db"})).is_ok());
        assert!(check(json!({"data_source": "preprocessed"})).is_ok());
        for bad in [
            json!({"data_source": "nonsense"}),
            json!({"data_source": null}),
            json!({}),
        ] {
            assert_eq!(check(bad).unwrap_err().code, "invalid_data_source");
        }
        // nothing else about a feed is a change
        let stray = check(json!({"data_source": "db", "version": 9})).unwrap_err();
        assert_eq!(stray.code, "invalid_payload");
        assert_eq!(
            check_payload("feed_config", "delete", "feed", &Value::Null)
                .unwrap_err()
                .code,
            "invalid_change"
        );
    }

    #[test]
    fn a_merge_may_carry_its_position_review() {
        let merge = |after: Value| check_payload("stop", "merge", "S", &after);
        assert!(merge(json!({"into_stop_id": "C", "position_review_id": 7})).is_ok());
        assert!(merge(json!({"into_stop_id": "C", "position_review_id": null})).is_ok());
        assert_eq!(
            merge(json!({"into_stop_id": "C", "position_review_id": 0}))
                .unwrap_err()
                .code,
            "invalid_payload"
        );
    }

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
    fn a_split_keeps_old_problems_old() {
        // S is called back to back with the wrong fare stage; C's stage is wrong too
        let live = vec![
            row("A", "NEW STOP", 1, "ADYAR"),
            row("S", "INTERMEDIATE STOP", 1, "WRONG"),
            row("S", "INTERMEDIATE STOP", 1, "WRONG"),
            row("C", "INTERMEDIATE STOP", 2, "ADYAR"),
            row("B", "NEW STOP", 2, "GUINDY"),
        ];
        let point = |rows: &[RouteRow], from: &str, to: &str| -> Vec<RouteRow> {
            rows.iter()
                .map(|r| {
                    let mut r = r.clone();
                    if r.stop_id.as_deref() == Some(from) {
                        r.stop_id = Some(to.into());
                    }
                    r
                })
                .collect()
        };
        let split = point(&live, "S", "ed_new");
        assert_eq!(
            repointed_stops(&live, &split),
            HashMap::from([("ed_new".to_string(), "S".to_string())])
        );
        // graded plainly, S's old problems look new under its new id
        let plain = grade_against_live(check_route_rows(&split), &check_route_rows(&live));
        assert_eq!(
            codes(&plain),
            vec![
                "fare_stage_mismatch",
                "fare_stage_mismatch",
                "stop_repeated"
            ]
        );
        // as a re-pointing, all four are what the route already had
        let graded = grade_repointed(&split, &live);
        assert!(codes(&graded).is_empty(), "{graded:?}");
        assert_eq!(graded.len(), 4, "{graded:?}");
        // a row that changes more than its stop brings its own problem
        let mut worse = split.clone();
        worse[3].stage_name = "OTHER".into();
        assert_eq!(
            codes(&grade_repointed(&worse, &live)),
            vec!["fare_stage_mismatch"]
        );
        // pointing a row at a stop the route already calls is no re-pointing:
        // the repeat it makes is new
        let onto_a = vec![
            row("A", "NEW STOP", 1, "ADYAR"),
            row("A", "NEW STOP", 2, "GUINDY"),
        ];
        let before = vec![
            row("A", "NEW STOP", 1, "ADYAR"),
            row("B", "NEW STOP", 2, "GUINDY"),
        ];
        assert!(repointed_stops(&before, &onto_a).is_empty());
        assert_eq!(
            codes(&grade_repointed(&onto_a, &before)),
            vec!["stop_repeated"]
        );
        // one new id for two live stops is not a re-pointing either
        let mut two = live.clone();
        two[3].stop_id = Some("ed_new".into());
        two[1].stop_id = Some("ed_new".into());
        two[2].stop_id = Some("ed_new".into());
        assert!(repointed_stops(&live, &two).is_empty());
        // nor a list of another length
        assert!(repointed_stops(&live, &split[1..]).is_empty());
        // a stage name missing names the stop as Some("id")
        let mut unnamed = live.clone();
        unnamed[1].stage_name = " ".into();
        let graded = grade_repointed(&point(&unnamed, "S", "ed_new"), &unnamed);
        assert!(codes(&graded).is_empty(), "{graded:?}");
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
        // a move made from a position review names it
        let update = |after: Value| check_payload("stop", "update", "A", &after);
        assert!(update(json!({"lat": 13.0, "lon": 80.2, "position_review_id": 7})).is_ok());
        assert!(update(json!({"lat": 13.0, "lon": 80.2, "position_review_id": null})).is_ok());
        for bad in [
            json!({"position_review_id": 7}),
            json!({"name": "ADYAR", "position_review_id": 7}),
            json!({"lat": 13.0, "lon": 80.2, "position_review_id": "7"}),
            json!({"lat": 13.0, "lon": 80.2, "position_review_id": 0}),
            json!({"lat": 13.0, "lon": 80.2, "position_review_id": 7.5}),
        ] {
            assert!(update(bad.clone()).is_err(), "{bad}");
        }
        // and a split's new stop and stop lists carry it too, stored and ignored
        let create = |review: Value| {
            check_payload(
                "stop",
                "create",
                "ed_0000000001",
                &json!({"stop_id": "ed_0000000001", "name": "New", "lat": 13.0, "lon": 80.0, "position_review_id": review}),
            )
        };
        assert!(create(json!(7)).is_ok() && create(Value::Null).is_ok());
        assert!(create(json!("7")).is_err() && create(json!(-1)).is_err());
        let replace = |review: Value| {
            check_payload(
                "route_stops",
                "replace",
                "R",
                &json!({"base_rows_hash": "abc", "position_review_id": review,
                        "rows": [{"stop_id": "A", "stop_type": "NEW STOP", "stage_no": 1, "stage_name": "X"}]}),
            )
        };
        assert!(replace(json!(7)).is_ok() && replace(Value::Null).is_ok());
        assert!(replace(json!(true)).is_err());
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
        assert!(check_payload(
            "station",
            "update",
            "S",
            &json!({"member_stop_ids": ["A", "B"]})
        )
        .is_ok());
        // a station groups at least two stops; dissolving one is station/delete
        for members in [json!(["A"]), json!([])] {
            let err = check_payload(
                "station",
                "update",
                "S",
                &json!({ "member_stop_ids": members }),
            )
            .unwrap_err();
            assert_eq!(err.code, "too_few_members", "{members}");
        }
        assert_eq!(
            check_payload("station", "create", "S", &json!({"station_id": "S", "name": "Adyar", "lat": 13.0, "lon": 80.2, "member_stop_ids": ["A"]}))
                .unwrap_err()
                .message,
            "station/create: a station groups at least two stops, and S would have only one"
        );
        assert!(check_payload("trip", "update", "T", &json!({})).is_err());
    }

    #[test]
    fn findings_carry_their_row_and_a_label() {
        let mut rows = good();
        rows[5].stage_no = 1;
        rows[5].stage_name = "ADYAR".into();
        let f = check_route_rows_labelled(&rows, &|i| format!("sequence {}", (i + 1) * 10));
        let mismatch = f.iter().find(|x| x.code == "fare_stage_mismatch").unwrap();
        assert_eq!(mismatch.row, Some(5));
        assert!(
            mismatch.message.starts_with("sequence 60 (D)"),
            "{}",
            mismatch.message
        );
        // the default label is the row number, as before
        let f = check_route_rows(&rows);
        assert!(f[0].message.starts_with("row 6 (D)"), "{}", f[0].message);
        // keys do not depend on the label, so grading matches either way
        let graded = grade_against_live(
            check_route_rows_labelled(&rows, &|i| format!("sequence {i}")),
            &check_route_rows(&rows),
        );
        assert!(codes(&graded).is_empty());
        let few = check_route_rows(&[row("A", "NEW STOP", 1, "X")]);
        assert_eq!(
            few.iter().find(|x| x.code == "too_few_stops").unwrap().row,
            None
        );
    }

    #[test]
    fn minted_stop_ids() {
        let ids: HashSet<String> = (0..2000).map(|_| mint_stop_id()).collect();
        assert_eq!(ids.len(), 2000);
        for id in &ids {
            assert_eq!(id.len(), 13, "{id}");
            assert!(id.starts_with("ed_"));
            assert!(id[3..]
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
            assert!(check_entity_id("stop_id", id).is_ok());
            assert!(check_payload(
                "stop",
                "create",
                id,
                &json!({"stop_id": id, "name": "New", "lat": 13.0, "lon": 80.0})
            )
            .is_ok());
        }
    }

    #[test]
    fn create_keys_settle_from_either_side() {
        let settle = |entity: &str, key: &str, after: Value| {
            let (mut k, mut a) = (key.to_string(), after);
            settle_create_key(entity, "create", &mut k, &mut a);
            (k, a)
        };
        assert_eq!(
            settle("stop", "", json!({"stop_id": " N1 ", "name": "x"})),
            ("N1".into(), json!({"stop_id": "N1", "name": "x"}))
        );
        assert_eq!(
            settle("route", "R9", json!({"short_name": "9"})),
            ("R9".into(), json!({"route_id": "R9", "short_name": "9"}))
        );
        // neither side: a stop the server mints an id for
        assert_eq!(
            settle("stop", "", json!({"stop_id": null, "name": "x"})),
            ("".into(), json!({"name": "x"}))
        );
        // a mismatch is left for check_payload to refuse
        let (k, a) = settle("station", "S1", json!({"station_id": "S2"}));
        assert_eq!((k.as_str(), &a["station_id"]), ("S1", &json!("S2")));
        // not a create: untouched
        let (mut k, mut a) = (String::new(), json!({"name": "x"}));
        settle_create_key("stop", "update", &mut k, &mut a);
        assert_eq!((k.as_str(), a), ("", json!({"name": "x"})));
    }

    #[test]
    fn route_create_and_delete_shapes() {
        let ok = |after: Value| check_payload("route", "create", "R9", &after);
        assert!(ok(json!({"route_id": "R9", "short_name": "9"})).is_ok());
        assert!(ok(
            json!({"route_id": "R9", "short_name": "9", "long_name": "A To B",
            "route_type": 3, "color": "#00aa11", "agency_id": "CUMTA"})
        )
        .is_ok());
        assert!(ok(json!({"route_id": "R9", "short_name": "9", "route_type": 715})).is_ok());
        let code = |after: Value| ok(after).unwrap_err().code;
        assert_eq!(code(json!({"route_id": "R9"})), "invalid_payload");
        assert_eq!(
            code(json!({"route_id": "R9", "short_name": " "})),
            "invalid_payload"
        );
        assert_eq!(
            code(json!({"route_id": "R:9", "short_name": "9"})),
            "invalid_id"
        );
        assert_eq!(
            code(json!({"route_id": "R8", "short_name": "9"})),
            "invalid_payload"
        );
        assert_eq!(
            code(json!({"route_id": "R9", "short_name": "9", "color": "red"})),
            "invalid_color"
        );
        assert_eq!(
            code(json!({"route_id": "R9", "short_name": "9", "route_type": 9})),
            "invalid_route_type"
        );
        assert_eq!(
            code(json!({"route_id": "R9", "short_name": "9", "route_type": "3"})),
            "invalid_route_type"
        );
        assert_eq!(
            code(json!({"route_id": "R9", "short_name": "9", "text_color": "#000000"})),
            "invalid_payload"
        );
        assert!(check_payload("route", "delete", "R9", &Value::Null).is_ok());
        assert!(check_payload("route", "delete", "R9", &json!({})).is_err());
        assert!(valid_route_type(0) && valid_route_type(12) && valid_route_type(1702));
        assert!(!valid_route_type(8) && !valid_route_type(99) && !valid_route_type(-1));
    }

    #[test]
    fn descriptions_and_platform_labels_have_a_length() {
        let update = |after: Value| check_payload("stop", "update", "S1", &after);
        assert!(update(json!({"description": "Opposite the temple tank"})).is_ok());
        // null and blank clear it; on its own it is a change
        assert!(update(json!({"description": null})).is_ok());
        assert!(update(json!({"description": "  "})).is_ok());
        assert_eq!(
            update(json!({"description": 5})).unwrap_err().code,
            "invalid_payload"
        );
        // characters, not bytes, once trimmed
        let fits = format!("  {}  ", "é".repeat(DESCRIPTION_MAX_CHARS));
        assert!(update(json!({"description": fits})).is_ok());
        let long = "x".repeat(DESCRIPTION_MAX_CHARS + 1);
        let f = update(json!({"description": long})).unwrap_err();
        assert_eq!(f.code, "description_too_long");
        assert!(f.message.contains("S1") && f.message.contains("500"));
        let label = "x".repeat(PLATFORM_CODE_MAX_CHARS + 1);
        assert_eq!(
            update(json!({"platform_code": label})).unwrap_err().code,
            "invalid_platform_code"
        );
        let create = |extra: Value| {
            let mut after = json!({"stop_id": "N1", "name": "New", "lat": 13.0, "lon": 80.2});
            after
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            check_payload("stop", "create", "N1", &after)
        };
        assert!(create(json!({"description": "By gate 2", "platform_code": "Towards X"})).is_ok());
        assert_eq!(
            create(json!({"description": long})).unwrap_err().code,
            "description_too_long"
        );
        // a station has a description, never a platform label of its own
        let station = |op: &str, after: Value| check_payload("station", op, "ST", &after);
        assert!(station("update", json!({"description": "Both sides of the road"})).is_ok());
        assert!(station("update", json!({"description": null})).is_ok());
        assert_eq!(
            station("update", json!({"description": long}))
                .unwrap_err()
                .code,
            "description_too_long"
        );
        assert_eq!(
            station("update", json!({"platform_code": "x"}))
                .unwrap_err()
                .code,
            "invalid_payload"
        );
        assert!(station(
            "create",
            json!({"station_id": "ST", "name": "Hub", "lat": 13.0, "lon": 80.2, "description": "The hub",
            "member_stop_ids": ["A", "B"]})
        )
        .is_ok());
    }

    #[test]
    fn station_members_with_platform_codes() {
        let station = |op: &str, after: Value| check_payload("station", op, "S", &after);
        let base = |members: Value| json!({"station_id": "S", "name": "Adyar", "lat": 13.0, "lon": 80.2, "members": members, "proposal_id": 7});
        assert!(station("create", base(json!([{"stop_id": "A", "platform_code": "Towards X"}, {"stop_id": "B"}, {"stop_id": "C", "platform_code": null}]))).is_ok());
        assert!(station(
            "update",
            json!({"members": [{"stop_id": "A"}, {"stop_id": "B"}]})
        )
        .is_ok());
        assert_eq!(
            station("update", json!({"members": [{"stop_id": "A"}]}))
                .unwrap_err()
                .code,
            "too_few_members"
        );
        let long = "x".repeat(PLATFORM_CODE_MAX_CHARS + 1);
        assert_eq!(
            station(
                "create",
                base(json!([{"stop_id": "A", "platform_code": long}, {"stop_id": "B"}]))
            )
            .unwrap_err()
            .code,
            "invalid_platform_code"
        );
        let fits = "é".repeat(PLATFORM_CODE_MAX_CHARS);
        assert!(station(
            "create",
            base(json!([{"stop_id": "A", "platform_code": fits}, {"stop_id": "B"}]))
        )
        .is_ok());
        for bad in [
            json!([{"stop_id": "A"}, {"stop_id": "A"}]),
            json!([{"stop_id": "S"}]),
            json!([{"stop_id": "A", "extra": 1}]),
            json!([{"platform_code": "x"}]),
            json!(["A"]),
            json!([{"stop_id": "A", "platform_code": 5}]),
        ] {
            assert!(station("create", base(bad.clone())).is_err(), "{bad}");
        }
        let mut both = base(json!([{"stop_id": "A"}]));
        both["member_stop_ids"] = json!(["A"]);
        assert!(station("create", both).is_err());
        let mut no_members = base(json!([]));
        no_members.as_object_mut().unwrap().remove("members");
        assert!(station("create", no_members).is_err());
        let mut bad_proposal = base(json!([{"stop_id": "A"}]));
        bad_proposal["proposal_id"] = json!("7");
        assert!(station("create", bad_proposal).is_err());
        assert!(station("update", json!({"proposal_id": 7})).is_err());

        let m = json!({"members": [{"stop_id": " A ", "platform_code": " Towards X "}, {"stop_id": "B"}, {"stop_id": "C", "platform_code": null}]});
        assert_eq!(
            station_members(m.as_object().unwrap()).unwrap(),
            vec![
                MemberSpec {
                    stop_id: "A".into(),
                    platform_code: Some(Some("Towards X".into()))
                },
                MemberSpec {
                    stop_id: "B".into(),
                    platform_code: None
                },
                MemberSpec {
                    stop_id: "C".into(),
                    platform_code: Some(None)
                },
            ]
        );
        let m = json!({"member_stop_ids": ["A"]});
        assert_eq!(
            station_members(m.as_object().unwrap()).unwrap(),
            vec![MemberSpec {
                stop_id: "A".into(),
                platform_code: None
            }]
        );
        assert!(station_members(json!({"name": "x"}).as_object().unwrap()).is_none());
    }

    #[test]
    fn merge_shapes() {
        let merge = |from: &str, after: Value| check_payload("stop", "merge", from, &after);
        assert!(merge("A", json!({"into_stop_id": "B"})).is_ok());
        assert!(merge("A", json!({"into_stop_id": "B", "into_row_version": 3, "keep_name": "from", "keep_position": "into"})).is_ok());
        assert_eq!(
            merge("A", json!({"into_stop_id": "A"})).unwrap_err().code,
            "merge_same_stop"
        );
        assert_eq!(
            merge("prm_A", json!({"into_stop_id": "B"}))
                .unwrap_err()
                .code,
            "merge_prm_stop"
        );
        assert_eq!(
            merge("A", json!({"into_stop_id": "prm_B"}))
                .unwrap_err()
                .code,
            "merge_prm_stop"
        );
        for bad in [
            json!({}),
            json!({"into_stop_id": "B", "keep_name": "both"}),
            json!({"into_stop_id": "B", "into_row_version": 0}),
            json!({"into_stop_id": "B", "into_row_version": "3"}),
            json!({"into_stop_id": "B", "name": "x"}),
            Value::Null,
        ] {
            assert!(merge("A", bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn station_merge_shapes() {
        let merge = |from: &str, after: Value| check_payload("station", "merge", from, &after);
        assert!(merge("STN_A", json!({"into_station_id": "STN_B"})).is_ok());
        assert!(merge(
            "STN_A",
            json!({"into_station_id": "STN_B", "into_row_version": 3,
                   "keep_name": "from", "keep_position": "into"})
        )
        .is_ok());
        assert_eq!(
            merge("STN_A", json!({"into_station_id": "STN_A"}))
                .unwrap_err()
                .code,
            "merge_same_station"
        );
        // a station merge is its own change type: the stop merge's field names
        // and its prm_ rule are not borrowed
        assert!(merge("STN_A", json!({"into_stop_id": "STN_B"})).is_err());
        assert!(merge("prm_A", json!({"into_station_id": "STN_B"})).is_ok());
        for bad in [
            json!({}),
            json!({"into_station_id": ""}),
            json!({"into_station_id": "STN_B", "keep_name": "both"}),
            json!({"into_station_id": "STN_B", "keep_position": true}),
            json!({"into_station_id": "STN_B", "into_row_version": 0}),
            json!({"into_station_id": "STN_B", "into_row_version": "3"}),
            json!({"into_station_id": "STN_B", "name": "x"}),
            json!({"into_station_id": "STN_B", "position_review_id": 7}),
            json!({"into_station_id": 7}),
            Value::Null,
        ] {
            assert!(merge("STN_A", bad.clone()).is_err(), "{bad}");
        }
        // a stop is never merged with the station change type, and the other way
        assert!(check_payload("stop", "merge", "A", &json!({"into_station_id": "B"})).is_err());
    }

    #[test]
    fn merge_effect_on_a_route() {
        let r = |q: i32, s: Option<&str>, t: &str| (q, s.map(str::to_string), t.to_string());
        // MANALI RD.JN on 56D: one id at stage 1, the other at stage 2, back to back
        let rows = vec![
            r(1, Some("FROM"), "NEW STOP"),
            r(2, Some("INTO"), "NEW STOP"),
            r(3, Some("X"), "NEW STOP"),
        ];
        let e = merge_effect(&rows, "FROM", "INTO");
        assert_eq!(e.repeats, vec![(1, 2)]);
        assert_eq!((e.from_seqs, e.into_seqs), (vec![1], vec![2]));
        // markers and unserved rows do not separate two calls
        let rows = vec![
            r(1, Some("INTO"), "NEW STOP"),
            r(2, None, "ROUTE CORRECTION"),
            r(3, Some("J"), "JUMP STOP"),
            r(4, Some("FROM"), "INTERMEDIATE STOP"),
        ];
        assert_eq!(merge_effect(&rows, "FROM", "INTO").repeats, vec![(1, 4)]);
        // apart: no repeat, both called
        let rows = vec![
            r(1, Some("FROM"), "NEW STOP"),
            r(2, Some("X"), "INTERMEDIATE STOP"),
            r(3, Some("INTO"), "NEW STOP"),
        ];
        let e = merge_effect(&rows, "FROM", "INTO");
        assert!(e.repeats.is_empty());
        assert_eq!((e.from_seqs, e.into_seqs), (vec![1], vec![3]));
        // an old repeat of the same id is not the merge's doing
        let rows = vec![
            r(1, Some("FROM"), "NEW STOP"),
            r(2, Some("FROM"), "INTERMEDIATE STOP"),
        ];
        assert!(merge_effect(&rows, "FROM", "INTO").repeats.is_empty());
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
