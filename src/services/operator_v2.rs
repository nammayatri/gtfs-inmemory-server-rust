//! transitV2 operator side: trip groups, trips, repeat rules, runs (duty groups), trips of a
//! run (duties), generation and the event log. API JSON is camelCase.
//! Model and rules: scripts/plans/gims/transitV2/README.md.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, NaiveTime, TimeZone, Utc};
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use tracing::{error, info, warn};
use utoipa::ToSchema;

use crate::services::field_generator::{gen_random_id, generate_waybill_number};
use crate::tools::error::{AppError, AppResult};

// ─── Enums (stored as TEXT) ──────────────────────────────────────────────────

macro_rules! text_enum {
    ($name:ident, $what:literal, { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn all() -> Vec<&'static str> {
                vec![$($text),+]
            }

            pub fn parse(s: &str) -> AppResult<Self> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(AppError::BadRequest(format!(
                        "Invalid {} '{}'. Valid: {:?}", $what, other, Self::all()
                    ))),
                }
            }
        }
    };
}

text_enum!(Shift, "shift", {
    Morning => "MORNING",
    Afternoon => "AFTERNOON",
    Evening => "EVENING",
    Night => "NIGHT",
    Midnight => "MIDNIGHT",
    AllDay => "ALLDAY",
});

// Kind of trip group; last part of its code.
text_enum!(GroupTripType, "trip type", {
    Normal => "NORMAL",
    Short => "SHORT",
    Feeder => "FEEDER",
});

text_enum!(RepeatStatus, "repeat status", {
    Active => "active",
    Inactive => "inactive",
});

// Lifecycle of one trip of a run.
text_enum!(DutyStatus, "trip status", {
    Upcoming => "upcoming",
    Active => "active",
    Completed => "completed",
    Skipped => "skipped",
    Cancelled => "cancelled",
});

// Why a trip was cancelled or skipped.
text_enum!(TripReason, "reason", {
    Operator => "OPERATOR",
    Breakdown => "BREAKDOWN",
    Admin => "ADMIN",
    Driver => "DRIVER",
    Other => "OTHER",
});

text_enum!(DutyEventType, "event type", {
    VehicleChange => "VEHICLE_CHANGE",
    CrewChange => "CREW_CHANGE",
    TripStatusChange => "TRIP_STATUS_CHANGE",
    RunActiveChange => "RUN_ACTIVE_CHANGE",
    GenerationFailure => "GENERATION_FAILURE",
});

text_enum!(GenerationTrigger, "trigger", {
    Api => "API",
    Cron => "CRON",
    RunFinish => "RUN_FINISH",
    RuleSave => "RULE_SAVE",
    ManualGenerate => "MANUAL_GENERATE",
});

text_enum!(TripActionV2, "trip action", {
    Start => "start",
    End => "end",
    Rollback => "rollback",
    Skip => "skip",
    Cancel => "cancel",
    Uncancel => "uncancel",
});

// ─── Caller (from headers) ───────────────────────────────────────────────────

/// Who is calling, from the `x-operator-id` / `x-actor-person-id` headers set by nammayatri.
/// `operator_id = None` means an admin call: no ownership filter.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    pub operator_id: Option<String>,
    pub actor_person_id: Option<String>,
}

impl Caller {
    pub fn require_actor(&self) -> AppResult<&str> {
        self.actor_person_id
            .as_deref()
            .ok_or_else(|| AppError::BadRequest("x-actor-person-id header is required".to_string()))
    }

    /// Rows owned by another operator are hidden from operator callers.
    pub fn can_see(&self, row_operator_id: Option<&str>) -> bool {
        match &self.operator_id {
            None => true,
            Some(op) => row_operator_id == Some(op.as_str()),
        }
    }
}

// ─── Rows ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TripGroup {
    pub id: String,
    pub gtfs_id: String,
    pub operator_id: Option<String>,
    /// ZONE_SHIFT_FIRSTDEP_TRIPTYPE, e.g. UNNAYANBHAVAN_MORNING_0720AM_FEEDER
    pub code: String,
    pub description: Option<String>,
    pub zone: String,
    pub shift: String,
    #[schema(value_type = String, example = "07:20:00")]
    pub first_departure: NaiveTime,
    pub trip_type: String,
    pub depot_id: Option<String>,
    /// Planned service type; runs without a bus use it.
    pub service_type_id: Option<String>,
    pub deleted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Trip {
    pub id: String,
    pub trip_group_id: String,
    pub gtfs_id: String,
    pub operator_id: Option<String>,
    pub route_id: String,
    pub is_bookable: bool,
    pub trip_number: i32,
    pub trip_order: i32,
    #[schema(value_type = String, example = "10:00:00")]
    pub scheduled_start_time: NaiveTime,
    pub scheduled_start_day_offset: i16,
    #[schema(value_type = String, example = "11:00:00")]
    pub scheduled_end_time: NaiveTime,
    pub scheduled_end_day_offset: i16,
    pub deleted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DutyRepeat {
    pub id: String,
    pub trip_group_id: String,
    pub gtfs_id: String,
    pub operator_id: Option<String>,
    pub repeat_status: String,
    pub recurrence_days: Vec<i16>,
    pub effective_from: NaiveDate,
    pub effective_till: Option<NaiveDate>,
    pub generated_till: Option<NaiveDate>,
    pub vehicle_number: Option<String>,
    pub driver_token_number: Option<String>,
    pub driver_name: Option<String>,
    pub conductor_token_number: Option<String>,
    pub conductor_name: Option<String>,
    pub deleted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DutyGroup {
    pub id: String,
    pub waybill_no: String,
    pub trip_group_id: String,
    pub duty_repeat_id: Option<String>,
    pub gtfs_id: String,
    pub operator_id: Option<String>,
    pub operation_date: NaiveDate,
    pub depot_id: Option<String>,
    pub vehicle_number: Option<String>,
    /// The bus's service type when a bus is set, else the trip group's.
    pub service_type_id: Option<String>,
    pub driver_token_number: Option<String>,
    pub driver_name: Option<String>,
    pub conductor_token_number: Option<String>,
    pub conductor_name: Option<String>,
    pub window_start_at: DateTime<Utc>,
    pub window_end_at: DateTime<Utc>,
    pub is_active: bool,
    pub deleted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Duty {
    pub id: String,
    pub duty_group_id: String,
    pub trip_id: String,
    pub gtfs_id: String,
    pub operator_id: Option<String>,
    pub route_id: String,
    pub is_bookable: bool,
    pub trip_number: i32,
    pub trip_order: i32,
    pub scheduled_start_at: DateTime<Utc>,
    pub scheduled_end_at: DateTime<Utc>,
    pub driver_token_number: Option<String>,
    pub driver_name: Option<String>,
    pub conductor_token_number: Option<String>,
    pub conductor_name: Option<String>,
    pub recorded_start_time: Option<DateTime<Utc>>,
    pub recorded_end_time: Option<DateTime<Utc>>,
    pub recorded_vehicle_number: Option<String>,
    pub recorded_service_type_id: Option<String>,
    pub run_active: bool,
    /// upcoming | active | completed | skipped | cancelled
    pub status: String,
    pub cancel_reason: Option<String>,
    pub skip_reason: Option<String>,
    pub status_changed_by: Option<String>,
    pub status_changed_at: Option<DateTime<Utc>>,
    pub deleted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// `waybill_no-trip_number`, the trip's id in the fleet / rider APIs (`trip_id` above is the
    /// template trip). Filled by the duty-group reads.
    #[sqlx(default)]
    pub duty_trip_id: Option<String>,
}

impl Duty {
    pub fn is(&self, status: DutyStatus) -> bool {
        self.status == status.as_str()
    }

    /// Still to run: upcoming or running.
    pub fn is_pending(&self) -> bool {
        !self.deleted && (self.is(DutyStatus::Upcoming) || self.is(DutyStatus::Active))
    }

    pub fn has_started(&self) -> bool {
        self.recorded_start_time.is_some()
    }
}

/// `duty_event_logs` row; JSONB columns are read as text (no sqlx json feature).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DutyEventLogRow {
    pub id: String,
    pub gtfs_id: String,
    pub operator_id: Option<String>,
    pub event_type: String,
    pub duty_group_id: Option<String>,
    pub duty_id: Option<String>,
    pub duty_repeat_id: Option<String>,
    pub operation_date: Option<NaiveDate>,
    pub actor_person_id: Option<String>,
    pub trigger: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub reason: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DutyEventLog {
    pub id: String,
    pub gtfs_id: String,
    pub operator_id: Option<String>,
    pub event_type: String,
    pub duty_group_id: Option<String>,
    pub duty_id: Option<String>,
    pub duty_repeat_id: Option<String>,
    pub operation_date: Option<NaiveDate>,
    pub actor_person_id: Option<String>,
    pub trigger: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub old_value: Option<Value>,
    #[schema(value_type = Option<Object>)]
    pub new_value: Option<Value>,
    pub reason: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl From<DutyEventLogRow> for DutyEventLog {
    fn from(r: DutyEventLogRow) -> Self {
        let parse = |s: Option<String>| s.and_then(|t| serde_json::from_str(&t).ok());
        DutyEventLog {
            id: r.id,
            gtfs_id: r.gtfs_id,
            operator_id: r.operator_id,
            event_type: r.event_type,
            duty_group_id: r.duty_group_id,
            duty_id: r.duty_id,
            duty_repeat_id: r.duty_repeat_id,
            operation_date: r.operation_date,
            actor_person_id: r.actor_person_id,
            trigger: r.trigger,
            old_value: parse(r.old_value),
            new_value: parse(r.new_value),
            reason: r.reason,
            error_code: r.error_code,
            error_message: r.error_message,
            resolved_at: r.resolved_at,
            created_at: r.created_at,
        }
    }
}

// ─── Responses ───────────────────────────────────────────────────────────────

/// List response. Not in the OpenAPI components (generic); handlers document `{items, total}`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DutyGroupListItem {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub duty_group: DutyGroup,
    pub trip_group_code: String,
    pub total_trips: i64,
    pub pending_trips: i64,
    /// Trips (not cancelled / deleted) with no driver, plus all of them when the run has no vehicle.
    pub unassigned_trips: i64,
    /// Trip number of the running trip, if any.
    pub running_trip_number: Option<i32>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TripGroupListItem {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub trip_group: TripGroup,
    pub trip_count: i64,
    pub repeat_count: i64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DutyGroupDetail {
    pub duty_group: DutyGroup,
    pub trip_group_code: String,
    pub duties: Vec<Duty>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GenerateEntry {
    pub duty_repeat_id: String,
    pub operation_date: NaiveDate,
    /// created | created_partial | exists | covered (a manual run already operates the trip
    /// group that day) | past (every trip of that day has ended) | ok (preview) | off_day |
    /// out_of_window | failed
    pub verdict: String,
    pub duty_group_id: Option<String>,
    pub waybill_no: Option<String>,
    pub error: Option<String>,
}

/// A trip as the driver / dashboard sees it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DutyView {
    pub duty_id: String,
    /// `waybill_no-trip_number`
    pub trip_id: String,
    /// Same as `trip_id`; the name used across GIMS responses (`dutyTripId`).
    pub duty_trip_id: String,
    pub trip_number: i32,
    pub trip_order: i32,
    pub route_id: String,
    pub is_bookable: bool,
    pub scheduled_start_at: DateTime<Utc>,
    pub scheduled_end_at: DateTime<Utc>,
    pub recorded_start_time: Option<DateTime<Utc>>,
    pub recorded_end_time: Option<DateTime<Utc>>,
    pub driver_token_number: Option<String>,
    pub driver_name: Option<String>,
    pub conductor_token_number: Option<String>,
    pub conductor_name: Option<String>,
    /// upcoming | active | completed | skipped | cancelled
    pub status: String,
    pub cancel_reason: Option<String>,
    pub skip_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CurrentOperationV2 {
    pub waybill_no: String,
    pub duty_group_id: String,
    pub trip_group_code: String,
    pub operation_date: NaiveDate,
    pub vehicle_number: Option<String>,
    pub service_type_id: Option<String>,
    pub driver_token: Option<String>,
    pub conductor_token: Option<String>,
    pub active: Option<DutyView>,
    pub upcoming: Vec<DutyView>,
    pub history: Vec<DutyView>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActiveTripV2 {
    pub waybill_no: String,
    pub duty_group_id: String,
    pub trip_id: String,
    /// Same as `trip_id` (`dutyTripId` everywhere in GIMS).
    pub duty_trip_id: String,
    pub trip_number: i32,
    pub route_id: String,
}

// ─── Requests ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpsertTripGroupReq {
    pub id: Option<String>,
    /// Optional: GIMS builds it from zone / shift / firstDeparture / tripType; if sent it must match.
    pub code: Option<String>,
    pub description: Option<String>,
    /// Spaces / special characters are dropped and it is uppercased ("Unnayan bhavan" -> UNNAYANBHAVAN).
    pub zone: String,
    pub shift: String,
    /// "HH:MM"; must equal the first trip's start.
    pub first_departure: String,
    /// NORMAL | SHORT | FEEDER
    pub trip_type: String,
    pub depot_id: Option<String>,
    pub service_type_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TripInput {
    pub id: Option<String>,
    pub route_id: String,
    pub trip_number: i32,
    pub trip_order: i32,
    /// "HH:MM" or "HH:MM:SS"
    pub scheduled_start_time: String,
    pub scheduled_end_time: String,
    pub is_bookable: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpsertTripsReq {
    /// Day offset of the first trip (0 = operation date, 1 = next day). Default: keep / 0.
    pub first_trip_day_offset: Option<i16>,
    pub trips: Vec<TripInput>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpsertDutyRepeatReq {
    pub id: Option<String>,
    pub trip_group_id: String,
    pub repeat_status: Option<String>,
    pub recurrence_days: Vec<i16>,
    pub effective_from: NaiveDate,
    pub effective_till: Option<NaiveDate>,
    pub vehicle_number: Option<String>,
    pub driver_token_number: Option<String>,
    pub driver_name: Option<String>,
    pub conductor_token_number: Option<String>,
    pub conductor_name: Option<String>,
    /// Create duty groups for today .. today + 7 right after saving. Default true; when false the
    /// daily cron (or an explicit generate) picks the rule up.
    pub generate: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GenerateReq {
    /// Either `daysAhead` (from today IST) or `from` + `to`.
    pub days_ahead: Option<i64>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub duty_repeat_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateDutyGroupReq {
    pub trip_group_id: String,
    pub operation_date: NaiveDate,
    pub vehicle_number: Option<String>,
    pub driver_token_number: Option<String>,
    pub driver_name: Option<String>,
    pub conductor_token_number: Option<String>,
    pub conductor_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateVehicleReq {
    /// `null` / absent clears the vehicle.
    pub vehicle_number: Option<String>,
}

/// Crew change. A field that is absent is left unchanged; an empty string clears it.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCrewReq {
    pub driver_token_number: Option<String>,
    pub driver_name: Option<String>,
    pub conductor_token_number: Option<String>,
    pub conductor_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetActiveReq {
    pub is_active: bool,
}

/// Exactly one of `vehicleNumber` / `driverToken` / `conductorToken`, or `dutyGroupId`
/// (dashboard: act on a known run).
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AnchorReq {
    pub vehicle_number: Option<String>,
    pub driver_token: Option<String>,
    pub conductor_token: Option<String>,
    pub duty_group_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Anchor {
    Vehicle(String),
    Driver(String),
    Conductor(String),
    DutyGroup(String),
}

impl AnchorReq {
    pub fn to_anchor(&self) -> AppResult<Anchor> {
        let non_empty = |v: &Option<String>| v.as_ref().filter(|s| !s.trim().is_empty()).cloned();
        let set: Vec<Anchor> = [
            non_empty(&self.vehicle_number).map(Anchor::Vehicle),
            non_empty(&self.driver_token).map(Anchor::Driver),
            non_empty(&self.conductor_token).map(Anchor::Conductor),
            non_empty(&self.duty_group_id).map(Anchor::DutyGroup),
        ]
        .into_iter()
        .flatten()
        .collect();
        match set.len() {
            1 => Ok(set.into_iter().next().expect("len checked")),
            _ => Err(AppError::BadRequest(
                "Provide exactly one of vehicleNumber, driverToken, conductorToken, dutyGroupId"
                    .to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TripActionV2Req {
    #[serde(flatten)]
    pub anchor: AnchorReq,
    pub action: TripActionV2,
    pub trip_number: Option<i32>,
    /// Epoch millis from the device; defaults to now.
    pub timestamp: Option<i64>,
    /// For `cancel` / `skip`: OPERATOR | BREAKDOWN | ADMIN | DRIVER | OTHER (default OTHER).
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PageQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TripGroupListQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub zone: Option<String>,
    pub trip_type: Option<String>,
    pub shift: Option<String>,
    pub depot_id: Option<String>,
    /// Case-insensitive "contains" on the code.
    pub code: Option<String>,
    /// Free text, partial and case-insensitive, over code, zone, description and the repeat
    /// configs' default bus / driver / conductor.
    pub search: Option<String>,
    /// Groups with a live repeat config whose default bus / driver / conductor is this.
    pub vehicle_number: Option<String>,
    pub driver_token_number: Option<String>,
    pub conductor_token_number: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DutyRepeatListQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub trip_group_id: Option<String>,
    /// Case-insensitive "contains" on the trip group's code.
    pub code: Option<String>,
    /// Free text, partial and case-insensitive, over the trip group code and the default bus /
    /// driver / conductor.
    pub search: Option<String>,
    pub vehicle_number: Option<String>,
    pub driver_token_number: Option<String>,
    pub conductor_token_number: Option<String>,
    /// active | inactive
    pub repeat_status: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DutyGroupListQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Case-insensitive "contains" on the trip group's code, or the waybill no.
    pub code: Option<String>,
    /// Free text, partial and case-insensitive, over waybill no, trip group code, bus and the
    /// run's / any trip's driver and conductor.
    pub search: Option<String>,
    pub operation_date: Option<NaiveDate>,
    pub trip_group_id: Option<String>,
    pub depot_id: Option<String>,
    pub vehicle_number: Option<String>,
    pub driver_token_number: Option<String>,
    pub conductor_token_number: Option<String>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailureListQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub resolved: Option<bool>,
}

pub const DEFAULT_PAGE_LIMIT: i64 = 50;
pub const MAX_PAGE_LIMIT: i64 = 500;

pub fn page(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    (
        limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT),
        offset.unwrap_or(0).max(0),
    )
}

// ─── Time (IST) ──────────────────────────────────────────────────────────────

pub fn ist() -> FixedOffset {
    FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("+05:30 is a valid offset")
}

pub fn today_ist() -> NaiveDate {
    Utc::now().with_timezone(&ist()).date_naive()
}

/// "HH:MM" or "HH:MM:SS".
pub fn parse_hhmm(s: &str) -> AppResult<NaiveTime> {
    let t = s.trim();
    NaiveTime::parse_from_str(t, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(t, "%H:%M"))
        .map_err(|_| AppError::BadRequest(format!("Invalid time '{}', expected HH:MM[:SS]", s)))
}

/// `(operation_date + day_offset) + time`, interpreted in IST.
pub fn to_instant(operation_date: NaiveDate, day_offset: i16, time: NaiveTime) -> DateTime<Utc> {
    let local = (operation_date + Duration::days(i64::from(day_offset))).and_time(time);
    ist()
        .from_local_datetime(&local)
        .single()
        .expect("fixed offset has no ambiguous local times")
        .with_timezone(&Utc)
}

/// Zone part of a trip group code: ASCII letters / digits only, uppercased.
pub fn normalize_zone(zone: &str) -> AppResult<String> {
    let z: String = zone
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if z.is_empty() {
        return Err(AppError::BadRequest(
            "zone needs at least one letter or digit".to_string(),
        ));
    }
    Ok(z)
}

/// First-departure part of a code: 12-hour HHMM + AM/PM, e.g. 07:20 -> 0720AM, 12:05 -> 1205PM.
pub fn first_departure_token(t: NaiveTime) -> String {
    use chrono::Timelike;
    let h = t.hour();
    let h12 = if h.is_multiple_of(12) { 12 } else { h % 12 };
    format!(
        "{:02}{:02}{}",
        h12,
        t.minute(),
        if h < 12 { "AM" } else { "PM" }
    )
}

/// ZONE_SHIFT_FIRSTDEP_TRIPTYPE
pub fn build_trip_group_code(
    zone: &str,
    shift: Shift,
    first_departure: NaiveTime,
    trip_type: GroupTripType,
) -> String {
    format!(
        "{}_{}_{}_{}",
        zone,
        shift.as_str(),
        first_departure_token(first_departure),
        trip_type.as_str()
    )
}

/// Day offsets for trips already sorted by `trip_order`.
///
/// A start earlier than the previous trip's start means midnight was crossed; an end earlier
/// than the trip's own start means the trip ends the next day. `first_offset` is the first
/// trip's start offset (the only ambiguous one).
pub fn compute_offsets(times: &[(NaiveTime, NaiveTime)], first_offset: i16) -> Vec<(i16, i16)> {
    let mut day = first_offset;
    let mut prev_start: Option<NaiveTime> = None;
    times
        .iter()
        .map(|(start, end)| {
            if let Some(p) = prev_start {
                if *start < p {
                    day += 1;
                }
            }
            prev_start = Some(*start);
            let end_offset = if end < start { day + 1 } else { day };
            (day, end_offset)
        })
        .collect()
}

// ─── DB helpers ──────────────────────────────────────────────────────────────

/// How far ahead repeat rules are generated (rule save, cron default).
pub const LOOKAHEAD_DAYS: i64 = 7;
const GENERATION_CONCURRENCY: usize = 8;
const MAX_GENERATE_RANGE_DAYS: i64 = 62;

fn overlap_message(constraint: &str) -> String {
    match constraint {
        "dg_vehicle_no_overlap" => {
            "Vehicle is already on another run at an overlapping time".to_string()
        }
        "duties_driver_no_overlap" => {
            "Driver is already on another trip at an overlapping time".to_string()
        }
        "duties_conductor_no_overlap" => {
            "Conductor is already on another trip at an overlapping time".to_string()
        }
        other => format!("Overlapping assignment ({})", other),
    }
}

/// Exclusion-constraint name when `e` is an overlap violation (SQLSTATE 23P01).
pub(crate) fn overlap_constraint(e: &sqlx::Error) -> Option<String> {
    match e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23P01") => {
            Some(db.constraint().unwrap_or_default().to_string())
        }
        _ => None,
    }
}

pub(crate) fn map_db(ctx: &str, e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db) = &e {
        let constraint = db.constraint().unwrap_or_default().to_string();
        match db.code().as_deref() {
            Some("23P01") => return AppError::Conflict(overlap_message(&constraint)),
            Some("23505") => return AppError::Conflict(format!("Already exists ({})", constraint)),
            Some("23503") => {
                return AppError::BadRequest(format!("Referenced row not found ({})", constraint))
            }
            _ => {}
        }
    }
    AppError::DbError(format!("{}: {}", ctx, e))
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// `%text%` for a partial, case-insensitive match (trigram-indexed columns).
fn contains(text: &str) -> String {
    format!("%{}%", escape_like(text))
}

/// Empty / whitespace-only string -> None.
fn clean(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub(crate) struct NewEvent<'a> {
    pub gtfs_id: &'a str,
    pub operator_id: Option<&'a str>,
    pub event_type: DutyEventType,
    pub duty_group_id: Option<&'a str>,
    pub duty_id: Option<&'a str>,
    pub duty_repeat_id: Option<&'a str>,
    pub operation_date: Option<NaiveDate>,
    pub actor_person_id: Option<&'a str>,
    pub trigger: GenerationTrigger,
    pub old_value: Option<Value>,
    pub new_value: Option<Value>,
    pub reason: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

pub(crate) async fn log_event(conn: &mut PgConnection, ev: NewEvent<'_>) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO duty_event_logs
           (id, gtfs_id, operator_id, event_type, duty_group_id, duty_id, duty_repeat_id,
            operation_date, actor_person_id, trigger, old_value, new_value, reason, error_code,
            error_message)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::jsonb, $12::jsonb, $13, $14, $15)",
    )
    .bind(gen_random_id())
    .bind(ev.gtfs_id)
    .bind(ev.operator_id)
    .bind(ev.event_type.as_str())
    .bind(ev.duty_group_id)
    .bind(ev.duty_id)
    .bind(ev.duty_repeat_id)
    .bind(ev.operation_date)
    .bind(ev.actor_person_id)
    .bind(ev.trigger.as_str())
    .bind(ev.old_value.map(|v| v.to_string()))
    .bind(ev.new_value.map(|v| v.to_string()))
    .bind(ev.reason)
    .bind(ev.error_code)
    .bind(ev.error_message)
    .execute(conn)
    .await
    .map_err(|e| map_db("log_event", e))?;
    Ok(())
}

const EVENT_LOG_COLUMNS: &str = "id, gtfs_id, operator_id, event_type, duty_group_id, duty_id,
    duty_repeat_id, operation_date, actor_person_id, trigger, old_value::text AS old_value,
    new_value::text AS new_value, reason, error_code, error_message, resolved_at, created_at";

/// Duties of a run in running order; `FOR UPDATE` when `lock`.
pub(crate) async fn load_duties(
    conn: &mut PgConnection,
    duty_group_id: &str,
    lock: bool,
) -> AppResult<Vec<Duty>> {
    let sql = format!(
        "SELECT * FROM duties WHERE duty_group_id = $1 AND NOT deleted
         ORDER BY scheduled_start_at, trip_order{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    sqlx::query_as::<_, Duty>(&sql)
        .bind(duty_group_id)
        .fetch_all(conn)
        .await
        .map_err(|e| map_db("load_duties", e))
}

pub(crate) async fn trip_group_code(
    conn: &mut PgConnection,
    trip_group_id: &str,
) -> AppResult<String> {
    sqlx::query_scalar::<_, String>("SELECT code FROM trip_groups WHERE id = $1")
        .bind(trip_group_id)
        .fetch_optional(conn)
        .await
        .map_err(|e| map_db("trip_group_code", e))
        .map(|c| c.unwrap_or_default())
}

/// A bus's `vehicles_internal.bus_service_type_id` (vehicle_number holds the fleet no, as on
/// waybills).
/// Fills `Duty::duty_trip_id` (`waybill_no-trip_number`) for duties of one run.
fn with_duty_trip_ids(waybill_no: &str, mut duties: Vec<Duty>) -> Vec<Duty> {
    for d in &mut duties {
        d.duty_trip_id = Some(format!("{}-{}", waybill_no, d.trip_number));
    }
    duties
}

pub(crate) async fn bus_service_type(
    conn: &mut PgConnection,
    gtfs_id: &str,
    vehicle_number: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT bus_service_type_id::text FROM vehicles_internal
         WHERE fleet_no = $1 AND gtfs_id = $2 AND deleted = false
         ORDER BY updated_at DESC NULLS LAST LIMIT 1",
    )
    .bind(vehicle_number)
    .bind(gtfs_id)
    .fetch_optional(conn)
    .await
    .map(Option::flatten)
}

/// Crew slot change computed from a request field pair against the current value.
struct CrewChange {
    old_token: Option<String>,
    new_token: Option<String>,
    new_name: Option<String>,
}

/// `token` absent = unchanged; `Some("")` = clear. `name` absent keeps the old name only if the
/// token didn't change.
fn crew_change(
    old_token: Option<String>,
    old_name: Option<String>,
    token: &Option<String>,
    name: &Option<String>,
) -> Option<CrewChange> {
    if token.is_none() && name.is_none() {
        return None;
    }
    let new_token = match token {
        Some(t) => clean(Some(t.clone())),
        None => old_token.clone(),
    };
    let new_name = match name {
        Some(n) => clean(Some(n.clone())),
        None if new_token == old_token => old_name.clone(),
        None => None,
    };
    let new_name = if new_token.is_none() { None } else { new_name };
    if new_token == old_token && new_name == old_name {
        return None;
    }
    Some(CrewChange {
        old_token,
        new_token,
        new_name,
    })
}

/// What a run is created with (manual create and generation share this).
#[derive(Debug, Clone)]
pub(crate) struct RunInput {
    pub trip_group_id: String,
    pub operation_date: NaiveDate,
    pub duty_repeat_id: Option<String>,
    pub operator_id: Option<String>,
    pub vehicle_number: Option<String>,
    pub driver_token_number: Option<String>,
    pub driver_name: Option<String>,
    pub conductor_token_number: Option<String>,
    pub conductor_name: Option<String>,
}

pub(crate) enum InsertRunError {
    /// Exclusion constraint name.
    Overlap(String),
    App(AppError),
}

impl From<AppError> for InsertRunError {
    fn from(e: AppError) -> Self {
        InsertRunError::App(e)
    }
}

fn classify(ctx: &str, e: sqlx::Error) -> InsertRunError {
    match overlap_constraint(&e) {
        Some(c) => InsertRunError::Overlap(c),
        None => InsertRunError::App(map_db(ctx, e)),
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpsertDutyRepeatResp {
    pub duty_repeat: DutyRepeat,
    /// Runs generated for today .. today + 7 right after saving (empty when inactive or
    /// `generate: false`).
    pub generated: Vec<GenerateEntry>,
}

// ─── Service ─────────────────────────────────────────────────────────────────

/// transitV2 operator service over the internal DB. `pool = None` (no internal DB configured)
/// makes every call return NotReady.
#[derive(Clone)]
pub struct OperatorV2Service {
    pool: Option<PgPool>,
}

impl OperatorV2Service {
    pub fn new(pool: Option<PgPool>) -> Self {
        OperatorV2Service { pool }
    }

    pub fn pool(&self) -> AppResult<&PgPool> {
        self.pool
            .as_ref()
            .ok_or_else(|| AppError::NotReady("internal database is not configured".to_string()))
    }

    // ── Trip groups ─────────────────────────────────────────────────────────

    pub async fn upsert_trip_group(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        req: UpsertTripGroupReq,
    ) -> AppResult<TripGroup> {
        let pool = self.pool()?;
        caller.require_actor()?;
        let shift = Shift::parse(req.shift.trim().to_uppercase().as_str())?;
        let trip_type = GroupTripType::parse(req.trip_type.trim().to_uppercase().as_str())?;
        let zone = normalize_zone(&req.zone)?;
        let first_departure = parse_hhmm(&req.first_departure)?;
        let code = build_trip_group_code(&zone, shift, first_departure, trip_type);
        if let Some(sent) = clean(req.code) {
            if sent != code {
                return Err(AppError::BadRequest(format!(
                    "code '{}' doesn't match zone / shift / firstDeparture / tripType, expected '{}'",
                    sent, code
                )));
            }
        }
        let description = clean(req.description);
        let depot_id = clean(req.depot_id);
        let service_type_id = clean(req.service_type_id);
        match clean(req.id) {
            Some(id) => {
                self.get_trip_group_for(gtfs_id, caller, &id, true).await?;
                sqlx::query_as::<_, TripGroup>(
                    "UPDATE trip_groups
                 SET code = $3, description = $4, shift = $5, depot_id = $6, service_type_id = $7,
                     zone = $8, first_departure = $9, trip_type = $10, updated_at = now()
                 WHERE id = $1 AND gtfs_id = $2 AND NOT deleted
                 RETURNING *",
                )
                .bind(&id)
                .bind(gtfs_id)
                .bind(&code)
                .bind(&description)
                .bind(shift.as_str())
                .bind(&depot_id)
                .bind(&service_type_id)
                .bind(&zone)
                .bind(first_departure)
                .bind(trip_type.as_str())
                .fetch_optional(pool)
                .await
                .map_err(|e| map_db("upsert_trip_group", e))?
                .ok_or_else(|| AppError::NotFound(format!("trip group '{}' not found", id)))
            }
            None => sqlx::query_as::<_, TripGroup>(
                "INSERT INTO trip_groups
                   (id, gtfs_id, code, description, shift, depot_id, operator_id, service_type_id,
                    zone, first_departure, trip_type)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                 RETURNING *",
            )
            .bind(gen_random_id())
            .bind(gtfs_id)
            .bind(&code)
            .bind(&description)
            .bind(shift.as_str())
            .bind(&depot_id)
            .bind(&caller.operator_id)
            .bind(&service_type_id)
            .bind(&zone)
            .bind(first_departure)
            .bind(trip_type.as_str())
            .fetch_one(pool)
            .await
            .map_err(|e| map_db("upsert_trip_group", e)),
        }
    }

    pub async fn get_trip_group(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
    ) -> AppResult<TripGroup> {
        self.get_trip_group_for(gtfs_id, caller, id, false).await
    }

    async fn get_trip_group_for(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
        for_write: bool,
    ) -> AppResult<TripGroup> {
        let tg = sqlx::query_as::<_, TripGroup>(
            "SELECT * FROM trip_groups WHERE id = $1 AND gtfs_id = $2 AND NOT deleted",
        )
        .bind(id)
        .bind(gtfs_id)
        .fetch_optional(self.pool()?)
        .await
        .map_err(|e| map_db("get_trip_group", e))?
        .ok_or_else(|| AppError::NotFound(format!("trip group '{}' not found", id)))?;
        if !caller.can_see(tg.operator_id.as_deref()) {
            return Err(if for_write {
                AppError::Forbidden("trip group belongs to another operator".to_string())
            } else {
                AppError::NotFound(format!("trip group '{}' not found", id))
            });
        }
        Ok(tg)
    }

    pub async fn list_trip_groups(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        q: TripGroupListQuery,
    ) -> AppResult<Paged<TripGroupListItem>> {
        let pool = self.pool()?;
        let (limit, offset) = page(q.limit, q.offset);
        let filters = |qb: &mut QueryBuilder<'_, Postgres>| {
            qb.push(" WHERE gtfs_id = ")
                .push_bind(gtfs_id.to_string())
                .push(" AND NOT deleted");
            if let Some(s) = clean(q.shift.clone()) {
                qb.push(" AND shift = ").push_bind(s.to_uppercase());
            }
            if let Some(z) = clean(q.zone.clone()) {
                qb.push(" AND zone = ")
                    .push_bind(normalize_zone(&z).unwrap_or(z));
            }
            if let Some(t) = clean(q.trip_type.clone()) {
                qb.push(" AND trip_type = ").push_bind(t.to_uppercase());
            }
            if let Some(d) = clean(q.depot_id.clone()) {
                qb.push(" AND depot_id = ").push_bind(d);
            }
            if let Some(c) = clean(q.code.clone()) {
                qb.push(" AND code ILIKE ")
                    .push_bind(format!("%{}%", escape_like(&c)));
            }
            if let Some(text) = clean(q.search.clone()) {
                let p = contains(&text);
                qb.push(" AND (code ILIKE ").push_bind(p.clone())
                    .push(" OR zone ILIKE ").push_bind(p.clone())
                    .push(" OR description ILIKE ").push_bind(p.clone())
                    .push(" OR id IN (SELECT trip_group_id FROM duty_repeats WHERE NOT deleted AND gtfs_id = ")
                    .push_bind(gtfs_id.to_string())
                    .push(" AND (vehicle_number ILIKE ").push_bind(p.clone())
                    .push(" OR driver_token_number ILIKE ").push_bind(p.clone())
                    .push(" OR conductor_token_number ILIKE ").push_bind(p)
                    .push(")))");
            }
            for (column, value) in [
                ("vehicle_number", clean(q.vehicle_number.clone())),
                ("driver_token_number", clean(q.driver_token_number.clone())),
                (
                    "conductor_token_number",
                    clean(q.conductor_token_number.clone()),
                ),
            ] {
                if let Some(v) = value {
                    qb.push(" AND id IN (SELECT trip_group_id FROM duty_repeats WHERE NOT deleted AND gtfs_id = ")
                        .push_bind(gtfs_id.to_string())
                        .push(format!(" AND {} = ", column))
                        .push_bind(v)
                        .push(")");
                }
            }
            if let Some(op) = &caller.operator_id {
                qb.push(" AND operator_id = ").push_bind(op.clone());
            }
        };

        let mut count_qb = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM trip_groups");
        filters(&mut count_qb);
        let total: i64 = count_qb
            .build_query_scalar()
            .fetch_one(pool)
            .await
            .map_err(|e| map_db("list_trip_groups (count)", e))?;

        let mut qb = QueryBuilder::<Postgres>::new(
            "SELECT trip_groups.*,
               (SELECT COUNT(*) FROM trips t WHERE t.trip_group_id = trip_groups.id AND NOT t.deleted)
                 AS trip_count,
               (SELECT COUNT(*) FROM duty_repeats r WHERE r.trip_group_id = trip_groups.id
                  AND NOT r.deleted AND r.repeat_status = 'active')
                 AS repeat_count
             FROM trip_groups",
        );
        filters(&mut qb);
        qb.push(" ORDER BY code LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let items = qb
            .build_query_as::<TripGroupListItem>()
            .fetch_all(pool)
            .await
            .map_err(|e| map_db("list_trip_groups", e))?;
        Ok(Paged { items, total })
    }

    /// Soft-deletes the group with its trips and repeat rules. Refused while the group still
    /// has runs today or later.
    pub async fn delete_trip_group(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
    ) -> AppResult<()> {
        caller.require_actor()?;
        self.get_trip_group_for(gtfs_id, caller, id, true).await?;
        let mut tx = self.pool()?.begin().await.map_err(|e| map_db("begin", e))?;
        let exists: Option<String> = sqlx::query_scalar(
            "SELECT id FROM trip_groups WHERE id = $1 AND gtfs_id = $2 AND NOT deleted FOR UPDATE",
        )
        .bind(id)
        .bind(gtfs_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| map_db("delete_trip_group", e))?;
        if exists.is_none() {
            return Err(AppError::NotFound(format!("trip group '{}' not found", id)));
        }
        let has_future_runs: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM duty_groups
                           WHERE trip_group_id = $1 AND NOT deleted AND operation_date >= $2)",
        )
        .bind(id)
        .bind(today_ist())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_db("delete_trip_group (runs)", e))?;
        if has_future_runs {
            return Err(AppError::BadRequest(
                "trip group has runs today or later; delete or deactivate them first".to_string(),
            ));
        }
        for sql in [
            "UPDATE trip_groups SET deleted = true, updated_at = now() WHERE id = $1",
            "UPDATE trips SET deleted = true, updated_at = now() WHERE trip_group_id = $1 AND NOT deleted",
            "UPDATE duty_repeats SET deleted = true, updated_at = now() WHERE trip_group_id = $1 AND NOT deleted",
        ] {
            sqlx::query(sql)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_db("delete_trip_group", e))?;
        }
        tx.commit().await.map_err(|e| map_db("commit", e))
    }

    // ── Trips ───────────────────────────────────────────────────────────────

    pub async fn list_trips(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        trip_group_id: &str,
    ) -> AppResult<Vec<Trip>> {
        self.get_trip_group(gtfs_id, caller, trip_group_id).await?;
        sqlx::query_as::<_, Trip>(
            "SELECT * FROM trips WHERE trip_group_id = $1 AND NOT deleted ORDER BY trip_order",
        )
        .bind(trip_group_id)
        .fetch_all(self.pool()?)
        .await
        .map_err(|e| map_db("list_trips", e))
    }

    /// Upserts some or all trips of a group, then recomputes every trip's day offsets.
    /// Trips not in the request are kept. Runs already created are not changed.
    pub async fn upsert_trips(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        trip_group_id: &str,
        req: UpsertTripsReq,
    ) -> AppResult<Vec<Trip>> {
        caller.require_actor()?;
        let tg = self
            .get_trip_group_for(gtfs_id, caller, trip_group_id, true)
            .await?;
        struct Planned {
            id: String,
            is_new: bool,
            route_id: String,
            trip_number: i32,
            trip_order: i32,
            start: NaiveTime,
            end: NaiveTime,
            is_bookable: bool,
        }

        if let Some(o) = req.first_trip_day_offset {
            if !(0..=1).contains(&o) {
                return Err(AppError::BadRequest(
                    "firstTripDayOffset must be 0 or 1".to_string(),
                ));
            }
        }

        let mut tx = self.pool()?.begin().await.map_err(|e| map_db("begin", e))?;
        let group: Option<String> = sqlx::query_scalar(
            "SELECT id FROM trip_groups WHERE id = $1 AND gtfs_id = $2 AND NOT deleted FOR UPDATE",
        )
        .bind(trip_group_id)
        .bind(gtfs_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| map_db("upsert_trips (group)", e))?;
        if group.is_none() {
            return Err(AppError::NotFound(format!(
                "trip group '{}' not found",
                trip_group_id
            )));
        }

        let existing: Vec<Trip> = sqlx::query_as(
            "SELECT * FROM trips WHERE trip_group_id = $1 AND NOT deleted ORDER BY trip_order",
        )
        .bind(trip_group_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| map_db("upsert_trips (existing)", e))?;
        let current_first_offset = existing
            .first()
            .map(|t| t.scheduled_start_day_offset)
            .unwrap_or(0);

        let mut planned: HashMap<String, Planned> = existing
            .iter()
            .map(|t| {
                (
                    t.id.clone(),
                    Planned {
                        id: t.id.clone(),
                        is_new: false,
                        route_id: t.route_id.clone(),
                        trip_number: t.trip_number,
                        trip_order: t.trip_order,
                        start: t.scheduled_start_time,
                        end: t.scheduled_end_time,
                        is_bookable: t.is_bookable,
                    },
                )
            })
            .collect();

        for input in req.trips {
            let start = parse_hhmm(&input.scheduled_start_time)?;
            let end = parse_hhmm(&input.scheduled_end_time)?;
            if start == end {
                return Err(AppError::BadRequest(format!(
                    "trip {}: start and end time are the same",
                    input.trip_number
                )));
            }
            if input.trip_number <= 0 || input.trip_order <= 0 {
                return Err(AppError::BadRequest(
                    "tripNumber and tripOrder must be positive".to_string(),
                ));
            }
            let route_id = input.route_id.trim().to_string();
            if route_id.is_empty() {
                return Err(AppError::BadRequest(format!(
                    "trip {}: routeId is required",
                    input.trip_number
                )));
            }
            let (id, is_new) = match clean(input.id) {
                Some(id) => {
                    if !planned.contains_key(&id) {
                        return Err(AppError::NotFound(format!(
                            "trip '{}' is not in trip group '{}'",
                            id, trip_group_id
                        )));
                    }
                    (id, false)
                }
                None => (gen_random_id(), true),
            };
            let is_bookable = input
                .is_bookable
                .or_else(|| planned.get(&id).map(|p| p.is_bookable))
                .unwrap_or(true);
            planned.insert(
                id.clone(),
                Planned {
                    id,
                    is_new,
                    route_id,
                    trip_number: input.trip_number,
                    trip_order: input.trip_order,
                    start,
                    end,
                    is_bookable,
                },
            );
        }

        let mut all: Vec<Planned> = planned.into_values().collect();
        all.sort_by_key(|p| p.trip_order);
        let mut numbers = HashSet::new();
        let mut orders = HashSet::new();
        for p in &all {
            if !numbers.insert(p.trip_number) {
                return Err(AppError::BadRequest(format!(
                    "duplicate tripNumber {}",
                    p.trip_number
                )));
            }
            if !orders.insert(p.trip_order) {
                return Err(AppError::BadRequest(format!(
                    "duplicate tripOrder {}",
                    p.trip_order
                )));
            }
        }

        if let Some(first) = all.first() {
            if first.start != tg.first_departure {
                return Err(AppError::BadRequest(format!(
                    "first trip starts at {} but trip group {} departs at {}; change the trips or the group's first departure",
                    first.start.format("%H:%M"),
                    tg.code,
                    tg.first_departure.format("%H:%M")
                )));
            }
        }
        let first_offset = req.first_trip_day_offset.unwrap_or(current_first_offset);
        let times: Vec<(NaiveTime, NaiveTime)> = all.iter().map(|p| (p.start, p.end)).collect();
        let offsets = compute_offsets(&times, first_offset);

        // Park existing trip_numbers out of the way so swaps don't trip the unique index.
        sqlx::query(
            "UPDATE trips SET trip_number = -trip_number WHERE trip_group_id = $1 AND NOT deleted",
        )
        .bind(trip_group_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_db("upsert_trips (park)", e))?;

        for (p, (start_off, end_off)) in all.iter().zip(offsets) {
            let sql = if p.is_new {
                "INSERT INTO trips
                   (id, trip_group_id, gtfs_id, route_id, is_bookable, trip_number, trip_order,
                    scheduled_start_time, scheduled_start_day_offset, scheduled_end_time,
                    scheduled_end_day_offset, operator_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"
            } else {
                "UPDATE trips
                 SET route_id = $4, is_bookable = $5, trip_number = $6, trip_order = $7,
                     scheduled_start_time = $8, scheduled_start_day_offset = $9,
                     scheduled_end_time = $10, scheduled_end_day_offset = $11,
                     operator_id = $12, updated_at = now()
                 WHERE id = $1 AND trip_group_id = $2 AND gtfs_id = $3"
            };
            sqlx::query(sql)
                .bind(&p.id)
                .bind(trip_group_id)
                .bind(gtfs_id)
                .bind(&p.route_id)
                .bind(p.is_bookable)
                .bind(p.trip_number)
                .bind(p.trip_order)
                .bind(p.start)
                .bind(start_off)
                .bind(p.end)
                .bind(end_off)
                .bind(&tg.operator_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_db("upsert_trips (write)", e))?;
        }
        sqlx::query("UPDATE trip_groups SET updated_at = now() WHERE id = $1")
            .bind(trip_group_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_db("upsert_trips (touch)", e))?;
        tx.commit().await.map_err(|e| map_db("commit", e))?;
        self.list_trips(gtfs_id, caller, trip_group_id).await
    }

    /// Soft-deletes one trip and recomputes the group's day offsets.
    pub async fn delete_trip(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        trip_id: &str,
    ) -> AppResult<()> {
        caller.require_actor()?;
        let mut tx = self.pool()?.begin().await.map_err(|e| map_db("begin", e))?;
        let trip: Trip = sqlx::query_as(
            "SELECT * FROM trips WHERE id = $1 AND gtfs_id = $2 AND NOT deleted FOR UPDATE",
        )
        .bind(trip_id)
        .bind(gtfs_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| map_db("delete_trip", e))?
        .ok_or_else(|| AppError::NotFound(format!("trip '{}' not found", trip_id)))?;
        if !caller.can_see(trip.operator_id.as_deref()) {
            return Err(AppError::Forbidden(
                "trip belongs to another operator".to_string(),
            ));
        }
        sqlx::query("UPDATE trips SET deleted = true, updated_at = now() WHERE id = $1")
            .bind(trip_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_db("delete_trip", e))?;

        let rest: Vec<Trip> = sqlx::query_as(
            "SELECT * FROM trips WHERE trip_group_id = $1 AND NOT deleted ORDER BY trip_order",
        )
        .bind(&trip.trip_group_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| map_db("delete_trip (rest)", e))?;
        let first_offset = rest
            .first()
            .map(|t| t.scheduled_start_day_offset)
            .unwrap_or(0);
        let times: Vec<(NaiveTime, NaiveTime)> = rest
            .iter()
            .map(|t| (t.scheduled_start_time, t.scheduled_end_time))
            .collect();
        for (t, (s, e)) in rest.iter().zip(compute_offsets(&times, first_offset)) {
            if t.scheduled_start_day_offset != s || t.scheduled_end_day_offset != e {
                sqlx::query(
                    "UPDATE trips SET scheduled_start_day_offset = $2, scheduled_end_day_offset = $3,
                     updated_at = now() WHERE id = $1",
                )
                .bind(&t.id)
                .bind(s)
                .bind(e)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_db("delete_trip (offsets)", e))?;
            }
        }
        tx.commit().await.map_err(|e| map_db("commit", e))
    }

    // ── Repeat rules ────────────────────────────────────────────────────────

    async fn load_repeat(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
        for_write: bool,
    ) -> AppResult<DutyRepeat> {
        let rule: DutyRepeat = sqlx::query_as(
            "SELECT * FROM duty_repeats WHERE id = $1 AND gtfs_id = $2 AND NOT deleted",
        )
        .bind(id)
        .bind(gtfs_id)
        .fetch_optional(self.pool()?)
        .await
        .map_err(|e| map_db("load_repeat", e))?
        .ok_or_else(|| AppError::NotFound(format!("repeat rule '{}' not found", id)))?;
        if !caller.can_see(rule.operator_id.as_deref()) {
            return Err(if for_write {
                AppError::Forbidden("repeat rule belongs to another operator".to_string())
            } else {
                AppError::NotFound(format!("repeat rule '{}' not found", id))
            });
        }
        Ok(rule)
    }

    /// Saves a rule, then generates today .. today + 7 for it (when active). Edits apply only to
    /// runs generated after the save; runs that already exist keep their values.
    pub async fn upsert_duty_repeat(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        req: UpsertDutyRepeatReq,
    ) -> AppResult<UpsertDutyRepeatResp> {
        let pool = self.pool()?;
        caller.require_actor()?;
        let status = RepeatStatus::parse(req.repeat_status.as_deref().unwrap_or("active"))?;
        let generate_now = req.generate.unwrap_or(true);
        let mut days = req.recurrence_days.clone();
        days.sort_unstable();
        days.dedup();
        if let Some(bad) = days.iter().find(|d| !(1..=7).contains(*d)) {
            return Err(AppError::BadRequest(format!(
                "recurrenceDays must be ISO weekdays 1-7 (1 = Monday), got {}",
                bad
            )));
        }
        if status == RepeatStatus::Active && days.is_empty() {
            return Err(AppError::BadRequest(
                "recurrenceDays needs at least one day for an active rule".to_string(),
            ));
        }
        if let Some(till) = req.effective_till {
            if till < req.effective_from {
                return Err(AppError::BadRequest(
                    "effectiveTill cannot be before effectiveFrom".to_string(),
                ));
            }
        }
        self.get_trip_group_for(gtfs_id, caller, &req.trip_group_id, true)
            .await?;

        let rule: DutyRepeat = match clean(req.id.clone()) {
            Some(id) => {
                self.load_repeat(gtfs_id, caller, &id, true).await?;
                sqlx::query_as(
                    "UPDATE duty_repeats
                     SET trip_group_id = $3, repeat_status = $4, recurrence_days = $5,
                         effective_from = $6, effective_till = $7, vehicle_number = $8,
                         driver_token_number = $9, driver_name = $10,
                         conductor_token_number = $11, conductor_name = $12,
                         generated_till = NULL, updated_at = now()
                     WHERE id = $1 AND gtfs_id = $2
                     RETURNING *",
                )
                .bind(&id)
                .bind(gtfs_id)
                .bind(&req.trip_group_id)
                .bind(status.as_str())
                .bind(&days)
                .bind(req.effective_from)
                .bind(req.effective_till)
                .bind(clean(req.vehicle_number))
                .bind(clean(req.driver_token_number))
                .bind(clean(req.driver_name))
                .bind(clean(req.conductor_token_number))
                .bind(clean(req.conductor_name))
                .fetch_one(pool)
                .await
                .map_err(|e| map_db("upsert_duty_repeat", e))?
            }
            None => sqlx::query_as(
                "INSERT INTO duty_repeats
                   (id, trip_group_id, gtfs_id, operator_id, repeat_status, recurrence_days,
                    effective_from, effective_till, vehicle_number, driver_token_number,
                    driver_name, conductor_token_number, conductor_name)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                 RETURNING *",
            )
            .bind(gen_random_id())
            .bind(&req.trip_group_id)
            .bind(gtfs_id)
            .bind(&caller.operator_id)
            .bind(status.as_str())
            .bind(&days)
            .bind(req.effective_from)
            .bind(req.effective_till)
            .bind(clean(req.vehicle_number))
            .bind(clean(req.driver_token_number))
            .bind(clean(req.driver_name))
            .bind(clean(req.conductor_token_number))
            .bind(clean(req.conductor_name))
            .fetch_one(pool)
            .await
            .map_err(|e| map_db("upsert_duty_repeat", e))?,
        };

        let generated = if status == RepeatStatus::Active && generate_now {
            let today = today_ist();
            match self
                .generate(
                    gtfs_id,
                    today,
                    today + Duration::days(LOOKAHEAD_DAYS),
                    Some(vec![rule.id.clone()]),
                    None,
                    false,
                    GenerationTrigger::RuleSave,
                )
                .await
            {
                Ok(entries) => entries,
                Err(e) => {
                    error!(
                        "upsert_duty_repeat: generation for {} failed: {}",
                        rule.id, e
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        Ok(UpsertDutyRepeatResp {
            duty_repeat: rule,
            generated,
        })
    }

    pub async fn list_duty_repeats(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        q: DutyRepeatListQuery,
    ) -> AppResult<Paged<DutyRepeat>> {
        let pool = self.pool()?;
        let (limit, offset) = page(q.limit, q.offset);
        let filters = |qb: &mut QueryBuilder<'_, Postgres>| {
            qb.push(" WHERE gtfs_id = ")
                .push_bind(gtfs_id.to_string())
                .push(" AND NOT deleted");
            if let Some(g) = clean(q.trip_group_id.clone()) {
                qb.push(" AND trip_group_id = ").push_bind(g);
            }
            if let Some(c) = clean(q.code.clone()) {
                qb.push(" AND trip_group_id IN (SELECT id FROM trip_groups WHERE NOT deleted AND gtfs_id = ")
                    .push_bind(gtfs_id.to_string())
                    .push(" AND code ILIKE ")
                    .push_bind(format!("%{}%", escape_like(&c)))
                    .push(")");
            }
            for (column, value) in [
                ("vehicle_number", clean(q.vehicle_number.clone())),
                ("driver_token_number", clean(q.driver_token_number.clone())),
                (
                    "conductor_token_number",
                    clean(q.conductor_token_number.clone()),
                ),
            ] {
                if let Some(v) = value {
                    qb.push(format!(" AND {} = ", column)).push_bind(v);
                }
            }
            if let Some(text) = clean(q.search.clone()) {
                let p = contains(&text);
                qb.push(" AND (vehicle_number ILIKE ").push_bind(p.clone())
                    .push(" OR driver_token_number ILIKE ").push_bind(p.clone())
                    .push(" OR conductor_token_number ILIKE ").push_bind(p.clone())
                    .push(" OR trip_group_id IN (SELECT id FROM trip_groups WHERE NOT deleted AND gtfs_id = ")
                    .push_bind(gtfs_id.to_string())
                    .push(" AND code ILIKE ").push_bind(p)
                    .push("))");
            }
            if let Some(st) = clean(q.repeat_status.clone()) {
                qb.push(" AND repeat_status = ")
                    .push_bind(st.to_lowercase());
            }
            if let Some(op) = &caller.operator_id {
                qb.push(" AND operator_id = ").push_bind(op.clone());
            }
        };
        let mut count_qb = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM duty_repeats");
        filters(&mut count_qb);
        let total: i64 = count_qb
            .build_query_scalar()
            .fetch_one(pool)
            .await
            .map_err(|e| map_db("list_duty_repeats (count)", e))?;
        let mut qb = QueryBuilder::<Postgres>::new("SELECT * FROM duty_repeats");
        filters(&mut qb);
        qb.push(" ORDER BY created_at DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let items = qb
            .build_query_as::<DutyRepeat>()
            .fetch_all(pool)
            .await
            .map_err(|e| map_db("list_duty_repeats", e))?;
        Ok(Paged { items, total })
    }

    /// Soft-deletes a rule. Runs it already generated stay.
    pub async fn delete_duty_repeat(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
    ) -> AppResult<()> {
        caller.require_actor()?;
        self.load_repeat(gtfs_id, caller, id, true).await?;
        sqlx::query("UPDATE duty_repeats SET deleted = true, updated_at = now() WHERE id = $1")
            .bind(id)
            .execute(self.pool()?)
            .await
            .map_err(|e| map_db("delete_duty_repeat", e))?;
        Ok(())
    }

    // ── Generation ──────────────────────────────────────────────────────────

    fn rule_verdict(rule: &DutyRepeat, date: NaiveDate) -> Option<&'static str> {
        if rule.deleted || rule.repeat_status != RepeatStatus::Active.as_str() {
            return Some("out_of_window");
        }
        if date < rule.effective_from || rule.effective_till.is_some_and(|t| date > t) {
            return Some("out_of_window");
        }
        let weekday = i16::try_from(date.weekday().number_from_monday()).unwrap_or(0);
        if !rule.recurrence_days.contains(&weekday) {
            return Some("off_day");
        }
        None
    }

    /// Why a rule should not get a run on `date` even though it has none of its own:
    /// `covered` = a live manual run (no rule) already operates the trip group that day;
    /// `past` = every trip of that day has already ended.
    async fn date_skip(
        &self,
        gtfs_id: &str,
        rule: &DutyRepeat,
        date: NaiveDate,
    ) -> AppResult<Option<(&'static str, Option<(String, String)>)>> {
        let pool = self.pool()?;
        let manual: Option<(String, String)> = sqlx::query_as(
            "SELECT id, waybill_no FROM duty_groups
             WHERE gtfs_id = $1 AND trip_group_id = $2 AND operation_date = $3
               AND duty_repeat_id IS NULL AND NOT deleted
             ORDER BY created_at LIMIT 1",
        )
        .bind(gtfs_id)
        .bind(&rule.trip_group_id)
        .bind(date)
        .fetch_optional(pool)
        .await
        .map_err(|e| map_db("date_skip (manual)", e))?;
        if let Some(dg) = manual {
            return Ok(Some(("covered", Some(dg))));
        }
        if date <= today_ist() {
            let ends: Vec<(NaiveTime, i16)> = sqlx::query_as(
                "SELECT scheduled_end_time, scheduled_end_day_offset FROM trips
                 WHERE trip_group_id = $1 AND NOT deleted",
            )
            .bind(&rule.trip_group_id)
            .fetch_all(pool)
            .await
            .map_err(|e| map_db("date_skip (trips)", e))?;
            let last_end = ends.iter().map(|(t, off)| to_instant(date, *off, *t)).max();
            if last_end.is_some_and(|end| end <= Utc::now()) {
                return Ok(Some(("past", None)));
            }
        }
        Ok(None)
    }

    /// Generates runs for active rules over `[from, to]`. Idempotent: (rule, date) pairs that
    /// already have a run — including cancelled or deleted ones — are skipped.
    #[allow(clippy::too_many_arguments)]
    pub async fn generate(
        &self,
        gtfs_id: &str,
        from: NaiveDate,
        to: NaiveDate,
        duty_repeat_ids: Option<Vec<String>>,
        operator_id: Option<String>,
        dry_run: bool,
        trigger: GenerationTrigger,
    ) -> AppResult<Vec<GenerateEntry>> {
        let pool = self.pool()?;
        if to < from {
            return Err(AppError::BadRequest("to cannot be before from".to_string()));
        }
        if (to - from).num_days() > MAX_GENERATE_RANGE_DAYS {
            return Err(AppError::BadRequest(format!(
                "range is limited to {} days",
                MAX_GENERATE_RANGE_DAYS
            )));
        }

        let mut qb = QueryBuilder::<Postgres>::new(
            "SELECT * FROM duty_repeats WHERE NOT deleted AND repeat_status = 'active' AND gtfs_id = ",
        );
        qb.push_bind(gtfs_id.to_string());
        if let Some(ids) = &duty_repeat_ids {
            qb.push(" AND id = ANY(").push_bind(ids.clone()).push(")");
        }
        if let Some(op) = &operator_id {
            qb.push(" AND operator_id = ").push_bind(op.clone());
        }
        let rules: Vec<DutyRepeat> = qb
            .build_query_as()
            .fetch_all(pool)
            .await
            .map_err(|e| map_db("generate (rules)", e))?;
        if rules.is_empty() {
            return Ok(Vec::new());
        }

        let rule_ids: Vec<String> = rules.iter().map(|r| r.id.clone()).collect();
        let existing_rows: Vec<(String, NaiveDate, String, String)> = sqlx::query_as(
            "SELECT duty_repeat_id, operation_date, id, waybill_no FROM duty_groups
             WHERE duty_repeat_id = ANY($1) AND operation_date BETWEEN $2 AND $3",
        )
        .bind(&rule_ids)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await
        .map_err(|e| map_db("generate (existing)", e))?;
        let existing: HashMap<(String, NaiveDate), (String, String)> = existing_rows
            .into_iter()
            .map(|(r, d, id, wb)| ((r, d), (id, wb)))
            .collect();

        let mut entries = Vec::new();
        let mut tasks: Vec<(DutyRepeat, NaiveDate)> = Vec::new();
        for rule in &rules {
            let mut date = from;
            while date <= to {
                let entry = |verdict: &str, dg: Option<&(String, String)>| GenerateEntry {
                    duty_repeat_id: rule.id.clone(),
                    operation_date: date,
                    verdict: verdict.to_string(),
                    duty_group_id: dg.map(|x| x.0.clone()),
                    waybill_no: dg.map(|x| x.1.clone()),
                    error: None,
                };
                if let Some(v) = Self::rule_verdict(rule, date) {
                    entries.push(entry(v, None));
                } else if let Some(dg) = existing.get(&(rule.id.clone(), date)) {
                    entries.push(entry("exists", Some(dg)));
                } else if let Some((v, dg)) = self.date_skip(gtfs_id, rule, date).await? {
                    entries.push(entry(v, dg.as_ref()));
                } else if dry_run {
                    entries.push(entry("ok", None));
                } else {
                    tasks.push((rule.clone(), date));
                }
                date += Duration::days(1);
            }
        }

        let created: Vec<GenerateEntry> = stream::iter(tasks)
            .map(|(rule, date)| async move {
                self.create_run_for_repeat(gtfs_id, &rule, date, trigger)
                    .await
            })
            .buffer_unordered(GENERATION_CONCURRENCY)
            .collect()
            .await;

        if !dry_run {
            let failed: HashSet<&str> = created
                .iter()
                .filter(|e| e.verdict == "failed")
                .map(|e| e.duty_repeat_id.as_str())
                .collect();
            let done: Vec<String> = rule_ids
                .iter()
                .filter(|id| !failed.contains(id.as_str()))
                .cloned()
                .collect();
            if !done.is_empty() {
                if let Err(e) = sqlx::query(
                    "UPDATE duty_repeats
                     SET generated_till = GREATEST(COALESCE(generated_till, $2), $2)
                     WHERE id = ANY($1)",
                )
                .bind(&done)
                .bind(to)
                .execute(pool)
                .await
                {
                    warn!("generate: could not update generated_till: {}", e);
                }
            }
        }

        entries.extend(created);
        entries.sort_by(|a, b| {
            (a.duty_repeat_id.as_str(), a.operation_date)
                .cmp(&(b.duty_repeat_id.as_str(), b.operation_date))
        });
        Ok(entries)
    }

    /// Creates one run for a rule and date. A vehicle / crew clash is retried with the clashing
    /// field cleared (run shows as unassigned); every clash and failure is logged as
    /// GENERATION_FAILURE.
    async fn create_run_for_repeat(
        &self,
        gtfs_id: &str,
        rule: &DutyRepeat,
        date: NaiveDate,
        trigger: GenerationTrigger,
    ) -> GenerateEntry {
        let mut input = RunInput {
            trip_group_id: rule.trip_group_id.clone(),
            operation_date: date,
            duty_repeat_id: Some(rule.id.clone()),
            operator_id: rule.operator_id.clone(),
            vehicle_number: rule.vehicle_number.clone(),
            driver_token_number: rule.driver_token_number.clone(),
            driver_name: rule.driver_name.clone(),
            conductor_token_number: rule.conductor_token_number.clone(),
            conductor_name: rule.conductor_name.clone(),
        };
        let mut cleared: Vec<&'static str> = Vec::new();
        let mut entry = GenerateEntry {
            duty_repeat_id: rule.id.clone(),
            operation_date: date,
            verdict: "failed".to_string(),
            duty_group_id: None,
            waybill_no: None,
            error: None,
        };

        for _ in 0..4 {
            match self.insert_run(gtfs_id, &input).await {
                Ok(Some(dg)) => {
                    entry.verdict = if cleared.is_empty() {
                        "created".to_string()
                    } else {
                        "created_partial".to_string()
                    };
                    entry.duty_group_id = Some(dg.id);
                    entry.waybill_no = Some(dg.waybill_no);
                    if !cleared.is_empty() {
                        entry.error = Some(format!("unassigned: {}", cleared.join(", ")));
                    }
                    return entry;
                }
                Ok(None) => {
                    entry.verdict = "exists".to_string();
                    return entry;
                }
                Err(InsertRunError::Overlap(constraint)) => {
                    let field = match constraint.as_str() {
                        "dg_vehicle_no_overlap" if input.vehicle_number.is_some() => {
                            input.vehicle_number = None;
                            Some("vehicle")
                        }
                        "duties_driver_no_overlap" if input.driver_token_number.is_some() => {
                            input.driver_token_number = None;
                            input.driver_name = None;
                            Some("driver")
                        }
                        "duties_conductor_no_overlap" if input.conductor_token_number.is_some() => {
                            input.conductor_token_number = None;
                            input.conductor_name = None;
                            Some("conductor")
                        }
                        _ => None,
                    };
                    let message = overlap_message(&constraint);
                    self.log_generation_failure(gtfs_id, rule, date, trigger, "23P01", &message)
                        .await;
                    match field {
                        Some(f) => cleared.push(f),
                        None => {
                            entry.error = Some(message);
                            return entry;
                        }
                    }
                }
                Err(InsertRunError::App(e)) => {
                    let message = e.to_string();
                    self.log_generation_failure(gtfs_id, rule, date, trigger, "", &message)
                        .await;
                    entry.error = Some(message);
                    return entry;
                }
            }
        }
        entry.error = Some("gave up after repeated clashes".to_string());
        entry
    }

    async fn log_generation_failure(
        &self,
        gtfs_id: &str,
        rule: &DutyRepeat,
        date: NaiveDate,
        trigger: GenerationTrigger,
        code: &str,
        message: &str,
    ) {
        let result = async {
            let mut conn = self
                .pool()?
                .acquire()
                .await
                .map_err(|e| map_db("acquire", e))?;
            log_event(
                &mut conn,
                NewEvent {
                    gtfs_id,
                    operator_id: rule.operator_id.as_deref(),
                    event_type: DutyEventType::GenerationFailure,
                    duty_group_id: None,
                    duty_id: None,
                    duty_repeat_id: Some(&rule.id),
                    operation_date: Some(date),
                    actor_person_id: None,
                    trigger,
                    old_value: None,
                    new_value: None,
                    reason: None,
                    error_code: Some(code.to_string()).filter(|c| !c.is_empty()),
                    error_message: Some(message.to_string()),
                },
            )
            .await
        }
        .await;
        if let Err(e) = result {
            error!(
                "could not log generation failure for rule {} on {}: {} (original: {})",
                rule.id, date, e, message
            );
        }
    }

    /// Creates run `date + 7` for the rule of a run that just finished. Best-effort; the cron
    /// fills anything this misses.
    pub async fn generate_after_run_finish(&self, gtfs_id: &str, run: &DutyGroup) {
        let Some(rule_id) = run.duty_repeat_id.as_deref() else {
            return;
        };
        let date = run.operation_date + Duration::days(7);
        let result: AppResult<Option<GenerateEntry>> = async {
            let rule: Option<DutyRepeat> = sqlx::query_as(
                "SELECT * FROM duty_repeats WHERE id = $1 AND NOT deleted AND repeat_status = 'active'",
            )
            .bind(rule_id)
            .fetch_optional(self.pool()?)
            .await
            .map_err(|e| map_db("generate_after_run_finish", e))?;
            let Some(rule) = rule else { return Ok(None) };
            if Self::rule_verdict(&rule, date).is_some()
                || self.date_skip(gtfs_id, &rule, date).await?.is_some()
            {
                return Ok(None);
            }
            Ok(Some(
                self.create_run_for_repeat(gtfs_id, &rule, date, GenerationTrigger::RunFinish)
                    .await,
            ))
        }
        .await;
        match result {
            Ok(Some(e)) if e.verdict.starts_with("created") => {
                info!(
                    "run finish: created {} for rule {} on {}",
                    e.waybill_no.unwrap_or_default(),
                    rule_id,
                    date
                )
            }
            Ok(_) => {}
            Err(e) => error!(
                "run finish generation for rule {} on {} failed: {}",
                rule_id, date, e
            ),
        }
    }

    /// Header + one duty per trip, in one transaction. `Ok(None)` = the (rule, date) run exists.
    pub(crate) async fn insert_run(
        &self,
        gtfs_id: &str,
        input: &RunInput,
    ) -> Result<Option<DutyGroup>, InsertRunError> {
        let pool = self.pool()?;
        let mut tx = pool.begin().await.map_err(|e| classify("begin", e))?;

        let group: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT depot_id, service_type_id FROM trip_groups
             WHERE id = $1 AND gtfs_id = $2 AND NOT deleted",
        )
        .bind(&input.trip_group_id)
        .bind(gtfs_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| classify("insert_run (group)", e))?;
        let Some((depot_id, group_service_type)) = group else {
            return Err(AppError::NotFound(format!(
                "trip group '{}' not found",
                input.trip_group_id
            ))
            .into());
        };
        let service_type_id = match &input.vehicle_number {
            Some(v) => bus_service_type(&mut tx, gtfs_id, v)
                .await
                .map_err(|e| classify("insert_run (bus service type)", e))?
                .or(group_service_type),
            None => group_service_type,
        };

        let trips: Vec<Trip> = sqlx::query_as(
            "SELECT * FROM trips WHERE trip_group_id = $1 AND NOT deleted ORDER BY trip_order",
        )
        .bind(&input.trip_group_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| classify("insert_run (trips)", e))?;
        if trips.is_empty() {
            return Err(AppError::BadRequest("trip group has no trips".to_string()).into());
        }

        let starts: Vec<DateTime<Utc>> = trips
            .iter()
            .map(|t| {
                to_instant(
                    input.operation_date,
                    t.scheduled_start_day_offset,
                    t.scheduled_start_time,
                )
            })
            .collect();
        let ends: Vec<DateTime<Utc>> = trips
            .iter()
            .map(|t| {
                to_instant(
                    input.operation_date,
                    t.scheduled_end_day_offset,
                    t.scheduled_end_time,
                )
            })
            .collect();
        let window_start = *starts.iter().min().expect("non-empty");
        let window_end = *ends.iter().max().expect("non-empty");

        let dg: Option<DutyGroup> = sqlx::query_as(
            "INSERT INTO duty_groups
               (id, waybill_no, trip_group_id, duty_repeat_id, gtfs_id, operator_id,
                operation_date, depot_id, vehicle_number, driver_token_number, driver_name,
                conductor_token_number, conductor_name, window_start_at, window_end_at,
                service_type_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             ON CONFLICT (duty_repeat_id, operation_date) DO NOTHING
             RETURNING *",
        )
        .bind(gen_random_id())
        .bind(generate_waybill_number())
        .bind(&input.trip_group_id)
        .bind(&input.duty_repeat_id)
        .bind(gtfs_id)
        .bind(&input.operator_id)
        .bind(input.operation_date)
        .bind(&depot_id)
        .bind(&input.vehicle_number)
        .bind(&input.driver_token_number)
        .bind(&input.driver_name)
        .bind(&input.conductor_token_number)
        .bind(&input.conductor_name)
        .bind(window_start)
        .bind(window_end)
        .bind(&service_type_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| classify("insert_run (header)", e))?;
        let Some(dg) = dg else {
            return Ok(None);
        };

        let ids: Vec<String> = trips.iter().map(|_| gen_random_id()).collect();
        let trip_ids: Vec<String> = trips.iter().map(|t| t.id.clone()).collect();
        let route_ids: Vec<String> = trips.iter().map(|t| t.route_id.clone()).collect();
        let bookable: Vec<bool> = trips.iter().map(|t| t.is_bookable).collect();
        let numbers: Vec<i32> = trips.iter().map(|t| t.trip_number).collect();
        let orders: Vec<i32> = trips.iter().map(|t| t.trip_order).collect();
        sqlx::query(
            "INSERT INTO duties
               (id, duty_group_id, trip_id, gtfs_id, route_id, is_bookable, trip_number,
                trip_order, scheduled_start_at, scheduled_end_at, driver_token_number,
                driver_name, conductor_token_number, conductor_name, operator_id)
             SELECT u.id, $1, u.trip_id, $2, u.route_id, u.is_bookable, u.trip_number,
                    u.trip_order, u.s, u.e, $3, $4, $5, $6, $15
             FROM UNNEST($7::text[], $8::text[], $9::text[], $10::bool[], $11::int4[],
                         $12::int4[], $13::timestamptz[], $14::timestamptz[])
                  AS u(id, trip_id, route_id, is_bookable, trip_number, trip_order, s, e)",
        )
        .bind(&dg.id)
        .bind(gtfs_id)
        .bind(&input.driver_token_number)
        .bind(&input.driver_name)
        .bind(&input.conductor_token_number)
        .bind(&input.conductor_name)
        .bind(&ids)
        .bind(&trip_ids)
        .bind(&route_ids)
        .bind(&bookable)
        .bind(&numbers)
        .bind(&orders)
        .bind(&starts)
        .bind(&ends)
        .bind(&dg.operator_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| classify("insert_run (duties)", e))?;

        // Overlap constraints are deferred: they fire here.
        tx.commit()
            .await
            .map_err(|e| classify("insert_run (commit)", e))?;
        Ok(Some(dg))
    }

    // ── Runs (duty groups) ──────────────────────────────────────────────────

    pub async fn load_run(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
        for_write: bool,
    ) -> AppResult<DutyGroup> {
        let dg: DutyGroup = sqlx::query_as(
            "SELECT * FROM duty_groups WHERE id = $1 AND gtfs_id = $2 AND NOT deleted",
        )
        .bind(id)
        .bind(gtfs_id)
        .fetch_optional(self.pool()?)
        .await
        .map_err(|e| map_db("load_run", e))?
        .ok_or_else(|| AppError::NotFound(format!("run '{}' not found", id)))?;
        if !caller.can_see(dg.operator_id.as_deref()) {
            return Err(if for_write {
                AppError::Forbidden("run belongs to another operator".to_string())
            } else {
                AppError::NotFound(format!("run '{}' not found", id))
            });
        }
        Ok(dg)
    }

    /// Locks the run row inside `tx` after the ownership check.
    async fn lock_run(
        tx: &mut PgConnection,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
    ) -> AppResult<DutyGroup> {
        let dg: DutyGroup = sqlx::query_as(
            "SELECT * FROM duty_groups WHERE id = $1 AND gtfs_id = $2 AND NOT deleted FOR UPDATE",
        )
        .bind(id)
        .bind(gtfs_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| map_db("lock_run", e))?
        .ok_or_else(|| AppError::NotFound(format!("run '{}' not found", id)))?;
        if !caller.can_see(dg.operator_id.as_deref()) {
            return Err(AppError::Forbidden(
                "run belongs to another operator".to_string(),
            ));
        }
        Ok(dg)
    }

    pub async fn create_duty_group(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        req: CreateDutyGroupReq,
    ) -> AppResult<DutyGroupDetail> {
        caller.require_actor()?;
        self.get_trip_group_for(gtfs_id, caller, &req.trip_group_id, true)
            .await?;
        let driver = clean(req.driver_token_number);
        let conductor = clean(req.conductor_token_number);
        let input = RunInput {
            trip_group_id: req.trip_group_id,
            operation_date: req.operation_date,
            duty_repeat_id: None,
            operator_id: caller.operator_id.clone(),
            vehicle_number: clean(req.vehicle_number),
            driver_name: driver.as_ref().and(clean(req.driver_name)),
            driver_token_number: driver,
            conductor_name: conductor.as_ref().and(clean(req.conductor_name)),
            conductor_token_number: conductor,
        };
        let dg = match self.insert_run(gtfs_id, &input).await {
            Ok(Some(dg)) => dg,
            Ok(None) => return Err(AppError::Internal("run was not created".to_string())),
            Err(InsertRunError::Overlap(c)) => return Err(AppError::Conflict(overlap_message(&c))),
            Err(InsertRunError::App(e)) => return Err(e),
        };
        self.get_duty_group_detail(gtfs_id, caller, &dg.id).await
    }

    pub async fn get_duty_group_detail(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
    ) -> AppResult<DutyGroupDetail> {
        let dg = self.load_run(gtfs_id, caller, id, false).await?;
        let mut conn = self
            .pool()?
            .acquire()
            .await
            .map_err(|e| map_db("acquire", e))?;
        let duties =
            with_duty_trip_ids(&dg.waybill_no, load_duties(&mut conn, &dg.id, false).await?);
        let code = trip_group_code(&mut conn, &dg.trip_group_id).await?;
        Ok(DutyGroupDetail {
            duty_group: dg,
            trip_group_code: code,
            duties,
        })
    }

    pub async fn list_duties(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
    ) -> AppResult<Vec<Duty>> {
        let dg = self.load_run(gtfs_id, caller, id, false).await?;
        let mut conn = self
            .pool()?
            .acquire()
            .await
            .map_err(|e| map_db("acquire", e))?;
        Ok(with_duty_trip_ids(
            &dg.waybill_no,
            load_duties(&mut conn, &dg.id, false).await?,
        ))
    }

    pub async fn list_duty_groups(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        q: DutyGroupListQuery,
    ) -> AppResult<Paged<DutyGroupListItem>> {
        let pool = self.pool()?;
        let (limit, offset) = page(q.limit, q.offset);
        let filters = |qb: &mut QueryBuilder<'_, Postgres>| {
            qb.push(" WHERE dg.gtfs_id = ")
                .push_bind(gtfs_id.to_string())
                .push(" AND NOT dg.deleted");
            if let Some(op) = &caller.operator_id {
                qb.push(" AND dg.operator_id = ").push_bind(op.clone());
            }
            if let Some(d) = q.operation_date {
                qb.push(" AND dg.operation_date = ").push_bind(d);
            }
            if let Some(g) = clean(q.trip_group_id.clone()) {
                qb.push(" AND dg.trip_group_id = ").push_bind(g);
            }
            if let Some(c) = clean(q.code.clone()) {
                qb.push(" AND (dg.waybill_no = ")
                    .push_bind(c.clone())
                    .push(" OR dg.trip_group_id IN (SELECT id FROM trip_groups WHERE NOT deleted AND gtfs_id = ")
                    .push_bind(gtfs_id.to_string())
                    .push(" AND code ILIKE ")
                    .push_bind(format!("%{}%", escape_like(&c)))
                    .push("))");
            }
            if let Some(text) = clean(q.search.clone()) {
                let p = contains(&text);
                qb.push(" AND (dg.waybill_no ILIKE ").push_bind(p.clone())
                    .push(" OR dg.vehicle_number ILIKE ").push_bind(p.clone())
                    .push(" OR dg.driver_token_number ILIKE ").push_bind(p.clone())
                    .push(" OR dg.conductor_token_number ILIKE ").push_bind(p.clone())
                    .push(" OR dg.trip_group_id IN (SELECT id FROM trip_groups WHERE NOT deleted AND gtfs_id = ")
                    .push_bind(gtfs_id.to_string())
                    .push(" AND code ILIKE ").push_bind(p.clone())
                    .push(") OR dg.id IN (SELECT duty_group_id FROM duties WHERE NOT deleted AND gtfs_id = ")
                    .push_bind(gtfs_id.to_string())
                    .push(" AND (driver_token_number ILIKE ").push_bind(p.clone())
                    .push(" OR conductor_token_number ILIKE ").push_bind(p)
                    .push(")))");
            }
            if let Some(d) = clean(q.depot_id.clone()) {
                qb.push(" AND dg.depot_id = ").push_bind(d);
            }
            if let Some(v) = clean(q.vehicle_number.clone()) {
                qb.push(" AND dg.vehicle_number = ").push_bind(v);
            }
            if let Some(a) = q.is_active {
                qb.push(" AND dg.is_active = ").push_bind(a);
            }
            for (column, value) in [
                ("driver_token_number", clean(q.driver_token_number.clone())),
                (
                    "conductor_token_number",
                    clean(q.conductor_token_number.clone()),
                ),
            ] {
                if let Some(v) = value {
                    // the run's default crew, or any of its trips' crew (per-trip overrides)
                    qb.push(format!(" AND (dg.{} = ", column))
                        .push_bind(v.clone())
                        .push(" OR dg.id IN (SELECT duty_group_id FROM duties WHERE NOT deleted AND gtfs_id = ")
                        .push_bind(gtfs_id.to_string())
                        .push(format!(" AND {} = ", column))
                        .push_bind(v)
                        .push("))");
                }
            }
        };

        let mut count_qb = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM duty_groups dg");
        filters(&mut count_qb);
        let total: i64 = count_qb
            .build_query_scalar()
            .fetch_one(pool)
            .await
            .map_err(|e| map_db("list_duty_groups (count)", e))?;

        let mut qb = QueryBuilder::<Postgres>::new(
            "SELECT dg.*, tg.code AS trip_group_code,
               (SELECT COUNT(*) FROM duties d WHERE d.duty_group_id = dg.id AND NOT d.deleted)
                 AS total_trips,
               (SELECT COUNT(*) FROM duties d WHERE d.duty_group_id = dg.id AND NOT d.deleted
                  AND d.status IN ('upcoming', 'active'))
                 AS pending_trips,
               (SELECT COUNT(*) FROM duties d WHERE d.duty_group_id = dg.id AND NOT d.deleted
                  AND d.status <> 'cancelled'
                  AND (dg.vehicle_number IS NULL
                       OR COALESCE(d.driver_token_number, dg.driver_token_number) IS NULL))
                 AS unassigned_trips,
               (SELECT d.trip_number FROM duties d WHERE d.duty_group_id = dg.id AND NOT d.deleted
                  AND d.status = 'active' LIMIT 1)
                 AS running_trip_number
             FROM duty_groups dg JOIN trip_groups tg ON tg.id = dg.trip_group_id",
        );
        filters(&mut qb);
        qb.push(" ORDER BY dg.operation_date DESC, dg.window_start_at LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let items = qb
            .build_query_as::<DutyGroupListItem>()
            .fetch_all(pool)
            .await
            .map_err(|e| map_db("list_duty_groups", e))?;
        Ok(Paged { items, total })
    }

    /// Run-level bus change. Started trips keep their `recorded_vehicle_number`.
    pub async fn update_run_vehicle(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
        vehicle_number: Option<String>,
    ) -> AppResult<DutyGroup> {
        let actor = caller.require_actor()?.to_string();
        let new_vehicle = clean(vehicle_number);
        let mut tx = self.pool()?.begin().await.map_err(|e| map_db("begin", e))?;
        let dg = Self::lock_run(&mut tx, gtfs_id, caller, id).await?;
        if dg.vehicle_number == new_vehicle {
            return Ok(dg);
        }
        let bus_type = match &new_vehicle {
            Some(v) => bus_service_type(&mut tx, gtfs_id, v)
                .await
                .map_err(|e| map_db("update_run_vehicle (service type)", e))?,
            None => None,
        };
        let updated: DutyGroup = sqlx::query_as(
            "UPDATE duty_groups
             SET vehicle_number = $2,
                 service_type_id = COALESCE($3,
                   (SELECT service_type_id FROM trip_groups WHERE id = duty_groups.trip_group_id)),
                 updated_at = now()
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(&new_vehicle)
        .bind(&bus_type)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_db("update_run_vehicle", e))?;
        log_event(
            &mut tx,
            NewEvent {
                gtfs_id,
                operator_id: dg.operator_id.as_deref(),
                event_type: DutyEventType::VehicleChange,
                duty_group_id: Some(id),
                duty_id: None,
                duty_repeat_id: dg.duty_repeat_id.as_deref(),
                operation_date: Some(dg.operation_date),
                actor_person_id: Some(&actor),
                trigger: GenerationTrigger::Api,
                old_value: Some(json!({ "vehicleNumber": dg.vehicle_number, "serviceTypeId": dg.service_type_id })),
                new_value: Some(json!({ "vehicleNumber": new_vehicle, "serviceTypeId": updated.service_type_id })),
                reason: None,
                error_code: None,
                error_message: None,
            },
        )
        .await?;
        tx.commit().await.map_err(|e| map_db("commit", e))?;
        Ok(updated)
    }

    /// Run-level crew change: updates the run default and every not-started trip that still
    /// has the old default (individually swapped trips keep their swap).
    pub async fn update_run_crew(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
        req: UpdateCrewReq,
    ) -> AppResult<DutyGroupDetail> {
        let actor = caller.require_actor()?.to_string();
        let mut tx = self.pool()?.begin().await.map_err(|e| map_db("begin", e))?;
        let dg = Self::lock_run(&mut tx, gtfs_id, caller, id).await?;

        let slots = [
            (
                "driver",
                crew_change(
                    dg.driver_token_number.clone(),
                    dg.driver_name.clone(),
                    &req.driver_token_number,
                    &req.driver_name,
                ),
            ),
            (
                "conductor",
                crew_change(
                    dg.conductor_token_number.clone(),
                    dg.conductor_name.clone(),
                    &req.conductor_token_number,
                    &req.conductor_name,
                ),
            ),
        ];
        let mut trips_updated: u64 = 0;
        let mut changed = false;
        for (slot, change) in &slots {
            let Some(c) = change else { continue };
            changed = true;
            sqlx::query(&format!(
                "UPDATE duty_groups SET {s}_token_number = $2, {s}_name = $3, updated_at = now()
                 WHERE id = $1",
                s = slot
            ))
            .bind(id)
            .bind(&c.new_token)
            .bind(&c.new_name)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_db("update_run_crew (header)", e))?;
            trips_updated += sqlx::query(&format!(
                "UPDATE duties SET {s}_token_number = $2, {s}_name = $3, updated_at = now()
                 WHERE duty_group_id = $1 AND NOT deleted AND recorded_start_time IS NULL
                   AND status = 'upcoming'
                   AND {s}_token_number IS NOT DISTINCT FROM $4",
                s = slot
            ))
            .bind(id)
            .bind(&c.new_token)
            .bind(&c.new_name)
            .bind(&c.old_token)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_db("update_run_crew (trips)", e))?
            .rows_affected();
        }
        if changed {
            log_event(
                &mut tx,
                NewEvent {
                    gtfs_id,
                    operator_id: dg.operator_id.as_deref(),
                    event_type: DutyEventType::CrewChange,
                    duty_group_id: Some(id),
                    duty_id: None,
                    duty_repeat_id: dg.duty_repeat_id.as_deref(),
                    operation_date: Some(dg.operation_date),
                    actor_person_id: Some(&actor),
                    trigger: GenerationTrigger::Api,
                    old_value: Some(json!({
                        "scope": "run",
                        "driverTokenNumber": dg.driver_token_number,
                        "driverName": dg.driver_name,
                        "conductorTokenNumber": dg.conductor_token_number,
                        "conductorName": dg.conductor_name,
                    })),
                    new_value: Some(json!({
                        "scope": "run",
                        "driverTokenNumber": slots[0].1.as_ref().map(|c| c.new_token.clone()).unwrap_or(dg.driver_token_number.clone()),
                        "driverName": slots[0].1.as_ref().map(|c| c.new_name.clone()).unwrap_or(dg.driver_name.clone()),
                        "conductorTokenNumber": slots[1].1.as_ref().map(|c| c.new_token.clone()).unwrap_or(dg.conductor_token_number.clone()),
                        "conductorName": slots[1].1.as_ref().map(|c| c.new_name.clone()).unwrap_or(dg.conductor_name.clone()),
                        "tripsUpdated": trips_updated,
                    })),
                    reason: None,
                    error_code: None,
                    error_message: None,
                },
            )
            .await?;
        }
        tx.commit().await.map_err(|e| map_db("commit", e))?;
        self.get_duty_group_detail(gtfs_id, caller, id).await
    }

    /// Activates / deactivates a whole run (and its trips' `run_active` mirror).
    pub async fn set_run_active(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
        is_active: bool,
    ) -> AppResult<DutyGroup> {
        let actor = caller.require_actor()?.to_string();
        let mut tx = self.pool()?.begin().await.map_err(|e| map_db("begin", e))?;
        let dg = Self::lock_run(&mut tx, gtfs_id, caller, id).await?;
        if dg.is_active == is_active {
            return Ok(dg);
        }
        if !is_active {
            let running: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM duties WHERE duty_group_id = $1 AND status = 'active' AND NOT deleted)",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| map_db("set_run_active", e))?;
            if running {
                return Err(AppError::BadRequest(
                    "a trip of this run is running; end it before deactivating".to_string(),
                ));
            }
        }
        let updated: DutyGroup = sqlx::query_as(
            "UPDATE duty_groups SET is_active = $2, updated_at = now() WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(is_active)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_db("set_run_active", e))?;
        sqlx::query(
            "UPDATE duties SET run_active = $2, updated_at = now() WHERE duty_group_id = $1",
        )
        .bind(id)
        .bind(is_active)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_db("set_run_active (trips)", e))?;
        log_event(
            &mut tx,
            NewEvent {
                gtfs_id,
                operator_id: dg.operator_id.as_deref(),
                event_type: DutyEventType::RunActiveChange,
                duty_group_id: Some(id),
                duty_id: None,
                duty_repeat_id: dg.duty_repeat_id.as_deref(),
                operation_date: Some(dg.operation_date),
                actor_person_id: Some(&actor),
                trigger: GenerationTrigger::Api,
                old_value: Some(json!({ "isActive": dg.is_active })),
                new_value: Some(json!({ "isActive": is_active })),
                reason: None,
                error_code: None,
                error_message: None,
            },
        )
        .await?;
        tx.commit().await.map_err(|e| map_db("commit", e))?;
        Ok(updated)
    }

    /// Soft-deletes a run created by mistake. Refused once any trip has started.
    pub async fn delete_duty_group(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
    ) -> AppResult<()> {
        caller.require_actor()?;
        let mut tx = self.pool()?.begin().await.map_err(|e| map_db("begin", e))?;
        Self::lock_run(&mut tx, gtfs_id, caller, id).await?;
        let started: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM duties WHERE duty_group_id = $1 AND NOT deleted
                           AND recorded_start_time IS NOT NULL)",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_db("delete_duty_group", e))?;
        if started {
            return Err(AppError::BadRequest(
                "a trip of this run has started; deactivate the run instead".to_string(),
            ));
        }
        sqlx::query("UPDATE duty_groups SET deleted = true, updated_at = now() WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_db("delete_duty_group", e))?;
        sqlx::query(
            "UPDATE duties SET deleted = true, updated_at = now() WHERE duty_group_id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_db("delete_duty_group (trips)", e))?;
        tx.commit().await.map_err(|e| map_db("commit", e))
    }

    pub async fn get_run_logs(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
    ) -> AppResult<Vec<DutyEventLog>> {
        self.load_run(gtfs_id, caller, id, false).await?;
        let rows: Vec<DutyEventLogRow> = sqlx::query_as(&format!(
            "SELECT {} FROM duty_event_logs WHERE duty_group_id = $1 ORDER BY created_at DESC",
            EVENT_LOG_COLUMNS
        ))
        .bind(id)
        .fetch_all(self.pool()?)
        .await
        .map_err(|e| map_db("get_run_logs", e))?;
        Ok(rows.into_iter().map(DutyEventLog::from).collect())
    }

    // ── Trips of a run (duties) ─────────────────────────────────────────────

    async fn lock_duty(
        tx: &mut PgConnection,
        gtfs_id: &str,
        caller: &Caller,
        duty_id: &str,
    ) -> AppResult<(DutyGroup, Duty)> {
        let duty: Duty = sqlx::query_as(
            "SELECT * FROM duties WHERE id = $1 AND gtfs_id = $2 AND NOT deleted FOR UPDATE",
        )
        .bind(duty_id)
        .bind(gtfs_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| map_db("lock_duty", e))?
        .ok_or_else(|| AppError::NotFound(format!("trip '{}' not found", duty_id)))?;
        let dg = Self::lock_run(tx, gtfs_id, caller, &duty.duty_group_id).await?;
        Ok((dg, duty))
    }

    /// Trip-level crew change; only before the trip starts.
    pub async fn update_trip_crew(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        duty_id: &str,
        req: UpdateCrewReq,
    ) -> AppResult<Duty> {
        let actor = caller.require_actor()?.to_string();
        let mut tx = self.pool()?.begin().await.map_err(|e| map_db("begin", e))?;
        let (dg, duty) = Self::lock_duty(&mut tx, gtfs_id, caller, duty_id).await?;
        if duty.has_started() || !duty.is(DutyStatus::Upcoming) {
            return Err(AppError::BadRequest(
                "crew can only be changed before the trip starts".to_string(),
            ));
        }
        let driver = crew_change(
            duty.driver_token_number
                .clone()
                .or(dg.driver_token_number.clone()),
            duty.driver_name.clone().or(dg.driver_name.clone()),
            &req.driver_token_number,
            &req.driver_name,
        );
        let conductor = crew_change(
            duty.conductor_token_number
                .clone()
                .or(dg.conductor_token_number.clone()),
            duty.conductor_name.clone().or(dg.conductor_name.clone()),
            &req.conductor_token_number,
            &req.conductor_name,
        );
        if driver.is_none() && conductor.is_none() {
            return Ok(duty);
        }
        let mut updated = duty.clone();
        for (slot, change) in [("driver", &driver), ("conductor", &conductor)] {
            let Some(c) = change else { continue };
            updated = sqlx::query_as(&format!(
                "UPDATE duties SET {s}_token_number = $2, {s}_name = $3, updated_at = now()
                 WHERE id = $1 RETURNING *",
                s = slot
            ))
            .bind(duty_id)
            .bind(&c.new_token)
            .bind(&c.new_name)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| map_db("update_trip_crew", e))?;
        }
        log_event(
            &mut tx,
            NewEvent {
                gtfs_id,
                operator_id: dg.operator_id.as_deref(),
                event_type: DutyEventType::CrewChange,
                duty_group_id: Some(&dg.id),
                duty_id: Some(duty_id),
                duty_repeat_id: dg.duty_repeat_id.as_deref(),
                operation_date: Some(dg.operation_date),
                actor_person_id: Some(&actor),
                trigger: GenerationTrigger::Api,
                old_value: Some(json!({
                    "scope": "trip",
                    "tripNumber": duty.trip_number,
                    "driverTokenNumber": duty.driver_token_number,
                    "driverName": duty.driver_name,
                    "conductorTokenNumber": duty.conductor_token_number,
                    "conductorName": duty.conductor_name,
                })),
                new_value: Some(json!({
                    "scope": "trip",
                    "tripNumber": updated.trip_number,
                    "driverTokenNumber": updated.driver_token_number,
                    "driverName": updated.driver_name,
                    "conductorTokenNumber": updated.conductor_token_number,
                    "conductorName": updated.conductor_name,
                })),
                reason: None,
                error_code: None,
                error_message: None,
            },
        )
        .await?;
        tx.commit().await.map_err(|e| map_db("commit", e))?;
        Ok(with_duty_trip_ids(&dg.waybill_no, vec![updated]).remove(0))
    }

    /// Soft-deletes one trip of a run (a mistake). Use `cancel` for "won't run".
    pub async fn delete_duty(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        duty_id: &str,
    ) -> AppResult<()> {
        caller.require_actor()?;
        let mut tx = self.pool()?.begin().await.map_err(|e| map_db("begin", e))?;
        let (_, duty) = Self::lock_duty(&mut tx, gtfs_id, caller, duty_id).await?;
        if duty.has_started() {
            return Err(AppError::BadRequest(
                "trip has started; it can't be deleted".to_string(),
            ));
        }
        sqlx::query("UPDATE duties SET deleted = true, updated_at = now() WHERE id = $1")
            .bind(duty_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_db("delete_duty", e))?;
        tx.commit().await.map_err(|e| map_db("commit", e))
    }

    // ── Generation failures ─────────────────────────────────────────────────

    pub async fn list_generation_failures(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        q: FailureListQuery,
    ) -> AppResult<Paged<DutyEventLog>> {
        let pool = self.pool()?;
        let (limit, offset) = page(q.limit, q.offset);
        let filters = |qb: &mut QueryBuilder<'_, Postgres>| {
            qb.push(" WHERE gtfs_id = ")
                .push_bind(gtfs_id.to_string())
                .push(" AND event_type = 'GENERATION_FAILURE'");
            if let Some(op) = &caller.operator_id {
                qb.push(" AND operator_id = ").push_bind(op.clone());
            }
            match q.resolved {
                Some(true) => {
                    qb.push(" AND resolved_at IS NOT NULL");
                }
                Some(false) => {
                    qb.push(" AND resolved_at IS NULL");
                }
                None => {}
            }
        };
        let mut count_qb = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM duty_event_logs");
        filters(&mut count_qb);
        let total: i64 = count_qb
            .build_query_scalar()
            .fetch_one(pool)
            .await
            .map_err(|e| map_db("list_generation_failures (count)", e))?;
        let mut qb = QueryBuilder::<Postgres>::new(format!(
            "SELECT {} FROM duty_event_logs",
            EVENT_LOG_COLUMNS
        ));
        filters(&mut qb);
        qb.push(" ORDER BY created_at DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let rows = qb
            .build_query_as::<DutyEventLogRow>()
            .fetch_all(pool)
            .await
            .map_err(|e| map_db("list_generation_failures", e))?;
        Ok(Paged {
            items: rows.into_iter().map(DutyEventLog::from).collect(),
            total,
        })
    }

    pub async fn resolve_generation_failure(
        &self,
        gtfs_id: &str,
        caller: &Caller,
        id: &str,
    ) -> AppResult<DutyEventLog> {
        caller.require_actor()?;
        let row: DutyEventLogRow = sqlx::query_as(&format!(
            "UPDATE duty_event_logs SET resolved_at = COALESCE(resolved_at, now())
             WHERE id = $1 AND gtfs_id = $2 AND event_type = 'GENERATION_FAILURE'
               AND ($3::text IS NULL OR operator_id = $3)
             RETURNING {}",
            EVENT_LOG_COLUMNS
        ))
        .bind(id)
        .bind(gtfs_id)
        .bind(&caller.operator_id)
        .fetch_optional(self.pool()?)
        .await
        .map_err(|e| map_db("resolve_generation_failure", e))?
        .ok_or_else(|| AppError::NotFound(format!("failure '{}' not found", id)))?;
        Ok(row.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> NaiveTime {
        parse_hhmm(s).unwrap()
    }

    #[test]
    fn offsets_same_day() {
        let got = compute_offsets(&[(t("10:00"), t("11:00")), (t("12:00"), t("13:00"))], 0);
        assert_eq!(got, vec![(0, 0), (0, 0)]);
    }

    #[test]
    fn offsets_cross_midnight() {
        let got = compute_offsets(
            &[
                (t("22:30"), t("23:30")),
                (t("23:40"), t("00:30")),
                (t("00:45"), t("01:40")),
            ],
            0,
        );
        assert_eq!(got, vec![(0, 0), (0, 1), (1, 1)]);
    }

    #[test]
    fn offsets_first_trip_next_day() {
        let got = compute_offsets(&[(t("00:15"), t("01:00"))], 1);
        assert_eq!(got, vec![(1, 1)]);
    }

    #[test]
    fn code_parts() {
        assert_eq!(first_departure_token(t("07:20")), "0720AM");
        assert_eq!(first_departure_token(t("12:05")), "1205PM");
        assert_eq!(first_departure_token(t("00:15")), "1215AM");
        assert_eq!(first_departure_token(t("22:30")), "1030PM");
        assert_eq!(
            normalize_zone("Unnayan bhavan-2!").unwrap(),
            "UNNAYANBHAVAN2"
        );
        assert!(normalize_zone(" - ").is_err());
        assert_eq!(
            build_trip_group_code(
                "UNNAYANBHAVAN",
                Shift::Morning,
                t("07:20"),
                GroupTripType::Feeder
            ),
            "UNNAYANBHAVAN_MORNING_0720AM_FEEDER"
        );
    }

    #[test]
    fn instant_is_ist() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let at = to_instant(d, 1, t("00:30"));
        assert_eq!(at.to_rfc3339(), "2026-10-01T19:00:00+00:00");
    }
}
