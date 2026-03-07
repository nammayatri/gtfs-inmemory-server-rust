use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

use sqlx::PgPool;
use tracing::{error, info};

use crate::models::{BusSchedule, VehicleData, VehicleDataWithRouteId, WaybillStatus};
use crate::tools::error::{AppError, AppResult};

#[async_trait]
pub trait VehicleDataReaderInternal: Send + Sync {
    async fn is_vehicle_in_internal(&self, vehicle_no: &str, gtfs_id: &str) -> bool;
    async fn get_vehicle_data(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId>;
    async fn get_chennai_waybills_by_route_id(
        &self,
        route_id: &str,
        gtfs_id: &str,
    ) -> AppResult<Vec<VehicleData>>;
    async fn get_chennai_waybill_by_waybill_and_trip(
        &self,
        waybill_no: &str,
        trip_number: i32,
        gtfs_id: &str,
    ) -> AppResult<Vec<VehicleData>>;
}

// Mock implementation for local testing without a database
pub struct MockDBVehicleReaderInternal;

impl MockDBVehicleReaderInternal {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MockDBVehicleReaderInternal {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VehicleDataReaderInternal for MockDBVehicleReaderInternal {
    async fn is_vehicle_in_internal(&self, _vehicle_no: &str, _gtfs_id: &str) -> bool {
        false
    }

    async fn get_vehicle_data(
        &self,
        _vehicle_no: &str,
        _gtfs_id: &str,
        _trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_chennai_waybills_by_route_id(
        &self,
        _route_id: &str,
        _gtfs_id: &str,
    ) -> AppResult<Vec<VehicleData>> {
        Ok(Vec::new())
    }

    async fn get_chennai_waybill_by_waybill_and_trip(
        &self,
        _waybill_no: &str,
        _trip_number: i32,
        _gtfs_id: &str,
    ) -> AppResult<Vec<VehicleData>> {
        Ok(Vec::new())
    }
}

pub struct DBVehicleReaderInternal {
    pool: Option<PgPool>,
    waybills_by_route_cache: Arc<RwLock<HashMap<String, (Vec<VehicleData>, SystemTime)>>>,
}

const WAYBILL_ROUTE_CACHE_DURATION: u64 = 300;

impl DBVehicleReaderInternal {
    pub fn new(pool: PgPool) -> Self {
        Self { 
            pool: Some(pool),
            waybills_by_route_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Used when no DB is available (local mock / no DATABASE_URL).
    /// is_vehicle_in_internal will always return false, so get_vehicle_data is never reached.
    pub fn new_disconnected() -> Self {
        Self { 
            pool: None,
            waybills_by_route_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn pool(&self) -> AppResult<&PgPool> {
        self.pool.as_ref().ok_or_else(|| {
            AppError::Internal("DBVehicleReaderInternal: no pool available".to_string())
        })
    }

    fn is_waybills_by_route_cache_expired(&self, timestamp: SystemTime) -> bool {
        let elapsed = timestamp.elapsed().unwrap_or_default();
        elapsed >= Duration::from_secs(WAYBILL_ROUTE_CACHE_DURATION)
    }

    fn get_waybills_by_route_cache_key(&self, gtfs_id: &str, route_id: &str) -> String {
        format!("{}_{}", gtfs_id, route_id)
    }

    /// Returns true if vehicle_no exists in vehicles_internal for the given gtfs_id.
    /// Any DB error is treated as "not found" so the caller falls back gracefully.
    async fn is_vehicle_in_internal_impl(&self, vehicle_no: &str, gtfs_id: &str) -> bool {
        let pool = match &self.pool {
            Some(p) => p,
            None => return false,
        };

        let query = r#"
            SELECT 1
            FROM vehicles_internal
            WHERE fleet_no = $1
              AND gtfs_id    = $2
              AND deleted    = false
            LIMIT 1
        "#;

        match sqlx::query_scalar::<_, i32>(query)
            .bind(vehicle_no)
            .bind(gtfs_id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(_)) => {
                info!(
                    "vehicle_no={} found in vehicles_internal for gtfs_id={}",
                    vehicle_no, gtfs_id
                );
                true
            }
            Ok(None) => false,
            Err(e) => {
                error!(
                    "is_vehicle_in_internal query failed for vehicle_no={} gtfs_id={}: {}",
                    vehicle_no, gtfs_id, e
                );
                false
            }
        }
    }

    /// Full vehicle data fetch against _internal tables, mirroring DBVehicleReader::get_vehicle_data.
    async fn get_vehicle_data_impl(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId> {
        let (waybill_result, waybill_status) =
            self.get_waybill_with_priority(vehicle_no, gtfs_id).await?;

        let mut schedule_map: HashMap<i64, Vec<BusSchedule>> = HashMap::new();

        match waybill_result {
            Some(vehicle_data) => {
                info!(
                    "Internal flow — vehicle_data: {:?}, status: {:?}",
                    vehicle_data, waybill_status
                );

                let (schedule_result, is_active_trip, remaining_trip_details) = self
                    .resolve_trip_data(vehicle_data.clone(), waybill_status, trip_number)
                    .await?;

                if let Some(ref remaining) = remaining_trip_details {
                    for row in remaining.iter() {
                        if let Some(key) = row.schedule_trip_id {
                            schedule_map
                                .entry(key)
                                .or_insert_with(Vec::new)
                                .push(row.clone());
                        }
                    }
                }
                if let Some(ref schedule) = schedule_result {
                    if let Some(key) = schedule.schedule_trip_id {
                        schedule_map
                            .entry(key)
                            .or_insert_with(Vec::new)
                            .insert(0, schedule.clone());
                    }
                }

                let mut vehicle_data_with_route_id = VehicleDataWithRouteId {
                    waybill_id: Some(vehicle_data.waybill_id),
                    waybill_no: Some(vehicle_data.waybill_no),
                    service_type: Some(vehicle_data.service_type),
                    vehicle_no: vehicle_data.vehicle_no,
                    schedule_no: Some(vehicle_data.schedule_no),
                    last_updated: vehicle_data.last_updated,
                    duty_date: vehicle_data.duty_date,
                    route_number: None,
                    route_id: None,
                    depot: None,
                    trip_number: None,
                    is_active_trip,
                    remaining_trip_details,
                    entity_remark: vehicle_data.entity_remark,
                    driver_code: vehicle_data.driver_code,
                    conductor_code: vehicle_data.conductor_code,
                    deleted: vehicle_data.deleted,
                    status: vehicle_data.status,
                    schedule_details: Some(schedule_map),
                    db_start_time: None,
                    db_end_time: None,
                    seat_layout_id: None,
                };

                if let Some(schedule) = schedule_result {
                    vehicle_data_with_route_id.trip_number = schedule.trip_number;
                    vehicle_data_with_route_id.route_id = Some(schedule.route_id.to_owned());
                    vehicle_data_with_route_id.depot = schedule.org_name.clone();
                    vehicle_data_with_route_id.route_number = schedule.route_number.clone();
                    vehicle_data_with_route_id.db_start_time = schedule.db_start_time.clone();
                    vehicle_data_with_route_id.db_end_time = schedule.db_end_time.clone();
                }

                Ok(vehicle_data_with_route_id)
            }
            None => {
                // Fallback: any waybill in _internal regardless of status
                let fallback_query = r#"
                    SELECT
                        w.waybill_id::text,
                        w.waybill_no::text,
                        w.service_type,
                        w.vehicle_no,
                        w.schedule_no,
                        w.updated_at::timestamptz AS last_updated,
                        w.duty_date,
                        w.schedule_trip_id::text,
                        w.is_flexi,
                        e.entity_remark::text AS entity_remark,
                        w.driver_token_no::text AS driver_code,
                        w.conductor_token_no::text AS conductor_code,
                        w.deleted AS deleted,
                        w.status AS status
                    FROM waybills_internal w
                    LEFT JOIN entities_internal e ON e.entity_id = w.entity_id AND e.gtfs_id = $2
                    WHERE w.vehicle_no = $1
                      AND w.gtfs_id   = $2
                    ORDER BY w.updated_at DESC
                    LIMIT 1
                "#;

                let minimal = match sqlx::query_as::<_, VehicleData>(fallback_query)
                    .bind(vehicle_no)
                    .bind(gtfs_id)
                    .fetch_optional(self.pool()?)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        error!(
                            "Internal status-agnostic query failed for {}: {}",
                            vehicle_no, e
                        );
                        None
                    }
                };

                let out = if let Some(m) = minimal {
                    VehicleDataWithRouteId {
                        waybill_id: Some(m.waybill_id),
                        waybill_no: Some(m.waybill_no),
                        service_type: Some(m.service_type),
                        vehicle_no: m.vehicle_no,
                        schedule_no: Some(m.schedule_no),
                        last_updated: m.last_updated,
                        duty_date: m.duty_date,
                        route_id: None,
                        route_number: None,
                        depot: None,
                        trip_number: None,
                        is_active_trip: false,
                        remaining_trip_details: None,
                        entity_remark: m.entity_remark,
                        driver_code: m.driver_code,
                        conductor_code: m.conductor_code,
                        deleted: m.deleted,
                        status: m.status,
                        schedule_details: None,
                        db_start_time: None,
                        db_end_time: None,
                        seat_layout_id: None,
                    }
                } else {
                    VehicleDataWithRouteId {
                        waybill_id: None,
                        waybill_no: None,
                        service_type: None,
                        vehicle_no: vehicle_no.to_string(),
                        schedule_no: None,
                        last_updated: None,
                        duty_date: None,
                        route_id: None,
                        route_number: None,
                        depot: None,
                        trip_number: None,
                        is_active_trip: false,
                        remaining_trip_details: None,
                        entity_remark: None,
                        driver_code: None,
                        conductor_code: None,
                        deleted: None,
                        status: None,
                        schedule_details: None,
                        db_start_time: None,
                        db_end_time: None,
                        seat_layout_id: None,
                    }
                };

                Ok(out)
            }
        }
    }

    // ─── Private helpers ───────────────────────────────────────────────────────

    async fn get_online_waybill(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<VehicleData>> {
        let query = r#"
            SELECT
                w.waybill_id::text,
                w.waybill_no::text,
                w.service_type,
                w.vehicle_no,
                w.schedule_no,
                w.updated_at::timestamptz AS last_updated,
                w.duty_date,
                w.schedule_trip_id::text,
                w.is_flexi,
                e.entity_remark::text AS entity_remark,
                w.driver_token_no::text AS driver_code,
                w.conductor_token_no::text AS conductor_code,
                w.deleted AS deleted,
                w.status AS status
            FROM waybills_internal w
            LEFT JOIN entities_internal e
                   ON e.entity_id = w.entity_id AND e.gtfs_id = $2
            WHERE w.vehicle_no = $1
              AND w.status     = 'online'
              AND w.deleted    = false
              AND w.gtfs_id   = $2
            ORDER BY w.updated_at DESC
            LIMIT 1
        "#;

        match sqlx::query_as::<_, VehicleData>(query)
            .bind(vehicle_no)
            .bind(gtfs_id)
            .fetch_optional(self.pool()?)
            .await
        {
            Ok(result) => Ok(result),
            Err(e) => {
                error!(
                    "Online waybill_internal query failed for vehicle_no={} gtfs_id={}: {}",
                    vehicle_no, gtfs_id, e
                );
                Ok(None)
            }
        }
    }

    async fn get_processed_new_waybill(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<VehicleData>> {
        let query = r#"
            SELECT
                w.waybill_id::text,
                w.waybill_no::text,
                w.service_type,
                w.vehicle_no,
                w.schedule_no,
                w.updated_at::timestamptz AS last_updated,
                w.duty_date,
                w.schedule_trip_id::text,
                w.is_flexi,
                e.entity_remark::text AS entity_remark,
                w.driver_token_no::text AS driver_code,
                w.conductor_token_no::text AS conductor_code,
                w.deleted AS deleted,
                w.status AS status
            FROM waybills_internal w
            LEFT JOIN entities_internal e
                   ON e.entity_id = w.entity_id AND e.gtfs_id = $2
            WHERE w.vehicle_no = $1
              AND w.status IN ('Processed', 'New', 'Closed')
              AND w.deleted    = false
              AND w.gtfs_id   = $2
            ORDER BY
                CASE
                    WHEN w.status = 'Processed' THEN 1
                    WHEN w.status = 'New'       THEN 2
                    WHEN w.status = 'Closed'    THEN 3
                END,
                w.updated_at DESC
            LIMIT 1
        "#;

        match sqlx::query_as::<_, VehicleData>(query)
            .bind(vehicle_no)
            .bind(gtfs_id)
            .fetch_optional(self.pool()?)
            .await
        {
            Ok(result) => Ok(result),
            Err(e) => {
                error!(
                    "Processed/New waybill_internal query failed for vehicle_no={} gtfs_id={}: {}",
                    vehicle_no, gtfs_id, e
                );
                Ok(None)
            }
        }
    }

    async fn get_waybill_with_priority(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
    ) -> AppResult<(Option<VehicleData>, WaybillStatus)> {
        if let Some(w) = self.get_online_waybill(vehicle_no, gtfs_id).await? {
            return Ok((Some(w), WaybillStatus::Online));
        }
        if let Some(w) = self.get_processed_new_waybill(vehicle_no, gtfs_id).await? {
            return Ok((Some(w), WaybillStatus::ProcessedOrNew));
        }
        Ok((None, WaybillStatus::NotFound))
    }

    async fn resolve_trip_data(
        &self,
        waybill_data: VehicleData,
        status: WaybillStatus,
        trip_number: Option<i32>,
    ) -> AppResult<(Option<BusSchedule>, bool, Option<Vec<BusSchedule>>)> {
        match status {
            WaybillStatus::Online => {
                if waybill_data.is_flexi == Some(true) {
                    self.handle_flexi_trips(&waybill_data, trip_number).await
                } else {
                    self.handle_detail_trips(&waybill_data, false, trip_number)
                        .await
                }
            }
            WaybillStatus::ProcessedOrNew => {
                self.handle_detail_trips(&waybill_data, true, trip_number)
                    .await
            }
            WaybillStatus::NotFound => Ok((None, false, None)),
        }
    }

    async fn handle_flexi_trips(
        &self,
        waybill_data: &VehicleData,
        trip_number: Option<i32>,
    ) -> AppResult<(Option<BusSchedule>, bool, Option<Vec<BusSchedule>>)> {
        let flexi_query = if let Some(trip_num) = trip_number {
            format!(
                r#"
                SELECT
                    NULL::int AS stops_count,
                    route_number_id::text AS route_id,
                    schedule_number,
                    org_name::text AS org_name,
                    trip_number,
                    schedule_trip_id,
                    trip_start_time AS start_time,
                    trip_end_time AS end_time,
                    deleted,
                    is_active_trip,
                    trip_order,
                    start_time AS db_start_time,
                    end_time AS db_end_time
                FROM bus_schedule_trip_flexi_internal
                WHERE waybill_id = $1::bigint
                  AND trip_number >= {}
                  AND trip_type != 'dead-trip'
                ORDER BY trip_number ASC
                "#,
                trip_num
            )
        } else {
            r#"
                SELECT
                    NULL::int AS stops_count,
                    route_number_id::text AS route_id,
                    schedule_number,
                    org_name::text AS org_name,
                    trip_number,
                    schedule_trip_id,
                    trip_start_time AS start_time,
                    trip_end_time AS end_time,
                    deleted,
                    is_active_trip,
                    trip_order,
                    start_time AS db_start_time,
                    end_time AS db_end_time
                FROM bus_schedule_trip_flexi_internal
                WHERE waybill_id = $1::bigint
                  AND trip_type != 'dead-trip'
                ORDER BY trip_number ASC
            "#
            .to_string()
        };

        let flexi_rows = match sqlx::query_as::<_, BusSchedule>(&flexi_query)
            .bind(waybill_data.waybill_id.parse::<i64>().unwrap_or(0))
            .fetch_all(self.pool()?)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                error!(
                    "Flexi_internal query failed for waybill_id {}: {}",
                    waybill_data.waybill_id, e
                );
                Vec::new()
            }
        };

        if !flexi_rows.is_empty() {
            return self.process_trip_rows(flexi_rows, false).await;
        }

        self.handle_detail_trips(waybill_data, false, trip_number)
            .await
    }

    async fn handle_detail_trips(
        &self,
        waybill_data: &VehicleData,
        force_first_active: bool,
        trip_number: Option<i32>,
    ) -> AppResult<(Option<BusSchedule>, bool, Option<Vec<BusSchedule>>)> {
        let detail_query = if let Some(trip_num) = trip_number {
            format!(
                r#"
                SELECT
                    NULL::int AS stops_count,
                    route_number_id::text AS route_id,
                    schedule_number,
                    org_name::text AS org_name,
                    trip_number,
                    schedule_trip_id,
                    trip_start_time AS start_time,
                    trip_end_time AS end_time,
                    start_time AS db_start_time,
                    end_time AS db_end_time,
                    deleted,
                    is_active_trip,
                    trip_order
                FROM bus_schedule_trip_detail_internal
                WHERE schedule_trip_id = $1::bigint
                  AND trip_number >= {}
                  AND trip_type != 'dead-trip'
                ORDER BY trip_number ASC
                "#,
                trip_num
            )
        } else {
            r#"
                SELECT
                    NULL::int AS stops_count,
                    route_number_id::text AS route_id,
                    schedule_number,
                    org_name::text AS org_name,
                    trip_number,
                    schedule_trip_id,
                    trip_start_time AS start_time,
                    trip_end_time AS end_time,
                    start_time AS db_start_time,
                    end_time AS db_end_time,
                    deleted,
                    is_active_trip,
                    trip_order
                FROM bus_schedule_trip_detail_internal
                WHERE schedule_trip_id = $1::bigint
                  AND trip_number >= (
                      SELECT COALESCE(
                          (SELECT trip_number FROM bus_schedule_trip_detail_internal
                           WHERE schedule_trip_id = $1::bigint
                             AND is_active_trip = true
                             AND trip_type != 'dead-trip'),
                          1))
                  AND trip_type != 'dead-trip'
                ORDER BY trip_number ASC
            "#
            .to_string()
        };

        let detail_rows = match sqlx::query_as::<_, BusSchedule>(&detail_query)
            .bind(&waybill_data.schedule_trip_id)
            .fetch_all(self.pool()?)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                error!(
                    "Detail_internal query failed for schedule_trip_id {:?}: {}",
                    waybill_data.schedule_trip_id, e
                );
                Vec::new()
            }
        };

        if !detail_rows.is_empty() {
            return self.process_trip_rows(detail_rows, force_first_active).await;
        }

        // Final fallback: bus_schedule_internal
        self.handle_schedule_fallback(&waybill_data.schedule_no).await
    }

    async fn handle_schedule_fallback(
        &self,
        schedule_no: &str,
    ) -> AppResult<(Option<BusSchedule>, bool, Option<Vec<BusSchedule>>)> {
        let query = r#"
            SELECT
                NULL::int AS stops_count,
                route_id::text AS route_id,
                schedule_number,
                NULL::text AS org_name,
                NULL::int AS trip_number,
                NULL::bigint AS schedule_trip_id,
                NULL::text AS start_time,
                NULL::text AS end_time,
                FALSE AS deleted,
                FALSE AS is_active_trip,
                NULL::int AS trip_order,
                NULL::text AS db_start_time,
                NULL::text AS db_end_time
            FROM bus_schedule_internal
            WHERE schedule_number = $1 AND deleted = false
        "#;

        let mut rows = match sqlx::query_as::<_, BusSchedule>(query)
            .bind(schedule_no)
            .fetch_all(self.pool()?)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                error!(
                    "bus_schedule_internal fallback failed for schedule_no={}: {}",
                    schedule_no, e
                );
                Vec::new()
            }
        };

        if !rows.is_empty() {
            self.enrich_route_numbers(&mut rows).await?;
            let first = rows.remove(0);
            let remaining = if rows.is_empty() { None } else { Some(rows) };
            Ok((Some(first), false, remaining))
        } else {
            Ok((None, false, None))
        }
    }

    async fn process_trip_rows(
        &self,
        mut rows: Vec<BusSchedule>,
        force_first_active: bool,
    ) -> AppResult<(Option<BusSchedule>, bool, Option<Vec<BusSchedule>>)> {
        if rows.is_empty() {
            return Ok((None, false, None));
        }

        self.enrich_route_numbers(&mut rows).await?;

        if force_first_active {
            let first = rows.remove(0);
            let remaining = if rows.is_empty() { None } else { Some(rows) };
            Ok((Some(first), true, remaining))
        } else {
            if let Some(active_idx) = rows.iter().position(|r| r.is_active_trip.unwrap_or(false)) {
                let active_trip = rows.remove(active_idx);
                let active_trip_num = active_trip.trip_number.unwrap_or(0);
                let remaining_trips: Vec<BusSchedule> = rows
                    .into_iter()
                    .filter(|t| t.trip_number.unwrap_or(0) > active_trip_num)
                    .collect();
                let remaining = if remaining_trips.is_empty() {
                    None
                } else {
                    Some(remaining_trips)
                };
                Ok((Some(active_trip), true, remaining))
            } else {
                let first = rows.remove(0);
                let remaining = if rows.is_empty() { None } else { Some(rows) };
                Ok((Some(first), false, remaining))
            }
        }
    }

    /// Enrich route_number on each schedule row from route_internal.
    async fn enrich_route_numbers(&self, schedules: &mut [BusSchedule]) -> AppResult<()> {
        let route_ids: Vec<String> = schedules
            .iter()
            .map(|s| s.route_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        if route_ids.is_empty() {
            return Ok(());
        }

        let placeholders: Vec<String> =
            (1..=route_ids.len()).map(|i| format!("${}", i)).collect();
        let placeholders_str = placeholders.join(",");

        let query = format!(
            "SELECT route_id::text AS route_id, route_number FROM route_internal WHERE route_id::text IN ({})",
            placeholders_str
        );

        let mut qb = sqlx::query_as::<_, (String, Option<String>)>(&query);
        for id in &route_ids {
            qb = qb.bind(id);
        }

        let mappings = match qb.fetch_all(self.pool()?).await {
            Ok(m) => m,
            Err(e) => {
                error!("enrich_route_numbers (internal) failed: {}", e);
                return Ok(());
            }
        };

        let map: HashMap<String, Option<String>> = mappings.into_iter().collect();
        for s in schedules.iter_mut() {
            if let Some(num) = map.get(&s.route_id) {
                s.route_number = num.clone();
            }
        }

        Ok(())
    }

    async fn get_chennai_waybills_by_route_id_impl(
        &self,
        route_id: &str,
        gtfs_id: &str,
    ) -> AppResult<Vec<VehicleData>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => {
                return Ok(Vec::new()); // No pool available (mock mode)
            }
        };

        let cache_key = self.get_waybills_by_route_cache_key(gtfs_id, route_id);

        // Check cache first
        {
            let cache = self.waybills_by_route_cache.read().await;
            if let Some((data, ts)) = cache.get(&cache_key) {
                if !self.is_waybills_by_route_cache_expired(*ts) {
                    info!("internal get_chennai_waybills_by_route_id cache HIT for route_id={}", route_id);
                    return Ok(data.clone());
                }
            }
        }

        let query = r#"
            WITH base AS (
                SELECT
                    w.waybill_id::text,
                    w.waybill_no::text,
                    w.service_type,
                    w.vehicle_no,
                    w.schedule_no,
                    w.updated_at::timestamptz AS last_updated,
                    w.duty_date,
                    w.schedule_trip_id::text,
                    e.entity_remark::text,
                    w.driver_token_no::text AS driver_code,
                    w.conductor_token_no::text AS conductor_code,
                    w.deleted,
                    w.status,
                    w.is_flexi,
                    COALESCE(bstd.start_time, bstf.start_time) AS db_start_time,
                    COALESCE(bstd.trip_start_time, bstf.trip_start_time)::text AS start_time_epoch,
                    COALESCE(bstd.trip_number, bstf.trip_number)::int AS trip_number,
                    COALESCE(bstd.is_active_trip, bstf.is_active_trip) AS is_active_trip
                FROM waybills_internal w
                LEFT JOIN entities_internal e
                    ON e.entity_id = w.entity_id
                    AND e.gtfs_id = $2
                LEFT JOIN bus_schedule_trip_detail_internal bstd
                    ON  w.is_flexi = false
                    AND bstd.schedule_trip_id = w.schedule_trip_id::bigint
                    AND bstd.route_number_id::text = $1
                    AND bstd.gtfs_id = $2
                    AND bstd.trip_type <> 'dead-trip'
                LEFT JOIN bus_schedule_trip_flexi_internal bstf
                    ON  w.is_flexi = true
                    AND bstf.schedule_trip_id = w.schedule_trip_id::bigint
                    AND bstf.route_number_id::text = $1
                    AND bstf.gtfs_id = $2
                    AND bstf.trip_type <> 'dead-trip'
                WHERE
                    w.status = 'online'
                    AND w.deleted = false
                    AND w.gtfs_id = $2
                    AND (
                        (w.is_flexi = false AND bstd.schedule_trip_id IS NOT NULL)
                        OR
                        (w.is_flexi = true  AND bstf.schedule_trip_id IS NOT NULL)
                    )
            )

            SELECT *
            FROM (
                SELECT *,
                    MAX(CASE WHEN is_active_trip THEN 1 ELSE 0 END)
                    OVER (
                        PARTITION BY waybill_no
                        ORDER BY trip_number
                        ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                    ) AS seen_active
                FROM base
            ) t
            WHERE seen_active = 1
            ORDER BY waybill_no, trip_number;
        "#;

        match sqlx::query_as::<_, VehicleData>(query)
            .bind(route_id)
            .bind(gtfs_id)
            .fetch_all(pool)
            .await
        {
            Ok(rows) => {
                info!(
                    "Chennai internal direct query: found {} waybills for route_id={}",
                    rows.len(),
                    route_id
                );

                // Update cache
                {
                    let mut cache = self.waybills_by_route_cache.write().await;
                    cache.insert(cache_key, (rows.clone(), SystemTime::now()));
                }

                Ok(rows)
            }
            Err(e) => {
                error!(
                    "Internal get_chennai_waybills_by_route_id failed for route_id={}: {}",
                    route_id, e
                );
                // Fail gracefully so we just don't append internal records
                Ok(Vec::new())
            }
        }
    }

    async fn get_chennai_waybill_by_waybill_and_trip_impl(
        &self,
        waybill_no: &str,
        trip_number: i32,
        gtfs_id: &str,
    ) -> AppResult<Vec<VehicleData>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => {
                return Ok(Vec::new()); // No pool available (mock mode)
            }
        };

        let query = r#"
            SELECT
                w.waybill_id::text,
                w.waybill_no::text,
                w.service_type,
                w.vehicle_no,
                w.schedule_no,
                w.updated_at::timestamptz AS last_updated,
                w.duty_date,
                w.schedule_trip_id::text,
                e.entity_remark::text,
                w.driver_token_no::text AS driver_code,
                w.conductor_token_no::text AS conductor_code,
                w.deleted,
                w.status,
                w.is_flexi,
                COALESCE(bstd.start_time, bstf.start_time) AS db_start_time,
                COALESCE(bstd.trip_start_time, bstf.trip_start_time)::text AS start_time_epoch,
                COALESCE(bstd.trip_number, bstf.trip_number)::int AS trip_number
            FROM waybills_internal w
            LEFT JOIN entities_internal e
                ON e.entity_id = w.entity_id
                AND e.gtfs_id = $3
            LEFT JOIN bus_schedule_trip_detail_internal bstd
                ON w.is_flexi = false
                AND bstd.schedule_trip_id = w.schedule_trip_id::bigint
                AND bstd.trip_number = $2
                AND bstd.gtfs_id = $3
                AND bstd.trip_type <> 'dead-trip'
            LEFT JOIN bus_schedule_trip_flexi_internal bstf
                ON w.is_flexi = true
                AND bstf.waybill_id = w.waybill_id::bigint
                AND bstf.trip_number = $2
                AND bstf.gtfs_id = $3
                AND bstf.trip_type <> 'dead-trip'
            WHERE
                w.waybill_no::text = $1
                AND w.gtfs_id = $3
                AND w.status = 'online'
                AND w.deleted = false
                AND (
                    (w.is_flexi = false AND bstd.schedule_trip_id IS NOT NULL)
                    OR
                    (w.is_flexi = true  AND bstf.schedule_trip_id IS NOT NULL)
                )
            ORDER BY w.waybill_no, trip_number;
        "#;

        match sqlx::query_as::<_, VehicleData>(query)
            .bind(waybill_no)
            .bind(trip_number)
            .bind(gtfs_id)
            .fetch_all(pool)
            .await
        {
            Ok(rows) => {
                info!(
                    "Internal Chennai waybill+trip query: found {} rows for waybill_no={}, trip_number={}, gtfs_id={}",
                    rows.len(),
                    waybill_no,
                    trip_number,
                    gtfs_id
                );
                Ok(rows)
            }
            Err(e) => {
                error!(
                    "Internal get_chennai_waybill_by_waybill_and_trip failed for waybill_no={}, trip_number={}, gtfs_id={}: {}",
                    waybill_no, trip_number, gtfs_id, e
                );
                // Fail gracefully so external results are still returned
                Ok(Vec::new())
            }
        }
    }
}

#[async_trait]
impl VehicleDataReaderInternal for DBVehicleReaderInternal {
    async fn is_vehicle_in_internal(&self, vehicle_no: &str, gtfs_id: &str) -> bool {
        self.is_vehicle_in_internal_impl(vehicle_no, gtfs_id).await
    }

    async fn get_vehicle_data(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId> {
        self.get_vehicle_data_impl(vehicle_no, gtfs_id, trip_number).await
    }

    async fn get_chennai_waybills_by_route_id(
        &self,
        route_id: &str,
        gtfs_id: &str,
    ) -> AppResult<Vec<VehicleData>> {
        self.get_chennai_waybills_by_route_id_impl(route_id, gtfs_id).await
    }

    async fn get_chennai_waybill_by_waybill_and_trip(
        &self,
        waybill_no: &str,
        trip_number: i32,
        gtfs_id: &str,
    ) -> AppResult<Vec<VehicleData>> {
        self.get_chennai_waybill_by_waybill_and_trip_impl(waybill_no, trip_number, gtfs_id).await
    }
}
