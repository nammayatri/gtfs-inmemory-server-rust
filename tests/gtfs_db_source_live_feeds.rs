//! `GtfsDbSource::live_feeds` against a real Postgres holding the editor
//! schema (db/gtfs_editor/0001..0008): the live, per-feed `data_source` query
//! that `GTFSService::start_db_version_polling` now reconciles against every
//! cycle, instead of a fixed `gtfs_db_feeds` list.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host
//! that is not local (see scripts/editor_flow_test.sh). Uses its own feed
//! (`gims_test_live_feed`) and removes it afterwards; never touches
//! chennai_bus or any other real feed's row.

use gtfs_routes_service::services::gtfs_db_source::GtfsDbSource;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

const FEED: &str = "gims_test_live_feed";

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
            .max_connections(5)
            .connect(&url)
            .await
            .unwrap(),
    )
}

async fn clear(pool: &PgPool) {
    sqlx::query("DELETE FROM gtfs_feed WHERE gtfs_id = $1")
        .bind(FEED)
        .execute(pool)
        .await
        .unwrap();
}

async fn upsert(pool: &PgPool, data_source: &str, version: i64) {
    sqlx::query(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, version, data_source) \
         VALUES ($1, 'GIMS live_feeds test', $2, $3) \
         ON CONFLICT (gtfs_id) DO UPDATE SET version = $2, data_source = $3",
    )
    .bind(FEED)
    .bind(version)
    .bind(data_source)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn live_feeds_reflects_the_row_in_both_directions() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;

    let db = GtfsDbSource::new(pool.clone(), vec![]);

    // No row at all, and not in the fallback list: completely absent.
    let live = db.live_feeds(&[]).await.unwrap();
    assert_eq!(live.get(FEED), None, "untouched feed must be absent");

    // data_source = 'db': picked up with its version, even though the feed
    // is not named in any static fallback list.
    upsert(&pool, "db", 3).await;
    let live = db.live_feeds(&[]).await.unwrap();
    assert_eq!(live.get(FEED), Some(&Some(3)));

    // version moves: live_feeds reflects the new value on the next call.
    upsert(&pool, "db", 4).await;
    let live = db.live_feeds(&[]).await.unwrap();
    assert_eq!(live.get(FEED), Some(&Some(4)));

    // Flipped back to 'preprocessed': absent, even if a fallback list still
    // names it - the row wins over the static list.
    upsert(&pool, "preprocessed", 5).await;
    let live = db.live_feeds(&[FEED.to_string()]).await.unwrap();
    assert_eq!(
        live.get(FEED),
        None,
        "a preprocessed row must win over the fallback list"
    );

    // Flipped to 'db' again: picked up again (round trip).
    upsert(&pool, "db", 6).await;
    let live = db.live_feeds(&[]).await.unwrap();
    assert_eq!(live.get(FEED), Some(&Some(6)));

    clear(&pool).await;
}

#[tokio::test]
async fn live_feeds_fallback_only_applies_when_there_is_no_row() {
    let Some(pool) = local_pool().await else {
        return;
    };
    clear(&pool).await;

    let db = GtfsDbSource::new(pool.clone(), vec![]);

    // No row, but named in the fallback list: treated as db mode with an
    // unknown version (nothing to load until a row exists).
    let live = db.live_feeds(&[FEED.to_string()]).await.unwrap();
    assert_eq!(live.get(FEED), Some(&None));

    // A row appears with data_source = 'preprocessed' (e.g. explicitly
    // created that way): the fallback list no longer applies.
    upsert(&pool, "preprocessed", 1).await;
    let live = db.live_feeds(&[FEED.to_string()]).await.unwrap();
    assert_eq!(live.get(FEED), None);

    clear(&pool).await;
}
