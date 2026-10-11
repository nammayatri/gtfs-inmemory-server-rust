//! transitV2 driver-facing side: resolve a vehicle / driver / conductor to its live run, then
//! `tripAction` (start / end / rollback / skip / cancel / uncancel), `currentOperation` and
//! `activeTrip`. GIMS owns trip state (`duties.status`); nammayatri keeps no cursor for v2.
//! Every status change is logged to `duty_event_logs` (TRIP_STATUS_CHANGE) with its reason.

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;
use sqlx::PgConnection;

use crate::services::operator_v2::{
    load_duties, log_event, map_db, today_ist, trip_group_code, ActiveTripV2, Anchor, AnchorReq,
    Caller, CurrentOperationV2, Duty, DutyEventType, DutyGroup, DutyStatus, DutyView,
    GenerationTrigger, NewEvent, OperatorV2Service, TripActionV2, TripActionV2Req, TripReason,
};
use crate::tools::error::{AppError, AppResult};

/// "Skip remaining" is offered once a trip is this far past its scheduled end.
pub const SKIP_GRACE_MINUTES: i64 = 30;

const PENDING_SQL: &str = "d.status IN ('upcoming', 'active')";

#[derive(Clone)]
pub struct FleetOperatorV2Service {
    ops: OperatorV2Service,
}

pub fn trip_id(waybill_no: &str, trip_number: i32) -> String {
    format!("{}-{}", waybill_no, trip_number)
}

fn view(dg: &DutyGroup, d: &Duty) -> DutyView {
    DutyView {
        duty_id: d.id.clone(),
        trip_id: trip_id(&dg.waybill_no, d.trip_number),
        duty_trip_id: trip_id(&dg.waybill_no, d.trip_number),
        trip_number: d.trip_number,
        trip_order: d.trip_order,
        route_id: d.route_id.clone(),
        is_bookable: d.is_bookable,
        scheduled_start_at: d.scheduled_start_at,
        scheduled_end_at: d.scheduled_end_at,
        recorded_start_time: d.recorded_start_time,
        recorded_end_time: d.recorded_end_time,
        driver_token_number: d
            .driver_token_number
            .clone()
            .or(dg.driver_token_number.clone()),
        driver_name: d.driver_name.clone().or(dg.driver_name.clone()),
        conductor_token_number: d
            .conductor_token_number
            .clone()
            .or(dg.conductor_token_number.clone()),
        conductor_name: d.conductor_name.clone().or(dg.conductor_name.clone()),
        status: d.status.clone(),
        cancel_reason: d.cancel_reason.clone(),
        skip_reason: d.skip_reason.clone(),
    }
}

fn active(duties: &[Duty]) -> Option<&Duty> {
    duties.iter().find(|d| d.is(DutyStatus::Active))
}

/// Next trip to start: the first upcoming one.
fn next_pending(duties: &[Duty]) -> Option<&Duty> {
    duties.iter().find(|d| d.is(DutyStatus::Upcoming))
}

fn by_number(duties: &[Duty], n: i32) -> AppResult<&Duty> {
    duties
        .iter()
        .find(|d| d.trip_number == n)
        .ok_or_else(|| AppError::NotFound(format!("trip {} is not in this run", n)))
}

fn bad(msg: impl Into<String>) -> AppError {
    AppError::BadRequest(msg.into())
}

/// One status change of one trip, applied and logged in the same transaction.
struct Transition<'a> {
    duty: &'a Duty,
    to: DutyStatus,
    reason: Option<TripReason>,
    /// Extra column updates (SQL fragment after `SET status = …,`), bound from `$4` on.
    extra_sql: &'a str,
    extra_ts: Option<DateTime<Utc>>,
    extra_text: Option<String>,
}

async fn apply_transition(
    tx: &mut PgConnection,
    gtfs_id: &str,
    run: &DutyGroup,
    actor: Option<&str>,
    t: Transition<'_>,
) -> AppResult<()> {
    // $3 = reason, $4 = timestamp, $5 = actor, $6 = text for `extra_sql`; unused ones are
    // still sent typed.
    let reason_sql = match t.to {
        DutyStatus::Cancelled => "cancel_reason = $3,",
        DutyStatus::Skipped => "skip_reason = $3,",
        // leaving cancelled / skipped clears the old reasons
        _ => "cancel_reason = NULL, skip_reason = NULL,",
    };
    let sql = format!(
        "UPDATE duties SET status = $2, {reason} {extra}
           status_changed_by = $5, status_changed_at = now(), updated_at = now()
         WHERE id = $1",
        reason = reason_sql,
        extra = t.extra_sql,
    );
    sqlx::query(&sql)
        .bind(&t.duty.id)
        .bind(t.to.as_str())
        .bind(t.reason.map(|r| r.as_str()))
        .bind(t.extra_ts)
        .bind(actor)
        .bind(t.extra_text.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(|e| map_db("trip status change", e))?;
    log_event(
        tx,
        NewEvent {
            gtfs_id,
            operator_id: run.operator_id.as_deref(),
            event_type: DutyEventType::TripStatusChange,
            duty_group_id: Some(&run.id),
            duty_id: Some(&t.duty.id),
            duty_repeat_id: run.duty_repeat_id.as_deref(),
            operation_date: Some(run.operation_date),
            actor_person_id: actor,
            trigger: GenerationTrigger::Api,
            old_value: Some(json!({ "tripNumber": t.duty.trip_number, "status": t.duty.status })),
            new_value: Some(json!({ "tripNumber": t.duty.trip_number, "status": t.to.as_str() })),
            reason: t.reason.map(|r| r.as_str().to_string()),
            error_code: None,
            error_message: None,
        },
    )
    .await
}

impl FleetOperatorV2Service {
    pub fn new(ops: OperatorV2Service) -> Self {
        FleetOperatorV2Service { ops }
    }

    /// The anchor's live run: active, not deleted, operation date today or yesterday (IST, for
    /// runs past midnight), with a trip still to run. A running trip wins, then the earliest.
    pub async fn resolve_run(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        anchor: &Anchor,
    ) -> AppResult<DutyGroup> {
        let pool = self.ops.pool()?;
        let since = today_ist() - Duration::days(1);
        let found: Option<DutyGroup> = match anchor {
            Anchor::DutyGroup(id) => Some(self.ops.load_run(gtfs_id, caller, id, false).await?),
            Anchor::Vehicle(v) => sqlx::query_as(&format!(
                "SELECT dg.* FROM duty_groups dg
                 WHERE dg.gtfs_id = $1 AND dg.vehicle_number = $2 AND dg.is_active
                   AND NOT dg.deleted AND dg.operation_date >= $3
                   AND ($4::text IS NULL OR dg.operator_id = $4)
                   AND EXISTS (SELECT 1 FROM duties d WHERE d.duty_group_id = dg.id
                               AND NOT d.deleted AND {p})
                 ORDER BY EXISTS (SELECT 1 FROM duties d WHERE d.duty_group_id = dg.id
                                  AND d.status = 'active' AND NOT d.deleted) DESC,
                          dg.window_start_at
                 LIMIT 1",
                p = PENDING_SQL
            ))
            .bind(gtfs_id)
            .bind(v)
            .bind(since)
            .bind(&caller.operator_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| map_db("resolve_run (vehicle)", e))?,
            Anchor::Driver(t) | Anchor::Conductor(t) => {
                let slot = if matches!(anchor, Anchor::Driver(_)) {
                    "driver"
                } else {
                    "conductor"
                };
                sqlx::query_as(&format!(
                    "SELECT dg.* FROM duties d JOIN duty_groups dg ON dg.id = d.duty_group_id
                     WHERE d.gtfs_id = $1
                       AND (d.{s}_token_number = $2
                            OR (d.{s}_token_number IS NULL AND dg.{s}_token_number = $2))
                       AND NOT d.deleted AND {p}
                       AND dg.is_active AND NOT dg.deleted AND dg.operation_date >= $3
                       AND ($4::text IS NULL OR dg.operator_id = $4)
                     ORDER BY (d.status = 'active') DESC, d.scheduled_start_at
                     LIMIT 1",
                    s = slot,
                    p = PENDING_SQL
                ))
                .bind(gtfs_id)
                .bind(t)
                .bind(since)
                .bind(&caller.operator_id)
                .fetch_optional(pool)
                .await
                .map_err(|e| map_db("resolve_run (crew)", e))?
            }
        };
        found.ok_or_else(|| {
            AppError::NotFound("No active run found for the provided anchor.".to_string())
        })
    }

    pub async fn current_operation(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        anchor: &AnchorReq,
    ) -> AppResult<CurrentOperationV2> {
        let run = self
            .resolve_run(gtfs_id, caller, &anchor.to_anchor()?)
            .await?;
        self.operation_of(&run).await
    }

    /// The run as the driver sees it: running trip, upcoming (incl. future cancelled), history.
    async fn operation_of(&self, run: &DutyGroup) -> AppResult<CurrentOperationV2> {
        let mut conn = self
            .ops
            .pool()?
            .acquire()
            .await
            .map_err(|e| map_db("acquire", e))?;
        let duties = load_duties(&mut conn, &run.id, false).await?;
        let code = trip_group_code(&mut conn, &run.trip_group_id).await?;
        let now = Utc::now();

        let mut result = CurrentOperationV2 {
            waybill_no: run.waybill_no.clone(),
            duty_group_id: run.id.clone(),
            trip_group_code: code,
            operation_date: run.operation_date,
            vehicle_number: run.vehicle_number.clone(),
            service_type_id: run.service_type_id.clone(),
            driver_token: run.driver_token_number.clone(),
            conductor_token: run.conductor_token_number.clone(),
            active: None,
            upcoming: Vec::new(),
            history: Vec::new(),
        };
        for d in &duties {
            let v = view(run, d);
            let done = d.is(DutyStatus::Completed)
                || d.is(DutyStatus::Skipped)
                || (d.is(DutyStatus::Cancelled) && d.scheduled_end_at <= now);
            if d.is(DutyStatus::Active) {
                result.active = Some(v);
            } else if done {
                result.history.push(v);
            } else {
                result.upcoming.push(v);
            }
        }
        Ok(result)
    }

    pub async fn active_trip(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        anchor: &AnchorReq,
    ) -> AppResult<ActiveTripV2> {
        let run = self
            .resolve_run(gtfs_id, caller, &anchor.to_anchor()?)
            .await?;
        let mut conn = self
            .ops
            .pool()?
            .acquire()
            .await
            .map_err(|e| map_db("acquire", e))?;
        let duties = load_duties(&mut conn, &run.id, false).await?;
        let d = active(&duties).ok_or_else(|| AppError::NotFound("No active trip".to_string()))?;
        Ok(ActiveTripV2 {
            waybill_no: run.waybill_no.clone(),
            duty_group_id: run.id.clone(),
            trip_id: trip_id(&run.waybill_no, d.trip_number),
            duty_trip_id: trip_id(&run.waybill_no, d.trip_number),
            trip_number: d.trip_number,
            route_id: d.route_id.clone(),
        })
    }

    pub async fn trip_action(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        req: TripActionV2Req,
    ) -> AppResult<CurrentOperationV2> {
        let anchor = req.anchor.to_anchor()?;
        let resolved = self.resolve_run(gtfs_id, caller, &anchor).await?;
        let now = Utc::now();
        let at: DateTime<Utc> = match req.timestamp {
            Some(ms) => Utc
                .timestamp_millis_opt(ms)
                .single()
                .ok_or_else(|| bad(format!("invalid timestamp {}", ms)))?,
            None => now,
        };
        let reason = req
            .reason
            .as_deref()
            .map(TripReason::parse)
            .transpose()?
            .unwrap_or(TripReason::Other);
        let actor = caller.actor_person_id.as_deref();

        let mut tx = self
            .ops
            .pool()?
            .begin()
            .await
            .map_err(|e| map_db("begin", e))?;
        let run: DutyGroup = sqlx::query_as("SELECT * FROM duty_groups WHERE id = $1 FOR UPDATE")
            .bind(&resolved.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| map_db("trip_action (run)", e))?;
        if !run.is_active || run.deleted {
            return Err(bad("run is not active"));
        }
        let duties = load_duties(&mut tx, &run.id, true).await?;
        let plain = |duty, to| Transition {
            duty,
            to,
            reason: None,
            extra_sql: "",
            extra_ts: None,
            extra_text: None,
        };

        match req.action {
            TripActionV2::Start => {
                if let Some(a) = active(&duties) {
                    return Err(bad(format!(
                        "trip {} is still running; end it first",
                        a.trip_number
                    )));
                }
                let next = next_pending(&duties).ok_or_else(|| bad("no trip left to start"))?;
                if let Some(n) = req.trip_number {
                    if n != next.trip_number {
                        return Err(bad(format!(
                            "trip {} can't start now; the next trip is {}",
                            n, next.trip_number
                        )));
                    }
                }
                if let Some(vehicle) = &run.vehicle_number {
                    let busy: Option<i32> = sqlx::query_scalar(
                        "SELECT d.trip_number FROM duties d
                         JOIN duty_groups dg ON dg.id = d.duty_group_id
                         WHERE dg.gtfs_id = $1 AND dg.vehicle_number = $2 AND dg.id <> $3
                           AND d.status = 'active' AND NOT d.deleted AND NOT dg.deleted
                         LIMIT 1",
                    )
                    .bind(gtfs_id)
                    .bind(vehicle)
                    .bind(&run.id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| map_db("trip_action (vehicle busy)", e))?;
                    if busy.is_some() {
                        return Err(bad(format!(
                            "vehicle {} has a running trip on another run",
                            vehicle
                        )));
                    }
                }
                apply_transition(
                    &mut tx,
                    gtfs_id,
                    &run,
                    actor,
                    Transition {
                        extra_sql: "recorded_start_time = $4, recorded_vehicle_number = $6,
                                    recorded_service_type_id = (SELECT service_type_id FROM duty_groups
                                                                WHERE id = duties.duty_group_id),",
                        extra_ts: Some(at),
                        extra_text: run.vehicle_number.clone(),
                        ..plain(next, DutyStatus::Active)
                    },
                )
                .await?;
            }
            TripActionV2::End => {
                let a = active(&duties).ok_or_else(|| bad("no running trip to end"))?;
                if let Some(n) = req.trip_number {
                    if n != a.trip_number {
                        return Err(bad(format!(
                            "trip {} isn't running; the running trip is {}",
                            n, a.trip_number
                        )));
                    }
                }
                apply_transition(
                    &mut tx,
                    gtfs_id,
                    &run,
                    actor,
                    Transition {
                        extra_sql: "recorded_end_time = $4,",
                        extra_ts: Some(at),
                        ..plain(a, DutyStatus::Completed)
                    },
                )
                .await?;
            }
            TripActionV2::Rollback => {
                if let Some(a) = active(&duties) {
                    apply_transition(
                        &mut tx,
                        gtfs_id,
                        &run,
                        actor,
                        Transition {
                            extra_sql: "recorded_start_time = NULL, recorded_vehicle_number = NULL,
                                        recorded_service_type_id = NULL,",
                            ..plain(a, DutyStatus::Upcoming)
                        },
                    )
                    .await?;
                } else {
                    let last = duties
                        .iter()
                        .filter(|d| d.is(DutyStatus::Completed))
                        .max_by_key(|d| (d.recorded_end_time, d.scheduled_start_at))
                        .ok_or_else(|| bad("nothing to roll back"))?;
                    apply_transition(
                        &mut tx,
                        gtfs_id,
                        &run,
                        actor,
                        Transition {
                            extra_sql: "recorded_end_time = NULL,",
                            ..plain(last, DutyStatus::Active)
                        },
                    )
                    .await?;
                }
            }
            TripActionV2::Skip => {
                let targets: Vec<&Duty> = match req.trip_number {
                    Some(n) => {
                        let d = by_number(&duties, n)?;
                        if !d.is(DutyStatus::Upcoming) {
                            return Err(bad(format!("trip {} can't be skipped", n)));
                        }
                        vec![d]
                    }
                    None => duties
                        .iter()
                        .filter(|d| {
                            d.is(DutyStatus::Upcoming)
                                && d.scheduled_end_at + Duration::minutes(SKIP_GRACE_MINUTES) <= now
                        })
                        .collect(),
                };
                if targets.is_empty() {
                    return Err(bad("no overdue trip to skip"));
                }
                for d in targets {
                    apply_transition(
                        &mut tx,
                        gtfs_id,
                        &run,
                        actor,
                        Transition {
                            reason: Some(reason),
                            ..plain(d, DutyStatus::Skipped)
                        },
                    )
                    .await?;
                }
            }
            TripActionV2::Cancel => {
                let actor = Some(caller.require_actor()?);
                let n = req
                    .trip_number
                    .ok_or_else(|| bad("tripNumber is required"))?;
                let d = by_number(&duties, n)?;
                if !d.is(DutyStatus::Upcoming) {
                    return Err(bad(format!("trip {} can't be cancelled", n)));
                }
                apply_transition(
                    &mut tx,
                    gtfs_id,
                    &run,
                    actor,
                    Transition {
                        reason: Some(reason),
                        ..plain(d, DutyStatus::Cancelled)
                    },
                )
                .await?;
            }
            TripActionV2::Uncancel => {
                let actor = Some(caller.require_actor()?);
                let n = req
                    .trip_number
                    .ok_or_else(|| bad("tripNumber is required"))?;
                let d = by_number(&duties, n)?;
                if !d.is(DutyStatus::Cancelled) {
                    return Err(bad(format!("trip {} isn't cancelled", n)));
                }
                apply_transition(
                    &mut tx,
                    gtfs_id,
                    &run,
                    actor,
                    plain(d, DutyStatus::Upcoming),
                )
                .await?;
            }
        }
        // Deferred overlap constraints (e.g. uncancel onto a busy driver) fire here.
        tx.commit().await.map_err(|e| map_db("commit", e))?;

        let after = self.operation_of(&run).await?;
        let finished = after.active.is_none()
            && !after
                .upcoming
                .iter()
                .any(|t| t.status == DutyStatus::Upcoming.as_str());

        if finished
            && run.duty_repeat_id.is_some()
            && matches!(
                req.action,
                TripActionV2::End | TripActionV2::Skip | TripActionV2::Cancel
            )
        {
            let ops = self.ops.clone();
            let gtfs = gtfs_id.to_string();
            let finished_run = run.clone();
            tokio::spawn(async move {
                ops.generate_after_run_finish(&gtfs, &finished_run).await;
            });
        }

        Ok(after)
    }
}
