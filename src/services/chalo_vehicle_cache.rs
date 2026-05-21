use crate::services::gtfs_service::GTFSService;
use crate::tools::error::{AppError, AppResult};
use chrono::{DateTime, Utc};
use csv::ReaderBuilder;
use once_cell::sync::Lazy;
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use shared::tools::callapi::{call_api, Protocol};
use std::collections::{HashMap, HashSet};
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
    route_name: Option<String>,
    #[serde(rename = "startStopName", default)]
    start_stop_name: Option<String>,
    #[serde(rename = "endStopName", default)]
    end_stop_name: Option<String>,
    #[serde(rename = "tripStartTime", default)]
    trip_start_time: Option<i64>,
    #[serde(rename = "driverId", default)]
    driver_id: Option<String>,
    #[serde(rename = "conductorId", default)]
    conductor_id: Option<String>,
}

// The API returns a direct array, not wrapped

#[derive(Debug, Clone, Serialize)]
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
    pub schedule_trip_id: Option<i64>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub deleted: Option<bool>,
    pub trip_order: Option<i32>,
    pub driver_id: Option<String>,
    pub conductor_id: Option<String>,
    pub db_start_time: Option<String>,
    pub db_end_time: Option<String>,
}

#[derive(Debug, Clone)]
struct CityConfig {
    city_name: String,
    gtfs_id: String,
    api_url: String,
}

/// Single source of truth for CHALO-backed GTFS feeds (city metadata + external API URL).
#[derive(Debug, Clone, Copy)]
pub struct ChaloCityDef {
    pub city_name: &'static str,
    pub gtfs_id: &'static str,
    pub api_url: &'static str,
}

pub const CHALO_CITY_DEFS: [ChaloCityDef; 2] = [
    ChaloCityDef {
        city_name: "bhubaneswar",
        gtfs_id: "bhubaneshwar_bus",
        api_url: "https://external.chalo.com/dashboard/operator-app/bhubaneswar/crut/liveTripsData?mode=bus",
    },
    ChaloCityDef {
        city_name: "sambalpur",
        gtfs_id: "sambalpur_bus",
        api_url: "https://external.chalo.com/dashboard/operator-app/sambalpur/crut/liveTripsData?mode=bus",
    },
];

/// Membership for [`CHALO_CITY_DEFS`], built once per process. The set only changes with a deploy
/// (same as edits to [`CHALO_CITY_DEFS`]), so no runtime invalidation.
static CHALO_GTFS_ID_SET: Lazy<HashSet<&'static str>> =
    Lazy::new(|| CHALO_CITY_DEFS.iter().map(|c| c.gtfs_id).collect());

/// Same order as [`CHALO_CITY_DEFS`]; built once per process (see [`CHALO_GTFS_ID_SET`]).
static CHALO_GTFS_IDS_SLICE: Lazy<Box<[&'static str]>> = Lazy::new(|| {
    CHALO_CITY_DEFS
        .iter()
        .map(|c| c.gtfs_id)
        .collect::<Vec<_>>()
        .into_boxed_slice()
});

pub fn is_chalo_gtfs_id(gtfs_id: &str) -> bool {
    CHALO_GTFS_ID_SET.contains(gtfs_id)
}

/// GTFS IDs backed by CHALO — derived from [`CHALO_CITY_DEFS`], cached for process lifetime.
pub fn chalo_gtfs_ids() -> &'static [&'static str] {
    Lazy::force(&CHALO_GTFS_IDS_SLICE).as_ref()
}

pub struct ChaloVehicleCache {
    // Cache structure: HashMap<gtfs_id, HashMap<vehicle_no, CachedVehicleData>>
    cache: Arc<RwLock<HashMap<String, HashMap<String, CachedVehicleData>>>>,
    city_configs: Vec<CityConfig>,
    external_auth_header: Option<String>,
    update_interval_secs: u64,
    gtfs_service: Arc<GTFSService>,
}

impl ChaloVehicleCache {
    pub fn new(external_auth: Option<String>, gtfs_service: Arc<GTFSService>) -> AppResult<Self> {
        let cache = Arc::new(RwLock::new(HashMap::new()));

        let city_configs = CHALO_CITY_DEFS
            .iter()
            .map(|def| CityConfig {
                city_name: def.city_name.to_string(),
                gtfs_id: def.gtfs_id.to_string(),
                api_url: def.api_url.to_string(),
            })
            .collect();

        Ok(Self {
            cache,
            city_configs,
            external_auth_header: external_auth,
            update_interval_secs: 10, // Default, will be set from config
            gtfs_service,
        })
    }

    fn get_city_config(&self, gtfs_id: &str) -> Option<&CityConfig> {
        self.city_configs
            .iter()
            .find(|config| config.gtfs_id == gtfs_id)
    }

    pub fn set_update_interval(&mut self, interval_secs: u64) {
        self.update_interval_secs = interval_secs;
    }

    pub async fn initialize(&self) -> AppResult<()> {
        info!(
            "Initializing CHALO vehicle cache for {} cities...",
            self.city_configs.len()
        );
        for config in &self.city_configs {
            info!(
                "Initializing cache for {} (gtfs_id: {})",
                config.city_name, config.gtfs_id
            );
            self.update_cache_for_city(config.gtfs_id.as_str()).await?;
        }
        info!("CHALO vehicle cache initialized successfully");
        Ok(())
    }

    pub async fn get_vehicle_data(
        &self,
        gtfs_id: &str,
        vehicle_no: &str,
    ) -> Option<CachedVehicleData> {
        let cache = self.cache.read().await;
        cache
            .get(gtfs_id)
            .and_then(|city_cache| city_cache.get(vehicle_no))
            .cloned()
    }

    pub async fn get_vehicles_by_route_id(
        &self,
        gtfs_id: &str,
        route_id: &str,
    ) -> Vec<CachedVehicleData> {
        let cache = self.cache.read().await;
        cache
            .get(gtfs_id)
            .map(|city_cache| {
                city_cache
                    .iter()
                    .filter_map(|(_, data)| {
                        if data.route_id.as_deref() == Some(route_id) {
                            Some(data.clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn get_all_vehicles_by_gtfs_id(&self, gtfs_id: &str) -> Vec<CachedVehicleData> {
        let cache = self.cache.read().await;
        cache
            .get(gtfs_id)
            .map(|city_cache| city_cache.values().cloned().collect())
            .unwrap_or_default()
    }

    pub async fn update_cache(&self) -> AppResult<()> {
        for config in &self.city_configs {
            if let Err(e) = self.update_cache_for_city(&config.gtfs_id).await {
                error!(
                    "Failed to update cache for {} ({}): {}",
                    config.city_name, config.gtfs_id, e
                );
            }
        }
        Ok(())
    }

    async fn update_cache_for_city(&self, gtfs_id: &str) -> AppResult<()> {
        let city_config = self.get_city_config(gtfs_id).ok_or_else(|| {
            AppError::Internal(format!("No configuration found for gtfs_id: {}", gtfs_id))
        })?;

        info!(
            "Updating CHALO vehicle cache for {} (gtfs_id: {}) from external API...",
            city_config.city_name, gtfs_id
        );

        let url: Url = Url::parse(&city_config.api_url).map_err(|e| {
            error!(
                "Invalid CHALO API URL for {}: {}",
                city_config.city_name, e
            );
        })?;

        let mut headers: Vec<(&str, &str)> = Vec::new();
        if let Some(ref auth_header) = self.external_auth_header {
            headers.push(("externalauth", auth_header.as_str()));
        } else {
            warn!("No external auth header configured for CHALO vehicle cache");
        }

        // The API returns a direct array. `call_api` handles the request, status
        // check, JSON deserialization, Prometheus metrics, and structured logging.
        let vehicles: Vec<ExternalApiVehicle> = call_api::<Vec<ExternalApiVehicle>, ()>(
            Protocol::Http1,
            Method::GET,
            &url,
            headers,
            None,
            Some("chalo_live_trips"),
        )
        .await
        .map_err(|e| {
            error!(
                "Failed to fetch data from external API for {}: {}",
                city_config.city_name, e
            );
            AppError::Internal(format!(
                "Failed to fetch data from external API for {}: {}",
                city_config.city_name, e
            ))
        })?;

        info!(
            "Fetched {} vehicles from external API for {}",
            vehicles.len(),
            city_config.city_name
        );

        // Log a sample vehicle for debugging
        if let Some(sample_vehicle) = vehicles.first() {
            info!(
                "Sample vehicle data for {}: vehicle_no={}, route_id={}, route_name={}, waybill={}",
                city_config.city_name,
                sample_vehicle.vehicle_no,
                sample_vehicle.route_id,
                sample_vehicle.route_name.clone().unwrap_or_default(),
                sample_vehicle.waybill
            );
        }

        // Load route service tier mapping
        let route_mapping = self
            .load_route_service_tier_mapping_internal(gtfs_id)
            .await?;

        let mut cache = self.cache.write().await;
        let city_cache = cache
            .entry(gtfs_id.to_string())
            .or_insert_with(HashMap::new);
        city_cache.clear();

        for vehicle in vehicles {
            // Get service_type from route_service_tier_mapping.csv based on route_id
            let mut service_type: Option<String> = route_mapping.get(&vehicle.route_id).cloned();

            // Fallback: if not found, try to get from static_fleet_info via GTFS service by vehicle number
            if service_type.is_none() {
                service_type = self
                    .gtfs_service
                    .get_fleet_service_type(gtfs_id, &vehicle.vehicle_no)
                    .await;
            }

            // Derive schedule_no from service_type
            // AC -> starts with "Z-", NON_AC -> starts with "OS-"
            let schedule_no = service_type.as_ref().and_then(|st| {
                vehicle.route_name.as_ref().map(|route_name| {
                    let prefix = if st == "AC" { "Z-" } else { "OS-" };
                    format!("{}{}", prefix, route_name)
                })
            });

            // Parse waybill - it's in format "25543126:3", extract the number part
            let waybill_no = vehicle.waybill.split(':').next().map(|s| s.to_string());

            let cached_data = CachedVehicleData {
                vehicle_no: vehicle.vehicle_no.clone(),
                route_id: Some(vehicle.route_id.clone()),
                route_number: vehicle.route_name.clone(),
                waybill_no,
                schedule_no,
                trip_number: None,              // Optional, not provided by API
                is_active_trip: true,           // API only returns active trips
                depot: None,                    // Not provided by API
                last_updated: Some(Utc::now()), // Use current time since API doesn't provide it
                service_type,
                schedule_trip_id: None,
                start_time: None,
                end_time: None,
                deleted: None,
                trip_order: None,
                driver_id: vehicle.driver_id,
                conductor_id: vehicle.conductor_id,
                db_start_time: None,
                db_end_time: None,
            };

            city_cache.insert(vehicle.vehicle_no, cached_data);
        }

        info!(
            "Updated cache for {} with {} vehicles",
            city_config.city_name,
            city_cache.len()
        );
        Ok(())
    }

    async fn load_route_service_tier_mapping_internal(
        &self,
        _gtfs_id: &str,
    ) -> AppResult<HashMap<String, String>> {
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
                    // Filter by gtfs_id if the CSV has a gtfs_id column, otherwise include all
                    // For now, we'll include all records and filter by route_id matching
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
        info!(
            "Starting CHALO vehicle cache background update task for {} cities with interval: {} seconds",
            self.city_configs.len(),
            interval_secs
        );
        loop {
            interval.tick().await;
            if let Err(e) = self.update_cache().await {
                error!("Failed to update CHALO vehicle cache: {}", e);
            }
        }
    }
}
