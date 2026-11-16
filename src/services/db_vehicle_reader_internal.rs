use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

use sqlx::PgPool;
use tracing::{debug, error, info, warn};

use crate::models::{
    BusSchedule, DepotInfo, DepotVehicleInfo, FleetOperationInfo, RouteLastScheduleTime,
    VehicleData, VehicleDataWithRouteId, VehicleOwnership, VehicleProperties,
    WaybillMetadataResponse, WaybillStatus, WaybillTripInfo,
};
use crate::services::gtfs_service::RouteCatalog;
use crate::services::operator::SUPPORTED_OPERATOR_GTFS_IDS;
use crate::services::reader_config::ReaderConfig;
use crate::services::schedule_snapshot::{
    ScheduleSnapshot, ScheduleSnapshotCache, ScheduleTripRow, ScheduledTrip, SCHEDULE_SNAPSHOT_TTL,
};
use crate::tools::error::{AppError, AppResult};

const INTERNAL_SCHEDULE_SNAPSHOT_QUERY: &str = r#"
    WITH live AS MATERIALIZED (
        SELECT waybill_id, waybill_no, service_type, vehicle_no, duty_date, status, schedule_trip_id, is_flexi
        FROM waybills_internal
        WHERE gtfs_id = $1 AND status IN ('online', 'upcoming') AND deleted = false
    ),
    trips AS (
        SELECT
            l.*,
            bstd.route_number_id,
            bstd.start_time,
            bstd.trip_start_time,
            bstd.trip_number,
            bstd.is_active_trip,
            bstd.is_completed
        FROM live l
        JOIN bus_schedule_trip_detail_internal bstd
            ON bstd.schedule_trip_id = l.schedule_trip_id
            AND bstd.gtfs_id = $1
            AND bstd.trip_type <> 'dead-trip'
            AND bstd.deleted = false
            AND (l.status <> 'online' OR LOWER(COALESCE(bstd.status, 'active')) <> 'inactive')
        WHERE l.is_flexi = false
        UNION ALL
        SELECT
            l.*,
            bstf.route_number_id,
            bstf.start_time,
            bstf.trip_start_time,
            bstf.trip_number,
            bstf.is_active_trip,
            NULL::boolean
        FROM live l
        JOIN bus_schedule_trip_flexi_internal bstf
            ON bstf.waybill_id = l.waybill_id
            AND bstf.gtfs_id = $1
            AND bstf.trip_type <> 'dead-trip'
            AND bstf.deleted = false
        WHERE l.is_flexi = true
    )
    SELECT
        route_number_id AS route_id,
        waybill_id,
        COALESCE(waybill_no, '') AS waybill_no,
        COALESCE(service_type, '') AS service_type,
        COALESCE(vehicle_no, '') AS vehicle_no,
        duty_date,
        status,
        start_time AS db_start_time,
        trip_start_time::text AS start_time_epoch,
        trip_number,
        CASE WHEN status <> 'online' THEN false ELSE is_active_trip END AS is_active_trip,
        is_completed
    FROM trips
"#;

#[async_trait]
pub trait VehicleDataReaderInternal: Send + Sync {
    async fn is_vehicle_in_internal(&self, vehicle_no: &str, gtfs_id: &str) -> bool;
    async fn get_vehicle_data(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId>;
    async fn get_waybills_by_route_id(
        &self,
        route_id: &str,
        gtfs_id: &str,
        vehicle_number: Option<&str>,
        max_duty_date: Option<&str>,
    ) -> AppResult<Vec<ScheduledTrip>>;
    async fn get_waybill_by_waybill_and_trip(
        &self,
        waybill_no: &str,
        trip_number: i32,
        gtfs_id: &str,
    ) -> AppResult<Vec<ScheduledTrip>>;
    async fn warm_schedule_snapshots(&self) {}

    async fn get_routes_served_today(
        &self,
        _gtfs_id: &str,
    ) -> AppResult<Vec<RouteLastScheduleTime>> {
        Ok(Vec::new())
    }

    async fn get_station_etas(&self, gtfs_id: &str) -> AppResult<HashMap<(String, String), i32>>;

    async fn upsert_station_eta(
        &self,
        gtfs_id: &str,
        source_station_code: &str,
        destination_station_code: &str,
        eta_in_seconds: i32,
    ) -> AppResult<()>;

    async fn get_waybill_metadata(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: Option<i32>,
    ) -> AppResult<WaybillMetadataResponse>;

    async fn get_vehicle_properties(
        &self,
        fleet_no: &str,
        gtfs_id: &str,
    ) -> AppResult<VehicleProperties>;

    async fn get_vehicle_ownership(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
    ) -> AppResult<VehicleOwnership>;

    /// Service type only: same waybill priority as `get_vehicle_data`, without the trip/schedule queries.
    async fn get_vehicle_service_type(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<String>>;

    // ── Depots (entities_internal / vehicles_internal) ──────────────────
    // Feed-scoped mirrors of the external reader's depot calls, same `Option`/empty contract.

    async fn get_depots(&self, gtfs_id: &str) -> AppResult<Vec<DepotInfo>>;

    async fn get_depot_by_id(&self, depot_id: &str, gtfs_id: &str) -> AppResult<Option<DepotInfo>>;

    async fn get_depot_vehicles(
        &self,
        depot_id: &str,
        gtfs_id: &str,
    ) -> AppResult<Vec<DepotVehicleInfo>>;

    /// Every registered bus of the feed (fleet-wide live map).
    async fn get_fleet_vehicles(&self, gtfs_id: &str) -> AppResult<Vec<DepotVehicleInfo>>;

    async fn get_fleet_operation(
        &self,
        fleet_no: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<FleetOperationInfo>>;

    async fn clear_depot_cache(&self) -> AppResult<()>;

    /// Drops the depot read caches after this service writes to the internal DB, so an
    /// operator's own edit is not hidden behind the TTL. Best-effort, hence no error.
    async fn clear_caches_after_write(&self);
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

    async fn get_waybills_by_route_id(
        &self,
        _route_id: &str,
        _gtfs_id: &str,
        _vehicle_number: Option<&str>,
        _max_duty_date: Option<&str>,
    ) -> AppResult<Vec<ScheduledTrip>> {
        Ok(Vec::new())
    }

    async fn get_waybill_by_waybill_and_trip(
        &self,
        _waybill_no: &str,
        _trip_number: i32,
        _gtfs_id: &str,
    ) -> AppResult<Vec<ScheduledTrip>> {
        Ok(Vec::new())
    }

    async fn get_station_etas(&self, _gtfs_id: &str) -> AppResult<HashMap<(String, String), i32>> {
        Ok(HashMap::new())
    }

    async fn upsert_station_eta(
        &self,
        _gtfs_id: &str,
        _source_station_code: &str,
        _destination_station_code: &str,
        _eta_in_seconds: i32,
    ) -> AppResult<()> {
        Ok(())
    }

    async fn get_waybill_metadata(
        &self,
        _gtfs_id: &str,
        _waybill_no: &str,
        _trip_number: Option<i32>,
    ) -> AppResult<WaybillMetadataResponse> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_vehicle_properties(
        &self,
        _fleet_no: &str,
        _gtfs_id: &str,
    ) -> AppResult<VehicleProperties> {
        Ok(VehicleProperties::default())
    }

    async fn get_vehicle_ownership(
        &self,
        _vehicle_no: &str,
        _gtfs_id: &str,
    ) -> AppResult<VehicleOwnership> {
        Ok(VehicleOwnership::default())
    }

    async fn get_vehicle_service_type(
        &self,
        _vehicle_no: &str,
        _gtfs_id: &str,
    ) -> AppResult<Option<String>> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_depots(&self, _gtfs_id: &str) -> AppResult<Vec<DepotInfo>> {
        Ok(Vec::new())
    }

    async fn get_depot_by_id(
        &self,
        _depot_id: &str,
        _gtfs_id: &str,
    ) -> AppResult<Option<DepotInfo>> {
        Ok(None)
    }

    async fn get_depot_vehicles(
        &self,
        _depot_id: &str,
        _gtfs_id: &str,
    ) -> AppResult<Vec<DepotVehicleInfo>> {
        Ok(Vec::new())
    }

    async fn get_fleet_vehicles(&self, _gtfs_id: &str) -> AppResult<Vec<DepotVehicleInfo>> {
        Ok(Vec::new())
    }

    async fn get_fleet_operation(
        &self,
        _fleet_no: &str,
        _gtfs_id: &str,
    ) -> AppResult<Option<FleetOperationInfo>> {
        Ok(None)
    }

    async fn clear_depot_cache(&self) -> AppResult<()> {
        Ok(())
    }

    async fn clear_caches_after_write(&self) {}
}

/// Feed-scoped depot lookups. Keys carry the gtfs_id because `entities_internal` is shared
/// across feeds, unlike the external reader's single-tenant `entities` table.
#[derive(Default)]
struct InternalDepotCache {
    depots: HashMap<String, (Vec<DepotInfo>, SystemTime)>,
    depot_by_id: HashMap<(String, String), (DepotInfo, SystemTime)>,
    depot_vehicles: HashMap<(String, String), (Vec<DepotVehicleInfo>, SystemTime)>,
}

#[allow(clippy::type_complexity)]
pub struct DBVehicleReaderInternal {
    pool: Option<PgPool>,
    schedule_snapshot: ScheduleSnapshotCache,
    station_eta_cache: Arc<RwLock<HashMap<String, (HashMap<(String, String), i32>, SystemTime)>>>,
    depot_cache: Arc<RwLock<InternalDepotCache>>,
    routes_served_today_cache:
        Arc<RwLock<HashMap<String, (Vec<RouteLastScheduleTime>, SystemTime)>>>,
    reader_config: Arc<ReaderConfig>,
    routes: RouteCatalog,
}

const STATION_ETA_CACHE_DURATION: u64 = 1800; // 30 mins

// 5 minutes. This service's own writes drop the cache (see clear_caches_after_write); the TTL
// only bounds staleness from writes made by another pod.
const DEPOT_CACHE_DURATION: u64 = 300;
const ROUTES_SERVED_TODAY_CACHE_DURATION: u64 = 1800; // 30 mins, same as the external reader

impl DBVehicleReaderInternal {
    pub fn new(pool: PgPool, reader_config: Arc<ReaderConfig>, routes: RouteCatalog) -> Self {
        Self {
            pool: Some(pool),
            schedule_snapshot: ScheduleSnapshotCache::new(SCHEDULE_SNAPSHOT_TTL),
            station_eta_cache: Arc::new(RwLock::new(HashMap::new())),
            depot_cache: Arc::new(RwLock::new(InternalDepotCache::default())),
            routes_served_today_cache: Arc::new(RwLock::new(HashMap::new())),
            reader_config,
            routes,
        }
    }

    /// Used when no DB is available (local mock / no DATABASE_URL).
    /// is_vehicle_in_internal will always return false, so get_vehicle_data is never reached.
    pub fn new_disconnected() -> Self {
        Self {
            pool: None,
            schedule_snapshot: ScheduleSnapshotCache::new(SCHEDULE_SNAPSHOT_TTL),
            station_eta_cache: Arc::new(RwLock::new(HashMap::new())),
            depot_cache: Arc::new(RwLock::new(InternalDepotCache::default())),
            routes_served_today_cache: Arc::new(RwLock::new(HashMap::new())),
            reader_config: Arc::new(ReaderConfig::default()),
            routes: RouteCatalog::empty(),
        }
    }

    fn empty_vehicle_data(vehicle_no: &str) -> VehicleDataWithRouteId {
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
            waybill_status: None,
            duty_trip_id: None,
        }
    }

    fn pool(&self) -> AppResult<&PgPool> {
        self.pool.as_ref().ok_or_else(|| {
            AppError::Internal("DBVehicleReaderInternal: no pool available".to_string())
        })
    }

    fn is_station_eta_cache_expired(&self, timestamp: SystemTime) -> bool {
        let elapsed = timestamp.elapsed().unwrap_or_default();
        elapsed >= Duration::from_secs(STATION_ETA_CACHE_DURATION)
    }

    fn is_depot_cache_expired(&self, timestamp: SystemTime) -> bool {
        let elapsed = timestamp.elapsed().unwrap_or_default();
        elapsed >= Duration::from_secs(DEPOT_CACHE_DURATION)
    }

    fn get_station_eta_cache_key(&self, gtfs_id: &str) -> String {
        format!("eta_map_{}", gtfs_id)
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

    pub async fn get_station_etas_impl(
        &self,
        gtfs_id: &str,
    ) -> AppResult<HashMap<(String, String), i32>> {
        let cache_key = self.get_station_eta_cache_key(gtfs_id);

        // Check cache
        {
            let cache = self.station_eta_cache.read().await;
            if let Some((etas, timestamp)) = cache.get(&cache_key) {
                if !self.is_station_eta_cache_expired(*timestamp) {
                    debug!("station_eta_cache HIT for gtfs_id={}", gtfs_id);
                    return Ok(etas.clone());
                }
            }
        }

        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(HashMap::new()),
        };

        let query = r#"
            SELECT source_station_code, destination_station_code, eta_in_seconds
            FROM station_eta
            WHERE gtfs_id = $1
        "#;

        let rows = sqlx::query(query)
            .bind(gtfs_id)
            .fetch_all(pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

        let mut eta_map = HashMap::new();
        for row in rows {
            use sqlx::Row;
            let src: String = row.get::<String, &str>("source_station_code");
            let dst: String = row.get::<String, &str>("destination_station_code");
            let secs: i32 = row.get::<i32, &str>("eta_in_seconds");
            eta_map.insert((src, dst), secs);
        }

        // Update cache
        {
            let mut cache = self.station_eta_cache.write().await;
            cache.insert(cache_key, (eta_map.clone(), SystemTime::now()));
        }

        Ok(eta_map)
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

        let mut schedule_map: HashMap<String, Vec<BusSchedule>> = HashMap::new();

        match waybill_result {
            Some(vehicle_data) => {
                info!(
                    "Internal flow — vehicle_data: {:?}, status: {:?}",
                    vehicle_data, waybill_status
                );

                let (schedule_result, is_active_trip, remaining_trip_details) = self
                    .resolve_trip_data(vehicle_data.clone(), waybill_status.clone(), trip_number)
                    .await?;

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
                    duty_trip_id: None,
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
                    let waybill_status = m.status.as_deref().map(WaybillStatus::from_db_str);
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
                        waybill_status,
                        duty_trip_id: None,
                    }
                } else {
                    Self::empty_vehicle_data(vehicle_no)
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

    async fn get_closed_audited_waybill(
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
              AND w.status IN ('closed', 'audited')
              AND w.deleted    = false
              AND w.gtfs_id   = $2
            ORDER BY
                CASE
                    WHEN w.status = 'closed'    THEN 1
                    WHEN w.status = 'audited'   THEN 2
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
        // Only consider Closed/Audited waybills, not New/Processed
        if let Some(w) = self.get_closed_audited_waybill(vehicle_no, gtfs_id).await? {
            let status = w
                .status
                .as_deref()
                .map(WaybillStatus::from_db_str)
                .unwrap_or(WaybillStatus::NotFound);
            return Ok((Some(w), status));
        }
        Ok((None, WaybillStatus::NotFound))
    }

    async fn get_vehicle_service_type_impl(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<String>> {
        let query = r#"
            SELECT w.service_type
            FROM waybills_internal w
            WHERE w.vehicle_no = $1
              AND w.gtfs_id    = $2
              AND w.service_type IS NOT NULL
            ORDER BY
                CASE
                    WHEN w.deleted = false AND w.status = 'online'  THEN 0
                    WHEN w.deleted = false AND w.status = 'closed'  THEN 1
                    WHEN w.deleted = false AND w.status = 'audited' THEN 2
                    ELSE 3
                END,
                w.updated_at DESC
            LIMIT 1
        "#;

        match sqlx::query_scalar::<_, String>(query)
            .bind(vehicle_no)
            .bind(gtfs_id)
            .fetch_optional(self.pool()?)
            .await
        {
            Ok(result) => Ok(result),
            Err(e) => {
                error!(
                    "Service type waybill_internal query failed for vehicle_no={} gtfs_id={}: {}",
                    vehicle_no, gtfs_id, e
                );
                Ok(None)
            }
        }
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
                    COALESCE(schedule_number, $2) AS schedule_number,
                    org_name::text AS org_name,
                    trip_number,
                    schedule_trip_id::text,
                    trip_start_time::text AS start_time,
                    trip_end_time::text AS end_time,
                    deleted,
                    is_active_trip,
                    trip_order,
                    start_time::text AS db_start_time,
                    end_time::text AS db_end_time
                FROM bus_schedule_trip_flexi_internal
                WHERE waybill_id::text = $1
                  AND trip_number >= {}
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                ORDER BY trip_number ASC
                "#,
                trip_num
            )
        } else {
            r#"
                SELECT
                    NULL::int AS stops_count,
                    route_number_id::text AS route_id,
                    COALESCE(schedule_number, $2) AS schedule_number,
                    org_name::text AS org_name,
                    trip_number,
                    schedule_trip_id::text,
                    trip_start_time::text AS start_time,
                    trip_end_time::text AS end_time,
                    deleted,
                    is_active_trip,
                    trip_order,
                    start_time::text AS db_start_time,
                    end_time::text AS db_end_time
                FROM bus_schedule_trip_flexi_internal
                WHERE waybill_id::text = $1
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                ORDER BY trip_number ASC
            "#
            .to_string()
        };

        let flexi_rows = match sqlx::query_as::<_, BusSchedule>(&flexi_query)
            .bind(&waybill_data.waybill_id)
            .bind(&waybill_data.schedule_no)
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
                    COALESCE(schedule_number, $2) AS schedule_number,
                    org_name::text AS org_name,
                    trip_number,
                    schedule_trip_id::text,
                    trip_start_time::text AS start_time,
                    trip_end_time::text AS end_time,
                    start_time::text AS db_start_time,
                    end_time::text AS db_end_time,
                    deleted,
                    is_active_trip,
                    trip_order
                FROM bus_schedule_trip_detail_internal
                WHERE schedule_trip_id::text = $1
                  AND trip_number >= {}
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                  AND LOWER(COALESCE(status, 'active')) <> 'inactive'
                ORDER BY trip_number ASC
                "#,
                trip_num
            )
        } else {
            r#"
                SELECT
                    NULL::int AS stops_count,
                    route_number_id::text AS route_id,
                    COALESCE(schedule_number, $2) AS schedule_number,
                    org_name::text AS org_name,
                    trip_number,
                    schedule_trip_id::text,
                    trip_start_time::text AS start_time,
                    trip_end_time::text AS end_time,
                    start_time::text AS db_start_time,
                    end_time::text AS db_end_time,
                    deleted,
                    is_active_trip,
                    trip_order
                FROM bus_schedule_trip_detail_internal
                WHERE schedule_trip_id::text = $1
                  AND trip_number >= (
                      SELECT COALESCE(
                          (SELECT trip_number FROM bus_schedule_trip_detail_internal
                           WHERE schedule_trip_id::text = $1
                             AND is_active_trip = true
                             AND trip_type != 'dead-trip'
                             AND deleted = false
                             AND LOWER(COALESCE(status, 'active')) <> 'inactive'),
                          1))
                  AND trip_type != 'dead-trip'
                  AND deleted = false
                  AND LOWER(COALESCE(status, 'active')) <> 'inactive'
                ORDER BY trip_number ASC
            "#
            .to_string()
        };

        let detail_rows = match sqlx::query_as::<_, BusSchedule>(&detail_query)
            .bind(&waybill_data.schedule_trip_id)
            .bind(&waybill_data.schedule_no)
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
            return self
                .process_trip_rows(detail_rows, force_first_active)
                .await;
        }

        // Final fallback: bus_schedule_internal
        self.handle_schedule_fallback(&waybill_data.schedule_no)
            .await
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
                NULL::text AS schedule_trip_id,
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

    async fn enrich_route_numbers(&self, schedules: &mut [BusSchedule]) -> AppResult<()> {
        for s in schedules.iter_mut() {
            if let Some(n) = self.routes.names(None, &s.route_id) {
                s.route_number = n.number;
            }
        }
        Ok(())
    }

    async fn schedule_snapshot(&self, gtfs_id: &str) -> Option<Arc<ScheduleSnapshot>> {
        if !SUPPORTED_OPERATOR_GTFS_IDS.contains(&gtfs_id) {
            return None;
        }
        let pool = self.pool.clone()?;
        let bound_gtfs_id = gtfs_id.to_string();
        self.schedule_snapshot
            .get(gtfs_id, move || async move {
                sqlx::query_as::<_, ScheduleTripRow>(INTERNAL_SCHEDULE_SNAPSHOT_QUERY)
                    .bind(bound_gtfs_id)
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| AppError::DbError(e.to_string()))
            })
            .await
            .map_err(|e| {
                error!(
                    "internal schedule snapshot unavailable for gtfs_id={}: {}",
                    gtfs_id, e
                )
            })
            .ok()
    }

    async fn get_waybills_by_route_id_impl(
        &self,
        route_id: &str,
        gtfs_id: &str,
        vehicle_number: Option<&str>,
        max_duty_date: Option<&str>,
    ) -> AppResult<Vec<ScheduledTrip>> {
        let Some(snapshot) = self.schedule_snapshot(gtfs_id).await else {
            return Ok(Vec::new());
        };
        let vehicle_number = vehicle_number.map(|v| v.trim()).filter(|v| !v.is_empty());
        Ok(snapshot
            .route_trips(route_id)
            .filter(|t| {
                let w = &t.waybill;
                let online = w.status.as_deref() == Some("online");
                vehicle_number.is_none_or(|vn| w.vehicle_no == vn)
                    && (online
                        || max_duty_date
                            .is_none_or(|max| w.duty_date.as_deref().is_some_and(|d| d <= max)))
                    && (!online || t.is_completed != Some(true))
            })
            .map(|t| t.to_scheduled())
            .collect())
    }

    async fn get_waybill_by_waybill_and_trip_impl(
        &self,
        waybill_no: &str,
        trip_number: i32,
        gtfs_id: &str,
    ) -> AppResult<Vec<ScheduledTrip>> {
        let Some(snapshot) = self.schedule_snapshot(gtfs_id).await else {
            return Ok(Vec::new());
        };
        Ok(snapshot
            .waybill_trips(waybill_no, trip_number)
            .map(|t| t.to_scheduled())
            .collect())
    }

    async fn upsert_station_eta_impl(
        &self,
        gtfs_id: &str,
        source_station_code: &str,
        destination_station_code: &str,
        eta_in_seconds: i32,
    ) -> AppResult<()> {
        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| AppError::DbError("Internal Database pool is not active".into()))?;

        let query = r#"
            INSERT INTO station_eta (gtfs_id, source_station_code, destination_station_code, eta_in_seconds)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (gtfs_id, source_station_code, destination_station_code)
            DO UPDATE SET
                eta_in_seconds = EXCLUDED.eta_in_seconds,
                updated_at = CURRENT_TIMESTAMP
        "#;

        sqlx::query(query)
            .bind(gtfs_id)
            .bind(source_station_code)
            .bind(destination_station_code)
            .bind(eta_in_seconds)
            .execute(pool)
            .await
            .map_err(|e| {
                error!(
                    "Failed to upsert station_eta for gtfs_id={}: {}",
                    gtfs_id, e
                );
                AppError::DbError(e.to_string())
            })?;

        // Invalidate cache
        {
            let mut cache = self.station_eta_cache.write().await;
            cache.remove(gtfs_id);
        }

        info!(
            "Successfully upserted station_eta for gtfs_id={} src={} dst={}",
            gtfs_id, source_station_code, destination_station_code
        );

        Ok(())
    }
}

fn v2_vehicle_data_select(status: &str) -> String {
    let vehicle_service = vehicle_service_join("dg");
    format!(
        r#"
    SELECT
        dg.id AS waybill_id,
        dg.waybill_no,
        COALESCE(st.service_type_name, '') AS service_type,
        COALESCE(dg.vehicle_number, '') AS vehicle_no,
        tg.code AS schedule_no,
        dg.updated_at AS last_updated,
        dg.operation_date::text AS duty_date,
        dg.id AS schedule_trip_id,
        e.entity_remark::text AS entity_remark,
        COALESCE(d.driver_token_number, dg.driver_token_number) AS driver_code,
        COALESCE(d.conductor_token_number, dg.conductor_token_number) AS conductor_code,
        false AS deleted,
        {status} AS status,
        false AS is_flexi,
        to_char(d.scheduled_start_at AT TIME ZONE COALESCE(f.timezone, '{DEFAULT_FEED_TZ}'), 'HH24:MI') AS db_start_time,
        (EXTRACT(EPOCH FROM d.recorded_start_time) * 1000)::bigint::text AS start_time_epoch,
        d.trip_number,
        (d.status = 'active') AS is_active_trip,
        (d.status = 'completed') AS is_completed,
        dg.waybill_no || '-' || d.trip_number AS duty_trip_id
    FROM duties d
    JOIN duty_groups dg ON dg.id = d.duty_group_id
    JOIN trip_groups tg ON tg.id = dg.trip_group_id
    {vehicle_service}
    LEFT JOIN entities_internal e
        ON e.entity_id::text = dg.depot_id AND e.gtfs_id = dg.gtfs_id
    LEFT JOIN gtfs_feed f ON f.gtfs_id = dg.gtfs_id
"#
    )
}

fn vehicle_service_join(run: &str) -> String {
    format!(
        "LEFT JOIN vehicles_internal v
        ON v.fleet_no = {run}.vehicle_number AND v.gtfs_id = {run}.gtfs_id AND v.deleted = false
    LEFT JOIN service_type_internal st
        ON st.service_type_id::text = COALESCE({run}.service_type_id, v.bus_service_type_id::text) AND st.gtfs_id = {run}.gtfs_id"
    )
}

fn in_live_window(column: &str, today: &str) -> String {
    format!("{column} BETWEEN {today} - 1 AND {today}")
}

fn or_empty<T: Default>(result: Result<T, sqlx::Error>, context: &str) -> T {
    result.unwrap_or_else(|e| {
        debug!("{}: {}", context, e);
        T::default()
    })
}

const DEFAULT_FEED_TZ: &str = "Asia/Kolkata";

fn feed_tz(gtfs_param: &str) -> String {
    format!("COALESCE((SELECT tz.timezone FROM gtfs_feed tz WHERE tz.gtfs_id = {gtfs_param}), '{DEFAULT_FEED_TZ}')")
}

fn feed_today(gtfs_param: &str) -> String {
    format!("(now() AT TIME ZONE {})::date", feed_tz(gtfs_param))
}

fn live_run_cte() -> String {
    let window = in_live_window("dg.operation_date", &feed_today("$1"));
    format!(
        r#"
            WITH run AS (
                SELECT dg.* FROM duty_groups dg
                WHERE dg.gtfs_id = $1 AND dg.vehicle_number = $2 AND dg.is_active AND NOT dg.deleted
                  AND {window}
                  AND EXISTS (SELECT 1 FROM duties d WHERE d.duty_group_id = dg.id AND NOT d.deleted
                              AND d.status IN ('upcoming', 'active'))
                ORDER BY EXISTS (SELECT 1 FROM duties d WHERE d.duty_group_id = dg.id
                                 AND d.status = 'active' AND NOT d.deleted) DESC,
                         dg.window_start_at
                LIMIT 1
            )
"#
    )
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct V2TripRow {
    run_id: String,
    waybill_no: String,
    schedule_no: String,
    vehicle_no: Option<String>,
    service_type: Option<String>,
    run_updated_at: Option<chrono::DateTime<chrono::Utc>>,
    duty_date: String,
    depot_name: Option<String>,
    entity_remark: Option<String>,
    driver_code: Option<String>,
    conductor_code: Option<String>,
    route_id: String,
    trip_number: i32,
    trip_order: i32,
    is_active_trip: bool,
    start_epoch: Option<String>,
    end_epoch: Option<String>,
    db_start_time: Option<String>,
    db_end_time: Option<String>,
}

fn scheduled(rows: Vec<VehicleData>) -> Vec<ScheduledTrip> {
    rows.into_iter()
        .map(|v| ScheduledTrip {
            waybill_no: v.waybill_no,
            service_type: v.service_type,
            vehicle_no: v.vehicle_no,
            duty_date: v.duty_date,
            status: v.status,
            db_start_time: v.db_start_time,
            start_time_epoch: v.start_time_epoch,
            trip_number: v.trip_number,
            is_active_trip: v.is_active_trip,
            is_completed: v.is_completed,
            duty_trip_id: v.duty_trip_id,
        })
        .collect()
}

impl V2TripRow {
    fn duty_trip_id(&self) -> String {
        format!("{}-{}", self.waybill_no, self.trip_number)
    }

    fn to_bus_schedule(&self) -> BusSchedule {
        BusSchedule {
            schedule_number: self.schedule_no.clone(),
            route_id: self.route_id.clone(),
            route_name: None,
            org_name: self.depot_name.clone(),
            trip_number: Some(self.trip_number),
            route_number: None,
            stops_count: None,
            is_active_trip: Some(self.is_active_trip),
            schedule_trip_id: Some(self.run_id.clone()),
            start_time: self.start_epoch.clone(),
            end_time: self.end_epoch.clone(),
            deleted: Some(false),
            trip_order: Some(self.trip_order),
            db_start_time: self.db_start_time.clone(),
            db_end_time: self.db_end_time.clone(),
            duty_trip_id: Some(self.duty_trip_id()),
        }
    }
}

impl DBVehicleReaderInternal {
    async fn v2_has_live_run(&self, vehicle_no: &str, gtfs_id: &str) -> bool {
        let Some(pool) = &self.pool else { return false };
        let window = in_live_window("operation_date", &feed_today("$1"));
        let result = sqlx::query_scalar::<_, bool>(&format!(
            "SELECT EXISTS(SELECT 1 FROM duty_groups
                           WHERE gtfs_id = $1 AND vehicle_number = $2 AND is_active AND NOT deleted
                             AND {window})"
        ))
        .bind(gtfs_id)
        .bind(vehicle_no)
        .fetch_one(pool)
        .await;
        or_empty(result, "v2_has_live_run")
    }

    async fn v2_live_trips(&self, vehicle_no: &str, gtfs_id: &str) -> Vec<V2TripRow> {
        let Some(pool) = &self.pool else {
            return Vec::new();
        };
        let cte = live_run_cte();
        let tz = feed_tz("$1");
        let vehicle_service = vehicle_service_join("run");
        let query = format!(
            r#"{cte}
            SELECT
                run.id AS run_id,
                run.waybill_no,
                tg.code AS schedule_no,
                run.vehicle_number AS vehicle_no,
                st.service_type_name AS service_type,
                run.updated_at AS run_updated_at,
                run.operation_date::text AS duty_date,
                COALESCE(e.entity_name, run.depot_id) AS depot_name,
                e.entity_remark::text AS entity_remark,
                COALESCE(d.driver_token_number, run.driver_token_number) AS driver_code,
                COALESCE(d.conductor_token_number, run.conductor_token_number) AS conductor_code,
                d.route_id,
                d.trip_number,
                d.trip_order,
                (d.status = 'active') AS is_active_trip,
                (EXTRACT(EPOCH FROM d.recorded_start_time) * 1000)::bigint::text AS start_epoch,
                (EXTRACT(EPOCH FROM d.recorded_end_time) * 1000)::bigint::text AS end_epoch,
                to_char(d.scheduled_start_at AT TIME ZONE {tz}, 'HH24:MI') AS db_start_time,
                to_char(d.scheduled_end_at AT TIME ZONE {tz}, 'HH24:MI') AS db_end_time
            FROM run
            JOIN duties d ON d.duty_group_id = run.id
            JOIN trip_groups tg ON tg.id = run.trip_group_id
            {vehicle_service}
            LEFT JOIN entities_internal e
                ON e.entity_id::text = run.depot_id AND e.gtfs_id = run.gtfs_id
            WHERE NOT d.deleted AND d.status IN ('upcoming', 'active')
            ORDER BY (d.status = 'active') DESC, d.scheduled_start_at, d.trip_order
        "#
        );

        let result = sqlx::query_as::<_, V2TripRow>(&query)
            .bind(gtfs_id)
            .bind(vehicle_no)
            .fetch_all(pool)
            .await;
        or_empty(result, "v2_live_trips")
    }

    async fn v2_vehicle_data(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
        trip_number: Option<i32>,
    ) -> Option<VehicleDataWithRouteId> {
        let mut rows = self.v2_live_trips(vehicle_no, gtfs_id).await;
        if let Some(n) = trip_number {
            rows.retain(|r| r.trip_number >= n);
        }
        rows.sort_by_key(|r| r.trip_order);
        let current_idx = rows.iter().position(|r| r.is_active_trip).unwrap_or(0);
        if current_idx >= rows.len() {
            return None;
        }
        let current = rows.remove(current_idx);
        rows.retain(|r| r.trip_order > current.trip_order);
        let mut schedules: Vec<BusSchedule> = std::iter::once(&current)
            .chain(rows.iter())
            .map(V2TripRow::to_bus_schedule)
            .collect();
        let _ = self.enrich_route_numbers(&mut schedules).await;
        let first = schedules.first().cloned();
        let mut schedule_map: HashMap<String, Vec<BusSchedule>> = HashMap::new();
        schedule_map.insert(current.run_id.clone(), schedules.clone());
        let remaining = if schedules.len() > 1 {
            Some(schedules.split_off(1))
        } else {
            None
        };

        Some(VehicleDataWithRouteId {
            waybill_id: Some(current.run_id.clone()),
            waybill_no: Some(current.waybill_no.clone()),
            service_type: current.service_type.clone(),
            vehicle_no: current
                .vehicle_no
                .clone()
                .unwrap_or_else(|| vehicle_no.to_string()),
            schedule_no: Some(current.schedule_no.clone()),
            last_updated: current.run_updated_at,
            duty_date: Some(current.duty_date.clone()),
            route_id: Some(current.route_id.clone()),
            route_number: first.as_ref().and_then(|s| s.route_number.clone()),
            depot: current.depot_name.clone(),
            trip_number: Some(current.trip_number),
            is_active_trip: current.is_active_trip,
            remaining_trip_details: remaining,
            entity_remark: current.entity_remark.clone(),
            driver_code: current.driver_code.clone(),
            conductor_code: current.conductor_code.clone(),
            deleted: Some(false),
            status: Some("online".to_string()),
            schedule_details: Some(schedule_map),
            db_start_time: current.db_start_time.clone(),
            db_end_time: current.db_end_time.clone(),
            seat_layout_id: None,
            waybill_status: Some(WaybillStatus::Online),
            duty_trip_id: Some(current.duty_trip_id()),
        })
    }

    async fn v2_route_rows(
        &self,
        route_id: &str,
        gtfs_id: &str,
        vehicle_number: Option<&str>,
        max_duty_date: Option<&str>,
    ) -> Vec<ScheduledTrip> {
        let Some(pool) = &self.pool else {
            return Vec::new();
        };
        let today = feed_today("$2");
        let select = v2_vehicle_data_select(&format!(
            "CASE WHEN dg.operation_date > {today} THEN 'upcoming' ELSE 'online' END"
        ));
        let query = format!(
            "{select}
             WHERE d.gtfs_id = $2 AND d.route_id = $1
               AND NOT d.deleted AND d.status IN ('upcoming', 'active')
               AND dg.is_active AND NOT dg.deleted
               AND dg.operation_date BETWEEN {today} - 1
                   AND GREATEST({today}, COALESCE($4::date, {today}))
               AND ($3::text IS NULL OR dg.vehicle_number = $3)
             ORDER BY dg.operation_date, dg.waybill_no, d.trip_number"
        );
        let result = sqlx::query_as::<_, VehicleData>(&query)
            .bind(route_id)
            .bind(gtfs_id)
            .bind(vehicle_number)
            .bind(max_duty_date)
            .fetch_all(pool)
            .await
            .map(scheduled);
        or_empty(result, "v2_route_rows")
    }

    async fn v2_waybill_trip_rows(
        &self,
        waybill_no: &str,
        trip_number: i32,
        gtfs_id: &str,
    ) -> Vec<ScheduledTrip> {
        let Some(pool) = &self.pool else {
            return Vec::new();
        };
        let query = format!(
            "{}
             WHERE dg.waybill_no = $1 AND d.trip_number = $2 AND dg.gtfs_id = $3
               AND NOT d.deleted AND NOT dg.deleted",
            v2_vehicle_data_select("'online'::text")
        );
        let result = sqlx::query_as::<_, VehicleData>(&query)
            .bind(waybill_no)
            .bind(trip_number)
            .bind(gtfs_id)
            .fetch_all(pool)
            .await
            .map(scheduled);
        or_empty(result, "v2_waybill_trip_rows")
    }

    async fn v2_vehicle_service_type(&self, vehicle_no: &str, gtfs_id: &str) -> Option<String> {
        let pool = self.pool.as_ref()?;
        let window = in_live_window("dg.operation_date", &feed_today("$1"));
        let vehicle_service = vehicle_service_join("dg");
        let result = sqlx::query_scalar::<_, String>(&format!(
            "SELECT st.service_type_name
             FROM duty_groups dg
             {vehicle_service}
             WHERE dg.gtfs_id = $1 AND dg.vehicle_number = $2 AND NOT dg.deleted
               AND st.service_type_name IS NOT NULL
             ORDER BY (dg.is_active AND {window}) DESC,
                      dg.operation_date DESC, dg.updated_at DESC
             LIMIT 1"
        ))
        .bind(gtfs_id)
        .bind(vehicle_no)
        .fetch_optional(pool)
        .await;
        or_empty(result, "v2_vehicle_service_type")
    }

    async fn v2_waybill_metadata(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: Option<i32>,
    ) -> Option<WaybillTripInfo> {
        let pool = self.pool.as_ref()?;
        let query = format!(
            r#"
            SELECT
                dg.waybill_no,
                COALESCE(dg.vehicle_number, '') AS vehicle_no,
                COALESCE(st.service_type_name, '') AS service_type,
                COALESCE(cur.driver_token_number, dg.driver_token_number) AS driver_token_no,
                dg.waybill_no || '-' || cur.trip_number AS duty_trip_id,
                emp.first_name AS driver_first_name,
                emp.last_name AS driver_last_name,
                emp.mobile_no AS driver_mobile_number
            FROM duty_groups dg
            {vehicle_service}
            LEFT JOIN LATERAL (
                SELECT d.trip_number, d.driver_token_number
                FROM duties d
                WHERE d.duty_group_id = dg.id AND NOT d.deleted
                  AND (($3::int IS NOT NULL AND d.trip_number = $3)
                       OR ($3::int IS NULL AND d.status IN ('upcoming', 'active')))
                ORDER BY (d.status = 'active') DESC, d.scheduled_start_at
                LIMIT 1
            ) cur ON true
            LEFT JOIN LATERAL (
                SELECT e.first_name, e.last_name, e.mobile_no
                FROM employees_internal e
                WHERE e.gtfs_id = dg.gtfs_id AND e.deleted = false
                  AND e.token_no = COALESCE(cur.driver_token_number, dg.driver_token_number)
                LIMIT 1
            ) emp ON true
            WHERE dg.waybill_no = $1 AND dg.gtfs_id = $2 AND NOT dg.deleted
            LIMIT 1
        "#,
            vehicle_service = vehicle_service_join("dg")
        );
        let result = sqlx::query_as::<_, WaybillTripInfo>(&query)
            .bind(waybill_no)
            .bind(gtfs_id)
            .bind(trip_number)
            .fetch_optional(pool)
            .await;
        or_empty(result, "v2_waybill_metadata")
    }
}

#[async_trait]
impl VehicleDataReaderInternal for DBVehicleReaderInternal {
    async fn get_routes_served_today(
        &self,
        gtfs_id: &str,
    ) -> AppResult<Vec<RouteLastScheduleTime>> {
        if let Some((data, ts)) = self.routes_served_today_cache.read().await.get(gtfs_id) {
            if ts.elapsed().unwrap_or_default()
                < Duration::from_secs(ROUTES_SERVED_TODAY_CACHE_DURATION)
            {
                return Ok(data.clone());
            }
        }

        let today_ist = {
            let ist = chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
            chrono::Utc::now()
                .with_timezone(&ist)
                .format("%Y-%m-%d")
                .to_string()
        };

        // Same shape as the external reader's query, scoped to one feed in the _internal tables.
        let query = r#"
            WITH today AS MATERIALIZED (
                SELECT waybill_id, schedule_trip_id, is_flexi
                FROM waybills_internal
                WHERE gtfs_id = $1 AND duty_date = $2 AND deleted = false
            )
            SELECT
                route_number_id::text AS route_id,
                MAX(end_time) AS last_schedule_time
            FROM (
                SELECT bstd.route_number_id, bstd.end_time
                FROM today t
                JOIN bus_schedule_trip_detail_internal bstd
                    ON bstd.schedule_trip_id = t.schedule_trip_id AND bstd.gtfs_id = $1
                WHERE NOT t.is_flexi AND bstd.trip_type <> 'dead-trip' AND bstd.deleted = false
                UNION ALL
                SELECT bstf.route_number_id, bstf.end_time
                FROM today t
                JOIN bus_schedule_trip_flexi_internal bstf
                    ON bstf.waybill_id = t.waybill_id AND bstf.gtfs_id = $1
                WHERE t.is_flexi AND bstf.trip_type <> 'dead-trip' AND bstf.deleted = false
            ) combined
            GROUP BY route_number_id
            ORDER BY route_number_id
        "#;

        let routes = sqlx::query_as::<_, RouteLastScheduleTime>(query)
            .bind(gtfs_id)
            .bind(&today_ist)
            .fetch_all(self.pool()?)
            .await
            .map_err(|e| {
                error!(
                    "Failed to query internal routes served today for {}: {}",
                    gtfs_id, e
                );
                AppError::Internal(format!("Database query failed: {}", e))
            })?;
        info!(
            "Found {} internal routes served today for {} ({})",
            routes.len(),
            gtfs_id,
            today_ist
        );

        self.routes_served_today_cache
            .write()
            .await
            .insert(gtfs_id.to_string(), (routes.clone(), SystemTime::now()));
        Ok(routes)
    }

    async fn warm_schedule_snapshots(&self) {
        futures::future::join_all(
            SUPPORTED_OPERATOR_GTFS_IDS
                .iter()
                .map(|gtfs_id| self.schedule_snapshot(gtfs_id)),
        )
        .await;
    }

    async fn is_vehicle_in_internal(&self, vehicle_no: &str, gtfs_id: &str) -> bool {
        if self.reader_config.reads_v1(gtfs_id)
            && self.is_vehicle_in_internal_impl(vehicle_no, gtfs_id).await
        {
            return true;
        }
        self.v2_has_live_run(vehicle_no, gtfs_id).await
    }

    async fn get_vehicle_data(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId> {
        let reads_v1 = self.reader_config.reads_v1(gtfs_id);
        if reads_v1
            && self
                .get_online_waybill(vehicle_no, gtfs_id)
                .await?
                .is_some()
        {
            return self
                .get_vehicle_data_impl(vehicle_no, gtfs_id, trip_number)
                .await;
        }
        if let Some(v2) = self.v2_vehicle_data(vehicle_no, gtfs_id, trip_number).await {
            return Ok(v2);
        }
        if reads_v1 {
            return self
                .get_vehicle_data_impl(vehicle_no, gtfs_id, trip_number)
                .await;
        }
        Ok(Self::empty_vehicle_data(vehicle_no))
    }

    async fn get_waybills_by_route_id(
        &self,
        route_id: &str,
        gtfs_id: &str,
        vehicle_number: Option<&str>,
        max_duty_date: Option<&str>,
    ) -> AppResult<Vec<ScheduledTrip>> {
        let mut rows = if self.reader_config.reads_v1(gtfs_id) {
            self.get_waybills_by_route_id_impl(route_id, gtfs_id, vehicle_number, max_duty_date)
                .await?
        } else {
            Vec::new()
        };
        let vehicle_number = vehicle_number.map(str::trim).filter(|v| !v.is_empty());
        rows.extend(
            self.v2_route_rows(route_id, gtfs_id, vehicle_number, max_duty_date)
                .await,
        );
        Ok(rows)
    }

    async fn get_waybill_by_waybill_and_trip(
        &self,
        waybill_no: &str,
        trip_number: i32,
        gtfs_id: &str,
    ) -> AppResult<Vec<ScheduledTrip>> {
        let v2 = self
            .v2_waybill_trip_rows(waybill_no, trip_number, gtfs_id)
            .await;
        if !v2.is_empty() {
            return Ok(v2);
        }
        if self.reader_config.reads_v1(gtfs_id) {
            return self
                .get_waybill_by_waybill_and_trip_impl(waybill_no, trip_number, gtfs_id)
                .await;
        }
        Ok(Vec::new())
    }

    async fn get_station_etas(&self, gtfs_id: &str) -> AppResult<HashMap<(String, String), i32>> {
        self.get_station_etas_impl(gtfs_id).await
    }

    async fn upsert_station_eta(
        &self,
        gtfs_id: &str,
        source_station_code: &str,
        destination_station_code: &str,
        eta_in_seconds: i32,
    ) -> AppResult<()> {
        self.upsert_station_eta_impl(
            gtfs_id,
            source_station_code,
            destination_station_code,
            eta_in_seconds,
        )
        .await
    }

    async fn get_vehicle_service_type(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<String>> {
        let reads_v1 = self.reader_config.reads_v1(gtfs_id);
        if reads_v1 {
            if let Some(w) = self.get_online_waybill(vehicle_no, gtfs_id).await? {
                return Ok(Some(w.service_type));
            }
        }
        if let Some(st) = self.v2_vehicle_service_type(vehicle_no, gtfs_id).await {
            return Ok(Some(st));
        }
        if reads_v1 {
            return self
                .get_vehicle_service_type_impl(vehicle_no, gtfs_id)
                .await;
        }
        Ok(None)
    }

    async fn get_waybill_metadata(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: Option<i32>,
    ) -> AppResult<WaybillMetadataResponse> {
        self.get_waybill_metadata_impl(gtfs_id, waybill_no, trip_number)
            .await
    }

    async fn get_depots(&self, gtfs_id: &str) -> AppResult<Vec<DepotInfo>> {
        self.get_depots_impl(gtfs_id).await
    }

    async fn get_depot_by_id(&self, depot_id: &str, gtfs_id: &str) -> AppResult<Option<DepotInfo>> {
        self.get_depot_by_id_impl(depot_id, gtfs_id).await
    }

    async fn get_depot_vehicles(
        &self,
        depot_id: &str,
        gtfs_id: &str,
    ) -> AppResult<Vec<DepotVehicleInfo>> {
        self.get_depot_vehicles_impl(depot_id, gtfs_id).await
    }

    async fn get_fleet_vehicles(&self, gtfs_id: &str) -> AppResult<Vec<DepotVehicleInfo>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };
        sqlx::query_as::<_, DepotVehicleInfo>(
            r#"
            SELECT v.fleet_no AS fleet_no,
                   v.vehicle_no AS vehicle_no,
                   v.status AS status,
                   st.service_type_name AS service_type
            FROM vehicles_internal v
            LEFT JOIN service_type_internal st
                   ON st.service_type_id = v.bus_service_type_id
                  AND st.gtfs_id = v.gtfs_id
                  AND st.deleted = false
            WHERE v.gtfs_id = $1
              AND v.deleted = false
              AND v.fleet_no <> ''
            "#,
        )
        .bind(gtfs_id)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_fleet_vehicles(gtfs_id={}): {}", gtfs_id, e)))
    }

    async fn get_fleet_operation(
        &self,
        fleet_no: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<FleetOperationInfo>> {
        self.get_fleet_operation_impl(fleet_no, gtfs_id).await
    }

    async fn clear_depot_cache(&self) -> AppResult<()> {
        self.clear_depot_cache_impl().await
    }

    async fn clear_caches_after_write(&self) {
        *self.depot_cache.write().await = InternalDepotCache::default();
    }

    async fn get_vehicle_ownership(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
    ) -> AppResult<VehicleOwnership> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(VehicleOwnership::default()),
        };
        let cte = live_run_cte();
        let query = format!(
            r#"{cte}
            SELECT COALESCE(run.depot_id, v.entity_id) AS depot_id,
                   COALESCE(run.organization_id, v.organization_id) AS organization_id
            FROM (SELECT 1) AS one
            LEFT JOIN run ON true
            LEFT JOIN LATERAL (
                SELECT entity_id, organization_id FROM vehicles_internal
                WHERE fleet_no = $2 AND gtfs_id = $1 AND deleted = false
                LIMIT 1
            ) v ON true
        "#
        );
        let row = sqlx::query_as::<_, (Option<String>, Option<String>)>(&query)
            .bind(gtfs_id)
            .bind(vehicle_no)
            .fetch_optional(pool)
            .await
            .map_err(|e| {
                AppError::DbError(format!(
                    "get_vehicle_ownership({}, {}): {}",
                    gtfs_id, vehicle_no, e
                ))
            })?;
        let (depot_id, organization_id) = row.unwrap_or((None, None));
        Ok(VehicleOwnership {
            depot_id,
            organization_id,
        })
    }

    async fn get_vehicle_properties(
        &self,
        fleet_no: &str,
        gtfs_id: &str,
    ) -> AppResult<VehicleProperties> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(VehicleProperties::default()),
        };
        // Uncached: tags and variant drive rider-app fare classes, and rider-app owns the cache (Redis, cleared on its
        // vehicle upsert), so a read here must reflect the row as it is now. One lookup on the (gtfs_id, fleet_no) index.
        let db_result = sqlx::query_as::<_, (Option<String>, Option<String>, Option<Vec<String>>)>(
            "SELECT tag_number, vehicle_variant, vehicle_tags FROM vehicles_internal WHERE fleet_no = $1 AND gtfs_id = $2 AND deleted = false LIMIT 1",
        )
        .bind(fleet_no)
        .bind(gtfs_id)
        .fetch_optional(pool)
        .await;

        fn non_blank(v: Option<String>) -> Option<String> {
            v.map(|x| x.trim().to_string()).filter(|x| !x.is_empty())
        }

        let props = match db_result {
            Ok(row) => {
                let (tag, variant, vehicle_tags) = row.unwrap_or((None, None, None));
                VehicleProperties {
                    tag_number: non_blank(tag),
                    variant: non_blank(variant),
                    vehicle_tags: vehicle_tags.filter(|tags| !tags.is_empty()),
                }
            }
            Err(e) => {
                warn!(
                    "get_vehicle_properties: DB failure for ({}, {}): {}; returning no properties",
                    gtfs_id, fleet_no, e
                );
                VehicleProperties::default()
            }
        };

        Ok(props)
    }
}

impl DBVehicleReaderInternal {
    async fn get_waybill_metadata_impl(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: Option<i32>,
    ) -> AppResult<WaybillMetadataResponse> {
        let pool = self.pool()?;
        if let Some(row) = self
            .v2_waybill_metadata(gtfs_id, waybill_no, trip_number)
            .await
        {
            return Ok(Self::waybill_metadata_from_row(row));
        }
        if !self.reader_config.reads_v1(gtfs_id) {
            return Err(AppError::NotFound(format!(
                "Waybill not found: {}",
                waybill_no
            )));
        }

        let waybill_query = r#"
            SELECT
                w.waybill_no::text,
                w.vehicle_no,
                w.service_type,
                w.driver_token_no::text AS driver_token_no,
                d.first_name AS driver_first_name,
                d.last_name AS driver_last_name,
                d.mobile_no AS driver_mobile_number
            FROM waybills_internal w
            LEFT JOIN employees_internal d
                   ON d.token_no = w.driver_token_no::text AND d.gtfs_id = $2 AND d.deleted = false
            WHERE w.waybill_no = $1
              AND w.gtfs_id = $2
              AND w.deleted = false
            ORDER BY w.updated_at DESC
            LIMIT 1
        "#;

        let waybill_row = sqlx::query_as::<_, WaybillTripInfo>(waybill_query)
            .bind(waybill_no)
            .bind(gtfs_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| AppError::DbError(format!("Waybill query failed: {}", e)))?
            .ok_or_else(|| AppError::NotFound(format!("Waybill not found: {}", waybill_no)))?;

        Ok(Self::waybill_metadata_from_row(waybill_row))
    }

    fn waybill_metadata_from_row(waybill_row: WaybillTripInfo) -> WaybillMetadataResponse {
        // Build driver name
        let driver_name = match (waybill_row.driver_first_name, waybill_row.driver_last_name) {
            (Some(f), Some(l)) => {
                let combined = format!("{} {}", f, l).trim().to_string();
                if combined.is_empty() {
                    None
                } else {
                    Some(combined)
                }
            }
            (Some(f), None) if !f.trim().is_empty() => Some(f),
            (None, Some(l)) if !l.trim().is_empty() => Some(l),
            _ => None,
        };
        let driver_mobile_number = waybill_row
            .driver_mobile_number
            .filter(|m| !m.trim().is_empty());

        WaybillMetadataResponse {
            waybill_no: waybill_row.waybill_no,
            vehicle_no: waybill_row.vehicle_no,
            service_type: waybill_row.service_type,
            driver_id: waybill_row.driver_token_no,
            driver_name,
            driver_mobile_number,
            bus_tag_number: None,
            bus_vehicle_variant: None,
            vehicle_tags: None,
            duty_trip_id: waybill_row.duty_trip_id,
        }
    }

    // ── Depots (entities_internal / vehicles_internal) ───────────────────
    // Failures surface rather than being swallowed: an outage must never reach the caller as
    // "no such depot". entity_remark is the short code — nullable, so entity_id stays the key.

    async fn get_depots_impl(&self, gtfs_id: &str) -> AppResult<Vec<DepotInfo>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        {
            let cache = self.depot_cache.read().await;
            if let Some((depots, timestamp)) = cache.depots.get(gtfs_id) {
                if !self.is_depot_cache_expired(*timestamp) {
                    debug!("internal depot cache HIT for depots gtfs_id={}", gtfs_id);
                    return Ok(depots.clone());
                }
            }
        }

        // No join to vehicles: a depot with no buses must still be listed.
        let query = r#"
            SELECT entity_id::text AS depot_id,
                   entity_remark AS depot_code,
                   entity_name AS depot_name
            FROM entities_internal
            WHERE gtfs_id = $1 AND deleted = false
            ORDER BY entity_name
        "#;

        match sqlx::query_as::<_, DepotInfo>(query)
            .bind(gtfs_id)
            .fetch_all(pool)
            .await
        {
            Ok(rows) => {
                info!(
                    "internal get_depots gtfs_id={} rows={}",
                    gtfs_id,
                    rows.len()
                );
                let mut cache = self.depot_cache.write().await;
                cache
                    .depots
                    .insert(gtfs_id.to_string(), (rows.clone(), SystemTime::now()));
                Ok(rows)
            }
            Err(e) => Err(AppError::DbError(format!(
                "get_depots(gtfs_id={}): {}",
                gtfs_id, e
            ))),
        }
    }

    async fn get_depot_by_id_impl(
        &self,
        depot_id: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<DepotInfo>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(None),
        };

        let cache_key = (gtfs_id.to_string(), depot_id.to_string());
        {
            let cache = self.depot_cache.read().await;
            if let Some((depot, timestamp)) = cache.depot_by_id.get(&cache_key) {
                if !self.is_depot_cache_expired(*timestamp) {
                    return Ok(Some(depot.clone()));
                }
            }
        }

        // `::text` cast so the predicate works whether entity_id is bigint or text.
        let query = r#"
            SELECT entity_id::text AS depot_id,
                   entity_remark AS depot_code,
                   entity_name AS depot_name
            FROM entities_internal
            WHERE entity_id::text = $1 AND gtfs_id = $2 AND deleted = false
            LIMIT 1
        "#;

        match sqlx::query_as::<_, DepotInfo>(query)
            .bind(depot_id)
            .bind(gtfs_id)
            .fetch_optional(pool)
            .await
        {
            Ok(row) => {
                // Only hits are cached: unknown ids are caller-supplied and would grow this
                // map without bound.
                if let Some(depot) = &row {
                    let mut cache = self.depot_cache.write().await;
                    cache
                        .depot_by_id
                        .insert(cache_key, (depot.clone(), SystemTime::now()));
                }
                Ok(row)
            }
            Err(e) => Err(AppError::DbError(format!(
                "get_depot_by_id(depot_id={}, gtfs_id={}): {}",
                depot_id, gtfs_id, e
            ))),
        }
    }

    async fn get_depot_vehicles_impl(
        &self,
        depot_id: &str,
        gtfs_id: &str,
    ) -> AppResult<Vec<DepotVehicleInfo>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let cache_key = (gtfs_id.to_string(), depot_id.to_string());
        {
            let cache = self.depot_cache.read().await;
            if let Some((vehicles, timestamp)) = cache.depot_vehicles.get(&cache_key) {
                if !self.is_depot_cache_expired(*timestamp) {
                    debug!(
                        "internal depot cache HIT for depot_id={} gtfs_id={}",
                        depot_id, gtfs_id
                    );
                    return Ok(vehicles.clone());
                }
            }
        }

        let query = r#"
            SELECT v.fleet_no AS fleet_no,
                   v.vehicle_no AS vehicle_no,
                   v.status AS status,
                   st.service_type_name AS service_type
            FROM vehicles_internal v
            LEFT JOIN service_type_internal st
                   ON st.service_type_id = v.bus_service_type_id
                  AND st.gtfs_id = v.gtfs_id
                  AND st.deleted = false
            WHERE v.entity_id::text = $1
              AND v.gtfs_id = $2
              AND v.deleted = false
              AND v.fleet_no <> ''
            ORDER BY v.fleet_no
            LIMIT 1048575
        "#;

        match sqlx::query_as::<_, DepotVehicleInfo>(query)
            .bind(depot_id)
            .bind(gtfs_id)
            .fetch_all(pool)
            .await
        {
            Ok(rows) => {
                info!(
                    "internal get_depot_vehicles gtfs_id={} depot_id={} rows={}",
                    gtfs_id,
                    depot_id,
                    rows.len()
                );
                let mut cache = self.depot_cache.write().await;
                cache
                    .depot_vehicles
                    .insert(cache_key, (rows.clone(), SystemTime::now()));
                Ok(rows)
            }
            Err(e) => Err(AppError::DbError(format!(
                "get_depot_vehicles(depot_id={}, gtfs_id={}): {}",
                depot_id, gtfs_id, e
            ))),
        }
    }

    async fn get_fleet_operation_impl(
        &self,
        fleet_no: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<FleetOperationInfo>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(None),
        };

        // Online waybill first, anchored on waybills so a bus with a duty but no vehicle row
        // still resolves; a flexi waybill takes its schedule number from the active trip.
        let online_query = r#"
            SELECT
                w.waybill_id::text AS waybill_id,
                w.waybill_no::text AS waybill_no,
                w.entity_id::text AS depot_id,
                NULLIF(w.conductor_token_no, '') AS conductor_code,
                NULLIF(w.driver_token_no, '') AS driver_code,
                CASE
                    WHEN w.is_flexi THEN COALESCE((
                        SELECT f.schedule_number
                        FROM bus_schedule_trip_flexi_internal f
                        WHERE f.waybill_id = w.waybill_id
                          AND f.gtfs_id = w.gtfs_id
                          AND f.deleted = false
                        ORDER BY f.is_active_trip DESC NULLS LAST, f.updated_at DESC NULLS LAST
                        LIMIT 1
                    ), w.schedule_no)
                    ELSE w.schedule_no
                END AS schedule_no,
                e.entity_name AS depot_name,
                w.service_type AS service_type
            FROM waybills_internal w
            LEFT JOIN entities_internal e
                   ON e.entity_id = w.entity_id
                  AND e.gtfs_id = w.gtfs_id
                  AND e.deleted = false
            WHERE w.vehicle_no = $1
              AND w.gtfs_id = $2
              AND w.status = 'online'
              AND w.deleted = false
            ORDER BY w.updated_at DESC NULLS LAST, w.waybill_id DESC
            LIMIT 1
        "#;

        match sqlx::query_as::<_, FleetOperationInfo>(online_query)
            .bind(fleet_no)
            .bind(gtfs_id)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(row)) => return Ok(Some(row)),
            Ok(None) => {}
            Err(e) => {
                return Err(AppError::DbError(format!(
                    "get_fleet_operation(online, fleet_no={}, gtfs_id={}): {}",
                    fleet_no, gtfs_id, e
                )))
            }
        }

        // No open waybill: the vehicle's own depot and configured service type.
        let vehicle_query = r#"
            SELECT
                NULL::text AS waybill_id,
                NULL::text AS waybill_no,
                v.entity_id::text AS depot_id,
                NULL::text AS conductor_code,
                NULL::text AS driver_code,
                NULL::text AS schedule_no,
                e.entity_name AS depot_name,
                st.service_type_name AS service_type
            FROM vehicles_internal v
            LEFT JOIN entities_internal e
                   ON e.entity_id = v.entity_id
                  AND e.gtfs_id = v.gtfs_id
                  AND e.deleted = false
            LEFT JOIN service_type_internal st
                   ON st.service_type_id = v.bus_service_type_id
                  AND st.gtfs_id = v.gtfs_id
                  AND st.deleted = false
            WHERE v.fleet_no = $1
              AND v.gtfs_id = $2
              AND v.deleted = false
            LIMIT 1
        "#;

        sqlx::query_as::<_, FleetOperationInfo>(vehicle_query)
            .bind(fleet_no)
            .bind(gtfs_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| {
                AppError::DbError(format!(
                    "get_fleet_operation(vehicle, fleet_no={}, gtfs_id={}): {}",
                    fleet_no, gtfs_id, e
                ))
            })
    }

    async fn clear_depot_cache_impl(&self) -> AppResult<()> {
        let mut cache = self.depot_cache.write().await;
        *cache = InternalDepotCache::default();
        info!("Internal depot cache cleared successfully");
        Ok(())
    }
}
