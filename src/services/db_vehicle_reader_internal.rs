use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

use sqlx::PgPool;
use tracing::{debug, error, info, warn};

use crate::models::{
    ActiveTripEtaOverride, BusSchedule, EtaVariant, StationEtaEntry, StationEtaMap, VehicleData,
    VehicleDataWithRouteId, WaybillMetadataResponse, WaybillStatus, WaybillTripInfo,
};
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
    async fn get_waybills_by_route_id(
        &self,
        route_id: &str,
        gtfs_id: &str,
        vehicle_number: Option<&str>,
    ) -> AppResult<Vec<VehicleData>>;
    async fn get_waybill_by_waybill_and_trip(
        &self,
        waybill_no: &str,
        trip_number: i32,
        gtfs_id: &str,
    ) -> AppResult<Vec<VehicleData>>;

    async fn get_station_etas(&self, gtfs_id: &str) -> AppResult<StationEtaMap>;

    async fn upsert_station_eta(
        &self,
        gtfs_id: &str,
        source_station_code: &str,
        destination_station_code: &str,
        eta_in_seconds: i32,
    ) -> AppResult<()>;

    /// Bulk form of [`Self::upsert_station_eta`]. One statement, one transaction — a UI
    /// saving a route's worth of pairs cannot be a request per pair.
    async fn upsert_station_etas(
        &self,
        gtfs_id: &str,
        variant_id: Option<&str>,
        entries: &[StationEtaEntry],
    ) -> AppResult<u64>;

    /// Variant catalogue for a feed, newest-safe: soft-deleted rows are never returned.
    async fn get_eta_variants(&self, gtfs_id: &str) -> AppResult<Vec<EtaVariant>>;

    async fn upsert_eta_variant(&self, variant: &EtaVariant) -> AppResult<EtaVariant>;

    /// Soft delete. A variant still referenced by an override or a schedule default reads as
    /// absent afterwards and resolution falls through to the feed default.
    async fn delete_eta_variant(&self, gtfs_id: &str, variant_id: &str) -> AppResult<u64>;

    async fn set_trip_eta_override(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: i32,
        variant_id: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
        updated_by: Option<&str>,
    ) -> AppResult<()>;

    async fn clear_trip_eta_override(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: i32,
    ) -> AppResult<u64>;

    /// Every override still in force for a feed. Small by construction, and the only thing a
    /// caching consumer needs in order to know which trips it must not cache.
    async fn list_active_trip_eta_overrides(
        &self,
        gtfs_id: &str,
    ) -> AppResult<Vec<ActiveTripEtaOverride>>;

    async fn get_waybill_metadata(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
    ) -> AppResult<WaybillMetadataResponse>;

    async fn get_vehicle_tag_number(
        &self,
        fleet_no: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<String>>;
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
    ) -> AppResult<Vec<VehicleData>> {
        Ok(Vec::new())
    }

    async fn get_waybill_by_waybill_and_trip(
        &self,
        _waybill_no: &str,
        _trip_number: i32,
        _gtfs_id: &str,
    ) -> AppResult<Vec<VehicleData>> {
        Ok(Vec::new())
    }

    async fn get_station_etas(&self, _gtfs_id: &str) -> AppResult<StationEtaMap> {
        Ok(StationEtaMap::new())
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

    async fn upsert_station_etas(
        &self,
        _gtfs_id: &str,
        _variant_id: Option<&str>,
        _entries: &[StationEtaEntry],
    ) -> AppResult<u64> {
        Ok(0)
    }

    async fn get_eta_variants(&self, _gtfs_id: &str) -> AppResult<Vec<EtaVariant>> {
        Ok(Vec::new())
    }

    async fn upsert_eta_variant(&self, _variant: &EtaVariant) -> AppResult<EtaVariant> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn delete_eta_variant(&self, _gtfs_id: &str, _variant_id: &str) -> AppResult<u64> {
        Ok(0)
    }

    async fn set_trip_eta_override(
        &self,
        _gtfs_id: &str,
        _waybill_no: &str,
        _trip_number: i32,
        _variant_id: &str,
        _expires_at: chrono::DateTime<chrono::Utc>,
        _updated_by: Option<&str>,
    ) -> AppResult<()> {
        Ok(())
    }

    async fn clear_trip_eta_override(
        &self,
        _gtfs_id: &str,
        _waybill_no: &str,
        _trip_number: i32,
    ) -> AppResult<u64> {
        Ok(0)
    }

    async fn list_active_trip_eta_overrides(
        &self,
        _gtfs_id: &str,
    ) -> AppResult<Vec<ActiveTripEtaOverride>> {
        Ok(Vec::new())
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

    async fn get_vehicle_tag_number(
        &self,
        _fleet_no: &str,
        _gtfs_id: &str,
    ) -> AppResult<Option<String>> {
        Ok(None)
    }
}

#[allow(clippy::type_complexity)]
pub struct DBVehicleReaderInternal {
    pool: Option<PgPool>,
    waybills_by_route_cache: Arc<RwLock<HashMap<String, (Vec<VehicleData>, SystemTime)>>>,
    station_eta_cache: Arc<RwLock<HashMap<String, (StationEtaMap, SystemTime)>>>,
    eta_variant_cache: Arc<RwLock<HashMap<String, (Vec<EtaVariant>, SystemTime)>>>,
    tag_number_cache: Arc<RwLock<HashMap<(String, String), (Option<String>, SystemTime)>>>,
}

const WAYBILL_ROUTE_CACHE_DURATION: u64 = 30; // short TTL so bus swaps are reflected quickly
const STATION_ETA_CACHE_DURATION: u64 = 1800; // 30 mins
const ETA_VARIANT_CACHE_DURATION: u64 = 1800; // catalogue changes as rarely as the times it labels
const TAG_NUMBER_CACHE_DURATION: u64 = 43200; // 12 hours — tag_number rarely changes
const TAG_NUMBER_NEGATIVE_CACHE_DURATION: u64 = 300; // 5m — CSV is the current source, so misses are hot
const TAG_NUMBER_CACHE_MAX_ENTRIES: usize = 4000;

impl DBVehicleReaderInternal {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool: Some(pool),
            waybills_by_route_cache: Arc::new(RwLock::new(HashMap::new())),
            station_eta_cache: Arc::new(RwLock::new(HashMap::new())),
            eta_variant_cache: Arc::new(RwLock::new(HashMap::new())),
            tag_number_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Used when no DB is available (local mock / no DATABASE_URL).
    /// is_vehicle_in_internal will always return false, so get_vehicle_data is never reached.
    pub fn new_disconnected() -> Self {
        Self {
            pool: None,
            waybills_by_route_cache: Arc::new(RwLock::new(HashMap::new())),
            station_eta_cache: Arc::new(RwLock::new(HashMap::new())),
            eta_variant_cache: Arc::new(RwLock::new(HashMap::new())),
            tag_number_cache: Arc::new(RwLock::new(HashMap::new())),
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

    fn is_station_eta_cache_expired(&self, timestamp: SystemTime) -> bool {
        let elapsed = timestamp.elapsed().unwrap_or_default();
        elapsed >= Duration::from_secs(STATION_ETA_CACHE_DURATION)
    }

    fn get_waybills_by_route_cache_key(&self, gtfs_id: &str, route_id: &str) -> String {
        format!("{}_{}", gtfs_id, route_id)
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

    /// Segment times for a feed, grouped by variant. Reference data, so the whole feed is
    /// loaded once and held: which variant a given trip uses is decided per request against
    /// this map, never by re-querying.
    pub async fn get_station_etas_impl(&self, gtfs_id: &str) -> AppResult<StationEtaMap> {
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
            None => return Ok(StationEtaMap::new()),
        };

        let query = r#"
            SELECT s.variant_id, s.source_station_code, s.destination_station_code, s.eta_in_seconds
            FROM station_eta s
            JOIN eta_variant v ON v.variant_id = s.variant_id AND NOT v.deleted
            WHERE s.gtfs_id = $1
        "#;

        let rows = sqlx::query(query)
            .bind(gtfs_id)
            .fetch_all(pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

        let mut eta_map = StationEtaMap::new();
        for row in rows {
            use sqlx::Row;
            // try_get so a schema skew (code live before its migration) degrades this feed to
            // haversine instead of panicking the worker mid-request.
            let variant: String = row
                .try_get("variant_id")
                .map_err(|e| AppError::DbError(e.to_string()))?;
            let src: String = row
                .try_get("source_station_code")
                .map_err(|e| AppError::DbError(e.to_string()))?;
            let dst: String = row
                .try_get("destination_station_code")
                .map_err(|e| AppError::DbError(e.to_string()))?;
            let secs: i32 = row
                .try_get("eta_in_seconds")
                .map_err(|e| AppError::DbError(e.to_string()))?;
            eta_map.entry(variant).or_default().insert((src, dst), secs);
        }

        // Update cache
        {
            let mut cache = self.station_eta_cache.write().await;
            cache.insert(cache_key, (eta_map.clone(), SystemTime::now()));
        }

        Ok(eta_map)
    }

    fn is_eta_variant_cache_expired(&self, timestamp: SystemTime) -> bool {
        let elapsed = timestamp.elapsed().unwrap_or_default();
        elapsed >= Duration::from_secs(ETA_VARIANT_CACHE_DURATION)
    }

    async fn get_eta_variants_impl(&self, gtfs_id: &str) -> AppResult<Vec<EtaVariant>> {
        {
            let cache = self.eta_variant_cache.read().await;
            if let Some((variants, ts)) = cache.get(gtfs_id) {
                if !self.is_eta_variant_cache_expired(*ts) {
                    return Ok(variants.clone());
                }
            }
        }

        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let variants = sqlx::query_as::<_, EtaVariant>(
            r#"
            SELECT variant_id, gtfs_id, code, display_name, is_default,
                   band_start_time, band_end_time
            FROM eta_variant
            WHERE gtfs_id = $1 AND deleted = false
            ORDER BY is_default DESC, code
            "#,
        )
        .bind(gtfs_id)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        {
            let mut cache = self.eta_variant_cache.write().await;
            cache.insert(gtfs_id.to_string(), (variants.clone(), SystemTime::now()));
        }

        Ok(variants)
    }

    async fn invalidate_eta_variant_cache(&self, gtfs_id: &str) {
        self.eta_variant_cache.write().await.remove(gtfs_id);
    }

    async fn invalidate_station_eta_cache(&self, gtfs_id: &str) {
        let key = self.get_station_eta_cache_key(gtfs_id);
        self.station_eta_cache.write().await.remove(&key);
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

    async fn get_waybills_by_route_id_impl(
        &self,
        route_id: &str,
        gtfs_id: &str,
        vehicle_number: Option<&str>,
    ) -> AppResult<Vec<VehicleData>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => {
                return Ok(Vec::new()); // No pool available (mock mode)
            }
        };

        // Normalize: trim and convert empty string to None
        let vehicle_number = vehicle_number.map(|v| v.trim()).filter(|v| !v.is_empty());

        let is_filtered = vehicle_number.is_some();

        // Only cache unfiltered results to prevent unbounded cache growth
        let cache_key =
            (!is_filtered).then(|| self.get_waybills_by_route_cache_key(gtfs_id, route_id));

        // Check cache first (only for unfiltered queries)
        if let Some(ref key) = cache_key {
            let cache = self.waybills_by_route_cache.read().await;
            if let Some((data, ts)) = cache.get(key) {
                if !self.is_waybills_by_route_cache_expired(*ts) {
                    info!(
                        "internal get_waybills_by_route_id cache HIT for route_id={}",
                        route_id
                    );
                    return Ok(data.clone());
                }
            }
        }

        // Use separate queries for better index utilization
        let (query, bound_vehicle): (&str, Option<&str>) = if let Some(vn) = vehicle_number {
            (
                r#"
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
                        END AS trip_number,
                        CASE
                            WHEN w.status <> 'online' THEN false
                            WHEN w.is_flexi THEN bstf.is_active_trip
                            ELSE bstd.is_active_trip
                        END AS is_active_trip,
                        CASE
                            WHEN w.is_flexi THEN NULL
                            ELSE bstd.is_completed
                        END AS is_completed,
                        COALESCE(
                            o.variant_id,
                            CASE WHEN w.is_flexi THEN NULL ELSE bstd.default_variant_id END
                        ) AS effective_variant_id,
                        o.expires_at AS override_expires_at
                    FROM waybills_internal w
                    LEFT JOIN entities_internal e
                        ON e.entity_id = w.entity_id
                        AND e.gtfs_id = $2
                    LEFT JOIN bus_schedule_trip_flexi_internal bstf
                        ON w.is_flexi = true
                        AND bstf.waybill_id = w.waybill_id
                        AND bstf.route_number_id::text = $1
                        AND bstf.gtfs_id = $2
                        AND bstf.trip_type <> 'dead-trip'
                        AND bstf.deleted = false
                    LEFT JOIN bus_schedule_trip_detail_internal bstd
                        ON w.is_flexi = false
                        AND bstd.schedule_trip_id = w.schedule_trip_id
                        AND bstd.route_number_id::text = $1
                        AND bstd.gtfs_id = $2
                        AND bstd.trip_type <> 'dead-trip'
                        AND bstd.deleted = false
                        AND (w.status <> 'online' OR LOWER(COALESCE(bstd.status, 'active')) <> 'inactive')
                    LEFT JOIN trip_eta_override o
                        ON o.gtfs_id = w.gtfs_id
                        AND o.waybill_id = w.waybill_id
                        AND o.trip_number = (CASE WHEN w.is_flexi THEN bstf.trip_number ELSE bstd.trip_number END)
                        AND o.expires_at > now()
                        AND EXISTS (SELECT 1 FROM eta_variant v WHERE v.variant_id = o.variant_id AND NOT v.deleted)
                    WHERE
                        w.status in ('online', 'upcoming')
                        AND w.deleted = false
                        AND w.gtfs_id = $2
                        AND w.vehicle_no = $3
                        AND (
                            (w.is_flexi = true AND bstf.waybill_id IS NOT NULL)
                            OR
                            (w.is_flexi = false AND bstd.schedule_trip_id IS NOT NULL)
                        )
                )
                SELECT * FROM base WHERE (status <> 'online' OR is_completed IS NOT TRUE) ORDER BY waybill_no, trip_number;
                "#,
                Some(vn),
            )
        } else {
            (
                r#"
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
                        END AS trip_number,
                        CASE
                            WHEN w.status <> 'online' THEN false
                            WHEN w.is_flexi THEN bstf.is_active_trip
                            ELSE bstd.is_active_trip
                        END AS is_active_trip,
                        CASE
                            WHEN w.is_flexi THEN NULL
                            ELSE bstd.is_completed
                        END AS is_completed,
                        COALESCE(
                            o.variant_id,
                            CASE WHEN w.is_flexi THEN NULL ELSE bstd.default_variant_id END
                        ) AS effective_variant_id,
                        o.expires_at AS override_expires_at
                    FROM waybills_internal w
                    LEFT JOIN entities_internal e
                        ON e.entity_id = w.entity_id
                        AND e.gtfs_id = $2
                    LEFT JOIN bus_schedule_trip_flexi_internal bstf
                        ON w.is_flexi = true
                        AND bstf.waybill_id = w.waybill_id
                        AND bstf.route_number_id::text = $1
                        AND bstf.gtfs_id = $2
                        AND bstf.trip_type <> 'dead-trip'
                        AND bstf.deleted = false
                    LEFT JOIN bus_schedule_trip_detail_internal bstd
                        ON w.is_flexi = false
                        AND bstd.schedule_trip_id = w.schedule_trip_id
                        AND bstd.route_number_id::text = $1
                        AND bstd.gtfs_id = $2
                        AND bstd.trip_type <> 'dead-trip'
                        AND bstd.deleted = false
                        AND (w.status <> 'online' OR LOWER(COALESCE(bstd.status, 'active')) <> 'inactive')
                    LEFT JOIN trip_eta_override o
                        ON o.gtfs_id = w.gtfs_id
                        AND o.waybill_id = w.waybill_id
                        AND o.trip_number = (CASE WHEN w.is_flexi THEN bstf.trip_number ELSE bstd.trip_number END)
                        AND o.expires_at > now()
                        AND EXISTS (SELECT 1 FROM eta_variant v WHERE v.variant_id = o.variant_id AND NOT v.deleted)
                    WHERE
                        w.status in ('online', 'upcoming')
                        AND w.deleted = false
                        AND w.gtfs_id = $2
                        AND (
                            (w.is_flexi = true AND bstf.waybill_id IS NOT NULL)
                            OR
                            (w.is_flexi = false AND bstd.schedule_trip_id IS NOT NULL)
                        )
                )
                SELECT * FROM base WHERE (status <> 'online' OR is_completed IS NOT TRUE) ORDER BY waybill_no, trip_number;
                "#,
                None::<&str>,
            )
        };

        let query_builder = sqlx::query_as::<_, VehicleData>(query)
            .bind(route_id)
            .bind(gtfs_id);
        let query_builder = if let Some(vn) = bound_vehicle {
            query_builder.bind(vn)
        } else {
            query_builder
        };

        match query_builder.fetch_all(pool).await {
            Ok(rows) => {
                info!(
                    "Chennai internal direct query: found {} waybills for route_id={}",
                    rows.len(),
                    route_id
                );

                // Update cache only for unfiltered queries
                if let Some(key) = cache_key {
                    let mut cache = self.waybills_by_route_cache.write().await;
                    cache.insert(key, (rows.clone(), SystemTime::now()));
                }

                Ok(rows)
            }
            Err(e) => {
                error!(
                    "Internal get_waybills_by_route_id failed for route_id={}: {}",
                    route_id, e
                );
                // Fail gracefully so we just don't append internal records
                Ok(Vec::new())
            }
        }
    }

    async fn get_waybill_by_waybill_and_trip_impl(
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
                END AS trip_number,
                CASE
                    WHEN w.status <> 'online' THEN false
                    WHEN w.is_flexi THEN bstf.is_active_trip
                    ELSE bstd.is_active_trip
                END AS is_active_trip,
                CASE
                    WHEN w.is_flexi THEN NULL
                    ELSE bstd.is_completed
                END AS is_completed,
                COALESCE(
                    o.variant_id,
                    CASE WHEN w.is_flexi THEN NULL ELSE bstd.default_variant_id END
                ) AS effective_variant_id,
                o.expires_at AS override_expires_at
            FROM waybills_internal w
            LEFT JOIN entities_internal e
                ON e.entity_id = w.entity_id
                AND e.gtfs_id = $3
            LEFT JOIN bus_schedule_trip_flexi_internal bstf
                ON w.is_flexi = true
                AND bstf.waybill_id = w.waybill_id
                AND bstf.trip_number = $2
                AND bstf.gtfs_id = $3
                AND bstf.trip_type <> 'dead-trip'
                AND bstf.deleted = false
            LEFT JOIN bus_schedule_trip_detail_internal bstd
                ON w.is_flexi = false
                AND bstd.schedule_trip_id = w.schedule_trip_id
                AND bstd.trip_number = $2
                AND bstd.gtfs_id = $3
                AND bstd.trip_type <> 'dead-trip'
                AND bstd.deleted = false
                AND (w.status <> 'online' OR LOWER(COALESCE(bstd.status, 'active')) <> 'inactive')
            LEFT JOIN trip_eta_override o
                ON o.gtfs_id = w.gtfs_id
                AND o.waybill_id = w.waybill_id
                AND o.trip_number = $2
                AND o.expires_at > now()
                AND EXISTS (SELECT 1 FROM eta_variant v WHERE v.variant_id = o.variant_id AND NOT v.deleted)
            WHERE
                w.waybill_no::text = $1
                AND w.gtfs_id = $3
                AND w.status in ('online', 'upcoming')
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
                    "Internal get_waybill_by_waybill_and_trip failed for waybill_no={}, trip_number={}, gtfs_id={}: {}",
                    waybill_no, trip_number, gtfs_id, e
                );
                // Fail gracefully so external results are still returned
                Ok(Vec::new())
            }
        }
    }

    /// Resolve the variant a write targets: the caller's if given, else the feed's default.
    async fn resolve_write_variant(
        &self,
        gtfs_id: &str,
        variant_id: Option<&str>,
    ) -> AppResult<String> {
        match variant_id {
            Some(v) => {
                // Deliberately not get_eta_variants_impl: its 30-min cache is per-process, so
                // one replica would reject a variant another just created (and accept one it
                // just deleted). Ops writes are rare enough to always ask the database.
                let known: bool = sqlx::query_scalar(
                    r#"
                    SELECT EXISTS (
                        SELECT 1 FROM eta_variant
                        WHERE gtfs_id = $1 AND variant_id = $2 AND NOT deleted
                    )
                    "#,
                )
                .bind(gtfs_id)
                .bind(v)
                .fetch_one(self.pool()?)
                .await
                .map_err(|e| AppError::DbError(e.to_string()))?;
                if known {
                    Ok(v.to_string())
                } else {
                    Err(AppError::BadRequest(format!(
                        "Unknown or deleted eta variant '{}' for gtfs_id '{}'",
                        v, gtfs_id
                    )))
                }
            }
            None => sqlx::query_scalar::<_, String>(
                r#"
                SELECT variant_id FROM eta_variant
                WHERE gtfs_id = $1 AND is_default AND NOT deleted
                "#,
            )
            .bind(gtfs_id)
            .fetch_optional(self.pool()?)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "No default eta variant configured for '{}'",
                    gtfs_id
                ))
            }),
        }
    }

    async fn upsert_station_eta_impl(
        &self,
        gtfs_id: &str,
        source_station_code: &str,
        destination_station_code: &str,
        eta_in_seconds: i32,
    ) -> AppResult<()> {
        let entry = StationEtaEntry {
            source_station_code: source_station_code.to_string(),
            destination_station_code: destination_station_code.to_string(),
            eta_in_seconds,
        };
        self.upsert_station_etas_impl(gtfs_id, None, std::slice::from_ref(&entry))
            .await?;
        Ok(())
    }

    async fn upsert_station_etas_impl(
        &self,
        gtfs_id: &str,
        variant_id: Option<&str>,
        entries: &[StationEtaEntry],
    ) -> AppResult<u64> {
        if entries.is_empty() {
            return Ok(0);
        }

        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| AppError::DbError("Internal Database pool is not active".into()))?;

        let variant = self.resolve_write_variant(gtfs_id, variant_id).await?;

        // ON CONFLICT DO UPDATE cannot touch the same row twice in one statement, so a batch
        // carrying a pair more than once has to be collapsed first — last value wins.
        let mut seen: HashMap<(&str, &str), i32> = HashMap::with_capacity(entries.len());
        for e in entries {
            if e.eta_in_seconds <= 0 {
                return Err(AppError::BadRequest(format!(
                    "eta_in_seconds must be positive (got {} for {} -> {})",
                    e.eta_in_seconds, e.source_station_code, e.destination_station_code
                )));
            }
            seen.insert(
                (
                    e.source_station_code.as_str(),
                    e.destination_station_code.as_str(),
                ),
                e.eta_in_seconds,
            );
        }

        let mut srcs: Vec<&str> = Vec::with_capacity(seen.len());
        let mut dsts: Vec<&str> = Vec::with_capacity(seen.len());
        let mut secs: Vec<i32> = Vec::with_capacity(seen.len());
        for ((s, d), v) in &seen {
            srcs.push(s);
            dsts.push(d);
            secs.push(*v);
        }

        let result = sqlx::query(
            r#"
            INSERT INTO station_eta (gtfs_id, variant_id, source_station_code, destination_station_code, eta_in_seconds)
            SELECT $1, $2, s.src, s.dst, s.secs
            FROM UNNEST($3::text[], $4::text[], $5::int[]) AS s(src, dst, secs)
            ON CONFLICT (gtfs_id, variant_id, source_station_code, destination_station_code)
            DO UPDATE SET
                eta_in_seconds = EXCLUDED.eta_in_seconds,
                updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(gtfs_id)
        .bind(&variant)
        .bind(&srcs)
        .bind(&dsts)
        .bind(&secs)
        .execute(pool)
        .await
        .map_err(|e| {
            error!("Failed to upsert station_eta for gtfs_id={}: {}", gtfs_id, e);
            AppError::DbError(e.to_string())
        })?;

        self.invalidate_station_eta_cache(gtfs_id).await;

        info!(
            "Upserted {} station_eta pair(s) for gtfs_id={} variant={}",
            result.rows_affected(),
            gtfs_id,
            variant
        );

        Ok(result.rows_affected())
    }

    async fn upsert_eta_variant_impl(&self, variant: &EtaVariant) -> AppResult<EtaVariant> {
        let pool = self.pool()?;

        let mut tx = pool
            .begin()
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

        // eta_variant_one_default_idx stops a feed having two defaults but not zero, and
        // ON CONFLICT writes is_default unconditionally, so demoting the current default
        // would strand the feed with none and drop every unpinned trip to haversine.
        let had_default: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM eta_variant WHERE gtfs_id = $1 AND is_default AND NOT deleted
            )
            "#,
        )
        .bind(&variant.gtfs_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        // eta_variant_one_default_idx permits a single default per feed, so an incoming
        // default has to displace the current one inside the same transaction.
        if variant.is_default {
            sqlx::query(
                r#"
                UPDATE eta_variant
                SET is_default = false, updated_at = now()
                WHERE gtfs_id = $1 AND is_default AND NOT deleted AND code <> $2
                "#,
            )
            .bind(&variant.gtfs_id)
            .bind(&variant.code)
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;
        }

        let saved = sqlx::query_as::<_, EtaVariant>(
            r#"
            INSERT INTO eta_variant
                (variant_id, gtfs_id, code, display_name, is_default, band_start_time, band_end_time)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (gtfs_id, code) WHERE (NOT deleted)
            DO UPDATE SET
                display_name = EXCLUDED.display_name,
                is_default = EXCLUDED.is_default,
                band_start_time = EXCLUDED.band_start_time,
                band_end_time = EXCLUDED.band_end_time,
                updated_at = now()
            RETURNING variant_id, gtfs_id, code, display_name, is_default,
                      band_start_time, band_end_time
            "#,
        )
        .bind(&variant.variant_id)
        .bind(&variant.gtfs_id)
        .bind(&variant.code)
        .bind(&variant.display_name)
        .bind(variant.is_default)
        .bind(&variant.band_start_time)
        .bind(&variant.band_end_time)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        if had_default {
            let still_has_default: bool = sqlx::query_scalar(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM eta_variant WHERE gtfs_id = $1 AND is_default AND NOT deleted
                )
                "#,
            )
            .bind(&variant.gtfs_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

            if !still_has_default {
                return Err(AppError::BadRequest(format!(
                    "'{}' would be left with no default eta variant; promote another variant to default instead of clearing this one",
                    variant.gtfs_id
                )));
            }
        }

        tx.commit()
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

        self.invalidate_eta_variant_cache(&variant.gtfs_id).await;
        Ok(saved)
    }

    async fn delete_eta_variant_impl(&self, gtfs_id: &str, variant_id: &str) -> AppResult<u64> {
        let pool = self.pool()?;

        // Retiring the default would leave every trip that names no variant with nowhere to
        // fall back to, silently dropping the whole feed to haversine.
        let result = sqlx::query(
            r#"
            UPDATE eta_variant
            SET deleted = true, updated_at = now()
            WHERE gtfs_id = $1 AND variant_id = $2 AND NOT deleted AND NOT is_default
              AND NOT EXISTS (
                  SELECT 1 FROM trip_eta_override o
                  WHERE o.variant_id = eta_variant.variant_id AND o.expires_at > now()
              )
            "#,
        )
        .bind(gtfs_id)
        .bind(variant_id)
        .execute(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        if result.rows_affected() == 0 {
            // The guard above is folded into the UPDATE so a concurrent override cannot slip in
            // between check and delete; the count is re-read only to say which rule was hit.
            let in_use: i64 = sqlx::query_scalar(
                r#"
                SELECT count(*) FROM trip_eta_override o
                WHERE o.gtfs_id = $1 AND o.variant_id = $2 AND o.expires_at > now()
                "#,
            )
            .bind(gtfs_id)
            .bind(variant_id)
            .fetch_one(pool)
            .await
            .unwrap_or(0);

            if in_use > 0 {
                return Err(AppError::BadRequest(format!(
                    "Variant '{}' is still in force on {} trip(s); clear those overrides before deleting it",
                    variant_id, in_use
                )));
            }
            return Err(AppError::BadRequest(format!(
                "Variant '{}' is unknown, already deleted, or is the default for '{}'",
                variant_id, gtfs_id
            )));
        }

        self.invalidate_eta_variant_cache(gtfs_id).await;
        Ok(result.rows_affected())
    }

    async fn set_trip_eta_override_impl(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: i32,
        variant_id: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
        updated_by: Option<&str>,
    ) -> AppResult<()> {
        let pool = self.pool()?;
        let variant = self
            .resolve_write_variant(gtfs_id, Some(variant_id))
            .await?;

        // waybill_no is what ops and the backend address a trip by; waybill_id is what the
        // table keys on. Resolving it here keeps that translation in one place.
        let result = sqlx::query(
            r#"
            INSERT INTO trip_eta_override
                (gtfs_id, waybill_id, trip_number, variant_id, expires_at, updated_by)
            SELECT $1, w.waybill_id, $3, $4, $5, $6
            FROM waybills_internal w
            WHERE w.gtfs_id = $1 AND w.waybill_no::text = $2 AND w.deleted = false
            ON CONFLICT (gtfs_id, waybill_id, trip_number)
            DO UPDATE SET
                variant_id = EXCLUDED.variant_id,
                expires_at = EXCLUDED.expires_at,
                updated_by = EXCLUDED.updated_by,
                updated_at = now()
            "#,
        )
        .bind(gtfs_id)
        .bind(waybill_no)
        .bind(trip_number)
        .bind(&variant)
        .bind(expires_at)
        .bind(updated_by)
        .execute(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!(
                "No live waybill '{}' for gtfs_id '{}'",
                waybill_no, gtfs_id
            )));
        }

        info!(
            "Set eta override gtfs_id={} waybill_no={} trip={} variant={} until={}",
            gtfs_id, waybill_no, trip_number, variant, expires_at
        );
        Ok(())
    }

    async fn clear_trip_eta_override_impl(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: i32,
    ) -> AppResult<u64> {
        let pool = self.pool()?;

        let result = sqlx::query(
            r#"
            DELETE FROM trip_eta_override o
            USING waybills_internal w
            WHERE o.gtfs_id = $1
              AND o.waybill_id = w.waybill_id
              AND w.gtfs_id = $1
              AND w.waybill_no::text = $2
              AND o.trip_number = $3
            "#,
        )
        .bind(gtfs_id)
        .bind(waybill_no)
        .bind(trip_number)
        .execute(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        Ok(result.rows_affected())
    }

    async fn list_active_trip_eta_overrides_impl(
        &self,
        gtfs_id: &str,
    ) -> AppResult<Vec<ActiveTripEtaOverride>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        sqlx::query_as::<_, ActiveTripEtaOverride>(
            r#"
            SELECT
                w.waybill_no::text AS waybill_no,
                o.trip_number,
                COALESCE(bstf.route_number_id::text, bstd.route_number_id::text) AS route_id,
                o.variant_id,
                o.expires_at
            FROM trip_eta_override o
            JOIN waybills_internal w
                ON w.waybill_id = o.waybill_id
                AND w.gtfs_id = o.gtfs_id
            LEFT JOIN bus_schedule_trip_flexi_internal bstf
                ON w.is_flexi
                AND bstf.waybill_id = w.waybill_id
                AND bstf.trip_number = o.trip_number
                AND bstf.gtfs_id = o.gtfs_id
                AND bstf.deleted = false
            LEFT JOIN bus_schedule_trip_detail_internal bstd
                ON NOT w.is_flexi
                AND bstd.schedule_trip_id = w.schedule_trip_id
                AND bstd.trip_number = o.trip_number
                AND bstd.gtfs_id = o.gtfs_id
                AND bstd.deleted = false
            WHERE o.gtfs_id = $1
              AND o.expires_at > now()
              AND EXISTS (SELECT 1 FROM eta_variant v WHERE v.variant_id = o.variant_id AND NOT v.deleted)
              AND w.waybill_no IS NOT NULL
            "#,
        )
        .bind(gtfs_id)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))
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
        self.get_vehicle_data_impl(vehicle_no, gtfs_id, trip_number)
            .await
    }

    async fn get_waybills_by_route_id(
        &self,
        route_id: &str,
        gtfs_id: &str,
        vehicle_number: Option<&str>,
    ) -> AppResult<Vec<VehicleData>> {
        self.get_waybills_by_route_id_impl(route_id, gtfs_id, vehicle_number)
            .await
    }

    async fn get_waybill_by_waybill_and_trip(
        &self,
        waybill_no: &str,
        trip_number: i32,
        gtfs_id: &str,
    ) -> AppResult<Vec<VehicleData>> {
        self.get_waybill_by_waybill_and_trip_impl(waybill_no, trip_number, gtfs_id)
            .await
    }

    async fn get_station_etas(&self, gtfs_id: &str) -> AppResult<StationEtaMap> {
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

    async fn upsert_station_etas(
        &self,
        gtfs_id: &str,
        variant_id: Option<&str>,
        entries: &[StationEtaEntry],
    ) -> AppResult<u64> {
        self.upsert_station_etas_impl(gtfs_id, variant_id, entries)
            .await
    }

    async fn get_eta_variants(&self, gtfs_id: &str) -> AppResult<Vec<EtaVariant>> {
        self.get_eta_variants_impl(gtfs_id).await
    }

    async fn upsert_eta_variant(&self, variant: &EtaVariant) -> AppResult<EtaVariant> {
        self.upsert_eta_variant_impl(variant).await
    }

    async fn delete_eta_variant(&self, gtfs_id: &str, variant_id: &str) -> AppResult<u64> {
        self.delete_eta_variant_impl(gtfs_id, variant_id).await
    }

    async fn set_trip_eta_override(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: i32,
        variant_id: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
        updated_by: Option<&str>,
    ) -> AppResult<()> {
        self.set_trip_eta_override_impl(
            gtfs_id,
            waybill_no,
            trip_number,
            variant_id,
            expires_at,
            updated_by,
        )
        .await
    }

    async fn clear_trip_eta_override(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: i32,
    ) -> AppResult<u64> {
        self.clear_trip_eta_override_impl(gtfs_id, waybill_no, trip_number)
            .await
    }

    async fn list_active_trip_eta_overrides(
        &self,
        gtfs_id: &str,
    ) -> AppResult<Vec<ActiveTripEtaOverride>> {
        self.list_active_trip_eta_overrides_impl(gtfs_id).await
    }

    async fn get_waybill_metadata(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
    ) -> AppResult<WaybillMetadataResponse> {
        self.get_waybill_metadata_impl(gtfs_id, waybill_no).await
    }

    async fn get_vehicle_tag_number(
        &self,
        fleet_no: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<String>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(None),
        };

        let cache_key = (fleet_no.to_string(), gtfs_id.to_string());
        {
            let cache = self.tag_number_cache.read().await;
            if let Some((tag, fetched_at)) = cache.get(&cache_key) {
                let ttl = if tag.is_some() {
                    TAG_NUMBER_CACHE_DURATION
                } else {
                    TAG_NUMBER_NEGATIVE_CACHE_DURATION
                };
                if fetched_at.elapsed().unwrap_or_default() < Duration::from_secs(ttl) {
                    return Ok(tag.clone());
                }
            }
        }

        let db_result = sqlx::query_scalar::<_, Option<String>>(
            "SELECT tag_number FROM vehicles_internal WHERE fleet_no = $1 AND gtfs_id = $2 AND deleted = false LIMIT 1",
        )
        .bind(fleet_no)
        .bind(gtfs_id)
        .fetch_optional(pool)
        .await;

        // Blank DB tag_number must not beat a real CSV value at the handler.
        // On DB error, cache None with the negative TTL so a dead DB is paid once
        // per window instead of per request.
        let tag = match db_result {
            Ok(row) => row.flatten().and_then(|s| {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            }),
            Err(e) => {
                warn!(
                    "get_vehicle_tag_number: DB failure for ({}, {}): {}; caching negative for {}s",
                    gtfs_id, fleet_no, e, TAG_NUMBER_NEGATIVE_CACHE_DURATION
                );
                None
            }
        };

        {
            let mut cache = self.tag_number_cache.write().await;
            if cache.len() >= TAG_NUMBER_CACHE_MAX_ENTRIES {
                cache.retain(|_, (t, stamp)| {
                    let ttl = if t.is_some() {
                        TAG_NUMBER_CACHE_DURATION
                    } else {
                        TAG_NUMBER_NEGATIVE_CACHE_DURATION
                    };
                    stamp.elapsed().unwrap_or_default() < Duration::from_secs(ttl)
                });
                while cache.len() >= TAG_NUMBER_CACHE_MAX_ENTRIES {
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
            cache.insert(cache_key, (tag.clone(), SystemTime::now()));
        }

        Ok(tag)
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
        };

        Ok(response)
    }
}
