use crate::config::OtpConfig;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct OtpManager {
    instances: HashMap<String, OtpConfig>,
}

impl OtpManager {
    pub fn new(configs: Vec<OtpConfig>) -> Self {
        let instances = configs.into_iter().map(|c| (c.name.clone(), c)).collect();
        Self { instances }
    }

    pub fn get_instance(&self, name: &str) -> Result<&OtpConfig> {
        self.instances
            .get(name)
            .ok_or_else(|| anyhow!("OTP instance not found: {}", name))
    }
}
