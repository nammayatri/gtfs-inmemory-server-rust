//! A whole feed in the shape the editor's tables hold it (docs/gtfs-editor.md
//! section 18), built from a zip's files or read back from the tables.
//!
//! Most files are rows of values: stops, routes, agencies, pathways and every
//! other record file keep what the file said, canonicalised by the
//! [`spec`](super::spec). The timetable is not kept as `stop_times.txt` rows.
//! It is kept the way section 16 stores it:
//!
//! - a **pattern** is a route's stop order together with what each stop time
//!   says besides the time - `stop_sequence`, `stop_headsign`, pickup and drop
//!   off, `timepoint`, continuous stopping, `shape_dist_traveled`, booking rules.
//!   Trips of one stop order that differ in any of those are separate patterns
//!   (they share the public pattern id GIMS computes from the stops alone, and
//!   GIMS serves them as one). A route's first pattern is its longest stop
//!   order, the first of them on a tie, as the preprocessor picks it;
//! - a **timing profile** is a pattern's arrival and departure offsets from a
//!   trip's reference time, stored once for every trip that runs to it;
//! - a **trip** is a pattern, a profile, a service, a reference time (its first
//!   arrival) and its own fields, plus any frequency windows.

use super::read::{RawFeed, RawTable};
use super::spec::{self, FieldSpec, FileSpec, Key, Storage};
use super::{Finding, Level};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// A row's values by field name; an empty cell is absent.
pub type Row = BTreeMap<&'static str, Value>;

/// The `stop_times.txt` fields a pattern keeps per stop (everything but the
/// trip, the stop and the times).
pub const PATTERN_STOP_FIELDS: &[&str] = &[
    "stop_sequence",
    "stop_headsign",
    "pickup_type",
    "drop_off_type",
    "continuous_pickup",
    "continuous_drop_off",
    "shape_dist_traveled",
    "timepoint",
    "pickup_booking_rule_id",
    "drop_off_booking_rule_id",
];

/// The `trips.txt` fields a trip keeps in `values` (its id, route and service
/// are fields of their own).
pub const TRIP_FIELDS: &[&str] = &[
    "trip_headsign",
    "trip_short_name",
    "direction_id",
    "block_id",
    "shape_id",
    "wheelchair_accessible",
    "bikes_allowed",
    "cars_allowed",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
    pub stop_id: String,
    /// Every other stops.txt field.
    pub values: Row,
    /// Its line in stops.txt: GIMS keeps the last stop read of several sharing
    /// a code, so the order is data.
    pub sort_key: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub route_id: String,
    pub values: Row,
    pub sort_key: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PatternStop {
    pub stop_id: String,
    /// The [`PATTERN_STOP_FIELDS`] given.
    pub values: Row,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub route_id: String,
    pub pattern_key: i16,
    pub stops: Vec<PatternStop>,
}

impl Pattern {
    pub fn stop_ids(&self) -> Vec<&str> {
        self.stops.iter().map(|s| s.stop_id.as_str()).collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub route_id: String,
    pub pattern_key: i16,
    pub profile_key: i32,
    pub arrival: Vec<i32>,
    pub departure: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frequency {
    pub start_s: i32,
    pub end_s: i32,
    pub headway_s: i32,
    pub exact_times: Option<i16>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Trip {
    pub trip_id: String,
    pub route_id: String,
    pub service_id: String,
    pub pattern_key: i16,
    /// None: the feed's default timing.
    pub profile_key: Option<i32>,
    pub ref_s: i32,
    /// The [`TRIP_FIELDS`] given.
    pub values: Row,
    pub frequencies: Vec<Frequency>,
    pub sort_key: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub service_id: String,
    /// Monday first; None for a service calendar.txt does not list.
    pub days: Option<[bool; 7]>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    /// `(ISO date, exception_type)`, in file order.
    pub dates: Vec<(String, i16)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShapePoint {
    pub sequence: i64,
    pub lat: f64,
    pub lon: f64,
    pub dist: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub shape_id: String,
    pub points: Vec<ShapePoint>,
}

/// One row of a record file.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// The row's key: its id, the gtfs_id for feed_info, or a minted
    /// `row_id` for a file with no id of its own.
    pub key: String,
    pub values: Row,
    pub sort_key: Option<i32>,
}

/// A whole feed.
#[derive(Debug, Clone, Default)]
pub struct FeedModel {
    pub gtfs_id: String,
    pub stops: Vec<Stop>,
    pub routes: Vec<Route>,
    pub patterns: Vec<Pattern>,
    pub profiles: Vec<Profile>,
    pub trips: Vec<Trip>,
    pub services: Vec<Service>,
    pub shapes: Vec<Shape>,
    /// Record files other than shapes, by entity (`pathway`).
    pub records: BTreeMap<&'static str, Vec<Record>>,
    /// Cells the import could not keep, as `(file, row key, field)`: the round
    /// trip does not expect them back.
    pub dropped: BTreeSet<(String, String, String)>,
    /// `(run_s, dwell_s)`: the timing of a trip with no profile.
    pub default_timing: (i32, i32),
}

/// The feed default timing a new feed row gets (`gtfs_feed.default_run_s`,
/// `default_dwell_s`).
pub const DEFAULT_TIMING: (i32, i32) = (120, 15);

/// How [`FeedModel::from_raw`] builds the timetable.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildOptions {
    /// `(run_s, dwell_s)`: a trip whose offsets are exactly the feed's default
    /// timing stores no profile. None stores every timing explicitly.
    pub default_timing: Option<(i32, i32)>,
}

fn file_spec(name: &str) -> &'static FileSpec {
    spec::file(name).expect("a file of the reference")
}

/// The key a compare and the model use for a row of a record file.
pub fn natural_key(spec: &FileSpec, values: &Row) -> String {
    let text = |f: &str| {
        values
            .get(f)
            .map(|v| spec::to_text(spec.field(f).expect("a key field"), &Some(v.clone())))
            .unwrap_or_default()
    };
    match spec.key() {
        Some(Key::Field(f)) => text(f),
        Some(Key::Minted { natural }) => natural
            .iter()
            .map(|f| text(f))
            .collect::<Vec<_>>()
            .join("\u{1f}"),
        Some(Key::Feed) | None => String::new(),
    }
}

/// Read one row of a file into canonical values. A bad value in a required
/// field is an error (the row cannot be kept); in any other field a warning,
/// and the cell is dropped.
fn read_row(
    spec: &FileSpec,
    table: &RawTable,
    row: &[String],
    line: usize,
    findings: &mut Vec<Finding>,
    dropped: &mut Vec<&'static str>,
) -> Option<Row> {
    let mut out = Row::new();
    let mut ok = true;
    for fs in spec.fields {
        let Some(i) = table.col(fs.name) else {
            continue;
        };
        let cell = row.get(i).map(String::as_str).unwrap_or("");
        match spec::from_text(fs, cell) {
            Ok(Some(v)) => {
                out.insert(fs.name, v);
            }
            Ok(None) => {}
            Err(why) => {
                let required = fs.presence == spec::Presence::Required;
                let f = if required {
                    ok = false;
                    Finding::error("invalid_value", format!("{} {why}: {cell:?}", fs.name))
                } else {
                    dropped.push(fs.name);
                    Finding::warning(
                        "invalid_value",
                        format!("{} {why}: {cell:?}; the cell was left out", fs.name),
                    )
                };
                findings.push(f.at(spec.name, Some(line), Some(fs.name)));
            }
        }
    }
    ok.then_some(out)
}

/// The values of a row that does read, for naming a row that does not.
fn read_partial(spec: &FileSpec, table: &RawTable, row: &[String]) -> Row {
    spec.fields
        .iter()
        .filter_map(|fs| {
            let i = table.col(fs.name)?;
            let v = spec::from_text(fs, row.get(i)?).ok()??;
            Some((fs.name, v))
        })
        .collect()
}

fn text(row: &Row, field: &str) -> Option<String> {
    row.get(field).and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn int(row: &Row, field: &str) -> Option<i64> {
    row.get(field).and_then(Value::as_i64)
}

fn float(row: &Row, field: &str) -> Option<f64> {
    row.get(field).and_then(Value::as_f64)
}

/// Warn once per file about columns the reference does not have.
fn unknown_columns(spec: &FileSpec, table: &RawTable, findings: &mut Vec<Finding>) {
    let unknown: Vec<&str> = table
        .header
        .iter()
        .map(String::as_str)
        .filter(|h| !h.is_empty() && spec.field(h).is_none())
        .collect();
    if !unknown.is_empty() {
        findings.push(
            Finding::warning(
                "unknown_field",
                format!(
                    "{} are not fields of {} and were left out",
                    unknown.join(", "),
                    spec.name
                ),
            )
            .at(spec.name, None, None),
        );
    }
}

/// Rows of a file keyed by `key_of`, refusing a key seen twice.
fn keyed<'a>(
    spec: &FileSpec,
    rows: impl Iterator<Item = (usize, &'a str)>,
    findings: &mut Vec<Finding>,
) -> HashSet<usize> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut dup = HashSet::new();
    for (line, key) in rows {
        if let Some(first) = seen.get(key) {
            findings.push(
                Finding::error(
                    "duplicate_key",
                    format!("{key:?} is also on line {first}; only the first is kept"),
                )
                .at(spec.name, Some(line), None),
            );
            dup.insert(line);
        } else {
            seen.insert(key, line);
        }
    }
    dup
}

impl FeedModel {
    /// Build the model of a feed from its files. Findings say what could not
    /// be kept; an error means the feed cannot be stored as it is.
    pub fn from_raw(raw: &RawFeed, gtfs_id: &str, opts: BuildOptions) -> (FeedModel, Vec<Finding>) {
        let mut m = FeedModel {
            gtfs_id: gtfs_id.to_string(),
            default_timing: opts.default_timing.unwrap_or(DEFAULT_TIMING),
            ..Default::default()
        };
        let mut findings = Vec::new();
        for required in [
            "agency.txt",
            "stops.txt",
            "routes.txt",
            "trips.txt",
            "stop_times.txt",
        ] {
            if raw.table(required).is_none() {
                findings.push(
                    Finding::error("missing_file", format!("{required} is required"))
                        .at(required, None, None),
                );
            }
        }
        if raw.table("calendar.txt").is_none() && raw.table("calendar_dates.txt").is_none() {
            findings.push(Finding::error(
                "missing_file",
                "one of calendar.txt and calendar_dates.txt is required",
            ));
        }
        for (name, table) in &raw.files {
            unknown_columns(file_spec(name), table, &mut findings);
        }
        m.read_stops(raw, &mut findings);
        m.read_routes(raw, &mut findings);
        m.read_services(raw, &mut findings);
        m.read_records(raw, &mut findings);
        m.read_shapes(raw, &mut findings);
        m.read_locations(raw, &mut findings);
        m.read_timetable(raw, opts, &mut findings);
        m.check_references(&mut findings);
        (m, findings)
    }

    fn drop_cells(&mut self, file: &str, key: &str, fields: &[&str]) {
        for f in fields {
            self.dropped
                .insert((file.to_string(), key.to_string(), f.to_string()));
        }
    }

    fn read_stops(&mut self, raw: &RawFeed, findings: &mut Vec<Finding>) {
        let Some(t) = raw.table("stops.txt") else {
            return;
        };
        let spec = file_spec("stops.txt");
        let dup = keyed(
            spec,
            t.rows
                .iter()
                .enumerate()
                .map(|(i, r)| (i + 2, t.cell(r, "stop_id"))),
            findings,
        );
        for (i, row) in t.rows.iter().enumerate() {
            let line = i + 2;
            if dup.contains(&line) {
                continue;
            }
            let mut dropped = Vec::new();
            let Some(mut values) = read_row(spec, t, row, line, findings, &mut dropped) else {
                continue;
            };
            let Some(stop_id) = text(&values, "stop_id") else {
                continue;
            };
            values.remove("stop_id");
            let lt = int(&values, "location_type").unwrap_or(0);
            // the tables need a position and a name on every stop: the spec
            // lets a generic node or boarding area go without them, no shipped
            // feed does, and every reader of gtfs_stop takes them as given
            if !values.contains_key("stop_lat") || !values.contains_key("stop_lon") {
                findings.push(
                    Finding::error(
                        "stop_without_position",
                        format!("stop {stop_id} has no stop_lat / stop_lon; every stop the editor keeps has a position"),
                    )
                    .at("stops.txt", Some(line), Some("stop_lat")),
                );
                continue;
            }
            if !values.contains_key("stop_name") && lt <= 2 {
                findings.push(
                    Finding::error(
                        "stop_without_name",
                        format!(
                            "stop {stop_id} has no stop_name, which location type {lt} requires"
                        ),
                    )
                    .at("stops.txt", Some(line), Some("stop_name")),
                );
                continue;
            }
            self.drop_cells("stops.txt", &stop_id, &dropped);
            self.stops.push(Stop {
                stop_id,
                values,
                sort_key: Some(i as i32),
            });
        }
    }

    fn read_routes(&mut self, raw: &RawFeed, findings: &mut Vec<Finding>) {
        let Some(t) = raw.table("routes.txt") else {
            return;
        };
        let spec = file_spec("routes.txt");
        let dup = keyed(
            spec,
            t.rows
                .iter()
                .enumerate()
                .map(|(i, r)| (i + 2, t.cell(r, "route_id"))),
            findings,
        );
        for (i, row) in t.rows.iter().enumerate() {
            let line = i + 2;
            if dup.contains(&line) {
                continue;
            }
            let mut dropped = Vec::new();
            let Some(mut values) = read_row(spec, t, row, line, findings, &mut dropped) else {
                continue;
            };
            let Some(route_id) = text(&values, "route_id") else {
                continue;
            };
            values.remove("route_id");
            if !values.contains_key("route_short_name") && !values.contains_key("route_long_name") {
                findings.push(
                    Finding::warning(
                        "route_without_name",
                        format!("route {route_id} has neither a short nor a long name"),
                    )
                    .at("routes.txt", Some(line), None),
                );
            }
            self.drop_cells("routes.txt", &route_id, &dropped);
            self.routes.push(Route {
                route_id,
                values,
                sort_key: Some(i as i32),
            });
        }
    }

    fn read_services(&mut self, raw: &RawFeed, findings: &mut Vec<Finding>) {
        let mut by_id: BTreeMap<String, usize> = BTreeMap::new();
        if let Some(t) = raw.table("calendar.txt") {
            let spec = file_spec("calendar.txt");
            let dup = keyed(
                spec,
                t.rows
                    .iter()
                    .enumerate()
                    .map(|(i, r)| (i + 2, t.cell(r, "service_id"))),
                findings,
            );
            for (i, row) in t.rows.iter().enumerate() {
                let line = i + 2;
                if dup.contains(&line) {
                    continue;
                }
                let mut dropped = Vec::new();
                let Some(values) = read_row(spec, t, row, line, findings, &mut dropped) else {
                    continue;
                };
                let Some(id) = text(&values, "service_id") else {
                    continue;
                };
                let mut days = [false; 7];
                for (d, name) in [
                    "monday",
                    "tuesday",
                    "wednesday",
                    "thursday",
                    "friday",
                    "saturday",
                    "sunday",
                ]
                .iter()
                .enumerate()
                {
                    days[d] = int(&values, name) == Some(1);
                }
                by_id.insert(id.clone(), self.services.len());
                self.services.push(Service {
                    service_id: id,
                    days: Some(days),
                    start_date: text(&values, "start_date"),
                    end_date: text(&values, "end_date"),
                    dates: vec![],
                });
            }
        }
        if let Some(t) = raw.table("calendar_dates.txt") {
            let spec = file_spec("calendar_dates.txt");
            let mut seen: HashSet<(String, String)> = HashSet::new();
            for (i, row) in t.rows.iter().enumerate() {
                let line = i + 2;
                let mut dropped = Vec::new();
                let Some(values) = read_row(spec, t, row, line, findings, &mut dropped) else {
                    continue;
                };
                let (Some(id), Some(date), Some(kind)) = (
                    text(&values, "service_id"),
                    text(&values, "date"),
                    int(&values, "exception_type"),
                ) else {
                    continue;
                };
                if !seen.insert((id.clone(), date.clone())) {
                    findings.push(
                        Finding::error(
                            "duplicate_key",
                            format!("service {id} lists {date} twice; only the first is kept"),
                        )
                        .at("calendar_dates.txt", Some(line), None),
                    );
                    continue;
                }
                let at = *by_id.entry(id.clone()).or_insert_with(|| {
                    self.services.push(Service {
                        service_id: id.clone(),
                        days: None,
                        start_date: None,
                        end_date: None,
                        dates: vec![],
                    });
                    self.services.len() - 1
                });
                self.services[at].dates.push((date, kind as i16));
            }
        }
    }

    fn read_records(&mut self, raw: &RawFeed, findings: &mut Vec<Finding>) {
        for spec in spec::FILES {
            let (Some(entity), Some(key)) = (spec.entity(), spec.key()) else {
                continue;
            };
            if matches!(entity, "shape" | "location") {
                continue;
            }
            let Some(t) = raw.table(spec.name) else {
                continue;
            };
            let mut rows = Vec::new();
            let mut seen: HashMap<String, usize> = HashMap::new();
            for (i, row) in t.rows.iter().enumerate() {
                let line = i + 2;
                let mut dropped = Vec::new();
                let mut row_findings = Vec::new();
                let core = matches!(key, Key::Feed) || entity == "agency";
                let Some(values) = read_row(spec, t, row, line, &mut row_findings, &mut dropped)
                else {
                    if core {
                        // a feed cannot lose an agency, or its feed_info,
                        // quietly
                        findings.extend(row_findings);
                        continue;
                    }
                    // a pathway or a fare rule with a value that is not one
                    // is left out, not the whole feed with it
                    for mut f in row_findings {
                        if f.level == Level::Error {
                            f.level = Level::Warning;
                            f.message.push_str("; the row was left out");
                        }
                        findings.push(f);
                    }
                    let partial = read_partial(spec, t, row);
                    self.dropped.insert((
                        spec.name.to_string(),
                        natural_key(spec, &partial),
                        "*".into(),
                    ));
                    continue;
                };
                findings.extend(row_findings);
                let natural = natural_key(spec, &values);
                if let Some(first) = seen.get(&natural) {
                    findings.push(
                        Finding::error(
                            "duplicate_key",
                            format!("the same row as line {first}; only the first is kept"),
                        )
                        .at(spec.name, Some(line), None),
                    );
                    continue;
                }
                seen.insert(natural.clone(), line);
                let key = match key {
                    Key::Field(_) => natural.clone(),
                    Key::Feed => self.gtfs_id.clone(),
                    // deterministic, so importing the same zip twice gives the
                    // same ids; the editor mints r_ ids of its own
                    Key::Minted { .. } => format!("imp_{:06}", i + 1),
                };
                self.drop_cells(spec.name, &natural, &dropped);
                rows.push(Record {
                    key,
                    values,
                    sort_key: Some(i as i32),
                });
            }
            if matches!(key, Key::Feed) && rows.len() > 1 {
                findings.push(
                    Finding::error(
                        "duplicate_key",
                        "feed_info.txt has more than one row; only the first is kept",
                    )
                    .at(spec.name, None, None),
                );
                rows.truncate(1);
            }
            if !rows.is_empty() {
                self.records.insert(entity, rows);
            }
        }
        // feed_info names the feed: nandi's preprocessor takes the gtfs_id
        // from its feed_id
        if let Some(info) = self.records.get("feed_info").and_then(|r| r.first()) {
            if let Some(id) = text(&info.values, "feed_id").filter(|id| *id != self.gtfs_id) {
                findings.push(
                    Finding::error(
                        "feed_id_mismatch",
                        format!(
                            "feed_info.txt says feed_id {id}, and this feed is {}",
                            self.gtfs_id
                        ),
                    )
                    .at("feed_info.txt", Some(2), Some("feed_id")),
                );
            }
        }
    }

    fn read_shapes(&mut self, raw: &RawFeed, findings: &mut Vec<Finding>) {
        let Some(t) = raw.table("shapes.txt") else {
            return;
        };
        let spec = file_spec("shapes.txt");
        let mut by_id: BTreeMap<String, usize> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut shapes: Vec<Shape> = Vec::new();
        for (i, row) in t.rows.iter().enumerate() {
            let line = i + 2;
            let mut dropped = Vec::new();
            let Some(values) = read_row(spec, t, row, line, findings, &mut dropped) else {
                continue;
            };
            let (Some(id), Some(seq), Some(lat), Some(lon)) = (
                text(&values, "shape_id"),
                int(&values, "shape_pt_sequence"),
                float(&values, "shape_pt_lat"),
                float(&values, "shape_pt_lon"),
            ) else {
                continue;
            };
            let at = *by_id.entry(id.clone()).or_insert_with(|| {
                order.push(id.clone());
                shapes.push(Shape {
                    shape_id: id.clone(),
                    points: vec![],
                });
                shapes.len() - 1
            });
            if shapes[at].points.iter().any(|p| p.sequence == seq) {
                findings.push(
                    Finding::error(
                        "duplicate_key",
                        format!("shape {id} has point {seq} twice; only the first is kept"),
                    )
                    .at("shapes.txt", Some(line), None),
                );
                continue;
            }
            shapes[at].points.push(ShapePoint {
                sequence: seq,
                lat,
                lon,
                dist: float(&values, "shape_dist_traveled"),
            });
        }
        for s in &mut shapes {
            s.points.sort_by_key(|p| p.sequence);
            if s.points.len() < 2 {
                findings.push(
                    Finding::warning(
                        "shape_too_short",
                        format!("shape {} has fewer than two points", s.shape_id),
                    )
                    .at("shapes.txt", None, None),
                );
            }
        }
        self.shapes = shapes;
    }

    fn read_locations(&mut self, raw: &RawFeed, findings: &mut Vec<Finding>) {
        let Some(geo) = &raw.locations else { return };
        let features = geo["features"].as_array().cloned().unwrap_or_default();
        let mut rows = Vec::new();
        for (i, feat) in features.iter().enumerate() {
            let id =
                match &feat["id"] {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => {
                        findings.push(
                            Finding::error("invalid_value", format!("feature {} has no id", i + 1))
                                .at("locations.geojson", None, Some("id")),
                        );
                        continue;
                    }
                };
            let mut values = Row::new();
            values.insert("location_id", json!(id));
            for f in ["stop_name", "stop_desc"] {
                if let Some(v) = feat["properties"][f].as_str() {
                    values.insert(
                        if f == "stop_name" {
                            "stop_name"
                        } else {
                            "stop_desc"
                        },
                        json!(v),
                    );
                }
            }
            values.insert("geometry", feat["geometry"].clone());
            rows.push(Record {
                key: id,
                values,
                sort_key: Some(i as i32),
            });
        }
        if !rows.is_empty() {
            self.records.insert("location", rows);
        }
    }

    fn read_timetable(&mut self, raw: &RawFeed, opts: BuildOptions, findings: &mut Vec<Finding>) {
        let (Some(trips_t), Some(times_t)) = (raw.table("trips.txt"), raw.table("stop_times.txt"))
        else {
            return;
        };
        let trip_spec = file_spec("trips.txt");
        let st_spec = file_spec("stop_times.txt");
        let routes: HashSet<String> = self.routes.iter().map(|r| r.route_id.clone()).collect();
        let services: HashSet<String> =
            self.services.iter().map(|s| s.service_id.clone()).collect();
        let stops: HashSet<String> = self.stops.iter().map(|s| s.stop_id.clone()).collect();

        // ---- trips.txt
        struct TripRow {
            trip_id: String,
            route_id: String,
            service_id: String,
            values: Row,
            sort_key: i32,
        }
        let dup = keyed(
            trip_spec,
            trips_t
                .rows
                .iter()
                .enumerate()
                .map(|(i, r)| (i + 2, trips_t.cell(r, "trip_id"))),
            findings,
        );
        let mut trip_rows: Vec<TripRow> = Vec::new();
        let mut trip_at: HashMap<String, usize> = HashMap::new();
        let mut dropped_cells: Vec<(String, Vec<&'static str>)> = Vec::new();
        for (i, row) in trips_t.rows.iter().enumerate() {
            let line = i + 2;
            if dup.contains(&line) {
                continue;
            }
            let mut dropped = Vec::new();
            let Some(values) = read_row(trip_spec, trips_t, row, line, findings, &mut dropped)
            else {
                continue;
            };
            let (Some(trip_id), Some(route_id), Some(service_id)) = (
                text(&values, "trip_id"),
                text(&values, "route_id"),
                text(&values, "service_id"),
            ) else {
                continue;
            };
            if !routes.contains(&route_id) {
                findings.push(
                    Finding::error(
                        "route_not_found",
                        format!(
                            "trip {trip_id} runs route {route_id}, which routes.txt does not have"
                        ),
                    )
                    .at("trips.txt", Some(line), Some("route_id")),
                );
                continue;
            }
            if !services.contains(&service_id) {
                findings.push(
                    Finding::error("service_not_found", format!("trip {trip_id} runs on service {service_id}, which no calendar file has"))
                        .at("trips.txt", Some(line), Some("service_id")),
                );
                continue;
            }
            let own: Row = values
                .iter()
                .filter(|(k, _)| TRIP_FIELDS.contains(k))
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            dropped_cells.push((trip_id.clone(), dropped));
            trip_at.insert(trip_id.clone(), trip_rows.len());
            trip_rows.push(TripRow {
                trip_id,
                route_id,
                service_id,
                values: own,
                sort_key: i as i32,
            });
        }
        for (trip, dropped) in dropped_cells {
            self.drop_cells("trips.txt", &trip, &dropped);
        }

        // ---- stop_times.txt, grouped by trip
        struct Call {
            seq: i64,
            stop_id: String,
            arrival: Option<i64>,
            departure: Option<i64>,
            values: Row,
            line: usize,
        }
        let mut calls: Vec<Vec<Call>> = (0..trip_rows.len()).map(|_| Vec::new()).collect();
        let (mut flex, mut unknown_trip) = (0usize, 0usize);
        for (i, row) in times_t.rows.iter().enumerate() {
            let line = i + 2;
            let mut dropped = Vec::new();
            let Some(values) = read_row(st_spec, times_t, row, line, findings, &mut dropped) else {
                continue;
            };
            let Some(trip_id) = text(&values, "trip_id") else {
                continue;
            };
            let Some(&at) = trip_at.get(&trip_id) else {
                unknown_trip += 1;
                continue;
            };
            if values.contains_key("location_id") || values.contains_key("location_group_id") {
                flex += 1;
                continue;
            }
            let Some(stop_id) = text(&values, "stop_id") else {
                findings.push(
                    Finding::error(
                        "missing_value",
                        "stop_id is required on a stop time that names no Flex location",
                    )
                    .at("stop_times.txt", Some(line), Some("stop_id")),
                );
                continue;
            };
            if !stops.contains(&stop_id) {
                findings.push(
                    Finding::error(
                        "stop_not_found",
                        format!(
                            "trip {trip_id} calls at stop {stop_id}, which stops.txt does not have"
                        ),
                    )
                    .at("stop_times.txt", Some(line), Some("stop_id")),
                );
                continue;
            }
            let seq = int(&values, "stop_sequence").unwrap_or(0);
            if !dropped.is_empty() {
                self.drop_cells("stop_times.txt", &format!("{trip_id}\u{1f}{seq}"), &dropped);
            }
            let kept: Row = values
                .iter()
                .filter(|(k, _)| PATTERN_STOP_FIELDS.contains(k))
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            calls[at].push(Call {
                seq,
                stop_id,
                arrival: int(&values, "arrival_time"),
                departure: int(&values, "departure_time"),
                values: kept,
                line,
            });
        }
        if unknown_trip > 0 {
            findings.push(
                Finding::error(
                    "trip_not_found",
                    format!("{unknown_trip} stop times name a trip trips.txt does not have (or could not keep)"),
                )
                .at("stop_times.txt", None, Some("trip_id")),
            );
        }
        if flex > 0 {
            findings.push(
                Finding::warning(
                    "flex_stop_time",
                    format!("{flex} stop times call at a Flex location or location group; the editor keeps trips that call at stops only, and left them out"),
                )
                .at("stop_times.txt", None, None),
            );
        }

        // ---- patterns, profiles and trips, route by route
        #[derive(Default)]
        struct RoutePatterns {
            /// pattern signature -> index in `list`
            by_sig: HashMap<String, usize>,
            /// `(stops, first trip's sort key)`
            list: Vec<(Vec<PatternStop>, i32)>,
            /// offsets signature -> profile index per pattern
            profiles: Vec<Vec<(Vec<i32>, Vec<i32>)>>,
        }
        let mut per_route: BTreeMap<String, RoutePatterns> = BTreeMap::new();
        let mut repeated_seq: Vec<(String, i64, usize)> = Vec::new();
        let mut short_trips: Vec<String> = Vec::new();
        let mut dropped_trips: Vec<String> = Vec::new();
        let mut backwards_trips: Vec<(String, usize)> = Vec::new();
        struct Placed {
            trip: usize,
            pattern: usize,
            profile: Option<usize>,
            ref_s: i32,
        }
        let mut placed: Vec<Placed> = Vec::new();
        for (t, mut list) in calls.into_iter().enumerate() {
            let trip = &trip_rows[t];
            // a stable sort: stops sharing a stop_sequence keep the order the
            // file has them in, which is how the preprocessor reads them
            list.sort_by_key(|c| c.seq);
            if let Some(w) = list.windows(2).find(|w| w[0].seq == w[1].seq) {
                repeated_seq.push((trip.trip_id.clone(), w[0].seq, w[1].line));
            }
            if list.is_empty() {
                findings.push(
                    Finding::warning(
                        "trip_without_stops",
                        format!("trip {} has no stop times and was left out", trip.trip_id),
                    )
                    .at("trips.txt", None, Some("trip_id")),
                );
                dropped_trips.push(trip.trip_id.clone());
                continue;
            }
            if list.len() < 2 {
                short_trips.push(trip.trip_id.clone());
            }
            // times: both given, or one standing for the other; none at all is
            // a stop the editor cannot time (interpolation is not stored)
            let mut arrival = Vec::with_capacity(list.len());
            let mut departure = Vec::with_capacity(list.len());
            let mut untimed = None;
            for c in &list {
                match (c.arrival.or(c.departure), c.departure.or(c.arrival)) {
                    (Some(a), Some(d)) => {
                        arrival.push(a);
                        departure.push(d);
                    }
                    _ => {
                        untimed = Some(c.line);
                        break;
                    }
                }
            }
            if let Some(line) = untimed {
                findings.push(
                    Finding::error(
                        "untimed_stop_time",
                        format!("trip {} has a stop with no time; the editor keeps a time at every stop", trip.trip_id),
                    )
                    .at("stop_times.txt", Some(line), Some("arrival_time")),
                );
                continue;
            }
            let ref_s = arrival[0];
            let offsets_a: Vec<i32> = arrival.iter().map(|a| (a - ref_s) as i32).collect();
            let offsets_d: Vec<i32> = departure.iter().map(|d| (d - ref_s) as i32).collect();
            let backwards = offsets_a
                .iter()
                .zip(&offsets_d)
                .enumerate()
                .any(|(i, (a, d))| d < a || (i > 0 && *a < offsets_d[i - 1]));
            if backwards {
                // invalid GTFS, but what GIMS serves today: kept as the feed
                // has it, for ops to fix through a draft
                backwards_trips.push((trip.trip_id.clone(), list[0].line));
            }
            // the stop sequence is kept only when it is not simply 1 to n
            let plain = list.iter().enumerate().all(|(i, c)| c.seq == i as i64 + 1);
            let stops: Vec<PatternStop> = list
                .iter()
                .map(|c| {
                    let mut values = c.values.clone();
                    if plain {
                        values.remove("stop_sequence");
                    }
                    PatternStop {
                        stop_id: c.stop_id.clone(),
                        values,
                    }
                })
                .collect();
            let sig = serde_json::to_string(
                &stops
                    .iter()
                    .map(|s| json!([s.stop_id, s.values]))
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_default();
            let rp = per_route.entry(trip.route_id.clone()).or_default();
            let pattern = *rp.by_sig.entry(sig).or_insert_with(|| {
                rp.list.push((stops, trip.sort_key));
                rp.profiles.push(Vec::new());
                rp.list.len() - 1
            });
            let is_default = opts.default_timing.is_some_and(|(run, dwell)| {
                let d = crate::services::gtfs_timing::default_offsets(offsets_a.len(), run, dwell);
                d.arrival == offsets_a && d.departure == offsets_d
            });
            let profile = if is_default {
                None
            } else {
                let profs = &mut rp.profiles[pattern];
                Some(
                    match profs
                        .iter()
                        .position(|(a, d)| *a == offsets_a && *d == offsets_d)
                    {
                        Some(p) => p,
                        None => {
                            profs.push((offsets_a, offsets_d));
                            profs.len() - 1
                        }
                    },
                )
            };
            placed.push(Placed {
                trip: t,
                pattern,
                profile,
                ref_s: ref_s as i32,
            });
        }

        if let Some((trip, seq, line)) = repeated_seq.first() {
            findings.push(
                Finding::warning(
                    "duplicate_stop_sequence",
                    format!(
                        "{} trips give two stops one stop_sequence (the first: trip {trip}, {seq}); kept in the order the file has them",
                        repeated_seq.len()
                    ),
                )
                .at("stop_times.txt", Some(*line), Some("stop_sequence")),
            );
        }
        if let Some((first, line)) = backwards_trips.first() {
            findings.push(
                Finding::warning(
                    "timing_goes_backwards",
                    format!(
                        "{} trips arrive somewhere before they left the stop before, or leave before they arrive (the first: {first}); kept as the feed has them",
                        backwards_trips.len()
                    ),
                )
                .at("stop_times.txt", Some(*line), None),
            );
        }
        if let Some(first) = short_trips.first() {
            findings.push(
                Finding::warning(
                    "trip_too_short",
                    format!(
                        "{} trips call at only one stop (the first: {first}); kept as the feed has them",
                        short_trips.len()
                    ),
                )
                .at("stop_times.txt", None, None),
            );
        }
        for t in dropped_trips {
            self.dropped.insert(("trips.txt".into(), t, "*".into()));
        }

        // pattern keys: the longest stop order is 1 (first on a tie), the rest
        // follow in the order their first trip comes
        let mut keys: HashMap<(String, usize), i16> = HashMap::new();
        for (route_id, rp) in &per_route {
            let first = rp
                .list
                .iter()
                .enumerate()
                .max_by(|(ia, (a, ta)), (ib, (b, tb))| {
                    a.len().cmp(&b.len()).then(tb.cmp(ta)).then(ib.cmp(ia))
                })
                .map(|(i, _)| i)
                .unwrap_or(0);
            let mut rest: Vec<usize> = (0..rp.list.len()).filter(|i| *i != first).collect();
            rest.sort_by_key(|i| rp.list[*i].1);
            keys.insert((route_id.clone(), first), 1);
            for (n, i) in rest.into_iter().enumerate() {
                keys.insert((route_id.clone(), i), n as i16 + 2);
            }
            for (i, (stops, _)) in rp.list.iter().enumerate() {
                let pattern_key = keys[&(route_id.clone(), i)];
                self.patterns.push(Pattern {
                    route_id: route_id.clone(),
                    pattern_key,
                    stops: stops.clone(),
                });
                for (p, (a, d)) in rp.profiles[i].iter().enumerate() {
                    self.profiles.push(Profile {
                        route_id: route_id.clone(),
                        pattern_key,
                        profile_key: p as i32 + 1,
                        arrival: a.clone(),
                        departure: d.clone(),
                    });
                }
            }
        }
        self.patterns
            .sort_by(|a, b| (&a.route_id, a.pattern_key).cmp(&(&b.route_id, b.pattern_key)));
        self.profiles.sort_by(|a, b| {
            (&a.route_id, a.pattern_key, a.profile_key).cmp(&(
                &b.route_id,
                b.pattern_key,
                b.profile_key,
            ))
        });

        // frequencies.txt
        let mut freqs: HashMap<String, Vec<Frequency>> = HashMap::new();
        if let Some(t) = raw.table("frequencies.txt") {
            let spec = file_spec("frequencies.txt");
            for (i, row) in t.rows.iter().enumerate() {
                let line = i + 2;
                let mut dropped = Vec::new();
                let Some(values) = read_row(spec, t, row, line, findings, &mut dropped) else {
                    continue;
                };
                let (Some(trip), Some(start), Some(end), Some(headway)) = (
                    text(&values, "trip_id"),
                    int(&values, "start_time"),
                    int(&values, "end_time"),
                    int(&values, "headway_secs"),
                ) else {
                    continue;
                };
                if !trip_at.contains_key(&trip) {
                    findings.push(
                        Finding::error(
                            "trip_not_found",
                            format!("a headway names trip {trip}, which trips.txt does not have"),
                        )
                        .at("frequencies.txt", Some(line), Some("trip_id")),
                    );
                    continue;
                }
                freqs.entry(trip).or_default().push(Frequency {
                    start_s: start as i32,
                    end_s: end as i32,
                    headway_s: headway as i32,
                    exact_times: int(&values, "exact_times").map(|v| v as i16),
                });
            }
        }

        for p in placed {
            let trip = &trip_rows[p.trip];
            let pattern_key = keys[&(trip.route_id.clone(), p.pattern)];
            self.trips.push(Trip {
                trip_id: trip.trip_id.clone(),
                route_id: trip.route_id.clone(),
                service_id: trip.service_id.clone(),
                pattern_key,
                profile_key: p.profile.map(|x| x as i32 + 1),
                ref_s: p.ref_s,
                values: trip.values.clone(),
                frequencies: freqs.remove(&trip.trip_id).unwrap_or_default(),
                sort_key: trip.sort_key,
            });
        }
        self.trips.sort_by_key(|t| t.sort_key);
    }

    /// References the tables cannot hold broken: a parent station that does
    /// not exist (a foreign key) is an error; any other dangling id is a
    /// warning, kept as the feed has it.
    fn check_references(&self, findings: &mut Vec<Finding>) {
        let stop_ids: HashSet<&str> = self.stops.iter().map(|s| s.stop_id.as_str()).collect();
        let self_parents: Vec<&Stop> = self
            .stops
            .iter()
            .filter(|s| text(&s.values, "parent_station").as_deref() == Some(s.stop_id.as_str()))
            .collect();
        if let Some(first) = self_parents.first() {
            findings.push(
                Finding::warning(
                    "self_parent",
                    format!(
                        "{} stops are their own parent_station (the first: {}); kept as the feed has them",
                        self_parents.len(),
                        first.stop_id
                    ),
                )
                .at("stops.txt", first.sort_key.map(|k| k as usize + 2), Some("parent_station")),
            );
        }
        for s in &self.stops {
            if let Some(parent) = text(&s.values, "parent_station") {
                if parent == s.stop_id {
                    continue;
                } else if !stop_ids.contains(parent.as_str()) {
                    findings.push(
                        Finding::error(
                            "parent_not_found",
                            format!(
                                "stop {}'s parent_station {parent} is not in stops.txt",
                                s.stop_id
                            ),
                        )
                        .at(
                            "stops.txt",
                            s.sort_key.map(|k| k as usize + 2),
                            Some("parent_station"),
                        ),
                    );
                }
            }
        }
        // every other reference, by the spec
        let mut have: HashMap<(&str, &str), HashSet<String>> = HashMap::new();
        let mut add = |file: &'static str, field: &'static str, v: String| {
            have.entry((file, field)).or_default().insert(v);
        };
        for s in &self.stops {
            add("stops.txt", "stop_id", s.stop_id.clone());
            if let Some(z) = text(&s.values, "zone_id") {
                add("stops.txt", "zone_id", z);
            }
        }
        for r in &self.routes {
            add("routes.txt", "route_id", r.route_id.clone());
            if let Some(n) = text(&r.values, "network_id") {
                add("routes.txt", "network_id", n);
            }
        }
        for t in &self.trips {
            add("trips.txt", "trip_id", t.trip_id.clone());
        }
        for s in &self.services {
            add("calendar.txt", "service_id", s.service_id.clone());
            add("calendar_dates.txt", "service_id", s.service_id.clone());
        }
        for s in &self.shapes {
            add("shapes.txt", "shape_id", s.shape_id.clone());
        }
        for (entity, rows) in &self.records {
            let spec = spec::record_file(entity).expect("a record file");
            for r in rows {
                for fs in spec.fields {
                    if let Some(v) = text(&r.values, fs.name) {
                        add(spec.name, fs.name, v);
                    }
                }
            }
        }
        let mut dangling: BTreeMap<(String, String, String), usize> = BTreeMap::new();
        let mut check = |file: &str, row: &Row| {
            let spec = file_spec(file);
            for fs in spec.fields {
                if fs.refs.is_empty() || fs.name == "parent_station" {
                    continue;
                }
                let Some(v) = text(row, fs.name) else {
                    continue;
                };
                let found = fs
                    .refs
                    .iter()
                    .any(|t| have.get(&(t.file, t.field)).is_some_and(|s| s.contains(&v)));
                if !found {
                    let target = fs
                        .refs
                        .iter()
                        .map(|t| format!("{}.{}", t.file, t.field))
                        .collect::<Vec<_>>()
                        .join(" or ");
                    *dangling
                        .entry((file.to_string(), fs.name.to_string(), target))
                        .or_default() += 1;
                }
            }
        };
        for s in &self.stops {
            check("stops.txt", &s.values);
        }
        for r in &self.routes {
            check("routes.txt", &r.values);
        }
        for t in &self.trips {
            check("trips.txt", &t.values);
        }
        for p in &self.patterns {
            for s in &p.stops {
                check("stop_times.txt", &s.values);
            }
        }
        for (entity, rows) in &self.records {
            let spec = spec::record_file(entity).expect("a record file");
            for r in rows {
                check(spec.name, &r.values);
            }
        }
        for ((file, field, target), n) in dangling {
            findings.push(
                Finding::warning(
                    "reference_not_found",
                    format!("{n} rows name a {field} that is not in {target}; kept as the feed has them"),
                )
                .at(&file, None, Some(&field)),
            );
        }
    }

    /// Counts per file, for a report.
    pub fn counts(&self) -> BTreeMap<String, usize> {
        let mut c = BTreeMap::new();
        c.insert("stops.txt".into(), self.stops.len());
        c.insert("routes.txt".into(), self.routes.len());
        c.insert("trips.txt".into(), self.trips.len());
        c.insert(
            "stop_times.txt".into(),
            self.trips
                .iter()
                .map(|t| {
                    self.pattern(&t.route_id, t.pattern_key)
                        .map(|p| p.stops.len())
                        .unwrap_or(0)
                })
                .sum(),
        );
        c.insert("patterns".into(), self.patterns.len());
        c.insert("timing_profiles".into(), self.profiles.len());
        c.insert("services".into(), self.services.len());
        c.insert("shapes.txt".into(), self.shapes.len());
        c.insert(
            "frequencies.txt".into(),
            self.trips.iter().map(|t| t.frequencies.len()).sum(),
        );
        for (entity, rows) in &self.records {
            let spec = spec::record_file(entity).expect("a record file");
            c.insert(spec.name.to_string(), rows.len());
        }
        c
    }

    pub fn pattern(&self, route_id: &str, key: i16) -> Option<&Pattern> {
        self.patterns
            .iter()
            .find(|p| p.route_id == route_id && p.pattern_key == key)
    }

    pub fn profile(&self, route_id: &str, pattern: i16, key: i32) -> Option<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.route_id == route_id && p.pattern_key == pattern && p.profile_key == key)
    }
}

/// Whether a finding list lets a feed be stored.
pub fn storable(findings: &[Finding]) -> bool {
    !findings.iter().any(|f| f.level == Level::Error)
}

/// The spec entry of a record entity's key field, if it has one.
pub fn key_field(spec: &FileSpec) -> Option<&'static FieldSpec> {
    match spec.storage {
        Storage::Record {
            key: Key::Field(f), ..
        } => spec.field(f),
        _ => None,
    }
}
