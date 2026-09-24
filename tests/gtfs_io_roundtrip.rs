//! A GTFS zip read into the model the editor's tables hold, written back out and
//! compared (docs/gtfs-editor.md section 18). No database.
//!
//! - `a_feed_using_every_file_round_trips`: a small feed with a row in every
//!   file of the reference, and the awkward cases the shipped feeds have.
//! - `every_shipped_zip_round_trips` (ignored: run it with `--ignored`): every
//!   `*.gtfs.zip` in `$NANDI_ASSETS` (nandi's `assets/`).

use gtfs_routes_service::gtfs::{compare, model, read, write, Level};
use std::io::{Cursor, Write};

fn zip_of(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, body) in entries {
        w.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(body.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn fixture() -> Vec<u8> {
    zip_of(&[
        ("f.gtfs/agency.txt", "agency_id,agency_name,agency_url,agency_timezone,agency_lang,agency_phone,agency_fare_url,agency_email\nA1,Metro,https://m.example,Asia/Kolkata,en,044-1,https://m.example/fares,ops@m.example\nA2,Bus,https://b.example,Asia/Kolkata,ta,,,\n"),
        ("f.gtfs/feed_info.txt", "feed_publisher_name,feed_publisher_url,feed_lang,default_lang,feed_start_date,feed_end_date,feed_version,feed_contact_email,feed_contact_url,feed_id\nNY,https://ny.example,en,en,20260101,20261231,v7,a@ny.example,https://ny.example/c,test_feed\n"),
        ("f.gtfs/levels.txt", "level_id,level_index,level_name\nL0,0,Street\nL-1,-1,Concourse\n"),
        ("f.gtfs/stops.txt", "stop_id,stop_code,stop_name,tts_stop_name,stop_desc,stop_lat,stop_lon,zone_id,stop_url,location_type,parent_station,stop_timezone,wheelchair_boarding,level_id,platform_code,info_json\n\
STN,STN,Central,Central station,The big one,13.08,80.27,Z1,https://m.example/stn,1,,Asia/Kolkata,1,,,\n\
P1,P1,Central,,Platform 1,13.0801,80.2701,Z1,,0,STN,,1,L-1,1,\n\
P2,P2,Central,,Platform 2,13.0802,80.2702,Z1,,0,STN,,2,L-1,2,\n\
E1,E1,Central Gate A,,,13.0805,80.2705,,,2,STN,,1,L0,,\n\
B1,B1,Beach,,,13.10,80.29,Z2,,,,,,,,\"{\"\"clusterId\"\": \"\"c7\"\"}\"\n\
B2,B2,Beach 2,,,13.11,80.30,Z2,,,,,,,,\n\
SELF,SELF,Loop,,,13.2,80.2,,,0,SELF,,,,,\n\
N1,,,,,13.0803,80.2703,,,3,STN,,,,,\n"),
        ("f.gtfs/routes.txt", "route_id,agency_id,route_short_name,route_long_name,route_desc,route_type,route_url,route_color,route_text_color,route_sort_order,continuous_pickup,continuous_drop_off\n\
R1,A1,Blue,Central - Beach,Metro blue,1,https://m.example/blue,0000ff,FFFFFF,2,1,1\n\
R2,A2,21G,,,3,,,,,,\n\
R3,A2,EXT,,,700,,,,,,\n"),
        ("f.gtfs/calendar.txt", "service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\nWK,1,1,1,1,1,0,0,20260101,20261231\n"),
        ("f.gtfs/calendar_dates.txt", "service_id,date,exception_type\nWK,20260126,2\nHOL,20260815,1\nHOL,20261002,1\n"),
        ("f.gtfs/shapes.txt", "shape_id,shape_pt_lat,shape_pt_lon,shape_pt_sequence,shape_dist_traveled\nSH1,13.08,80.27,1,0\nSH1,13.09,80.28,2,1.4\nSH1,13.10,80.29,3,2.9\n"),
        ("f.gtfs/trips.txt", "route_id,service_id,trip_id,trip_headsign,trip_short_name,direction_id,block_id,shape_id,wheelchair_accessible,bikes_allowed,cars_allowed\n\
R1,WK,T1,Beach,B1,0,BLK1,SH1,1,2,2\n\
R1,WK,T2,Beach,,0,,SH1,,,\n\
R1,HOL,T3,Beach (short),,0,,,,,\n\
R1,WK,T4,Beach,,0,,SH1,,,\n\
R2,WK,T5,,,1,,,,,\n\
R3,HOL,T6,,,,,,,,\n"),
        // T1 and T2: one stop order and one timing, from different starts;
        // T4: the same stops, but no pickup at P1 - a pattern of its own;
        // T3: a short turn; T5 numbers its stops from 10 in steps of 10
        ("f.gtfs/stop_times.txt", "trip_id,arrival_time,departure_time,stop_id,stop_sequence,stop_headsign,pickup_type,drop_off_type,continuous_pickup,continuous_drop_off,shape_dist_traveled,timepoint\n\
T1,5:30:00,5:30:30,P1,1,Beach,,,,,0,1\n\
T1,05:34:00,05:34:30,B1,2,,,,,,2.9,1\n\
T1,05:40:00,05:40:00,B2,3,,,1,,,,0\n\
T2,6:00:00,6:00:30,P1,1,Beach,,,,,0,1\n\
T2,06:04:00,06:04:30,B1,2,,,,,,2.9,1\n\
T2,06:10:00,06:10:00,B2,3,,,1,,,,0\n\
T3,7:00:00,7:00:00,P1,1,,,,,,,\n\
T3,7:03:00,7:03:00,B1,2,,,,,,,\n\
T4,8:00:00,8:00:30,P1,1,Beach,1,,,,0,1\n\
T4,08:04:00,08:04:30,B1,2,,,,,,2.9,1\n\
T4,08:10:00,08:10:00,B2,3,,,1,,,,0\n\
T5,25:50:00,25:50:00,B2,10,,,,,,,\n\
T5,26:07:15,26:07:15,P2,20,,,,,,,\n\
T6,9:00:00,9:00:00,SELF,1,,,,,,,\n\
T6,9:10:00,9:10:00,B2,2,,,,,,,\n"),
        ("f.gtfs/frequencies.txt", "trip_id,start_time,end_time,headway_secs,exact_times\nT2,06:00:00,09:00:00,600,0\nT2,17:00:00,20:00:00,300,\n"),
        ("f.gtfs/transfers.txt", "from_stop_id,to_stop_id,from_route_id,to_route_id,from_trip_id,to_trip_id,transfer_type,min_transfer_time\nP1,P2,,,,,2,180\nP2,P1,,,,,2,180\n,,,,T1,T5,4,\n"),
        ("f.gtfs/pathways.txt", "pathway_id,from_stop_id,to_stop_id,pathway_mode,is_bidirectional,length,traversal_time,stair_count,max_slope,min_width,signposted_as,reversed_signposted_as\nPW1,E1,P1,2,1,12.5,40,24,,1.2,To platforms,To Gate A\nPW2,N1,P2,5,1,,30,,,,,\nPW3,E1,N1,1,0,5.0,,,,,,\n"),
        ("f.gtfs/fare_attributes.txt", "fare_id,price,currency_type,payment_method,transfers,agency_id,transfer_duration\nF1,10.50,INR,1,,A1,3600\n"),
        ("f.gtfs/fare_rules.txt", "fare_id,route_id,origin_id,destination_id,contains_id\nF1,R1,Z1,Z2,\nF1,,Z2,Z1,\n"),
        ("f.gtfs/timeframes.txt", "timeframe_group_id,start_time,end_time,service_id\npeak,07:00:00,10:00:00,WK\nall,,,WK\n"),
        ("f.gtfs/rider_categories.txt", "rider_category_id,rider_category_name,is_default_fare_category,eligibility_url\nadult,Adult,1,\nstudent,Student,0,https://m.example/students\n"),
        ("f.gtfs/fare_media.txt", "fare_media_id,fare_media_name,fare_media_type\ncard,Travel card,2\n"),
        ("f.gtfs/fare_products.txt", "fare_product_id,fare_product_name,rider_category_id,fare_media_id,amount,currency\nsingle,Single,adult,card,20.00,INR\nsingle,Single,student,card,10.00,INR\n"),
        ("f.gtfs/areas.txt", "area_id,area_name\nAR1,Central\n"),
        ("f.gtfs/stop_areas.txt", "area_id,stop_id\nAR1,P1\nAR1,P2\n"),
        ("f.gtfs/networks.txt", "network_id,network_name\nmetro,Metro\n"),
        ("f.gtfs/route_networks.txt", "network_id,route_id\nmetro,R1\n"),
        ("f.gtfs/fare_leg_rules.txt", "leg_group_id,network_id,from_area_id,to_area_id,from_timeframe_group_id,to_timeframe_group_id,fare_product_id,rule_priority\nG1,metro,AR1,,peak,,single,1\n"),
        ("f.gtfs/fare_leg_join_rules.txt", "from_network_id,to_network_id,from_stop_id,to_stop_id\nmetro,metro,P1,P2\n"),
        ("f.gtfs/fare_transfer_rules.txt", "from_leg_group_id,to_leg_group_id,transfer_count,duration_limit,duration_limit_type,fare_transfer_type,fare_product_id\nG1,G1,-1,5400,1,0,\n"),
        ("f.gtfs/location_groups.txt", "location_group_id,location_group_name\nLG1,Beach stops\n"),
        ("f.gtfs/location_group_stops.txt", "location_group_id,stop_id\nLG1,B1\nLG1,B2\n"),
        ("f.gtfs/booking_rules.txt", "booking_rule_id,booking_type,prior_notice_duration_min,prior_notice_duration_max,prior_notice_last_day,prior_notice_last_time,prior_notice_start_day,prior_notice_start_time,prior_notice_service_id,message,pickup_message,drop_off_message,phone_number,info_url,booking_url\nBR1,1,30,,,,,,,Call ahead,,,044-2,https://m.example/book,\n"),
        ("f.gtfs/translations.txt", "table_name,field_name,language,translation,record_id,record_sub_id,field_value\nstops,stop_name,ta,சென்ட்ரல்,STN,,\nroutes,route_long_name,ta,சென்ட்ரல் - கடற்கரை,R1,,\nfeed_info,feed_publisher_name,ta,என்ஒய்,,,\n"),
        ("f.gtfs/attributions.txt", "attribution_id,agency_id,route_id,trip_id,organization_name,is_producer,is_operator,is_authority,attribution_url,attribution_email,attribution_phone\nAT1,A1,,,Chennai Metro Rail,0,1,1,https://m.example,,\n,,R2,,Moving Tech,1,,,,,\n"),
        ("f.gtfs/locations.geojson", r#"{"type":"FeatureCollection","features":[{"type":"Feature","id":"Z-beach","properties":{"stop_name":"Beach zone"},"geometry":{"type":"Polygon","coordinates":[[[80.28,13.09],[80.31,13.09],[80.31,13.12],[80.28,13.09]]]}}]}"#),
        ("f.gtfs/._stops.txt", "junk"),
    ])
}

#[test]
fn a_feed_using_every_file_round_trips() {
    let (raw, mut findings) = read::read_zip(&fixture()).unwrap();
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
