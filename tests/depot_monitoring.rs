//! Depot-day reads against the MTC replica's table shapes (bigint ids, no gtfs_id), built as
//! temporary tables. Runs only when `DEPOT_MONITORING_TEST_DATABASE_URL` points at a local database.

use chrono::NaiveDate;
use gtfs_routes_service::services::depot_monitoring::{
    read_crew, read_overview, read_routes, read_waybills, DepotDayRequest,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn local_pool() -> Option<PgPool> {
    let Ok(url) = std::env::var("DEPOT_MONITORING_TEST_DATABASE_URL") else {
        eprintln!("DEPOT_MONITORING_TEST_DATABASE_URL not set; skipping");
        return None;
    };
    assert!(
        url.contains("@127.0.0.1") || url.contains("@localhost"),
        "the depot monitoring tests only run against a local database"
    );
    // One connection: temporary tables live in the session that created them.
    Some(
        PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap(),
    )
}

const FIXTURE: &[&str] = &[
    "CREATE TEMP TABLE vehicles (vehicle_id bigint, fleet_no text, vehicle_no text, status text, entity_id bigint, deleted boolean)",
    "CREATE TEMP TABLE waybills (waybill_id bigint, waybill_no text, entity_id bigint, duty_date varchar(20), vehicle_no text, driver_token_no text, conductor_token_no text, schedule_no text, schedule_trip_id bigint, is_flexi boolean, status text, updated_at timestamp, deleted boolean)",
    "CREATE TEMP TABLE bus_schedule_trip_detail (schedule_trip_detail_id bigint, schedule_trip_id bigint, route_number_id bigint, trip_number int, trip_order int, trip_type text, start_time text, end_time text, is_active_trip boolean, deleted boolean, trip_start_time text, trip_end_time text, distance real DEFAULT 14.5)",
    "CREATE TEMP TABLE bus_schedule_trip_flexi (schedule_trip_flexi_id bigint, waybill_id bigint, route_number_id bigint, trip_number int, trip_order int, trip_type text, start_time text, end_time text, is_active_trip boolean, deleted boolean, trip_start_time text, trip_end_time text, distance real DEFAULT 14.5)",
    "CREATE TEMP TABLE bus_route (route_id bigint, route_number text, route_name text, route_direction text, deleted boolean)",
    "CREATE TEMP TABLE bus_schedule (schedule_id bigint, entity_id bigint, effective_from timestamp, effective_till timestamp, deleted boolean)",
    "CREATE TEMP TABLE bus_schedule_trip (schedule_trip_id bigint, schedule_id bigint, effective_start_date timestamp, effective_end_date timestamp, deleted boolean)",
    "CREATE TEMP TABLE employees (emp_id bigint, token_no text, designation_id bigint, status text, entity_id bigint, deleted boolean)",
    "CREATE TEMP TABLE designations (designation_id bigint, designation_name text, deleted boolean)",
    // Depot 4: J0773 and I0510 registered here; J1935 belongs to depot 37.
    "INSERT INTO vehicles VALUES (67,'J0773','TN01AN1524','Active',4,false), (1,'I0510','TN01N4568','InActive',4,false), (500,'J1935','TN01AN9999','Active',37,false), (2,'J0001','TN01X1','Active',4,true)",
    // Two waybills on the day; a deleted one, another depot's and the previous day's are out of scope.
    "INSERT INTO waybills VALUES \
        (3576222,'T1792011920055143784',4,'2026-10-15','J1935','D50965','C57276','S-99-N-AM',313,false,'Closed',now(),false), \
        (3576298,'T1792016922929931832',4,'2026-10-15','J0773','D53566','','S-102X-A-AS',59,true,'Closed',now(),false), \
        (10,'DELETED',4,'2026-10-15','J0773','D1','C1','S-1',313,false,'Closed',now(),true), \
        (11,'OTHER',37,'2026-10-15','J1935','D1','C1','S-1',313,false,'Closed',now(),false), \
        (12,'YESTERDAY',4,'2026-10-14','J0773','D1','C1','S-1',313,false,'Closed',now(),false)",
    // Schedule trip 313: 6:00 recorded on the day; 8:00 recorded the day before (a different run).
    "INSERT INTO bus_schedule_trip_detail VALUES \
        (1,313,99001,1,1,'regular-trip','6:00','7:10',false,false,'1792024440000',''), \
        (4,313,99001,2,2,'dead-trip','8:00','9:10',false,false,'1791938040000','06:04'), \
        (5,313,99001,3,3,'regular-trip','9:00','9:30',false,true,NULL,NULL), \
        (3,777,99001,1,1,'regular-trip','0:00','0:30',false,false,'',''), \
        (6,314,88001,1,1,'regular-trip','7:00','8:00',false,false,NULL,NULL), \
        (7,315,77001,1,1,'regular-trip','7:00','8:00',false,false,NULL,NULL)",
    "INSERT INTO bus_schedule_trip_flexi VALUES (2,3576298,102001,1,1,'regular-trip','8:00','9:00',false,false,' ',NULL)",
    "INSERT INTO bus_route VALUES (99001,'99','Adyar - Kelambakkam','UP',false), (88001,'88','Expired','UP',false)",
    // Schedule 10 is effective; 11 ended before the day; 12 is another depot's.
    "INSERT INTO bus_schedule VALUES (10,4,NULL,NULL,false), (11,4,NULL,'2026-10-01',false), (12,37,NULL,NULL,false)",
    "INSERT INTO bus_schedule_trip VALUES (313,10,NULL,NULL,false), (314,11,NULL,NULL,false), (315,12,NULL,NULL,false)",
    "INSERT INTO employees VALUES (1,'D50965',1,'Active',4,false), (2,'C57276',2,'Active',4,false), (3,'X1',4,'Active',4,false), (4,'C9',2,'Active',37,false), (5,'D9',1,'Active',4,true)",
    "INSERT INTO designations VALUES (1,'Driver',false), (2,'Conductor',false), (4,'Admin Manager',false)",
];

fn day(page: i64, size: i64, q: &str) -> DepotDayRequest {
    let date = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
    DepotDayRequest::new("4", date, Some(page), Some(size), Some(q)).unwrap()
}

#[tokio::test]
async fn depot_day_reads_the_replica_shape() {
    let Some(pool) = local_pool().await else {
        return;
    };
    for sql in FIXTURE {
        sqlx::query(sql)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    // Waybills: both shifts, regular trips by schedule trip, flexi trips by waybill.
    let wb = read_waybills(&pool, &day(1, 50, "")).await.unwrap();
    assert_eq!(wb.total, 2);
    let regular = &wb.rows[0].trips.0;
    assert_eq!(regular.len(), 2); // Deleted trip row left out.
    assert_eq!(regular[0].route_number.as_deref(), Some("99"));
    assert_eq!(regular[0].planned_km, Some(14.5));
    assert_eq!(regular[0].actual_start.as_deref(), Some("1792024440000"));
    assert!(regular[0].actual_end.is_none()); // Blank means not recorded.
                                              // Previous day's epoch and clock-only text cannot belong to this run.
    assert!(regular[1].actual_start.is_none() && regular[1].actual_end.is_none());
    assert_eq!(regular[1].trip_type.as_deref(), Some("dead-trip"));
    assert!(regular[0].recorded_completed.is_none()); // The replica has no completion flag.
    assert!(wb.rows[1].conductor_token.is_none()); // '' in the source.
    assert_eq!(wb.rows[1].trips.0[0].trip_id, "2");
    assert!(wb.rows[1].trips.0[0].route_number.is_none()); // Missing route master kept.
    let second = read_waybills(&pool, &day(2, 1, "")).await.unwrap();
    assert_eq!(
        (second.total, second.rows[0].waybill_id.as_str()),
        (2, "3576298")
    );
    assert_eq!(
        read_waybills(&pool, &day(1, 50, "j0773"))
            .await
            .unwrap()
            .total,
        1
    );
    assert_eq!(
        read_waybills(&pool, &day(1, 50, "%")).await.unwrap().total,
        0
    );
    let json = serde_json::to_value(&wb).unwrap();
    assert!(json["rows"][0]["trips"].is_array());
    assert!(json.get("dataThrough").is_none());

    // Routes: scheduled 99001 plus waybill-only 102001; expired (88001) and other depot (77001) excluded.
    let routes = read_routes(&pool, &day(1, 50, "")).await.unwrap();
    assert_eq!(routes.total, 2);
    let r99 = routes.rows.iter().find(|r| r.route_id == "99001").unwrap();
    assert_eq!(
        (
            r99.scheduled_schedules,
            r99.allocated_waybills,
            r99.allocated_vehicles
        ),
        (1, 1, 1)
    );
    let flexi = routes.rows.iter().find(|r| r.route_id == "102001").unwrap();
    assert_eq!(
        (flexi.scheduled_schedules, flexi.allocated_waybills),
        (0, 1)
    );
    assert_eq!(
        read_routes(&pool, &day(1, 50, "kelambakkam"))
            .await
            .unwrap()
            .total,
        1
    );
    let paged = read_routes(&pool, &day(2, 1, "")).await.unwrap();
    assert_eq!((paged.total, paged.rows.len()), (2, 1));

    // Crew: the depot roster only, with the day's assignments per token.
    let crew = read_crew(&pool, &day(1, 50, "")).await.unwrap();
    assert_eq!(crew.total, 3);
    let driver = crew
        .rows
        .iter()
        .find(|c| c.token.as_deref() == Some("D50965"))
        .unwrap();
    assert_eq!(
        (driver.assigned_waybills, driver.assigned_roles.clone()),
        (1, vec!["driver".to_string()])
    );
    let admin = crew
        .rows
        .iter()
        .find(|c| c.token.as_deref() == Some("X1"))
        .unwrap();
    assert!(admin.assigned_roles.is_empty());
    assert_eq!(
        read_crew(&pool, &day(1, 50, "driver")).await.unwrap().total,
        1
    );

    // Overview: whole day, page and search ignored.
    let o = read_overview(&pool, &day(1, 1, "ignored")).await.unwrap();
    let c = &o.counts;
    // J0773 matches by fleet number; J1935 is registered under depot 37.
    assert_eq!(
        (
            c.registered_vehicles,
            c.waybills,
            c.matched_vehicles,
            c.other_depot_vehicles,
            c.unmatched_vehicle_identifiers
        ),
        (2, 2, 1, 1, 0)
    );
    // Roster D50965, C57276, X1; assigned D50965, C57276, D53566 (not on the roster).
    assert_eq!(
        (
            c.rostered_crew,
            c.assigned_crew_tokens,
            c.unrostered_crew_tokens
        ),
        (3, 3, 1)
    );
    assert_eq!(c.waybills_by_status.0, serde_json::json!({"Closed": 2}));
    assert_eq!(
        (o.routes.scheduled_routes, o.routes.allocated_routes),
        (1, 2)
    );
    // Two regular trips and one flexi; only the 6:00 trip was started on the day. Past day: no overdue.
    assert_eq!(
        (
            o.trips.planned_trips,
            o.trips.started_trips,
            o.trips.completed_trips,
            o.trips.overdue_unstarted_trips
        ),
        (3, 1, 0, None)
    );
    assert_eq!(
        o.trips_by_hour
            .iter()
            .map(|h| (h.hour, h.planned, h.started))
            .collect::<Vec<_>>(),
        vec![(6, 1, 1), (8, 2, 0)]
    );
    let json = serde_json::to_value(&o).unwrap();
    assert_eq!(
        (
            json["registeredVehicles"].as_i64(),
            json["plannedTrips"].as_i64()
        ),
        (Some(2), Some(3))
    );

    // Today: a 0:00 trip with no recorded start is overdue at any later time.
    let today = (chrono::Utc::now() + chrono::Duration::minutes(330)).date_naive();
    sqlx::query("INSERT INTO waybills VALUES (9,'TODAY',4,$1,'J0773','D50965','C57276','S-1',777,false,'Online',now(),false)")
        .bind(today.format("%Y-%m-%d").to_string())
        .execute(&pool)
        .await
        .unwrap();
    let live = read_overview(
        &pool,
        &DepotDayRequest::new("4", today, None, None, None).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        (live.trips.planned_trips, live.trips.overdue_unstarted_trips),
        (1, Some(1))
    );
    pool.close().await;
}
