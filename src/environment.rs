use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::services::{
    db_vehicle_reader::{DBVehicleReader, MockDBVehicleReader, VehicleDataReader},
    gtfs_service::GTFSService,
    trip_service::TripService,
};
use crate::tools::dhall::read_dhall_config as dhall_read_config;
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

#[derive(Clone)]
pub struct AppState {
    pub gtfs_service: Arc<GTFSService>,
    pub db_vehicle_reader: Arc<dyn VehicleDataReader>,
    pub trip_service: Arc<TripService>,
    pub config: AppConfig,
}

impl AppState {
    pub async fn new(app_config: AppConfig) -> Result<AppState> {
        // Initialize services
        let gtfs_service = Arc::new(GTFSService::new(app_config.clone()).await?);

        let db_vehicle_reader: Arc<dyn VehicleDataReader> = if let Some(db_url) =
            &app_config.database_url
        {
            if db_url.contains("localhost") {
                // For local development, fall back to mock reader on connection failure
                match DBVehicleReader::new(&app_config).await {
                    Ok(reader) => {
                        info!("Successfully connected to the local database.");
                        Arc::new(reader)
                    }
                    Err(e) => {
                        error!("Failed to connect to the local database: {}. Falling back to mock DB reader.", e);
                        Arc::new(MockDBVehicleReader::new())
                    }
                }
            } else {
                // For non-local (production) environments, require a valid DB connection
                info!("Connecting to production database...");
                let reader = DBVehicleReader::new(&app_config).await?;
                Arc::new(reader)
            }
        } else {
            // If no DATABASE_URL is provided, use the mock reader
            info!("No DATABASE_URL found, using mock DB reader");
            Arc::new(MockDBVehicleReader::new())
        };

        let trip_service = Arc::new(TripService::new(gtfs_service.clone()));

        let app_state = AppState {
            gtfs_service,
            db_vehicle_reader,
            trip_service,
            config: app_config,
        };

        Ok(app_state)
    }
}
