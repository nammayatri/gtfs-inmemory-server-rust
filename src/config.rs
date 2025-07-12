use anyhow::{Context, Result};
use serde::Serialize;
use std::env;

#[derive(Debug, Clone, Serialize)]
pub struct OtpInstance {
    pub url: String,
    pub identifier: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OtpConfig {
    pub city_based_instances: Vec<OtpInstance>,
    pub gtfs_id_based_instances: Vec<OtpInstance>,
    pub default_instance: OtpInstance,
}

impl OtpConfig {
    pub fn find_instance_by_gtfs_id(&self, gtfs_id: &str) -> Option<&OtpInstance> {
        self.gtfs_id_based_instances.iter()
            .find(|instance| instance.identifier == gtfs_id)
    }

    pub fn find_instance_by_city(&self, city: &str) -> Option<&OtpInstance> {
        self.city_based_instances.iter()
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

#[derive(Debug, Clone, Serialize)]
pub struct AppConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
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
    pub api_host: String,
    pub api_port: u16,
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

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: env::var("DATABASE_URL").ok(),
            db_max_connections: env::var("WAYBILLS_DB_MAX_CONNECTIONS")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .context("Failed to parse WAYBILLS_DB_MAX_CONNECTIONS")?,
            db_min_connections: env::var("WAYBILLS_DB_MIN_CONNECTIONS")
                .unwrap_or_else(|_| "2".to_string())
                .parse()
                .context("Failed to parse WAYBILLS_DB_MIN_CONNECTIONS")?,
            db_acquire_timeout: env::var("WAYBILLS_DB_ACQUIRE_TIMEOUT")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .context("Failed to parse WAYBILLS_DB_ACQUIRE_TIMEOUT")?,
            db_idle_timeout: env::var("WAYBILLS_DB_IDLE_TIMEOUT")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .context("Failed to parse WAYBILLS_DB_IDLE_TIMEOUT")?,
            db_max_lifetime: env::var("WAYBILLS_DB_MAX_LIFETIME")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .context("Failed to parse WAYBILLS_DB_MAX_LIFETIME")?,
            cache_duration: env::var("CACHE_DURATION")
                .unwrap_or_else(|_| "3600".to_string())
                .parse()
                .context("Failed to parse CACHE_DURATION")?,
            otp_instances: {
                let city_based = env::var("CITY_ID_OTP_INSTANCES")
                    .map(|s| {
                        s.split(',')
                            .filter_map(|pair| {
                                let mut parts = pair.splitn(2, '#');
                                let identifier = parts.next()?.trim().to_string();
                                let url = parts.next()?.trim().to_string();
                                Some(OtpInstance { identifier, url })
                            })
                            .collect()
                    })
                    .unwrap_or_else(|_| vec![]);

                let gtfs_id_based = env::var("GTFS_ID_OTP_INSTANCES")
                    .map(|s| {
                        s.split(',')
                            .filter_map(|pair| {
                                let mut parts = pair.splitn(2, '#');
                                let identifier = parts.next()?.trim().to_string();
                                let url = parts.next()?.trim().to_string();
                                Some(OtpInstance { identifier, url })
                            })
                            .collect()
                    })
                    .unwrap_or_else(|_| vec![]);

                let default_instance = OtpInstance {
                    identifier: "default".to_string(),
                    url: env::var("DEFAULT_OTP_INSTANCE")
                        .unwrap_or_else(|_| "http://localhost:8080".to_string()),
                };

                // If both are empty, provide default
                if city_based.is_empty() && gtfs_id_based.is_empty() {
                    OtpConfig {
                        city_based_instances: vec![],
                        gtfs_id_based_instances: vec![],
                        default_instance,
                    }
                } else {
                    OtpConfig {
                        city_based_instances: city_based,
                        gtfs_id_based_instances: gtfs_id_based,
                        default_instance,
                    }
                }
            },
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
            http_pool_idle_timeout: env::var("GTFS_HTTP_POOL_IDLE_TIMEOUT")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .context("Failed to parse GTFS_HTTP_POOL_IDLE_TIMEOUT")?,
            http_tcp_keepalive: env::var("GTFS_HTTP_TCP_KEEPALIVE")
                .unwrap_or_else(|_| "7200".to_string())
                .parse()
                .context("Failed to parse GTFS_HTTP_TCP_KEEPALIVE")?,
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
