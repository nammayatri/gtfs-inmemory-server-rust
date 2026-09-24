//! A GTFS zip, read into its files as text - nothing interpreted yet.
//!
//! Shipped zips are not tidy. Some keep their files in a folder inside the zip
//! (`sambalpur.bus.gtfs/stops.txt`), some carry macOS litter (`._stops.txt`,
//! `.DS_Store`, `__MACOSX/`), some start with a byte-order mark or end without
//! a newline, and one names a file the reference does not have (`feed.txt`).
//! The folder is found the way nandi's preprocessor finds it - the one holding
//! `stops.txt` - the litter is skipped, and anything else not in the reference
//! is left out with a warning.

use super::spec;
use super::Finding;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Cursor, Read};

/// One file's header and rows, every cell as written (minus surrounding
/// whitespace in the header).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawTable {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl RawTable {
    pub fn col(&self, name: &str) -> Option<usize> {
        self.header.iter().position(|h| h == name)
    }

    /// A row's cell under `name`, or `""` when the file has no such column.
    pub fn cell<'a>(&self, row: &'a [String], name: &str) -> &'a str {
        self.col(name)
            .and_then(|i| row.get(i))
            .map(String::as_str)
            .unwrap_or("")
    }
}

/// A feed's files, by name (`stops.txt`), and the parsed `locations.geojson`.
#[derive(Debug, Clone, Default)]
pub struct RawFeed {
    pub files: BTreeMap<String, RawTable>,
    pub locations: Option<Value>,
}

impl RawFeed {
    pub fn table(&self, name: &str) -> Option<&RawTable> {
        self.files.get(name)
    }
}

fn is_litter(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    name.starts_with("__MACOSX/")
        || base.starts_with("._")
        || base == ".DS_Store"
        || base.is_empty()
}

/// The folder inside the zip that holds the feed: where `stops.txt` is, else
/// `agency.txt`, else the zip's root.
fn feed_prefix(names: &[String]) -> String {
    for anchor in ["stops.txt", "agency.txt", "routes.txt"] {
        let mut found: Vec<&String> = names
            .iter()
            .filter(|n| {
                !is_litter(n) && (n.as_str() == anchor || n.ends_with(&format!("/{anchor}")))
            })
            .collect();
        found.sort_by_key(|n| n.len());
        if let Some(n) = found.first() {
            return n[..n.len() - anchor.len()].to_string();
        }
    }
    String::new()
}

/// Text from a file's bytes: UTF-8 without its byte-order mark.
fn text_of(bytes: &[u8], name: &str, findings: &mut Vec<Finding>) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match String::from_utf8(bytes.to_vec()) {
        Ok(s) => s,
        Err(_) => {
            findings.push(
                Finding::warning(
                    "not_utf8",
                    "the file is not UTF-8; unreadable bytes were replaced",
                )
                .at(name, None, None),
            );
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

/// Parse one CSV file. Short rows are padded, long rows cut, each with one
/// warning for the file saying how many.
pub fn parse_csv(text: &str, name: &str, findings: &mut Vec<Finding>) -> RawTable {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(text.as_bytes());
    let header: Vec<String> = match rdr.headers() {
        Ok(h) => h.iter().map(|c| c.trim().to_string()).collect(),
        Err(e) => {
            findings.push(Finding::error("unreadable_file", e.to_string()).at(name, None, None));
            return RawTable::default();
        }
    };
    let (mut short, mut long) = (0usize, 0usize);
    let mut rows = Vec::new();
    for (i, rec) in rdr.records().enumerate() {
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                findings.push(Finding::error("unreadable_row", e.to_string()).at(
                    name,
                    Some(i + 2),
                    None,
                ));
                continue;
            }
        };
        // a line holding nothing at all is not a row
        if rec.len() == 1 && rec.get(0).is_some_and(|c| c.trim().is_empty()) && header.len() > 1 {
            continue;
        }
        let mut row: Vec<String> = rec.iter().map(str::to_string).collect();
        if row.len() < header.len() {
            short += 1;
            row.resize(header.len(), String::new());
        } else if row.len() > header.len() {
            long += 1;
            row.truncate(header.len());
        }
        rows.push(row);
    }
    if short > 0 {
        findings.push(
            Finding::warning(
                "short_rows",
                format!("{short} rows have fewer cells than the header; the rest read as empty"),
            )
            .at(name, None, None),
        );
    }
    if long > 0 {
        findings.push(
            Finding::warning(
                "long_rows",
                format!(
                    "{long} rows have more cells than the header; the extra cells were left out"
                ),
            )
            .at(name, None, None),
        );
    }
    RawTable { header, rows }
}

/// Every file of a GTFS zip, as text.
pub fn read_zip(bytes: &[u8]) -> Result<(RawFeed, Vec<Finding>), String> {
    let mut zip =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not a zip: {e}"))?;
    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();
    let prefix = feed_prefix(&names);
    let mut findings = Vec::new();
    let mut feed = RawFeed::default();
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("unreadable zip entry: {e}"))?;
        let name = entry.name().to_string();
        if entry.is_dir() || is_litter(&name) {
            continue;
        }
        let Some(file) = name.strip_prefix(&prefix).filter(|f| !f.contains('/')) else {
            findings.push(
                Finding::warning(
                    "ignored_entry",
                    format!("{name} is outside the feed's folder {prefix:?} and was left out"),
                )
                .at(&name, None, None),
            );
            continue;
        };
        if spec::file(file).is_none() || !(file.ends_with(".txt") || file.ends_with(".geojson")) {
            findings.push(
                Finding::warning(
                    "unknown_file",
                    format!("{file} is not a file of the GTFS reference and was left out"),
                )
                .at(file, None, None),
            );
            continue;
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| format!("unreadable {name}: {e}"))?;
        let text = text_of(&bytes, file, &mut findings);
        if file.ends_with(".geojson") {
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => feed.locations = Some(v),
                Err(e) => findings.push(
                    Finding::error("unreadable_file", format!("not JSON: {e}"))
                        .at(file, None, None),
                ),
            }
            continue;
        }
        let table = parse_csv(&text, file, &mut findings);
        feed.files.insert(file.to_string(), table);
    }
    Ok((feed, findings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn a_feed_in_a_folder_with_litter_reads_as_if_it_were_tidy() {
        let bytes = zip_of(&[
            ("x.gtfs/", b""),
            ("x.gtfs/._stops.txt", b"junk"),
            ("__MACOSX/x.gtfs/._stops.txt", b"junk"),
            ("x.gtfs/.DS_Store", b"junk"),
            (
                "x.gtfs/stops.txt",
                b"\xEF\xBB\xBFstop_id , stop_name,stop_lat,stop_lon\r\nA,Alpha,13.0,80.0\r\nB,Beta,13.1,80.1",
            ),
            ("x.gtfs/feed.txt", b"a\n1\n"),
        ]);
        let (feed, findings) = read_zip(&bytes).unwrap();
        let stops = feed.table("stops.txt").unwrap();
        assert_eq!(
            stops.header,
            vec!["stop_id", "stop_name", "stop_lat", "stop_lon"]
        );
        assert_eq!(stops.rows.len(), 2);
        assert_eq!(stops.cell(&stops.rows[1], "stop_name"), "Beta");
        assert_eq!(stops.cell(&stops.rows[1], "stop_code"), "");
        let codes: Vec<&str> = findings.iter().map(|f| f.code).collect();
        assert_eq!(codes, vec!["unknown_file"], "{findings:?}");
    }

    #[test]
    fn ragged_rows_are_padded_or_cut_and_said_once() {
        let mut findings = vec![];
        let t = parse_csv("a,b,c\n1,2\n1,2,3,4\n\n5,6,7\n", "x.txt", &mut findings);
        assert_eq!(
            t.rows,
            vec![vec!["1", "2", ""], vec!["1", "2", "3"], vec!["5", "6", "7"]]
        );
        let codes: Vec<&str> = findings.iter().map(|f| f.code).collect();
        assert_eq!(codes, vec!["short_rows", "long_rows"]);
    }
}
