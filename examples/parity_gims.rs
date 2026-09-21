//! A local GIMS for `scripts/parity_gtfs_db.py` (docs/gtfs-editor.md sections 1
//! and 16.6), and a pattern-level check to go with it.
//!
//!     DHALL_CONFIG=a.dhall cargo run --example parity_gims -- serve
//!     cargo run --example parity_gims -- patterns --db postgres://postgres@127.0.0.1:55432/db \
//!         --preprocessed-dir DIR --gtfs-id chennai_bus
//!
//! `serve` answers the public static APIs of `handlers::routes` and polls
//! `gtfs_feed` like a pod does. It starts none of the live-vehicle pollers the
//! service binary starts: a parity run compares static data and must never call
//! a vendor's API.
//!
//! `patterns` compares the patterns a feed's preprocessed data holds with the
//! ones the loader builds from the tables (`trips_source = 'db'`), pattern by
//! pattern: public id, stops with their times and headsigns, and trips, and the
//! order of each route's patterns. No public API serves a pattern whole, so this
//! is where they are compared. It reads the database; it writes nothing.

use actix_web::{web, App, HttpServer};
use gtfs_routes_service::environment::{self, AppState};
use gtfs_routes_service::handlers::routes::create_routes;
use gtfs_routes_service::models::NandiPatternDetails;
use gtfs_routes_service::services::gtfs_db_source::GtfsDbSource;
use serde_json::Value;
use std::collections::HashMap;

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve().await,
        Some("patterns") => {
            let arg = |name: &str| {
                args.iter()
                    .position(|a| a == name)
                    .and_then(|i| args.get(i + 1))
                    .cloned()
                    .unwrap_or_else(|| panic!("{name} is required"))
            };
            let failed =
                patterns(&arg("--db"), &arg("--preprocessed-dir"), &arg("--gtfs-id")).await?;
            std::process::exit(if failed { 1 } else { 0 });
        }
        _ => {
            eprintln!(
                "usage: parity_gims serve | patterns --db URL --preprocessed-dir DIR --gtfs-id G"
            );
            std::process::exit(2);
        }
    }
}

async fn serve() -> anyhow::Result<()> {
    let path = std::env::var("DHALL_CONFIG")
        .unwrap_or_else(|_| "./dhall-configs/dev/gtfs_in_memory_server_rust.dhall".to_string());
    let config = environment::read_dhall_config(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let _guard = shared::tools::logger::setup_tracing(config.logger_cfg);
    let port = config.port;
    let state = AppState::new(config).await?;
    if state.gtfs_service.has_db_feeds() {
        let service = state.gtfs_service.clone();
        tokio::spawn(async move { service.start_db_version_polling().await });
    }
    let data = web::Data::new(state);
    HttpServer::new(move || App::new().app_data(data.clone()).configure(create_routes))
        .bind(("127.0.0.1", port))?
        .run()
        .await?;
    Ok(())
}

/// The fields of a pattern that are compared, as JSON: stops with everything
/// the loader sets, trips with their direction.
fn comparable(p: &NandiPatternDetails) -> Value {
    serde_json::json!({
        "route_id": p.route_id,
        "desc": p.desc,
        "stops": p.stops,
        "trips": p.trips,
    })
}

async fn patterns(db: &str, dir: &str, gtfs_id: &str) -> anyhow::Result<bool> {
    assert!(
        db.contains("@127.0.0.1") || db.contains("@localhost"),
        "patterns reads a local database only"
    );
    let raw = std::fs::read(std::path::Path::new(dir).join("patterns.json"))?;
    let mut by_feed: HashMap<String, Vec<NandiPatternDetails>> = serde_json::from_slice(&raw)?;
    let pre = by_feed.remove(gtfs_id).unwrap_or_default();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(db)
        .await?;
    let feed = GtfsDbSource::new(pool, vec![])
        .load_feed(gtfs_id, &HashMap::new())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    anyhow::ensure!(
        feed.trips.is_some(),
        "{gtfs_id} does not take its trips from the tables (trips_source)"
    );

    let order = |list: &[NandiPatternDetails]| -> HashMap<String, Vec<String>> {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for p in list {
            out.entry(p.route_id.clone())
                .or_default()
                .push(p.id.clone());
        }
        out
    };
    let (pre_order, db_order) = (order(&pre), order(&feed.patterns));
    let db_by_id: HashMap<&str, &NandiPatternDetails> =
        feed.patterns.iter().map(|p| (p.id.as_str(), p)).collect();
    let (mut same, mut differ, mut missing) = (0, 0, 0);
    let mut examples = Vec::new();
    for p in &pre {
        match db_by_id.get(p.id.as_str()) {
            None => {
                missing += 1;
                if examples.len() < 5 {
                    examples.push(format!(
                        "{} ({}): not built from the tables",
                        p.id, p.route_id
                    ));
                }
            }
            Some(d) if comparable(d) == comparable(p) => same += 1,
            Some(d) => {
                differ += 1;
                if examples.len() < 5 {
                    examples.push(format!(
                        "{}: preprocessed {} / tables {}",
                        p.id,
                        comparable(p)
                            .to_string()
                            .chars()
                            .take(300)
                            .collect::<String>(),
                        comparable(d)
                            .to_string()
                            .chars()
                            .take(300)
                            .collect::<String>()
                    ));
                }
            }
        }
    }
    let extra = feed.patterns.len().saturating_sub(pre.len() - missing);
    let reordered = pre_order
        .iter()
        .filter(|(route, ids)| db_order.get(*route) != Some(*ids))
        .count();
    println!(
        "{gtfs_id}: {} preprocessed patterns, {} from the tables: {same} identical, {differ} differing, \
         {missing} missing, {extra} extra; {} routes, {reordered} with their patterns in another order",
        pre.len(),
        feed.patterns.len(),
        pre_order.len()
    );
    for e in &examples {
        println!("  {e}");
    }
    let failed = differ + missing + extra + reordered > 0;
    println!(
        "{gtfs_id}: PATTERNS {}",
        if failed { "DIFFER" } else { "IDENTICAL" }
    );
    Ok(failed)
}
