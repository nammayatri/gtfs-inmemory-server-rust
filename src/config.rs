use anyhow::{Context, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub database_url: Option<String>,
    pub db_max_connections: u32,
    pub cache_duration: u64,
    pub base_url: String,
    pub polling_interval: u64,
    pub process_batch_size: usize,
    pub api_host: String,
    pub api_port: u16,
    pub gc_interval: u64,
    pub max_retries: u32,
    pub retry_delay: u64,
    pub rate_limit_delay: f64,
    pub cpu_threshold: f32,
    pub connection_limit: usize,
    pub dns_ttl: u64,
    pub memory_threshold: u64,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: env::var("DATABASE_URL").ok(),
            db_max_connections: env::var("WAYBILLS_DB_MAX_CONNECTIONS")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .context("Failed to parse WAYBILLS_DB_MAX_CONNECTIONS")?,
            cache_duration: env::var("CACHE_DURATION")
                .unwrap_or_else(|_| "3600".to_string())
                .parse()
                .context("Failed to parse CACHE_DURATION")?,
            base_url: env::var("GTFS_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8080".to_string()),
            polling_interval: env::var("GTFS_POLLING_INTERVAL")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .context("Failed to parse GTFS_POLLING_INTERVAL")?,
            process_batch_size: env::var("GTFS_PROCESS_BATCH_SIZE")
                .unwrap_or_else(|_| "50".to_string())
                .parse()
                .context("Failed to parse GTFS_PROCESS_BATCH_SIZE")?,
            api_host: env::var("GTFS_API_HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            api_port: env::var("GTFS_API_PORT")
                .unwrap_or_else(|_| "8000".to_string())
                .parse()
                .context("Failed to parse GTFS_API_PORT")?,
            gc_interval: env::var("GTFS_GC_INTERVAL")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .context("Failed to parse GTFS_GC_INTERVAL")?,
            max_retries: env::var("GTFS_MAX_RETRIES")
                .unwrap_or_else(|_| "3".to_string())
                .parse()
                .context("Failed to parse GTFS_MAX_RETRIES")?,
            retry_delay: env::var("GTFS_RETRY_DELAY")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .context("Failed to parse GTFS_RETRY_DELAY")?,
            rate_limit_delay: env::var("GTFS_RATE_LIMIT_DELAY")
                .unwrap_or_else(|_| "0.1".to_string())
                .parse()
                .context("Failed to parse GTFS_RATE_LIMIT_DELAY")?,
            cpu_threshold: env::var("GTFS_CPU_THRESHOLD")
                .unwrap_or_else(|_| "80.0".to_string())
                .parse()
                .context("Failed to parse GTFS_CPU_THRESHOLD")?,
            connection_limit: env::var("GTFS_CONNECTION_LIMIT")
                .unwrap_or_else(|_| "100".to_string())
                .parse()
                .context("Failed to parse GTFS_CONNECTION_LIMIT")?,
            dns_ttl: env::var("GTFS_DNS_TTL")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .context("Failed to parse GTFS_DNS_TTL")?,
            memory_threshold: env::var("GTFS_MEMORY_THRESHOLD")
                .unwrap_or_else(|_| "5000".to_string())
                .parse()
                .context("Failed to parse GTFS_MEMORY_THRESHOLD")?,
        })
    }
}
