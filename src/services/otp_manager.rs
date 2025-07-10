use crate::config::{OtpConfig, OtpInstance};
use anyhow::{anyhow, Result};

#[derive(Debug, Clone)]
pub struct OtpManager {
    config: OtpConfig,
}

impl OtpManager {
    pub fn new(config: OtpConfig) -> Self {
        Self { config }
    }

    pub fn get_instance_by_city(&self, city: &str) -> Result<&OtpInstance> {
        self.config
            .find_instance_by_city(city)
            .ok_or_else(|| anyhow!("OTP instance not found for city: {}", city))
    }

    pub fn get_instance_by_gtfs_id(&self, gtfs_id: &str) -> Result<&OtpInstance> {
        self.config
            .find_instance_by_gtfs_id(gtfs_id)
            .ok_or_else(|| anyhow!("OTP instance not found for gtfs_id: {}", gtfs_id))
    }

    pub fn get_instance_by_identifier(&self, identifier: &str) -> Result<&OtpInstance> {
        // Try both city-based and gtfs-id-based instances
        self.config
            .find_instance_by_city(identifier)
            .or_else(|| self.config.find_instance_by_gtfs_id(identifier))
            .ok_or_else(|| anyhow!("OTP instance not found for identifier: {}", identifier))
    }

    pub fn get_default_instance(&self) -> &OtpInstance {
        &self.config.default_instance
    }

    pub fn get_all_instances(&self) -> Vec<&OtpInstance> {
        self.config.get_all_instances()
    }
}
