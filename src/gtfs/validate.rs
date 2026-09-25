//! The feed report (docs/gtfs-editor.md section 18): what a whole feed breaks
//! of the GTFS reference, on the model a zip or the editor's tables give - so
//! one report reads a zip before it is imported and a feed as it stands.
//!
//! Row by row it checks what [`spec`] and [`rules`] say of one row: required
//! fields and the conditional rules. Across rows and files it checks:
//!
//! - the files a feed must have;
//! - every reference the reference names (`trips.service_id` to a service,
//!   `pathways.from_stop_id` to a stop, ...), to a row that exists;
//! - stations and what stands in them: a parent of the right type, and a stop
//!   that is its own parent (a warning: shipped feeds have them, and GIMS
//!   reads them);
//! - pathways between stops, never a station itself;
//! - times that never go backwards along a trip, and `shape_dist_traveled`
//!   that never decreases along a trip or a shape;
//! - calendars: a service that never runs, a feed none of whose services runs
//!   today or later, feed_info's end date past;
//! - several agencies: every route names one, and they share a timezone;
//! - what nothing uses (warnings): an agency, a route with no trips, a
//!   service, a shape, a level, a stop no stop order calls at.

use super::model::{FeedModel, Row};
use super::rules;
use super::spec::{self, FileSpec, Presence};
use super::{Finding, Level};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

/// The findings of one kind, counted, with a few of them.
#[derive(Debug, Clone, Serialize)]
pub struct CodeSummary {
    pub level: Level,
    pub code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub count: usize,
    pub samples: Vec<String>,
}

/// A report's findings, counted by kind.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub errors: usize,
    pub warnings: usize,
    /// Errors first, then the most frequent.
    pub codes: Vec<CodeSummary>,
}

/// Findings counted by `(level, code, file)`, with up to `samples` messages
/// of each.
pub fn summarise(findings: &[Finding], samples: usize) -> Report {
    let mut by: BTreeMap<(u8, &'static str, Option<String>), CodeSummary> = BTreeMap::new();
    let mut report = Report::default();
    for f in findings {
        match f.level {
            Level::Error => report.errors += 1,
            Level::Warning => report.warnings += 1,
        }
        let rank = if f.level == Level::Error { 0 } else { 1 };
        let s = by
            .entry((rank, f.code, f.file.clone()))
            .or_insert_with(|| CodeSummary {
                level: f.level,
                code: f.code,
                file: f.file.clone(),
                count: 0,
                samples: vec![],
            });
        s.count += 1;
        if s.samples.len() < samples {
            s.samples.push(f.message.clone());
        }
    }
    let mut codes: Vec<CodeSummary> = by.into_values().collect();
    codes.sort_by(|a, b| {
        (a.level != Level::Error)
            .cmp(&(b.level != Level::Error))
            .then(b.count.cmp(&a.count))
            .then(a.code.cmp(b.code))
    });
    report.codes = codes;
    report
}

/// A value as the text a reference compares.
fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn file_of(entity: &str) -> Option<&'static FileSpec> {
    spec::FILES.iter().find(|f| f.entity() == Some(entity))
}

fn int(row: &Row, f: &str) -> Option<i64> {
    row.get(f).and_then(Value::as_i64)
}

/// Every row the model keeps as a row of a file, as `(file, key, row)`, the
/// ids of its own included. stop_times and shapes are checked as the trips and
/// shapes they are.
fn rows(m: &FeedModel) -> Vec<(&'static FileSpec, String, Row)> {
    let spec_of = |name: &str| spec::file(name).expect("a file of the reference");
    let mut out = Vec::new();
    let stops = spec_of("stops.txt");
    for s in &m.stops {
        let mut r = s.values.clone();
        r.insert("stop_id", json!(s.stop_id));
        out.push((stops, s.stop_id.clone(), r));
    }
    let routes = spec_of("routes.txt");
    for x in &m.routes {
        let mut r = x.values.clone();
        r.insert("route_id", json!(x.route_id));
        out.push((routes, x.route_id.clone(), r));
    }
    let trips = spec_of("trips.txt");
    for t in &m.trips {
        let mut r = t.values.clone();
        r.insert("trip_id", json!(t.trip_id));
        r.insert("route_id", json!(t.route_id));
        r.insert("service_id", json!(t.service_id));
        out.push((trips, t.trip_id.clone(), r));
    }
    let (calendar, dates) = (spec_of("calendar.txt"), spec_of("calendar_dates.txt"));
    for s in &m.services {
        if let Some(days) = s.days {
            let mut r = Row::new();
            r.insert("service_id", json!(s.service_id));
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
                r.insert(name, json!(days[d] as i64));
            }
            if let Some(d) = &s.start_date {
                r.insert("start_date", json!(d));
            }
            if let Some(d) = &s.end_date {
                r.insert("end_date", json!(d));
            }
            out.push((calendar, s.service_id.clone(), r));
        }
        for (date, kind) in &s.dates {
            let mut r = Row::new();
            r.insert("service_id", json!(s.service_id));
            r.insert("date", json!(date));
            r.insert("exception_type", json!(kind));
            out.push((dates, format!("{} {date}", s.service_id), r));
        }
    }
    for (entity, records) in &m.records {
        let Some(fspec) = file_of(entity) else {
            continue;
        };
        for rec in records {
            out.push((fspec, rec.key.clone(), rec.values.clone()));
        }
    }
    out
}

/// What feed `m` breaks, as the module says; `today` is an ISO date.
pub fn validate(m: &FeedModel, today: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    let all = rows(m);
    let stop_type: HashMap<&str, i64> = m
        .stops
        .iter()
        .map(|s| {
            (
                s.stop_id.as_str(),
                int(&s.values, "location_type").unwrap_or(0),
            )
        })
        .collect();
    let records = |entity: &str| m.records.get(entity).map(Vec::len).unwrap_or(0);

    // ---- the files a feed must have
    let mut missing = |file: &str, why: &str| {
        out.push(
            Finding::error("missing_file", format!("the feed has no {file}: {why}"))
                .at(file, None, None),
        );
    };
    if records("agency") == 0 {
        missing("agency.txt", "every feed names its agency");
    }
    if m.stops.is_empty() && records("location") == 0 {
        missing(
            "stops.txt",
            "a feed that is not only Flex locations has stops",
        );
    }
    if m.routes.is_empty() {
        missing("routes.txt", "every feed has routes");
    }
    if m.trips.is_empty() {
        missing("trips.txt", "every feed has trips");
    }
    if m.services.is_empty() {
        missing(
            "calendar.txt",
            "a feed says when its trips run, in calendar.txt or calendar_dates.txt",
        );
    }
    if records("translation") > 0 && records("feed_info") == 0 {
        missing("feed_info.txt", "a feed with translations has feed_info");
    }
    let elevators = m
        .records
        .get("pathway")
        .is_some_and(|ps| ps.iter().any(|p| int(&p.values, "pathway_mode") == Some(5)));
    if elevators && records("level") == 0 {
        missing("levels.txt", "a feed with elevator pathways has levels");
    }

    // ---- row by row: required fields and the rules of one row
    for (fspec, key, row) in &all {
        for fs in fspec.fields {
            if fs.presence == Presence::Required && !row.contains_key(fs.name) {
                out.push(
                    Finding::error(
                        "missing_field",
                        format!("{} {key}: {} is required", fspec.name, fs.name),
                    )
                    .at(fspec.name, None, Some(fs.name)),
                );
            }
        }
        for r in rules::row_findings(fspec.name, row) {
            let message = format!("{} {key}: {}", fspec.name, r.message);
            let f = if r.error {
                Finding::error(r.code, message)
            } else {
                Finding::warning(r.code, message)
            };
            out.push(f.at(fspec.name, None, Some(r.field)));
        }
    }

    // ---- references
    let mut values: HashMap<(&str, &str), HashSet<String>> = HashMap::new();
    for (fspec, _, row) in &all {
        for (f, v) in row {
            if let Some(t) = text(v) {
                values.entry((fspec.name, f)).or_default().insert(t);
            }
        }
    }
    for s in &m.shapes {
        values
            .entry(("shapes.txt", "shape_id"))
            .or_default()
            .insert(s.shape_id.clone());
    }
    for t in &m.trips {
        values
            .entry(("stop_times.txt", "trip_id"))
            .or_default()
            .insert(t.trip_id.clone());
    }
    let has = |file: &str, field: &str, v: &str| {
        values
            .get(&(file, field))
            .is_some_and(|set| set.contains(v))
    };
    let dangling = |file: &str, key: &str, fs: &spec::FieldSpec, v: &str| {
        if fs.refs.iter().any(|r| has(r.file, r.field, v)) {
            return None;
        }
        let targets: Vec<String> = fs
            .refs
            .iter()
            .map(|r| format!("{}.{}", r.file, r.field))
            .collect();
        Some(
            Finding::error(
                "reference_not_found",
                format!(
                    "{file} {key}: {} {v} is not a {}",
                    fs.name,
                    targets.join(" or ")
                ),
            )
            .at(file, None, Some(fs.name)),
        )
    };
    for (fspec, key, row) in &all {
        for fs in fspec.fields.iter().filter(|f| !f.refs.is_empty()) {
            let Some(v) = row.get(fs.name).and_then(text) else {
                continue;
            };
            if let Some(f) = dangling(fspec.name, key, fs, &v) {
                out.push(f);
            }
        }
    }
    // stop_times: what each stop order calls at, once
    let times = spec::file("stop_times.txt").expect("stop_times.txt");
    let mut seen: HashSet<(&str, String)> = HashSet::new();
    for p in &m.patterns {
        for s in &p.stops {
            let mut named = vec![("stop_id", s.stop_id.clone())];
            for f in ["pickup_booking_rule_id", "drop_off_booking_rule_id"] {
                if let Some(v) = s.values.get(f).and_then(text) {
                    named.push((f, v));
                }
            }
            for (field, v) in named {
                if !seen.insert((field, v.clone())) {
                    continue;
                }
                let fs = times.field(field).expect("a stop_times field");
                let key = format!("(route {} stop order {})", p.route_id, p.pattern_key);
                if let Some(f) = dangling("stop_times.txt", &key, fs, &v) {
                    out.push(f);
                }
            }
        }
    }

    // ---- stations and what stands in them
    for s in &m.stops {
        let lt = int(&s.values, "location_type").unwrap_or(0);
        let parent = s.values.get("parent_station").and_then(text);
        let id = s.stop_id.as_str();
        let at = |f: Finding| f.at("stops.txt", None, Some("parent_station"));
        // a station with a parent, and a place in one without, are the row
        // rules'
        match (lt, parent.as_deref()) {
            (1, _) => {}
            (_, Some(p)) if p == id => out.push(at(Finding::warning(
                "parent_is_itself",
                format!("stops.txt {id} is its own parent_station"),
            ))),
            (_, Some(p)) => {
                let want = if lt == 4 { 0 } else { 1 };
                if stop_type.get(p).is_some_and(|t| *t != want) {
                    out.push(at(Finding::error(
                        "parent_wrong_type",
                        format!(
                            "stops.txt {id}: its parent_station {p} is not a {}",
                            if want == 0 { "platform" } else { "station" }
                        ),
                    )));
                }
            }
            _ => {}
        }
    }

    // ---- pathways join places inside a station, never the station itself
    for p in m.records.get("pathway").into_iter().flatten() {
        for f in ["from_stop_id", "to_stop_id"] {
            let Some(s) = p.values.get(f).and_then(text) else {
                continue;
            };
            if stop_type.get(s.as_str()) == Some(&1) {
                out.push(
                    Finding::error(
                        "pathway_to_station",
                        format!(
                            "pathways.txt {}: {f} {s} is a station; a pathway joins its platforms, entrances and nodes",
                            p.key
                        ),
                    )
                    .at("pathways.txt", None, Some(f)),
                );
            }
        }
    }

    // ---- times and distances that never go backwards
    let used_profiles: HashMap<(&str, i16, i32), usize> =
        m.trips.iter().fold(HashMap::new(), |mut acc, t| {
            if let Some(p) = t.profile_key {
                *acc.entry((t.route_id.as_str(), t.pattern_key, p))
                    .or_default() += 1;
            }
            acc
        });
    for p in &m.profiles {
        let n = used_profiles
            .get(&(p.route_id.as_str(), p.pattern_key, p.profile_key))
            .copied()
            .unwrap_or(0);
        let backwards = (0..p.arrival.len()).find(|&i| {
            p.departure.get(i).is_some_and(|d| *d < p.arrival[i])
                || p.arrival
                    .get(i + 1)
                    .is_some_and(|next| *next < p.departure.get(i).copied().unwrap_or(p.arrival[i]))
        });
        if let Some(i) = backwards {
            out.push(
                Finding::error(
                    "time_goes_backwards",
                    format!(
                        "route {} stop order {} timing {} ({n} trips): the time goes backwards after stop {}",
                        p.route_id,
                        p.pattern_key,
                        p.profile_key,
                        i + 1
                    ),
                )
                .at("stop_times.txt", None, Some("arrival_time")),
            );
        }
    }
    for p in &m.patterns {
        let dist: Vec<f64> = p
            .stops
            .iter()
            .filter_map(|s| s.values.get("shape_dist_traveled").and_then(Value::as_f64))
            .collect();
        if dist.windows(2).any(|w| w[1] < w[0]) {
            out.push(
                Finding::error(
                    "decreasing_distance",
                    format!(
                        "route {} stop order {}: shape_dist_traveled decreases along it",
                        p.route_id, p.pattern_key
                    ),
                )
                .at("stop_times.txt", None, Some("shape_dist_traveled")),
            );
        }
    }
    for s in &m.shapes {
        let back = s.points.windows(2).any(|w| w[1].sequence <= w[0].sequence);
        let shrinks = s.points.windows(2).any(|w| match (w[0].dist, w[1].dist) {
            (Some(a), Some(b)) => b < a,
            _ => false,
        });
        if back || shrinks {
            out.push(
                Finding::error(
                    "decreasing_distance",
                    format!(
                        "shapes.txt {}: {} decreases along the shape",
                        s.shape_id,
                        if back {
                            "shape_pt_sequence"
                        } else {
                            "shape_dist_traveled"
                        }
                    ),
                )
                .at("shapes.txt", None, None),
            );
        }
    }

    // ---- calendars
    let mut last_day: Option<String> = None;
    for s in &m.services {
        let runs_weekly = s.days.is_some_and(|d| d.iter().any(|x| *x));
        let added: Vec<&String> = s
            .dates
            .iter()
            .filter(|(_, k)| *k == 1)
            .map(|(d, _)| d)
            .collect();
        if !runs_weekly && added.is_empty() {
            out.push(
                Finding::warning(
                    "service_never_runs",
                    format!(
                        "service {} runs on no day of the week and no added date",
                        s.service_id
                    ),
                )
                .at("calendar.txt", None, Some("service_id")),
            );
            continue;
        }
        let end = if runs_weekly {
            s.end_date.as_ref()
        } else {
            None
        };
        for d in added.into_iter().chain(end) {
            if last_day.as_deref().is_none_or(|l| d.as_str() > l) {
                last_day = Some(d.clone());
            }
        }
    }
    if let Some(last) = last_day.filter(|l| l.as_str() < today) {
        out.push(
            Finding::warning(
                "feed_expired",
                format!("no service runs on or after {today}: the last day any runs is {last}"),
            )
            .at("calendar.txt", None, Some("end_date")),
        );
    }
    for fi in m.records.get("feed_info").into_iter().flatten() {
        if let Some(end) = fi.values.get("feed_end_date").and_then(Value::as_str) {
            if end < today {
                out.push(
                    Finding::warning(
                        "feed_expired",
                        format!("feed_info.txt says the feed ends on {end}, before {today}"),
                    )
                    .at("feed_info.txt", None, Some("feed_end_date")),
                );
            }
        }
    }

    // ---- several agencies
    let agencies = m.records.get("agency").map(Vec::as_slice).unwrap_or(&[]);
    if agencies.len() > 1 {
        for r in m
            .routes
            .iter()
            .filter(|r| !r.values.contains_key("agency_id"))
        {
            out.push(
                Finding::error(
                    "missing_field",
                    format!(
                        "routes.txt {}: agency_id is required in a feed of several agencies",
                        r.route_id
                    ),
                )
                .at("routes.txt", None, Some("agency_id")),
            );
        }
        let zones: HashSet<&str> = agencies
            .iter()
            .filter_map(|a| a.values.get("agency_timezone").and_then(Value::as_str))
            .collect();
        if zones.len() > 1 {
            let mut zones: Vec<&str> = zones.into_iter().collect();
            zones.sort_unstable();
            out.push(
                Finding::error(
                    "agency_timezones_differ",
                    format!(
                        "the feed's agencies are in {}; they share one timezone",
                        zones.join(", ")
                    ),
                )
                .at("agency.txt", None, Some("agency_timezone")),
            );
        }
    }

    // ---- what nothing uses
    let used = |file: &str, field: &str| values.get(&(file, field)).cloned().unwrap_or_default();
    let mut unused = |code: &'static str, file: &str, what: &str, id: &str, why: &str| {
        out.push(Finding::warning(code, format!("{what} {id} {why}")).at(file, None, None));
    };
    if agencies.len() > 1 {
        let mut named = used("routes.txt", "agency_id");
        named.extend(used("fare_attributes.txt", "agency_id"));
        named.extend(used("attributions.txt", "agency_id"));
        for a in agencies.iter().filter(|a| !named.contains(&a.key)) {
            unused(
                "unused_agency",
                "agency.txt",
                "agency",
                &a.key,
                "runs no route",
            );
        }
    }
    let run = used("trips.txt", "route_id");
    for r in m.routes.iter().filter(|r| !run.contains(&r.route_id)) {
        unused(
            "route_without_trips",
            "routes.txt",
            "route",
            &r.route_id,
            "has no trips",
        );
    }
    let mut named = used("trips.txt", "service_id");
    named.extend(used("timeframes.txt", "service_id"));
    named.extend(used("booking_rules.txt", "prior_notice_service_id"));
    for s in m.services.iter().filter(|s| !named.contains(&s.service_id)) {
        unused(
            "unused_service",
            "calendar.txt",
            "service",
            &s.service_id,
            "runs no trip",
        );
    }
    let named = used("trips.txt", "shape_id");
    for s in m.shapes.iter().filter(|s| !named.contains(&s.shape_id)) {
        unused(
            "unused_shape",
            "shapes.txt",
            "shape",
            &s.shape_id,
            "is taken by no trip",
        );
    }
    let named = used("stops.txt", "level_id");
    for l in m.records.get("level").into_iter().flatten() {
        if !named.contains(&l.key) {
            unused(
                "unused_level",
                "levels.txt",
                "level",
                &l.key,
                "has no stop on it",
            );
        }
    }
    let called: HashSet<&str> = m
        .patterns
        .iter()
        .flat_map(|p| p.stops.iter().map(|s| s.stop_id.as_str()))
        .collect();
    for s in &m.stops {
        if int(&s.values, "location_type").unwrap_or(0) == 0 && !called.contains(s.stop_id.as_str())
        {
            unused(
                "unused_stop",
                "stops.txt",
                "stop",
                &s.stop_id,
                "is called at by no stop order",
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gtfs::model::{BuildOptions, FeedModel};
    use crate::gtfs::read::{RawFeed, RawTable};

    fn table(text: &str) -> RawTable {
        let mut lines = text.lines();
        let header: Vec<String> = lines
            .next()
            .unwrap()
            .split(',')
            .map(str::to_string)
            .collect();
        RawTable {
            header,
            rows: lines
                .map(|l| l.split(',').map(str::to_string).collect())
                .collect(),
        }
    }

    fn feed(files: &[(&str, &str)]) -> FeedModel {
        let mut raw = RawFeed::default();
        for (name, text) in files {
            raw.files.insert(name.to_string(), table(text));
        }
        FeedModel::from_raw(&raw, "t", BuildOptions::default()).0
    }

    fn codes(fs: &[Finding]) -> Vec<String> {
        let mut c: Vec<String> = fs
            .iter()
            .map(|f| {
                format!(
                    "{}:{}",
                    if f.level == Level::Error { "E" } else { "W" },
                    f.code
                )
            })
            .collect();
        c.sort();
        c.dedup();
        c
    }

    const OK: &[(&str, &str)] = &[
        ("agency.txt", "agency_id,agency_name,agency_url,agency_timezone\nA,Metro,https://m.example,Asia/Kolkata"),
        ("stops.txt", "stop_id,stop_name,stop_lat,stop_lon,location_type,parent_station\nST,Central,13.0,80.0,1,\nP1,Central 1,13.0,80.0,0,ST\nP2,Beach,13.1,80.1,0,"),
        ("routes.txt", "route_id,agency_id,route_short_name,route_type\nR,A,1,1"),
        ("trips.txt", "route_id,service_id,trip_id\nR,WK,T"),
        ("stop_times.txt", "trip_id,arrival_time,departure_time,stop_id,stop_sequence\nT,06:00:00,06:00:00,P1,1\nT,06:05:00,06:05:00,P2,2"),
        ("calendar.txt", "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\nWK,1,1,1,1,1,0,0,20260101,20261231"),
    ];

    #[test]
    fn a_sound_feed_has_nothing_to_report() {
        let m = feed(OK);
        assert_eq!(codes(&validate(&m, "2026-09-24")), Vec::<String>::new());
    }

    #[test]
    fn what_a_feed_breaks_is_reported_by_kind() {
        let mut files = OK.to_vec();
        files[0] = (
            "agency.txt",
            "agency_id,agency_name,agency_url,agency_timezone\nA,Metro,,Asia/Kolkata\nB,Bus,https://b.example,Europe/Amsterdam",
        );
        files[1] = (
            "stops.txt",
            "stop_id,stop_name,stop_lat,stop_lon,location_type,parent_station\nST,Central,13.0,80.0,1,\nP1,Central 1,13.0,80.0,0,ST\nP2,Beach,13.1,80.1,0,P2\nE1,Gate,13.0,80.0,2,\nX,Unused,13.2,80.2,0,",
        );
        files[2] = (
            "routes.txt",
            "route_id,route_short_name,route_type\nR,1,1\nR2,2,3",
        );
        files.push((
            "pathways.txt",
            "pathway_id,from_stop_id,to_stop_id,pathway_mode,is_bidirectional\nPW,E1,ST,1,1\nPW2,E1,NOPE,1,1",
        ));
        files.push((
            "stop_times.txt",
            "trip_id,arrival_time,departure_time,stop_id,stop_sequence\nT,06:00:00,06:00:00,P1,1\nT,05:55:00,05:55:00,P2,2",
        ));
        files.retain(|(n, t)| *n != "stop_times.txt" || t.contains("05:55"));
        let m = feed(&files);
        let found = validate(&m, "2027-01-01");
        assert_eq!(
            codes(&found),
            vec![
                "E:agency_timezones_differ",
                "E:missing_field",
                "E:pathway_to_station",
                "E:reference_not_found",
                "E:time_goes_backwards",
                "W:feed_expired",
                "W:parent_is_itself",
                "W:route_without_trips",
                "W:unused_agency",
                "W:unused_stop",
            ],
            "{found:#?}"
        );
        let report = summarise(&found, 2);
        assert_eq!(report.codes[0].level, Level::Error);
        assert_eq!(report.errors + report.warnings, found.len(), "{report:#?}");
    }
}
