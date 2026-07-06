use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

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
    MobileNumber,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, utoipa::ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Driver,
    Conductor,
    Manager,
    #[serde(rename = "driver_conductor")]
    DriverConductor,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, utoipa::ToSchema, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EmployeeLoginError {
    PersonNotFound,
    TokenMismatch,
    EmailAuthFailed,
    MissingMobileNo,
    MissingEmailHash,
    MissingPasswordHash,
    MissingAuthType,
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct EmployeeLoginRequest {
    pub auth_type: Option<AuthType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mobile_no: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_no: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct EmployeeLoginResponse {
    pub verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<EmployeeLoginError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<crate::models::EmployeeMetadata>,
}

/// Maps a designation_name (case-insensitive) to a `Role`.
///
/// Real-world designations seen so far:
/// - primary `designations`: "Driver", "Conductor", "Driver-Conductor", "Admin Manager", "Admin Staff"
/// - `designations_internal`: "driver", "conductor", "depot_manager"
///
/// Rules:
/// 1. Empty / missing → `None`.
/// 2. Dual-role ("driver-conductor" / "driver_conductor") is recognised *before* the
///    single-role substrings so it doesn't get swallowed by the "conductor" branch.
/// 3. Any title containing "admin" → `None`. This intentionally excludes office roles
///    like "Admin Manager" and "Admin Staff" from the operational `Role::Manager` bucket
///    (which is reserved for depot managers).
/// 4. Otherwise substring match on "conductor" → Conductor, "driver" → Driver,
///    "manager" → Manager. Anything else → `None`.
fn map_designation_to_role(designation_name: Option<&str>) -> Option<Role> {
    let name = designation_name?.trim().to_lowercase();
    if name.is_empty() {
        return None;
    }
    if name.contains("driver-conductor") || name.contains("driver_conductor") {
        return Some(Role::DriverConductor);
    }
    if name.contains("admin") {
        return None;
    }
    if name.contains("conductor") {
        Some(Role::Conductor)
    } else if name.contains("driver") {
        Some(Role::Driver)
    } else if name.contains("manager") {
        Some(Role::Manager)
    } else {
        None
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
        with_metadata: bool,
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
        _with_metadata: bool,
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
    employee_reader: Arc<dyn crate::services::db_employee_reader::EmployeeReader>,
    vehicle_reader: Arc<dyn crate::services::db_vehicle_reader::VehicleDataReader>,
}

impl DBFleetOperatorService {
    pub fn new(
        pool: PgPool,
        employee_reader: Arc<dyn crate::services::db_employee_reader::EmployeeReader>,
        vehicle_reader: Arc<dyn crate::services::db_vehicle_reader::VehicleDataReader>,
    ) -> Self {
        Self {
            pool,
            route_info_cache: Arc::new(RwLock::new(HashMap::new())),
            employee_reader,
            vehicle_reader,
        }
    }

    async fn login_email(
        &self,
        gtfs_id: &str,
        req: &EmployeeLoginRequest,
    ) -> AppResult<EmployeeLoginResponse> {
        let email_hash = match req.email_hash.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => s,
            _ => {
                return Ok(EmployeeLoginResponse {
                    verified: false,
                    token: None,
                    role: None,
                    error: Some(EmployeeLoginError::MissingEmailHash),
                    metadata: None,
                });
            }
        };
        let password_hash = match req.password_hash.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => s,
            _ => {
                return Ok(EmployeeLoginResponse {
                    verified: false,
                    token: None,
                    role: None,
                    error: Some(EmployeeLoginError::MissingPasswordHash),
                    metadata: None,
                });
            }
        };

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

        match row {
            Some((token, designation_name)) => {
                // Email auth currently masks Manager and DriverConductor → None until the
                // driver-app Haskell side (Registration.hs:`Just GimsConductor → BUS_CONDUCTOR;
                // _ → BUS_DRIVER`) is updated to handle the new variants. Remove these filters
                // once that lands.
                let role = match map_designation_to_role(designation_name.as_deref()) {
                    Some(Role::Manager) | Some(Role::DriverConductor) => None,
                    other => other,
                };
                Ok(EmployeeLoginResponse {
                    verified: true,
                    token: Some(token),
                    role,
                    error: None,
                    metadata: None,
                })
            }
            None => Ok(EmployeeLoginResponse {
                verified: false,
                token: None,
                role: None,
                error: Some(EmployeeLoginError::EmailAuthFailed),
                metadata: None,
            }),
        }
    }

    async fn login_mobile_number(
        &self,
        gtfs_id: &str,
        req: &EmployeeLoginRequest,
        with_metadata: bool,
    ) -> AppResult<EmployeeLoginResponse> {
        let mobile_no = match req.mobile_no.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => s,
            _ => {
                return Ok(EmployeeLoginResponse {
                    verified: false,
                    token: None,
                    role: None,
                    error: Some(EmployeeLoginError::MissingMobileNo),
                    metadata: None,
                });
            }
        };

        // Primary feed → try primary DB first, fall back to internal.
        // Other gtfs_id → internal only.
        let row = if gtfs_id == crate::services::PRIMARY_GTFS_ID {
            match self
                .employee_reader
                .lookup_by_mobile_primary(mobile_no)
                .await?
            {
                Some(r) => Some(r),
                None => {
                    self.employee_reader
                        .lookup_by_mobile_internal(gtfs_id, mobile_no)
                        .await?
                }
            }
        } else {
            self.employee_reader
                .lookup_by_mobile_internal(gtfs_id, mobile_no)
                .await?
        };

        let row = match row {
            Some(r) => r,
            None => {
                return Ok(EmployeeLoginResponse {
                    verified: false,
                    token: None,
                    role: None,
                    error: Some(EmployeeLoginError::PersonNotFound),
                    metadata: None,
                });
            }
        };

        // Optional token verification
        if let Some(req_token) = req.token_no.as_deref().map(str::trim) {
            if !req_token.is_empty() && row.token_no.as_deref() != Some(req_token) {
                return Ok(EmployeeLoginResponse {
                    verified: false,
                    token: None,
                    role: None,
                    error: Some(EmployeeLoginError::TokenMismatch),
                    metadata: None,
                });
            }
        }

        let role = map_designation_to_role(row.designation_name.as_deref());

        let metadata = if with_metadata {
            // Depot lookup is optional context; never fail the login because of it.
            // Surface the failure in logs instead of swallowing silently so a
            // backend outage doesn't masquerade as "employee has no depot".
            let depot = match self
                .vehicle_reader
                .get_depot_info(gtfs_id, row.entity_id)
                .await
            {
                Ok(d) => d,
                Err(e) => {
                    warn!(
                        "depot lookup failed for gtfs_id={} entity_id={}: {}",
                        gtfs_id, row.entity_id, e
                    );
                    None
                }
            };
            Some(crate::models::EmployeeMetadata {
                first_name: row.first_name.clone(),
                last_name: row.last_name.clone(),
                mobile_no: row.mobile_no.clone(),
                depot_name: depot.as_ref().map(|d| d.name.clone()),
                depot_code: depot.and_then(|d| d.code),
            })
        } else {
            None
        };

        Ok(EmployeeLoginResponse {
            verified: true,
            token: row.token_no,
            role,
            error: None,
            metadata,
        })
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
        with_metadata: bool,
    ) -> AppResult<EmployeeLoginResponse> {
        match req.auth_type {
            Some(AuthType::Email) => self.login_email(gtfs_id, req).await,
            Some(AuthType::MobileNumber) => {
                self.login_mobile_number(gtfs_id, req, with_metadata).await
            }
            None => Ok(EmployeeLoginResponse {
                verified: false,
                token: None,
                role: None,
                error: Some(EmployeeLoginError::MissingAuthType),
                metadata: None,
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
                    Role::Manager => "manager",
                    Role::DriverConductor => "driver-conductor",
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
    use crate::services::db_employee_reader::{EmployeeLookupRow, EmployeeReader};
    use crate::services::db_vehicle_reader::{DepotInfo, MockDBVehicleReader, VehicleDataReader};
    use crate::services::PRIMARY_GTFS_ID;
    use async_trait::async_trait;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Stubs ────────────────────────────────────────────────────────────────

    /// Records call counts and returns canned rows per lookup function.
    struct StubEmployeeReader {
        primary_row: Option<EmployeeLookupRow>,
        internal_row: Option<EmployeeLookupRow>,
        primary_calls: AtomicUsize,
        internal_calls: AtomicUsize,
    }

    impl StubEmployeeReader {
        fn new(primary: Option<EmployeeLookupRow>, internal: Option<EmployeeLookupRow>) -> Self {
            Self {
                primary_row: primary,
                internal_row: internal,
                primary_calls: AtomicUsize::new(0),
                internal_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl EmployeeReader for StubEmployeeReader {
        async fn get_employee_by_phone(
            &self,
            _phone: &str,
        ) -> AppResult<Option<crate::models::MinimalEmployee>> {
            Ok(None)
        }
        async fn lookup_by_mobile_primary(
            &self,
            _mobile_no: &str,
        ) -> AppResult<Option<EmployeeLookupRow>> {
            self.primary_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.primary_row.clone())
        }
        async fn lookup_by_mobile_internal(
            &self,
            _gtfs_id: &str,
            _mobile_no: &str,
        ) -> AppResult<Option<EmployeeLookupRow>> {
            self.internal_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.internal_row.clone())
        }
    }

    struct StubVehicleReader {
        depot: Option<DepotInfo>,
    }

    #[async_trait]
    impl VehicleDataReader for StubVehicleReader {
        // Only get_depot_info is exercised; everything else proxies to the mock's behaviour.
        async fn get_depot_info(
            &self,
            _gtfs_id: &str,
            _entity_id: i64,
        ) -> AppResult<Option<DepotInfo>> {
            Ok(self.depot.clone())
        }
        // Delegate the rest to MockDBVehicleReader's stub responses by re-implementing as
        // "not used in these tests" stubs.
        async fn get_vehicle_data(
            &self,
            _vehicle_no: &str,
            _trip_number: Option<i32>,
        ) -> AppResult<crate::models::VehicleDataWithRouteId> {
            unreachable!("not used in login tests")
        }
        async fn get_vehicles_by_ids(
            &self,
            _vehicle_nos: Vec<String>,
        ) -> AppResult<Vec<crate::models::VehicleDataWithRouteId>> {
            Ok(Vec::new())
        }
        async fn get_all_vehicles(&self) -> AppResult<Vec<crate::models::VehicleData>> {
            Ok(Vec::new())
        }
        async fn get_vehicles_by_service_type(
            &self,
            _service_type: &str,
        ) -> AppResult<Vec<crate::models::VehicleData>> {
            Ok(Vec::new())
        }
        async fn search_vehicles(
            &self,
            _query: &str,
        ) -> AppResult<Vec<crate::models::VehicleData>> {
            Ok(Vec::new())
        }
        async fn get_vehicle_count(&self) -> AppResult<i64> {
            Ok(0)
        }
        async fn get_vehicles_by_depot_name(
            &self,
            _depot_name: &str,
        ) -> AppResult<Vec<crate::models::DepotVehicleSummary>> {
            Ok(Vec::new())
        }
        async fn get_vehicles_by_depot_id(
            &self,
            _depot_id: &str,
        ) -> AppResult<Vec<crate::models::DepotVehicleSummary>> {
            Ok(Vec::new())
        }
        async fn get_depot_names(&self) -> AppResult<Vec<String>> {
            Ok(Vec::new())
        }
        async fn get_depot_ids(&self) -> AppResult<Vec<String>> {
            Ok(Vec::new())
        }
        async fn get_depot_name_by_id(&self, _depot_id: String) -> AppResult<String> {
            Err(AppError::NotFound("n/a".into()))
        }
        async fn clear_depot_cache(&self) -> AppResult<()> {
            Ok(())
        }
        async fn get_vehicle_operation_data(
            &self,
            _fleet_no: &str,
        ) -> AppResult<crate::models::VehicleOperationData> {
            Err(AppError::NotFound("n/a".into()))
        }
        async fn verify_vehicle(&self, _vehicle_no: &str) -> AppResult<bool> {
            Ok(false)
        }
        async fn get_chennai_waybills_by_route_id(
            &self,
            _route_id: &str,
            _vehicle_number: Option<&str>,
        ) -> AppResult<Vec<crate::models::VehicleData>> {
            Ok(Vec::new())
        }
        async fn get_chennai_waybill_by_waybill_and_trip(
            &self,
            _waybill_no: &str,
            _trip_number: i32,
        ) -> AppResult<Vec<crate::models::VehicleData>> {
            Ok(Vec::new())
        }
        async fn get_routes_served_today(
            &self,
        ) -> AppResult<Vec<crate::models::RouteLastScheduleTime>> {
            Ok(Vec::new())
        }
        async fn get_vehicles_by_service_tier(
            &self,
            _gtfs_id: &str,
            _service_tier: &str,
        ) -> AppResult<Vec<String>> {
            Ok(Vec::new())
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn lookup_row(token: &str, designation: Option<&str>, entity_id: i64) -> EmployeeLookupRow {
        EmployeeLookupRow {
            token_no: Some(token.to_string()),
            first_name: "Test".to_string(),
            last_name: None,
            mobile_no: Some("9000000000".to_string()),
            entity_id,
            designation_name: designation.map(|s| s.to_string()),
        }
    }

    fn build_service(
        emp: StubEmployeeReader,
        veh: StubVehicleReader,
    ) -> (Arc<StubEmployeeReader>, DBFleetOperatorService) {
        // `pool` is never touched by the mobile-number path; a lazy pool against an
        // unreachable host is fine for these tests. (Email path is exercised by manual
        // curl tests against a real DB elsewhere.)
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://nobody:none@127.0.0.1:1/none")
            .expect("lazy pool builds");
        let emp_arc = Arc::new(emp);
        let svc = DBFleetOperatorService::new(
            pool,
            emp_arc.clone() as Arc<dyn EmployeeReader>,
            Arc::new(veh) as Arc<dyn VehicleDataReader>,
        );
        (emp_arc, svc)
    }

    fn mobile_req(mobile: Option<&str>, token: Option<&str>) -> EmployeeLoginRequest {
        EmployeeLoginRequest {
            auth_type: Some(AuthType::MobileNumber),
            email_hash: None,
            password_hash: None,
            mobile_no: mobile.map(|s| s.to_string()),
            token_no: token.map(|s| s.to_string()),
        }
    }

    // ── map_designation_to_role ─────────────────────────────────────────────

    #[test]
    fn role_mapper_handles_real_designations() {
        // primary `designations` (LOWER()-ed in SQL)
        assert_eq!(map_designation_to_role(Some("driver")), Some(Role::Driver));
        assert_eq!(
            map_designation_to_role(Some("conductor")),
            Some(Role::Conductor)
        );
        assert_eq!(
            map_designation_to_role(Some("driver-conductor")),
            Some(Role::DriverConductor),
            "primary 'Driver-Conductor' must NOT be swallowed by the conductor branch"
        );
        assert_eq!(
            map_designation_to_role(Some("admin manager")),
            None,
            "Admin Manager is office staff, not a depot manager"
        );
        assert_eq!(map_designation_to_role(Some("admin staff")), None);

        // designations_internal
        assert_eq!(
            map_designation_to_role(Some("depot_manager")),
            Some(Role::Manager)
        );

        // misc / edge cases
        assert_eq!(map_designation_to_role(None), None);
        assert_eq!(map_designation_to_role(Some("")), None);
        assert_eq!(map_designation_to_role(Some("   ")), None);
        assert_eq!(
            map_designation_to_role(Some("DRIVER")),
            Some(Role::Driver),
            "case-insensitive"
        );
        assert_eq!(
            map_designation_to_role(Some("ticket inspector")),
            None,
            "unknown designations should not be force-bucketed"
        );
    }

    // ── login dispatch ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn missing_auth_type_returns_typed_error() {
        let (_, svc) = build_service(
            StubEmployeeReader::new(None, None),
            StubVehicleReader { depot: None },
        );
        let req = EmployeeLoginRequest {
            auth_type: None,
            email_hash: None,
            password_hash: None,
            mobile_no: None,
            token_no: None,
        };
        let resp = svc.login("kolkata_bus", &req, false).await.unwrap();
        assert!(!resp.verified);
        assert_eq!(resp.error, Some(EmployeeLoginError::MissingAuthType));
        assert!(resp.token.is_none());
        assert!(resp.role.is_none());
    }

    #[tokio::test]
    async fn mobile_missing_mobile_no_returns_typed_error() {
        let (_, svc) = build_service(
            StubEmployeeReader::new(None, None),
            StubVehicleReader { depot: None },
        );
        let resp = svc
            .login("kolkata_bus", &mobile_req(None, Some("KDEMP001")), false)
            .await
            .unwrap();
        assert!(!resp.verified);
        assert_eq!(resp.error, Some(EmployeeLoginError::MissingMobileNo));
    }

    #[tokio::test]
    async fn mobile_blank_mobile_no_returns_typed_error() {
        let (_, svc) = build_service(
            StubEmployeeReader::new(None, None),
            StubVehicleReader { depot: None },
        );
        let resp = svc
            .login("kolkata_bus", &mobile_req(Some("   "), None), false)
            .await
            .unwrap();
        assert_eq!(resp.error, Some(EmployeeLoginError::MissingMobileNo));
    }

    #[tokio::test]
    async fn chennai_primary_hit_short_circuits_internal() {
        let (emp_arc, svc) = build_service(
            StubEmployeeReader::new(
                Some(lookup_row("O30007", Some("driver"), 42)),
                Some(lookup_row("OTHER", Some("conductor"), 99)), // must not be reached
            ),
            StubVehicleReader { depot: None },
        );
        let resp = svc
            .login(
                PRIMARY_GTFS_ID,
                &mobile_req(Some("9361392963"), Some("O30007")),
                false,
            )
            .await
            .unwrap();
        assert!(resp.verified);
        assert_eq!(resp.token.as_deref(), Some("O30007"));
        assert_eq!(resp.role, Some(Role::Driver));
        assert_eq!(emp_arc.primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            emp_arc.internal_calls.load(Ordering::SeqCst),
            0,
            "internal lookup must not run when primary hits"
        );
    }

    #[tokio::test]
    async fn chennai_primary_miss_falls_through_to_internal() {
        let (emp_arc, svc) = build_service(
            StubEmployeeReader::new(None, Some(lookup_row("MGR_TEST", Some("depot_manager"), 2))),
            StubVehicleReader { depot: None },
        );
        let resp = svc
            .login(
                PRIMARY_GTFS_ID,
                &mobile_req(Some("9000000001"), Some("MGR_TEST")),
                false,
            )
            .await
            .unwrap();
        assert!(resp.verified);
        assert_eq!(resp.role, Some(Role::Manager));
        assert_eq!(emp_arc.primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(emp_arc.internal_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn non_chennai_skips_primary() {
        let (emp_arc, svc) = build_service(
            StubEmployeeReader::new(
                Some(lookup_row("MUST_NOT_BE_USED", Some("driver"), 1)),
                Some(lookup_row("KDEMP001", Some("driver"), 5)),
            ),
            StubVehicleReader { depot: None },
        );
        let resp = svc
            .login(
                "kolkata_bus",
                &mobile_req(Some("7397438357"), Some("KDEMP001")),
                false,
            )
            .await
            .unwrap();
        assert!(resp.verified);
        assert_eq!(resp.token.as_deref(), Some("KDEMP001"));
        assert_eq!(
            emp_arc.primary_calls.load(Ordering::SeqCst),
            0,
            "primary must not run for non-chennai feeds"
        );
        assert_eq!(emp_arc.internal_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn token_mismatch_returns_typed_error() {
        let (_, svc) = build_service(
            StubEmployeeReader::new(Some(lookup_row("REAL_TOKEN", Some("driver"), 1)), None),
            StubVehicleReader { depot: None },
        );
        let resp = svc
            .login(
                PRIMARY_GTFS_ID,
                &mobile_req(Some("9361392963"), Some("WRONG")),
                false,
            )
            .await
            .unwrap();
        assert!(!resp.verified);
        assert_eq!(resp.error, Some(EmployeeLoginError::TokenMismatch));
        assert!(resp.token.is_none());
        assert!(resp.role.is_none());
    }

    #[tokio::test]
    async fn token_absent_skips_verification() {
        let (_, svc) = build_service(
            StubEmployeeReader::new(Some(lookup_row("REAL_TOKEN", Some("driver"), 1)), None),
            StubVehicleReader { depot: None },
        );
        let resp = svc
            .login(
                PRIMARY_GTFS_ID,
                &mobile_req(Some("9361392963"), None),
                false,
            )
            .await
            .unwrap();
        assert!(resp.verified);
        assert_eq!(resp.token.as_deref(), Some("REAL_TOKEN"));
    }

    #[tokio::test]
    async fn person_not_found_returns_typed_error() {
        let (_, svc) = build_service(
            StubEmployeeReader::new(None, None),
            StubVehicleReader { depot: None },
        );
        let resp = svc
            .login(
                PRIMARY_GTFS_ID,
                &mobile_req(Some("0000000001"), Some("X")),
                false,
            )
            .await
            .unwrap();
        assert!(!resp.verified);
        assert_eq!(resp.error, Some(EmployeeLoginError::PersonNotFound));
    }

    #[tokio::test]
    async fn with_metadata_populates_depot() {
        let (_, svc) = build_service(
            StubEmployeeReader::new(Some(lookup_row("O30007", Some("driver"), 42)), None),
            StubVehicleReader {
                depot: Some(DepotInfo {
                    name: "Poonamallee Depot EV".into(),
                    code: Some("PN".into()),
                }),
            },
        );
        let resp = svc
            .login(
                PRIMARY_GTFS_ID,
                &mobile_req(Some("9361392963"), Some("O30007")),
                true,
            )
            .await
            .unwrap();
        let meta = resp
            .metadata
            .expect("metadata populated when with_metadata=true");
        assert_eq!(meta.depot_name.as_deref(), Some("Poonamallee Depot EV"));
        assert_eq!(meta.depot_code.as_deref(), Some("PN"));
    }

    #[tokio::test]
    async fn with_metadata_missing_depot_is_tolerated() {
        let (_, svc) = build_service(
            StubEmployeeReader::new(Some(lookup_row("O30007", Some("driver"), 9999)), None),
            StubVehicleReader { depot: None },
        );
        let resp = svc
            .login(
                PRIMARY_GTFS_ID,
                &mobile_req(Some("9361392963"), Some("O30007")),
                true,
            )
            .await
            .unwrap();
        let meta = resp
            .metadata
            .expect("metadata still populated, depot fields null");
        assert_eq!(meta.depot_name, None);
        assert_eq!(meta.depot_code, None);
    }

    #[tokio::test]
    async fn without_metadata_omits_metadata_field() {
        let (_, svc) = build_service(
            StubEmployeeReader::new(Some(lookup_row("O30007", Some("driver"), 42)), None),
            StubVehicleReader {
                depot: Some(DepotInfo {
                    name: "X".into(),
                    code: Some("X".into()),
                }),
            },
        );
        let resp = svc
            .login(
                PRIMARY_GTFS_ID,
                &mobile_req(Some("9361392963"), Some("O30007")),
                false,
            )
            .await
            .unwrap();
        assert!(resp.metadata.is_none());
    }

    // Suppress dead-code warning on MockDBVehicleReader so this test module can import it freely.
    #[allow(dead_code)]
    fn _ensure_mock_visible() -> MockDBVehicleReader {
        MockDBVehicleReader::new()
    }
}
