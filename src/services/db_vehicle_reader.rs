use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::{debug, error, info};

use crate::environment::AppConfig;
use crate::models::{BusSchedule, MinimalVehicleData, VehicleData, VehicleDataWithRouteId};
use crate::tools::error::{AppError, AppResult};

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
    async fn search_vehicles(&self, query: &str) -> AppResult<Vec<VehicleData>>;
    async fn get_vehicle_count(&self) -> AppResult<i64>;
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
}

pub struct DBVehicleReader {
    pool: PgPool,
    cache: Arc<RwLock<HashMap<String, (VehicleDataWithRouteId, SystemTime)>>>,
    cache_duration: Duration,
}

impl DBVehicleReader {
    pub async fn new(config: &AppConfig) -> AppResult<Self> {
        let db_url = config
            .database_url
            .as_ref()
            .ok_or_else(|| AppError::Internal("DATABASE_URL is not set".to_string()))?;
        let pool = PgPoolOptions::new()
            .max_connections(config.db_max_connections)
            .min_connections(config.db_min_connections)
            .acquire_timeout(Duration::from_secs(config.db_acquire_timeout))
            .idle_timeout(Duration::from_secs(config.db_idle_timeout))
            .max_lifetime(Duration::from_secs(config.db_max_lifetime))
            .connect(db_url)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to connect to database: {}", e)))?;

        info!("Database connection pool created successfully.");

        Ok(Self {
            pool,
            cache: Arc::new(RwLock::new(HashMap::new())),
            cache_duration: Duration::from_secs(config.cache_duration),
        })
    }

    async fn get_cached_vehicle_data(&self, vehicle_no: &str) -> Option<VehicleDataWithRouteId> {
        let cache = self.cache.read().await;
        if let Some((data, timestamp)) = cache.get(vehicle_no) {
            if timestamp.elapsed().unwrap_or_default() < self.cache_duration {
                debug!("Cache HIT for vehicle {}", vehicle_no);
                return Some(data.clone());
            }
        }
        debug!("Cache MISS for vehicle {}", vehicle_no);
        None
    }

    async fn cache_vehicle_data(&self, vehicle_data: &VehicleDataWithRouteId) {
        let mut cache = self.cache.write().await;
        cache.insert(
            vehicle_data.vehicle_no.clone()
                + vehicle_data
                    .trip_number
                    .map(|t| t.to_string())
                    .unwrap_or("".to_string())
                    .as_str(),
            (vehicle_data.clone(), SystemTime::now()),
        );
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

    fn log_trip_rows(&self, source: &str, rows: &[BusSchedule]) {
        info!(
            source = source,
            count = rows.len(),
            "Trip rows fetched from table"
        );
        // Print a small table of up to 10 rows for readability
        info!(
            source = source,
            "{:<16} | {:<16} | {:<6} | {:<5} | {}",
            "schedule_no",
            "route_id",
            "trip#",
            "active",
            "org_name"
        );
        for r in rows.iter().take(10) {
            info!(
                source = source,
                "{:<16} | {:<16} | {:<6} | {:<5} | {}",
                r.schedule_number,
                r.route_id,
                r.trip_number.unwrap_or_default(),
                if r.is_active_trip.unwrap_or(false) { "true" } else { "false" },
                r.org_name.as_deref().unwrap_or("")
            );
        }
        if rows.len() > 10 {
            info!(source = source, remaining = rows.len() - 10, "... more rows omitted");
        }
    }

    async fn fetch_trip_rows_for_schedule(
        &self,
        schedule_trip_id: &str,
        detail_query: &str,
        flexi_query: &str,
    ) -> Vec<BusSchedule> {
        // Fetch from detailed trips first
        let mut detail_rows = match sqlx::query_as::<_, BusSchedule>(detail_query)
            .bind(schedule_trip_id)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                error!(
                    "fetch_trip_rows_for_schedule: detail query failed. query={} error={}",
                    detail_query, e
                );
                Vec::new()
            }
        };
        self.log_trip_rows("bus_schedule_trip_detail", &detail_rows);

        // Only fetch from flexi trips if no active trip is present in detail rows
        let detail_has_active = detail_rows
            .iter()
            .any(|row| row.is_active_trip.unwrap_or(false));

        let mut flexi_rows: Vec<BusSchedule> = Vec::new();
        if !detail_has_active {
            flexi_rows = match sqlx::query_as::<_, BusSchedule>(flexi_query)
                .bind(schedule_trip_id)
                .fetch_all(&self.pool)
                .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    error!(
                        "fetch_trip_rows_for_schedule: flexi query failed. query={} error={}",
                        flexi_query, e
                    );
                    Vec::new()
                }
            };
            self.log_trip_rows("bus_schedule_trip_flexi", &flexi_rows);
        } else {
            info!(
                schedule_trip_id = schedule_trip_id,
                "Skipping flexi fetch: active trip found in detail table"
            );
        }

        // Combine (flexi_rows may be empty if skipped)
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
        let has_active_front = detail_rows.get(0).map(|r| r.is_active_trip.unwrap_or(false)).unwrap_or(false);
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

        detail_rows
    }
}

#[async_trait]
impl VehicleDataReader for DBVehicleReader {
    async fn get_vehicle_data(
        &self,
        vehicle_no: &str,
        trip_number: Option<i32>,
    ) -> AppResult<VehicleDataWithRouteId> {
        if let Some(cached_data) = self.get_cached_vehicle_data(vehicle_no).await {
            return Ok(cached_data);
        }

        let waybill_online_query =
            "
            SELECT w.waybill_id::text, w.waybill_no::text, w.service_type, w.vehicle_no, w.schedule_no, w.updated_at::timestamptz as last_updated, w.duty_date, w.schedule_trip_id::text, e.entity_remark::text as entity_remark
            FROM waybills w
            left join vehicles v on v.fleet_no = w.vehicle_no
            left join entities e on e.entity_id = v.entity_id
            WHERE w.vehicle_no = $1
            and w.status = 'Online'
            LIMIT 1
        ";

        let result = match sqlx::query_as::<_, VehicleData>(waybill_online_query)
            .bind(vehicle_no)
            .fetch_optional(&self.pool)
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
                info!("vehicle_data in db_vehicle_readerss {:?}", vehicle_data);
                let bus_schedule_trip_detail_query: String = if let Some(trip_number) = trip_number
                {
                    format!("select NULL::int as stops_count, route_number_id::text as route_id, schedule_number, org_name::text as org_name, trip_number from bus_schedule_trip_detail where schedule_trip_id = $1::bigint and trip_number >= {} order by trip_number asc", trip_number)
                } else {
                    "select NULL::int as stops_count, route_number_id::text as route_id, schedule_number, org_name::text as org_name, trip_number from bus_schedule_trip_detail where schedule_trip_id = $1::bigint and trip_number >= (SELECT COALESCE((select trip_number from bus_schedule_trip_detail where schedule_trip_id = $1::bigint and is_active_trip = true), 1)) order by trip_number asc".to_string()
                };
                let bus_schedule_trip_flexi_query: String = if let Some(trip_number) = trip_number {
                    format!("select NULL::int as stops_count, route_number_id::text as route_id, schedule_number, org_name::text as org_name, trip_number from bus_schedule_trip_flexi where schedule_trip_id = $1::bigint and trip_number >= {} order by trip_number asc", trip_number)
                } else {
                    "WITH latest AS ( SELECT trip_number AS active_trip_number, created_at AS active_created_at FROM bus_schedule_trip_flexi WHERE schedule_trip_id = $1 AND is_active_trip = TRUE ORDER BY created_at DESC LIMIT 1 ) SELECT NULL::int AS stops_count, f.route_number_id::text AS route_id, f.schedule_number, f.org_name::text AS org_name, f.trip_number FROM bus_schedule_trip_flexi f LEFT JOIN latest l ON TRUE WHERE f.schedule_trip_id = $1 AND f.trip_number >= COALESCE(l.active_trip_number, 1) AND f.created_at > COALESCE(l.active_created_at, now() - INTERVAL '1 day') ORDER BY f.trip_number ASC".to_string()
                };
                let bus_schedule_query: String = "select NULL::int as stops_count, route_id::text as route_id, schedule_number, NULL::text as org_name, NULL::int as trip_number from bus_schedule where schedule_number = $1 and deleted = false".to_string();

                let (schedule_result, is_active_trip, remaining_trip_details) =
                    if let Some(schedule_trip_id) = &vehicle_data.schedule_trip_id {
                        // Always fetch from BOTH detail and flexi tables
                        let mut rows = self
                            .fetch_trip_rows_for_schedule(
                                schedule_trip_id,
                                &bus_schedule_trip_detail_query,
                                &bus_schedule_trip_flexi_query,
                            )
                            .await;
                        if !rows.is_empty() {
                            self.enrich_route_numbers(&mut rows).await?;
                            let first = rows.remove(0);
                            let remaining = if rows.is_empty() { None } else { Some(rows) };
                            (Some(first), true, remaining)
                        } else {
                            // Fallback to bus_schedule
                            let mut rows = match sqlx::query_as::<_, BusSchedule>(&bus_schedule_query)
                                .bind(vehicle_data.schedule_no.clone())
                                .fetch_all(&self.pool)
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
                                self.enrich_route_numbers(&mut rows).await?;
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
                            .fetch_all(&self.pool)
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                error!("Query failed (bus_schedule direct): {}", e);
                                Vec::new()
                            }
                        };
                        if !rows.is_empty() {
                            self.enrich_route_numbers(&mut rows).await?;
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
                };
                if let Some(schedule) = schedule_result {
                    vehicle_data_with_route_id.trip_number = schedule.trip_number;
                    vehicle_data_with_route_id.route_id = Some(schedule.route_id.to_owned());
                    vehicle_data_with_route_id.depot = schedule.org_name.clone();
                    vehicle_data_with_route_id.route_number = schedule.route_number.clone();
                }
                self.cache_vehicle_data(&vehicle_data_with_route_id).await;
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
                        }
                    };

                Ok(vehicle_data_with_route_id)
            }
        }
    }

    async fn get_all_vehicles(&self) -> AppResult<Vec<VehicleData>> {
        let query = "
            SELECT DISTINCT ON (vehicle_no)
              waybill_id::text, service_type, vehicle_no, schedule_no, updated_at::timestamptz as last_updated, duty_date
            FROM waybills
            ORDER BY vehicle_no, updated_at DESC
        ";

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
        let query = "
            SELECT DISTINCT ON (vehicle_no)
              waybill_id::text, service_type, vehicle_no, schedule_no, updated_at::timestamptz as last_updated, duty_date
            FROM waybills
            WHERE service_type = $1
            ORDER BY vehicle_no, updated_at DESC
        ";

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
        let query_sql = "
            SELECT DISTINCT ON (vehicle_no)
              waybill_id::text, service_type, vehicle_no, schedule_no, updated_at::timestamptz as last_updated, duty_date
            FROM waybills
            WHERE vehicle_no ILIKE $1 OR waybill_id::text ILIKE $1 OR schedule_no ILIKE $1
            ORDER BY vehicle_no, updated_at DESC
        ";

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
            if let Some(cached_data) = self.get_cached_vehicle_data(vehicle_no).await {
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
            "SELECT waybill_id::text, service_type, vehicle_no, schedule_no, updated_at::timestamptz as last_updated, duty_date
             FROM waybills
             WHERE vehicle_no IN ({})
             ORDER BY updated_at DESC",
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
}
