use async_trait::async_trait;
use serde::Serialize;
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

pub enum TripAction {
    Start,
    End,
}

#[derive(Debug, Clone)]
struct WaybillRow {
    waybill_id: i64,
    waybill_no: String,
    vehicle_no: String,
    conductor_token_no: Option<String>,
    driver_token_no: Option<String>,
    schedule_trip_id: Option<i64>,
    is_flexi: bool,
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
        is_obu: bool,
    ) -> AppResult<VerifyResponse>;
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
        _is_obu: bool,
    ) -> AppResult<VerifyResponse> {
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
                waybill_id,
                waybill_no,
                vehicle_no,
                conductor_token_no,
                driver_token_no,
                schedule_trip_id,
                is_flexi
            FROM waybills_internal
            WHERE gtfs_id = $1
              AND status = 'online'
              AND deleted = false
        "#;

        let row: Option<(i64, String, String, Option<String>, Option<String>, Option<i64>, bool)> =
            match anchor {
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
            Some((waybill_id, waybill_no, vehicle_no, conductor_token_no, driver_token_no, schedule_trip_id, is_flexi)) => {
                Ok(WaybillRow {
                    waybill_id,
                    waybill_no,
                    vehicle_no,
                    conductor_token_no,
                    driver_token_no,
                    schedule_trip_id,
                    is_flexi,
                })
            }
            None => Err(AppError::NotFound(
                "No active (online) waybill found for the provided anchor.".to_string(),
            )),
        }
    }

    // ── Trip count ───────────────────────────────────────────────────────────

    async fn get_trip_count(&self, waybill: &WaybillRow) -> AppResult<i64> {
        if waybill.is_flexi {
            let count: i64 = sqlx::query_scalar(
                r#"
                SELECT COUNT(*)
                FROM bus_schedule_trip_flexi_internal
                WHERE waybill_id = $1
                  AND trip_type != 'dead-trip'
                "#,
            )
            .bind(waybill.waybill_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                error!("get_trip_count (flexi) failed for waybill_id={}: {}", waybill.waybill_id, e);
                AppError::Internal(e.to_string())
            })?;
            Ok(count)
        } else {
            let schedule_trip_id = waybill.schedule_trip_id.ok_or_else(|| {
                AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
            })?;
            let count: i64 = sqlx::query_scalar(
                r#"
                SELECT COUNT(*)
                FROM bus_schedule_trip_detail_internal
                WHERE schedule_trip_id = $1
                  AND trip_type != 'dead-trip'
                "#,
            )
            .bind(schedule_trip_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| {
                error!(
                    "get_trip_count (detail) failed for schedule_trip_id={}: {}",
                    schedule_trip_id, e
                );
                AppError::Internal(e.to_string())
            })?;
            Ok(count)
        }
    }

    // ── All trips ────────────────────────────────────────────────────────────

    async fn get_all_trips(
        &self,
        waybill: &WaybillRow,
    ) -> AppResult<Vec<(i32, String, bool)>> {
        // Returns Vec<(trip_number, route_id, is_active_trip)>
        if waybill.is_flexi {
            let rows: Vec<(i32, String, bool)> = sqlx::query_as(
                r#"
                SELECT
                    trip_number,
                    route_number_id::text AS route_id,
                    is_active_trip
                FROM bus_schedule_trip_flexi_internal
                WHERE waybill_id = $1
                  AND trip_type != 'dead-trip'
                ORDER BY trip_number ASC
                "#,
            )
            .bind(waybill.waybill_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                error!("get_all_trips (flexi) failed for waybill_id={}: {}", waybill.waybill_id, e);
                AppError::Internal(e.to_string())
            })?;
            Ok(rows)
        } else {
            let schedule_trip_id = waybill.schedule_trip_id.ok_or_else(|| {
                AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
            })?;
            let rows: Vec<(i32, String, bool)> = sqlx::query_as(
                r#"
                SELECT
                    trip_number,
                    route_number_id::text AS route_id,
                    is_active_trip
                FROM bus_schedule_trip_detail_internal
                WHERE schedule_trip_id = $1
                  AND trip_type != 'dead-trip'
                ORDER BY trip_number ASC
                "#,
            )
            .bind(schedule_trip_id)
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
                WHERE waybill_id = $1
                  AND trip_number = $2
                  AND trip_type != 'dead-trip'
                LIMIT 1
                "#,
            )
            .bind(waybill.waybill_id)
            .bind(trip_number)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AppError::Internal(e.to_string()))?;
            r.is_some()
        } else {
            let schedule_trip_id = waybill.schedule_trip_id.ok_or_else(|| {
                AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
            })?;
            let r: Option<i32> = sqlx::query_scalar(
                r#"
                SELECT 1
                FROM bus_schedule_trip_detail_internal
                WHERE schedule_trip_id = $1
                  AND trip_number = $2
                  AND trip_type != 'dead-trip'
                LIMIT 1
                "#,
            )
            .bind(schedule_trip_id)
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
                                trip_start_time = CASE WHEN trip_number = $2 THEN $3 ELSE trip_start_time END,
                                sync_start_time = CASE WHEN trip_number = $2 THEN $4 ELSE sync_start_time END
                            WHERE waybill_id = $1
                              AND trip_type != 'dead-trip'
                            "#,
                        )
                        .bind(waybill.waybill_id)
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
                            SET is_active_trip = (trip_number = $2)
                            WHERE waybill_id = $1
                              AND trip_type != 'dead-trip'
                            "#,
                        )
                        .bind(waybill.waybill_id)
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
                    let schedule_trip_id = waybill.schedule_trip_id.ok_or_else(|| {
                        AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
                    })?;
                    if let Some(ts) = timestamp {
                        sqlx::query(
                            r#"
                            UPDATE bus_schedule_trip_detail_internal
                            SET is_active_trip  = (trip_number = $2),
                                trip_start_time = CASE WHEN trip_number = $2 THEN $3 ELSE trip_start_time END,
                                sync_start_time = CASE WHEN trip_number = $2 THEN $4 ELSE sync_start_time END
                            WHERE schedule_trip_id = $1
                              AND trip_type != 'dead-trip'
                            "#,
                        )
                        .bind(schedule_trip_id)
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
                            SET is_active_trip = (trip_number = $2)
                            WHERE schedule_trip_id = $1
                              AND trip_type != 'dead-trip'
                            "#,
                        )
                        .bind(schedule_trip_id)
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
                            WHERE waybill_id = $1
                            "#,
                        )
                        .bind(waybill.waybill_id)
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
                            WHERE waybill_id = $1
                            "#,
                        )
                        .bind(waybill.waybill_id)
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
                    let schedule_trip_id = waybill.schedule_trip_id.ok_or_else(|| {
                        AppError::NotFound("Waybill has no schedule_trip_id.".to_string())
                    })?;
                    if let Some(ts) = timestamp {
                        sqlx::query(
                            r#"
                            UPDATE bus_schedule_trip_detail_internal
                            SET is_active_trip = false,
                                trip_end_time  = CASE WHEN is_active_trip THEN $2 ELSE trip_end_time END,
                                sync_end_time  = CASE WHEN is_active_trip THEN $3 ELSE sync_end_time END
                            WHERE schedule_trip_id = $1
                            "#,
                        )
                        .bind(schedule_trip_id)
                        .bind(ts)
                        .bind(sync_now)
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
                            SET is_active_trip = false
                            WHERE schedule_trip_id = $1
                            "#,
                        )
                        .bind(schedule_trip_id)
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
        }
        Ok(())
    }

    // ── Route info cache ─────────────────────────────────────────────────────

    async fn get_route_infos(
        &self,
        route_ids: &[String],
    ) -> AppResult<HashMap<String, RouteInfo>> {
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
            "SELECT route_id::text, route_number, route_name FROM route_internal WHERE route_id::text IN ({})",
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
            let info = RouteInfo { route_number, route_name };
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
        let number_of_trips = self.get_trip_count(&waybill).await?;

        Ok(CurrentOperationResponse {
            waybill_no: waybill.waybill_no,
            vehicle_number: waybill.vehicle_no,
            conductor_token: waybill.conductor_token_no,
            driver_token: waybill.driver_token_no,
            number_of_trips,
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
        self.validate_trip_exists(&waybill, trip_number).await?;
        self.apply_trip_action(&waybill, &action, trip_number, timestamp).await?;
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
            .map(|(_, route_id, _)| route_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let route_map = self.get_route_infos(&route_ids).await?;

        let mut history: Vec<TripData> = Vec::new();
        let mut current: Option<TripData> = None;
        let mut upcoming: Vec<TripData> = Vec::new();

        for (trip_number, route_id, is_active_trip) in raw_trips {
            let info = route_map.get(&route_id);
            let trip_data = TripData {
                trip_number,
                route_id: Some(route_id),
                route_number: info.and_then(|i| i.route_number.clone()),
                route_name: info.and_then(|i| i.route_name.clone()),
                is_active_trip,
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

    async fn verify(
        &self,
        gtfs_id: &str,
        token: &str,
        device_serial_no: &str,
        is_obu: bool,
    ) -> AppResult<VerifyResponse> {
        // Check employee exists by token_no
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
            error!("verify employee check failed for token={} gtfs_id={}: {}", token, gtfs_id, e);
            AppError::Internal(e.to_string())
        })?
        .map(|_: i32| true)
        .unwrap_or(false);

        if !employee_ok {
            return Ok(VerifyResponse { verified: false });
        }

        // Check device exists
        let device_ok: bool = if is_obu {
            sqlx::query_scalar(
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
                error!("verify OBU check failed for obu_id={} gtfs_id={}: {}", device_serial_no, gtfs_id, e);
                AppError::Internal(e.to_string())
            })?
            .map(|_: i32| true)
            .unwrap_or(false)
        } else {
            sqlx::query_scalar(
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
                error!("verify ETM check failed for etm_serial_no={} gtfs_id={}: {}", device_serial_no, gtfs_id, e);
                AppError::Internal(e.to_string())
            })?
            .map(|_: i32| true)
            .unwrap_or(false)
        };

        Ok(VerifyResponse { verified: device_ok })
    }
}
