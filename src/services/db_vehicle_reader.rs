use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::environment::AppConfig;
use crate::models::{BusSchedule, VehicleData, VehicleDataWithRouteId};
use crate::tools::error::{AppError, AppResult};

#[async_trait]
pub trait VehicleDataReader: Send + Sync {
    async fn get_vehicle_data(&self, vehicle_no: &str) -> AppResult<VehicleDataWithRouteId>;
    async fn get_vehicles_by_ids(&self, vehicle_nos: Vec<String>) -> AppResult<Vec<VehicleDataWithRouteId>>;
    async fn get_all_vehicles(&self) -> AppResult<Vec<VehicleData>>;
    async fn get_vehicles_by_service_type(&self, service_type: &str)
        -> AppResult<Vec<VehicleData>>;
    async fn search_vehicles(&self, query: &str) -> AppResult<Vec<VehicleData>>;
    async fn get_vehicle_count(&self) -> AppResult<i64>;
}

// Mock implementation for local testing without a database
pub struct MockDBVehicleReader;

impl MockDBVehicleReader {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl VehicleDataReader for MockDBVehicleReader {
    async fn get_vehicle_data(&self, _vehicle_no: &str) -> AppResult<VehicleDataWithRouteId> {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    }

    async fn get_vehicles_by_ids(&self, _vehicle_nos: Vec<String>) -> AppResult<Vec<VehicleDataWithRouteId>> {
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
            vehicle_data.vehicle_no.clone(),
            (vehicle_data.clone(), SystemTime::now()),
        );
    }
}

#[async_trait]
impl VehicleDataReader for DBVehicleReader {
    async fn get_vehicle_data(&self, vehicle_no: &str) -> AppResult<VehicleDataWithRouteId> {
        if let Some(cached_data) = self.get_cached_vehicle_data(vehicle_no).await {
            return Ok(cached_data);
        }

        let query = "
            SELECT waybill_id::text, service_type, vehicle_no, schedule_no, updated_at::timestamptz as last_updated, duty_date
            FROM waybills
            WHERE vehicle_no = $1
            ORDER BY updated_at DESC
            LIMIT 1
        ";

        let schedule_query = "select schedule_number, schedule_id::text, route_id::text, deleted, status from bus_schedule where schedule_number = $1 and deleted = false limit 1";

        let result = sqlx::query_as::<_, VehicleData>(query)
            .bind(vehicle_no)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

        match result {
            Some(vehicle_data) => {
                let schedule_result = sqlx::query_as::<_, BusSchedule>(schedule_query)
                    .bind(vehicle_data.schedule_no.clone())
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|e| AppError::DbError(e.to_string()))?;
                let mut vehicle_data_with_route_id = VehicleDataWithRouteId {
                    waybill_id: vehicle_data.waybill_id,
                    service_type: vehicle_data.service_type,
                    vehicle_no: vehicle_data.vehicle_no,
                    schedule_no: vehicle_data.schedule_no,
                    last_updated: vehicle_data.last_updated,
                    duty_date: vehicle_data.duty_date,
                    route_id: None,
                };
                if let Some(schedule) = schedule_result {
                    vehicle_data_with_route_id.route_id = Some(schedule.route_id);
                }
                self.cache_vehicle_data(&vehicle_data_with_route_id).await;
                Ok(vehicle_data_with_route_id)
            }
            None => Err(AppError::NotFound(format!(
                "Vehicle not found: {}",
                vehicle_no
            ))),
        }
    }

    async fn get_all_vehicles(&self) -> AppResult<Vec<VehicleData>> {
        let query = "
            SELECT DISTINCT ON (vehicle_no)
              waybill_id::text, service_type, vehicle_no, schedule_no, updated_at::timestamptz as last_updated, duty_date
            FROM waybills
            ORDER BY vehicle_no, updated_at DESC
        ";

        sqlx::query_as::<_, VehicleData>(query)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))
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

        sqlx::query_as::<_, VehicleData>(query)
            .bind(service_type)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))
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

        sqlx::query_as::<_, VehicleData>(query_sql)
            .bind(&search_pattern)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))
    }

    async fn get_vehicles_by_ids(&self, vehicle_nos: Vec<String>) -> AppResult<Vec<VehicleDataWithRouteId>> {
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

        let vehicle_results = query_builder
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

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
                "SELECT schedule_number, schedule_id::text, route_id::text, deleted, status 
                 FROM bus_schedule 
                 WHERE schedule_number IN ({}) AND deleted = false",
                schedule_placeholders_str
            );

            let mut schedule_query_builder = sqlx::query_as::<_, BusSchedule>(&schedule_query);
            for schedule_no in &schedule_numbers {
                schedule_query_builder = schedule_query_builder.bind(schedule_no);
            }

            let schedule_results = schedule_query_builder
                .fetch_all(&self.pool)
                .await
                .map_err(|e| AppError::DbError(e.to_string()))?;

            for schedule in schedule_results {
                schedule_map.insert(schedule.schedule_number.clone(), schedule);
            }
        }

        // Build the final result
        for vehicle_data in vehicle_results {
            let mut vehicle_data_with_route_id = VehicleDataWithRouteId {
                waybill_id: vehicle_data.waybill_id,
                service_type: vehicle_data.service_type,
                vehicle_no: vehicle_data.vehicle_no,
                schedule_no: vehicle_data.schedule_no,
                last_updated: vehicle_data.last_updated,
                duty_date: vehicle_data.duty_date,
                route_id: None,
            };

            if let Some(schedule) = schedule_map.get(&vehicle_data_with_route_id.schedule_no) {
                vehicle_data_with_route_id.route_id = Some(schedule.route_id.clone());
            }

            self.cache_vehicle_data(&vehicle_data_with_route_id).await;
            found_vehicles.push(vehicle_data_with_route_id);
        }

        Ok(found_vehicles)
    }

    async fn get_vehicle_count(&self) -> AppResult<i64> {
        let row =
            sqlx::query_as::<_, (i64,)>("SELECT COUNT(DISTINCT vehicle_no) as count FROM waybills")
                .fetch_one(&self.pool)
                .await
                .map_err(|e| AppError::DbError(e.to_string()))?;
        Ok(row.0)
    }
}
