use chrono::{Duration, NaiveDate};
use gtfs_routes_service::services::db_vehicle_reader_internal::{
    DBVehicleReaderInternal, VehicleDataReaderInternal,
};
use gtfs_routes_service::services::gtfs_service::RouteCatalog;
use gtfs_routes_service::services::reader_config::{ReaderConfig, V1V2Cutoff};
use gtfs_routes_service::services::service_hopper::today_ist;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::Arc;

const V2_TABLES: [&str; 4] = ["duties", "duty_groups", "trips", "trip_groups"];

const MASTER_TABLES: [&str; 4] = [
    "service_type_internal",
    "entities_internal",
    "vehicles_internal",
    "employees_internal",
];

async fn exec(pool: &PgPool, sql: &str, gtfs: &str) {
    sqlx::query(sql).bind(gtfs).execute(pool).await.unwrap();
}

async fn cleanup(pool: &PgPool, gtfs: &str) {
    exec(
        pool,
        "DELETE FROM waybills_internal WHERE gtfs_id = $1",
        gtfs,
    )
    .await;
    for t in V2_TABLES.iter().chain(MASTER_TABLES.iter()) {
        exec(pool, &format!("DELETE FROM {} WHERE gtfs_id = $1", t), gtfs).await;
    }
    exec(
        pool,
        "DELETE FROM organizations_internal WHERE organization_id = 'org-'||$1",
        gtfs,
    )
    .await;
}

async fn seed_masters(pool: &PgPool, gtfs: &str) {
    for sql in [
        "INSERT INTO organizations_internal (organization_id, organization_name, code) VALUES ('org-'||$1, 'Org', 'ORG-'||$1)",
        "INSERT INTO service_type_internal (service_type_id, service_type_name, gtfs_id) VALUES ('st-'||$1, 'Deluxe', $1)",
        "INSERT INTO entities_internal (entity_id, entity_name, entity_remark, organization_id, gtfs_id) VALUES ('dep-'||$1, 'Depot One', 'D1 remark', 'org-'||$1, $1)",
        "INSERT INTO vehicles_internal (vehicle_id, fleet_no, vehicle_no, bus_service_type_id, entity_id, organization_id, gtfs_id) VALUES ('veh-'||$1, 'VR1', 'TN01', 'st-'||$1, 'dep-'||$1, 'org-'||$1, $1)",
        "INSERT INTO employees_internal (emp_id, token_no, first_name, last_name, mobile_no, department_id, designation_id, entity_id, organization_id, gtfs_id) VALUES ('emp-'||$1, 'DR1', 'Ravi', 'K', '9999999999', 'x', 'x', 'x', 'org-'||$1, $1)",
        "INSERT INTO trip_groups (id, gtfs_id, code, zone, shift, first_departure, trip_type, depot_id, service_type_id, organization_id)
         VALUES ('tg-'||$1, $1, 'RDONE_MORNING_1205AM_NORMAL', 'RDONE', 'MORNING', '00:05', 'NORMAL', 'dep-'||$1, 'planned-st', 'org-'||$1)",
        "INSERT INTO trips (id, trip_group_id, gtfs_id, route_id, trip_number, trip_order, scheduled_start_time, scheduled_end_time, organization_id)
         VALUES ('t1-'||$1, 'tg-'||$1, $1, 'R-'||$1, 1, 1, '00:05', '00:10', 'org-'||$1),
                ('t2-'||$1, 'tg-'||$1, $1, 'R-'||$1, 2, 2, '23:50', '23:55', 'org-'||$1)",
    ] {
        exec(pool, sql, gtfs).await;
    }
}

async fn seed_run(
    pool: &PgPool,
    gtfs: &str,
    run: &str,
    date: NaiveDate,
    vehicle: Option<&str>,
    driver: Option<&str>,
    service_type: &str,
) {
    sqlx::query(
        "INSERT INTO duty_groups (id, waybill_no, trip_group_id, gtfs_id, operation_date, depot_id, vehicle_number,
                                  service_type_id, driver_token_number, driver_name, window_start_at, window_end_at, organization_id)
         VALUES ($2, $2, 'tg-'||$1, $1, $3, 'dep-'||$1, $4, $5, $6, $6,
                 ($3 + time '00:05') AT TIME ZONE 'Asia/Kolkata', ($3 + time '23:55') AT TIME ZONE 'Asia/Kolkata', 'org-'||$1)",
    )
    .bind(gtfs)
    .bind(run)
    .bind(date)
    .bind(vehicle)
    .bind(service_type)
    .bind(driver)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO duties (id, duty_group_id, trip_id, gtfs_id, route_id, is_bookable, trip_number, trip_order,
                             scheduled_start_at, scheduled_end_at, driver_token_number, organization_id)
         SELECT $2||'-'||t.trip_number, $2, t.id, $1, t.route_id, t.is_bookable, t.trip_number, t.trip_order,
                ($3 + t.scheduled_start_time) AT TIME ZONE 'Asia/Kolkata',
                ($3 + t.scheduled_end_time) AT TIME ZONE 'Asia/Kolkata', $4, 'org-'||$1
         FROM trips t WHERE t.trip_group_id = 'tg-'||$1",
    )
    .bind(gtfs)
    .bind(run)
    .bind(date)
    .bind(driver)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_v1_waybill(pool: &PgPool, gtfs: &str, waybill_no: &str, status: &str) {
    sqlx::query(
        "INSERT INTO waybills_internal (waybill_id, waybill_no, vehicle_no, service_type, schedule_no, status,
                                        no_of_device, schedule_id, entity_id, organization_id, gtfs_id, updated_at)
         VALUES ($2, $2, 'VR1', 'Ordinary', 'V1-SCHED', $3, 1, 'sched-'||$1, 'dep-'||$1, 'org-'||$1, $1, now())",
    )
    .bind(gtfs)
    .bind(waybill_no)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

fn reader(pool: &PgPool, gtfs: &str, config: ReaderConfig) -> DBVehicleReaderInternal {
    DBVehicleReaderInternal::new(
        pool.clone(),
        Arc::new(config),
        RouteCatalog::from_names(gtfs, &[(&format!("R-{gtfs}"), Some("21G"), None)]),
    )
}

fn with_cutoff(pool: &PgPool, gtfs: &str, date: NaiveDate) -> DBVehicleReaderInternal {
    reader(
        pool,
        gtfs,
        ReaderConfig::from_cutoffs(&[V1V2Cutoff {
            gtfs_id: gtfs.to_string(),
            cutoff_date: date.format("%Y-%m-%d").to_string(),
        }])
        .unwrap(),
    )
}

async fn waybill(r: &DBVehicleReaderInternal, gtfs: &str, vehicle: &str) -> Option<String> {
    r.get_vehicle_data(vehicle, gtfs, None)
        .await
        .unwrap()
        .waybill_no
}

async fn service_type(r: &DBVehicleReaderInternal, gtfs: &str) -> Option<String> {
    r.get_vehicle_service_type("VR1", gtfs).await.unwrap()
}

async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("TRANSIT_V2_TEST_DATABASE_URL").ok()?;
    assert!(
        url.contains("localhost") || url.contains("127.0.0.1"),
        "refusing a non-local database"
    );
    Some(
        PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn transit_v2_reads() {
    let Some(pool) = test_pool().await else {
        eprintln!("TRANSIT_V2_TEST_DATABASE_URL not set; skipping");
        return;
    };
    let gtfs = format!("tv2_read_{}", std::process::id());
    cleanup(&pool, &gtfs).await;
    seed_masters(&pool, &gtfs).await;
    let today = today_ist();
    let run = format!("WB-{gtfs}");
    let busless = format!("WB2-{gtfs}");
    let route = format!("R-{gtfs}");
    let st = format!("st-{gtfs}");
    seed_run(&pool, &gtfs, &run, today, Some("VR1"), Some("DR1"), &st).await;
    seed_run(
        &pool,
        &gtfs,
        &busless,
        today + Duration::days(1),
        None,
        None,
        "planned-st",
    )
    .await;

    let both = reader(&pool, &gtfs, ReaderConfig::default());
    assert!(both.is_vehicle_in_internal("VR1", &gtfs).await);

    let vd = both.get_vehicle_data("VR1", &gtfs, None).await.unwrap();
    assert_eq!(vd.waybill_no.as_deref(), Some(run.as_str()));
    assert_eq!(
        vd.schedule_no.as_deref(),
        Some("RDONE_MORNING_1205AM_NORMAL")
    );
    assert_eq!(vd.service_type.as_deref(), Some("Deluxe"));
    assert_eq!(vd.depot.as_deref(), Some("Depot One"));
    assert_eq!(vd.trip_number, Some(1));
    assert_eq!(vd.route_number.as_deref(), Some("21G"));
    assert_eq!(vd.driver_code.as_deref(), Some("DR1"));
    assert_eq!(vd.db_start_time.as_deref(), Some("00:05"));
    assert_eq!(vd.duty_trip_id, Some(format!("{run}-1")));
    let remaining: Vec<Option<String>> = vd
        .remaining_trip_details
        .as_ref()
        .unwrap()
        .iter()
        .map(|t| t.duty_trip_id.clone())
        .collect();
    assert_eq!(remaining, vec![Some(format!("{run}-2"))]);
    let all: Vec<Option<i32>> = vd
        .schedule_details
        .as_ref()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .iter()
        .map(|t| t.trip_number)
        .collect();
    assert_eq!(all, vec![Some(1), Some(2)]);

    let by_route = both
        .get_waybills_by_route_id(&route, &gtfs, None, None)
        .await
        .unwrap();
    assert_eq!(by_route.iter().filter(|r| r.waybill_no == run).count(), 2);
    assert!(by_route.iter().all(|r| r.waybill_no != busless));
    let tomorrow = (today + Duration::days(1)).format("%Y-%m-%d").to_string();
    let ahead = both
        .get_waybills_by_route_id(&route, &gtfs, None, Some(&tomorrow))
        .await
        .unwrap();
    let upcoming: Vec<&str> = ahead
        .iter()
        .filter(|r| r.waybill_no == busless)
        .map(|r| r.status.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(upcoming, vec!["upcoming", "upcoming"]);
    assert!(ahead
        .iter()
        .filter(|r| r.waybill_no == run)
        .all(|r| r.status.as_deref() == Some("online")));
    assert!(both
        .get_waybills_by_route_id(&route, &gtfs, Some("OTHER"), None)
        .await
        .unwrap()
        .iter()
        .all(|r| r.waybill_no != run));

    let one = both
        .get_waybill_by_waybill_and_trip(&run, 2, &gtfs)
        .await
        .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].trip_number, Some(2));
    assert_eq!(one[0].db_start_time.as_deref(), Some("23:50"));
    assert_eq!(one[0].duty_trip_id, Some(format!("{run}-2")));

    let meta = both.get_waybill_metadata(&gtfs, &run, None).await.unwrap();
    assert_eq!(meta.vehicle_no, "VR1");
    assert_eq!(meta.driver_id.as_deref(), Some("DR1"));
    assert_eq!(meta.driver_name.as_deref(), Some("Ravi K"));
    assert_eq!(meta.service_type, "Deluxe");
    exec(
        &pool,
        "UPDATE duties SET driver_token_number = 'DR2' WHERE gtfs_id = $1 AND trip_number = 2",
        &gtfs,
    )
    .await;
    let meta2 = both
        .get_waybill_metadata(&gtfs, &run, Some(2))
        .await
        .unwrap();
    assert_eq!(meta2.driver_id.as_deref(), Some("DR2"));
    assert_eq!(meta2.duty_trip_id, Some(format!("{run}-2")));
    let meta1 = both
        .get_waybill_metadata(&gtfs, &run, Some(1))
        .await
        .unwrap();
    assert_eq!(meta1.driver_id.as_deref(), Some("DR1"));

    assert!(waybill(&both, &gtfs, "NOPE").await.is_none());

    let v2_only = with_cutoff(&pool, &gtfs, today);
    assert_eq!(waybill(&v2_only, &gtfs, "VR1").await, Some(run.clone()));
    assert_eq!(
        service_type(&v2_only, &gtfs).await.as_deref(),
        Some("Deluxe")
    );
    assert_eq!(
        v2_only
            .get_waybill_metadata(&gtfs, &run, None)
            .await
            .unwrap()
            .vehicle_no,
        "VR1"
    );

    seed_v1_waybill(&pool, &gtfs, &format!("OLD-{gtfs}"), "closed").await;
    assert_eq!(waybill(&both, &gtfs, "VR1").await, Some(run.clone()));
    assert_eq!(service_type(&both, &gtfs).await.as_deref(), Some("Deluxe"));

    let online = format!("ON-{gtfs}");
    seed_v1_waybill(&pool, &gtfs, &online, "online").await;
    assert_eq!(waybill(&both, &gtfs, "VR1").await, Some(online.clone()));
    assert_eq!(
        service_type(&both, &gtfs).await.as_deref(),
        Some("Ordinary")
    );

    let before = with_cutoff(&pool, &gtfs, today + Duration::days(1));
    assert_eq!(waybill(&before, &gtfs, "VR1").await, Some(online.clone()));
    assert_eq!(waybill(&v2_only, &gtfs, "VR1").await, Some(run.clone()));
    assert_eq!(
        service_type(&v2_only, &gtfs).await.as_deref(),
        Some("Deluxe")
    );

    cleanup(&pool, &gtfs).await;
}
