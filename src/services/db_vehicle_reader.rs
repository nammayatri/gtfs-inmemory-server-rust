use async_trait::async_trait;
use chrono::{FixedOffset, NaiveTime, TimeZone, Utc, Timelike};
use sqlx::postgres::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::{debug, error, info};

use crate::environment::AppConfig;
use crate::models::{
    BusSchedule, DepotVehicleSummary, MinimalVehicleData, VehicleData, VehicleDataWithRouteId,
    VehicleOperationData, WaybillStatus,
};
use crate::tools::error::{AppError, AppResult};

// Depot cache structure
struct DepotCache {
    depot_names: Option<(Vec<String>, SystemTime)>,
    depot_ids: Option<(Vec<String>, SystemTime)>,
    depot_name_by_id: HashMap<String, (String, SystemTime)>,
    vehicles_by_depot_name: HashMap<String, (Vec<DepotVehicleSummary>, SystemTime)>,
    vehicles_by_depot_id: HashMap<String, (Vec<DepotVehicleSummary>, SystemTime)>,
}

// Vehicle pool cache structure
struct VehiclePoolCache {
    all_vehicles: Option<(std::collections::HashSet<String>, SystemTime)>,
}

// Waybills by route cache structure
struct WaybillsByRouteCache {
    waybills_by_route: HashMap<String, (Vec<VehicleData>, SystemTime)>,
}

const DB_ACTIVE_TRIP_STALE_HOURS: i64 = 36; // If active trip started more than 36 hours ago, consider it stale

#[async_trait]
pub trait VehicleDataReader: Send + Sync {
    async fn get_vehicle_data(
        &self,
        vehicle_no: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId>;
    async fn get_vehicles_by_ids(
        &self,
        vehicle_nos: Vec<String>,
    ) -> AppResult<Vec<VehicleDataWithRouteId>>;
    async fn get_all_vehicles(&self) -> AppResult<Vec<VehicleData>>;
    async fn get_vehicles_by_service_type(&self, service_type: &str)
        -> AppResult<Vec<VehicleData>>;
    async fn get_vehicles_by_service_tier(
        &self,
        gtfs_id: &str,
        service_tier: &str,
    ) -> AppResult<Vec<String>>;
    async fn search_vehicles(&self, query: &str) -> AppResult<Vec<VehicleData>>;
    async fn get_vehicle_count(&self) -> AppResult<i64>;
    async fn get_vehicles_by_depot_name(
        &self,
        depot_name: &str,
    ) -> AppResult<Vec<DepotVehicleSummary>>;
    async fn get_vehicles_by_depot_id(&self, depot_id: &str)
        -> AppResult<Vec<DepotVehicleSummary>>;
    async fn get_depot_names(&self) -> AppResult<Vec<String>>;
    async fn get_depot_ids(&self) -> AppResult<Vec<String>>;
    async fn get_depot_name_by_id(&self, depot_id: String) -> AppResult<String>;
    async fn clear_depot_cache(&self) -> AppResult<()>;
    async fn get_vehicle_operation_data(&self, fleet_no: &str) -> AppResult<VehicleOperationData>;
    async fn verify_vehicle(&self, vehicle_no: &str) -> AppResult<bool>;
    async fn get_waybills_by_route_id(
        &self,
        gtfs_id: &str,
        route_id: &str,
    ) -> AppResult<Vec<VehicleData>>;
}

// Mock implementation for local testing without a database
pub struct MockDBVehicleReader;

impl Default for MockDBVehicleReader {
    fn default() -> Self {
        Self::new()
    }
}

impl MockDBVehicleReader {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl VehicleDataReader for MockDBVehicleReader {
    async fn get_vehicle_data(
        &self,
        _vehicle_no: &str,
        _trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_vehicles_by_ids(
        &self,
        _vehicle_nos: Vec<String>,
    ) -> AppResult<Vec<VehicleDataWithRouteId>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_all_vehicles(&self) -> AppResult<Vec<VehicleData>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_vehicles_by_service_type(
        &self,
        _service_type: &str,
    ) -> AppResult<Vec<VehicleData>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn search_vehicles(&self, _query: &str) -> AppResult<Vec<VehicleData>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_vehicle_count(&self) -> AppResult<i64> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_vehicles_by_depot_name(
        &self,
        _depot_name: &str,
    ) -> AppResult<Vec<DepotVehicleSummary>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_vehicles_by_depot_id(
        &self,
        _depot_id: &str,
    ) -> AppResult<Vec<DepotVehicleSummary>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_depot_names(&self) -> AppResult<Vec<String>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_depot_ids(&self) -> AppResult<Vec<String>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_depot_name_by_id(&self, _depot_id: String) -> AppResult<String> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }
    async fn clear_depot_cache(&self) -> AppResult<()> {
        Ok(())
    }

    async fn get_vehicle_operation_data(&self, _fleet_no: &str) -> AppResult<VehicleOperationData> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn verify_vehicle(&self, _vehicle_no: &str) -> AppResult<bool> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_waybills_by_route_id(
        &self,
        _gtfs_id: &str,
        _route_id: &str,
    ) -> AppResult<Vec<VehicleData>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }
    async fn get_vehicles_by_service_tier(
        &self,
        _gtfs_id: &str,
        _service_tier: &str,
    ) -> AppResult<Vec<String>> {
        // Mock returns empty list for now
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_get_vehicles_by_service_tier_returns_empty() {
        let reader = MockDBVehicleReader::new();
        let res = reader
            .get_vehicles_by_service_tier("any", "any")
            .await
            .unwrap();
        assert!(res.is_empty());
    }
}

pub struct DBVehicleReader {
    pool: PgPool,
    cache: Arc<RwLock<HashMap<String, (VehicleDataWithRouteId, SystemTime)>>>,
    cache_duration: Duration,
    refresh_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<bool>>>>>,
    depot_cache: Arc<RwLock<DepotCache>>,
    depot_cache_duration: Duration,
    vehicle_pool_cache: Arc<RwLock<VehiclePoolCache>>,
    vehicle_pool_cache_duration: Duration,
    waybills_by_route_cache: Arc<RwLock<WaybillsByRouteCache>>,
    waybills_by_route_cache_duration: Duration,
}

impl DBVehicleReader {
    pub fn new(pool: PgPool, config: &AppConfig) -> Self {
        Self {
            pool,
            cache: Arc::new(RwLock::new(HashMap::new())),
            cache_duration: Duration::from_secs(config.cache_duration),
            refresh_locks: Arc::new(RwLock::new(HashMap::new())),
            depot_cache: Arc::new(RwLock::new(DepotCache {
                depot_names: None,
                depot_ids: None,
                depot_name_by_id: HashMap::new(),
                vehicles_by_depot_name: HashMap::new(),
                vehicles_by_depot_id: HashMap::new(),
            })),
            depot_cache_duration: Duration::from_secs(7200), // 2 hours TTL
            vehicle_pool_cache: Arc::new(RwLock::new(VehiclePoolCache { all_vehicles: None })),
            vehicle_pool_cache_duration: Duration::from_secs(10800), // 3 hours TTL
            waybills_by_route_cache: Arc::new(RwLock::new(WaybillsByRouteCache {
                waybills_by_route: HashMap::new(),
            })),
            waybills_by_route_cache_duration: Duration::from_secs(3600), // 60 minutes TTL
        }
    }

    fn is_depot_cache_expired(&self, timestamp: SystemTime) -> bool {
        let elapsed = timestamp.elapsed().unwrap_or_default();
        elapsed >= self.depot_cache_duration
    }

    fn is_vehicle_pool_cache_expired(&self, timestamp: SystemTime) -> bool {
        let elapsed = timestamp.elapsed().unwrap_or_default();
        elapsed >= self.vehicle_pool_cache_duration
    }

    fn is_waybills_by_route_cache_expired(&self, timestamp: SystemTime) -> bool {
        let elapsed = timestamp.elapsed().unwrap_or_default();
        elapsed >= self.waybills_by_route_cache_duration
    }

    fn get_waybills_by_route_cache_key(&self, gtfs_id: &str, route_id: &str) -> String {
        format!("{}:{}", gtfs_id, route_id)
    }

    async fn get_all_vehicles_pool(&self) -> AppResult<std::collections::HashSet<String>> {
        // Check cache first
        {
            let cache = self.vehicle_pool_cache.read().await;
            if let Some((vehicles, timestamp)) = &cache.all_vehicles {
                if !self.is_vehicle_pool_cache_expired(*timestamp) {
                    info!("All vehicles pool cache HIT");
                    return Ok(vehicles.clone());
                }
            }
        }

        info!("All vehicles pool cache MISS");
        let waybill_query = r#"
            SELECT DISTINCT ON (vehicle_no)
                vehicle_no
            FROM public.waybills
            ORDER BY vehicle_no;
        "#;

        let fleet_query = r#"
            SELECT fleet_no from vehicles
        "#;

        let mut all_vehicles = std::collections::HashSet::new();

        // Get vehicles from waybills
        match sqlx::query_as::<_, (String,)>(waybill_query)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => {
                for (vehicle_no,) in rows {
                    all_vehicles.insert(vehicle_no);
                }
            }
            Err(e) => {
                error!("All vehicles pool query failed: {}", e);
            }
        }

        // Get vehicles from fleet
        match sqlx::query_as::<_, (String,)>(fleet_query)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => {
                for (fleet_no,) in rows {
                    all_vehicles.insert(fleet_no);
                }
            }
            Err(e) => {
                error!("Fleet vehicles query failed: {}", e);
            }
        }

        // Update cache
        let mut cache = self.vehicle_pool_cache.write().await;
        cache.all_vehicles = Some((all_vehicles.clone(), SystemTime::now()));
        Ok(all_vehicles)
    }

    async fn get_cached_vehicle_data(
        &self,
        vehicle_no: &str,
        trip_number: Option<i32>,
    ) -> Option<(VehicleDataWithRouteId, SystemTime)> {
        let cache_key = self.get_cache_key(vehicle_no, trip_number);
        let cache = self.cache.read().await;
        if let Some((data, timestamp)) = cache.get(&cache_key) {
            debug!("Cache HIT for vehicle {} (key: {})", vehicle_no, cache_key);
            return Some((data.clone(), *timestamp));
        }
        debug!("Cache MISS for vehicle {} (key: {})", vehicle_no, cache_key);
        None
    }

    fn is_cache_expired(&self, timestamp: SystemTime) -> bool {
        let elapsed = timestamp.elapsed().unwrap_or_default();
        elapsed >= self.cache_duration
    }

    fn get_cache_key(&self, vehicle_no: &str, trip_number: Option<i32>) -> String {
        vehicle_no.to_string()
            + trip_number
                .map(|t| t.to_string())
                .unwrap_or("".to_string())
                .as_str()
    }

    async fn cache_vehicle_data(&self, vehicle_data: &VehicleDataWithRouteId) {
        let cache_key = self.get_cache_key(&vehicle_data.vehicle_no, vehicle_data.trip_number);
        let mut cache = self.cache.write().await;
        cache.insert(cache_key, (vehicle_data.clone(), SystemTime::now()));
    }

    async fn get_or_create_refresh_lock(&self, cache_key: &str) -> Arc<tokio::sync::Mutex<bool>> {
        // First, try to get existing lock
        {
            let locks = self.refresh_locks.read().await;
            if let Some(lock) = locks.get(cache_key) {
                return lock.clone();
            }
        }

        // Lock doesn't exist, create it
        let mut locks = self.refresh_locks.write().await;
        // Double-check after acquiring write lock
        if let Some(lock) = locks.get(cache_key) {
            return lock.clone();
        }

        // Create new lock with false (not refreshing)
        let lock = Arc::new(tokio::sync::Mutex::new(false));
        locks.insert(cache_key.to_string(), lock.clone());
        lock
    }

    async fn refresh_vehicle_data_in_background(
        &self,
        vehicle_no: String,
        trip_number: Option<i32>,
    ) {
        let cache_key = self.get_cache_key(&vehicle_no, trip_number);
        let lock = self.get_or_create_refresh_lock(&cache_key).await;

        // Try to acquire lock and set flag to true - if we can't, another thread is already refreshing
        let mut lock_guard = match lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                debug!(
                    "Another thread is already refreshing vehicle {}",
                    vehicle_no
                );
                return;
            }
        };

        // Check if already refreshing
        if *lock_guard {
            debug!(
                "Another thread is already refreshing vehicle {}",
                vehicle_no
            );
            return;
        }

        // Set flag to true to indicate refresh is in progress
        *lock_guard = true;
        drop(lock_guard);

        // Spawn background task to refresh
        let pool = self.pool.clone();
        let cache = self.cache.clone();
        let cache_key_clone = cache_key.clone();
        let vehicle_no_clone = vehicle_no.clone();
        let trip_number_clone = trip_number;
        let lock_clone = lock.clone();

        tokio::spawn(async move {
            debug!(
                "Starting background refresh for vehicle {}",
                vehicle_no_clone
            );

            // Perform the actual database query (same logic as get_vehicle_data)
            // We'll call a helper that works with just the pool
            let result =
                Self::fetch_vehicle_data_with_pool(&pool, &vehicle_no_clone, trip_number_clone)
                    .await;

            match result {
                Ok(vehicle_data) => {
                    // Update cache
                    let mut cache_write = cache.write().await;
                    cache_write.insert(cache_key_clone, (vehicle_data, SystemTime::now()));
                    debug!(
                        "Background refresh completed for vehicle {}",
                        vehicle_no_clone
                    );
                }
                Err(e) => {
                    error!(
                        "Background refresh failed for vehicle {}: {:?}",
                        vehicle_no_clone, e
                    );
                }
            }

            // Reset flag to false, allowing other threads to refresh if needed
            let mut lock_guard = lock_clone.lock().await;
            *lock_guard = false;
        });
    }

    // async fn fetch_vehicle_data_impl(
    //     &self,
    //     vehicle_no: &str,
    //     trip_number: Option<i32>,
    // ) -> AppResult<VehicleDataWithRouteId> {
    //     Self::fetch_vehicle_data_with_pool(&self.pool, vehicle_no, trip_number).await
    // }
    //
    async fn fetch_vehicle_data_with_pool(
        pool: &PgPool,
        vehicle_no: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId> {
        let waybill_online_query = r#"
            SELECT
                w.waybill_id::text,
                w.waybill_no::text,
                w.service_type,
                w.vehicle_no,
                w.schedule_no,
                w.updated_at::timestamptz AS last_updated,
                w.duty_date,
                w.schedule_trip_id::text,
                e.entity_remark::text AS entity_remark,
                w.driver_token_no::text AS driver_code,
                w.conductor_token_no::text AS conductor_code,
                w.deleted AS deleted,
                w.status AS status,
            FROM waybills w
            LEFT JOIN entities e
                ON e.entity_id = w.entity_id
            WHERE w.vehicle_no = $1
              AND w.status = 'Online'
            LIMIT 1;
        "#;
        let result = match sqlx::query_as::<_, VehicleData>(waybill_online_query)
            .bind(vehicle_no)
            .fetch_optional(pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                error!("Waybill Online query failed for {}: {}", vehicle_no, e);
                None
            }
        };

        match result {
            Some(vehicle_data) => {
                info!("vehicle_data in db_vehicle_readers {:?}", vehicle_data);
                let bus_schedule_trip_detail_query: String = if let Some(trip_number) = trip_number
                {
                    format!("select NULL::int as stops_count, route_number_id::text as route_id, schedule_number, org_name::text as org_name, trip_number from bus_schedule_trip_detail where schedule_trip_id = $1::bigint and trip_number >= {} and trip_type != 'dead-trip' order by trip_number asc", trip_number)
                } else {
                    "select NULL::int as stops_count, route_number_id::text as route_id, schedule_number, org_name::text as org_name, trip_number from bus_schedule_trip_detail where schedule_trip_id = $1::bigint and trip_number >= (SELECT COALESCE((select trip_number from bus_schedule_trip_detail where schedule_trip_id = $1::bigint and is_active_trip = true and trip_type != 'dead-trip'), 1)) and trip_type != 'dead-trip' order by trip_number asc".to_string()
                };
                let bus_schedule_trip_flexi_query: String = if let Some(trip_number) = trip_number {
                    format!("select NULL::int as stops_count, route_number_id::text as route_id, schedule_number, org_name::text as org_name, trip_number, start_time as db_start_time, end_time as db_end_time from bus_schedule_trip_flexi where schedule_trip_id = $1::bigint and trip_number >= {} and trip_type != 'dead-trip' order by trip_number asc", trip_number)
                } else {
                    "WITH latest AS ( SELECT trip_number AS active_trip_number, created_at AS active_created_at FROM bus_schedule_trip_flexi WHERE schedule_trip_id = $1::bigint AND is_active_trip = TRUE AND trip_type != 'dead-trip' ORDER BY created_at DESC LIMIT 1 ) SELECT NULL::int AS stops_count, f.route_number_id::text AS route_id, f.schedule_number, f.org_name::text AS org_name, f.trip_number, f.start_time as db_start_time, f.end_time as db_end_time FROM bus_schedule_trip_flexi f LEFT JOIN latest l ON TRUE WHERE f.schedule_trip_id = $1::bigint AND f.trip_number >= COALESCE(l.active_trip_number, 1) AND f.created_at > COALESCE(l.active_created_at, now() - INTERVAL '1 day') AND f.trip_type != 'dead-trip' ORDER BY f.trip_number ASC".to_string()
                };
                let bus_schedule_query: String = "select NULL::int as stops_count, route_id::text as route_id, schedule_number, NULL::text as org_name, NULL::int as trip_number from bus_schedule where schedule_number = $1 and deleted = false".to_string();

                // Fetch trip rows (simplified version without instance methods)
                let (schedule_result, is_active_trip, remaining_trip_details) = if let Some(
                    schedule_trip_id,
                ) =
                    &vehicle_data.schedule_trip_id
                {
                    // Fetch from detailed trips first
                    let mut detail_rows = match sqlx::query_as::<_, BusSchedule>(
                        &bus_schedule_trip_detail_query,
                    )
                    .bind(schedule_trip_id)
                    .fetch_all(pool)
                    .await
                    {
                        Ok(rows) => rows,
                        Err(e) => {
                            error!("fetch_trip_rows_for_schedule: detail query failed. query={} error={}", bus_schedule_trip_detail_query, e);
                            Vec::new()
                        }
                    };

                    let detail_has_active = detail_rows
                        .iter()
                        .any(|row| row.is_active_trip.unwrap_or(false));

                    let mut flexi_rows: Vec<BusSchedule> = Vec::new();
                    if !detail_has_active {
                        flexi_rows = match sqlx::query_as::<_, BusSchedule>(
                            &bus_schedule_trip_flexi_query,
                        )
                        .bind(schedule_trip_id)
                        .fetch_all(pool)
                        .await
                        {
                            Ok(rows) => rows,
                            Err(e) => {
                                error!("fetch_trip_rows_for_schedule: flexi query failed. query={} error={}", bus_schedule_trip_flexi_query, e);
                                Vec::new()
                            }
                        };
                    }

                    detail_rows.append(&mut flexi_rows);

                    // Stable partition to bring active trip to front if present
                    if let Some(idx) = detail_rows
                        .iter()
                        .position(|row| row.is_active_trip.unwrap_or(false))
                    {
                        if idx != 0 {
                            detail_rows.swap(0, idx);
                        }
                    }

                    // Sort by trip_number if available, but keep index 0 if it is active trip
                    let has_active_front = detail_rows
                        .get(0)
                        .map(|r| r.is_active_trip.unwrap_or(false))
                        .unwrap_or(false);
                    detail_rows.sort_by(|a, b| {
                        let at = a.trip_number.unwrap_or(i32::MAX);
                        let bt = b.trip_number.unwrap_or(i32::MAX);
                        at.cmp(&bt)
                    });
                    if has_active_front {
                        if let Some(pos) = detail_rows
                            .iter()
                            .position(|row| row.is_active_trip.unwrap_or(false))
                        {
                            if pos != 0 {
                                detail_rows.swap(0, pos);
                            }
                        }
                    }

                    if !detail_rows.is_empty() {
                        // Enrich route numbers
                        Self::enrich_route_numbers_with_pool(pool, &mut detail_rows).await?;
                        let first = detail_rows.remove(0);
                        let remaining = if detail_rows.is_empty() {
                            None
                        } else {
                            Some(detail_rows)
                        };
                        (Some(first), true, remaining)
                    } else {
                        // Fallback to bus_schedule
                        let mut rows = match sqlx::query_as::<_, BusSchedule>(&bus_schedule_query)
                            .bind(vehicle_data.schedule_no.clone())
                            .fetch_all(pool)
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                error!(
                                    "Query failed (bus_schedule): {} | {}",
                                    bus_schedule_query, e
                                );
                                Vec::new()
                            }
                        };
                        if !rows.is_empty() {
                            Self::enrich_route_numbers_with_pool(pool, &mut rows).await?;
                            let first = rows.remove(0);
                            let remaining = if rows.is_empty() { None } else { Some(rows) };
                            (Some(first), false, remaining)
                        } else {
                            (None, false, None)
                        }
                    }
                } else {
                    // If no schedule_trip_id, directly try bus_schedule_query
                    let mut rows = match sqlx::query_as::<_, BusSchedule>(&bus_schedule_query)
                        .bind(vehicle_data.schedule_no.clone())
                        .fetch_all(pool)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            error!("Query failed (bus_schedule direct): {}", e);
                            Vec::new()
                        }
                    };
                    if !rows.is_empty() {
                        Self::enrich_route_numbers_with_pool(pool, &mut rows).await?;
                        let first = rows.remove(0);
                        let remaining = if rows.is_empty() { None } else { Some(rows) };
                        (Some(first), false, remaining)
                    } else {
                        (None, false, None)
                    }
                };

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
                    schedule_details: None,
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
                let waybill_status_agnostic_query = "
                    SELECT w.vehicle_no, w.service_type
                    FROM waybills w
                    WHERE w.vehicle_no = $1
                    ORDER BY w.updated_at DESC
                    LIMIT 1
                ";

                let minimal_vehicle_data =
                    match sqlx::query_as::<_, MinimalVehicleData>(waybill_status_agnostic_query)
                        .bind(vehicle_no)
                        .fetch_optional(pool)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            error!(
                                "Waybill Status Agnostic query failed for {}: {}",
                                vehicle_no, e
                            );
                            None
                        }
                    };

                let vehicle_data_with_route_id =
                    if let Some(minimal_vehicle_data) = minimal_vehicle_data {
                        VehicleDataWithRouteId {
                            waybill_id: None,
                            waybill_no: None,
                            service_type: Some(minimal_vehicle_data.service_type),
                            vehicle_no: minimal_vehicle_data.vehicle_no.to_string(),
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

                Ok(vehicle_data_with_route_id)
            }
        }
    }

    async fn enrich_route_numbers_with_pool(
        pool: &PgPool,
        schedules: &mut [BusSchedule],
    ) -> AppResult<()> {
        let route_ids: Vec<String> = schedules
            .iter()
            .map(|s| s.route_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        if route_ids.is_empty() {
            return Ok(());
        }

        let placeholders: Vec<String> = (1..=route_ids.len()).map(|i| format!("${}", i)).collect();
        let placeholders_str = placeholders.join(",");

        let query = format!(
            "SELECT route_id::text as route_id, route_number FROM bus_route WHERE route_id::text IN ({})",
            placeholders_str
        );

        let mut query_builder = sqlx::query_as::<_, (String, Option<String>)>(&query);
        for id in &route_ids {
            query_builder = query_builder.bind(id);
        }

        let mappings = match query_builder.fetch_all(pool).await {
            Ok(m) => m,
            Err(e) => {
                error!("Failed to fetch route numbers: {}", e);
                return Ok(());
            }
        };

        let map: std::collections::HashMap<String, Option<String>> = mappings.into_iter().collect();

        for s in schedules.iter_mut() {
            if let Some(num) = map.get(&s.route_id) {
                s.route_number = num.clone();
            }
        }

        Ok(())
    }
}

impl DBVehicleReader {
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

        let placeholders: Vec<String> = (1..=route_ids.len()).map(|i| format!("${}", i)).collect();
        let placeholders_str = placeholders.join(",");

        let query = format!(
            "SELECT route_id::text as route_id, route_number FROM bus_route WHERE route_id::text IN ({})",
            placeholders_str
        );

        let mut query_builder = sqlx::query_as::<_, (String, Option<String>)>(&query);
        for id in &route_ids {
            query_builder = query_builder.bind(id);
        }

        let mappings = match query_builder.fetch_all(&self.pool).await {
            Ok(m) => m,
            Err(e) => {
                error!("Failed to fetch route numbers: {}", e);
                return Ok(());
            }
        };

        let map: std::collections::HashMap<String, Option<String>> = mappings.into_iter().collect();

        for s in schedules.iter_mut() {
            if let Some(num) = map.get(&s.route_id) {
                s.route_number = num.clone();
            }
        }

        Ok(())
    }

    // fn log_trip_rows(&self, source: &str, rows: &[BusSchedule]) {
    //     info!(
    //         source = source,
    //         count = rows.len(),
    //         "Trip rows fetched from table"
    //     );
    //     // Print a small table of up to 10 rows for readability
    //     info!(
    //         source = source,
    //         "{:<16} | {:<16} | {:<6} | {:<5} | {}",
    //         "schedule_no",
    //         "route_id",
    //         "trip#",
    //         "active",
    //         "org_name"
    //     );
    //     for r in rows.iter().take(10) {
    //         info!(
    //             source = source,
    //             "{:<16} | {:<16} | {:<6} | {:<5} | {}",
    //             r.schedule_number,
    //             r.route_id,
    //             r.trip_number.unwrap_or_default(),
    //             if r.is_active_trip.unwrap_or(false) {
    //                 "true"
    //             } else {
    //                 "false"
    //             },
    //             r.org_name.as_deref().unwrap_or("")
    //         );
    //     }
    //     if rows.len() > 10 {
    //         info!(
    //             source = source,
    //             remaining = rows.len() - 10,
    //             "... more rows omitted"
    //         );
    //     }
    // }

    /// Query for Online waybills
    async fn get_online_waybill(&self, vehicle_no: &str) -> AppResult<Option<VehicleData>> {
        let online_query = r#"
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
            FROM waybills w
            LEFT JOIN entities e ON e.entity_id = w.entity_id
            WHERE w.vehicle_no = $1 AND w.status = 'Online' AND w.deleted = false
            ORDER BY w.updated_at DESC
            LIMIT 1
        "#;

        match sqlx::query_as::<_, VehicleData>(online_query)
            .bind(vehicle_no)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(result) => Ok(result),
            Err(e) => {
                error!("Online waybill query failed for {}: {}", vehicle_no, e);
                Ok(None)
            }
        }
    }

    /// Query for Processed/New waybills
    async fn get_processed_new_waybill(&self, vehicle_no: &str) -> AppResult<Option<VehicleData>> {
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
            FROM waybills w
            LEFT JOIN entities e ON e.entity_id = w.entity_id
            WHERE w.vehicle_no = $1 AND w.status IN ('Processed', 'New', 'Closed') AND w.deleted = false
            ORDER BY CASE
                WHEN w.status = 'Processed' THEN 1
                WHEN w.status = 'New' THEN 2
                WHEN w.status = 'Closed' THEN 3
            END, w.updated_at DESC
            LIMIT 1
        "#;

        match sqlx::query_as::<_, VehicleData>(fallback_query)
            .bind(vehicle_no)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(result) => Ok(result),
            Err(e) => {
                error!(
                    "Processed/New waybill query failed for {}: {}",
                    vehicle_no, e
                );
                Ok(None)
            }
        }
    }

    /// Split waybill query strategy with priority: Online first, then Processed/New
    async fn get_waybill_with_priority(
        &self,
        vehicle_no: &str,
    ) -> AppResult<(Option<VehicleData>, WaybillStatus)> {
        // Priority 1: Online waybills
        if let Some(online_waybill) = self.get_online_waybill(vehicle_no).await? {
            return Ok((Some(online_waybill), WaybillStatus::Online));
        }

        // Priority 2: Processed/New waybills
        if let Some(processed_waybill) = self.get_processed_new_waybill(vehicle_no).await? {
            return Ok((Some(processed_waybill), WaybillStatus::ProcessedOrNew));
        }

        // No waybill found
        Ok((None, WaybillStatus::NotFound))
    }

    /// Handle flexi trips using waybill_id binding
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
                    trip_end_time AS end_time,
                    deleted,
                    is_active_trip,
                    trip_order,
                    start_time AS db_start_time,
                    end_time AS db_end_time
                FROM bus_schedule_trip_flexi
                WHERE waybill_id = $1::bigint AND trip_number >= {} AND trip_type != 'dead-trip'
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
                    trip_end_time AS end_time,
                    deleted,
                    is_active_trip,
                    trip_order,
                    start_time AS db_start_time,
                    end_time AS db_end_time
                FROM bus_schedule_trip_flexi
                WHERE waybill_id = $1::bigint AND trip_type != 'dead-trip'
                ORDER BY trip_number ASC
            "#
            .to_string()
        };

        let flexi_rows = match sqlx::query_as::<_, BusSchedule>(&flexi_query)
            .bind(waybill_data.waybill_id.parse::<i64>().unwrap_or(0))
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                error!(
                    "Flexi query failed for waybill_id {}: {}",
                    waybill_data.waybill_id, e
                );
                Vec::new()
            }
        };

        if !flexi_rows.is_empty() {
            return self.process_trip_rows(flexi_rows, false).await;
        }

        // Fallback to detail trips if no flexi rows
        self.handle_detail_trips(waybill_data, false, trip_number)
            .await
    }

    /// Handle detail trips using schedule_trip_id binding
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
                FROM bus_schedule_trip_detail
                WHERE schedule_trip_id = $1::bigint AND trip_number >= {} AND trip_type != 'dead-trip'
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
                FROM bus_schedule_trip_detail
                WHERE schedule_trip_id = $1::bigint
                    AND trip_number >= (SELECT COALESCE(
                        (SELECT trip_number FROM bus_schedule_trip_detail
                         WHERE schedule_trip_id = $1::bigint AND is_active_trip = true AND trip_type != 'dead-trip'), 1))
                    AND trip_type != 'dead-trip'
                ORDER BY trip_number ASC
            "#.to_string()
        };

        let detail_rows = match sqlx::query_as::<_, BusSchedule>(&detail_query)
            .bind(&waybill_data.schedule_trip_id)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                error!(
                    "Detail query failed for schedule_trip_id {:?}: {}",
                    waybill_data.schedule_trip_id, e
                );
                Vec::new()
            }
        };

        if !detail_rows.is_empty() {
            return self
                .process_trip_rows(detail_rows, force_first_active)
                .await;
        }

        // Final fallback to bus_schedule table
        self.handle_schedule_fallback(&waybill_data.schedule_no)
            .await
    }

    /// Process trip rows with consistent active trip + remaining trips logic
    async fn process_trip_rows(
        &self,
        mut rows: Vec<BusSchedule>,
        force_first_active: bool,
    ) -> AppResult<(Option<BusSchedule>, bool, Option<Vec<BusSchedule>>)> {
        if rows.is_empty() {
            return Ok((None, false, None));
        }

        // Enrich with route numbers
        self.enrich_route_numbers(&mut rows).await?;

        if force_first_active {
            // For Processed/New: First trip becomes active, remaining are only FUTURE trips
            let first = rows.remove(0);
            let remaining = if rows.is_empty() { None } else { Some(rows) };
            Ok((Some(first), true, remaining))
        } else {
            // For Online: Respect is_active_trip flags
            if let Some(active_idx) = rows.iter().position(|r| r.is_active_trip.unwrap_or(false)) {
                // Remove active trip from list
                let active_trip = rows.remove(active_idx);

                // Remaining trips = preserve all fetched rows except the active trip
                let remaining_trips: Vec<BusSchedule> = rows.into_iter().collect();

                let remaining = if remaining_trips.is_empty() {
                    None
                } else {
                    Some(remaining_trips)
                };
                Ok((Some(active_trip), true, remaining))
            } else {
                // No active trip, use first and remaining are FUTURE trips
                let first = rows.remove(0);
                let remaining = if rows.is_empty() { None } else { Some(rows) };
                Ok((Some(first), false, remaining))
            }
        }
    }

    /// Handle bus_schedule fallback
    async fn handle_schedule_fallback(
        &self,
        schedule_no: &str,
    ) -> AppResult<(Option<BusSchedule>, bool, Option<Vec<BusSchedule>>)> {
        let bus_schedule_query = r#"
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
                FALSE AS is_active_trip,
                NULL::int AS trip_order,
                NULL::text AS db_start_time,
                NULL::text AS db_end_time
            FROM bus_schedule
            WHERE schedule_number = $1 AND deleted = false
        "#;

        let mut rows = match sqlx::query_as::<_, BusSchedule>(bus_schedule_query)
            .bind(schedule_no)
            .fetch_all(&self.pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                error!(
                    "Bus schedule fallback query failed for {}: {}",
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

    /// Resolve trip data based on waybill status and is_flexi flag
    async fn resolve_trip_data(
        &self,
        waybill_data: VehicleData,
        status: WaybillStatus,
        trip_number: Option<i32>,
    ) -> AppResult<(Option<BusSchedule>, bool, Option<Vec<BusSchedule>>)> {
        match status {
            WaybillStatus::Online => {
                if waybill_data.is_flexi == Some(true) {
                    // Flexi branch: Use waybill_id binding
                    self.handle_flexi_trips(&waybill_data, trip_number).await
                } else {
                    // Detail branch: Use schedule_trip_id binding
                    self.handle_detail_trips(&waybill_data, false, trip_number)
                        .await
                }
            }
            WaybillStatus::ProcessedOrNew => {
                // Always use detail trips, force first as active
                self.handle_detail_trips(&waybill_data, true, trip_number)
                    .await
            }
            WaybillStatus::NotFound => {
                // This shouldn't happen in this context
                Ok((None, false, None))
            }
        }
    }

    // async fn fetch_trip_rows_for_schedule(
    //     &self,
    //     schedule_trip_id: &str,
    //     detail_query: &str,
    //     flexi_query: &str,
    // ) -> Vec<BusSchedule> {
    //     // Fetch from detailed trips first
    //     let mut detail_rows = match sqlx::query_as::<_, BusSchedule>(detail_query)
    //         .bind(schedule_trip_id)
    //         .fetch_all(&self.pool)
    //         .await
    //     {
    //         Ok(rows) => rows,
    //         Err(e) => {
    //             error!(
    //                 "fetch_trip_rows_for_schedule: detail query failed. query={} error={}",
    //                 detail_query, e
    //             );
    //             Vec::new()
    //         }
    //     };
    //     self.log_trip_rows("bus_schedule_trip_detail", &detail_rows);
    //
    //     // Only fetch from flexi trips if no active trip is present in detail rows
    //     let detail_has_active = detail_rows
    //         .iter()
    //         .any(|row| row.is_active_trip.unwrap_or(false));
    //
    //     let mut flexi_rows: Vec<BusSchedule> = Vec::new();
    //     if !detail_has_active {
    //         flexi_rows = match sqlx::query_as::<_, BusSchedule>(flexi_query)
    //             .bind(schedule_trip_id)
    //             .fetch_all(&self.pool)
    //             .await
    //         {
    //             Ok(rows) => rows,
    //             Err(e) => {
    //                 error!(
    //                     "fetch_trip_rows_for_schedule: flexi query failed. query={} error={}",
    //                     flexi_query, e
    //                 );
    //                 Vec::new()
    //             }
    //         };
    //         self.log_trip_rows("bus_schedule_trip_flexi", &flexi_rows);
    //     } else {
    //         info!(
    //             schedule_trip_id = schedule_trip_id,
    //             "Skipping flexi fetch: active trip found in detail table"
    //         );
    //     }
    //
    //     // Combine (flexi_rows may be empty if skipped)
    //     detail_rows.append(&mut flexi_rows);
    //
    //     // Stable partition to bring active trip to front if present
    //     if let Some(idx) = detail_rows
    //         .iter()
    //         .position(|row| row.is_active_trip.unwrap_or(false))
    //     {
    //         if idx != 0 {
    //             detail_rows.swap(0, idx);
    //         }
    //     }
    //
    //     // Sort by trip_number if available, but keep index 0 if it is active trip
    //     let has_active_front = detail_rows
    //         .get(0)
    //         .map(|r| r.is_active_trip.unwrap_or(false))
    //         .unwrap_or(false);
    //     detail_rows.sort_by(|a, b| {
    //         let at = a.trip_number.unwrap_or(i32::MAX);
    //         let bt = b.trip_number.unwrap_or(i32::MAX);
    //         at.cmp(&bt)
    //     });
    //     if has_active_front {
    //         if let Some(pos) = detail_rows
    //             .iter()
    //             .position(|row| row.is_active_trip.unwrap_or(false))
    //         {
    //             if pos != 0 {
    //                 detail_rows.swap(0, pos);
    //             }
    //         }
    //     }
    //
    //     detail_rows
    // }
}

/// HACK: If the first trip's `startTime` (trip_start_time) is stale check DB_ACTIVE_TRIP_STALE_HOURS and if so,
//  re-derive the active trip using IST clock time, even if the DB already
/// set `is_active_trip = true` on a row.  Whichever trip's `dbStartTime`..`dbEndTime`
/// window contains the current IST time becomes the active trip.
/// If the data is fresh or no window matches, the response is left unchanged.
fn apply_ist_active_trip_hack(vehicle_data: &mut VehicleDataWithRouteId) {
    // Collect all trips from schedule_details into one sorted vec
    let all_trips: Vec<BusSchedule> = {
        let Some(ref schedule_details) = vehicle_data.schedule_details else {
            return;
        };
        let mut trips: Vec<BusSchedule> = schedule_details
            .values()
            .flatten()
            .cloned()
            .collect();
        if trips.is_empty() {
            return;
        }
        trips.sort_by_key(|t| t.trip_number.unwrap_or(i32::MAX));
        trips
    };

    // Check staleness: only apply the hack if the first trip's start_time is old.
    // start_time is trip_start_time from the DB, which is always epoch milliseconds or NULL.
    let start_time_utc = match &all_trips[0].start_time {
        Some(s) if !s.is_empty() => match s.parse::<i64>() {
            Ok(epoch_ms) => Utc.timestamp_millis_opt(epoch_ms).single(),
            Err(_) => None,
        },
        _ => None,
    };
    let Some(start_time_utc) = start_time_utc else {
        return; // no valid timestamp – bail out
    };
    let now_utc = Utc::now();
    if now_utc.signed_duration_since(start_time_utc).num_hours() < DB_ACTIVE_TRIP_STALE_HOURS {
        return; // Data is fresh; no hack needed
    }
    let ist_offset = FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
    let now_ist = now_utc.with_timezone(&ist_offset);

    // Use `last_updated` as the base date for anchoring db_start_time values.
    let base_date = vehicle_data
        .last_updated
        .map(|dt| dt.with_timezone(&ist_offset).date_naive())
        .unwrap_or_else(|| now_ist.date_naive());

    // Reconstruct per-trip start datetimes by walking trips in order and bumping the
    // date when the clock time goes backwards (midnight cross).
    let mut current_date = base_date;
    let mut prev_seconds: Option<u32> = None;
    let mut trip_start_datetimes: Vec<(Option<i32>, chrono::DateTime<FixedOffset>)> = Vec::new();
    for trip in &all_trips {
        let db_start = trip.db_start_time.as_deref().unwrap_or("");
        if db_start.is_empty() {
            continue;
        }
        let Ok(time) = NaiveTime::parse_from_str(db_start, "%H:%M") else {
            continue;
        };

        if let Some(prev) = prev_seconds {
            if time.num_seconds_from_midnight() < prev {
                current_date = current_date.succ_opt().unwrap_or(current_date);
            }
        }
        prev_seconds = Some(time.num_seconds_from_midnight());

        let naive_dt = current_date.and_time(time);
        if let Some(final_dt) = ist_offset.from_local_datetime(&naive_dt).single() {
            trip_start_datetimes.push((trip.trip_number, final_dt));
        }
    }

    // Pick the latest trip whose reconstructed start datetime is <= now_ist
    let matched_trip_num = trip_start_datetimes
        .into_iter()
        .filter(|(_, start_dt)| *start_dt <= now_ist)
        .max_by_key(|(_, start_dt)| *start_dt)
        .map(|(trip_num, _)| trip_num);

    let Some(new_trip_num) = matched_trip_num else {
        return; // No window matched – return as-is
    };

    // Nothing to do if the current active trip already matches
    if new_trip_num == vehicle_data.trip_number && vehicle_data.is_active_trip {
        return;
    }

    let Some(new_active) = all_trips.iter().find(|t| t.trip_number == new_trip_num) else {
        return;
    };

    info!(
        "IST hack: shifting active trip from {:?} -> {:?} (db_start={:?} db_end={:?}) for vehicle {}",
        vehicle_data.trip_number,
        new_active.trip_number,
        new_active.db_start_time,
        new_active.db_end_time,
        vehicle_data.vehicle_no
    );

    // Update the top-level active-trip fields
    vehicle_data.trip_number = new_active.trip_number;
    vehicle_data.route_id = Some(new_active.route_id.clone());
    vehicle_data.route_number = new_active.route_number.clone();
    vehicle_data.depot = new_active.org_name.clone();
    vehicle_data.db_start_time = new_active.db_start_time.clone();
    vehicle_data.db_end_time = new_active.db_end_time.clone();
    vehicle_data.is_active_trip = true;

    // Rebuild remaining_trip_details: include all trips except the newly activated one
    let remaining: Vec<BusSchedule> = all_trips
        .into_iter()
        .filter(|t| t.trip_number != new_trip_num)
        .collect();
    vehicle_data.remaining_trip_details = if remaining.is_empty() {
        None
    } else {
        Some(remaining)
    };
}

#[async_trait]
impl VehicleDataReader for DBVehicleReader {
    async fn get_vehicle_data(
        &self,
        vehicle_no: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId> {
        // Check cache (including stale data)
        if let Some((cached_data, timestamp)) =
            self.get_cached_vehicle_data(vehicle_no, trip_number).await
        {
            // Check if cache is expired and trigger background refresh if needed
            if self.is_cache_expired(timestamp) {
                debug!(
                    "Cache expired for vehicle {}, triggering background refresh",
                    vehicle_no
                );
                // Trigger background refresh (non-blocking)
                self.refresh_vehicle_data_in_background(vehicle_no.to_string(), trip_number)
                    .await;
            }
            // Return cached data (even if stale) - background refresh will update it
            return Ok(cached_data);
        }

        // Enhanced flow: Split waybill query with priority (Online -> Processed/New)
        let (waybill_result, waybill_status) = self.get_waybill_with_priority(vehicle_no).await?;

        let mut schedule_map: HashMap<i64, Vec<BusSchedule>> = HashMap::new();
        match waybill_result {
            Some(vehicle_data) => {
                info!(
                    "Enhanced flow - vehicle_data: {:?}, status: {:?}",
                    vehicle_data, waybill_status
                );

                // Resolve trip data based on waybill status and is_flexi flag
                let (schedule_result, is_active_trip, remaining_trip_details) = self
                    .resolve_trip_data(vehicle_data.clone(), waybill_status, trip_number)
                    .await?;

                // Populate schedule_map for backward compatibility
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
                            .insert(0, schedule.clone()); // Insert active trip at front
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

                // Set route and trip details from active schedule
                if let Some(schedule) = schedule_result {
                    vehicle_data_with_route_id.trip_number = schedule.trip_number;
                    vehicle_data_with_route_id.route_id = Some(schedule.route_id.to_owned());
                    vehicle_data_with_route_id.depot = schedule.org_name.clone();
                    vehicle_data_with_route_id.route_number = schedule.route_number.clone();
                    vehicle_data_with_route_id.db_start_time = schedule.db_start_time.clone();
                    vehicle_data_with_route_id.db_end_time = schedule.db_end_time.clone();
                }

                // HACK: if trip timestamps are stale, re-derive the active trip
                // from the current IST clock time vs each trip's dbStartTime/dbEndTime window.
                apply_ist_active_trip_hack(&mut vehicle_data_with_route_id);

                self.cache_vehicle_data(&vehicle_data_with_route_id).await;
                Ok(vehicle_data_with_route_id)
            }
            None => {
                // No waybill found - fallback to minimal data
                let waybill_status_agnostic_query = "
                    SELECT w.vehicle_no, w.service_type
                    FROM waybills w
                    WHERE w.vehicle_no = $1
                    ORDER BY w.updated_at DESC
                    LIMIT 1
                ";

                let minimal_vehicle_data =
                    match sqlx::query_as::<_, MinimalVehicleData>(waybill_status_agnostic_query)
                        .bind(vehicle_no)
                        .fetch_optional(&self.pool)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            error!(
                                "Waybill Status Agnostic query failed for {}: {}",
                                vehicle_no, e
                            );
                            None
                        }
                    };

                let vehicle_data_with_route_id =
                    if let Some(minimal_vehicle_data) = minimal_vehicle_data {
                        VehicleDataWithRouteId {
                            waybill_id: None,
                            waybill_no: None,
                            service_type: Some(minimal_vehicle_data.service_type),
                            vehicle_no: minimal_vehicle_data.vehicle_no.to_string(),
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
                            schedule_details: None,
                            status: None,
                            db_start_time: None,
                            db_end_time: None,
                            seat_layout_id: None,
                        }
                    };

                Ok(vehicle_data_with_route_id)
            }
        }
    }

    async fn get_all_vehicles(&self) -> AppResult<Vec<VehicleData>> {
        let query = r#"
            SELECT DISTINCT ON (vehicle_no)
                waybill_id::text,
                service_type,
                vehicle_no,
                schedule_no,
                updated_at::timestamptz AS last_updated,
                duty_date,
                driver_token_no::text AS driver_code,
                conductor_token_no::text AS conductor_code,
                deleted,
                status,
            FROM waybills
            ORDER BY vehicle_no, updated_at DESC;
        "#;

        match sqlx::query_as::<_, VehicleData>(query)
            .fetch_all(&self.pool)
            .await
        {
            Ok(v) => Ok(v),
            Err(e) => {
                error!("get_all_vehicles query failed: {}", e);
                Ok(Vec::new())
            }
        }
    }

    async fn get_vehicles_by_service_type(
        &self,
        service_type: &str,
    ) -> AppResult<Vec<VehicleData>> {
        let query = r#"
            SELECT DISTINCT ON (vehicle_no)
                waybill_id::text,
                service_type,
                vehicle_no,
                schedule_no,
                updated_at::timestamptz AS last_updated,
                duty_date,
                driver_token_no::text AS driver_code,
                conductor_token_no::text AS conductor_code,
                deleted,
                status
            FROM waybills
            WHERE service_type = $1
            ORDER BY vehicle_no, updated_at DESC;
        "#;

        match sqlx::query_as::<_, VehicleData>(query)
            .bind(service_type)
            .fetch_all(&self.pool)
            .await
        {
            Ok(v) => Ok(v),
            Err(e) => {
                error!("get_vehicles_by_service_type query failed: {}", e);
                Ok(Vec::new())
            }
        }
    }

    async fn search_vehicles(&self, query: &str) -> AppResult<Vec<VehicleData>> {
        let search_pattern = format!("%{}%", query);
        let query_sql = r#"
            SELECT DISTINCT ON (vehicle_no)
                waybill_id::text,
                service_type,
                vehicle_no,
                schedule_no,
                updated_at::timestamptz AS last_updated,
                duty_date,
                driver_token_no::text AS driver_code,
                conductor_token_no::text AS conductor_code,
                deleted,
                status
            FROM waybills
            WHERE
                vehicle_no ILIKE $1
                OR waybill_id::text ILIKE $1
                OR schedule_no ILIKE $1
            ORDER BY vehicle_no, updated_at DESC;
        "#;

        match sqlx::query_as::<_, VehicleData>(query_sql)
            .bind(&search_pattern)
            .fetch_all(&self.pool)
            .await
        {
            Ok(v) => Ok(v),
            Err(e) => {
                error!("search_vehicles query failed: {}", e);
                Ok(Vec::new())
            }
        }
    }

    async fn get_vehicles_by_ids(
        &self,
        vehicle_nos: Vec<String>,
    ) -> AppResult<Vec<VehicleDataWithRouteId>> {
        if vehicle_nos.is_empty() {
            return Ok(Vec::new());
        }

        // Check cache first for any cached vehicles
        let mut found_vehicles = Vec::new();
        let mut uncached_vehicle_nos = Vec::new();

        for vehicle_no in &vehicle_nos {
            // For get_vehicles_by_ids, we don't have trip_number, so use None
            if let Some((cached_data, _timestamp)) =
                self.get_cached_vehicle_data(vehicle_no, None).await
            {
                found_vehicles.push(cached_data);
            } else {
                uncached_vehicle_nos.push(vehicle_no.clone());
            }
        }

        // If all vehicles were cached, return early
        if uncached_vehicle_nos.is_empty() {
            return Ok(found_vehicles);
        }

        // Build the IN clause for the query
        let placeholders: Vec<String> = (1..=uncached_vehicle_nos.len())
            .map(|i| format!("${}", i))
            .collect();
        let placeholders_str = placeholders.join(",");

        let query = format!(
            r#"
            SELECT
                waybill_id::text,
                service_type,
                vehicle_no,
                schedule_no,
                updated_at::timestamptz AS last_updated,
                duty_date,
                driver_token_no::text AS driver_code,
                conductor_token_no::text AS conductor_code,
                deleted,
                status
            FROM waybills
            WHERE vehicle_no IN ({})
            ORDER BY updated_at DESC
            "#,
            placeholders_str
        );
        // Execute the batch query
        let mut query_builder = sqlx::query_as::<_, VehicleData>(&query);
        for vehicle_no in &uncached_vehicle_nos {
            query_builder = query_builder.bind(vehicle_no);
        }

        let vehicle_results = match query_builder.fetch_all(&self.pool).await {
            Ok(v) => v,
            Err(e) => {
                error!("get_vehicles_by_ids batch query failed: {}", e);
                Vec::new()
            }
        };

        // Get all unique schedule numbers
        let schedule_numbers: Vec<String> = vehicle_results
            .iter()
            .map(|v| v.schedule_no.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // Fetch schedules in batch
        let mut schedule_map = std::collections::HashMap::new();
        if !schedule_numbers.is_empty() {
            let schedule_placeholders: Vec<String> = (1..=schedule_numbers.len())
                .map(|i| format!("${}", i))
                .collect();
            let schedule_placeholders_str = schedule_placeholders.join(",");

            let schedule_query = format!(
                "SELECT schedule_number, route_id::text as route_id, NULL::text as org_name
                 FROM bus_schedule
                 WHERE schedule_number IN ({}) AND deleted = false",
                schedule_placeholders_str
            );

            let mut schedule_query_builder = sqlx::query_as::<_, BusSchedule>(&schedule_query);
            for schedule_no in &schedule_numbers {
                schedule_query_builder = schedule_query_builder.bind(schedule_no);
            }

            let mut schedule_results = match schedule_query_builder.fetch_all(&self.pool).await {
                Ok(v) => v,
                Err(e) => {
                    error!("schedule batch query failed: {}", e);
                    Vec::new()
                }
            };

            // Enrich route_number for these schedules
            self.enrich_route_numbers(&mut schedule_results).await?;

            for schedule in schedule_results {
                schedule_map.insert(schedule.schedule_number.clone(), schedule);
            }
        }

        // Build the final result
        for vehicle_data in vehicle_results {
            let mut vehicle_data_with_route_id = VehicleDataWithRouteId {
                waybill_id: Some(vehicle_data.waybill_id),
                waybill_no: Some(vehicle_data.waybill_no),
                service_type: Some(vehicle_data.service_type),
                vehicle_no: vehicle_data.vehicle_no,
                schedule_no: Some(vehicle_data.schedule_no),
                last_updated: vehicle_data.last_updated,
                duty_date: vehicle_data.duty_date,
                route_id: None,
                route_number: None,
                depot: None,
                trip_number: None,
                is_active_trip: false,
                remaining_trip_details: None,
                entity_remark: None,
                driver_code: vehicle_data.driver_code,
                conductor_code: vehicle_data.conductor_code,
                deleted: vehicle_data.deleted,
                status: vehicle_data.status,
                schedule_details: None,
                db_start_time: None,
                db_end_time: None,
                seat_layout_id: None,
            };

            if let Some(schedule_no) = &vehicle_data_with_route_id.schedule_no {
                if let Some(schedule) = schedule_map.get(schedule_no) {
                    vehicle_data_with_route_id.route_id = Some(schedule.route_id.clone());
                    vehicle_data_with_route_id.depot = schedule.org_name.clone();
                    vehicle_data_with_route_id.route_number = schedule.route_number.clone();
                }
            }

            self.cache_vehicle_data(&vehicle_data_with_route_id).await;
            found_vehicles.push(vehicle_data_with_route_id);
        }

        Ok(found_vehicles)
    }

    async fn get_vehicle_count(&self) -> AppResult<i64> {
        match sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(DISTINCT vehicle_no) as count FROM waybills",
        )
        .fetch_one(&self.pool)
        .await
        {
            Ok(row) => Ok(row.0),
            Err(e) => {
                error!("get_vehicle_count query failed: {}", e);
                Ok(0)
            }
        }
    }

    async fn get_vehicles_by_depot_name(
        &self,
        depot_name: &str,
    ) -> AppResult<Vec<DepotVehicleSummary>> {
        // Check cache first
        {
            let cache = self.depot_cache.read().await;
            if let Some((vehicles, timestamp)) = cache.vehicles_by_depot_name.get(depot_name) {
                if !self.is_depot_cache_expired(*timestamp) {
                    debug!("Depot cache HIT for depot_name: {}", depot_name);
                    return Ok(vehicles.clone());
                }
            }
        }

        debug!("Depot cache MISS for depot_name: {}", depot_name);
        let query: &str = r#"SELECT vehicles.fleet_no AS fleet_no, vehicles.status AS status, vehicles.vehicle_no AS vehicle_no FROM vehicles LEFT JOIN entities AS Entities ON vehicles.entity_id = Entities.entity_id WHERE Entities.entity_name = $1 AND fleet_no <> '' LIMIT 1048575"#;

        match sqlx::query_as::<_, DepotVehicleSummary>(query)
            .bind(depot_name)
            .fetch_all(&self.pool)
            .await
        {
            Ok(v) => {
                info!("get_vehicles_by_depot_name rows={}", v.len());
                // Update cache
                let mut cache = self.depot_cache.write().await;
                cache
                    .vehicles_by_depot_name
                    .insert(depot_name.to_string(), (v.clone(), SystemTime::now()));
                Ok(v)
            }
            Err(e) => {
                error!("get_vehicles_by_depot_name query failed: {}", e);
                Ok(Vec::new())
            }
        }
    }

    async fn get_vehicles_by_depot_id(
        &self,
        depot_id: &str,
    ) -> AppResult<Vec<DepotVehicleSummary>> {
        // Check cache first
        {
            let cache = self.depot_cache.read().await;
            if let Some((vehicles, timestamp)) = cache.vehicles_by_depot_id.get(depot_id) {
                if !self.is_depot_cache_expired(*timestamp) {
                    debug!("Depot cache HIT for depot_id: {}", depot_id);
                    return Ok(vehicles.clone());
                }
            }
        }

        debug!("Depot cache MISS for depot_id: {}", depot_id);
        let depot_id_int = depot_id
            .parse::<i64>()
            .map_err(|_| AppError::BadRequest(format!("Invalid depot_id: {}", depot_id)))?;

        // Keep columns and aliasing the same as get_vehicles_by_depot_name
        let query: &str = r#"SELECT vehicles.fleet_no AS fleet_no, vehicles.status AS status, vehicles.vehicle_no AS vehicle_no FROM vehicles WHERE vehicles.entity_id = $1 AND fleet_no <> '' LIMIT 1048575"#;
        info!("get_vehicles_by_depot_id query: {}", query);
        match sqlx::query_as::<_, DepotVehicleSummary>(query)
            .bind(depot_id_int)
            .fetch_all(&self.pool)
            .await
        {
            Ok(v) => {
                info!("get_vehicles_by_depot_id rows={}", v.len());
                // Update cache
                let mut cache = self.depot_cache.write().await;
                cache
                    .vehicles_by_depot_id
                    .insert(depot_id.to_string(), (v.clone(), SystemTime::now()));
                Ok(v)
            }
            Err(e) => {
                error!("get_vehicles_by_depot_id query failed: {}", e);
                Ok(Vec::new())
            }
        }
    }

    async fn get_depot_names(&self) -> AppResult<Vec<String>> {
        // Check cache first
        {
            let cache = self.depot_cache.read().await;
            if let Some((names, timestamp)) = &cache.depot_names {
                if !self.is_depot_cache_expired(*timestamp) {
                    debug!("Depot cache HIT for depot_names");
                    return Ok(names.clone());
                }
            }
        }

        debug!("Depot cache MISS for depot_names");
        let query = "SELECT DISTINCT entity_name FROM entities LIMIT 1048575";
        match sqlx::query_as::<_, (Option<String>,)>(query)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => {
                let names: Vec<String> = rows.into_iter().filter_map(|r| r.0).collect();
                // Update cache
                let mut cache = self.depot_cache.write().await;
                cache.depot_names = Some((names.clone(), SystemTime::now()));
                Ok(names)
            }
            Err(e) => {
                error!("get_depot_names query failed: {}", e);
                Ok(Vec::new())
            }
        }
    }

    async fn get_depot_ids(&self) -> AppResult<Vec<String>> {
        // Check cache first
        {
            let cache = self.depot_cache.read().await;
            if let Some((ids, timestamp)) = &cache.depot_ids {
                if !self.is_depot_cache_expired(*timestamp) {
                    debug!("Depot cache HIT for depot_ids");
                    return Ok(ids.clone());
                }
            }
        }

        debug!("Depot cache MISS for depot_ids");
        let query = "SELECT DISTINCT entity_id FROM entities LIMIT 1048575";
        match sqlx::query_as::<_, (Option<i64>,)>(query)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => {
                let ids: Vec<String> = rows
                    .into_iter()
                    .filter_map(|r| r.0.map(|id| id.to_string()))
                    .collect();
                // Update cache
                let mut cache = self.depot_cache.write().await;
                cache.depot_ids = Some((ids.clone(), SystemTime::now()));
                Ok(ids)
            }
            Err(e) => {
                error!("get_depot_ids query failed: {}", e);
                Ok(Vec::new())
            }
        }
    }

    async fn get_depot_name_by_id(&self, depot_id: String) -> AppResult<String> {
        // Check cache first
        {
            let cache = self.depot_cache.read().await;
            if let Some((name, timestamp)) = cache.depot_name_by_id.get(&depot_id) {
                if !self.is_depot_cache_expired(*timestamp) {
                    debug!("Depot cache HIT for depot_name_by_id: {}", depot_id);
                    return Ok(name.clone());
                }
            }
        }

        debug!("Depot cache MISS for depot_name_by_id: {}", depot_id);
        let depot_id_int = depot_id
            .parse::<i32>()
            .map_err(|_| AppError::BadRequest(format!("Invalid depot_id: {}", depot_id)))?;

        let query = r#"SELECT entity_name FROM entities WHERE entity_id = $1"#;
        match sqlx::query_as::<_, (String,)>(query)
            .bind(depot_id_int)
            .fetch_one(&self.pool)
            .await
        {
            Ok((depot_name,)) => {
                info!(
                    "get_depot_name_by_id: depot_id={}, depot_name={}",
                    depot_id, depot_name
                );
                // Update cache
                let mut cache = self.depot_cache.write().await;
                cache
                    .depot_name_by_id
                    .insert(depot_id.clone(), (depot_name.clone(), SystemTime::now()));
                Ok(depot_name)
            }
            Err(e) => {
                error!(
                    "get_depot_name_by_id query failed for depot_id {}: {}",
                    depot_id, e
                );
                Err(AppError::NotFound(format!(
                    "Depot with id {} not found",
                    depot_id
                )))
            }
        }
    }

    async fn clear_depot_cache(&self) -> AppResult<()> {
        let mut cache = self.depot_cache.write().await;
        cache.depot_names = None;
        cache.depot_ids = None;
        cache.depot_name_by_id.clear();
        cache.vehicles_by_depot_name.clear();
        cache.vehicles_by_depot_id.clear();
        info!("Depot cache cleared successfully");
        Ok(())
    }

    async fn get_vehicle_operation_data(&self, fleet_no: &str) -> AppResult<VehicleOperationData> {
        // First try to get data from waybills with status = 'Online'
        let waybill_query = r#"SELECT w.waybill_id::text, w.waybill_no::text, w.entity_id::text AS depot_id, e.entity_name AS depot_name, w.conductor_token_no::text AS conductor_code, w.driver_token_no::text AS driver_code, w.schedule_no FROM waybills w LEFT JOIN entities e ON e.entity_id = w.entity_id WHERE w.vehicle_no = $1 AND w.status = 'Online' LIMIT 1"#;

        match sqlx::query_as::<_, VehicleOperationData>(waybill_query)
            .bind(fleet_no)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(data)) => {
                info!(
                    "get_vehicle_operation_data: Found online waybill for fleet_no={}",
                    fleet_no
                );
                return Ok(data);
            }
            Ok(None) => {
                debug!("get_vehicle_operation_data: No online waybill found for fleet_no={}, checking vehicles table", fleet_no);
            }
            Err(e) => {
                error!(
                    "get_vehicle_operation_data: waybill query failed for fleet_no={}: {}",
                    fleet_no, e
                );
            }
        }

        // Fallback to vehicles table if waybill not found or query failed
        let vehicles_query = r#"SELECT NULL::text AS waybill_id, NULL::text AS waybill_no, v.entity_id::text AS depot_id, e.entity_name AS depot_name, NULL::text AS conductor_code, NULL::text AS driver_code, NULL::text as schedule_no FROM vehicles v LEFT JOIN entities e ON e.entity_id = v.entity_id WHERE v.fleet_no = $1 LIMIT 1;"#;

        match sqlx::query_as::<_, VehicleOperationData>(vehicles_query)
            .bind(fleet_no)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(data)) => {
                info!(
                    "get_vehicle_operation_data: Found vehicle data for fleet_no={}",
                    fleet_no
                );
                Ok(data)
            }
            Ok(None) => {
                error!(
                    "get_vehicle_operation_data: No data found for fleet_no={}",
                    fleet_no
                );
                Err(AppError::NotFound(format!(
                    "No operation data found for fleet_no: {}",
                    fleet_no
                )))
            }
            Err(e) => {
                error!(
                    "get_vehicle_operation_data: vehicles query failed for fleet_no={}: {}",
                    fleet_no, e
                );
                Err(AppError::Internal(format!("Database query failed: {}", e)))
            }
        }
    }

    async fn verify_vehicle(&self, vehicle_no: &str) -> AppResult<bool> {
        let all_pool = self.get_all_vehicles_pool().await?;

        let is_valid = all_pool.contains(vehicle_no);

        info!(
            "Vehicle verification for {}: is_valid={}, all_pool_size={}",
            vehicle_no,
            is_valid,
            all_pool.len()
        );

        Ok(is_valid)
    }

    async fn get_waybills_by_route_id(
        &self,
        gtfs_id: &str,
        route_id: &str,
    ) -> AppResult<Vec<VehicleData>> {
        let cache_key = self.get_waybills_by_route_cache_key(gtfs_id, route_id);

        // Check cache first
        {
            let cache = self.waybills_by_route_cache.read().await;
            if let Some((waybills, timestamp)) = cache.waybills_by_route.get(&cache_key) {
                if !self.is_waybills_by_route_cache_expired(*timestamp) {
                    info!(
                        "Waybills by route cache HIT for gtfs_id={}, route_id={}",
                        gtfs_id, route_id
                    );
                    return Ok(waybills.clone());
                }
            }
        }

        info!(
            "Waybills by route cache MISS for gtfs_id={}, route_id={}",
            gtfs_id, route_id
        );

        // Query waybills by joining with bus_schedule_trip_detail and bus_schedule_trip_flexi
        // to find waybills that have trips on the given route_id (route_number_id)
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
                e.entity_remark::text AS entity_remark,
                w.driver_token_no::text AS driver_code,
                w.conductor_token_no::text AS conductor_code,
                w.deleted AS deleted,
                w.status AS status,
                w.is_flexi
            FROM waybills w
            LEFT JOIN entities e ON e.entity_id = w.entity_id
            WHERE w.status = 'Online'
                AND w.deleted = false
                AND (
                    EXISTS (
                        SELECT 1 FROM bus_schedule_trip_detail bstd
                        WHERE bstd.schedule_trip_id = w.schedule_trip_id::bigint
                            AND bstd.route_number_id::text = $1
                            AND bstd.trip_type != 'dead-trip'
                    )
                    OR EXISTS (
                        SELECT 1 FROM bus_schedule_trip_flexi bstf
                        WHERE bstf.schedule_trip_id = w.schedule_trip_id::bigint
                            AND bstf.route_number_id::text = $1
                            AND bstf.trip_type != 'dead-trip'
                    )
                )
            ORDER BY w.updated_at DESC
        "#;

        let waybills = match sqlx::query_as::<_, VehicleData>(query)
            .bind(route_id)
            .fetch_all(&self.pool)
            .await
        {
            Ok(waybills) => {
                info!(
                    "Found {} waybills for gtfs_id={}, route_id={}",
                    waybills.len(),
                    gtfs_id,
                    route_id
                );
                waybills
            }
            Err(e) => {
                error!("Failed to query waybills by route_id {}: {}", route_id, e);
                return Err(AppError::Internal(format!("Database query failed: {}", e)));
            }
        };

        // Cache the results
        {
            let mut cache = self.waybills_by_route_cache.write().await;
            cache
                .waybills_by_route
                .insert(cache_key, (waybills.clone(), SystemTime::now()));
        }

        Ok(waybills)
    }

    async fn get_vehicles_by_service_tier(
        &self,
        gtfs_id: &str,
        service_tier: &str,
    ) -> AppResult<Vec<String>> {
        if gtfs_id == "chennai_bus" && service_tier.eq_ignore_ascii_case("AC") {
            let query = r#"
                SELECT vehicle_no
                FROM (
                    SELECT DISTINCT ON (vehicle_no)
                        vehicle_no,
                        schedule_no,
                        created_at
                    FROM public.waybills
                    WHERE vehicle_no IS NOT NULL
                    ORDER BY vehicle_no, created_at DESC
                ) t
                 WHERE schedule_no LIKE 'Z%';
            "#;

            match sqlx::query_as::<_, (String,)>(query)
                .fetch_all(&self.pool)
                .await
            {
                Ok(rows) => Ok(rows.into_iter().map(|(v,)| v).collect::<Vec<String>>()),
                Err(e) => {
                    error!("get_vehicles_by_service_tier query failed for gtfs_id={}, service_tier={}: {}", gtfs_id, service_tier, e);
                    Ok(Vec::new())
                }
            }
        } else {
            // For other gtfs_id/service_tier combinations, return empty list
            Ok(Vec::new())
        }
    }
}
