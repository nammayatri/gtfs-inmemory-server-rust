//! transitV2 end to end against a real Postgres (services/operator_v2.rs,
//! services/fleet_operator_v2.rs):
//!
//! - trip day offsets across midnight;
//! - a run gets one duty per trip, crew copied from the run, times in IST;
//! - per-trip driver overlap is rejected, gaps are usable;
//! - run-level crew change skips individually swapped trips; trip-level change works;
//! - tripAction start / end / rollback / skip / cancel / uncancel and strict order;
//! - currentOperation buckets;
//! - repeat generation is idempotent, a bus clash creates the run unassigned and logs it,
//!   finishing a run creates the run 7 days later;
//! - x-operator-id scoping.
//!
//! Runs only when `TRANSIT_V2_TEST_DATABASE_URL` is set (local hosts only) and the transitV2
//! tables exist there (relations/internal/{trip_groups,trips,duty_repeats,duty_groups,duties,
//! duty_event_logs}). Uses a throwaway gtfs_id and deletes its rows afterwards.

use chrono::{Duration, NaiveDate};
use gtfs_routes_service::services::fleet_operator_v2::FleetOperatorV2Service;
use gtfs_routes_service::services::operator_v2::*;
use gtfs_routes_service::tools::error::AppError;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

fn caller(op: Option<&str>) -> Caller {
    Caller {
        operator_id: op.map(str::to_string),
        actor_person_id: Some("person-1".to_string()),
    }
}

fn trip(n: i32, start: &str, end: &str, route: &str) -> TripInput {
    TripInput {
        id: None,
        route_id: route.to_string(),
        trip_number: n,
        trip_order: n,
        scheduled_start_time: start.to_string(),
        scheduled_end_time: end.to_string(),
        is_bookable: None,
    }
}

fn group_req(zone: &str, shift: &str, first_departure: &str) -> UpsertTripGroupReq {
    UpsertTripGroupReq {
        id: None,
        code: None,
        description: None,
        zone: zone.to_string(),
        shift: shift.to_string(),
        first_departure: first_departure.to_string(),
        trip_type: "NORMAL".to_string(),
        depot_id: Some("DEPOT1".to_string()),
        service_type_id: None,
    }
}

fn run_req(
    group: &str,
    date: NaiveDate,
    vehicle: Option<&str>,
    driver: Option<&str>,
) -> CreateDutyGroupReq {
    CreateDutyGroupReq {
        trip_group_id: group.to_string(),
        operation_date: date,
        vehicle_number: vehicle.map(str::to_string),
        driver_token_number: driver.map(str::to_string),
        driver_name: driver.map(|d| format!("name-{}", d)),
        conductor_token_number: None,
        conductor_name: None,
    }
}

fn action(vehicle: &str, a: TripActionV2, n: Option<i32>) -> TripActionV2Req {
    TripActionV2Req {
        anchor: AnchorReq {
            vehicle_number: Some(vehicle.to_string()),
            ..Default::default()
        },
        action: a,
        trip_number: n,
        timestamp: None,
        reason: None,
    }
}

async fn cleanup(pool: &PgPool, gtfs: &str) {
    for t in [
        "duty_event_logs",
        "duties",
        "duty_groups",
        "duty_repeats",
        "trips",
        "trip_groups",
    ] {
        sqlx::query(&format!("DELETE FROM {} WHERE gtfs_id = $1", t))
            .bind(gtfs)
            .execute(pool)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn transit_v2_flow() {
    let Ok(url) = std::env::var("TRANSIT_V2_TEST_DATABASE_URL") else {
        eprintln!("TRANSIT_V2_TEST_DATABASE_URL not set; skipping");
        return;
    };
    assert!(
        url.contains("localhost")
            || url.contains("127.0.0.1")
            || url.contains("/tmp")
            || url.contains("%2Ftmp"),
        "refusing a non-local database"
    );
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&url)
        .await
        .unwrap();
    let gtfs = format!("tv2_test_{}", std::process::id());
    cleanup(&pool, &gtfs).await;

    let ops = OperatorV2Service::new(Some(pool.clone()));
    let fleet = FleetOperatorV2Service::new(ops.clone());
    let admin = caller(None);
    let op_a = caller(Some("opA"));
    let op_b = caller(Some("opB"));
    let today = today_ist();

    // ── Offsets across midnight ─────────────────────────────────────────────
    let night = ops
        .upsert_trip_group(&gtfs, &op_a, group_req("Night One", "NIGHT", "22:30"))
        .await
        .unwrap();
    assert_eq!(night.operator_id.as_deref(), Some("opA"));
    assert_eq!(night.code, "NIGHTONE_NIGHT_1030PM_NORMAL");
    // a sent code must match the parts
    let mut wrong = group_req("Night One", "NIGHT", "22:30");
    wrong.code = Some("NIGHTONE_NIGHT_1030PM_FEEDER".into());
    assert!(matches!(
        ops.upsert_trip_group(&gtfs, &op_a, wrong)
            .await
            .unwrap_err(),
        AppError::BadRequest(_)
    ));
    // trips must start at the group's first departure
    let bad_first = ops
        .upsert_trips(
            &gtfs,
            &op_a,
            &night.id,
            UpsertTripsReq {
                first_trip_day_offset: None,
                trips: vec![trip(1, "22:45", "23:30", "R1")],
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(bad_first, AppError::BadRequest(_)),
        "got {:?}",
        bad_first
    );
    let trips = ops
        .upsert_trips(
            &gtfs,
            &op_a,
            &night.id,
            UpsertTripsReq {
                first_trip_day_offset: None,
                trips: vec![
                    trip(1, "22:30", "23:30", "R1"),
                    trip(2, "23:40", "00:30", "R2"),
                    trip(3, "00:45", "01:40", "R3"),
                ],
            },
        )
        .await
        .unwrap();
    let offsets: Vec<(i16, i16)> = trips
        .iter()
        .map(|t| (t.scheduled_start_day_offset, t.scheduled_end_day_offset))
        .collect();
    assert_eq!(offsets, vec![(0, 0), (0, 1), (1, 1)]);
    assert!(trips
        .iter()
        .all(|t| t.operator_id.as_deref() == Some("opA")));

    // ── Manual run ──────────────────────────────────────────────────────────
    let run = ops
        .create_duty_group(
            &gtfs,
            &op_a,
            run_req(&night.id, today, Some("V1"), Some("D1")),
        )
        .await
        .unwrap();
    assert_eq!(run.duties.len(), 3);
    assert!(!run.duty_group.waybill_no.contains('-'));
    assert!(run
        .duties
        .iter()
        .all(|d| d.driver_token_number.as_deref() == Some("D1")));
    assert!(run
        .duties
        .iter()
        .all(|d| d.operator_id.as_deref() == Some("opA")));
    // trip 3 starts 00:45 IST on the next day = 19:15 UTC on `today`
    let t3 = run.duties.iter().find(|d| d.trip_number == 3).unwrap();
    assert_eq!(
        t3.scheduled_start_at,
        to_instant(today, 1, parse_hhmm("00:45").unwrap())
    );
    assert_eq!(
        run.duty_group.window_end_at,
        to_instant(today, 1, parse_hhmm("01:40").unwrap())
    );

    // ── Per-trip driver overlap ─────────────────────────────────────────────
    let clash = ops
        .upsert_trip_group(&gtfs, &op_a, group_req("CLASHZONE", "NIGHT", "23:00"))
        .await
        .unwrap();
    ops.upsert_trips(
        &gtfs,
        &op_a,
        &clash.id,
        UpsertTripsReq {
            first_trip_day_offset: None,
            trips: vec![trip(1, "23:00", "23:50", "R9")],
        },
    )
    .await
    .unwrap();
    let err = ops
        .create_duty_group(&gtfs, &op_a, run_req(&clash.id, today, None, Some("D1")))
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Conflict(_)), "got {:?}", err);

    let gap = ops
        .upsert_trip_group(&gtfs, &op_a, group_req("GAPZONE", "EVENING", "18:00"))
        .await
        .unwrap();
    ops.upsert_trips(
        &gtfs,
        &op_a,
        &gap.id,
        UpsertTripsReq {
            first_trip_day_offset: None,
            trips: vec![trip(1, "18:00", "19:00", "R8")],
        },
    )
    .await
    .unwrap();
    ops.create_duty_group(&gtfs, &op_a, run_req(&gap.id, today, None, Some("D1")))
        .await
        .expect("non-overlapping trip for the same driver is fine");

    // ── Crew: trip-level then run-level ─────────────────────────────────────
    let t3_id = t3.id.clone();
    let swapped = ops
        .update_trip_crew(
            &gtfs,
            &op_a,
            &t3_id,
            UpdateCrewReq {
                driver_token_number: Some("D2".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(swapped.driver_token_number.as_deref(), Some("D2"));
    let after = ops
        .update_run_crew(
            &gtfs,
            &op_a,
            &run.duty_group.id,
            UpdateCrewReq {
                driver_token_number: Some("D3".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let drivers: Vec<Option<&str>> = after
        .duties
        .iter()
        .map(|d| d.driver_token_number.as_deref())
        .collect();
    assert_eq!(drivers, vec![Some("D3"), Some("D3"), Some("D2")]);
    assert_eq!(after.duty_group.driver_token_number.as_deref(), Some("D3"));

    // ── Operator scoping ────────────────────────────────────────────────────
    assert!(matches!(
        ops.get_duty_group_detail(&gtfs, &op_b, &run.duty_group.id)
            .await
            .unwrap_err(),
        AppError::NotFound(_)
    ));
    assert!(matches!(
        ops.set_run_active(&gtfs, &op_b, &run.duty_group.id, false)
            .await
            .unwrap_err(),
        AppError::NotFound(_) | AppError::Forbidden(_)
    ));
    assert_eq!(
        ops.list_trip_groups(&gtfs, &op_b, TripGroupListQuery::default())
            .await
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        ops.list_trip_groups(
            &gtfs,
            &admin,
            TripGroupListQuery {
                code: Some("ghtone".into()),
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .total,
        1
    );

    // ── Trip actions (anchor: vehicle V1) ───────────────────────────────────
    let r = fleet
        .trip_action(&gtfs, &op_a, action("V1", TripActionV2::Start, None))
        .await
        .unwrap();
    assert_eq!(r.active.as_ref().map(|a| a.trip_number), Some(1));
    assert_eq!(r.upcoming.len(), 2);
    assert!(fleet
        .trip_action(&gtfs, &op_a, action("V1", TripActionV2::Start, None))
        .await
        .is_err());
    let cur = fleet
        .current_operation(
            &gtfs,
            &op_a,
            &AnchorReq {
                driver_token: Some("D3".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(cur.active.as_ref().map(|a| a.trip_number), Some(1));
    assert_eq!(cur.upcoming.len(), 2);
    assert_eq!(
        cur.active.as_ref().unwrap().trip_id,
        format!("{}-1", cur.waybill_no)
    );
    let active = fleet
        .active_trip(
            &gtfs,
            &op_a,
            &AnchorReq {
                vehicle_number: Some("V1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(active.route_id, "R1");

    fleet
        .trip_action(&gtfs, &op_a, action("V1", TripActionV2::End, Some(1)))
        .await
        .unwrap();
    // rollback re-opens trip 1
    let r = fleet
        .trip_action(&gtfs, &op_a, action("V1", TripActionV2::Rollback, None))
        .await
        .unwrap();
    assert_eq!(r.active.as_ref().map(|a| a.trip_number), Some(1));
    fleet
        .trip_action(&gtfs, &op_a, action("V1", TripActionV2::End, None))
        .await
        .unwrap();
    // strict order: trip 3 can't start before 2
    assert!(fleet
        .trip_action(&gtfs, &op_a, action("V1", TripActionV2::Start, Some(3)))
        .await
        .is_err());
    fleet
        .trip_action(&gtfs, &op_a, action("V1", TripActionV2::Skip, Some(2)))
        .await
        .unwrap();
    let mut cancel = action("V1", TripActionV2::Cancel, Some(3));
    cancel.reason = Some("BREAKDOWN".into());
    fleet
        .trip_action(&gtfs, &op_a, cancel.clone())
        .await
        .unwrap();
    // all done -> the vehicle has no live run
    assert!(matches!(
        fleet
            .current_operation(
                &gtfs,
                &op_a,
                &AnchorReq {
                    vehicle_number: Some("V1".into()),
                    ..Default::default()
                }
            )
            .await
            .unwrap_err(),
        AppError::NotFound(_)
    ));
    // uncancel via the run id (dashboard)
    let mut uncancel = action("V1", TripActionV2::Uncancel, Some(3));
    uncancel.anchor = AnchorReq {
        duty_group_id: Some(run.duty_group.id.clone()),
        ..Default::default()
    };
    let r = fleet.trip_action(&gtfs, &op_a, uncancel).await.unwrap();
    assert!(r.active.is_none());
    assert_eq!(
        r.upcoming
            .first()
            .map(|t| (t.trip_number, t.status.as_str())),
        Some((3, "upcoming"))
    );
    let logs = ops
        .get_run_logs(&gtfs, &op_a, &run.duty_group.id)
        .await
        .unwrap();
    let kinds: Vec<&str> = logs.iter().map(|l| l.event_type.as_str()).collect();
    assert!(kinds.contains(&"TRIP_STATUS_CHANGE") && kinds.contains(&"CREW_CHANGE"));
    // start, end, rollback, end, skip, cancel, uncancel = 7 status changes
    assert_eq!(
        kinds.iter().filter(|k| **k == "TRIP_STATUS_CHANGE").count(),
        7
    );
    assert!(logs
        .iter()
        .any(|l| l.reason.as_deref() == Some("BREAKDOWN")));
    let detail = ops
        .get_duty_group_detail(&gtfs, &op_a, &run.duty_group.id)
        .await
        .unwrap();
    let statuses: Vec<&str> = detail.duties.iter().map(|d| d.status.as_str()).collect();
    assert_eq!(statuses, vec!["completed", "skipped", "upcoming"]);
    assert_eq!(detail.duties[1].skip_reason.as_deref(), Some("OTHER"));
    assert!(detail.duties[2].cancel_reason.is_none());

    // ── Repeat generation ───────────────────────────────────────────────────
    // ends after midnight, so today's run is never already over whenever the test runs
    let day = ops
        .upsert_trip_group(&gtfs, &op_a, group_req("DAYONE", "NIGHT", "23:58"))
        .await
        .unwrap();
    ops.upsert_trips(
        &gtfs,
        &op_a,
        &day.id,
        UpsertTripsReq {
            first_trip_day_offset: None,
            trips: vec![trip(1, "23:58", "00:30", "R5")],
        },
    )
    .await
    .unwrap();
    let rule_req = |vehicle: &str, driver: &str| UpsertDutyRepeatReq {
        id: None,
        trip_group_id: day.id.clone(),
        repeat_status: None,
        recurrence_days: vec![1, 2, 3, 4, 5, 6, 7],
        effective_from: today,
        effective_till: None,
        vehicle_number: Some(vehicle.to_string()),
        driver_token_number: Some(driver.to_string()),
        driver_name: None,
        conductor_token_number: None,
        conductor_name: None,
        generate: None,
    };
    let saved = ops
        .upsert_duty_repeat(&gtfs, &op_a, rule_req("V9", "D9"))
        .await
        .unwrap();
    assert_eq!(
        saved
            .generated
            .iter()
            .filter(|e| e.verdict == "created")
            .count(),
        (LOOKAHEAD_DAYS + 1) as usize
    );
    let again = ops
        .generate(
            &gtfs,
            today,
            today + Duration::days(LOOKAHEAD_DAYS),
            None,
            Some("opA".into()),
            false,
            GenerationTrigger::Cron,
        )
        .await
        .unwrap();
    assert!(again.iter().all(|e| e.verdict == "exists"), "{:?}", again);

    // generate: false saves the rule without creating any duty group
    let mut quiet = rule_req("V40", "D40");
    quiet.generate = Some(false);
    let quiet_saved = ops.upsert_duty_repeat(&gtfs, &op_a, quiet).await.unwrap();
    assert!(quiet_saved.generated.is_empty());
    let quiet_runs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM duty_groups WHERE duty_repeat_id = $1")
            .bind(&quiet_saved.duty_repeat.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(quiet_runs, 0);
    ops.delete_duty_repeat(&gtfs, &op_a, &quiet_saved.duty_repeat.id)
        .await
        .unwrap();

    // a day whose trips have all ended is not generated
    let early = ops
        .upsert_trip_group(&gtfs, &op_a, group_req("PASTZONE", "MIDNIGHT", "00:01"))
        .await
        .unwrap();
    ops.upsert_trips(
        &gtfs,
        &op_a,
        &early.id,
        UpsertTripsReq {
            first_trip_day_offset: None,
            trips: vec![trip(1, "00:01", "00:02", "R6")],
        },
    )
    .await
    .unwrap();
    let mut past_rule = rule_req("V30", "D30");
    past_rule.trip_group_id = early.id.clone();
    let past = ops
        .upsert_duty_repeat(&gtfs, &op_a, past_rule)
        .await
        .unwrap();
    let today_entry = past
        .generated
        .iter()
        .find(|e| e.operation_date == today)
        .unwrap();
    assert_eq!(today_entry.verdict, "past", "{:?}", past.generated);
    assert!(past
        .generated
        .iter()
        .filter(|e| e.operation_date > today)
        .all(|e| e.verdict == "created"));

    // a manual run of the trip group already covers that day: the rule doesn't add another
    let manual_group = ops
        .upsert_trip_group(&gtfs, &op_a, group_req("MANUALZONE", "NIGHT", "23:57"))
        .await
        .unwrap();
    ops.upsert_trips(
        &gtfs,
        &op_a,
        &manual_group.id,
        UpsertTripsReq {
            first_trip_day_offset: None,
            trips: vec![trip(1, "23:57", "00:20", "R7")],
        },
    )
    .await
    .unwrap();
    let manual = ops
        .create_duty_group(
            &gtfs,
            &op_a,
            run_req(&manual_group.id, today, Some("V20"), Some("D20")),
        )
        .await
        .unwrap();
    let mut covered_rule = rule_req("V20", "D20");
    covered_rule.trip_group_id = manual_group.id.clone();
    let covered = ops
        .upsert_duty_repeat(&gtfs, &op_a, covered_rule)
        .await
        .unwrap();
    let today_entry = covered
        .generated
        .iter()
        .find(|e| e.operation_date == today)
        .unwrap();
    assert_eq!(today_entry.verdict, "covered", "{:?}", covered.generated);
    assert_eq!(
        today_entry.duty_group_id.as_deref(),
        Some(manual.duty_group.id.as_str())
    );
    assert!(covered
        .generated
        .iter()
        .filter(|e| e.operation_date > today)
        .all(|e| e.verdict == "created"));
    let manual_runs_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM duty_groups WHERE trip_group_id = $1 AND operation_date = $2 AND NOT deleted",
    )
    .bind(&manual_group.id)
    .bind(today)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(manual_runs_today, 1);

    // search: trip groups by a repeat config's default crew / bus; repeats and runs by code
    let by_driver = ops
        .list_trip_groups(
            &gtfs,
            &op_a,
            TripGroupListQuery {
                driver_token_number: Some("D20".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        by_driver
            .items
            .iter()
            .map(|g| g.trip_group.id.as_str())
            .collect::<Vec<_>>(),
        vec![manual_group.id.as_str()]
    );
    assert_eq!(
        (
            by_driver.items[0].trip_count,
            by_driver.items[0].repeat_count
        ),
        (1, 1)
    );
    // free-text partial search
    let tg_search = ops
        .list_trip_groups(
            &gtfs,
            &op_a,
            TripGroupListQuery {
                search: Some("d2".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        tg_search
            .items
            .iter()
            .any(|g| g.trip_group.id == manual_group.id),
        "partial driver token via repeat config"
    );
    let dg_search = ops
        .list_duty_groups(
            &gtfs,
            &op_a,
            DutyGroupListQuery {
                search: Some("nualzo".into()),
                operation_date: Some(today),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        dg_search
            .items
            .iter()
            .map(|r| r.duty_group.id.as_str())
            .collect::<Vec<_>>(),
        vec![manual.duty_group.id.as_str()]
    );
    let dg_by_crew = ops
        .list_duty_groups(
            &gtfs,
            &op_a,
            DutyGroupListQuery {
                search: Some("d2".into()),
                operation_date: Some(today),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(dg_by_crew
        .items
        .iter()
        .any(|r| r.duty_group.id == manual.duty_group.id));
    let rp_search = ops
        .list_duty_repeats(
            &gtfs,
            &op_a,
            DutyRepeatListQuery {
                search: Some("v2".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(rp_search.total >= 1);
    let by_vehicle = ops
        .list_trip_groups(
            &gtfs,
            &op_a,
            TripGroupListQuery {
                vehicle_number: Some("V30".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_vehicle.total, 1);
    let repeats_by_code = ops
        .list_duty_repeats(
            &gtfs,
            &op_a,
            DutyRepeatListQuery {
                code: Some("manualzone".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(repeats_by_code.total, 1);
    let repeats_by_vehicle = ops
        .list_duty_repeats(
            &gtfs,
            &op_a,
            DutyRepeatListQuery {
                vehicle_number: Some("V20".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(repeats_by_vehicle.total, 1);
    let runs_by_code = ops
        .list_duty_groups(
            &gtfs,
            &op_a,
            DutyGroupListQuery {
                code: Some("manualzone".into()),
                operation_date: Some(today),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        runs_by_code
            .items
            .iter()
            .map(|r| r.duty_group.id.as_str())
            .collect::<Vec<_>>(),
        vec![manual.duty_group.id.as_str()]
    );
    let runs_by_waybill = ops
        .list_duty_groups(
            &gtfs,
            &op_a,
            DutyGroupListQuery {
                code: Some(manual.duty_group.waybill_no.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(runs_by_waybill
        .items
        .iter()
        .any(|r| r.duty_group.id == manual.duty_group.id));

    // second rule, same bus at the same time -> created without the vehicle, failure logged
    let second = ops
        .upsert_duty_repeat(&gtfs, &op_a, rule_req("V9", "D10"))
        .await
        .unwrap();
    assert!(
        second
            .generated
            .iter()
            .all(|e| e.verdict == "created_partial"),
        "{:?}",
        second.generated
    );
    let failures = ops
        .list_generation_failures(
            &gtfs,
            &op_a,
            FailureListQuery {
                limit: None,
                offset: None,
                resolved: Some(false),
            },
        )
        .await
        .unwrap();
    assert!(failures.total >= 1);
    ops.resolve_generation_failure(&gtfs, &op_a, &failures.items[0].id)
        .await
        .unwrap();

    // run finish -> run for today + 7 is (re)created
    let rule_id = saved.duty_repeat.id.clone();
    let plus7 = today + Duration::days(7);
    let plus7_run: String = sqlx::query_scalar(
        "SELECT id FROM duty_groups WHERE duty_repeat_id = $1 AND operation_date = $2",
    )
    .bind(&rule_id)
    .bind(plus7)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM duties WHERE duty_group_id = $1")
        .bind(&plus7_run)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM duty_groups WHERE id = $1")
        .bind(&plus7_run)
        .execute(&pool)
        .await
        .unwrap();
    fleet
        .trip_action(&gtfs, &op_a, action("V9", TripActionV2::Start, None))
        .await
        .unwrap();
    fleet
        .trip_action(&gtfs, &op_a, action("V9", TripActionV2::End, None))
        .await
        .unwrap();
    let mut recreated = false;
    for _ in 0..50 {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM duty_groups WHERE duty_repeat_id = $1 AND operation_date = $2",
        )
        .bind(&rule_id)
        .bind(plus7)
        .fetch_one(&pool)
        .await
        .unwrap();
        if n == 1 {
            recreated = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(recreated, "run finish should create the run 7 days later");

    // a deactivated (cancelled) day is not regenerated
    let tomorrow_run: String = sqlx::query_scalar(
        "SELECT id FROM duty_groups WHERE duty_repeat_id = $1 AND operation_date = $2",
    )
    .bind(&rule_id)
    .bind(today + Duration::days(1))
    .fetch_one(&pool)
    .await
    .unwrap();
    ops.set_run_active(&gtfs, &op_a, &tomorrow_run, false)
        .await
        .unwrap();
    let regen = ops
        .generate(
            &gtfs,
            today + Duration::days(1),
            today + Duration::days(1),
            Some(vec![rule_id.clone()]),
            None,
            false,
            GenerationTrigger::Cron,
        )
        .await
        .unwrap();
    assert_eq!(regen[0].verdict, "exists");

    cleanup(&pool, &gtfs).await;
}

/// Existing read endpoints see transitV2 runs (db_vehicle_reader_internal.rs dual-read).
/// Also needs vehicles_internal, service_type_internal, entities_internal, employees_internal,
/// route_internal and the waybill tables in the test DB.
#[tokio::test]
async fn transit_v2_dual_read() {
    use gtfs_routes_service::services::db_vehicle_reader_internal::{
        DBVehicleReaderInternal, VehicleDataReaderInternal,
    };
    let Ok(url) = std::env::var("TRANSIT_V2_TEST_DATABASE_URL") else {
        eprintln!("TRANSIT_V2_TEST_DATABASE_URL not set; skipping");
        return;
    };
    assert!(
        url.contains("localhost") || url.contains("127.0.0.1"),
        "refusing a non-local database"
    );
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .unwrap();
    let gtfs = format!("tv2_read_{}", std::process::id());
    cleanup(&pool, &gtfs).await;
    let fixtures = [
        "INSERT INTO service_type_internal (service_type_id, service_type_name, gtfs_id) VALUES ('st-'||$1, 'Deluxe', $1)",
        "INSERT INTO entities_internal (entity_id, entity_name, entity_remark, organization_id, gtfs_id) VALUES ('dep-'||$1, 'Depot One', 'D1 remark', 'org', $1)",
        "INSERT INTO vehicles_internal (vehicle_id, fleet_no, vehicle_no, bus_service_type_id, entity_id, organization_id, gtfs_id) VALUES ('veh-'||$1, 'VR1', 'TN01', 'st-'||$1, 'dep-'||$1, 'org', $1)",
        "INSERT INTO employees_internal (emp_id, token_no, first_name, last_name, mobile_no, department_id, designation_id, entity_id, organization_id, gtfs_id) VALUES ('emp-'||$1, 'DR1', 'Ravi', 'K', '9999999999', 'x', 'x', 'x', 'x', $1)",
        "INSERT INTO route_internal (route_id, route_number, bus_service_type_id, end_point_id, start_point_id, gtfs_id) VALUES ('R-'||$1, '21G', 'x', 'x', 'x', $1)",
    ];
    for f in fixtures {
        sqlx::query(f).bind(&gtfs).execute(&pool).await.unwrap();
    }

    let ops = OperatorV2Service::new(Some(pool.clone()));
    let op = caller(None);
    let today = today_ist();
    let route = format!("R-{}", gtfs);
    let mut g = group_req("RDONE", "MORNING", "00:05");
    g.depot_id = Some(format!("dep-{}", gtfs));
    g.service_type_id = Some("planned-st".to_string());
    let tg = ops.upsert_trip_group(&gtfs, &op, g).await.unwrap();
    ops.upsert_trips(
        &gtfs,
        &op,
        &tg.id,
        UpsertTripsReq {
            first_trip_day_offset: None,
            trips: vec![
                trip(1, "00:05", "00:10", &route),
                trip(2, "23:50", "23:55", &route),
            ],
        },
    )
    .await
    .unwrap();
    let run = ops
        .create_duty_group(&gtfs, &op, run_req(&tg.id, today, Some("VR1"), Some("DR1")))
        .await
        .unwrap();
    let waybill_no = run.duty_group.waybill_no.clone();
    // service type comes from the bus when one is set
    assert_eq!(
        run.duty_group.service_type_id.as_deref(),
        Some(format!("st-{}", gtfs).as_str())
    );
    // a run without a bus takes the trip group's planned service type
    let busless = ops
        .create_duty_group(
            &gtfs,
            &op,
            run_req(&tg.id, today + Duration::days(1), None, None),
        )
        .await
        .unwrap();
    assert_eq!(
        busless.duty_group.service_type_id.as_deref(),
        Some("planned-st")
    );

    let reader = DBVehicleReaderInternal::new(pool.clone());
    assert!(reader.is_vehicle_in_internal("VR1", &gtfs).await);

    let vd = reader.get_vehicle_data("VR1", &gtfs, None).await.unwrap();
    assert_eq!(vd.waybill_no.as_deref(), Some(waybill_no.as_str()));
    assert_eq!(
        vd.schedule_no.as_deref(),
        Some("RDONE_MORNING_1205AM_NORMAL")
    );
    assert_eq!(vd.service_type.as_deref(), Some("Deluxe"));
    assert_eq!(vd.depot.as_deref(), Some("Depot One"));
    assert_eq!(vd.trip_number, Some(1));
    assert_eq!(vd.route_number.as_deref(), Some("21G"));
    assert_eq!(vd.driver_code.as_deref(), Some("DR1"));
    // like the waybill path: current trip at the top level, only the trips after it remaining
    assert_eq!(vd.remaining_trip_details.as_ref().map(|r| r.len()), Some(1));
    assert_eq!(vd.db_start_time.as_deref(), Some("00:05"));
    // dutyTripId: per trip (unlike scheduleTripId, which is the run id on every trip)
    assert_eq!(vd.duty_trip_id, Some(format!("{}-1", waybill_no)));
    let ids: Vec<Option<String>> = vd
        .remaining_trip_details
        .as_ref()
        .unwrap()
        .iter()
        .map(|t| t.duty_trip_id.clone())
        .collect();
    assert_eq!(ids, vec![Some(format!("{}-2", waybill_no))]);
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

    let by_route = reader
        .get_waybills_by_route_id(&route, &gtfs, None, None)
        .await
        .unwrap();
    assert_eq!(
        by_route
            .iter()
            .filter(|r| r.waybill_no == waybill_no)
            .count(),
        2
    );
    let by_route_vehicle = reader
        .get_waybills_by_route_id(&route, &gtfs, Some("OTHER"), None)
        .await
        .unwrap();
    assert!(by_route_vehicle.iter().all(|r| r.waybill_no != waybill_no));

    let one = reader
        .get_waybill_by_waybill_and_trip(&waybill_no, 2, &gtfs)
        .await
        .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].trip_number, Some(2));
    assert_eq!(one[0].db_start_time.as_deref(), Some("23:50"));
    assert_eq!(one[0].duty_trip_id, Some(format!("{}-2", waybill_no)));

    let meta = reader
        .get_waybill_metadata(&gtfs, &waybill_no, None)
        .await
        .unwrap();
    assert_eq!(meta.vehicle_no, "VR1");
    assert_eq!(meta.driver_id.as_deref(), Some("DR1"));
    assert_eq!(meta.driver_name.as_deref(), Some("Ravi K"));
    assert_eq!(meta.service_type, "Deluxe");
    // per-trip crew: trip 2 swapped to another driver
    let trip2 = run
        .duties
        .iter()
        .find(|d| d.trip_number == 2)
        .unwrap()
        .id
        .clone();
    ops.update_trip_crew(
        &gtfs,
        &op,
        &trip2,
        UpdateCrewReq {
            driver_token_number: Some("DR2".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let meta2 = reader
        .get_waybill_metadata(&gtfs, &waybill_no, Some(2))
        .await
        .unwrap();
    assert_eq!(meta2.driver_id.as_deref(), Some("DR2"));
    assert_eq!(meta2.duty_trip_id, Some(format!("{}-2", waybill_no)));
    let meta1 = reader
        .get_waybill_metadata(&gtfs, &waybill_no, Some(1))
        .await
        .unwrap();
    assert_eq!(meta1.driver_id.as_deref(), Some("DR1"));

    // a vehicle without a transitV2 run still goes through the waybill path
    // trip start copies the run's service type onto the trip
    let fleet = FleetOperatorV2Service::new(ops.clone());
    fleet
        .trip_action(&gtfs, &op, action("VR1", TripActionV2::Start, None))
        .await
        .unwrap();
    let after_start = ops
        .get_duty_group_detail(&gtfs, &op, &run.duty_group.id)
        .await
        .unwrap();
    assert_eq!(
        after_start.duties[0].recorded_service_type_id.as_deref(),
        Some(format!("st-{}", gtfs).as_str())
    );

    let none = reader.get_vehicle_data("NOPE", &gtfs, None).await.unwrap();
    assert!(none.waybill_no.is_none());

    cleanup(&pool, &gtfs).await;
    for t in [
        "service_type_internal",
        "entities_internal",
        "vehicles_internal",
        "employees_internal",
        "route_internal",
    ] {
        sqlx::query(&format!("DELETE FROM {} WHERE gtfs_id = $1", t))
            .bind(&gtfs)
            .execute(&pool)
            .await
            .unwrap();
    }
}
