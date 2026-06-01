use crate::environment::AppConfig;
use crate::models::{
    cast_vehicle_type, clean_identifier, CachedDataResponse, GTFSData, GTFSRouteData, GTFSStop,
    GTFSStopData, LatLong, NandiPattern, NandiPatternDetails, NandiRoutesRes, PlatformInfo,
    ProviderStopCodeRecord, RouteServiceTierRecord, RouteStopMapping, SeatLayoutMappingRecord,
    ServiceTierType, StaticFleetInfo, StaticFleetInfoRecord, StopGeojson, StopGeojsonRecord,
    StopRegionalNameRecord, SuburbanStopInfo, SuburbanStopInfoRecord,
};
use crate::models::{GTFSAlternateStopData, TripDetails};
use crate::tools::error::{AppError, AppResult};
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use csv::ReaderBuilder;
use futures::future::join_all;
use reqwest::Method;
use serde::Serialize;
use serde_json;
use sha2::{Digest, Sha256};
use shared::call_external_api;
use shared::tools::callapi::{call_api, Protocol};
use shared::tools::prometheus::CALL_EXTERNAL_API;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use url::Url;

fn get_sha256_hash<T: Serialize>(val: &T) -> String {
    let json = serde_json::to_vec(val).unwrap(); // handles f64 fine
    let mut hasher = Sha256::new();
    hasher.update(json);
    format!("{:x}", hasher.finalize())
}

const SENTINEL_CLUSTER_ID: &str = "INVALID_SENTINEL";

fn parse_cluster_id_from_info_json(info_json: Option<&str>) -> Option<String> {
    let raw = info_json?.trim();
    if raw.is_empty() || raw == "{}" {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    let cid = parsed.get("clusterId")?.as_str()?;
    if cid == SENTINEL_CLUSTER_ID {
        return None;
    }
    Some(cid.to_string())
}

fn normalize_stop_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
pub struct GTFSService {
    config: AppConfig,
    data: Arc<ArcSwap<GTFSData>>,
    http_client: reqwest::Client,
    is_ready: Arc<RwLock<bool>>,
    last_update: Arc<RwLock<DateTime<Utc>>>,
    /// Separate cache for lazily-fetched trip details (avoids mutating main Arc<GTFSData>)
    trip_details_cache: Arc<RwLock<HashMap<String, HashMap<String, TripDetails>>>>,
    /// Pre-serialized JSON bytes for the /cached-data endpoint
    cached_data_bytes: Arc<ArcSwap<Vec<u8>>>,
}

impl GTFSService {
    pub async fn new(config: AppConfig) -> AppResult<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(config.connection_limit)
            .pool_idle_timeout(Duration::from_secs(config.http_pool_idle_timeout))
            .tcp_keepalive(Some(Duration::from_secs(config.http_tcp_keepalive)))
            .tcp_nodelay(true) // Disable Nagle's algorithm for lower latency
            .local_address(None) // Allow system to choose optimal local address
            .build()
            .map_err(|e| AppError::Internal(format!("Failed to create HTTP client: {}", e)))?;

        let service = Self {
            config,
            data: Arc::new(ArcSwap::from_pointee(GTFSData::new())),
            http_client,
            is_ready: Arc::new(RwLock::new(false)),
            last_update: Arc::new(RwLock::new(Utc::now())),
            trip_details_cache: Arc::new(RwLock::new(HashMap::new())),
            cached_data_bytes: Arc::new(ArcSwap::from_pointee(Vec::new())),
        };

        service.load_initial_data().await?;

        Ok(service)
    }

    async fn load_initial_data(&self) -> AppResult<()> {
        info!("Loading initial GTFS data...");
        let start_time = std::time::Instant::now();

        let temp_data = self.fetch_and_process_data().await?;

        self.data.store(Arc::new(temp_data));

        // Pre-serialize cached data while service is still loading
        self.update_cached_data_bytes().await;

        let mut is_ready = self.is_ready.write().await;
        *is_ready = true;

        let mut last_update = self.last_update.write().await;
        *last_update = Utc::now();

        let duration = start_time.elapsed();
        info!("Initial data load complete in {:?}", duration);
        Ok(())
    }

    async fn fetch_and_process_data(&self) -> AppResult<GTFSData> {
        let mut temp_data = GTFSData::new();
        let mut all_pattern_details = Vec::new();
        let mut all_routes = Vec::new();
        let mut all_stops = Vec::new();
        let mut already_visited: HashSet<String> = HashSet::new();

        for otp_instance in self.config.otp_instances.get_all_instances() {
            let base_url = &otp_instance.url;
            if !already_visited.insert(base_url.to_string()) {
                continue;
            }
            let patterns = self.fetch_patterns(base_url).await?;
            let pattern_details = self
                .fetch_pattern_details_batch(base_url, &patterns)
                .await?;
            all_pattern_details.extend(pattern_details);
            all_routes.extend(self.fetch_routes(base_url).await?);
            all_stops.extend(self.fetch_stops(base_url).await?);
        }
        info!("Fetched {} patterns", all_pattern_details.len());

        // Read stop geojsons CSV file
        let stop_geojsons_by_gtfs = self.read_stop_geojsons_csv().await?;
        info!(
            "Loaded {} stop geojsons from CSV",
            stop_geojsons_by_gtfs.len()
        );

        let provider_stop_code_mapping = self.read_provider_stop_code_mapping_csv().await?;
        info!(
            "Loaded {} provider stop code mappings from CSV",
            provider_stop_code_mapping.len()
        );

        // Read stop regional names CSV file
        let stop_regional_names_by_gtfs = self.read_stop_regional_names_csv().await?;
        info!(
            "Loaded {} stop regional names from CSV",
            stop_regional_names_by_gtfs.len()
        );

        // Read suburban stop info CSV file
        let suburban_stop_info_by_gtfs = self.read_suburban_stop_info_csv().await?;
        info!(
            "Loaded suburban stop info for {} GTFS IDs from CSV",
            suburban_stop_info_by_gtfs.len()
        );

        // Read static fleet info CSV file (optional)
        let static_fleet_info_by_gtfs = self.read_static_fleet_info_csv().await?;
        if !static_fleet_info_by_gtfs.is_empty() {
            info!(
                "Loaded static fleet info for {} GTFS IDs from CSV",
                static_fleet_info_by_gtfs.len()
            );
        } else {
            info!("No static fleet info loaded from CSV");
        }

        let route_service_tiers_by_gtfs = self.read_route_service_tiers_csv().await?;
        info!(
            "Loaded route service tiers for {} GTFS IDs from CSV",
            route_service_tiers_by_gtfs.len()
        );

        let seat_layout_mapping_by_gtfs = self.read_seat_layout_mapping_csv().await?;
        info!(
            "Loaded seat layout mappings for {} GTFS IDs from CSV",
            seat_layout_mapping_by_gtfs.len()
        );

        // Calculate trip counts
        let route_trip_counts = self.calculate_trip_counts(&all_pattern_details);

        // Calculate stop counts
        let route_stop_counts = self.calculate_stop_counts(&all_pattern_details);

        // Fetch routes
        let mut routes_by_gtfs = self.build_routes_by_gtfs(
            all_routes,
            &route_trip_counts,
            &route_stop_counts,
            &route_service_tiers_by_gtfs,
        );

        // Build stops data first (needed by route data for parent_stop_code lookup)
        let stops_by_gtfs =
            self.build_stops_by_gtfs(all_stops.clone(), &stop_regional_names_by_gtfs);

        let alternate_stops_by_gtfs = self.build_alternate_stops_by_gtfs(all_stops.clone());

        // Build route data
        let route_data_by_gtfs = self.build_route_data(
            &all_pattern_details,
            &routes_by_gtfs,
            &stop_geojsons_by_gtfs,
            &provider_stop_code_mapping,
            &stop_regional_names_by_gtfs,
            &suburban_stop_info_by_gtfs,
            &stops_by_gtfs,
        );

        // Fetch example trip mapping per route for all GTFS feeds
        let route_example_trip_by_gtfs = self.fetch_route_example_trip_for_all_feeds().await?;

        // Update start and end points
        self.update_start_end_points(&mut routes_by_gtfs, &route_data_by_gtfs);

        // Fetch stops and build children mapping
        let children_by_parent = self.build_children_mapping(all_stops);

        // Compute data hashes
        let data_hash = self.compute_all_data_hashes(&routes_by_gtfs);

        // Pre-compute unique stops list per GTFS feed (avoids recomputation on /stops requests)
        let pre_computed_stops_by_gtfs =
            Self::pre_compute_stops(&route_data_by_gtfs, &stop_regional_names_by_gtfs);

        temp_data.route_data_by_gtfs = route_data_by_gtfs;
        temp_data.stops_by_gtfs = stops_by_gtfs;
        temp_data.routes_by_gtfs = routes_by_gtfs;
        temp_data.children_by_parent = children_by_parent;
        temp_data.data_hash = data_hash;
        temp_data.stop_geojsons_by_gtfs = stop_geojsons_by_gtfs;
        temp_data.provider_stop_code_mapping = provider_stop_code_mapping;
        temp_data.stop_regional_names_by_gtfs = stop_regional_names_by_gtfs;
        temp_data.suburban_stop_info_by_gtfs = suburban_stop_info_by_gtfs;
        temp_data.static_fleet_info_by_gtfs = static_fleet_info_by_gtfs;
        temp_data.route_example_trip_by_gtfs = route_example_trip_by_gtfs;
        temp_data.alternate_stop_by_gtfs = alternate_stops_by_gtfs;
        temp_data.route_service_tiers_by_gtfs = route_service_tiers_by_gtfs;
        temp_data.seat_layout_mapping_by_gtfs = seat_layout_mapping_by_gtfs;
        temp_data.pre_computed_stops_by_gtfs = pre_computed_stops_by_gtfs;

        let mem_stats = temp_data.memory_usage_bytes();
        info!(
            "GTFS data memory usage: ~{:.1} MB (routes: {:.1} KB, route_data: {:.1} KB, stops: {:.1} KB, geojson: {:.1} KB)",
            mem_stats.total_bytes as f64 / 1_048_576.0,
            mem_stats.routes_bytes as f64 / 1024.0,
            mem_stats.route_data_bytes as f64 / 1024.0,
            mem_stats.stops_bytes as f64 / 1024.0,
            mem_stats.geojson_bytes as f64 / 1024.0,
        );

        Ok(temp_data)
    }

    async fn read_stop_geojsons_csv(
        &self,
    ) -> AppResult<HashMap<String, HashMap<String, StopGeojson>>> {
        let file_path = "./assets/stop_geojsons.csv";

        // Check if file exists, if not return empty HashMap
        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                warn!("stop_geojsons.csv file not found, proceeding without geojson data");
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

        let mut stop_geojsons_by_gtfs = HashMap::new();
        for result in reader.deserialize() {
            match result {
                Ok(record) => {
                    let geojson: StopGeojsonRecord = record;
                    let inner = stop_geojsons_by_gtfs
                        .entry(geojson.gtfs_id.clone())
                        .or_insert_with(HashMap::new);
                    inner.insert(
                        geojson.stop_code.clone(),
                        StopGeojson {
                            geo_json: geojson.geo_json.clone(),
                            gates: geojson.gates.clone(),
                        },
                    );
                }
                Err(e) => {
                    error!("Error parsing CSV row: {}", e);
                }
            }
        }
        Ok(stop_geojsons_by_gtfs)
    }

    async fn read_provider_stop_code_mapping_csv(
        &self,
    ) -> AppResult<HashMap<String, HashMap<String, String>>> {
        let file_path = "./assets/stop_provider_mapping.csv";

        // Check if file exists, if not return empty HashMap
        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                warn!("stop_provider_mapping.csv file not found, proceeding without provider stop code mapping data");
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

        let mut mapping: HashMap<String, HashMap<String, String>> = HashMap::new();
        for result in reader.deserialize() {
            match result {
                Ok(record) => {
                    let record: ProviderStopCodeRecord = record;
                    mapping
                        .entry(record.gtfs_id)
                        .or_default()
                        .insert(record.provider_stop_code, record.stop_code);
                }
                Err(e) => {
                    error!("Error parsing CSV row: {}", e);
                }
            }
        }

        Ok(mapping)
    }

    async fn read_stop_regional_names_csv(
        &self,
    ) -> AppResult<HashMap<String, HashMap<String, StopRegionalNameRecord>>> {
        let file_path = "./assets/stop_regional_names.csv";

        // Check if file exists, if not return empty HashMap
        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                warn!("stop_regional_names.csv file not found, proceeding without regional names data");
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

        let mut stop_regional_names_by_gtfs = HashMap::new();
        for result in reader.deserialize() {
            match result {
                Ok(record) => {
                    let regional_name_record: StopRegionalNameRecord = record;
                    let inner = stop_regional_names_by_gtfs
                        .entry(regional_name_record.gtfs_id.clone())
                        .or_insert_with(HashMap::new);
                    inner.insert(regional_name_record.stop_code.clone(), regional_name_record);
                }
                Err(e) => {
                    error!("Error parsing CSV row: {}", e);
                }
            }
        }
        Ok(stop_regional_names_by_gtfs)
    }

    async fn read_suburban_stop_info_csv(
        &self,
    ) -> AppResult<HashMap<String, HashMap<String, SuburbanStopInfo>>> {
        let file_path = "./assets/suburban_stop_info.csv";

        // Check if file exists, if not return empty HashMap
        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                warn!("suburban_stop_info.csv file not found, proceeding without suburban stop info data");
                return Ok(HashMap::new());
            }
        };

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to read CSV file: {}", e)))?;

        // Use standard CSV reader since the file is now properly formatted
        let mut reader = ReaderBuilder::new()
            .has_headers(true)
            .from_reader(contents.as_bytes());

        let mut suburban_stop_info_by_gtfs = HashMap::new();

        for result in reader.deserialize() {
            match result {
                Ok(record) => {
                    let csv_record: SuburbanStopInfoRecord = record;

                    // Parse the platforms JSON string
                    let platforms: Vec<PlatformInfo> = if csv_record.platforms == "[]" {
                        Vec::new()
                    } else {
                        match serde_json::from_str(&csv_record.platforms) {
                            Ok(platforms) => platforms,
                            Err(e) => {
                                error!(
                                    "Error parsing platforms JSON for stop {}: {}",
                                    csv_record.stop_id, e
                                );
                                Vec::new()
                            }
                        }
                    };

                    let suburban_stop_info = SuburbanStopInfo {
                        stop_id: csv_record.stop_id.clone(),
                        location_name: csv_record.location_name,
                        platforms,
                    };

                    // Use the gtfs_id from the CSV record
                    let gtfs_id = csv_record.gtfs_id.clone();

                    let inner = suburban_stop_info_by_gtfs
                        .entry(gtfs_id)
                        .or_insert_with(HashMap::new);
                    inner.insert(csv_record.stop_id, suburban_stop_info);
                }
                Err(e) => {
                    error!("Error parsing CSV row: {}", e);
                }
            }
        }
        Ok(suburban_stop_info_by_gtfs)
    }

    async fn read_static_fleet_info_csv(
        &self,
    ) -> AppResult<HashMap<String, HashMap<String, StaticFleetInfo>>> {
        let file_path = "./assets/static_fleet_info.csv";

        // Check if file exists, if not return empty HashMap
        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                warn!("static_fleet_info.csv file not found, proceeding without fleet info data");
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

        let mut static_fleet_info_by_gtfs = HashMap::new();

        for result in reader.deserialize() {
            match result {
                Ok(record) => {
                    let csv_record: StaticFleetInfoRecord = record;
                    let fleet_info = StaticFleetInfo {
                        fleet_id: csv_record.fleet_id.clone(),
                        vehicle_type: csv_record.vehicle_type,
                        capacity: csv_record.capacity,
                        depot: csv_record.depot,
                        service_type: csv_record.service_type,
                    };
                    let inner = static_fleet_info_by_gtfs
                        .entry(csv_record.gtfs_id)
                        .or_insert_with(HashMap::new);
                    inner.insert(fleet_info.fleet_id.clone(), fleet_info);
                }
                Err(e) => {
                    error!("Error parsing static fleet info CSV row: {}", e);
                }
            }
        }
        Ok(static_fleet_info_by_gtfs)
    }

    async fn read_route_service_tiers_csv(
        &self,
    ) -> AppResult<HashMap<String, HashMap<String, ServiceTierType>>> {
        let file_path = "./assets/route_service_tiers.csv";

        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                warn!("route_service_tiers.csv file not found, proceeding without route service tiers");
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

        let mut route_service_tiers_by_gtfs = HashMap::new();

        for result in reader.deserialize() {
            match result {
                Ok(record) => {
                    let csv_record: RouteServiceTierRecord = record;
                    let inner = route_service_tiers_by_gtfs
                        .entry(csv_record.gtfs_id)
                        .or_insert_with(HashMap::new);
                    inner.insert(csv_record.route_id, csv_record.servicetier);
                }
                Err(e) => {
                    error!("Error parsing route service tiers CSV row: {}", e);
                }
            }
        }
        Ok(route_service_tiers_by_gtfs)
    }

    async fn read_seat_layout_mapping_csv(
        &self,
    ) -> AppResult<HashMap<String, HashMap<String, String>>> {
        let file_path = "./assets/seat_layout_mapping.csv";

        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                warn!("seat_layout_mapping.csv file not found, proceeding without seat layout mapping data");
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

        let mut seat_layout_mapping_by_gtfs = HashMap::new();

        for result in reader.deserialize() {
            match result {
                Ok(record) => {
                    let csv_record: SeatLayoutMappingRecord = record;
                    let inner = seat_layout_mapping_by_gtfs
                        .entry(csv_record.gtfs_id)
                        .or_insert_with(HashMap::new);
                    inner.insert(csv_record.fleet_id, csv_record.seat_layout_id);
                }
                Err(e) => {
                    error!("Error parsing seat layout mapping CSV row: {}", e);
                }
            }
        }
        Ok(seat_layout_mapping_by_gtfs)
    }

    async fn fetch_pattern_details_batch(
        &self,
        base_url: &str,
        patterns: &[NandiPattern],
    ) -> AppResult<Vec<NandiPatternDetails>> {
        let mut pattern_details = Vec::new();
        let chunks = patterns.chunks(self.config.process_batch_size);

        for chunk in chunks {
            let futures = chunk
                .iter()
                .map(|p| self.fetch_pattern_details(base_url, &p.id));
            let results = join_all(futures).await;

            for result in results {
                match result {
                    Ok(details) => pattern_details.push(details),
                    Err(e) => error!("Error fetching pattern details: {}", e),
                }
            }
        }
        Ok(pattern_details)
    }

    fn calculate_trip_counts(
        &self,
        pattern_details: &[NandiPatternDetails],
    ) -> HashMap<String, i32> {
        let mut counts = HashMap::new();
        for details in pattern_details {
            let route_code = details
                .route_id
                .split(':')
                .next_back()
                .unwrap_or(&details.route_id);
            *counts.entry(route_code.to_string()).or_insert(0) += details.trips.len() as i32;
        }
        counts
    }

    fn calculate_stop_counts(
        &self,
        pattern_details: &[NandiPatternDetails],
    ) -> HashMap<String, HashMap<String, usize>> {
        let mut counts: HashMap<String, HashMap<String, HashSet<String>>> = HashMap::new();
        for details in pattern_details {
            let parts: Vec<&str> = details.route_id.split(':').collect();
            if parts.len() < 2 {
                continue;
            }
            let gtfs_id = parts[0];
            let route_code = parts[1];

            let stop_codes = details
                .stops
                .iter()
                .map(|s| s.code.clone())
                .collect::<HashSet<String>>();
            counts
                .entry(gtfs_id.to_string())
                .or_default()
                .entry(route_code.to_string())
                .or_default()
                .extend(stop_codes);
        }
        counts
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().map(|(k2, v2)| (k2, v2.len())).collect()))
            .collect()
    }

    fn build_routes_by_gtfs(
        &self,
        routes: Vec<NandiRoutesRes>,
        trip_counts: &HashMap<String, i32>,
        stop_counts: &HashMap<String, HashMap<String, usize>>,
        route_service_tiers: &HashMap<String, HashMap<String, ServiceTierType>>,
    ) -> HashMap<String, HashMap<String, NandiRoutesRes>> {
        let mut routes_by_gtfs: HashMap<String, HashMap<String, NandiRoutesRes>> = HashMap::new();
        for route in routes {
            let parts: Vec<&str> = route.id.split(':').collect();
            if parts.len() < 2 {
                continue;
            }
            let gtfs_id = parts[0];
            let route_code = parts[1];

            let service_tier_type = route_service_tiers
                .get(gtfs_id)
                .and_then(|m| m.get(route_code))
                .cloned();

            let route_res = NandiRoutesRes {
                id: route_code.to_string(),
                short_name: route.short_name,
                long_name: route.long_name,
                mode: cast_vehicle_type(&route.mode),
                agency_name: route.agency_name,
                trip_count: trip_counts.get(route_code).copied(),
                stop_count: stop_counts
                    .get(gtfs_id)
                    .and_then(|r| r.get(route_code))
                    .copied()
                    .map(|c| c as i32),
                start_point: None,
                end_point: None,
                service_tier_type,
            };
            routes_by_gtfs
                .entry(gtfs_id.to_string())
                .or_default()
                .insert(route_code.to_string(), route_res);
        }
        routes_by_gtfs
    }

    fn build_alternate_stops_by_gtfs(
        &self,
        stops: Vec<GTFSStop>,
    ) -> HashMap<String, GTFSAlternateStopData> {
        let mut grouped: HashMap<(String, String), Vec<String>> = HashMap::new();
        let mut result: HashMap<String, GTFSAlternateStopData> = HashMap::new();
        for stop in stops {
            let (gtfs_id, stop_id) = match stop.id.split_once(':') {
                Some(v) => v,
                None => continue,
            };

            let normalized_name = normalize_stop_name(&stop.name);

            grouped
                .entry((gtfs_id.to_string(), normalized_name))
                .or_default()
                .push(stop_id.to_string());
        }

        for ((gtfs_id, _name), stop_ids) in grouped {
            if stop_ids.len() < 2 {
                continue;
            }

            let entry = result.entry(gtfs_id).or_default();

            for stop_id in &stop_ids {
                let alternates = stop_ids
                    .iter()
                    .filter(|id| *id != stop_id)
                    .cloned()
                    .collect();

                entry.alternate_stops.insert(stop_id.clone(), alternates);
            }
        }
        result
    }

    fn build_stops_by_gtfs(
        &self,
        stops: Vec<GTFSStop>,
        stop_regional_names_by_gtfs: &HashMap<String, HashMap<String, StopRegionalNameRecord>>,
    ) -> HashMap<String, GTFSStopData> {
        let mut stops_by_gtfs: HashMap<String, GTFSStopData> = HashMap::new();
        let mut by_cluster_set: HashMap<String, HashMap<String, HashSet<String>>> = HashMap::new();

        for stop in stops {
            let parts: Vec<&str> = stop.id.split(':').collect();
            if parts.len() < 2 {
                continue;
            }
            let gtfs_id = parts[0];
            let stop_code = parts[1];

            let stop_data = stops_by_gtfs.entry(gtfs_id.to_string()).or_default();
            let regional_name = stop_regional_names_by_gtfs
                .get(gtfs_id)
                .and_then(|m| m.get(stop_code));

            let cluster_id = parse_cluster_id_from_info_json(stop.info_json.as_deref());

            // Create a new GTFSStop with the clean stop code
            let stop_res = GTFSStop {
                id: stop.id.clone(),
                code: stop.code.clone(),
                name: stop.name.clone(),
                lat: stop.lat,
                lon: stop.lon,
                station_id: stop.station_id.clone(),
                cluster: stop.cluster.clone(),
                hindi_name: regional_name.map(|r| r.hindi_name.clone()),
                regional_name: regional_name.map(|r| r.regional_name.clone()),
                info_json: None,
                cluster_id: cluster_id.clone(),
            };
            if stop.cluster.is_some() {
                let cluster_stop_res = GTFSStop {
                    id: stop.cluster.clone().unwrap(),
                    code: stop.code.clone(),
                    name: stop.name.clone(),
                    lat: stop.lat,
                    lon: stop.lon,
                    station_id: stop.station_id.clone(),
                    cluster: stop.cluster.clone(),
                    hindi_name: regional_name.map(|r| r.hindi_name.clone()),
                    regional_name: regional_name.map(|r| r.regional_name.clone()),
                    info_json: None,
                    cluster_id: None,
                };
                stop_data
                    .stops
                    .insert(stop.cluster.clone().unwrap(), cluster_stop_res);
            }

            if let Some(cid) = &cluster_id {
                by_cluster_set
                    .entry(gtfs_id.to_string())
                    .or_default()
                    .entry(cid.clone())
                    .or_default()
                    .insert(stop_code.to_string());
            }

            stop_data.stops.insert(stop_code.to_string(), stop_res);
        }

        for (gtfs_id, per_cluster) in by_cluster_set {
            if let Some(stop_data) = stops_by_gtfs.get_mut(&gtfs_id) {
                for (cid, codes) in per_cluster {
                    let mut v: Vec<String> = codes.into_iter().collect();
                    v.sort();
                    stop_data.by_cluster_id.insert(cid, v);
                }
            }
        }

        for (gtfs_id, data) in &stops_by_gtfs {
            let total = data.stops.len();
            let clustered = data
                .stops
                .values()
                .filter(|s| s.cluster_id.is_some())
                .count();
            if clustered == 0 {
                warn!(
                    gtfs_id = %gtfs_id,
                    total_stops = total,
                    "No stops have a cluster_id; cluster destinations endpoint will fall back to single-stop walks for every request",
                );
            } else {
                info!(
                    gtfs_id = %gtfs_id,
                    total_stops = total,
                    clustered_stops = clustered,
                    distinct_clusters = data.by_cluster_id.len(),
                    "cluster_id coverage after build",
                );
            }
        }

        stops_by_gtfs
    }

    #[allow(clippy::too_many_arguments)]
    fn build_route_data(
        &self,
        pattern_details: &[NandiPatternDetails],
        routes_by_gtfs: &HashMap<String, HashMap<String, NandiRoutesRes>>,
        stop_geojsons_by_gtfs: &HashMap<String, HashMap<String, StopGeojson>>,
        provider_stop_code_mapping: &HashMap<String, HashMap<String, String>>,
        stop_regional_names_by_gtfs: &HashMap<String, HashMap<String, StopRegionalNameRecord>>,
        suburban_stop_info_by_gtfs: &HashMap<String, HashMap<String, SuburbanStopInfo>>,
        stops_by_gtfs: &HashMap<String, GTFSStopData>,
    ) -> HashMap<String, GTFSRouteData> {
        // Pre-compute reverse lookup: gtfs_id -> (stop_code -> provider_stop_code)
        // Eliminates O(n) linear scan per stop in the inner loop
        let reverse_provider_map: HashMap<&str, HashMap<&str, &str>> = provider_stop_code_mapping
            .iter()
            .map(|(gtfs_id, mapping)| {
                let reverse: HashMap<&str, &str> = mapping
                    .iter()
                    .map(|(prov, stop)| (stop.as_str(), prov.as_str()))
                    .collect();
                (gtfs_id.as_str(), reverse)
            })
            .collect();

        let mut route_data_by_gtfs: HashMap<String, GTFSRouteData> = HashMap::new();

        // Group patterns by route to find the longest pattern for each route
        let mut patterns_by_route: HashMap<String, Vec<&NandiPatternDetails>> = HashMap::new();
        for pattern in pattern_details {
            let parts: Vec<&str> = pattern.route_id.split(':').collect();
            if parts.len() < 2 {
                continue;
            }
            let gtfs_id = parts[0];
            let route_code = parts[1];
            let route_key = format!("{}:{}", gtfs_id, route_code);

            patterns_by_route
                .entry(route_key)
                .or_default()
                .push(pattern);
        }

        // Process only the longest pattern for each route
        for (_route_key, patterns) in patterns_by_route {
            // Find the pattern with the most stops
            let longest_pattern = patterns
                .iter()
                .max_by_key(|pattern| pattern.stops.len())
                .unwrap();

            let parts: Vec<&str> = longest_pattern.route_id.split(':').collect();
            let gtfs_id = parts[0];
            let route_code = parts[1];

            // Intern vehicle_type and route_code: all mappings for this route share one Arc
            let vehicle_type: Arc<str> = routes_by_gtfs
                .get(gtfs_id)
                .and_then(|r| r.get(route_code))
                .map(|route| Arc::from(route.mode.as_str()))
                .unwrap_or_else(|| Arc::from("UNKNOWN"));
            let route_code_arc: Arc<str> = Arc::from(route_code);

            let route_data = route_data_by_gtfs.entry(gtfs_id.to_string()).or_default();
            let mut visited_mapping: HashSet<String> = HashSet::new();

            for (seq, stop) in longest_pattern.stops.iter().enumerate() {
                let stop_geojson = stop_geojsons_by_gtfs
                    .get(gtfs_id)
                    .and_then(|g| g.get(&stop.code))
                    .cloned();

                // Find provider stop code via O(1) reverse lookup
                let provider_code: Arc<str> = reverse_provider_map
                    .get(gtfs_id)
                    .and_then(|m| m.get(stop.code.as_str()))
                    .map(|s| Arc::from(*s))
                    .unwrap_or_else(|| Arc::from("GTFS"));

                // Get platform from suburban stop info if available
                let platform: Option<Arc<str>> = suburban_stop_info_by_gtfs
                    .get(gtfs_id)
                    .and_then(|stops| stops.get(&stop.code))
                    .and_then(|suburban_stop| {
                        suburban_stop
                            .platforms
                            .first()
                            .map(|platform_info| Arc::from(platform_info.platforms.as_str()))
                    });

                let mapping = Arc::new(RouteStopMapping {
                    estimated_travel_time_from_previous_stop: None,
                    provider_code,
                    route_code: route_code_arc.clone(),
                    sequence_num: (seq + 1) as i32,
                    stop_code: Arc::from(stop.code.as_str()),
                    stop_name: Arc::from(stop.name.as_str()),
                    stop_point: LatLong {
                        lat: stop.lat,
                        lon: stop.lon,
                    },
                    parent_stop_code: stops_by_gtfs
                        .get(gtfs_id)
                        .and_then(|stops_data| stops_data.stops.get(&stop.code))
                        .and_then(|gtfs_stop| gtfs_stop.station_id.as_ref())
                        .and_then(|station_id| station_id.split(':').next_back())
                        .filter(|s| !s.is_empty())
                        .map(Arc::from),
                    vehicle_type: vehicle_type.clone(),
                    geo_json: stop_geojson.as_ref().map(|s| s.geo_json.clone()),
                    gates: stop_geojson.as_ref().and_then(|s| s.gates.clone()),
                    hindi_name: stop_regional_names_by_gtfs
                        .get(gtfs_id)
                        .and_then(|m| m.get(&stop.code))
                        .map(|r| Arc::from(r.hindi_name.as_str())),
                    regional_name: stop_regional_names_by_gtfs
                        .get(gtfs_id)
                        .and_then(|m| m.get(&stop.code))
                        .map(|r| Arc::from(r.regional_name.as_str())),
                    platform,
                });
                let hash = get_sha256_hash(&mapping);
                if !visited_mapping.insert(hash) {
                    continue;
                }

                let mapping_idx = route_data.mappings.len();

                route_data.mappings.push(mapping);

                route_data
                    .by_route
                    .entry(route_code.to_string())
                    .or_default()
                    .push(mapping_idx);
                route_data
                    .by_stop
                    .entry(stop.code.clone())
                    .or_default()
                    .push(mapping_idx);
            }
        }
        route_data_by_gtfs
    }

    fn update_start_end_points(
        &self,
        routes_by_gtfs: &mut HashMap<String, HashMap<String, NandiRoutesRes>>,
        route_data_by_gtfs: &HashMap<String, GTFSRouteData>,
    ) {
        for (gtfs_id, routes) in routes_by_gtfs.iter_mut() {
            if let Some(route_data) = route_data_by_gtfs.get(gtfs_id) {
                for (route_code, route) in routes.iter_mut() {
                    if let Some(indices) = route_data.by_route.get(route_code) {
                        if let Some(&first_idx) = indices.first() {
                            if let Some(first_stop) = route_data.mappings.get(first_idx) {
                                route.start_point = Some(first_stop.stop_point.clone());
                            }
                        }
                        if let Some(&last_idx) = indices.last() {
                            if let Some(last_stop) = route_data.mappings.get(last_idx) {
                                route.end_point = Some(last_stop.stop_point.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    fn build_children_mapping(
        &self,
        stops: Vec<GTFSStop>,
    ) -> HashMap<String, HashMap<String, HashSet<String>>> {
        let mut children_by_parent: HashMap<String, HashMap<String, HashSet<String>>> =
            HashMap::new();
        for stop in stops {
            if let Some(station_id) = &stop.station_id {
                let gtfs_id = stop.id.split(':').next().unwrap_or_default();
                let stop_code = stop.id.split(':').next_back().unwrap_or_default();
                let parent_code = station_id.split(':').next_back().unwrap_or_default();
                if !gtfs_id.is_empty() && !stop_code.is_empty() && !parent_code.is_empty() {
                    children_by_parent
                        .entry(gtfs_id.to_string())
                        .or_default()
                        .entry(parent_code.to_string())
                        .or_default()
                        .insert(stop_code.to_string());
                }
            }
        }
        children_by_parent
    }

    /// Pre-compute unique stops list per GTFS feed with regional names populated.
    /// This avoids cloning and enriching on every /stops API request.
    fn pre_compute_stops(
        route_data_by_gtfs: &HashMap<String, GTFSRouteData>,
        stop_regional_names_by_gtfs: &HashMap<String, HashMap<String, StopRegionalNameRecord>>,
    ) -> HashMap<String, Vec<Arc<RouteStopMapping>>> {
        let mut result = HashMap::new();
        for (gtfs_id, route_data) in route_data_by_gtfs {
            let regional_names = stop_regional_names_by_gtfs.get(gtfs_id);
            let mut stops = Vec::with_capacity(route_data.by_stop.len());
            for indices in route_data.by_stop.values() {
                if let Some(&i) = indices.first() {
                    if let Some(mapping) = route_data.mappings.get(i) {
                        // Check if we need to enrich with regional names
                        if let Some(regional_record) =
                            regional_names.and_then(|names| names.get(&*mapping.stop_code))
                        {
                            let mut enriched = (**mapping).clone();
                            enriched.hindi_name =
                                Some(Arc::from(regional_record.hindi_name.as_str()));
                            enriched.regional_name =
                                Some(Arc::from(regional_record.regional_name.as_str()));
                            stops.push(Arc::new(enriched));
                        } else {
                            stops.push(mapping.clone());
                        }
                    }
                }
            }
            result.insert(gtfs_id.clone(), stops);
        }
        result
    }

    fn compute_all_data_hashes(
        &self,
        routes_by_gtfs: &HashMap<String, HashMap<String, NandiRoutesRes>>,
    ) -> HashMap<String, String> {
        routes_by_gtfs
            .iter()
            .map(|(gtfs_id, routes)| (gtfs_id.clone(), self.compute_data_hash(routes)))
            .collect()
    }

    pub async fn start_polling(&self) -> AppResult<()> {
        info!("Starting GTFS data polling...");
        loop {
            sleep(Duration::from_secs(self.config.polling_interval)).await;
            match self.update_data().await {
                Ok(_) => debug!("Data update completed successfully"),
                Err(e) => error!("Error updating data: {}", e),
            }
        }
    }

    async fn update_data(&self) -> AppResult<()> {
        info!("Checking for GTFS data updates...");
        let start_time = std::time::Instant::now();
        match self.fetch_and_process_data().await {
            Ok(new_data) => {
                if self.check_for_changes(&new_data).await? {
                    info!("Changes detected, performing atomic update...");

                    // Atomic pointer swap - readers continue unblocked with old data
                    self.data.store(Arc::new(new_data));

                    // Clear trip details cache since underlying data changed
                    self.trip_details_cache.write().await.clear();

                    // Update pre-serialized cached data bytes
                    self.update_cached_data_bytes().await;

                    let mut last_update = self.last_update.write().await;
                    *last_update = Utc::now();
                    let duration = start_time.elapsed();
                    info!("Data updated atomically in {:?}", duration);

                    let mut is_ready = self.is_ready.write().await;
                    if !*is_ready {
                        *is_ready = true;
                        info!("Service is now ready.");
                    }
                } else {
                    info!("No changes in GTFS data detected. Skipping update.");
                }
                Ok(())
            }
            Err(e) => {
                error!("Failed to fetch and process data: {}", e);
                Err(e)
            }
        }
    }

    async fn check_for_changes(&self, new_data: &GTFSData) -> AppResult<bool> {
        let current_data = self.data.load_full();
        if new_data.data_hash.len() != current_data.data_hash.len() {
            return Ok(true);
        }

        for (gtfs_id, new_hash) in &new_data.data_hash {
            if let Some(current_hash) = current_data.data_hash.get(gtfs_id) {
                if new_hash != current_hash {
                    return Ok(true);
                }
            } else {
                return Ok(true); // New GTFS ID found
            }
        }
        Ok(false)
    }

    fn compute_data_hash(&self, data: &HashMap<String, NandiRoutesRes>) -> String {
        let btree_map: BTreeMap<_, _> = data.iter().collect();
        let json = serde_json::to_string(&btree_map).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(json.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    async fn fetch_with_retry<T>(&self, url_str: &str, service: &str) -> AppResult<T>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        let start_time = std::time::Instant::now();
        let method = "GET";
        let host = Url::parse(url_str)
            .ok()
            .and_then(|url| url.host_str().map(|s| s.to_string()))
            .unwrap_or(url_str.to_string());
        for attempt in 0..self.config.max_retries {
            match self.http_client.get(url_str).send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        call_external_api!(
                            method,
                            host.as_str(),
                            service,
                            status.as_str(),
                            start_time
                        );
                        return response.json::<T>().await.map_err(|e| {
                            AppError::Internal(format!("Failed to deserialize response: {}", e))
                        });
                    } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        let retry_after = response
                            .headers()
                            .get("Retry-After")
                            .and_then(|h| h.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(self.config.retry_delay);
                        warn!("Rate limited, waiting {} seconds", retry_after);
                        sleep(Duration::from_secs(retry_after)).await;
                    } else {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        error!(
                            "HTTP request to URL {}/{} failed with status {}: {}",
                            host, service, status, body
                        );
                        call_external_api!(
                            method,
                            host.as_str(),
                            service,
                            status.as_str(),
                            start_time
                        );
                        return Err(AppError::Internal(format!(
                            "HTTP request failed: {} - {}",
                            status, body
                        )));
                    }
                }
                Err(e) => {
                    error!("Error fetching {}: {}", url_str, e);
                    if attempt < self.config.max_retries - 1 {
                        sleep(Duration::from_secs(
                            self.config.retry_delay * (attempt as u64 + 1),
                        ))
                        .await;
                    } else {
                        call_external_api!(method, host.as_str(), service, "500", start_time);
                        return Err(AppError::HttpRequest(e));
                    }
                }
            }
        }
        Err(AppError::Internal("All retry attempts failed".to_string()))
    }

    async fn fetch_patterns(&self, base_url: &str) -> AppResult<Vec<NandiPattern>> {
        let url = format!("{}/otp/routers/default/index/patterns", base_url);
        self.fetch_with_retry(&url, "fetch_patterns").await
    }

    async fn fetch_pattern_details(
        &self,
        base_url: &str,
        pattern_id: &str,
    ) -> AppResult<NandiPatternDetails> {
        let url = format!(
            "{}/otp/routers/default/index/patterns/{}",
            base_url, pattern_id
        );
        self.fetch_with_retry(&url, "fetch_pattern_details").await
    }

    async fn fetch_routes(&self, base_url: &str) -> AppResult<Vec<NandiRoutesRes>> {
        let url = format!("{}/otp/routers/default/index/routes", base_url);
        self.fetch_with_retry(&url, "fetch_routes").await
    }

    async fn fetch_stops(&self, base_url: &str) -> AppResult<Vec<GTFSStop>> {
        let url = format!("{}/otp/routers/default/index/stops", base_url);
        self.fetch_with_retry(&url, "fetch_stops").await
    }

    pub async fn is_ready(&self) -> bool {
        *self.is_ready.read().await
    }

    pub async fn get_route(&self, gtfs_id: &str, route_id: &str) -> AppResult<NandiRoutesRes> {
        let data = self.data.load_full();
        data.routes_by_gtfs
            .get(clean_identifier(gtfs_id).as_str())
            .and_then(|r| r.get(clean_identifier(route_id).as_str()))
            .cloned()
            .ok_or_else(|| AppError::NotFound("Route not found".to_string()))
    }

    pub async fn get_routes(&self, gtfs_id: &str) -> AppResult<Vec<NandiRoutesRes>> {
        let data = self.data.load_full();
        data.routes_by_gtfs
            .get(clean_identifier(gtfs_id).as_str())
            .map(|r| r.values().cloned().collect())
            .ok_or_else(|| AppError::NotFound("GTFS ID not found".to_string()))
    }

    pub async fn get_routes_by_ids(
        &self,
        gtfs_id: &str,
        route_ids: Vec<String>,
    ) -> AppResult<Vec<NandiRoutesRes>> {
        let data = self.data.load_full();
        let gtfs_id = clean_identifier(gtfs_id);
        let mut found_routes = Vec::new();

        if let Some(routes) = data.routes_by_gtfs.get(&gtfs_id) {
            for route_id in route_ids {
                let route_code = clean_identifier(&route_id);
                if let Some(route) = routes.get(&route_code) {
                    found_routes.push(route.clone());
                }
            }
        }

        Ok(found_routes)
    }

    pub async fn get_route_stop_mapping_by_route(
        &self,
        gtfs_id: &str,
        route_code: &str,
    ) -> AppResult<Vec<Arc<RouteStopMapping>>> {
        self.get_route_stop_mapping_by_route_with_direction(gtfs_id, route_code, None)
            .await
    }

    pub async fn get_route_stop_mapping_by_route_with_direction(
        &self,
        gtfs_id: &str,
        route_code: &str,
        direction: Option<&str>,
    ) -> AppResult<Vec<Arc<RouteStopMapping>>> {
        let data = self.data.load_full();
        let gtfs_id = clean_identifier(gtfs_id);
        let route_code = clean_identifier(route_code);

        if let Some(route_data) = data.route_data_by_gtfs.get(&gtfs_id) {
            if let Some(indices) = route_data.by_route.get(&route_code) {
                let mut mappings = Vec::new();
                let mut found_direction_match = false;

                for &i in indices {
                    if let Some(mapping) = route_data.mappings.get(i) {
                        // If direction is specified, check if it matches
                        if let Some(direction_filter) = direction {
                            // Check if any platform in suburban stop info matches the direction
                            if let Some(suburban_stop_info) =
                                data.suburban_stop_info_by_gtfs.get(&gtfs_id)
                            {
                                if let Some(stop_info) = suburban_stop_info.get(&*mapping.stop_code)
                                {
                                    let has_matching_direction = stop_info
                                        .platforms
                                        .iter()
                                        .any(|platform| platform.direction == direction_filter);
                                    if has_matching_direction {
                                        mappings.push(mapping.clone());
                                        found_direction_match = true;
                                    }
                                }
                            }
                        } else {
                            // If no direction specified, include all mappings
                            mappings.push(mapping.clone());
                        }
                    }
                }

                // If direction was specified but no matches found, return all mappings with platform set to null
                if let Some(_direction_filter) = direction {
                    if !found_direction_match {
                        // Return all mappings for this route but with platform set to null
                        let mut all_mappings = Vec::new();
                        for &i in indices {
                            if let Some(mapping) = route_data.mappings.get(i) {
                                let mut modified_mapping = (**mapping).clone();
                                modified_mapping.platform = None;
                                all_mappings.push(Arc::new(modified_mapping));
                            }
                        }
                        return Ok(all_mappings);
                    }
                }

                if !mappings.is_empty() {
                    return Ok(mappings);
                }
            }
        }
        Err(AppError::NotFound("Route not found".to_string()))
    }

    pub async fn get_route_stop_mapping_by_stop(
        &self,
        gtfs_id: &str,
        stop_code: &str,
    ) -> AppResult<Vec<Arc<RouteStopMapping>>> {
        self.get_route_stop_mapping_by_stop_with_direction(gtfs_id, stop_code, None)
            .await
    }

    pub async fn get_route_stop_mapping_by_stop_with_direction(
        &self,
        gtfs_id: &str,
        stop_code: &str,
        direction: Option<&str>,
    ) -> AppResult<Vec<Arc<RouteStopMapping>>> {
        let data = self.data.load_full();
        let gtfs_id = clean_identifier(gtfs_id);
        let stop_code = clean_identifier(stop_code);

        if let Some(route_data) = data.route_data_by_gtfs.get(&gtfs_id) {
            if let Some(indices) = route_data.by_stop.get(&stop_code) {
                let mut mappings = Vec::new();
                let mut found_direction_match = false;

                for &i in indices {
                    if let Some(mapping) = route_data.mappings.get(i) {
                        // If direction is specified, check if it matches
                        if let Some(direction_filter) = direction {
                            // Check if any platform in suburban stop info matches the direction
                            if let Some(suburban_stop_info) =
                                data.suburban_stop_info_by_gtfs.get(&gtfs_id)
                            {
                                if let Some(stop_info) = suburban_stop_info.get(&*mapping.stop_code)
                                {
                                    let has_matching_direction = stop_info
                                        .platforms
                                        .iter()
                                        .any(|platform| platform.direction == direction_filter);
                                    if has_matching_direction {
                                        mappings.push(mapping.clone());
                                        found_direction_match = true;
                                    }
                                }
                            }
                        } else {
                            // If no direction specified, include all mappings
                            mappings.push(mapping.clone());
                        }
                    }
                }

                // If direction was specified but no matches found, return all mappings with platform set to null
                if let Some(_direction_filter) = direction {
                    if !found_direction_match {
                        // Return all mappings for this stop but with platform set to null
                        let mut all_mappings = Vec::new();
                        for &i in indices {
                            if let Some(mapping) = route_data.mappings.get(i) {
                                let mut modified_mapping = (**mapping).clone();
                                modified_mapping.platform = None;
                                all_mappings.push(Arc::new(modified_mapping));
                            }
                        }
                        return Ok(all_mappings);
                    }
                }

                if !mappings.is_empty() {
                    return Ok(mappings);
                }
            }
        }
        Err(AppError::NotFound("Stop not found".to_string()))
    }

    pub async fn get_stops(&self, gtfs_id: &str) -> AppResult<Vec<Arc<RouteStopMapping>>> {
        let data = self.data.load_full();
        let gtfs_id = clean_identifier(gtfs_id);

        data.pre_computed_stops_by_gtfs
            .get(&gtfs_id)
            .cloned()
            .ok_or_else(|| AppError::NotFound("GTFS ID not found".to_string()))
    }

    pub async fn get_alternate_stops(
        &self,
        gtfs_id: &str,
        stop_id: &str,
    ) -> AppResult<Vec<Arc<GTFSStop>>> {
        let data = self.data.load_full();
        let gtfs_id = clean_identifier(gtfs_id);
        let stop_id = clean_identifier(stop_id);

        let stop_ids = data
            .alternate_stop_by_gtfs
            .get(&gtfs_id)
            .ok_or_else(|| AppError::NotFound("GTFS ID not found".to_string()))?
            .alternate_stops
            .get(&stop_id)
            .cloned()
            .unwrap_or_default();
        let stops_by_gtfs = data
            .stops_by_gtfs
            .get(&gtfs_id)
            .ok_or_else(|| AppError::NotFound("GTFS ID not found".to_string()))?;
        let stops: Vec<Arc<GTFSStop>> = stop_ids
            .into_iter()
            .filter_map(|id| {
                stops_by_gtfs
                    .stops
                    .get(&id)
                    .map(|stop| Arc::new(stop.clone()))
            })
            .collect();
        Ok(stops)
    }

    pub fn get_cluster_destinations_for_stop(
        &self,
        gtfs_id: &str,
        stop_code: &str,
    ) -> AppResult<Vec<String>> {
        let data = self.data.load_full();
        let gtfs_id = clean_identifier(gtfs_id);
        let stop_code = clean_identifier(stop_code);

        // Unknown gtfs_id is a configuration error → 404. Unknown stop_code
        // inside a known feed is semantically "no destinations" → 200 [].
        let stops_data = data.stops_by_gtfs.get(&gtfs_id).ok_or_else(|| {
            AppError::NotFound(format!("Stops data not found for gtfs_id: {}", gtfs_id))
        })?;

        let src_stop = match stops_data.stops.get(&stop_code) {
            Some(s) => s,
            None => {
                info!(
                    gtfs_id = %gtfs_id,
                    stop_code = %stop_code,
                    "destinations: stop not found in feed, returning empty list",
                );
                return Ok(Vec::new());
            }
        };

        let sibling_codes: Vec<String> = match src_stop.cluster_id.as_ref() {
            Some(cid) => {
                let siblings = stops_data
                    .by_cluster_id
                    .get(cid)
                    .cloned()
                    .unwrap_or_default();
                info!(
                    gtfs_id = %gtfs_id,
                    stop_code = %stop_code,
                    cluster_id = %cid,
                    siblings = siblings.len(),
                    "destinations: cluster walk",
                );
                siblings
            }
            None => {
                debug!(
                    gtfs_id = %gtfs_id,
                    stop_code = %stop_code,
                    "destinations: no cluster_id, falling back to single-stop walk",
                );
                vec![stop_code.clone()]
            }
        };

        let route_data = match data.route_data_by_gtfs.get(&gtfs_id) {
            Some(r) => r,
            None => return Ok(Vec::new()),
        };

        // For each sibling source stop, collect (route_code, src_seq) pairs.
        // Multiple siblings may serve the same route — keep the min src_seq so
        // we look as far downstream as possible on that route.
        let mut src_seq_by_route: HashMap<Arc<str>, i32> = HashMap::new();
        for sib in &sibling_codes {
            if let Some(idxs) = route_data.by_stop.get(sib) {
                for &i in idxs {
                    if let Some(m) = route_data.mappings.get(i) {
                        src_seq_by_route
                            .entry(m.route_code.clone())
                            .and_modify(|existing| {
                                if m.sequence_num < *existing {
                                    *existing = m.sequence_num;
                                }
                            })
                            .or_insert(m.sequence_num);
                    }
                }
            }
        }

        let src_cluster = src_stop.cluster_id.as_deref();
        let mut rep_by_key: HashMap<String, String> = HashMap::new();
        for (route_code, src_seq) in &src_seq_by_route {
            let idxs = match route_data.by_route.get(route_code.as_ref()) {
                Some(v) => v,
                None => continue,
            };
            for &i in idxs {
                let m = match route_data.mappings.get(i) {
                    Some(m) => m,
                    None => continue,
                };
                if m.sequence_num <= *src_seq {
                    continue;
                }
                let dst_code = m.stop_code.as_ref();
                let dst_stop = match stops_data.stops.get(dst_code) {
                    Some(s) => s,
                    None => continue,
                };
                match (src_cluster, dst_stop.cluster_id.as_deref()) {
                    (Some(s), Some(d)) if s == d => continue,
                    (None, _) if dst_code == stop_code.as_str() => continue,
                    _ => {}
                }
                let dedup_key = dst_stop
                    .cluster_id
                    .clone()
                    .unwrap_or_else(|| dst_code.to_string());
                rep_by_key
                    .entry(dedup_key)
                    .and_modify(|existing| {
                        if dst_code < existing.as_str() {
                            *existing = dst_code.to_string();
                        }
                    })
                    .or_insert_with(|| dst_code.to_string());
            }
        }

        let mut out: Vec<String> = rep_by_key.into_values().collect();
        out.sort();
        info!(
            gtfs_id = %gtfs_id,
            stop_code = %stop_code,
            destinations = out.len(),
            "destinations: result",
        );
        Ok(out)
    }

    pub fn get_routes_between_clusters_for_stops(
        &self,
        gtfs_id: &str,
        src_stop_code: &str,
        dst_stop_code: &str,
    ) -> AppResult<Vec<String>> {
        let data = self.data.load_full();
        let gtfs_id = clean_identifier(gtfs_id);
        let src_stop_code = clean_identifier(src_stop_code);
        let dst_stop_code = clean_identifier(dst_stop_code);

        let stops_data = data.stops_by_gtfs.get(&gtfs_id).ok_or_else(|| {
            AppError::NotFound(format!("Stops data not found for gtfs_id: {}", gtfs_id))
        })?;

        let route_data = match data.route_data_by_gtfs.get(&gtfs_id) {
            Some(r) => r,
            None => return Ok(Vec::new()),
        };

        let collect_routes = |stop_code: &str| -> HashSet<Arc<str>> {
            let mut routes: HashSet<Arc<str>> = HashSet::new();
            let siblings: &[String] = match stops_data.stops.get(stop_code) {
                Some(stop) => match stop.cluster_id.as_ref() {
                    Some(cid) => stops_data
                        .by_cluster_id
                        .get(cid)
                        .map(|v| v.as_slice())
                        .unwrap_or_default(),
                    None => std::slice::from_ref(&stop.code),
                },
                None => &[],
            };
            for sib in siblings {
                if let Some(idxs) = route_data.by_stop.get(sib) {
                    for &i in idxs {
                        if let Some(m) = route_data.mappings.get(i) {
                            routes.insert(m.route_code.clone());
                        }
                    }
                }
            }
            routes
        };

        let src_routes = collect_routes(&src_stop_code);
        if src_routes.is_empty() {
            return Ok(Vec::new());
        }
        let dst_routes = collect_routes(&dst_stop_code);

        Ok(src_routes
            .intersection(&dst_routes)
            .map(|r| r.as_ref().to_string())
            .collect())
    }

    pub async fn get_stop(
        &self,
        gtfs_id: &str,
        stop_code: &str,
    ) -> AppResult<(GTFSStop, Option<Arc<RouteStopMapping>>)> {
        let data = self.data.load_full();
        let gtfs_id = clean_identifier(gtfs_id);
        let stop_code = clean_identifier(stop_code);

        let stops_data = data.stops_by_gtfs.get(&gtfs_id).ok_or_else(|| {
            AppError::NotFound(format!("Stops data not found for gtfs_id: {}", gtfs_id))
        })?;

        let mut stop = stops_data.stops.get(&stop_code).cloned().ok_or_else(|| {
            AppError::NotFound(format!(
                "Stop not found for stop_code: {} under gtfs_id: {}",
                stop_code, gtfs_id
            ))
        })?;

        if let Some(regional_record) = data
            .stop_regional_names_by_gtfs
            .get(&gtfs_id)
            .and_then(|names| names.get(&stop_code))
        {
            stop.hindi_name = Some(regional_record.hindi_name.clone());
            stop.regional_name = Some(regional_record.regional_name.clone());
        }
        let first_mapping = data
            .route_data_by_gtfs
            .get(&gtfs_id)
            .and_then(|route_data| {
                route_data
                    .by_stop
                    .get(&stop_code)?
                    .first()
                    .and_then(|&i| route_data.mappings.get(i).cloned())
            });

        Ok((stop, first_mapping))
    }

    pub async fn get_stops_by_ids(
        &self,
        gtfs_id: &str,
        stop_codes: Vec<String>,
    ) -> AppResult<Vec<GTFSStop>> {
        let data = self.data.load_full();
        let mut found_stops = Vec::new();

        if let Some(stops_data) = data.stops_by_gtfs.get(clean_identifier(gtfs_id).as_str()) {
            for stop_code in stop_codes {
                let clean_stop_code = clean_identifier(&stop_code);
                if let Some(stop) = stops_data.stops.get(clean_stop_code.as_str()) {
                    found_stops.push(stop.clone());
                }
            }
        }

        Ok(found_stops)
    }

    pub async fn get_route_stop_mappings_by_route_codes(
        &self,
        gtfs_id: &str,
        route_codes: Vec<String>,
    ) -> AppResult<Vec<Arc<RouteStopMapping>>> {
        let data = self.data.load_full();
        let mut found_mappings = Vec::new();

        if let Some(route_data) = data
            .route_data_by_gtfs
            .get(clean_identifier(gtfs_id).as_str())
        {
            for route_code in route_codes {
                let clean_route_code = clean_identifier(&route_code);
                if let Some(indices) = route_data.by_route.get(clean_route_code.as_str()) {
                    for &i in indices {
                        if let Some(mapping) = route_data.mappings.get(i) {
                            found_mappings.push(mapping.clone());
                        }
                    }
                }
            }
        }

        Ok(found_mappings)
    }

    pub async fn get_route_stop_mappings_by_stop_codes(
        &self,
        gtfs_id: &str,
        stop_codes: Vec<String>,
    ) -> AppResult<Vec<Arc<RouteStopMapping>>> {
        let data = self.data.load_full();
        let mut found_mappings = Vec::new();

        if let Some(route_data) = data
            .route_data_by_gtfs
            .get(clean_identifier(gtfs_id).as_str())
        {
            for stop_code in stop_codes {
                let clean_stop_code = clean_identifier(&stop_code);
                if let Some(indices) = route_data.by_stop.get(clean_stop_code.as_str()) {
                    for &i in indices {
                        if let Some(mapping) = route_data.mappings.get(i) {
                            found_mappings.push(mapping.clone());
                        }
                    }
                }
            }
        }

        Ok(found_mappings)
    }

    pub async fn get_station_children(
        &self,
        gtfs_id: &str,
        stop_code: &str,
    ) -> AppResult<Vec<String>> {
        let data = self.data.load_full();
        Ok(data
            .children_by_parent
            .get(clean_identifier(gtfs_id).as_str())
            .and_then(|p| p.get(clean_identifier(stop_code).as_str()))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect())
    }

    pub async fn get_version(&self, gtfs_id: &str) -> AppResult<String> {
        let data = self.data.load_full();
        data.data_hash
            .get(clean_identifier(gtfs_id).as_str())
            .cloned()
            .ok_or_else(|| AppError::NotFound("GTFS ID not found".to_string()))
    }

    pub async fn get_provider_stop_code(
        &self,
        gtfs_id: &str,
        provider_stop_code: &str,
    ) -> AppResult<String> {
        let data = self.data.load_full();
        let gtfs_id = clean_identifier(gtfs_id);
        let provider_stop_code = clean_identifier(provider_stop_code);

        data.provider_stop_code_mapping
            .get(&gtfs_id)
            .and_then(|mapping| mapping.get(&provider_stop_code))
            .cloned()
            .ok_or_else(|| AppError::NotFound("Provider stop code not found".to_string()))
    }

    // Memory monitoring utility
    pub async fn get_memory_stats(&self) -> serde_json::Value {
        let data = self.data.load_full();

        let total_routes: usize = data.routes_by_gtfs.values().map(|r| r.len()).sum();
        let (total_mappings, total_by_route, total_by_stop) =
            data.route_data_by_gtfs.values().fold((0, 0, 0), |acc, d| {
                (
                    acc.0 + d.mappings.len(),
                    acc.1 + d.by_route.len(),
                    acc.2 + d.by_stop.len(),
                )
            });
        let total_stops: usize = data.stops_by_gtfs.values().map(|s| s.stops.len()).sum();
        let total_pre_computed_stops: usize = data
            .pre_computed_stops_by_gtfs
            .values()
            .map(|s| s.len())
            .sum();

        let mem = data.memory_usage_bytes();

        serde_json::json!({
            "counts": {
                "gtfs_feeds": data.routes_by_gtfs.len(),
                "total_routes": total_routes,
                "total_mappings": total_mappings,
                "total_by_route_keys": total_by_route,
                "total_by_stop_keys": total_by_stop,
                "total_stops": total_stops,
                "total_pre_computed_stops": total_pre_computed_stops,
                "children_groups": data.children_by_parent.len(),
            },
            "memory_bytes": mem,
            "memory_mb": {
                "total": format!("{:.2}", mem.total_bytes as f64 / 1_048_576.0),
                "routes": format!("{:.2}", mem.routes_bytes as f64 / 1_048_576.0),
                "route_data": format!("{:.2}", mem.route_data_bytes as f64 / 1_048_576.0),
                "stops": format!("{:.2}", mem.stops_bytes as f64 / 1_048_576.0),
                "pre_computed_stops": format!("{:.2}", mem.pre_computed_stops_bytes as f64 / 1_048_576.0),
                "geojson": format!("{:.2}", mem.geojson_bytes as f64 / 1_048_576.0),
            }
        })
    }

    pub async fn get_fleet_service_type(&self, gtfs_id: &str, vehicle_no: &str) -> Option<String> {
        let data = self.data.load_full();
        data.static_fleet_info_by_gtfs
            .get(clean_identifier(gtfs_id).as_str())
            .and_then(|m| m.get(clean_identifier(vehicle_no).as_str()))
            .and_then(|info| info.service_type.clone())
    }

    /// Returns pre-serialized JSON bytes for the cached data endpoint.
    /// Avoids re-serialization and deep cloning on every request.
    pub async fn get_cached_data_bytes(&self) -> Arc<Vec<u8>> {
        self.cached_data_bytes.load_full()
    }

    /// Pre-serialize cached data after a data refresh so the /cached-data
    /// endpoint can serve bytes directly without lock contention or cloning.
    async fn update_cached_data_bytes(&self) {
        let data = self.data.load_full();
        let response = CachedDataResponse {
            route_data_by_gtfs: data.route_data_by_gtfs.clone(),
            stops_by_gtfs: data.stops_by_gtfs.clone(),
            stop_geojsons_by_gtfs: data.stop_geojsons_by_gtfs.clone(),
        };
        match serde_json::to_vec(&response) {
            Ok(bytes) => {
                self.cached_data_bytes.store(Arc::new(bytes));
            }
            Err(e) => {
                error!("Failed to pre-serialize cached data: {}", e);
            }
        }
    }

    pub async fn get_feeds_in_memory(&self) -> Vec<String> {
        let data = self.data.load_full();
        data.route_data_by_gtfs.keys().cloned().collect()
    }

    pub async fn get_example_trip(
        &self,
        gtfs_id: &str,
        route_code: &str,
    ) -> AppResult<TripDetails> {
        let clean_gtfs = clean_identifier(gtfs_id);
        let clean_route = clean_identifier(route_code);

        // Check trip details cache first (separate lock from main data)
        {
            let cache = self.trip_details_cache.read().await;
            if let Some(cached) = cache
                .get(clean_gtfs.as_str())
                .and_then(|m| m.get(clean_route.as_str()))
            {
                return Ok(cached.clone());
            }
        }

        let data = self.data.load_full();
        let trip_feed = data
            .route_example_trip_by_gtfs
            .get(clean_gtfs.as_str())
            .and_then(|m| m.get(clean_route.as_str()))
            .cloned()
            .ok_or_else(|| AppError::NotFound("Example trip not found".to_string()))?;

        // Check if trip is in ignored list
        if self.config.ignored_trip_ids.contains(&trip_feed) {
            warn!("Trip {} is in ignored list, returning not found", trip_feed);
            return Err(AppError::NotFound(format!(
                "Example trip not found for route {}",
                route_code
            )));
        }

        drop(data);

        // Query trip details by trip_feed
        let query = "query Trip($id: String!) { trip(id: $id) { gtfsId stoptimes { stop { id lat lon code platformCode name } scheduledArrival scheduledDeparture headsign stopPosition } } }";
        let variables = serde_json::json!({ "id": format!("{}:{}", &clean_gtfs, trip_feed) });
        let resp = self
            .execute_graphql_query("default", query, Some(variables), None, None)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to fetch trip details: {}", e)))?;

        let mut stops: Vec<crate::models::TripStopDetail> = Vec::new();
        if let Some(stoptimes) = resp
            .get("data")
            .and_then(|d| d.get("trip"))
            .and_then(|t| t.get("stoptimes"))
            .and_then(|s| s.as_array())
        {
            for st in stoptimes {
                let stop_obj = st.get("stop").cloned().unwrap_or(serde_json::Value::Null);
                let stop_code = stop_obj
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let stop_id = stop_obj
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let stop_name = stop_obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let platform_code = stop_obj
                    .get("platformCode")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let lat = stop_obj.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let lon = stop_obj.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let scheduled_arrival = st
                    .get("scheduledArrival")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32;
                let scheduled_departure = st
                    .get("scheduledDeparture")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32;
                let headsign = st
                    .get("headsign")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let stop_position =
                    st.get("stopPosition").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                if stop_code.is_empty() || stop_id.is_empty() {
                    continue;
                }
                stops.push(crate::models::TripStopDetail {
                    stop_id,
                    stop_code,
                    stop_name,
                    platform_code,
                    lat,
                    lon,
                    scheduled_arrival,
                    scheduled_departure,
                    headsign,
                    stop_position,
                });
            }
        }

        let details = TripDetails {
            trip_id: trip_feed.clone(),
            stops,
        };

        // Cache results in trip details cache (separate lock from main data)
        let mut cache = self.trip_details_cache.write().await;
        cache
            .entry(clean_gtfs)
            .or_default()
            .insert(clean_route, details.clone());

        Ok(details)
    }

    pub async fn get_route_example_trip_map(&self) -> HashMap<String, HashMap<String, String>> {
        let data = self.data.load_full();
        data.route_example_trip_by_gtfs.clone()
    }

    // GraphQL query execution
    pub async fn force_refresh_data(&self) -> AppResult<()> {
        info!("Force refresh triggered - checking for GTFS data updates...");
        let start_time = std::time::Instant::now();

        // Use the same efficient polling mechanism
        match self.update_data().await {
            Ok(_) => {
                let duration = start_time.elapsed();
                info!("Force refresh completed in {:?}", duration);
                Ok(())
            }
            Err(e) => {
                error!("Force refresh failed: {}", e);
                Err(e)
            }
        }
    }

    pub async fn execute_graphql_query(
        &self,
        city: &str,
        query: &str,
        variables: Option<serde_json::Value>,
        operation_name: Option<String>,
        gtfs_id: Option<String>,
    ) -> AppResult<serde_json::Value> {
        // Try to find instance by gtfs_id first, then by city, then fallback to default
        let instance = if let Some(gtfs_id) = gtfs_id {
            self.config
                .otp_instances
                .find_instance_by_gtfs_id(&gtfs_id)
                .or_else(|| self.config.otp_instances.find_instance_by_city(city))
                .unwrap_or_else(|| self.config.otp_instances.get_default_instance())
        } else {
            self.config
                .otp_instances
                .find_instance_by_city(city)
                .unwrap_or_else(|| self.config.otp_instances.get_default_instance())
        };

        let url: Url = Url::parse(&format!("{}/otp/gtfs/v1", instance.url))
            .map_err(|e| AppError::Internal(format!("Failed to parse URL: {}", e)))?;

        let mut request_body = serde_json::json!({
            "query": query
        });

        if let Some(vars) = variables {
            request_body["variables"] = vars;
        }

        if let Some(op_name) = operation_name {
            request_body["operationName"] = serde_json::Value::String(op_name);
        }

        call_api::<serde_json::Value, serde_json::Value>(
            Protocol::Http1,
            Method::POST,
            &url,
            vec![("Content-Type", "application/json")],
            Some(request_body),
            Some("execute_graphql_query"),
        )
        .await
        .map_err(|e| AppError::Internal(format!("Failed to call API: {}", e)))
    }

    async fn fetch_route_example_trip_for_all_feeds(
        &self,
    ) -> AppResult<HashMap<String, HashMap<String, String>>> {
        // GraphQL: trips(feeds:["<gtfs>"]){ gtfsId id route{ id } }
        let query =
            "query Trips($feeds: [String!]) { trips(feeds: $feeds) { gtfsId route { gtfsId } } }";
        let feed_query = "query Feed { feeds { feedId } }";

        let mut mapping: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut feed_to_instance: HashMap<String, String> = HashMap::new();

        // Iterate over all OTP instances to collect feeds and map them to their instances
        for otp_instance in self.config.otp_instances.get_all_instances() {
            info!(instance = %otp_instance.identifier, url = %otp_instance.url, "Fetching feeds from OTP instance");

            let resp = match self
                .execute_graphql_query(&otp_instance.identifier, feed_query, None, None, None)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    error!(instance = %otp_instance.identifier, error = %e, "GraphQL feeds fetch failed for instance");
                    return Err(e);
                }
            };

            if let Some(feeds_array) = resp
                .get("data")
                .and_then(|d| d.get("feeds"))
                .and_then(|f| f.as_array())
            {
                for feed in feeds_array {
                    if let Some(feed_id) = feed.get("feedId").and_then(|v| v.as_str()) {
                        feed_to_instance
                            .insert(feed_id.to_string(), otp_instance.identifier.clone());
                    }
                }
                info!(instance = %otp_instance.identifier, feeds = feeds_array.len(), "Collected feeds from instance");
            } else {
                warn!(instance = %otp_instance.identifier, "No feeds found in response from instance");
            }
        }

        info!(
            total_feeds = feed_to_instance.len(),
            "Collected feeds from all OTP instances"
        );
        debug!(feeds = ?feed_to_instance, "Feed to instance mapping created");

        // Now fetch example trips for each feed from its specific instance
        for (feed_id, instance_identifier) in feed_to_instance {
            info!(feed = %feed_id, instance = %instance_identifier, "Fetching example trips from specific instance");
            let variables = serde_json::json!({ "feeds": [feed_id] });

            let resp = match self
                .execute_graphql_query(&instance_identifier, query, Some(variables), None, None)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    error!(instance = %instance_identifier, feed = %feed_id, error = %e, "GraphQL trips fetch failed for feed");
                    return Err(e);
                }
            };

            if let Some(trips) = resp
                .get("data")
                .and_then(|d| d.get("trips"))
                .and_then(|t| t.as_array())
            {
                debug!(feed = %feed_id, instance = %instance_identifier, trips = trips.len(), "Trips array extracted from instance");
                for trip in trips {
                    let route_feed = trip
                        .get("route")
                        .and_then(|r| r.get("gtfsId"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let trip_feed = trip.get("gtfsId").and_then(|v| v.as_str()).unwrap_or("");
                    if route_feed.is_empty() || trip_feed.is_empty() {
                        warn!(feed = %feed_id, trip = ?trip, "Missing route.gtfsId or trip gtfsId in trip entry");
                        continue;
                    }
                    let clean_route_code = clean_identifier(route_feed);
                    let clean_trip_code = clean_identifier(trip_feed);
                    debug!(
                        feed = %feed_id,
                        route_feed = %route_feed,
                        route_code = %clean_route_code,
                        trip_feed = %trip_feed,
                        trip_code = %clean_trip_code,
                        "Cleaned route/trip feed IDs"
                    );
                    if clean_route_code.is_empty() || clean_trip_code.is_empty() {
                        warn!(
                            feed = %feed_id,
                            route_feed = %route_feed,
                            trip_feed = %trip_feed,
                            "Cleaned codes are empty, skipping"
                        );
                        continue;
                    }

                    // Skip trips that are in the ignored list
                    if self.config.ignored_trip_ids.contains(&clean_trip_code) {
                        debug!(
                            feed = %feed_id,
                            trip_code = %clean_trip_code,
                            "Trip is in ignored list, skipping"
                        );
                        continue;
                    }

                    mapping
                        .entry(feed_id.to_string())
                        .or_default()
                        .entry(clean_route_code)
                        .or_insert(clean_trip_code);
                }
                let inserted = mapping.get(&feed_id).map(|m| m.len()).unwrap_or(0);
                info!(feed = %feed_id, instance = %instance_identifier, routes = inserted, "Example trips mapped for feed");
            } else {
                warn!(feed = %feed_id, instance = %instance_identifier, "No trips found for feed in instance");
            }
        }

        let total_routes: usize = mapping.values().map(|m| m.len()).sum();
        info!(
            feeds = mapping.len(),
            total_routes = total_routes,
            "Finished building example trip map"
        );
        Ok(mapping)
    }

    pub async fn get_seat_layout_id(&self, gtfs_id: &str, fleet_id: &str) -> Option<String> {
        let data = self.data.load_full();
        data.seat_layout_mapping_by_gtfs
            .get(gtfs_id)
            .and_then(|m| m.get(fleet_id))
            .cloned()
    }

    pub async fn get_seat_layout_id_by_fleet_id(&self, fleet_id: &str) -> Option<String> {
        let data = self.data.load_full();
        for mapping in data.seat_layout_mapping_by_gtfs.values() {
            if let Some(seat_layout_id) = mapping.get(fleet_id) {
                return Some(seat_layout_id.clone());
            }
        }
        None
    }
}
