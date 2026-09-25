//! What GIMS serves from a feed imported whole into the editor's tables
//! (docs/gtfs-editor.md section 18): the loader, on a feed seeded from a zip
//! that uses every file of the reference, against a real Postgres holding the
//! editor schema (db/gtfs_editor/0001..0023):
//!
//! - a route is named by its own agency, as the preprocessor names it;
//! - every stop and station is served (`stops_scope = 'all'`), in the order the
//!   feed had them, and never an entrance or a generic node;
//! - two stop orders that differ only in what a stop time says besides its
//!   stop are one public pattern, with its first trip's stops and every trip of
//!   both;
//! - a feed that numbers its stops otherwise than 1 to n keeps its numbers, in
//!   the pattern and in `/trip`.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local. Uses its own feed and removes its rows afterwards.

use gtfs_routes_service::editor::feed_io;
use gtfs_routes_service::gtfs::spec;
use gtfs_routes_service::services::gtfs_db_source::GtfsDbSource;
use gtfs_routes_service::services::gtfs_timing;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::collections::HashMap;

#[path = "support/gtfs_fixture.rs"]
mod gtfs_fixture;

const FEED: &str = "gtfs_db_full_spec_test_feed";

async fn local_pool() -> Option<PgPool> {
    let Ok(url) = std::env::var("EDITOR_TEST_DATABASE_URL") else {
        eprintln!("EDITOR_TEST_DATABASE_URL not set; skipping");
        return None;
    };
    assert!(
        url.contains("@127.0.0.1") || url.contains("@localhost"),
        "this test only runs against a local database"
    );
    Some(
        PgPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap(),
    )
}

async fn clear_feed(pool: &PgPool) {
    let mut tables: Vec<String> = [
        "gtfs_change_set",
        "gtfs_frequency",
        "gtfs_trip",
        "gtfs_timing_profile",
        "gtfs_route_stop",
        "gtfs_pattern",
        "gtfs_service_date",
        "gtfs_service",
        "gtfs_route",
        "gtfs_stop",
    ]
    .iter()
    .map(|t| t.to_string())
    .collect();
    tables.extend(spec::FILES.iter().filter_map(|f| f.table()));
    tables.push("gtfs_feed".into());
    for t in tables {
        sqlx::query(&format!("DELETE FROM {t} WHERE gtfs_id = $1"))
            .bind(FEED)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{t}: {e}"));
    }
}

#[actix_web::test]
async fn a_feed_imported_whole_is_served_as_the_preprocessor_serves_it() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear_feed(&pool).await;
    let who = feed_io::Importer {
        user_id: None,
        email: None,
        label: "gtfs_db_full_spec_flow".into(),
    };
    let report = feed_io::import_zip(&pool, &gtfs_fixture::fixture(FEED), None, false, &who)
        .await
        .unwrap();
    assert!(report.seeded, "{report:#?}");
    sqlx::query("UPDATE gtfs_feed SET data_source = 'db', trips_source = 'db' WHERE gtfs_id = $1")
        .bind(FEED)
        .execute(&pool)
        .await
        .unwrap();

    let feed = GtfsDbSource::new(pool.clone(), vec![])
        .load_feed(FEED, &HashMap::new())
        .await
        .unwrap();

    // ---- each route under its own agency
    let agency: HashMap<String, Option<String>> = feed
        .routes
        .iter()
        .map(|r| (r.id.clone(), r.agency_name.clone()))
        .collect();
    assert_eq!(agency[&format!("{FEED}:R1")].as_deref(), Some("Metro"));
    assert_eq!(agency[&format!("{FEED}:R2")].as_deref(), Some("Bus"));

    // ---- every stop and station, in file order; no entrance, no node
    let stops: Vec<&str> = feed
        .stops
        .iter()
        .map(|s| s.id.rsplit(':').next().unwrap())
        .collect();
    assert_eq!(stops, vec!["STN", "P1", "P2", "B1", "B2", "SELF"]);
    let own_parent = feed.stops.iter().find(|s| s.id.ends_with(":SELF")).unwrap();
    assert_eq!(
        own_parent.station_id.as_deref(),
        Some(&*format!("{FEED}:SELF"))
    );

    // ---- R1: its pattern 1 and the pattern that differs only in a pickup are
    // one public pattern, with T1's stops and the trips of both in trip order;
    // the short turn is its own
    let r1: Vec<_> = feed
        .patterns
        .iter()
        .filter(|p| p.route_id == format!("{FEED}:R1"))
        .collect();
    assert_eq!(r1.len(), 2, "{r1:#?}");
    assert_eq!(
        r1[0].id,
        gtfs_timing::pattern_id(FEED, "R1", &["P1", "B1", "B2"])
    );
    let trips: Vec<&str> = r1[0].trips.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(trips, vec!["T1", "T2", "T4"]);
    assert_eq!(r1[0].stops[0].headsign.as_deref(), Some("Beach"));
    assert_eq!(
        r1[0].stops[0].arrival_time,
        Some(5 * 3600 + 30 * 60),
        "T1's times"
    );
    assert_eq!(r1[1].id, gtfs_timing::pattern_id(FEED, "R1", &["P1", "B1"]));
    let route = feed
        .routes
        .iter()
        .find(|r| r.id == format!("{FEED}:R1"))
        .unwrap();
    assert_eq!(route.trip_count, Some(4));

    // ---- R2 numbers its stops 10 and 20, in the pattern and in /trip
    let r2 = feed
        .patterns
        .iter()
        .find(|p| p.route_id == format!("{FEED}:R2"))
        .unwrap();
    let seq: Vec<Option<i32>> = r2.stops.iter().map(|s| s.stop_sequence).collect();
    assert_eq!(seq, vec![Some(10), Some(20)]);
    let t5 = feed.trips.as_ref().unwrap().trip("T5").unwrap();
    let seq: Vec<Option<i32>> = t5.stops.iter().map(|(s, _, _)| s.sequence).collect();
    assert_eq!(seq, vec![Some(10), Some(20)]);
    assert_eq!(t5.stops[1].1, 26 * 3600 + 7 * 60 + 15, "past midnight");
    // T4 runs its own stop order, pickup and all
    let t4 = feed.trips.as_ref().unwrap().trip("T4").unwrap();
    assert_eq!(t4.stops.len(), 3);
    assert_eq!(t4.stops[0].1, 8 * 3600);

    clear_feed(&pool).await;
}
