//! Vehicle tags as master-table rows: applies the relations/internal SQL for vehicles_internal and
//! vehicle_tag_internal in dependency order, then drives the vehicle upsert (tags + the variant enum), the generic
//! CRUD on the tag master and the vehicle-properties reader against them.
//! Runs only when `VEHICLE_TAGS_TEST_DATABASE_URL` points at a local scratch database (name containing "test" or
//! "scratch"): it drops and recreates those three tables in `public`.

use gtfs_routes_service::services::db_vehicle_reader_internal::{
    DBVehicleReaderInternal, VehicleDataReaderInternal,
};
use gtfs_routes_service::services::operator::{
    DBOperatorService, OperatorService, VehicleUpsertRequest,
};
use gtfs_routes_service::tools::error::AppError;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

const GTFS: &str = "chennai_bus";

const RELATIONS: &[&str] = &[
    "vehicles_internal/schema.sql",
    "vehicles_internal/migrations/0001_vehicles_internal_pkey.sql",
    "vehicles_internal/migrations/0002_vehicles_internal_add_tag_number.sql",
    "vehicles_internal/migrations/0003_drop_fk_sequence_defaults.sql",
    "vehicles_internal/migrations/0004_vehicles_internal_add_vehicle_variant.sql",
    "vehicles_internal/indexes/0006_idx_vehicles_internal_gtfs_fleet_uniq.sql",
    "vehicles_internal/indexes/0008_idx_vehicles_internal_gtfs_vehicle_no_uniq.sql",
    "SEED",
    "vehicle_tag_internal/schema.sql",
    "vehicle_tag_internal/indexes/0001_idx_vehicle_tag_internal_gtfs_id_tag_uniq.sql",
    "vehicles_internal/migrations/0005_vehicles_internal_add_vehicle_tags.sql",
    "vehicles_internal/indexes/0009_idx_vehicles_internal_vehicle_tags.sql",
];

async fn scratch_pool() -> Option<PgPool> {
    let Ok(url) = std::env::var("VEHICLE_TAGS_TEST_DATABASE_URL") else {
        eprintln!("VEHICLE_TAGS_TEST_DATABASE_URL not set; skipping");
        return None;
    };
    assert!(
        url.contains("@127.0.0.1") || url.contains("@localhost"),
        "the vehicle tag tests only run against a local database"
    );
    let db_name = url.rsplit('/').next().unwrap_or_default();
    assert!(
        db_name.contains("test") || db_name.contains("scratch"),
        "the vehicle tag tests drop tables, so the database name must contain \"test\" or \"scratch\""
    );
    Some(
        PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap(),
    )
}

async fn apply_relations(pool: &PgPool) {
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS public.vehicles_internal, public.vehicle_tag_internal CASCADE;
         CREATE SEQUENCE IF NOT EXISTS public.vehicles_vehicle_id_seq;
         CREATE SEQUENCE IF NOT EXISTS public.vehicles_bus_service_type_id_seq;
         CREATE SEQUENCE IF NOT EXISTS public.vehicles_entity_id_seq;
         CREATE SEQUENCE IF NOT EXISTS public.vehicles_organization_id_seq;",
    )
    .execute(pool)
    .await
    .unwrap();
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/relations/internal/");
    for file in RELATIONS {
        let sql = if *file == "SEED" {
            "INSERT INTO public.vehicles_internal (vehicle_id, vehicle_no, fleet_no, gtfs_id) VALUES
                ('seed-1', 'SEED1', 'SEED1', 'chennai_bus'),
                ('seed-2', 'SEED2', 'SEED2', 'kolkata_bus')"
                .to_string()
        } else {
            std::fs::read_to_string(format!("{}{}", root, file)).unwrap()
        };
        sqlx::raw_sql(&sql)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{}: {}", file, e));
    }
}

fn upsert(vehicle_no: &str, fields: Value) -> VehicleUpsertRequest {
    let mut body = json!({ "vehicle_no": vehicle_no, "fleet_no": vehicle_no });
    body.as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    serde_json::from_value(body).unwrap()
}

fn bad_request(result: Result<impl std::fmt::Debug, AppError>, needle: &str) {
    match result {
        Err(AppError::BadRequest(msg)) => {
            assert!(msg.contains(needle), "{} lacks {:?}", msg, needle)
        }
        other => panic!(
            "expected BadRequest containing {:?}, got {:?}",
            needle, other
        ),
    }
}

#[tokio::test]
async fn vehicle_tags_are_master_rows_and_variant_is_an_enum() {
    let Some(pool) = scratch_pool().await else {
        return;
    };
    apply_relations(&pool).await;
    let svc = DBOperatorService::new(pool.clone(), None);

    svc.upsert_one_row(
        "vehicle_tag_internal",
        GTFS,
        json!([{ "tag": "SPECIAL" }, { "tag": "FEEDER" }]),
        None,
    )
    .await
    .unwrap();

    let rows = svc
        .upsert_vehicles(
            GTFS,
            vec![upsert(
                "TN01A1",
                json!({ "vehicle_variant": "TEMPO_TRAVELLER", "vehicle_tags": [" SPECIAL", "FEEDER"] }),
            )],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].vehicle_variant.as_deref(), Some("TEMPO_TRAVELLER"));
    assert_eq!(
        rows[0].vehicle_tags,
        Some(vec!["SPECIAL".to_string(), "FEEDER".to_string()]),
        "tags are trimmed and keep their order"
    );

    bad_request(
        svc.upsert_vehicles(
            GTFS,
            vec![upsert("TN01A2", json!({ "vehicle_tags": ["NOPE"] }))],
        )
        .await,
        "not active",
    );
    bad_request(
        svc.upsert_vehicles(
            GTFS,
            vec![upsert(
                "TN01A2",
                json!({ "vehicle_tags": ["FEEDER", "FEEDER"] }),
            )],
        )
        .await,
        "repeats",
    );
    bad_request(
        svc.upsert_vehicles(
            GTFS,
            vec![upsert("TN01A2", json!({ "vehicle_tags": [" "] }))],
        )
        .await,
        "must be non-empty",
    );
    for variant in ["AC_BUS", "AC_TEMPO_TRAVELLER"] {
        let rows = svc
            .upsert_vehicles(
                GTFS,
                vec![upsert("TN01A2", json!({ "vehicle_variant": variant }))],
            )
            .await
            .unwrap();
        assert_eq!(rows[0].vehicle_variant.as_deref(), Some(variant));
    }
    assert!(
        serde_json::from_value::<VehicleUpsertRequest>(
            json!({ "vehicle_no": "TN01A3", "vehicle_variant": "AC_TEMPO" })
        )
        .is_err(),
        "an unknown variant is refused when the request is read"
    );

    let special_id: String = sqlx::query_scalar(
        "SELECT vehicle_tag_id FROM public.vehicle_tag_internal WHERE gtfs_id = $1 AND tag = 'SPECIAL'",
    )
    .bind(GTFS)
    .fetch_one(&pool)
    .await
    .unwrap();
    bad_request(
        svc.delete_one_row(
            "vehicle_tag_internal",
            GTFS,
            json!({ "vehicle_tag_id": special_id }),
        )
        .await,
        "still on 1 vehicle",
    );
    bad_request(
        svc.upsert_one_row(
            "vehicle_tag_internal",
            GTFS,
            json!({ "tag": "FEEDER" }),
            None,
        )
        .await,
        "already exists",
    );
    bad_request(
        svc.upsert_one_row(
            "vehicle_tag_internal",
            GTFS,
            json!({ "vehicle_tag_id": special_id, "tag": "SPECIAL", "deleted": true }),
            None,
        )
        .await,
        "delete through",
    );
    bad_request(
        svc.upsert_one_row(
            "vehicle_tag_internal",
            GTFS,
            json!({ "vehicle_tag_id": special_id, "tag": "SPECIAL2" }),
            None,
        )
        .await,
        "cannot be changed",
    );
    svc.upsert_one_row(
        "vehicle_tag_internal",
        GTFS,
        json!({ "vehicle_tag_id": special_id, "tag": "SPECIAL" }),
        None,
    )
    .await
    .expect("re-sending a row with its own tag is allowed");

    let reader = DBVehicleReaderInternal::new(pool.clone());
    let props = reader.get_vehicle_properties("TN01A1", GTFS).await.unwrap();
    assert_eq!(props.variant.as_deref(), Some("TEMPO_TRAVELLER"));
    assert_eq!(
        props.vehicle_tags,
        Some(vec!["SPECIAL".to_string(), "FEEDER".to_string()])
    );

    let queried = svc
        .query_vehicles(GTFS, Some("TN01A1"), None, None, None)
        .await
        .unwrap();
    assert_eq!(
        queried[0].vehicle_tags,
        Some(vec!["SPECIAL".to_string(), "FEEDER".to_string()])
    );

    svc.upsert_vehicles(
        GTFS,
        vec![upsert("TN01A4", json!({ "vehicle_tags": ["FEEDER"] }))],
    )
    .await
    .unwrap();
    let vehicle_nos = |rows: Vec<gtfs_routes_service::services::operator::FleetRow>| {
        let mut v: Vec<String> = rows.into_iter().filter_map(|r| r.vehicle_no).collect();
        v.sort();
        v
    };
    assert_eq!(
        vehicle_nos(
            svc.query_vehicles(GTFS, None, None, None, Some("FEEDER"))
                .await
                .unwrap()
        ),
        vec!["TN01A1".to_string(), "TN01A4".to_string()],
        "a tag query returns every vehicle carrying it, wherever it sits in the order"
    );
    assert_eq!(
        vehicle_nos(
            svc.query_vehicles(GTFS, None, None, None, Some("SPECIAL"))
                .await
                .unwrap()
        ),
        vec!["TN01A1".to_string()]
    );
    assert!(svc
        .query_vehicles(GTFS, None, None, None, Some("NOPE"))
        .await
        .unwrap()
        .is_empty());
    assert!(
        svc.query_vehicles("kolkata_bus", None, None, None, Some("FEEDER"))
            .await
            .unwrap()
            .is_empty(),
        "a tag query stays inside its feed"
    );

    svc.upsert_vehicles(GTFS, vec![upsert("TN01A1", json!({ "vehicle_tags": [] }))])
        .await
        .unwrap();
    let cleared = reader.get_vehicle_properties("TN01A1", GTFS).await.unwrap();
    assert_eq!(
        cleared.vehicle_tags, None,
        "the same reader sees the cleared tags at once (no cache in GIMS); an empty list reads back as no tags"
    );

    svc.delete_one_row(
        "vehicle_tag_internal",
        GTFS,
        json!({ "vehicle_tag_id": special_id }),
    )
    .await
    .expect("a tag no vehicle carries can be deleted");
}
