//! A reload of a feed served from its stages (docs/gtfs-editor.md sections 18.16
//! and 19), end to end against a real Postgres holding the editor schema with
//! the stage tables (db/gtfs_editor/0025_stages_diversions_unserviceable.sql,
//! 0027_use_stages.sql).
//!
//! A file of its own: it creates and drops a table with a key into gtfs_stop,
//! which would race the other feed tests' deletes of stops if they ran beside
//! it.
//!
//! Runs only when `EDITOR_TEST_DATABASE_URL` is set, and refuses any host that
//! is not local. Uses its own feed and removes its rows afterwards.

use gtfs_routes_service::editor::feed_io;
use gtfs_routes_service::gtfs::spec;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

#[path = "support/gtfs_fixture.rs"]
mod gtfs_fixture;
use gtfs_fixture::fixture;

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

/// Every row of a test feed, in an order its keys allow.
async fn clear_feed(pool: &PgPool, g: &str) {
    let mut tables = vec![
        "gtfs_route_stage",
        "gtfs_stage_stop",
        "gtfs_stage",
        "gtfs_stage_review",
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
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    tables.extend(spec::FILES.iter().filter_map(|f| f.table()));
    tables.push("gtfs_editor_feed_access".into());
    tables.push("gtfs_feed".into());
    for t in tables {
        sqlx::query(&format!("DELETE FROM {t} WHERE gtfs_id = $1"))
            .bind(g)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{t}: {e}"));
    }
}
/// A reload of a feed served from its stages (section 19): the stages, their
/// stops, the links from routes to them and the reviews of them go with the
/// feed, it is served from the zip's stop lists (`use_stages` off), and the
/// check says so before anything is written. A table the reload does not know
/// that still points at a stop is named, not a bare "database error".
#[actix_web::test]
async fn a_reload_clears_the_stages_of_a_feed_built_from_them() {
    let Some(pool) = local_pool().await else {
        return;
    };
    const G: &str = "editor_feed_io_stages_test_feed";
    const STRAY: &str = "zz_editor_feed_io_stray_stop_ref";
    let exec = |sql: String| {
        let pool = pool.clone();
        async move {
            sqlx::query(&sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
    };
    let clear_stages = || async {
        exec(format!("DROP TABLE IF EXISTS {STRAY}")).await;
        for t in [
            "gtfs_route_stage",
            "gtfs_stage_stop",
            "gtfs_stage",
            "gtfs_stage_review",
        ] {
            exec(format!("DELETE FROM {t} WHERE gtfs_id = '{G}'")).await;
        }
    };
    let count = |table: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(&format!(
                "SELECT count(*) FROM {table} WHERE gtfs_id = $1"
            ))
            .bind(G)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let use_stages = || async {
        sqlx::query_scalar::<_, bool>("SELECT use_stages FROM gtfs_feed WHERE gtfs_id = $1")
            .bind(G)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    clear_stages().await;
    clear_feed(&pool, G).await;
    let who = feed_io::Importer {
        user_id: None,
        email: None,
        label: "editor_feed_io_flow stages".into(),
    };
    let zip = fixture(G);
    let seeded = feed_io::import_zip(&pool, &zip, None, false, &who)
        .await
        .unwrap();
    assert!(seeded.seeded, "{seeded:#?}");

    // ---- built from stages, as the stage mapper leaves a feed
    exec(format!(
        "INSERT INTO gtfs_stage (gtfs_id, stage_id, name) VALUES ('{G}', 'S1', 'Central')"
    ))
    .await;
    exec(format!(
        "INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, position, stop_id, stop_type) VALUES \
         ('{G}', 'S1', 1, 'P1', 'NEW STOP'), ('{G}', 'S1', 2, 'B1', 'INTERMEDIATE STOP')"
    ))
    .await;
    exec(format!(
        "INSERT INTO gtfs_route_stage (gtfs_id, route_id, position, stage_id, stage_no) \
         VALUES ('{G}', 'R1', 1, 'S1', 1)"
    ))
    .await;
    exec(format!(
        "INSERT INTO gtfs_stage_review (gtfs_id, batch, name, name_key, reason) \
         VALUES ('{G}', 'test', 'Central', 'central', 'head_differs')"
    ))
    .await;
    exec(format!(
        "UPDATE gtfs_feed SET use_stages = true WHERE gtfs_id = '{G}'"
    ))
    .await;

    // ---- the check: says what goes, writes nothing
    let check = feed_io::reload_zip(&pool, &zip, G, true, &who, None)
        .await
        .unwrap();
    assert!(
        check.round_trip.is_empty(),
        "{:#?}",
        check.round_trip_sample
    );
    assert!(
        check.stages_turned_off,
        "the check says the feed stops using stages"
    );
    let replaced = check.replaced.clone().unwrap();
    for (table, n) in [
        ("gtfs_stage", 1),
        ("gtfs_stage_stop", 2),
        ("gtfs_route_stage", 1),
        ("gtfs_stage_review", 1),
    ] {
        assert_eq!(replaced.get(table), Some(&n), "{table}: {replaced:?}");
    }
    assert!(!check.seeded);
    assert_eq!(count("gtfs_stage").await, 1, "a check writes nothing");
    assert!(use_stages().await);

    // ---- a table the reload does not know, still pointing at a stop: named
    exec(format!(
        "CREATE TABLE {STRAY} (gtfs_id text COLLATE \"C\", stop_id text COLLATE \"C\", \
         FOREIGN KEY (gtfs_id, stop_id) REFERENCES gtfs_stop (gtfs_id, stop_id))"
    ))
    .await;
    exec(format!("INSERT INTO {STRAY} VALUES ('{G}', 'P1')")).await;
    let blocked = feed_io::reload_zip(&pool, &zip, G, true, &who, None).await;
    exec(format!("DROP TABLE {STRAY}")).await;
    let e = blocked.expect_err("a stop still pointed at blocks the reload");
    assert_eq!(e.code, "feed_data_in_use", "{}", e.message);
    assert_eq!(e.status.as_u16(), 409);
    assert_eq!(e.details["table"], "gtfs_stop", "{}", e.details);
    assert_eq!(e.details["referenced_from"], STRAY, "{}", e.details);

    // ---- the reload: the stages go, the zip's stop lists are served
    let reloaded = feed_io::reload_zip(&pool, &zip, G, false, &who, None)
        .await
        .unwrap();
    assert!(reloaded.seeded, "{reloaded:#?}");
    assert!(reloaded.stages_turned_off);
    for t in [
        "gtfs_stage",
        "gtfs_stage_stop",
        "gtfs_route_stage",
        "gtfs_stage_review",
    ] {
        assert_eq!(count(t).await, 0, "{t}");
    }
    assert!(!use_stages().await, "served from its stop lists now");
    assert!(
        count("gtfs_route_stop").await > 0,
        "the zip's stop lists are there"
    );
    // a feed that does not use stages says nothing about them
    let again = feed_io::reload_zip(&pool, &zip, G, true, &who, None)
        .await
        .unwrap();
    assert!(!again.stages_turned_off);

    clear_stages().await;
    clear_feed(&pool, G).await;
}
