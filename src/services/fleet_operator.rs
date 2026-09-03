use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::tools::error::{AppError, AppResult};

// ─── Domain types ─────────────────────────────────────────────────────────────

pub enum WaybillAnchor {
    DriverToken(String),
    ConductorToken(String),
    VehicleNumber(String),
}

#[derive(PartialEq)]
pub enum TripAction {
    Start,
    End,
    Reset,
    /// Undo the last forward step: revert the given trip_number back to "upcoming"
    /// (is_active_trip=false, is_completed=false) without touching any other trip.
    Rollback,
}

#[derive(Debug, Clone)]
struct WaybillRow {
    waybill_id: String,
    waybill_no: String,
    vehicle_no: String,
    conductor_token_no: Option<String>,
    driver_token_no: Option<String>,
    schedule_trip_id: Option<String>,
    is_flexi: bool,
    duty_date: Option<String>,
}

#[derive(Debug, Clone)]
struct RouteInfo {
    route_number: Option<String>,
    route_name: Option<String>,
}

// ─── Public response types ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CurrentOperationResponse {
    pub waybill_no: String,
    pub vehicle_number: String,
    pub conductor_token: Option<String>,
    pub driver_token: Option<String>,
    pub number_of_trips: i64,
    /// Ordered real (non-dead, non-inactive) trip_numbers, e.g. [2,3] when trip 1 is dead.
    /// Lets the caller drive tripAction off real numbers without the heavier currentTripDetails call.
    pub trip_numbers: Vec<i64>,
}

#[derive(Debug, Serialize)]
pub struct TripActionResponse {
    pub success: bool,
}

/// Just the active trip for a waybill, resolved directly off `is_active_trip` -- no
/// `previous_trip_number` needed, so unlike `current_trip_details` it can't be poisoned by a
/// caller's stale cache of what it last thought was current. `active_trip_number`/`route_id`
/// are `None` when no trip is currently active (between trips).
#[derive(Debug, Serialize)]
pub struct ActiveTripResponse {
    pub waybill_no: String,
    pub vehicle_number: String,
    pub conductor_token: Option<String>,
    pub driver_token: Option<String>,
    pub active_trip_number: Option<i32>,
    pub route_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct VerifyResponse {
    pub verified: bool,
}

#[derive(Debug, Serialize)]
pub struct TripData {
    pub trip_number: i32,
    pub route_id: Option<String>,
    pub route_number: Option<String>,
    pub route_name: Option<String>,
    pub is_active_trip: bool,
    pub duty_date: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CurrentTripDetailsResponse {
    pub waybill_no: String,
    pub vehicle_number: String,
    pub conductor_token: Option<String>,
    pub driver_token: Option<String>,
    pub history: Vec<TripData>,
    pub current: Option<TripData>,
    pub upcoming: Vec<TripData>,
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub enum AuthType {
    Email,
    EmployeeId,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Driver,
    Conductor,
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct EmployeeLoginRequest {
    pub auth_type: Option<AuthType>,
    pub email_hash: Option<String>,
    /// Plain badge/token number for `AuthType::EmployeeId` login (it is a username,
    /// not a secret, so unlike the email it is not hashed).
    pub employee_id: Option<String>,
    pub password_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct EmployeeLoginResponse {
    pub verified: bool,
    pub token: Option<String>,
    pub role: Option<Role>,
}

/// Map a matched `(token_no, designation_name)` login row into a response. Shared by
/// every `auth_type`: once an employee row is found — by whichever credential — deriving
/// the badge token and role is identical, so only the lookup query differs per auth type.
fn login_response_from_row(row: Option<(String, Option<String>)>) -> EmployeeLoginResponse {
    match row {
        Some((token, designation_name)) => {
            let role = designation_name.as_deref().and_then(|n| match n {
                "driver" => Some(Role::Driver),
                "conductor" => Some(Role::Conductor),
                _ => None,
            });
            EmployeeLoginResponse {
                verified: true,
                token: Some(token),
                role,
            }
        }
        None => EmployeeLoginResponse {
            verified: false,
            token: None,
            role: None,
        },
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct EmployeeRegisterRequest {
    pub token_no: String,
    pub email_hash: String,
    pub password_hash: String,
    pub first_name: String,
    pub role: Option<Role>,
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct EmployeeRegisterResponse {
    pub success: bool,
    pub token_no: String,
}

// ─── Service trait ─────────────────────────────────────────────────────────────

#[async_trait]
pub trait FleetOperatorService: Send + Sync {
    async fn current_operation(
        &self,
        gtfs_id: &str,
        anchor: WaybillAnchor,
    ) -> AppResult<CurrentOperationResponse>;

    async fn trip_action(
        &self,
        gtfs_id: &str,
        anchor: WaybillAnchor,
        action: TripAction,
        trip_number: i32,
        timestamp: Option<i64>,
    ) -> AppResult<TripActionResponse>;

    async fn current_trip_details(
        &self,
        gtfs_id: &str,
        anchor: WaybillAnchor,
        previous_trip_number: i32,
    ) -> AppResult<CurrentTripDetailsResponse>;

    async fn active_trip(
        &self,
        gtfs_id: &str,
        anchor: WaybillAnchor,
    ) -> AppResult<ActiveTripResponse>;

    async fn verify(
        &self,
        gtfs_id: &str,
        token: &str,
        device_serial_no: &str,
    ) -> AppResult<VerifyResponse>;

    async fn verify_without_device_serial_number(
        &self,
        gtfs_id: &str,
        token: &str,
        device_serial_no: &str,
    ) -> AppResult<VerifyResponse>;

    async fn login(
        &self,
        gtfs_id: &str,
        req: &EmployeeLoginRequest,
    ) -> AppResult<EmployeeLoginResponse>;

    async fn register(
        &self,
        gtfs_id: &str,
        req: &EmployeeRegisterRequest,
    ) -> AppResult<EmployeeRegisterResponse>;
}

// ─── Mock implementation ───────────────────────────────────────────────────────

pub struct MockFleetOperatorService;

impl MockFleetOperatorService {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MockFleetOperatorService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FleetOperatorService for MockFleetOperatorService {
    async fn current_operation(
        &self,
        _gtfs_id: &str,
        _anchor: WaybillAnchor,
    ) -> AppResult<CurrentOperationResponse> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn trip_action(
        &self,
        _gtfs_id: &str,
        _anchor: WaybillAnchor,
        _action: TripAction,
        _trip_number: i32,
        _timestamp: Option<i64>,
    ) -> AppResult<TripActionResponse> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn current_trip_details(
        &self,
        _gtfs_id: &str,
        _anchor: WaybillAnchor,
        _previous_trip_number: i32,
    ) -> AppResult<CurrentTripDetailsResponse> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn active_trip(
        &self,
        _gtfs_id: &str,
        _anchor: WaybillAnchor,
    ) -> AppResult<ActiveTripResponse> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn verify(
        &self,
        _gtfs_id: &str,
        _token: &str,
        _device_serial_no: &str,
    ) -> AppResult<VerifyResponse> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn verify_without_device_serial_number(
        &self,
        _gtfs_id: &str,
        _token: &str,
        _device_serial_no: &str,
    ) -> AppResult<VerifyResponse> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn login(
        &self,
        _gtfs_id: &str,
        _req: &EmployeeLoginRequest,
    ) -> AppResult<EmployeeLoginResponse> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn register(
        &self,
        _gtfs_id: &str,
        _req: &EmployeeRegisterRequest,
    ) -> AppResult<EmployeeRegisterResponse> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }
}

// ─── DB implementation ─────────────────────────────────────────────────────────

const ROUTE_CACHE_TTL_SECS: u64 = 6 * 3600;

pub struct DBFleetOperatorService {
    pool: PgPool,
    route_info_cache: Arc<RwLock<HashMap<String, (RouteInfo, SystemTime)>>>,
}

impl DBFleetOperatorService {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            route_info_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    // ── Waybill resolution ───────────────────────────────────────────────────

    async fn resolve_waybill(
        &self,
        gtfs_id: &str,
        anchor: &WaybillAnchor,
    ) -> AppResult<WaybillRow> {
        let base_select = r#"
            SELECT
                waybill_id::text,
                waybill_no,
                vehicle_no,
                conductor_token_no,
                driver_token_no,
                schedule_trip_id::text,
                is_flexi,
                duty_date
            FROM waybills_internal
            WHERE gtfs_id = $1
              AND status = 'online'
              AND deleted = false
        "#;

        #[allow(clippy::type_complexity)]
        let row: Option<(
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            bool,
            Option<String>,
        )> = match anchor {
            WaybillAnchor::DriverToken(token) => {
                let sql = format!(
                    "{} AND driver_token_no = $2 ORDER BY updated_at DESC LIMIT 1",
                    base_select
                );
                sqlx::query_as(&sql)
                    .bind(gtfs_id)
                    .bind(token)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|e| {
                        error!("resolve_waybill (driver_token) failed: {}", e);
                        AppError::Internal(e.to_string())
                    })?
            }
            WaybillAnchor::ConductorToken(token) => {
                let sql = format!(
                    "{} AND conductor_token_no = $2 ORDER BY updated_at DESC LIMIT 1",
                    base_select
                );
                sqlx::query_as(&sql)
                    .bind(gtfs_id)
                    .bind(token)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|e| {
                        error!("resolve_waybill (conductor_token) failed: {}", e);
                        AppError::Internal(e.to_string())
                    })?
            }
            WaybillAnchor::VehicleNumber(vehicle_no) => {
                let sql = format!(
                    "{} AND vehicle_no = $2 ORDER BY updated_at DESC LIMIT 1",
                    base_select
                );
                sqlx::query_as(&sql)
                    .bind(gtfs_id)
                    .bind(vehicle_no)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|e| {
                        error!("resolve_waybill (vehicle_no) failed: {}", e);
                        AppError::Internal(e.to_string())
                    })?
            }
        };

        match row {
            Some((
                waybill_id,
                waybill_no,
                vehicle_no,
                conductor_token_no,
                driver_token_no,
                schedule_trip_id,
                is_flexi,
                duty_date,
            )) => Ok(WaybillRow {
                waybill_id,
                waybill_no,
                vehicle_no,
                conductor_token_no,
                driver_token_no,
                schedule_trip_id,
                is_flexi,
                duty_date,
            }),
            None => Err(AppError::NotFound(
                "No active (online) waybill found for the provided anchor.".to_string(),
            )),
        }
    }

    // ── Trip count ───────────────────────────────────────────────────────────

    /// Ordered real (non-dead) trip_numbers for a waybill, e.g. [2, 3].
    async fn get_trip_numbers(&self, waybill: &WaybillRow) -> AppResult<Vec<i64>> {
        let rows: Vec<(i32,)> = if waybill.is_flexi {
            sqlx::query_as(
                r#"
                SELECT trip_number
                FROM bus_schedule_trip_flexi_internal
                WHERE waybill_id::text = $1
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                ORDER BY trip_number
                "#,
            )
            .bind(&waybill.waybill_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                error!(
                    "get_trip_numbers (flexi) failed for waybill_id={}: {}",
                    waybill.waybill_id, e
                );
                AppError::Internal(e.to_string())
            })?
        } else {
            let schedule_trip_id = waybill.schedule_trip_id.clone().ok_or_else(|| {
                AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
            })?;
            sqlx::query_as(
                r#"
                SELECT trip_number
                FROM bus_schedule_trip_detail_internal
                WHERE schedule_trip_id::text = $1
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                  AND LOWER(COALESCE(status, 'active')) <> 'inactive'
                ORDER BY trip_number
                "#,
            )
            .bind(&schedule_trip_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                error!(
                    "get_trip_numbers (detail) failed for schedule_trip_id={}: {}",
                    schedule_trip_id, e
                );
                AppError::Internal(e.to_string())
            })?
        };
        Ok(rows.into_iter().map(|(n,)| n as i64).collect())
    }

    // ── All trips ────────────────────────────────────────────────────────────

    #[allow(clippy::type_complexity)]
    async fn get_all_trips(
        &self,
        waybill: &WaybillRow,
    ) -> AppResult<Vec<(i32, String, bool, Option<String>, Option<String>)>> {
        // Returns Vec<(trip_number, route_id, is_active_trip, start_time, end_time)>
        if waybill.is_flexi {
            let rows: Vec<(i32, String, bool, Option<String>, Option<String>)> = sqlx::query_as(
                r#"
                SELECT
                    trip_number,
                    route_number_id::text AS route_id,
                    is_active_trip,
                    start_time,
                    end_time
                FROM bus_schedule_trip_flexi_internal
                WHERE waybill_id::text = $1
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                ORDER BY trip_number ASC
                "#,
            )
            .bind(&waybill.waybill_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                error!(
                    "get_all_trips (flexi) failed for waybill_id={}: {}",
                    waybill.waybill_id, e
                );
                AppError::Internal(e.to_string())
            })?;
            Ok(rows)
        } else {
            let schedule_trip_id = waybill.schedule_trip_id.clone().ok_or_else(|| {
                AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
            })?;
            let rows: Vec<(i32, String, bool, Option<String>, Option<String>)> = sqlx::query_as(
                r#"
                SELECT
                    trip_number,
                    route_number_id::text AS route_id,
                    is_active_trip,
                    start_time,
                    end_time
                FROM bus_schedule_trip_detail_internal
                WHERE schedule_trip_id::text = $1
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                  AND LOWER(COALESCE(status, 'active')) <> 'inactive'
                ORDER BY trip_number ASC
                "#,
            )
            .bind(&schedule_trip_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                error!(
                    "get_all_trips (detail) failed for schedule_trip_id={}: {}",
                    schedule_trip_id, e
                );
                AppError::Internal(e.to_string())
            })?;
            Ok(rows)
        }
    }

    // ── Active trip only ─────────────────────────────────────────────────────

    /// The single row with `is_active_trip = true` for this waybill, if any -- returns
    /// `(trip_number, route_id)`. No `previous_trip_number` input, unlike `get_all_trips`'s
    /// bucketing: this reads GIMS's own live flag directly, so it can't be misled by a stale
    /// caller-supplied reference point.
    async fn get_active_trip(&self, waybill: &WaybillRow) -> AppResult<Option<(i32, String)>> {
        if waybill.is_flexi {
            sqlx::query_as(
                r#"
                SELECT trip_number, route_number_id::text AS route_id
                FROM bus_schedule_trip_flexi_internal
                WHERE waybill_id::text = $1
                  AND is_active_trip = true
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                LIMIT 1
                "#,
            )
            .bind(&waybill.waybill_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                error!(
                    "get_active_trip (flexi) failed for waybill_id={}: {}",
                    waybill.waybill_id, e
                );
                AppError::Internal(e.to_string())
            })
        } else {
            let schedule_trip_id = waybill.schedule_trip_id.clone().ok_or_else(|| {
                AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
            })?;
            sqlx::query_as(
                r#"
                SELECT trip_number, route_number_id::text AS route_id
                FROM bus_schedule_trip_detail_internal
                WHERE schedule_trip_id::text = $1
                  AND is_active_trip = true
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                  AND LOWER(COALESCE(status, 'active')) <> 'inactive'
                LIMIT 1
                "#,
            )
            .bind(&schedule_trip_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                error!(
                    "get_active_trip (detail) failed for schedule_trip_id={}: {}",
                    schedule_trip_id, e
                );
                AppError::Internal(e.to_string())
            })
        }
    }

    // ── Trip validation ──────────────────────────────────────────────────────

    async fn validate_trip_exists(&self, waybill: &WaybillRow, trip_number: i32) -> AppResult<()> {
        let exists: bool = if waybill.is_flexi {
            let r: Option<i32> = sqlx::query_scalar(
                r#"
                SELECT 1
                FROM bus_schedule_trip_flexi_internal
                WHERE waybill_id::text = $1
                  AND trip_number = $2
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                LIMIT 1
                "#,
            )
            .bind(&waybill.waybill_id)
            .bind(trip_number)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?;
            r.is_some()
        } else {
            let schedule_trip_id = waybill.schedule_trip_id.clone().ok_or_else(|| {
                AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
            })?;
            let r: Option<i32> = sqlx::query_scalar(
                r#"
                SELECT 1
                FROM bus_schedule_trip_detail_internal
                WHERE schedule_trip_id::text = $1
                  AND trip_number = $2
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                  AND LOWER(COALESCE(status, 'active')) <> 'inactive'
                LIMIT 1
                "#,
            )
            .bind(&schedule_trip_id)
            .bind(trip_number)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?;
            r.is_some()
        };

        if exists {
            Ok(())
        } else {
            Err(AppError::NotFound(format!(
                "trip_number {} not found for this waybill.",
                trip_number
            )))
        }
    }

    // ── Trip start/end ───────────────────────────────────────────────────────

    async fn apply_trip_action(
        &self,
        waybill: &WaybillRow,
        action: &TripAction,
        trip_number: i32,
        timestamp: Option<i64>,
    ) -> AppResult<()> {
        let sync_now = chrono::Utc::now().timestamp_millis();
        match action {
            TripAction::Start => {
                if waybill.is_flexi {
                    if let Some(ts) = timestamp {
                        sqlx::query(
                            r#"
                            UPDATE bus_schedule_trip_flexi_internal
                            SET is_active_trip  = (trip_number = $2),
                                is_completed    = CASE WHEN trip_number >= $2 THEN false ELSE is_completed END,
                                trip_start_time = CASE WHEN trip_number = $2 THEN $3 ELSE trip_start_time END,
                                sync_start_time = CASE WHEN trip_number = $2 THEN $4 ELSE sync_start_time END
                            WHERE waybill_id::text = $1
                              AND trip_type != 'dead-trip'
                              AND deleted = false
                            "#,
                        )
                        .bind(&waybill.waybill_id)
                        .bind(trip_number)
                        .bind(ts)
                        .bind(sync_now)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| {
                            error!(
                                "apply_trip_action start (flexi+ts) failed for waybill_id={}: {}",
                                waybill.waybill_id, e
                            );
                            AppError::Internal(e.to_string())
                        })?;
                    } else {
                        sqlx::query(
                            r#"
                            UPDATE bus_schedule_trip_flexi_internal
                            SET is_active_trip = (trip_number = $2),
                                is_completed   = CASE WHEN trip_number >= $2 THEN false ELSE is_completed END
                            WHERE waybill_id::text = $1
                              AND trip_type != 'dead-trip'
                              AND deleted = false
                            "#,
                        )
                        .bind(&waybill.waybill_id)
                        .bind(trip_number)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| {
                            error!(
                                "apply_trip_action start (flexi) failed for waybill_id={}: {}",
                                waybill.waybill_id, e
                            );
                            AppError::Internal(e.to_string())
                        })?;
                    }
                } else {
                    let schedule_trip_id = waybill.schedule_trip_id.clone().ok_or_else(|| {
                        AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
                    })?;
                    if let Some(ts) = timestamp {
                        sqlx::query(
                            r#"
                            UPDATE bus_schedule_trip_detail_internal
                            SET is_active_trip  = (trip_number = $2),
                                is_completed    = CASE WHEN trip_number >= $2 THEN false ELSE is_completed END,
                                trip_start_time = CASE WHEN trip_number = $2 THEN $3 ELSE trip_start_time END,
                                sync_start_time = CASE WHEN trip_number = $2 THEN $4 ELSE sync_start_time END
                            WHERE schedule_trip_id::text = $1
                              AND trip_type != 'dead-trip'
                              AND LOWER(COALESCE(status, 'active')) <> 'inactive'
                              AND deleted = false
                            "#,
                        )
                        .bind(&schedule_trip_id)
                        .bind(trip_number)
                        .bind(ts)
                        .bind(sync_now)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| {
                            error!(
                                "apply_trip_action start (detail+ts) failed for schedule_trip_id={}: {}",
                                schedule_trip_id, e
                            );
                            AppError::Internal(e.to_string())
                        })?;
                    } else {
                        sqlx::query(
                            r#"
                            UPDATE bus_schedule_trip_detail_internal
                            SET is_active_trip = (trip_number = $2),
                                is_completed   = CASE WHEN trip_number >= $2 THEN false ELSE is_completed END
                            WHERE schedule_trip_id::text = $1
                              AND trip_type != 'dead-trip'
                              AND LOWER(COALESCE(status, 'active')) <> 'inactive'
                              AND deleted = false
                            "#,
                        )
                        .bind(&schedule_trip_id)
                        .bind(trip_number)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| {
                            error!(
                                "apply_trip_action start (detail) failed for schedule_trip_id={}: {}",
                                schedule_trip_id, e
                            );
                            AppError::Internal(e.to_string())
                        })?;
                    }
                }
            }
            TripAction::End => {
                if waybill.is_flexi {
                    if let Some(ts) = timestamp {
                        sqlx::query(
                            r#"
                            UPDATE bus_schedule_trip_flexi_internal
                            SET is_active_trip = false,
                                trip_end_time  = CASE WHEN is_active_trip THEN $2 ELSE trip_end_time END,
                                sync_end_time  = CASE WHEN is_active_trip THEN $3 ELSE sync_end_time END
                            WHERE waybill_id::text = $1
                              AND deleted = false
                            "#,
                        )
                        .bind(&waybill.waybill_id)
                        .bind(ts)
                        .bind(sync_now)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| {
                            error!(
                                "apply_trip_action end (flexi+ts) failed for waybill_id={}: {}",
                                waybill.waybill_id, e
                            );
                            AppError::Internal(e.to_string())
                        })?;
                    } else {
                        sqlx::query(
                            r#"
                            UPDATE bus_schedule_trip_flexi_internal
                            SET is_active_trip = false
                            WHERE waybill_id::text = $1
                              AND deleted = false
                            "#,
                        )
                        .bind(&waybill.waybill_id)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| {
                            error!(
                                "apply_trip_action end (flexi) failed for waybill_id={}: {}",
                                waybill.waybill_id, e
                            );
                            AppError::Internal(e.to_string())
                        })?;
                    }
                } else {
                    let schedule_trip_id = waybill.schedule_trip_id.clone().ok_or_else(|| {
                        AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
                    })?;
                    if let Some(ts) = timestamp {
                        sqlx::query(
                            r#"
                            UPDATE bus_schedule_trip_detail_internal
                            SET is_active_trip = false,
                                is_completed   = CASE WHEN trip_number = $4 AND is_active_trip THEN true ELSE is_completed END,
                                trip_end_time  = CASE WHEN is_active_trip THEN $2 ELSE trip_end_time END,
                                sync_end_time  = CASE WHEN is_active_trip THEN $3 ELSE sync_end_time END
                            WHERE schedule_trip_id::text = $1
                              AND deleted = false
                            "#,
                        )
                        .bind(&schedule_trip_id)
                        .bind(ts)
                        .bind(sync_now)
                        .bind(trip_number)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| {
                            error!(
                                "apply_trip_action end (detail+ts) failed for schedule_trip_id={}: {}",
                                schedule_trip_id, e
                            );
                            AppError::Internal(e.to_string())
                        })?;
                    } else {
                        sqlx::query(
                            r#"
                            UPDATE bus_schedule_trip_detail_internal
                            SET is_active_trip = false,
                                is_completed   = CASE WHEN trip_number = $2 AND is_active_trip THEN true ELSE is_completed END
                            WHERE schedule_trip_id::text = $1
                              AND deleted = false
                            "#,
                        )
                        .bind(&schedule_trip_id)
                        .bind(trip_number)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| {
                            error!(
                                "apply_trip_action end (detail) failed for schedule_trip_id={}: {}",
                                schedule_trip_id, e
                            );
                            AppError::Internal(e.to_string())
                        })?;
                    }
                }
            }
            TripAction::Reset => {
                if waybill.is_flexi {
                    sqlx::query(
                        r#"
                        UPDATE bus_schedule_trip_flexi_internal
                        SET is_active_trip = false,
                            is_completed   = false
                        WHERE waybill_id::text = $1
                          AND deleted = false
                        "#,
                    )
                    .bind(&waybill.waybill_id)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| {
                        error!(
                            "apply_trip_action reset (flexi) failed for waybill_id={}: {}",
                            waybill.waybill_id, e
                        );
                        AppError::Internal(e.to_string())
                    })?;
                } else {
                    let schedule_trip_id = waybill.schedule_trip_id.clone().ok_or_else(|| {
                        AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
                    })?;
                    sqlx::query(
                        r#"
                        UPDATE bus_schedule_trip_detail_internal
                        SET is_active_trip = false,
                            is_completed   = false
                        WHERE schedule_trip_id::text = $1
                          AND deleted = false
                        "#,
                    )
                    .bind(&schedule_trip_id)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| {
                        error!(
                            "apply_trip_action reset (detail) failed for schedule_trip_id={}: {}",
                            schedule_trip_id, e
                        );
                        AppError::Internal(e.to_string())
                    })?;
                }
            }
            TripAction::Rollback => {
                // Revert just this trip back to "upcoming" (de-activate + un-complete);
                // every other trip is left untouched.
                if waybill.is_flexi {
                    sqlx::query(
                        r#"
                        UPDATE bus_schedule_trip_flexi_internal
                        SET is_active_trip = false,
                            is_completed   = false
                        WHERE waybill_id::text = $1
                          AND trip_number = $2
                          AND trip_type != 'dead-trip'
                          AND deleted = false
                        "#,
                    )
                    .bind(&waybill.waybill_id)
                    .bind(trip_number)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| {
                        error!(
                            "apply_trip_action rollback (flexi) failed for waybill_id={}: {}",
                            waybill.waybill_id, e
                        );
                        AppError::Internal(e.to_string())
                    })?;
                } else {
                    let schedule_trip_id = waybill.schedule_trip_id.clone().ok_or_else(|| {
                        AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
                    })?;
                    sqlx::query(
                        r#"
                        UPDATE bus_schedule_trip_detail_internal
                        SET is_active_trip = false,
                            is_completed   = false
                        WHERE schedule_trip_id::text = $1
                          AND trip_number = $2
                          AND trip_type != 'dead-trip'
                          AND LOWER(COALESCE(status, 'active')) <> 'inactive'
                          AND deleted = false
                        "#,
                    )
                    .bind(&schedule_trip_id)
                    .bind(trip_number)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| {
                        error!(
                            "apply_trip_action rollback (detail) failed for schedule_trip_id={}: {}",
                            schedule_trip_id, e
                        );
                        AppError::Internal(e.to_string())
                    })?;
                }
            }
        }
        Ok(())
    }

    // ── Route info cache ─────────────────────────────────────────────────────

    async fn get_route_infos(&self, route_ids: &[String]) -> AppResult<HashMap<String, RouteInfo>> {
        if route_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let now = SystemTime::now();
        let mut result: HashMap<String, RouteInfo> = HashMap::new();
        let mut uncached: Vec<String> = Vec::new();

        // Check cache for each route_id
        {
            let cache = self.route_info_cache.read().await;
            for id in route_ids {
                if let Some((info, ts)) = cache.get(id) {
                    let elapsed = now.duration_since(*ts).unwrap_or_default();
                    if elapsed < Duration::from_secs(ROUTE_CACHE_TTL_SECS) {
                        result.insert(id.clone(), info.clone());
                        continue;
                    }
                }
                uncached.push(id.clone());
            }
        }

        if uncached.is_empty() {
            info!("route_info cache: all {} ids HIT", route_ids.len());
            return Ok(result);
        }

        info!(
            "route_info cache: {} HIT, {} MISS — querying DB",
            result.len(),
            uncached.len()
        );

        // Batch query for uncached ids (parameterized, no injection risk)
        let placeholders: Vec<String> = (1..=uncached.len()).map(|i| format!("${}", i)).collect();
        let sql = format!(
            "SELECT route_id::TEXT, route_number, route_name FROM route_internal WHERE route_id::text IN ({})",
            placeholders.join(",")
        );

        let mut qb = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(&sql);
        for id in &uncached {
            qb = qb.bind(id);
        }

        let rows = match qb.fetch_all(&self.pool).await {
            Ok(r) => r,
            Err(e) => {
                error!("get_route_infos DB query failed: {}", e);
                return Ok(result);
            }
        };

        let fetched_now = SystemTime::now();
        let mut cache = self.route_info_cache.write().await;
        for (route_id, route_number, route_name) in rows {
            let info = RouteInfo {
                route_number,
                route_name,
            };
            cache.insert(route_id.clone(), (info.clone(), fetched_now));
            result.insert(route_id, info);
        }

        Ok(result)
    }
}

// ─── Trait implementation ──────────────────────────────────────────────────────

#[async_trait]
impl FleetOperatorService for DBFleetOperatorService {
    async fn current_operation(
        &self,
        gtfs_id: &str,
        anchor: WaybillAnchor,
    ) -> AppResult<CurrentOperationResponse> {
        let waybill = self.resolve_waybill(gtfs_id, &anchor).await?;
        let trip_numbers = self.get_trip_numbers(&waybill).await?;
        let number_of_trips = trip_numbers.len() as i64;

        Ok(CurrentOperationResponse {
            waybill_no: waybill.waybill_no,
            vehicle_number: waybill.vehicle_no,
            conductor_token: waybill.conductor_token_no,
            driver_token: waybill.driver_token_no,
            number_of_trips,
            trip_numbers,
        })
    }

    async fn trip_action(
        &self,
        gtfs_id: &str,
        anchor: WaybillAnchor,
        action: TripAction,
        trip_number: i32,
        timestamp: Option<i64>,
    ) -> AppResult<TripActionResponse> {
        let waybill = self.resolve_waybill(gtfs_id, &anchor).await?;
        if action != TripAction::Reset {
            self.validate_trip_exists(&waybill, trip_number).await?;
        }
        self.apply_trip_action(&waybill, &action, trip_number, timestamp)
            .await?;
        Ok(TripActionResponse { success: true })
    }

    async fn current_trip_details(
        &self,
        gtfs_id: &str,
        anchor: WaybillAnchor,
        previous_trip_number: i32,
    ) -> AppResult<CurrentTripDetailsResponse> {
        let waybill = self.resolve_waybill(gtfs_id, &anchor).await?;
        let raw_trips = self.get_all_trips(&waybill).await?;

        // Collect unique route_ids for cache lookup
        let route_ids: Vec<String> = raw_trips
            .iter()
            .map(|(_, route_id, _, _, _)| route_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let route_map = self.get_route_infos(&route_ids).await?;

        let mut history: Vec<TripData> = Vec::new();
        let mut current: Option<TripData> = None;
        let mut upcoming: Vec<TripData> = Vec::new();

        for (trip_number, route_id, is_active_trip, start_time, end_time) in raw_trips {
            let info = route_map.get(&route_id);
            let trip_data = TripData {
                trip_number,
                route_id: Some(route_id),
                route_number: info.and_then(|i| i.route_number.clone()),
                route_name: info.and_then(|i| i.route_name.clone()),
                is_active_trip,
                duty_date: waybill.duty_date.clone(),
                start_time,
                end_time,
            };

            if trip_number == previous_trip_number && is_active_trip {
                current = Some(trip_data);
            } else if trip_number <= previous_trip_number {
                history.push(trip_data);
            } else {
                upcoming.push(trip_data);
            }
        }

        Ok(CurrentTripDetailsResponse {
            waybill_no: waybill.waybill_no,
            vehicle_number: waybill.vehicle_no,
            conductor_token: waybill.conductor_token_no,
            driver_token: waybill.driver_token_no,
            history,
            current,
            upcoming,
        })
    }

    async fn active_trip(
        &self,
        gtfs_id: &str,
        anchor: WaybillAnchor,
    ) -> AppResult<ActiveTripResponse> {
        let waybill = self.resolve_waybill(gtfs_id, &anchor).await?;
        let active = self.get_active_trip(&waybill).await?;

        Ok(ActiveTripResponse {
            waybill_no: waybill.waybill_no,
            vehicle_number: waybill.vehicle_no,
            conductor_token: waybill.conductor_token_no,
            driver_token: waybill.driver_token_no,
            active_trip_number: active.as_ref().map(|(n, _)| *n),
            route_id: active.map(|(_, r)| r),
        })
    }

    async fn verify_without_device_serial_number(
        &self,
        gtfs_id: &str,
        token: &str,
        _device_serial_no: &str,
    ) -> AppResult<VerifyResponse> {
        let employee_ok: bool = sqlx::query_scalar(
            r#"
            SELECT 1
            FROM employees_internal
            WHERE token_no = $1
              AND gtfs_id  = $2
              AND deleted  = false
            LIMIT 1
            "#,
        )
        .bind(token)
        .bind(gtfs_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            error!(
                "verify employee check failed for token={} gtfs_id={}: {}",
                token, gtfs_id, e
            );
            AppError::Internal(e.to_string())
        })?
        .map(|_: i32| true)
        .unwrap_or(false);

        Ok(VerifyResponse {
            verified: employee_ok,
        })
    }

    async fn verify(
        &self,
        gtfs_id: &str,
        token: &str,
        device_serial_no: &str,
    ) -> AppResult<VerifyResponse> {
        // Check employee exists by token_no in employees_internal (covers both operators and drivers)
        let employee_ok: bool = sqlx::query_scalar(
            r#"
            SELECT 1
            FROM employees_internal
            WHERE token_no = $1
              AND gtfs_id  = $2
              AND deleted  = false
            LIMIT 1
            "#,
        )
        .bind(token)
        .bind(gtfs_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            error!(
                "verify employee check failed for token={} gtfs_id={}: {}",
                token, gtfs_id, e
            );
            AppError::Internal(e.to_string())
        })?
        .map(|_: i32| true)
        .unwrap_or(false);

        if !employee_ok {
            return Ok(VerifyResponse { verified: false });
        }

        // Check device against both OBU and ETM tables
        let obu_ok: bool = sqlx::query_scalar(
            r#"
            SELECT 1
            FROM fleet_obu_mapping_internal
            WHERE obu_id  = $1
              AND gtfs_id = $2
              AND deleted = false
            LIMIT 1
            "#,
        )
        .bind(device_serial_no)
        .bind(gtfs_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            error!(
                "verify OBU check failed for device={} gtfs_id={}: {}",
                device_serial_no, gtfs_id, e
            );
            AppError::Internal(e.to_string())
        })?
        .map(|_: i32| true)
        .unwrap_or(false);

        if obu_ok {
            return Ok(VerifyResponse { verified: true });
        }

        let etm_ok: bool = sqlx::query_scalar(
            r#"
            SELECT 1
            FROM fleet_etm_mapping_internal
            WHERE etm_serial_no = $1
              AND gtfs_id       = $2
              AND deleted       = false
            LIMIT 1
            "#,
        )
        .bind(device_serial_no)
        .bind(gtfs_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            error!(
                "verify ETM check failed for device={} gtfs_id={}: {}",
                device_serial_no, gtfs_id, e
            );
            AppError::Internal(e.to_string())
        })?
        .map(|_: i32| true)
        .unwrap_or(false);

        Ok(VerifyResponse { verified: etm_ok })
    }

    async fn login(
        &self,
        gtfs_id: &str,
        req: &EmployeeLoginRequest,
    ) -> AppResult<EmployeeLoginResponse> {
        match req.auth_type {
            Some(AuthType::Email) => {
                let email_hash = req.email_hash.as_ref().ok_or_else(|| {
                    AppError::BadRequest("email_hash is required for email auth".into())
                })?;
                let password_hash = req.password_hash.as_ref().ok_or_else(|| {
                    AppError::BadRequest("password_hash is required for email auth".into())
                })?;

                let row: Option<(String, Option<String>)> = sqlx::query_as(
                    r#"
                    SELECT e.token_no, LOWER(d.designation_name)
                    FROM employees_internal e
                    LEFT JOIN designations_internal d
                      ON d.designation_id = e.designation_id
                     AND d.gtfs_id = e.gtfs_id
                     AND d.deleted = false
                    WHERE e.email_hash = $1
                      AND e.password_hash = $2
                      AND e.gtfs_id = $3
                      AND e.deleted = false
                    LIMIT 1
                    "#,
                )
                .bind(email_hash)
                .bind(password_hash)
                .bind(gtfs_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| {
                    error!("login failed for gtfs_id={}: {}", gtfs_id, e);
                    AppError::Internal(e.to_string())
                })?;

                Ok(login_response_from_row(row))
            }
            Some(AuthType::EmployeeId) => {
                let employee_id = req.employee_id.as_ref().ok_or_else(|| {
                    AppError::BadRequest("employee_id is required for employee_id auth".into())
                })?;
                let password_hash = req.password_hash.as_ref().ok_or_else(|| {
                    AppError::BadRequest("password_hash is required for employee_id auth".into())
                })?;

                // The id a conductor/driver types is their badge number (token_no); the
                // password is still verified. token_no is also what verify() keys off, so
                // the token returned here round-trips as the operator badge token.
                let row: Option<(String, Option<String>)> = sqlx::query_as(
                    r#"
                    SELECT e.token_no, LOWER(d.designation_name)
                    FROM employees_internal e
                    LEFT JOIN designations_internal d
                      ON d.designation_id = e.designation_id
                     AND d.gtfs_id = e.gtfs_id
                     AND d.deleted = false
                    WHERE e.token_no = $1
                      AND e.password_hash = $2
                      AND e.gtfs_id = $3
                      AND e.deleted = false
                    LIMIT 1
                    "#,
                )
                .bind(employee_id)
                .bind(password_hash)
                .bind(gtfs_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| {
                    error!(
                        "login failed for employee_id={} gtfs_id={}: {}",
                        employee_id, gtfs_id, e
                    );
                    AppError::Internal(e.to_string())
                })?;

                Ok(login_response_from_row(row))
            }
            None => Ok(EmployeeLoginResponse {
                verified: false,
                token: None,
                role: None,
            }),
        }
    }

    async fn register(
        &self,
        gtfs_id: &str,
        req: &EmployeeRegisterRequest,
    ) -> AppResult<EmployeeRegisterResponse> {
        let designation_id: Option<String> = match req.role {
            Some(role) => {
                let name = match role {
                    Role::Driver => "driver",
                    Role::Conductor => "conductor",
                };
                let id: Option<String> = sqlx::query_scalar(
                    r#"
                    SELECT designation_id
                    FROM designations_internal
                    WHERE LOWER(designation_name) = $1
                      AND gtfs_id = $2
                      AND deleted = false
                    LIMIT 1
                    "#,
                )
                .bind(name)
                .bind(gtfs_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| {
                    error!(
                        "register designation lookup failed for gtfs_id={}, role={}: {}",
                        gtfs_id, name, e
                    );
                    AppError::Internal(e.to_string())
                })?;
                Some(id.ok_or_else(|| {
                    AppError::NotFound(format!(
                        "designation '{}' not found for gtfs_id={}",
                        name, gtfs_id
                    ))
                })?)
            }
            None => None,
        };

        if let Some(did) = designation_id {
            sqlx::query(
                r#"
                INSERT INTO employees_internal (token_no, email_hash, password_hash, gtfs_id, first_name, designation_id)
                VALUES ($1, $2, $3, $4, $5, $6)
                ON CONFLICT (gtfs_id, token_no) DO UPDATE SET
                    email_hash = EXCLUDED.email_hash,
                    password_hash = EXCLUDED.password_hash,
                    designation_id = EXCLUDED.designation_id,
                    updated_at = NOW()
                "#,
            )
            .bind(&req.token_no)
            .bind(&req.email_hash)
            .bind(&req.password_hash)
            .bind(gtfs_id)
            .bind(&req.first_name)
            .bind(did)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                error!("register upsert failed for gtfs_id={}, token_no={}: {}", gtfs_id, req.token_no, e);
                AppError::Internal(e.to_string())
            })?;
        } else {
            sqlx::query(
                r#"
                INSERT INTO employees_internal (token_no, email_hash, password_hash, gtfs_id, first_name)
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT (gtfs_id, token_no) DO UPDATE SET
                    email_hash = EXCLUDED.email_hash,
                    password_hash = EXCLUDED.password_hash,
                    updated_at = NOW()
                "#,
            )
            .bind(&req.token_no)
            .bind(&req.email_hash)
            .bind(&req.password_hash)
            .bind(gtfs_id)
            .bind(&req.first_name)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                error!("register upsert failed for gtfs_id={}, token_no={}: {}", gtfs_id, req.token_no, e);
                AppError::Internal(e.to_string())
            })?;
        }

        Ok(EmployeeRegisterResponse {
            success: true,
            token_no: req.token_no.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── login_response_from_row: shared response shaping ─────────────────────────
    //
    // This pure function backs both the Email and EmployeeId auth branches: once a row is
    // found, deriving the token and role is identical, so testing it here covers the shared
    // response-shaping logic for every auth_type without needing a database.

    #[test]
    fn login_response_maps_driver_designation() {
        let res = login_response_from_row(Some(("BADGE-1".into(), Some("driver".into()))));
        assert!(res.verified);
        assert_eq!(res.token.as_deref(), Some("BADGE-1"));
        assert!(matches!(res.role, Some(Role::Driver)));
    }

    #[test]
    fn login_response_maps_conductor_designation() {
        let res = login_response_from_row(Some(("BADGE-2".into(), Some("conductor".into()))));
        assert!(res.verified);
        assert_eq!(res.token.as_deref(), Some("BADGE-2"));
        assert!(matches!(res.role, Some(Role::Conductor)));
    }

    #[test]
    fn login_response_verified_with_no_role_for_unknown_designation() {
        // A matched employee with a designation we don't map (or none) is still a valid
        // login — verified is true and a token is returned, only the role is absent.
        let res = login_response_from_row(Some(("BADGE-3".into(), Some("supervisor".into()))));
        assert!(res.verified);
        assert_eq!(res.token.as_deref(), Some("BADGE-3"));
        assert!(res.role.is_none());

        let res = login_response_from_row(Some(("BADGE-4".into(), None)));
        assert!(res.verified);
        assert_eq!(res.token.as_deref(), Some("BADGE-4"));
        assert!(res.role.is_none());
    }

    #[test]
    fn login_response_unverified_when_no_row() {
        // No matching credential row -> not verified, no token, no role.
        let res = login_response_from_row(None);
        assert!(!res.verified);
        assert!(res.token.is_none());
        assert!(res.role.is_none());
    }

    // ── login: request validation guards ─────────────────────────────────────────
    //
    // Each auth_type rejects a missing credential with BadRequest *before* running any
    // query. A lazily-connected pool never opens a socket until a query is executed, so
    // these guard paths are fully testable without a live database.

    fn lazy_service() -> DBFleetOperatorService {
        let pool = sqlx::PgPool::connect_lazy("postgres://user:pass@localhost/db")
            .expect("lazy pool construction must not attempt to connect");
        DBFleetOperatorService::new(pool)
    }

    fn assert_bad_request(err: AppError, expected_substring: &str) {
        match err {
            AppError::BadRequest(msg) => assert!(
                msg.contains(expected_substring),
                "expected BadRequest containing {expected_substring:?}, got {msg:?}"
            ),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn login_email_requires_email_hash() {
        let req = EmployeeLoginRequest {
            auth_type: Some(AuthType::Email),
            email_hash: None,
            employee_id: None,
            password_hash: Some("pw".into()),
        };
        let err = lazy_service().login("gtfs", &req).await.unwrap_err();
        assert_bad_request(err, "email_hash is required");
    }

    #[tokio::test]
    async fn login_email_requires_password_hash() {
        let req = EmployeeLoginRequest {
            auth_type: Some(AuthType::Email),
            email_hash: Some("eh".into()),
            employee_id: None,
            password_hash: None,
        };
        let err = lazy_service().login("gtfs", &req).await.unwrap_err();
        assert_bad_request(err, "password_hash is required for email auth");
    }

    #[tokio::test]
    async fn login_employee_id_requires_employee_id() {
        let req = EmployeeLoginRequest {
            auth_type: Some(AuthType::EmployeeId),
            email_hash: None,
            employee_id: None,
            password_hash: Some("pw".into()),
        };
        let err = lazy_service().login("gtfs", &req).await.unwrap_err();
        assert_bad_request(err, "employee_id is required");
    }

    #[tokio::test]
    async fn login_employee_id_requires_password_hash() {
        let req = EmployeeLoginRequest {
            auth_type: Some(AuthType::EmployeeId),
            email_hash: None,
            employee_id: Some("BADGE-9".into()),
            password_hash: None,
        };
        let err = lazy_service().login("gtfs", &req).await.unwrap_err();
        assert_bad_request(err, "password_hash is required for employee_id auth");
    }

    #[tokio::test]
    async fn login_without_auth_type_is_unverified() {
        // A request with no auth_type is not an error — it resolves to an unverified
        // response without ever touching the database.
        let req = EmployeeLoginRequest {
            auth_type: None,
            email_hash: None,
            employee_id: None,
            password_hash: None,
        };
        let res = lazy_service().login("gtfs", &req).await.unwrap();
        assert!(!res.verified);
        assert!(res.token.is_none());
        assert!(res.role.is_none());
    }
}
