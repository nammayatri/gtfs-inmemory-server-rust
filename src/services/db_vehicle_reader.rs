use async_trait::async_trait;
use sqlx::postgres::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::{debug, error, info};

use crate::environment::AppConfig;
use crate::models::{
    BusSchedule, DepotVehicleSummary, MinimalVehicleData, RouteLastScheduleTime, VehicleData,
    VehicleDataWithRouteId, VehicleOperationData, WaybillStatus,
};
use crate::tools::error::{AppError, AppResult};

pub use crate::services::chalo_vehicle_cache::{chalo_gtfs_ids, is_chalo_gtfs_id};

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
    last_failure: Option<SystemTime>,
}

struct CachedVehicleData {
    data: VehicleDataWithRouteId,
    stored_at: SystemTime,
    ttl: Duration,
}

const VEHICLE_DATA_NEGATIVE_CACHE_SECS: u64 = 300;
const VEHICLE_DATA_CACHE_MAX_ENTRIES: usize = 20000;
const VEHICLE_POOL_RETRY_SECS: u64 = 60;
const AC_VEHICLES_CACHE_SECS: u64 = 3600;
const VEHICLE_IDS_CHUNK_SIZE: usize = 1000;

struct RouteTripRow {
    vehicle_no: String,
    waybill_no: String,
    service_type: String,
    status: Option<String>,
    duty_date: Option<String>,
    db_start_time: Option<String>,
    start_time_epoch: Option<String>,
    trip_number: Option<i32>,
    is_active_trip: Option<bool>,
}

impl RouteTripRow {
    fn to_vehicle_data(&self) -> VehicleData {
        VehicleData {
            waybill_id: String::new(),
            waybill_no: self.waybill_no.clone(),
            service_type: self.service_type.clone(),
            vehicle_no: self.vehicle_no.clone(),
            schedule_no: String::new(),
            last_updated: None,
            duty_date: self.duty_date.clone(),
            schedule_trip_id: None,
            entity_remark: None,
            driver_code: None,
            conductor_code: None,
            deleted: Some(false),
            status: self.status.clone(),
            is_flexi: None,
            db_start_time: self.db_start_time.clone(),
            start_time_epoch: self.start_time_epoch.clone(),
            trip_number: self.trip_number,
            is_active_trip: self.is_active_trip,
            is_completed: None,
        }
    }
}

#[derive(sqlx::FromRow)]
struct RouteTripDbRow {
    route_id: Option<String>,
    vehicle_no: Option<String>,
    waybill_no: Option<String>,
    service_type: Option<String>,
    status: Option<String>,
    duty_date: Option<String>,
    db_start_time: Option<String>,
    start_time_epoch: Option<String>,
    trip_number: Option<i32>,
    is_active_trip: Option<bool>,
}

struct OnlineRouteSnapshot {
    by_route: HashMap<String, Vec<RouteTripRow>>,
    loaded_at: SystemTime,
}

#[derive(Default)]
struct OnlineRouteSnapshotState {
    snapshot: Option<Arc<OnlineRouteSnapshot>>,
    last_failure: Option<SystemTime>,
}

const ROUTE_SNAPSHOT_REFRESH_SECS: u64 = 30;
const ROUTE_SNAPSHOT_RETRY_SECS: u64 = 5;

const ONLINE_ROUTE_TRIPS_QUERY: &str = r#"
    WITH ow AS MATERIALIZED (
        SELECT waybill_id, waybill_no, service_type, vehicle_no, duty_date, status,
               schedule_trip_id, is_flexi
        FROM waybills
        WHERE status = 'Online' AND deleted = false
    )
    SELECT
        t.route_number_id::text AS route_id,
        t.vehicle_no,
        t.waybill_no::text AS waybill_no,
        t.service_type,
        t.status,
        t.duty_date,
        t.db_start_time,
        t.start_time_epoch,
        t.trip_number,
        t.is_active_trip
    FROM (
        SELECT
            bstd.route_number_id, ow.vehicle_no, ow.waybill_no, ow.service_type, ow.status,
            ow.duty_date, bstd.start_time AS db_start_time,
            bstd.trip_start_time::text AS start_time_epoch,
            bstd.trip_number::int AS trip_number, bstd.is_active_trip
        FROM ow
        JOIN bus_schedule_trip_detail bstd
            ON bstd.schedule_trip_id = ow.schedule_trip_id
            AND bstd.trip_type <> 'dead-trip'
        WHERE ow.is_flexi = false
            AND bstd.route_number_id IS NOT NULL
        UNION ALL
        SELECT
            bstf.route_number_id, ow.vehicle_no, ow.waybill_no, ow.service_type, ow.status,
            ow.duty_date, bstf.start_time AS db_start_time,
            bstf.trip_start_time::text AS start_time_epoch,
            bstf.trip_number::int AS trip_number, bstf.is_active_trip
        FROM ow
        JOIN bus_schedule_trip_flexi bstf
            ON bstf.waybill_id = ow.waybill_id
            AND bstf.trip_type <> 'dead-trip'
        WHERE ow.is_flexi = true
            AND bstf.route_number_id IS NOT NULL
    ) t
    ORDER BY route_id, waybill_no, trip_number
"#;

// Routes served today cache structure (30 min TTL)
struct RoutesServedTodayCache {
    data: Option<(Vec<RouteLastScheduleTime>, SystemTime)>,
}

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
    async fn get_chennai_waybills_by_route_id(
        &self,
        route_id: &str,
        vehicle_number: Option<&str>,
    ) -> AppResult<Vec<VehicleData>>;
    async fn get_chennai_waybill_by_waybill_and_trip(
        &self,
        waybill_no: &str,
        trip_number: i32,
    ) -> AppResult<Vec<VehicleData>>;
    async fn get_routes_served_today(&self) -> AppResult<Vec<RouteLastScheduleTime>>;

    /// Service type only: same waybill priority as `get_vehicle_data`, without the trip/schedule queries.
    async fn get_vehicle_service_type(&self, vehicle_no: &str) -> AppResult<Option<String>>;
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

    async fn get_vehicle_service_type(&self, _vehicle_no: &str) -> AppResult<Option<String>> {
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
    async fn get_chennai_waybills_by_route_id(
        &self,
        _route_id: &str,
        _vehicle_number: Option<&str>,
    ) -> AppResult<Vec<VehicleData>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }
    async fn get_chennai_waybill_by_waybill_and_trip(
        &self,
        _waybill_no: &str,
        _trip_number: i32,
    ) -> AppResult<Vec<VehicleData>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }
    async fn get_routes_served_today(&self) -> AppResult<Vec<RouteLastScheduleTime>> {
        Ok(Vec::new())
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

#[derive(Clone)]
pub struct DBVehicleReader {
    pool: PgPool,
    cache: Arc<RwLock<HashMap<String, CachedVehicleData>>>,
    cache_duration: Duration,
    refreshing_keys: Arc<RwLock<std::collections::HashSet<String>>>,
    depot_cache: Arc<RwLock<DepotCache>>,
    depot_cache_duration: Duration,
    vehicle_pool_cache: Arc<RwLock<VehiclePoolCache>>,
    vehicle_pool_cache_duration: Duration,
    vehicle_pool_refresh: Arc<tokio::sync::Mutex<()>>,
    route_snapshot: Arc<RwLock<OnlineRouteSnapshotState>>,
    route_snapshot_duration: Duration,
    route_snapshot_refresh: Arc<tokio::sync::Mutex<()>>,
    routes_served_today_cache: Arc<RwLock<RoutesServedTodayCache>>,
    routes_served_today_cache_duration: Duration,
    routes_served_today_refresh: Arc<tokio::sync::Mutex<()>>,
    ac_vehicles_cache: Arc<RwLock<Option<(Vec<String>, SystemTime)>>>,
    ac_vehicles_refresh: Arc<tokio::sync::Mutex<()>>,
}

impl DBVehicleReader {
    pub fn new(pool: PgPool, config: &AppConfig) -> Self {
        Self {
            pool,
            cache: Arc::new(RwLock::new(HashMap::new())),
            cache_duration: Duration::from_secs(config.cache_duration),
            refreshing_keys: Arc::new(RwLock::new(std::collections::HashSet::new())),
            depot_cache: Arc::new(RwLock::new(DepotCache {
                depot_names: None,
                depot_ids: None,
                depot_name_by_id: HashMap::new(),
                vehicles_by_depot_name: HashMap::new(),
                vehicles_by_depot_id: HashMap::new(),
            })),
            depot_cache_duration: Duration::from_secs(7200), // 2 hours TTL
            vehicle_pool_cache: Arc::new(RwLock::new(VehiclePoolCache {
                all_vehicles: None,
                last_failure: None,
            })),
            vehicle_pool_cache_duration: Duration::from_secs(10800), // 3 hours TTL
            vehicle_pool_refresh: Arc::new(tokio::sync::Mutex::new(())),
            route_snapshot: Arc::new(RwLock::new(OnlineRouteSnapshotState::default())),
            route_snapshot_duration: Duration::from_secs(ROUTE_SNAPSHOT_REFRESH_SECS),
            route_snapshot_refresh: Arc::new(tokio::sync::Mutex::new(())),
            routes_served_today_cache: Arc::new(RwLock::new(RoutesServedTodayCache { data: None })),
            routes_served_today_cache_duration: Duration::from_secs(1800), // 30 minutes TTL
            routes_served_today_refresh: Arc::new(tokio::sync::Mutex::new(())),
            ac_vehicles_cache: Arc::new(RwLock::new(None)),
            ac_vehicles_refresh: Arc::new(tokio::sync::Mutex::new(())),
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

    fn route_snapshot_recently_failed(state: &OnlineRouteSnapshotState) -> bool {
        state
            .last_failure
            .map(|t| {
                t.elapsed().unwrap_or_default() < Duration::from_secs(ROUTE_SNAPSHOT_RETRY_SECS)
            })
            .unwrap_or(false)
    }

    async fn load_online_route_snapshot(&self) -> AppResult<OnlineRouteSnapshot> {
        let rows = sqlx::query_as::<_, RouteTripDbRow>(ONLINE_ROUTE_TRIPS_QUERY)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::Internal(format!("Database query failed: {}", e)))?;

        let total = rows.len();
        let mut skipped = 0usize;
        let mut by_route: HashMap<String, Vec<RouteTripRow>> = HashMap::new();
        for row in rows {
            match (
                row.route_id,
                row.vehicle_no,
                row.waybill_no,
                row.service_type,
            ) {
                (Some(route_id), Some(vehicle_no), Some(waybill_no), Some(service_type)) => {
                    by_route.entry(route_id).or_default().push(RouteTripRow {
                        vehicle_no,
                        waybill_no,
                        service_type,
                        status: row.status,
                        duty_date: row.duty_date,
                        db_start_time: row.db_start_time,
                        start_time_epoch: row.start_time_epoch,
                        trip_number: row.trip_number,
                        is_active_trip: row.is_active_trip,
                    });
                }
                _ => skipped += 1,
            }
        }

        info!(
            "Online route snapshot loaded: rows={}, routes={}, skipped_incomplete_rows={}",
            total,
            by_route.len(),
            skipped
        );

        Ok(OnlineRouteSnapshot {
            by_route,
            loaded_at: SystemTime::now(),
        })
    }

    async fn refresh_online_route_snapshot(&self) -> AppResult<Arc<OnlineRouteSnapshot>> {
        match self.load_online_route_snapshot().await {
            Ok(snapshot) => {
                let snapshot = Arc::new(snapshot);
                let mut state = self.route_snapshot.write().await;
                state.snapshot = Some(snapshot.clone());
                state.last_failure = None;
                Ok(snapshot)
            }
            Err(e) => {
                error!("Online route snapshot refresh failed: {:?}", e);
                let mut state = self.route_snapshot.write().await;
                state.last_failure = Some(SystemTime::now());
                Err(e)
            }
        }
    }

    async fn online_route_snapshot(&self) -> AppResult<Arc<OnlineRouteSnapshot>> {
        let (stale, recently_failed) = {
            let state = self.route_snapshot.read().await;
            match &state.snapshot {
                Some(snapshot)
                    if snapshot.loaded_at.elapsed().unwrap_or_default()
                        < self.route_snapshot_duration =>
                {
                    return Ok(snapshot.clone());
                }
                Some(snapshot) => (
                    Some(snapshot.clone()),
                    Self::route_snapshot_recently_failed(&state),
                ),
                None => (None, Self::route_snapshot_recently_failed(&state)),
            }
        };

        if let Some(snapshot) = stale {
            if !recently_failed {
                if let Ok(guard) = self.route_snapshot_refresh.clone().try_lock_owned() {
                    let reader = self.clone();
                    tokio::spawn(async move {
                        let _guard = guard;
                        let _ = reader.refresh_online_route_snapshot().await;
                    });
                }
            }
            return Ok(snapshot);
        }

        let _guard = self.route_snapshot_refresh.lock().await;
        {
            let state = self.route_snapshot.read().await;
            if let Some(snapshot) = &state.snapshot {
                return Ok(snapshot.clone());
            }
            if Self::route_snapshot_recently_failed(&state) {
                return Err(AppError::Internal(
                    "Online route snapshot unavailable".to_string(),
                ));
            }
        }
        self.refresh_online_route_snapshot().await
    }

    fn current_vehicle_pool(
        &self,
        cache: &VehiclePoolCache,
    ) -> Option<(std::collections::HashSet<String>, bool)> {
        let (vehicles, timestamp) = cache.all_vehicles.as_ref()?;
        let recently_failed = cache
            .last_failure
            .map(|t| t.elapsed().unwrap_or_default() < Duration::from_secs(VEHICLE_POOL_RETRY_SECS))
            .unwrap_or(false);
        let fresh = !self.is_vehicle_pool_cache_expired(*timestamp) || recently_failed;
        Some((vehicles.clone(), fresh))
    }

    async fn get_all_vehicles_pool(&self) -> AppResult<std::collections::HashSet<String>> {
        let stale = {
            let cache = self.vehicle_pool_cache.read().await;
            match self.current_vehicle_pool(&cache) {
                Some((vehicles, true)) => {
                    info!("All vehicles pool cache HIT");
                    return Ok(vehicles);
                }
                Some((vehicles, false)) => Some(vehicles),
                None => None,
            }
        };

        let _guard = match (&stale, self.vehicle_pool_refresh.try_lock()) {
            (_, Ok(guard)) => guard,
            (Some(vehicles), Err(_)) => return Ok(vehicles.clone()),
            (None, Err(_)) => self.vehicle_pool_refresh.lock().await,
        };

        {
            let cache = self.vehicle_pool_cache.read().await;
            if let Some((vehicles, true)) = self.current_vehicle_pool(&cache) {
                return Ok(vehicles);
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
        let mut failed = false;

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
                failed = true;
            }
        }

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
                failed = true;
            }
        }

        let mut cache = self.vehicle_pool_cache.write().await;
        if failed {
            cache.last_failure = Some(SystemTime::now());
            return Ok(stale.unwrap_or(all_vehicles));
        }
        cache.all_vehicles = Some((all_vehicles.clone(), SystemTime::now()));
        cache.last_failure = None;
        Ok(all_vehicles)
    }

    async fn get_cached_entry(&self, cache_key: &str) -> Option<(VehicleDataWithRouteId, bool)> {
        let cache = self.cache.read().await;
        if let Some(entry) = cache.get(cache_key) {
            debug!("Cache HIT for key {}", cache_key);
            let expired = entry.stored_at.elapsed().unwrap_or_default() >= entry.ttl;
            return Some((entry.data.clone(), expired));
        }
        debug!("Cache MISS for key {}", cache_key);
        None
    }

    fn get_cache_key(&self, vehicle_no: &str, trip_number: Option<i32>) -> String {
        vehicle_no.to_string()
            + trip_number
                .map(|t| t.to_string())
                .unwrap_or("".to_string())
                .as_str()
    }

    fn get_vehicles_by_ids_cache_key(vehicle_no: &str) -> String {
        format!("ids:{}", vehicle_no)
    }

    async fn cache_vehicle_data(
        &self,
        cache_key: String,
        vehicle_data: VehicleDataWithRouteId,
        ttl: Duration,
    ) {
        let mut cache = self.cache.write().await;
        if cache.len() >= VEHICLE_DATA_CACHE_MAX_ENTRIES && !cache.contains_key(&cache_key) {
            cache.retain(|_, entry| entry.stored_at.elapsed().unwrap_or_default() < entry.ttl);
            if cache.len() >= VEHICLE_DATA_CACHE_MAX_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(
            cache_key,
            CachedVehicleData {
                data: vehicle_data,
                stored_at: SystemTime::now(),
                ttl,
            },
        );
    }

    async fn refresh_vehicle_data_in_background(
        &self,
        vehicle_no: String,
        trip_number: Option<i32>,
    ) {
        let cache_key = self.get_cache_key(&vehicle_no, trip_number);
        {
            let mut refreshing = self.refreshing_keys.write().await;
            if !refreshing.insert(cache_key.clone()) {
                debug!(
                    "Another thread is already refreshing vehicle {}",
                    vehicle_no
                );
                return;
            }
        }

        let reader = self.clone();
        tokio::spawn(async move {
            debug!("Starting background refresh for vehicle {}", vehicle_no);
            match reader.load_vehicle_data(&vehicle_no, trip_number).await {
                Ok(_) => debug!("Background refresh completed for vehicle {}", vehicle_no),
                Err(e) => error!(
                    "Background refresh failed for vehicle {}: {:?}",
                    vehicle_no, e
                ),
            }
            reader.refreshing_keys.write().await.remove(&cache_key);
        });
    }

    async fn load_vehicle_data(
        &self,
        vehicle_no: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId> {
        let cache_key = self.get_cache_key(vehicle_no, trip_number);
        let (waybill_result, waybill_status) = self.get_waybill_with_priority(vehicle_no).await?;

        let mut schedule_map: HashMap<String, Vec<BusSchedule>> = HashMap::new();
        match waybill_result {
            Some(vehicle_data) => {
                info!(
                    "Enhanced flow - vehicle_data: {:?}, status: {:?}",
                    vehicle_data, waybill_status
                );

                let (schedule_result, is_active_trip, remaining_trip_details) = self
                    .resolve_trip_data(vehicle_data.clone(), waybill_status.clone(), trip_number)
                    .await?;

                let is_active_trip =
                    is_active_trip && matches!(waybill_status, WaybillStatus::Online);

                if let Some(ref remaining) = remaining_trip_details {
                    for row in remaining.iter() {
                        if let Some(key) = row.schedule_trip_id.clone() {
                            schedule_map.entry(key).or_default().push(row.clone());
                        }
                    }
                }
                if let Some(ref schedule) = schedule_result {
                    if let Some(key) = schedule.schedule_trip_id.clone() {
                        schedule_map
                            .entry(key)
                            .or_default()
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
                    waybill_status: Some(waybill_status),
                };

                if let Some(schedule) = schedule_result {
                    vehicle_data_with_route_id.trip_number = schedule.trip_number;
                    vehicle_data_with_route_id.route_id = Some(schedule.route_id.to_owned());
                    vehicle_data_with_route_id.depot = schedule.org_name.clone();
                    vehicle_data_with_route_id.route_number = schedule.route_number.clone();
                    vehicle_data_with_route_id.db_start_time = schedule.db_start_time.clone();
                    vehicle_data_with_route_id.db_end_time = schedule.db_end_time.clone();
                }

                self.cache_vehicle_data(
                    cache_key,
                    vehicle_data_with_route_id.clone(),
                    self.cache_duration,
                )
                .await;
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
                        .fetch_optional(&self.pool)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            error!(
                                "Waybill Status Agnostic query failed for {}: {}",
                                vehicle_no, e
                            );
                            return Err(AppError::Internal(format!(
                                "Database query failed: {}",
                                e
                            )));
                        }
                    };

                let vehicle_data_with_route_id = match minimal_vehicle_data {
                    Some(minimal_vehicle_data) => VehicleDataWithRouteId::without_waybill(
                        minimal_vehicle_data.vehicle_no,
                        Some(minimal_vehicle_data.service_type),
                    ),
                    None => VehicleDataWithRouteId::without_waybill(vehicle_no.to_string(), None),
                };

                self.cache_vehicle_data(
                    cache_key,
                    vehicle_data_with_route_id.clone(),
                    Duration::from_secs(VEHICLE_DATA_NEGATIVE_CACHE_SECS),
                )
                .await;
                Ok(vehicle_data_with_route_id)
            }
        }
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
                Err(AppError::Internal(format!("Database query failed: {}", e)))
            }
        }
    }

    /// Query for non-online waybills (Processed, New, Closed, Audited)
    async fn get_processed_new_waybill(&self, vehicle_no: &str) -> AppResult<Option<VehicleData>> {
        let fallback_query = r#"
            WITH picked AS (
                SELECT * FROM (
                    (SELECT w.*, 1 AS prio FROM waybills w
                     WHERE w.vehicle_no = $1 AND w.status = 'Processed' AND w.deleted = false
                     ORDER BY w.updated_at DESC LIMIT 1)
                    UNION ALL
                    (SELECT w.*, 2 AS prio FROM waybills w
                     WHERE w.vehicle_no = $1 AND w.status = 'New' AND w.deleted = false
                     ORDER BY w.updated_at DESC LIMIT 1)
                    UNION ALL
                    (SELECT w.*, 3 AS prio FROM waybills w
                     WHERE w.vehicle_no = $1 AND w.status = 'Closed' AND w.deleted = false
                     ORDER BY w.updated_at DESC LIMIT 1)
                    UNION ALL
                    (SELECT w.*, 4 AS prio FROM waybills w
                     WHERE w.vehicle_no = $1 AND w.status = 'Audited' AND w.deleted = false
                     ORDER BY w.updated_at DESC LIMIT 1)
                ) candidates
                ORDER BY prio
                LIMIT 1
            )
            SELECT
                p.waybill_id::text,
                p.waybill_no::text,
                p.service_type,
                p.vehicle_no,
                p.schedule_no,
                p.updated_at::timestamptz AS last_updated,
                p.duty_date,
                p.schedule_trip_id::text,
                p.is_flexi,
                e.entity_remark::text AS entity_remark,
                p.driver_token_no::text AS driver_code,
                p.conductor_token_no::text AS conductor_code,
                p.deleted AS deleted,
                p.status AS status
            FROM picked p
            LEFT JOIN entities e ON e.entity_id = p.entity_id
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
                Err(AppError::Internal(format!("Database query failed: {}", e)))
            }
        }
    }

    /// Split waybill query strategy with priority: Online first, then Processed/New/Closed/Audited
    async fn get_waybill_with_priority(
        &self,
        vehicle_no: &str,
    ) -> AppResult<(Option<VehicleData>, WaybillStatus)> {
        // Priority 1: Online waybills
        if let Some(online_waybill) = self.get_online_waybill(vehicle_no).await? {
            return Ok((Some(online_waybill), WaybillStatus::Online));
        }

        // Priority 2: Processed/New/Closed/Audited waybills
        if let Some(w) = self.get_processed_new_waybill(vehicle_no).await? {
            let status = w
                .status
                .as_deref()
                .map(WaybillStatus::from_db_str)
                .unwrap_or(WaybillStatus::NotFound);
            return Ok((Some(w), status));
        }

        Ok((None, WaybillStatus::NotFound))
    }

    async fn get_vehicle_service_type_impl(&self, vehicle_no: &str) -> AppResult<Option<String>> {
        let prioritized_query = r#"
            SELECT service_type FROM (
                (SELECT w.service_type, 0 AS prio FROM waybills w
                 WHERE w.vehicle_no = $1 AND w.status = 'Online' AND w.deleted = false
                   AND w.service_type IS NOT NULL
                 ORDER BY w.updated_at DESC LIMIT 1)
                UNION ALL
                (SELECT w.service_type, 1 AS prio FROM waybills w
                 WHERE w.vehicle_no = $1 AND w.status = 'Processed' AND w.deleted = false
                   AND w.service_type IS NOT NULL
                 ORDER BY w.updated_at DESC LIMIT 1)
                UNION ALL
                (SELECT w.service_type, 2 AS prio FROM waybills w
                 WHERE w.vehicle_no = $1 AND w.status = 'New' AND w.deleted = false
                   AND w.service_type IS NOT NULL
                 ORDER BY w.updated_at DESC LIMIT 1)
                UNION ALL
                (SELECT w.service_type, 3 AS prio FROM waybills w
                 WHERE w.vehicle_no = $1 AND w.status = 'Closed' AND w.deleted = false
                   AND w.service_type IS NOT NULL
                 ORDER BY w.updated_at DESC LIMIT 1)
                UNION ALL
                (SELECT w.service_type, 4 AS prio FROM waybills w
                 WHERE w.vehicle_no = $1 AND w.status = 'Audited' AND w.deleted = false
                   AND w.service_type IS NOT NULL
                 ORDER BY w.updated_at DESC LIMIT 1)
            ) candidates
            ORDER BY prio
            LIMIT 1
        "#;

        let latest_query = r#"
            SELECT w.service_type
            FROM waybills w
            WHERE w.vehicle_no = $1
              AND w.service_type IS NOT NULL
            ORDER BY w.updated_at DESC
            LIMIT 1
        "#;

        for query in [prioritized_query, latest_query] {
            match sqlx::query_scalar::<_, String>(query)
                .bind(vehicle_no)
                .fetch_optional(&self.pool)
                .await
            {
                Ok(Some(service_type)) => return Ok(Some(service_type)),
                Ok(None) => {}
                Err(e) => {
                    error!(
                        "Service type waybill query failed for {}: {}",
                        vehicle_no, e
                    );
                    return Ok(None);
                }
            }
        }
        Ok(None)
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
                    schedule_trip_id::text AS schedule_trip_id,
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
                    schedule_trip_id::text AS schedule_trip_id,
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
                return Err(AppError::Internal(format!("Database query failed: {}", e)));
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
                    schedule_trip_id::text AS schedule_trip_id,
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
                    schedule_trip_id::text AS schedule_trip_id,
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
                         WHERE schedule_trip_id = $1::bigint AND is_active_trip = true AND trip_type != 'dead-trip'
                         ORDER BY trip_number LIMIT 1), 1))
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
                return Err(AppError::Internal(format!("Database query failed: {}", e)));
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

                // Remaining trips = only trips that come AFTER the active trip (by trip_number)
                let active_trip_num = active_trip.trip_number.unwrap_or(0);
                let remaining_trips: Vec<BusSchedule> = rows
                    .into_iter()
                    .filter(|trip| trip.trip_number.unwrap_or(0) > active_trip_num)
                    .collect();

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
                NULL::text AS schedule_trip_id,
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
                return Err(AppError::Internal(format!("Database query failed: {}", e)));
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
            WaybillStatus::Processed
            | WaybillStatus::New
            | WaybillStatus::Closed
            | WaybillStatus::Audited => {
                self.handle_detail_trips(&waybill_data, true, trip_number)
                    .await
            }
            WaybillStatus::NotFound => Ok((None, false, None)),
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

#[async_trait]
impl VehicleDataReader for DBVehicleReader {
    async fn get_vehicle_service_type(&self, vehicle_no: &str) -> AppResult<Option<String>> {
        self.get_vehicle_service_type_impl(vehicle_no).await
    }

    async fn get_vehicle_data(
        &self,
        vehicle_no: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId> {
        let cache_key = self.get_cache_key(vehicle_no, trip_number);
        if let Some((cached_data, expired)) = self.get_cached_entry(&cache_key).await {
            if expired {
                debug!(
                    "Cache expired for vehicle {}, triggering background refresh",
                    vehicle_no
                );
                self.refresh_vehicle_data_in_background(vehicle_no.to_string(), trip_number)
                    .await;
            }
            return Ok(cached_data);
        }

        self.load_vehicle_data(vehicle_no, trip_number).await
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

        let mut found_vehicles = Vec::new();
        let mut uncached_vehicle_nos = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for vehicle_no in &vehicle_nos {
            if !seen.insert(vehicle_no.clone()) {
                continue;
            }
            let cached = match self
                .get_cached_entry(&self.get_cache_key(vehicle_no, None))
                .await
            {
                Some(hit) => Some(hit),
                None => {
                    self.get_cached_entry(&Self::get_vehicles_by_ids_cache_key(vehicle_no))
                        .await
                }
            };
            match cached {
                Some((cached_data, false)) => found_vehicles.push(cached_data),
                _ => uncached_vehicle_nos.push(vehicle_no.clone()),
            }
        }

        if uncached_vehicle_nos.is_empty() {
            return Ok(found_vehicles);
        }

        let query = r#"
            SELECT DISTINCT ON (w.vehicle_no)
                w.waybill_id::text,
                w.waybill_no::text,
                w.service_type,
                w.vehicle_no,
                w.schedule_no,
                w.updated_at::timestamptz AS last_updated,
                w.duty_date,
                w.schedule_trip_id::text,
                NULL::text AS entity_remark,
                w.driver_token_no::text AS driver_code,
                w.conductor_token_no::text AS conductor_code,
                w.deleted,
                w.status,
                w.is_flexi
            FROM waybills w
            WHERE w.vehicle_no = ANY($1)
            ORDER BY w.vehicle_no, w.updated_at DESC
        "#;

        let mut vehicle_results: Vec<VehicleData> = Vec::new();
        for chunk in uncached_vehicle_nos.chunks(VEHICLE_IDS_CHUNK_SIZE) {
            match sqlx::query_as::<_, VehicleData>(query)
                .bind(chunk)
                .fetch_all(&self.pool)
                .await
            {
                Ok(mut rows) => vehicle_results.append(&mut rows),
                Err(e) => {
                    error!("get_vehicles_by_ids batch query failed: {}", e);
                }
            }
        }

        let schedule_numbers: Vec<String> = vehicle_results
            .iter()
            .map(|v| v.schedule_no.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let mut schedule_map = std::collections::HashMap::new();
        if !schedule_numbers.is_empty() {
            let schedule_query = r#"
                SELECT
                    NULL::int AS stops_count,
                    route_id::text AS route_id,
                    schedule_number,
                    NULL::text AS org_name,
                    NULL::int AS trip_number,
                    NULL::text AS schedule_trip_id,
                    NULL::text AS start_time,
                    NULL::text AS end_time,
                    FALSE AS deleted,
                    FALSE AS is_active_trip,
                    NULL::int AS trip_order,
                    NULL::text AS db_start_time,
                    NULL::text AS db_end_time
                FROM bus_schedule
                WHERE schedule_number = ANY($1) AND deleted = false
            "#;

            let mut schedule_results: Vec<BusSchedule> = Vec::new();
            for chunk in schedule_numbers.chunks(VEHICLE_IDS_CHUNK_SIZE) {
                match sqlx::query_as::<_, BusSchedule>(schedule_query)
                    .bind(chunk)
                    .fetch_all(&self.pool)
                    .await
                {
                    Ok(mut rows) => schedule_results.append(&mut rows),
                    Err(e) => {
                        error!("schedule batch query failed: {}", e);
                    }
                }
            }

            self.enrich_route_numbers(&mut schedule_results).await?;

            for schedule in schedule_results {
                schedule_map.insert(schedule.schedule_number.clone(), schedule);
            }
        }

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
                waybill_status: None,
            };

            if let Some(schedule_no) = &vehicle_data_with_route_id.schedule_no {
                if let Some(schedule) = schedule_map.get(schedule_no) {
                    vehicle_data_with_route_id.route_id = Some(schedule.route_id.clone());
                    vehicle_data_with_route_id.depot = schedule.org_name.clone();
                    vehicle_data_with_route_id.route_number = schedule.route_number.clone();
                }
            }

            self.cache_vehicle_data(
                Self::get_vehicles_by_ids_cache_key(&vehicle_data_with_route_id.vehicle_no),
                vehicle_data_with_route_id.clone(),
                self.cache_duration,
            )
            .await;
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

    async fn get_chennai_waybills_by_route_id(
        &self,
        route_id: &str,
        vehicle_number: Option<&str>,
    ) -> AppResult<Vec<VehicleData>> {
        let vehicle_number = vehicle_number.map(|v| v.trim()).filter(|v| !v.is_empty());
        let snapshot = self.online_route_snapshot().await?;
        let rows: Vec<VehicleData> = snapshot
            .by_route
            .get(route_id)
            .map(|rows| {
                rows.iter()
                    .filter(|row| vehicle_number.map_or(true, |vn| row.vehicle_no == vn))
                    .map(RouteTripRow::to_vehicle_data)
                    .collect()
            })
            .unwrap_or_default();
        debug!(
            "Chennai route snapshot: found {} rows for route_id={}",
            rows.len(),
            route_id
        );
        Ok(rows)
    }

    async fn get_chennai_waybill_by_waybill_and_trip(
        &self,
        waybill_no: &str,
        trip_number: i32,
    ) -> AppResult<Vec<VehicleData>> {
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
                CASE
                    WHEN w.is_flexi THEN bstf.start_time
                    ELSE bstd.start_time
                END AS db_start_time,
                CASE
                    WHEN w.is_flexi THEN bstf.trip_start_time::text
                    ELSE bstd.trip_start_time::text
                END AS start_time_epoch,
                CASE
                    WHEN w.is_flexi THEN bstf.trip_number::int
                    ELSE bstd.trip_number::int
                END AS trip_number
            FROM waybills w
            LEFT JOIN entities e
                ON e.entity_id = w.entity_id
            LEFT JOIN bus_schedule_trip_flexi bstf
                ON w.is_flexi = true
                AND bstf.waybill_id = w.waybill_id::bigint
                AND bstf.trip_number = $2
                AND bstf.trip_type <> 'dead-trip'
            LEFT JOIN bus_schedule_trip_detail bstd
                ON w.is_flexi = false
                AND bstd.schedule_trip_id = w.schedule_trip_id::bigint
                AND bstd.trip_number = $2
                AND bstd.trip_type <> 'dead-trip'
            WHERE
                w.waybill_no::text = $1
                AND w.status = 'Online'
                AND w.deleted = false
                AND (
                    (w.is_flexi = true AND bstf.waybill_id IS NOT NULL)
                    OR
                    (w.is_flexi = false AND bstd.schedule_trip_id IS NOT NULL)
                )
            ORDER BY w.waybill_no, trip_number;
        "#;

        match sqlx::query_as::<_, VehicleData>(query)
            .bind(waybill_no)
            .bind(trip_number)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => {
                info!(
                    "Chennai waybill+trip query: found {} rows for waybill_no={}, trip_number={}",
                    rows.len(),
                    waybill_no,
                    trip_number
                );
                Ok(rows)
            }
            Err(e) => {
                error!(
                    "get_chennai_waybill_by_waybill_and_trip failed for waybill_no={}, trip_number={}: {}",
                    waybill_no, trip_number, e
                );
                Err(AppError::Internal(format!("Database query failed: {}", e)))
            }
        }
    }

    async fn get_vehicles_by_service_tier(
        &self,
        gtfs_id: &str,
        service_tier: &str,
    ) -> AppResult<Vec<String>> {
        if gtfs_id != "chennai_bus" || !service_tier.eq_ignore_ascii_case("AC") {
            return Ok(Vec::new());
        }

        let is_fresh = |ts: &SystemTime| {
            ts.elapsed().unwrap_or_default() < Duration::from_secs(AC_VEHICLES_CACHE_SECS)
        };

        let stale = {
            let cache = self.ac_vehicles_cache.read().await;
            match cache.as_ref() {
                Some((vehicles, ts)) if is_fresh(ts) => return Ok(vehicles.clone()),
                Some((vehicles, _)) => Some(vehicles.clone()),
                None => None,
            }
        };

        let _guard = match (&stale, self.ac_vehicles_refresh.try_lock()) {
            (_, Ok(guard)) => guard,
            (Some(vehicles), Err(_)) => return Ok(vehicles.clone()),
            (None, Err(_)) => self.ac_vehicles_refresh.lock().await,
        };

        {
            let cache = self.ac_vehicles_cache.read().await;
            if let Some((vehicles, ts)) = cache.as_ref() {
                if is_fresh(ts) {
                    return Ok(vehicles.clone());
                }
            }
        }

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
            Ok(rows) => {
                let vehicles = rows.into_iter().map(|(v,)| v).collect::<Vec<String>>();
                let mut cache = self.ac_vehicles_cache.write().await;
                *cache = Some((vehicles.clone(), SystemTime::now()));
                Ok(vehicles)
            }
            Err(e) => {
                error!(
                    "get_vehicles_by_service_tier query failed for gtfs_id={}, service_tier={}: {}",
                    gtfs_id, service_tier, e
                );
                Ok(stale.unwrap_or_default())
            }
        }
    }

    async fn get_routes_served_today(&self) -> AppResult<Vec<RouteLastScheduleTime>> {
        let stale = {
            let cache = self.routes_served_today_cache.read().await;
            match &cache.data {
                Some((data, timestamp))
                    if timestamp.elapsed().unwrap_or_default()
                        < self.routes_served_today_cache_duration =>
                {
                    info!("Routes served today cache HIT");
                    return Ok(data.clone());
                }
                Some((data, _)) => Some(data.clone()),
                None => None,
            }
        };

        let _guard = match (&stale, self.routes_served_today_refresh.try_lock()) {
            (_, Ok(guard)) => guard,
            (Some(data), Err(_)) => return Ok(data.clone()),
            (None, Err(_)) => self.routes_served_today_refresh.lock().await,
        };

        {
            let cache = self.routes_served_today_cache.read().await;
            if let Some((data, timestamp)) = &cache.data {
                if timestamp.elapsed().unwrap_or_default() < self.routes_served_today_cache_duration
                {
                    return Ok(data.clone());
                }
            }
        }

        let today_ist = {
            let ist = chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
            chrono::Utc::now()
                .with_timezone(&ist)
                .format("%Y-%m-%d")
                .to_string()
        };

        info!(
            "Routes served today cache MISS, querying for date={}",
            today_ist
        );

        let query = r#"
            SELECT
                route_number_id::text AS route_id,
                MAX(end_time) AS last_schedule_time
            FROM (
                SELECT bstd.route_number_id, bstd.end_time
                FROM bus_schedule_trip_detail bstd
                INNER JOIN waybills w ON w.schedule_trip_id = bstd.schedule_trip_id
                WHERE w.duty_date = $1
                    AND w.deleted = false
                    AND w.is_flexi = false
                    AND bstd.trip_type != 'dead-trip'
                UNION ALL
                SELECT bstf.route_number_id, bstf.end_time
                FROM bus_schedule_trip_flexi bstf
                INNER JOIN waybills w ON w.waybill_id = bstf.waybill_id
                WHERE w.duty_date = $1
                    AND w.deleted = false
                    AND w.is_flexi = true
                    AND bstf.trip_type != 'dead-trip'
            ) combined
            GROUP BY route_number_id
            ORDER BY route_number_id
        "#;

        let routes = match sqlx::query_as::<_, RouteLastScheduleTime>(query)
            .bind(&today_ist)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => {
                info!("Found {} routes served today ({})", rows.len(), today_ist);
                rows
            }
            Err(e) => {
                error!("Failed to query routes served today: {}", e);
                if let Some(data) = stale {
                    return Ok(data);
                }
                return Err(AppError::Internal(format!("Database query failed: {}", e)));
            }
        };

        {
            let mut cache = self.routes_served_today_cache.write().await;
            cache.data = Some((routes.clone(), SystemTime::now()));
        }

        Ok(routes)
    }
}
