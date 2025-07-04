use async_trait::async_trait;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::config::AppConfig;
use crate::errors::{AppError, AppResult};
use crate::models::VehicleData;

#[async_trait]
pub trait VehicleDataReader: Send + Sync {
    async fn get_vehicle_data(&self, vehicle_no: &str) -> AppResult<VehicleData>;
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
    async fn get_vehicle_data(&self, _vehicle_no: &str) -> AppResult<VehicleData> {
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
    cache: Arc<RwLock<HashMap<String, (VehicleData, SystemTime)>>>,
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

    async fn get_cached_vehicle_data(&self, vehicle_no: &str) -> Option<VehicleData> {
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

    async fn cache_vehicle_data(&self, vehicle_data: VehicleData) {
        let mut cache = self.cache.write().await;
        cache.insert(
            vehicle_data.vehicle_no.clone(),
            (vehicle_data, SystemTime::now()),
        );
    }
}

#[async_trait]
impl VehicleDataReader for DBVehicleReader {
    async fn get_vehicle_data(&self, vehicle_no: &str) -> AppResult<VehicleData> {
        if let Some(cached_data) = self.get_cached_vehicle_data(vehicle_no).await {
            return Ok(cached_data);
        }

        let query = "
            SELECT waybill_id, service_type, vehicle_no, schedule_no, updated_at as last_updated, duty_date
            FROM waybills
            WHERE vehicle_no = $1
            ORDER BY updated_at DESC
            LIMIT 1
        ";

        let result = sqlx::query_as::<_, VehicleData>(query)
            .bind(vehicle_no)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

        match result {
            Some(vehicle_data) => {
                self.cache_vehicle_data(vehicle_data.clone()).await;
                Ok(vehicle_data)
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
              waybill_id, service_type, vehicle_no, schedule_no, updated_at as last_updated, duty_date
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
              waybill_id, service_type, vehicle_no, schedule_no, updated_at as last_updated, duty_date
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
              waybill_id, service_type, vehicle_no, schedule_no, updated_at as last_updated, duty_date
            FROM waybills
            WHERE vehicle_no ILIKE $1 OR waybill_id ILIKE $1 OR schedule_no ILIKE $1
            ORDER BY vehicle_no, updated_at DESC
        ";

        sqlx::query_as::<_, VehicleData>(query_sql)
            .bind(&search_pattern)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))
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
