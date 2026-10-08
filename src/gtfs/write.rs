//! A [`FeedModel`] back out as GTFS: the files as text, then a zip.
//!
//! The output is byte-stable - the same model always gives the same bytes - so
//! a published zip only changes when the feed does:
//! - files come in the reference's order, each row in the order the feed
//!   first had it (its `sort_key`), new rows after by id;
//! - a file has the columns some row fills, in the reference's order, plus the
//!   required ones;
//! - every zip entry carries the same timestamp.

use super::model::{FeedModel, Row, PATTERN_STOP_FIELDS, TRIP_FIELDS};
use super::read::{RawFeed, RawTable};
use super::spec::{self, FileSpec, Presence};
use crate::services::gtfs_timing::{self, Offsets};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Cursor, Write};

/// Rows of values for one file, and the columns they need.
struct Out {
    spec: &'static FileSpec,
    rows: Vec<BTreeMap<&'static str, String>>,
}

impl Out {
    fn new(name: &str) -> Out {
        Out {
            spec: spec::file(name).expect("a file of the reference"),
            rows: Vec::new(),
        }
    }

    fn push_row(&mut self, values: &Row, extra: &[(&'static str, String)]) {
        let mut row = BTreeMap::new();
        for fs in self.spec.fields {
            if let Some(v) = values.get(fs.name) {
                let text = spec::to_text(fs, &Some(v.clone()));
                if !text.is_empty() {
                    row.insert(fs.name, text);
                }
            }
        }
        for (k, v) in extra {
            if !v.is_empty() {
                row.insert(*k, v.clone());
            }
        }
        self.rows.push(row);
    }

    fn table(self) -> Option<(String, RawTable)> {
        if self.rows.is_empty() {
            return None;
        }
        let header: Vec<&'static str> = self
            .spec
            .fields
            .iter()
            .filter(|fs| {
                fs.presence == Presence::Required
                    || self.rows.iter().any(|r| r.contains_key(fs.name))
            })
            .map(|fs| fs.name)
            .collect();
        let rows = self
            .rows
            .iter()
            .map(|r| {
                header
                    .iter()
                    .map(|h| r.get(h).cloned().unwrap_or_default())
                    .collect()
            })
            .collect();
        Some((
            self.spec.name.to_string(),
            RawTable {
                header: header.iter().map(|h| h.to_string()).collect(),
                rows,
            },
        ))
    }
}

fn by_sort_key<T>(items: &mut [T], key: impl Fn(&T) -> (Option<i32>, String)) {
    items.sort_by(|a, b| {
        let (ka, ia) = key(a);
        let (kb, ib) = key(b);
        // rows the feed had come first, in its order; new rows after, by id
        (ka.is_none(), ka, ia).cmp(&(kb.is_none(), kb, ib))
    });
}

/// The feed's files, as text.
pub fn to_raw(m: &FeedModel) -> RawFeed {
    let mut raw = RawFeed::default();
    let mut put = |out: Out| {
        if let Some((name, table)) = out.table() {
            raw.files.insert(name, table);
        }
    };

    // record files, in the reference's order (shapes and locations have their
    // own shapes)
    for fspec in spec::FILES {
        let Some(entity) = fspec.entity() else {
            continue;
        };
        if matches!(entity, "shape" | "location") {
            continue;
        }
        let Some(rows) = m.records.get(entity) else {
            continue;
        };
        let mut rows = rows.clone();
        by_sort_key(&mut rows, |r| (r.sort_key, r.key.clone()));
        let mut out = Out::new(fspec.name);
        for r in &rows {
            out.push_row(&r.values, &[]);
        }
        put(out);
    }

    let mut stops = m.stops.clone();
    by_sort_key(&mut stops, |s| (s.sort_key, s.stop_id.clone()));
    let mut out = Out::new("stops.txt");
    for s in &stops {
        out.push_row(&s.values, &[("stop_id", s.stop_id.clone())]);
    }
    put(out);

    let mut routes = m.routes.clone();
    by_sort_key(&mut routes, |r| (r.sort_key, r.route_id.clone()));
    let mut out = Out::new("routes.txt");
    for r in &routes {
        out.push_row(&r.values, &[("route_id", r.route_id.clone())]);
    }
    put(out);

    let mut trips = m.trips.clone();
    trips.sort_by(|a, b| (a.sort_key, &a.trip_id).cmp(&(b.sort_key, &b.trip_id)));
    let mut trips_out = Out::new("trips.txt");
    let mut times_out = Out::new("stop_times.txt");
    let mut freq_out = Out::new("frequencies.txt");
    let mut default_at: BTreeMap<usize, Offsets> = BTreeMap::new();
    let fspec = spec::file("stop_times.txt").expect("stop_times.txt");
    for t in &trips {
        let own: Row = t
            .values
            .iter()
            .filter(|(k, _)| TRIP_FIELDS.contains(k))
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let Some(pattern) = m.pattern(&t.route_id, t.pattern_key) else {
            trips_out.push_row(
                &own,
                &[
                    ("route_id", t.route_id.clone()),
                    ("service_id", t.service_id.clone()),
                    ("trip_id", t.trip_id.clone()),
                ],
            );
            continue;
        };
        // A trip left with fewer than two calls is not a trip a feed may carry,
        // so while a stop is out of use (section 21) a trip that called at two
        // stops and now calls at one is not published at all - neither its
        // trips.txt row nor its frequencies. It comes back with the stop. Only
        // a trip the out-of-use stops shortened: one the feed itself has with a
        // single call (chennai_bus has 158) is written as it came, or a reload
        // of the feed's own zip would not round-trip.
        let calls = pattern
            .stops
            .iter()
            .filter(|s| !m.unserviceable.contains(&s.stop_id))
            .count();
        if calls < 2 && calls < pattern.stops.len() {
            continue;
        }
        trips_out.push_row(
            &own,
            &[
                ("route_id", t.route_id.clone()),
                ("service_id", t.service_id.clone()),
                ("trip_id", t.trip_id.clone()),
            ],
        );
        let n = pattern.stops.len();
        let offsets = match t.profile_key {
            Some(k) => match m.profile(&t.route_id, t.pattern_key, k) {
                Some(p) => Offsets {
                    arrival: p.arrival.clone(),
                    departure: p.departure.clone(),
                },
                None => continue,
            },
            None => default_at
                .entry(n)
                .or_insert_with(|| {
                    gtfs_timing::default_offsets(n, m.default_timing.0, m.default_timing.1)
                })
                .clone(),
        };
        // the headsign of a skipped row, for the next row that has none of its
        // own: a fare stage whose first stop is out of use is still that stage
        let mut carried: Option<Value> = None;
        for (i, s) in pattern.stops.iter().enumerate() {
            // A stop out of use keeps its stops.txt row and loses its calls
            // (section 21). The offsets are positional over the pattern, so the
            // row is skipped here rather than taken out of the pattern: every
            // other stop of the trip keeps the time it had, and stop_sequence
            // need only increase, so the gap this leaves is legal GTFS.
            if m.unserviceable.contains(&s.stop_id) {
                if let Some(h) = s.values.get("stop_headsign") {
                    carried = Some(h.clone());
                }
                continue;
            }
            let mut own: Row = s
                .values
                .iter()
                .filter(|(k, _)| PATTERN_STOP_FIELDS.contains(k))
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            // the stage of a skipped first stop passes to this one
            if let Some(h) = carried.take() {
                own.entry("stop_headsign").or_insert(h);
            }
            let seq = match s.values.get("stop_sequence") {
                Some(v) => spec::to_text(
                    fspec.field("stop_sequence").expect("a field"),
                    &Some(v.clone()),
                ),
                None => (i + 1).to_string(),
            };
            times_out.push_row(
                &own,
                &[
                    ("trip_id", t.trip_id.clone()),
                    (
                        "arrival_time",
                        spec::format_time((t.ref_s + offsets.arrival[i]) as i64),
                    ),
                    (
                        "departure_time",
                        spec::format_time((t.ref_s + offsets.departure[i]) as i64),
                    ),
                    ("stop_id", s.stop_id.clone()),
                    ("stop_sequence", seq),
                ],
            );
        }
        let mut windows = t.frequencies.clone();
        windows.sort_by_key(|f| f.start_s);
        for f in &windows {
            let mut row = Row::new();
            if let Some(e) = f.exact_times {
                row.insert("exact_times", json!(e));
            }
            freq_out.push_row(
                &row,
                &[
                    ("trip_id", t.trip_id.clone()),
                    ("start_time", spec::format_time(f.start_s as i64)),
                    ("end_time", spec::format_time(f.end_s as i64)),
                    ("headway_secs", f.headway_s.to_string()),
                ],
            );
        }
    }
    put(trips_out);
    put(times_out);
    put(freq_out);

    let mut cal = Out::new("calendar.txt");
    let mut dates = Out::new("calendar_dates.txt");
    for s in &m.services {
        if let Some(days) = s.days {
            let mut row = Row::new();
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
                row.insert(name, json!(days[d] as i64));
            }
            if let (Some(a), Some(b)) = (&s.start_date, &s.end_date) {
                row.insert("start_date", json!(a));
                row.insert("end_date", json!(b));
            }
            cal.push_row(&row, &[("service_id", s.service_id.clone())]);
        }
        for (date, kind) in &s.dates {
            let mut row = Row::new();
            row.insert("date", json!(date));
            row.insert("exception_type", json!(kind));
            dates.push_row(&row, &[("service_id", s.service_id.clone())]);
        }
    }
    put(cal);
    put(dates);

    let mut shapes = Out::new("shapes.txt");
    for s in &m.shapes {
        for p in &s.points {
            let mut row = Row::new();
            row.insert("shape_pt_lat", json!(p.lat));
            row.insert("shape_pt_lon", json!(p.lon));
            row.insert("shape_pt_sequence", json!(p.sequence));
            if let Some(d) = p.dist {
                row.insert("shape_dist_traveled", json!(d));
            }
            shapes.push_row(&row, &[("shape_id", s.shape_id.clone())]);
        }
    }
    put(shapes);

    if let Some(locs) = m.records.get("location") {
        let mut locs = locs.clone();
        by_sort_key(&mut locs, |r| (r.sort_key, r.key.clone()));
        let features: Vec<Value> = locs
            .iter()
            .map(|r| {
                let mut props = serde_json::Map::new();
                for f in ["stop_name", "stop_desc"] {
                    if let Some(v) = r.values.get(f) {
                        props.insert(f.into(), v.clone());
                    }
                }
                json!({
                    "type": "Feature",
                    "id": r.key,
                    "properties": props,
                    "geometry": r.values.get("geometry").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        raw.locations = Some(json!({"type": "FeatureCollection", "features": features}));
    }
    raw
}

/// One file as CSV text.
pub fn csv_text(t: &RawTable) -> String {
    let mut w = csv::WriterBuilder::new()
        .terminator(csv::Terminator::Any(b'\n'))
        .from_writer(Vec::new());
    let _ = w.write_record(&t.header);
    for r in &t.rows {
        let _ = w.write_record(r);
    }
    String::from_utf8(w.into_inner().unwrap_or_default()).unwrap_or_default()
}

/// The feed as a zip: its files in the reference's order.
pub fn zip_bytes(raw: &RawFeed) -> Result<Vec<u8>, String> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default());
    for fspec in spec::FILES {
        if fspec.name == "locations.geojson" {
            if let Some(geo) = &raw.locations {
                w.start_file(fspec.name, opts).map_err(|e| e.to_string())?;
                w.write_all(
                    serde_json::to_string_pretty(geo)
                        .unwrap_or_default()
                        .as_bytes(),
                )
                .map_err(|e| e.to_string())?;
            }
            continue;
        }
        let Some(t) = raw.files.get(fspec.name) else {
            continue;
        };
        w.start_file(fspec.name, opts).map_err(|e| e.to_string())?;
        w.write_all(csv_text(t).as_bytes())
            .map_err(|e| e.to_string())?;
    }
    Ok(w.finish().map_err(|e| e.to_string())?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::to_raw;
    use crate::gtfs::model::{BuildOptions, FeedModel};
    use crate::gtfs::read::{parse_csv, RawFeed};

    fn feed() -> FeedModel {
        let mut raw = RawFeed::default();
        for (name, text) in [
            ("agency.txt", "agency_id,agency_name,agency_url,agency_timezone\nA,Bus,https://b.example,Asia/Kolkata"),
            ("stops.txt", "stop_id,stop_name,stop_lat,stop_lon\nP1,One,13.0,80.0\nP2,Two,13.1,80.1"),
            ("routes.txt", "route_id,agency_id,route_short_name,route_type\nR1,A,1,3\nR2,A,2,3"),
            ("trips.txt", "route_id,service_id,trip_id\nR1,WK,ONE\nR2,WK,TWO"),
            ("stop_times.txt", "trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
                ONE,06:00:00,06:00:00,P1,1\n\
                TWO,07:00:00,07:00:00,P1,1\nTWO,07:05:00,07:05:00,P2,2"),
            ("calendar.txt", "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\nWK,1,1,1,1,1,0,0,20260101,20261231"),
        ] {
            raw.files.insert(name.to_string(), parse_csv(text, name, &mut Vec::new()));
        }
        FeedModel::from_raw(&raw, "t", BuildOptions::default()).0
    }

    fn trips(m: &FeedModel) -> Vec<String> {
        // no trip left: no trips.txt at all
        let out = to_raw(m);
        out.table("trips.txt")
            .map(|t| {
                t.rows
                    .iter()
                    .map(|r| t.cell(r, "trip_id").to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn a_trip_the_feed_has_with_one_call_is_written_as_it_came() {
        assert_eq!(trips(&feed()), vec!["ONE", "TWO"]);
    }

    #[test]
    fn a_trip_an_out_of_use_stop_leaves_with_one_call_is_not_written() {
        let mut m = feed();
        m.unserviceable.insert("P2".into());
        // TWO is down to P1 alone; ONE always called at P1 alone
        assert_eq!(trips(&m), vec!["ONE"]);
        m.unserviceable.insert("P1".into());
        assert!(
            trips(&m).is_empty(),
            "ONE has no call left: {:?}",
            trips(&m)
        );
    }
}
