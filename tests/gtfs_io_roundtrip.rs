//! A GTFS zip read into the model the editor's tables hold, written back out and
//! compared (docs/gtfs-editor.md section 18). No database.
//!
//! - `a_feed_using_every_file_round_trips`: a small feed with a row in every
//!   file of the reference, and the awkward cases the shipped feeds have.
//! - `every_shipped_zip_round_trips` (ignored: run it with `--ignored`): every
//!   `*.gtfs.zip` in `$NANDI_ASSETS` (nandi's `assets/`).

use gtfs_routes_service::gtfs::{compare, model, read, write, Level};

#[path = "support/gtfs_fixture.rs"]
mod gtfs_fixture;
use gtfs_fixture::{fixture, zip_of};

#[test]
fn a_feed_using_every_file_round_trips() {
    let (raw, mut findings) = read::read_zip(&fixture("test_feed")).unwrap();
    let (m, more) = model::FeedModel::from_raw(&raw, "test_feed", model::BuildOptions::default());
    findings.extend(more);
    assert!(
        findings.iter().all(|f| f.level != Level::Error),
        "{findings:#?}"
    );
    let codes: Vec<&str> = findings.iter().map(|f| f.code).collect();
    assert_eq!(codes, vec!["self_parent"], "{findings:#?}");

    // every file of the reference is there
    let counts = m.counts();
    for (file, n) in [
        ("agency.txt", 2),
        ("stops.txt", 8),
        ("routes.txt", 3),
        ("trips.txt", 6),
        ("stop_times.txt", 15),
        ("services", 2),
        ("shapes.txt", 1),
        ("frequencies.txt", 2),
        ("transfers.txt", 3),
        ("pathways.txt", 3),
        ("levels.txt", 2),
        ("fare_attributes.txt", 1),
        ("fare_rules.txt", 2),
        ("timeframes.txt", 2),
        ("rider_categories.txt", 2),
        ("fare_media.txt", 1),
        ("fare_products.txt", 2),
        ("fare_leg_rules.txt", 1),
        ("fare_leg_join_rules.txt", 1),
        ("fare_transfer_rules.txt", 1),
        ("areas.txt", 1),
        ("stop_areas.txt", 2),
        ("networks.txt", 1),
        ("route_networks.txt", 1),
        ("location_groups.txt", 1),
        ("location_group_stops.txt", 2),
        ("locations.geojson", 1),
        ("booking_rules.txt", 1),
        ("translations.txt", 3),
        ("feed_info.txt", 1),
        ("attributions.txt", 2),
    ] {
        assert_eq!(counts.get(file).copied(), Some(n), "{file}: {counts:?}");
    }

    // the timetable, as the tables keep it
    let r1: Vec<&model::Pattern> = m.patterns.iter().filter(|p| p.route_id == "R1").collect();
    assert_eq!(r1.len(), 3, "{r1:#?}");
    let one = m.pattern("R1", 1).unwrap();
    assert_eq!(
        one.stop_ids(),
        vec!["P1", "B1", "B2"],
        "the longest, first on a tie"
    );
    assert_eq!(
        m.pattern("R1", 2).unwrap().stop_ids(),
        vec!["P1", "B1"],
        "then by first trip: the short turn T3 runs before T4"
    );
    let split = m.pattern("R1", 3).unwrap();
    assert_eq!(
        split.stop_ids(),
        one.stop_ids(),
        "T4 differs only in a pickup"
    );
    assert_eq!(split.stops[0].values["pickup_type"], 1);
    let t = |id: &str| m.trips.iter().find(|t| t.trip_id == id).unwrap();
    assert_eq!((t("T1").pattern_key, t("T1").profile_key), (1, Some(1)));
    assert_eq!(
        (t("T2").pattern_key, t("T2").profile_key, t("T2").ref_s),
        (1, Some(1), 6 * 3600),
        "the same offsets from another start share a profile"
    );
    assert_eq!(t("T2").frequencies.len(), 2);
    let t5 = m.pattern("R2", 1).unwrap();
    assert_eq!(t5.stops[1].values["stop_sequence"], 20, "numbering kept");
    assert_eq!(t("T5").ref_s, 25 * 3600 + 50 * 60, "past midnight");
    assert!(
        !m.pattern("R1", 1).unwrap().stops[0]
            .values
            .contains_key("stop_sequence"),
        "1 to n is not stored"
    );
    let hol = m.services.iter().find(|s| s.service_id == "HOL").unwrap();
    assert_eq!(hol.days, None, "a calendar_dates-only service");
    assert_eq!(hol.dates.len(), 2);
    assert_eq!(m.records["feed_info"][0].key, "test_feed");
    assert!(m.records["transfer"][0].key.starts_with("imp_"));

    // out, in, compare
    let out = write::to_raw(&m);
    let bytes = write::zip_bytes(&out).unwrap();
    assert_eq!(
        bytes,
        write::zip_bytes(&write::to_raw(&m)).unwrap(),
        "byte-stable"
    );
    let (back, _) = read::read_zip(&bytes).unwrap();
    let diffs = compare::compare(&raw, &back, &m.dropped);
    assert!(diffs.is_empty(), "{diffs:#?}");
    // and the same model again from what it wrote
    let (again, _) = model::FeedModel::from_raw(&back, "test_feed", model::BuildOptions::default());
    assert_eq!(again.patterns, m.patterns);
    assert_eq!(again.profiles, m.profiles);
    assert_eq!(again.trips, m.trips);
    assert_eq!(again.stops, m.stops);
}

#[test]
fn a_cell_that_cannot_be_kept_is_reported_and_not_expected_back() {
    let bytes = zip_of(&[
        ("agency.txt", "agency_name,agency_url,agency_timezone\nX,notaurl,Asia/Kolkata\n"),
        ("stops.txt", "stop_id,stop_name,stop_lat,stop_lon,wheelchair_boarding\nA,A,13,80,7\nB,B,13.1,80.1,\n"),
        ("routes.txt", "route_id,route_short_name,route_type\nR,R,3\n"),
        ("calendar.txt", "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\nS,1,1,1,1,1,1,1,20260101,20261231\n"),
        ("trips.txt", "route_id,service_id,trip_id\nR,S,T\n"),
        ("stop_times.txt", "trip_id,arrival_time,departure_time,stop_id,stop_sequence\nT,6:00:00,6:00:00,A,1\nT,6:05:00,6:05:00,B,2\n"),
        ("pathways.txt", "pathway_id,from_stop_id,to_stop_id,pathway_mode,is_bidirectional\nPW,A,B,1,1x\n"),
    ]);
    let (raw, _) = read::read_zip(&bytes).unwrap();
    let (m, findings) = model::FeedModel::from_raw(&raw, "x", model::BuildOptions::default());
    let codes: Vec<(&str, Level)> = findings.iter().map(|f| (f.code, f.level)).collect();
    // the agency's required URL is an error; the stop's accessibility a dropped
    // cell; the pathway a dropped row
    assert!(
        codes.contains(&("invalid_value", Level::Error)),
        "{findings:#?}"
    );
    assert_eq!(
        codes
            .iter()
            .filter(|(c, l)| *c == "invalid_value" && *l == Level::Warning)
            .count(),
        2,
        "{findings:#?}"
    );
    assert!(!m.records.contains_key("pathway"));
    let (back, _) = read::read_zip(&write::zip_bytes(&write::to_raw(&m)).unwrap()).unwrap();
    let diffs = compare::compare(&raw, &back, &m.dropped);
    // only the agency, which could not be kept at all, differs
    assert_eq!(diffs.len(), 1, "{diffs:#?}");
    assert_eq!(diffs[0].file, "agency.txt");
}

/// Minutes in a debug build (chennai_bus alone is 1.4 million stop times):
/// `cargo test --test gtfs_io_roundtrip -- --ignored`, or the release CLI,
/// `gtfs_feed roundtrip --zip Z`.
#[test]
#[ignore]
fn every_shipped_zip_round_trips() {
    let Ok(dir) = std::env::var("NANDI_ASSETS") else {
        eprintln!("NANDI_ASSETS not set; skipping");
        return;
    };
    let mut zips: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with(".gtfs.zip"))
        .collect();
    zips.sort();
    assert!(!zips.is_empty(), "no *.gtfs.zip in {dir}");
    for zip in zips {
        let (raw, _) = read::read_zip(&std::fs::read(&zip).unwrap()).unwrap();
        let (m, findings) = model::FeedModel::from_raw(&raw, "", model::BuildOptions::default());
        let errors: Vec<_> = findings
            .iter()
            .filter(|f| f.level == Level::Error && f.code != "feed_id_mismatch")
            .collect();
        assert!(errors.is_empty(), "{}: {errors:#?}", zip.display());
        let (back, _) = read::read_zip(&write::zip_bytes(&write::to_raw(&m)).unwrap()).unwrap();
        let diffs = compare::compare(&raw, &back, &m.dropped);
        assert!(
            diffs.is_empty(),
            "{}: {:?}",
            zip.display(),
            compare::summarise(&diffs)
        );
    }
}
