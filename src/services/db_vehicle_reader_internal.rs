use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

use sqlx::PgPool;
use tracing::{debug, error, info, warn};

use crate::models::{
    BusSchedule, DepotInfo, DepotVehicleInfo, FleetOperationInfo, VehicleData,
    VehicleDataWithRouteId, VehicleProperties, WaybillMetadataResponse, WaybillStatus,
    WaybillTripInfo,
};
use crate::services::operator::SUPPORTED_OPERATOR_GTFS_IDS;
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
    ) -> AppResult<WaybillMetadataResponse>;

    async fn get_vehicle_properties(
        &self,
        fleet_no: &str,
        gtfs_id: &str,
    ) -> AppResult<VehicleProperties>;

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
    vehicle_properties_cache:
        Arc<RwLock<HashMap<(String, String), (VehicleProperties, SystemTime)>>>,
    depot_cache: Arc<RwLock<InternalDepotCache>>,
}

const STATION_ETA_CACHE_DURATION: u64 = 1800; // 30 mins
                                              // 10 minutes — vehicle_tags drive fare classes, so an operator edit (tag/variant) must reach
                                              // pricing quickly. Not invalidated on write: the cache is in-process, so every pod serves its
                                              // own entry until it expires. A miss is one lookup on the (gtfs_id, fleet_no) partial unique index.
const VEHICLE_PROPS_CACHE_DURATION: u64 = 600;
const VEHICLE_PROPS_NEGATIVE_CACHE_DURATION: u64 = 300; // 5m — CSV is the current source, so misses are hot
const VEHICLE_PROPS_CACHE_MAX_ENTRIES: usize = 4000;
// 5 minutes. This service's own writes drop the cache (see clear_caches_after_write); the TTL
// only bounds staleness from writes made by another pod.
const DEPOT_CACHE_DURATION: u64 = 300;

impl DBVehicleReaderInternal {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool: Some(pool),
            schedule_snapshot: ScheduleSnapshotCache::new(SCHEDULE_SNAPSHOT_TTL),
            station_eta_cache: Arc::new(RwLock::new(HashMap::new())),
            vehicle_properties_cache: Arc::new(RwLock::new(HashMap::new())),
            depot_cache: Arc::new(RwLock::new(InternalDepotCache::default())),
        }
    }

    /// Used when no DB is available (local mock / no DATABASE_URL).
    /// is_vehicle_in_internal will always return false, so get_vehicle_data is never reached.
    pub fn new_disconnected() -> Self {
        Self {
            pool: None,
            schedule_snapshot: ScheduleSnapshotCache::new(SCHEDULE_SNAPSHOT_TTL),
            station_eta_cache: Arc::new(RwLock::new(HashMap::new())),
            vehicle_properties_cache: Arc::new(RwLock::new(HashMap::new())),
            depot_cache: Arc::new(RwLock::new(InternalDepotCache::default())),
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
                        waybill_status: None,
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

        let placeholders: Vec<String> = (1..=route_ids.len()).map(|i| format!("${}", i)).collect();
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

#[async_trait]
impl VehicleDataReaderInternal for DBVehicleReaderInternal {
    async fn warm_schedule_snapshots(&self) {
        futures::future::join_all(
            SUPPORTED_OPERATOR_GTFS_IDS
                .iter()
                .map(|gtfs_id| self.schedule_snapshot(gtfs_id)),
        )
        .await;
    }

    async fn is_vehicle_in_internal(&self, vehicle_no: &str, gtfs_id: &str) -> bool {
        self.is_vehicle_in_internal_impl(vehicle_no, gtfs_id).await
    }

    async fn get_vehicle_data(
        &self,
        vehicle_no: &str,
        gtfs_id: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId> {
        self.get_vehicle_data_impl(vehicle_no, gtfs_id, trip_number)
            .await
    }

    async fn get_waybills_by_route_id(
        &self,
        route_id: &str,
        gtfs_id: &str,
        vehicle_number: Option<&str>,
        max_duty_date: Option<&str>,
    ) -> AppResult<Vec<ScheduledTrip>> {
        self.get_waybills_by_route_id_impl(route_id, gtfs_id, vehicle_number, max_duty_date)
            .await
    }

    async fn get_waybill_by_waybill_and_trip(
        &self,
        waybill_no: &str,
        trip_number: i32,
        gtfs_id: &str,
    ) -> AppResult<Vec<ScheduledTrip>> {
        self.get_waybill_by_waybill_and_trip_impl(waybill_no, trip_number, gtfs_id)
            .await
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
        self.get_vehicle_service_type_impl(vehicle_no, gtfs_id)
            .await
    }

    async fn get_waybill_metadata(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
    ) -> AppResult<WaybillMetadataResponse> {
        self.get_waybill_metadata_impl(gtfs_id, waybill_no).await
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
        // Only the depot caches: vehicle_properties_cache is bounded by its 10-minute TTL, and
        // dropping it pod-locally would not make it consistent across pods anyway.
        *self.depot_cache.write().await = InternalDepotCache::default();
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

        let cache_key = (fleet_no.to_string(), gtfs_id.to_string());
        {
            let cache = self.vehicle_properties_cache.read().await;
            if let Some((props, fetched_at)) = cache.get(&cache_key) {
                let ttl = if props.tag_number.is_some()
                    || props.variant.is_some()
                    || props.vehicle_tags.is_some()
                {
                    VEHICLE_PROPS_CACHE_DURATION
                } else {
                    VEHICLE_PROPS_NEGATIVE_CACHE_DURATION
                };
                if fetched_at.elapsed().unwrap_or_default() < Duration::from_secs(ttl) {
                    return Ok(props.clone());
                }
            }
        }

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
                    "get_vehicle_properties: DB failure for ({}, {}): {}; caching negative for {}s",
                    gtfs_id, fleet_no, e, VEHICLE_PROPS_NEGATIVE_CACHE_DURATION
                );
                VehicleProperties::default()
            }
        };

        {
            let mut cache = self.vehicle_properties_cache.write().await;
            if cache.len() >= VEHICLE_PROPS_CACHE_MAX_ENTRIES {
                cache.retain(|_, (p, stamp)| {
                    let ttl = if p.tag_number.is_some()
                        || p.variant.is_some()
                        || p.vehicle_tags.is_some()
                    {
                        VEHICLE_PROPS_CACHE_DURATION
                    } else {
                        VEHICLE_PROPS_NEGATIVE_CACHE_DURATION
                    };
                    stamp.elapsed().unwrap_or_default() < Duration::from_secs(ttl)
                });
                while cache.len() >= VEHICLE_PROPS_CACHE_MAX_ENTRIES {
                    let oldest = cache
                        .iter()
                        .min_by_key(|(_, (_, stamp))| *stamp)
                        .map(|(key, _)| key.clone());
                    match oldest {
                        Some(key) => {
                            cache.remove(&key);
                        }
                        None => break,
                    }
                }
            }
            cache.insert(cache_key, (props.clone(), SystemTime::now()));
        }

        Ok(props)
    }
}

impl DBVehicleReaderInternal {
    async fn get_waybill_metadata_impl(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
    ) -> AppResult<WaybillMetadataResponse> {
        let pool = self.pool()?;

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

        let response = WaybillMetadataResponse {
            waybill_no: waybill_row.waybill_no,
            vehicle_no: waybill_row.vehicle_no,
            service_type: waybill_row.service_type,
            driver_id: waybill_row.driver_token_no,
            driver_name,
            driver_mobile_number,
            bus_tag_number: None,
            bus_vehicle_variant: None,
            vehicle_tags: None,
        };

        Ok(response)
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
