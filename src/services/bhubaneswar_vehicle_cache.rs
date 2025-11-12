use crate::tools::error::{AppError, AppResult};
use chrono::{DateTime, Utc};
use csv::ReaderBuilder;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouteServiceTierMapping {
    route_id: String,
    service_tier_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExternalApiVehicle {
    #[serde(rename = "vehicleNo")]
    vehicle_no: String,
    #[serde(rename = "routeId")]
    route_id: String,
    #[serde(rename = "waybill")]
    waybill: String,
    #[serde(rename = "routeName")]
    route_name: String,
    #[serde(rename = "startStopName", default)]
    start_stop_name: Option<String>,
    #[serde(rename = "endStopName", default)]
    end_stop_name: Option<String>,
    #[serde(rename = "tripStartTime", default)]
    trip_start_time: Option<i64>,
}

// The API returns a direct array, not wrapped

#[derive(Debug, Clone)]
pub struct CachedVehicleData {
    pub vehicle_no: String,
    pub route_id: Option<String>,
    pub route_number: Option<String>,
    pub waybill_no: Option<String>,
    pub schedule_no: Option<String>,
    pub trip_number: Option<i32>,
    pub is_active_trip: bool,
    pub depot: Option<String>,
    pub last_updated: Option<DateTime<Utc>>,
    pub service_type: Option<String>,
}

pub struct BhubaneswarVehicleCache {
    http_client: Client,
    cache: Arc<RwLock<HashMap<String, CachedVehicleData>>>,
    external_api_url: String,
    external_auth_header: Option<String>,
    update_interval_secs: u64,
}

impl BhubaneswarVehicleCache {
    pub fn new(external_auth: Option<String>) -> AppResult<Self> {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| AppError::Internal(format!("Failed to create HTTP client: {}", e)))?;

        let cache = Arc::new(RwLock::new(HashMap::new()));

        Ok(Self {
            http_client,
            cache,
            external_api_url: "https://external.chalo.com/dashboard/operator-app/bhubaneswar/crut/liveTripsData?mode=bus".to_string(),
            external_auth_header: external_auth,
            update_interval_secs: 10, // Default, will be set from config
        })
    }

    pub fn set_update_interval(&mut self, interval_secs: u64) {
        self.update_interval_secs = interval_secs;
    }

    pub async fn initialize(&self) -> AppResult<()> {
        info!("Initializing Bhubaneswar vehicle cache...");
        self.update_cache().await?;
        info!("Bhubaneswar vehicle cache initialized successfully");
        Ok(())
    }

    pub async fn get_vehicle_data(
        &self,
        vehicle_no: &str,
    ) -> Option<CachedVehicleData> {
        let cache = self.cache.read().await;
        cache.get(vehicle_no).cloned()
    }

    pub async fn update_cache(&self) -> AppResult<()> {
        info!("Updating Bhubaneswar vehicle cache from external API...");
        
        let mut request = self.http_client.get(&self.external_api_url);
        
        if let Some(ref auth_header) = self.external_auth_header {
            request = request.header("externalauth", auth_header);
        } else {
            warn!("No external auth header configured for Bhubaneswar vehicle cache");
        }
        
        let response = request
            .send()
            .await
            .map_err(|e| {
                error!("Failed to fetch data from external API: {}", e);
                AppError::Internal(format!("Failed to fetch data from external API: {}", e))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            error!("External API returned error status: {}", status);
            return Err(AppError::Internal(format!(
                "External API returned error status: {}",
                status
            )));
        }

        // The API returns a direct array
        let vehicles: Vec<ExternalApiVehicle> = response
            .json()
            .await
            .map_err(|e| {
                error!("Failed to parse external API response: {}", e);
                AppError::Internal(format!("Failed to parse external API response: {}", e))
            })?;
        
        info!("Fetched {} vehicles from external API", vehicles.len());
        
        // Log a sample vehicle for debugging
        if let Some(sample_vehicle) = vehicles.first() {
            info!("Sample vehicle data: vehicle_no={}, route_id={}, route_name={}, waybill={}", 
                sample_vehicle.vehicle_no,
                sample_vehicle.route_id,
                sample_vehicle.route_name,
                sample_vehicle.waybill
            );
        }

        // Load route service tier mapping
        let route_mapping = self.load_route_service_tier_mapping_internal().await?;

        let mut cache = self.cache.write().await;
        cache.clear();

        for vehicle in vehicles {
            // Get service_type from route_service_tier_mapping.csv based on route_id
            let service_type = route_mapping.get(&vehicle.route_id).cloned();
            
            // Derive schedule_no from service_type
            // AC -> starts with "Z-", NON_AC -> starts with "OS-"
            let schedule_no = service_type.as_ref().map(|st| {
                let prefix = if st == "AC" { "Z-" } else { "OS-" };
                format!("{}{}", prefix, vehicle.route_name)
            });

            // Parse waybill - it's in format "25543126:3", extract the number part
            let waybill_no = vehicle.waybill.split(':').next().map(|s| s.to_string());

            let cached_data = CachedVehicleData {
                vehicle_no: vehicle.vehicle_no.clone(),
                route_id: Some(vehicle.route_id.clone()),
                route_number: Some(vehicle.route_name.clone()),
                waybill_no,
                schedule_no,
                trip_number: None, // Optional, not provided by API
                is_active_trip: true, // API only returns active trips
                depot: None, // Not provided by API
                last_updated: Some(Utc::now()), // Use current time since API doesn't provide it
                service_type,
            };

            cache.insert(vehicle.vehicle_no, cached_data);
        }

        info!("Updated cache with {} vehicles", cache.len());
        Ok(())
    }

    async fn load_route_service_tier_mapping_internal(&self) -> AppResult<HashMap<String, String>> {
        let file_path = "./assets/route_service_tier_mapping.csv";
        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(e) => {
                warn!("route_service_tier_mapping.csv file not found: {}, proceeding without service tier mapping", e);
                return Ok(HashMap::new());
            }
        };

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to read CSV file: {}", e)))?;

        let mut reader = ReaderBuilder::new()
            .has_headers(true)
            .from_reader(contents.as_bytes());

        let mut mapping = HashMap::new();
        for result in reader.deserialize() {
            match result {
                Ok(record) => {
                    let record: RouteServiceTierMapping = record;
                    mapping.insert(record.route_id, record.service_tier_type);
                }
                Err(e) => {
                    error!("Error parsing CSV row: {}", e);
                }
            }
        }

        Ok(mapping)
    }

    pub async fn start_background_update_task(self: Arc<Self>) {
        let interval_secs = self.update_interval_secs;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        info!("Starting Bhubaneswar vehicle cache background update task with interval: {} seconds", interval_secs);
        loop {
            interval.tick().await;
            if let Err(e) = self.update_cache().await {
                error!("Failed to update Bhubaneswar vehicle cache: {}", e);
            }
        }
    }
}


