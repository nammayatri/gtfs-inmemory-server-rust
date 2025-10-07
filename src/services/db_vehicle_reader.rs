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
                let bus_schedule_trip_detail_query: String = if let Some(trip_number) = trip_number
                {
                    format!("select NULL::int as stops_count, route_number_id::text as route_id, schedule_number, org_name::text as org_name, trip_number from bus_schedule_trip_detail where schedule_trip_id = $1::bigint and trip_number >= {} order by trip_number asc", trip_number)
                } else {
                    "select NULL::int as stops_count, route_number_id::text as route_id, schedule_number, org_name::text as org_name, trip_number from bus_schedule_trip_detail where schedule_trip_id = $1::bigint and trip_number >= (SELECT COALESCE((select trip_number from bus_schedule_trip_detail where schedule_trip_id = $1::bigint and is_active_trip = true), 1)) order by trip_number asc".to_string()
                };
                let bus_schedule_trip_flexi_query: String = if let Some(trip_number) = trip_number {
                    format!("select NULL::int as stops_count, route_number_id::text as route_id, schedule_number, org_name::text as org_name, trip_number from bus_schedule_trip_flexi where schedule_trip_id = $1::bigint and trip_number >= {} order by trip_number asc", trip_number)
                } else {
                    "select NULL::int as stops_count, route_number_id::text as route_id, schedule_number, org_name::text as org_name, trip_number from bus_schedule_trip_flexi where schedule_trip_id = $1::bigint and trip_number >= (SELECT COALESCE((select trip_number from bus_schedule_trip_flexi where schedule_trip_id = $1::bigint and is_active_trip = true), 1)) order by trip_number asc".to_string()
                };
                let bus_schedule_query: String = "select NULL::int as stops_count, route_id::text as route_id, schedule_number, NULL::text as org_name, NULL::int as trip_number from bus_schedule where schedule_number = $1 and deleted = false".to_string();

                let (schedule_result, is_active_trip, remaining_trip_details) =
                    if let Some(schedule_trip_id) = &vehicle_data.schedule_trip_id {
                        error!("schedule_trip_id: {}", bus_schedule_trip_detail_query);
                        // First try: bus_schedule_trip_detail_query (fetch all rows)
                        match sqlx::query_as::<_, BusSchedule>(&bus_schedule_trip_detail_query)
                            .bind(schedule_trip_id)
                            .fetch_all(&self.pool)
                            .await
                        {
                            Ok(mut rows) if !rows.is_empty() => {
                                self.enrich_route_numbers(&mut rows).await?;
                                let first = rows.remove(0);
                                let remaining = if rows.is_empty() { None } else { Some(rows) };
                                (Some(first), true, remaining)
                            }
                            Ok(_) => {
                                // Second try: bus_schedule_trip_flexi_query
                                match sqlx::query_as::<_, BusSchedule>(
                                    &bus_schedule_trip_flexi_query,
                                )
                                .bind(schedule_trip_id)
                                .fetch_all(&self.pool)
                                .await
                                {
                                    Ok(mut rows) if !rows.is_empty() => {
                                        self.enrich_route_numbers(&mut rows).await?;
                                        let first = rows.remove(0);
                                        let remaining =
                                            if rows.is_empty() { None } else { Some(rows) };
                                        (Some(first), true, remaining)
                                    }
                                    Ok(_) | Err(_) => {
                                        // Third try: bus_schedule_query
                                        let mut rows = match sqlx::query_as::<_, BusSchedule>(
                                            &bus_schedule_query,
                                        )
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
                                            let remaining =
                                                if rows.is_empty() { None } else { Some(rows) };
                                            (Some(first), false, remaining)
                                        } else {
                                            (None, false, None)
                                        }
                                    }
                                }
                            }
                            Err(_) => {
                                // If first query fails, try second query
                                match sqlx::query_as::<_, BusSchedule>(
                                    &bus_schedule_trip_flexi_query,
                                )
                                .bind(schedule_trip_id)
                                .fetch_all(&self.pool)
                                .await
                                {
                                    Ok(mut rows) if !rows.is_empty() => {
                                        self.enrich_route_numbers(&mut rows).await?;
                                        let first = rows.remove(0);
                                        let remaining =
                                            if rows.is_empty() { None } else { Some(rows) };
                                        (Some(first), true, remaining)
                                    }
                                    Ok(_) | Err(_) => {
                                        // Third try: bus_schedule_query
                                        let mut rows = match sqlx::query_as::<_, BusSchedule>(
                                            &bus_schedule_query,
                                        )
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
                                            let remaining =
                                                if rows.is_empty() { None } else { Some(rows) };
                                            (Some(first), false, remaining)
                                        } else {
                                            (None, false, None)
                                        }
                                    }
                                }
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
