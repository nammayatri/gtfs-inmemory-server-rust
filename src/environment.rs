use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::sync::Arc;
use std::time::Duration;

use crate::services::{
    bhubaneswar_vehicle_cache::BhubaneswarVehicleCache,
    db_employee_reader::{DBEmployeeReader, EmployeeReader, MockEmployeeReader},
    db_vehicle_reader::{DBVehicleReader, MockDBVehicleReader, VehicleDataReader},
    gtfs_service::GTFSService,
    trip_service::TripService,
};
use crate::tools::dhall::read_dhall_config as dhall_read_config;
use crate::tools::error::AppError;
use shared::tools::logger::LoggerConfig;
use tracing::{error, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtpInstance {
    pub url: String,
    pub identifier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtpConfig {
    pub city_based_instances: Vec<OtpInstance>,
    pub gtfs_id_based_instances: Vec<OtpInstance>,
    pub default_instance: OtpInstance,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub logger_cfg: LoggerConfig,
    pub database_url: Option<String>,
    pub db_max_connections: u32,
    pub db_min_connections: u32,
    pub db_acquire_timeout: u64,
    pub db_idle_timeout: u64,
    pub db_max_lifetime: u64,
    pub cache_duration: u64,
    pub otp_instances: OtpConfig,
    pub polling_enabled: bool,
    pub polling_interval: u64,
    pub process_batch_size: usize,
    pub port: u16,
    pub gc_interval: u64,
    pub max_retries: u32,
    pub retry_delay: u64,
    pub rate_limit_delay: f64,
    pub cpu_threshold: f32,
    pub connection_limit: usize,
    pub http_pool_idle_timeout: u64,
    pub http_tcp_keepalive: u64,
    pub dns_ttl: u64,
    pub memory_threshold: u64,
    pub ignored_trip_ids: Vec<String>,
    pub bhubaneswar_cache_update_interval: u64,
    pub bhubaneswar_external_auth: Option<String>,
}

impl OtpConfig {
    pub fn find_instance_by_gtfs_id(&self, gtfs_id: &str) -> Option<&OtpInstance> {
        self.gtfs_id_based_instances
            .iter()
            .find(|instance| instance.identifier == gtfs_id)
    }

    pub fn find_instance_by_city(&self, city: &str) -> Option<&OtpInstance> {
        self.city_based_instances
            .iter()
            .find(|instance| instance.identifier == city)
    }

    pub fn get_default_instance(&self) -> &OtpInstance {
        &self.default_instance
    }

    pub fn get_all_instances(&self) -> Vec<&OtpInstance> {
        let mut instances = Vec::new();
        instances.extend(&self.gtfs_id_based_instances);
        instances.extend(&self.city_based_instances);
        instances
    }
}

pub fn read_dhall_config(dhall_config_path: &str) -> Result<AppConfig> {
    dhall_read_config(dhall_config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read Dhall config: {}", e))
}

async fn create_database_pool(config: &AppConfig) -> Result<PgPool, AppError> {
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
    Ok(pool)
}

#[derive(Clone)]
pub struct AppState {
    pub gtfs_service: Arc<GTFSService>,
    pub db_vehicle_reader: Arc<dyn VehicleDataReader>,
    pub db_employee_reader: Arc<dyn EmployeeReader>,
    pub trip_service: Arc<TripService>,
    pub bhubaneswar_vehicle_cache: Arc<BhubaneswarVehicleCache>,
    pub config: AppConfig,
}

impl AppState {
    pub async fn new(app_config: AppConfig) -> Result<AppState> {
        // Initialize services
        let gtfs_service = Arc::new(GTFSService::new(app_config.clone()).await?);

        // Create shared database pool or use mock readers
        let (db_vehicle_reader, db_employee_reader): (
            Arc<dyn VehicleDataReader>,
            Arc<dyn EmployeeReader>,
        ) = if let Some(db_url) = &app_config.database_url {
            if db_url.contains("localhost") {
                // For local development, fall back to mock readers on connection failure
                match create_database_pool(&app_config).await {
                    Ok(pool) => {
                        info!("Successfully connected to the local database.");
                        let vehicle_reader =
                            Arc::new(DBVehicleReader::new(pool.clone(), &app_config));
                        let employee_reader = Arc::new(DBEmployeeReader::new(pool, &app_config));
                        (vehicle_reader, employee_reader)
                    }
                    Err(e) => {
                        error!("Failed to connect to the local database: {}. Falling back to mock DB readers.", e);
                        (
                            Arc::new(MockDBVehicleReader::new()),
                            Arc::new(MockEmployeeReader::new()),
                        )
                    }
                }
            } else {
                // For non-local (production) environments, require a valid DB connection
                info!("Connecting to production database...");
                let pool = create_database_pool(&app_config)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to create database pool: {}", e))?;
                let vehicle_reader = Arc::new(DBVehicleReader::new(pool.clone(), &app_config));
                let employee_reader = Arc::new(DBEmployeeReader::new(pool, &app_config));
                (vehicle_reader, employee_reader)
            }
        } else {
            // If no DATABASE_URL is provided, use the mock readers
            info!("No DATABASE_URL found, using mock DB readers");
            (
                Arc::new(MockDBVehicleReader::new()),
                Arc::new(MockEmployeeReader::new()),
            )
        };

        let trip_service = Arc::new(TripService::new(gtfs_service.clone()));

        let mut bhubaneswar_vehicle_cache = BhubaneswarVehicleCache::new(
            app_config.bhubaneswar_external_auth.clone(),
        )?;
        bhubaneswar_vehicle_cache.set_update_interval(app_config.bhubaneswar_cache_update_interval);
        let bhubaneswar_vehicle_cache = Arc::new(bhubaneswar_vehicle_cache);
        bhubaneswar_vehicle_cache.initialize().await?;

        let app_state = AppState {
            gtfs_service,
            db_vehicle_reader,
            db_employee_reader,
            trip_service,
            bhubaneswar_vehicle_cache,
            config: app_config,
        };

        Ok(app_state)
    }
}
