//! Two feeds' files compared row by row, keyed the way each file is keyed,
//! with every cell read through the [`spec`](super::spec) first - so `5:30:00`
//! is `05:30:00`, `ff0000` is `FF0000` and `13.0800` is `13.08`, and a column
//! one side leaves out is the same as a column of empty cells.
//!
//! This is the round trip's check: a zip imported into the tables and
//! exported again must compare equal to itself, except where the import said
//! it dropped a cell.

use super::model::natural_key;
use super::read::{RawFeed, RawTable};
use super::spec::{self, FileSpec, Key};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diff {
    pub file: String,
    pub key: String,
    /// None: the whole row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// What the first feed has (`None`: no row, or an empty cell).
    pub a: Option<String>,
    pub b: Option<String>,
}

/// How each file is keyed for a compare.
fn key_fields(spec: &FileSpec) -> Vec<&'static str> {
    match spec.name {
        "stops.txt" => vec!["stop_id"],
        "routes.txt" => vec!["route_id"],
        "trips.txt" => vec!["trip_id"],
        "stop_times.txt" => vec!["trip_id", "stop_sequence"],
        "calendar.txt" => vec!["service_id"],
        "calendar_dates.txt" => vec!["service_id", "date"],
        "frequencies.txt" => vec!["trip_id", "start_time"],
        "shapes.txt" => vec!["shape_id", "shape_pt_sequence"],
        _ => match spec.key() {
            Some(Key::Field(f)) => vec![f],
            Some(Key::Minted { natural }) => natural.to_vec(),
            Some(Key::Feed) | None => vec![],
        },
    }
}

/// A cell in canonical text: what the spec reads it as, written back out; the
/// trimmed text when it does not read.
fn canon(spec: &FileSpec, field: &str, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        // a stop's type is never blank in the tables (NOT NULL, 0): the
        // reference's own "empty means 0", and what the preprocessor reads a
        // missing column as
        if spec.name == "stops.txt" && field == "location_type" {
            return Some("0".into());
        }
        return None;
    }
    let Some(fs) = spec.field(field) else {
        return Some(raw.to_string());
    };
    match spec::from_text(fs, raw) {
        Ok(Some(Value::Number(n))) => Some(
            n.as_f64()
                .map(|x| {
                    if x.fract() == 0.0 {
                        format!("{}", x as i64)
                    } else {
                        format!("{x}")
                    }
                })
                .unwrap_or_else(|| n.to_string()),
        ),
        Ok(Some(v @ Value::Object(_))) | Ok(Some(v @ Value::Array(_))) => Some(v.to_string()),
        Ok(v) => Some(spec::to_text(fs, &v)).filter(|s| !s.is_empty()),
        Err(_) => Some(raw.to_string()),
    }
}

fn rows_by_key(spec: &FileSpec, t: &RawTable) -> BTreeMap<String, Vec<(String, Option<String>)>> {
    let keys = key_fields(spec);
    let fields: Vec<&str> = t
        .header
        .iter()
        .map(String::as_str)
        .filter(|h| spec.field(h).is_some())
        .collect();
    let mut out = BTreeMap::new();
    // two rows under one key (a trip giving two stops one stop_sequence) are
    // told apart by the order the file has them in
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for row in &t.rows {
        let mut cells: Vec<(String, Option<String>)> = fields
            .iter()
            .map(|f| (f.to_string(), canon(spec, f, t.cell(row, f))))
            .collect();
        if spec.name == "stops.txt" && !fields.contains(&"location_type") {
            cells.push(("location_type".into(), canon(spec, "location_type", "")));
        }
        let key = if keys.is_empty() {
            String::new()
        } else if spec.key().is_some() && spec.name != "shapes.txt" {
            let values: super::model::Row = cells
                .iter()
                .filter_map(|(f, v)| {
                    let fs = spec.field(f)?;
                    Some((
                        fs.name,
                        spec::from_text(fs, v.as_deref().unwrap_or("")).ok()??,
                    ))
                })
                .collect();
            natural_key(spec, &values)
        } else {
            keys.iter()
                .map(|k| canon(spec, k, t.cell(row, k)).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\u{1f}")
        };
        let n = seen.entry(key.clone()).or_default();
        let key = if *n == 0 { key } else { format!("{key}#{n}") };
        *n += 1;
        out.insert(key, cells);
    }
    out
}

/// Every difference between two feeds, file by file. `ignore` holds cells not
/// to compare, as `(file, key, field)`.
pub fn compare(a: &RawFeed, b: &RawFeed, ignore: &BTreeSet<(String, String, String)>) -> Vec<Diff> {
    let mut diffs = Vec::new();
    let names: BTreeSet<&String> = a.files.keys().chain(b.files.keys()).collect();
    let empty = RawTable::default();
    for name in names {
        let Some(fspec) = spec::file(name) else {
            continue;
        };
        let ta = a.files.get(name).unwrap_or(&empty);
        let tb = b.files.get(name).unwrap_or(&empty);
        let ra = rows_by_key(fspec, ta);
        let rb = rows_by_key(fspec, tb);
        for (key, cells) in &ra {
            let Some(other) = rb.get(key) else {
                if ignore.contains(&(name.clone(), key.clone(), "*".into())) {
                    continue;
                }
                diffs.push(Diff {
                    file: name.clone(),
                    key: key.clone(),
                    field: None,
                    a: Some("row".into()),
                    b: None,
                });
                continue;
            };
            let other: BTreeMap<&str, &Option<String>> =
                other.iter().map(|(f, v)| (f.as_str(), v)).collect();
            let mine: BTreeMap<&str, &Option<String>> =
                cells.iter().map(|(f, v)| (f.as_str(), v)).collect();
            let fields: BTreeSet<&str> = mine.keys().chain(other.keys()).copied().collect();
            for f in fields {
                let va = mine.get(f).copied().cloned().flatten();
                let vb = other.get(f).copied().cloned().flatten();
                if va != vb && !ignore.contains(&(name.clone(), key.clone(), f.to_string())) {
                    diffs.push(Diff {
                        file: name.clone(),
                        key: key.clone(),
                        field: Some(f.to_string()),
                        a: va,
                        b: vb,
                    });
                }
            }
        }
        for key in rb.keys().filter(|k| !ra.contains_key(*k)) {
            diffs.push(Diff {
                file: name.clone(),
                key: key.clone(),
                field: None,
                a: None,
                b: Some("row".into()),
            });
        }
    }
    let (ga, gb) = (a.locations.as_ref(), b.locations.as_ref());
    if canonical_geojson(ga) != canonical_geojson(gb) {
        diffs.push(Diff {
            file: "locations.geojson".into(),
            key: String::new(),
            field: None,
            a: ga.map(|_| "features".into()),
            b: gb.map(|_| "features".into()),
        });
    }
    diffs
}

fn canonical_geojson(v: Option<&Value>) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for f in v
        .and_then(|v| v["features"].as_array())
        .into_iter()
        .flatten()
    {
        let id = match &f["id"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        out.insert(
            id,
            serde_json::json!({"properties": f["properties"], "geometry": f["geometry"]}),
        );
    }
    out
}

/// Differences counted per file and field, for a report that would otherwise
/// list a million rows.
pub fn summarise(diffs: &[Diff]) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for d in diffs {
        let k = match &d.field {
            Some(f) => format!("{}.{}", d.file, f),
            None if d.a.is_some() => format!("{} rows only in the first", d.file),
            None => format!("{} rows only in the second", d.file),
        };
        *out.entry(k).or_default() += 1;
    }
    out
}
