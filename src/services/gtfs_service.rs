use crate::environment::AppConfig;
use crate::models::{
    cast_vehicle_type, clean_identifier, CachedDataResponse, GTFSData, GTFSRouteData, GTFSStop,
    GTFSStopData, LatLong, NandiPattern, NandiPatternDetails, NandiRoutesRes, PlatformInfo,
    ProviderStopCodeRecord, RouteStopMapping, StopGeojson, StopGeojsonRecord,
    StopRegionalNameRecord, SuburbanStopInfo, SuburbanStopInfoRecord,
};
use crate::tools::error::{AppError, AppResult};
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
pub struct GTFSService {
    config: AppConfig,
    data: Arc<RwLock<GTFSData>>,
    http_client: reqwest::Client,
    is_ready: Arc<RwLock<bool>>,
    last_update: Arc<RwLock<DateTime<Utc>>>,
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
            data: Arc::new(RwLock::new(GTFSData::new())),
            http_client,
            is_ready: Arc::new(RwLock::new(false)),
            last_update: Arc::new(RwLock::new(Utc::now())),
        };

        service.load_initial_data().await?;

        Ok(service)
    }

    async fn load_initial_data(&self) -> AppResult<()> {
        info!("Loading initial GTFS data...");
        let start_time = std::time::Instant::now();

        let temp_data = self.fetch_and_process_data().await?;

        let mut data = self.data.write().await;
        data.update_data(temp_data);

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
        let mut already_visited: HashMap<String, bool> = HashMap::new();

        for otp_instance in self.config.otp_instances.get_all_instances() {
            let base_url = &otp_instance.url;
            if already_visited.contains_key(base_url) {
                continue;
            }
            already_visited.insert(base_url.to_string(), true);
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

        // Calculate trip counts
        let route_trip_counts = self.calculate_trip_counts(&all_pattern_details);

        // Calculate stop counts
        let route_stop_counts = self.calculate_stop_counts(&all_pattern_details);

        // Fetch routes
        let mut routes_by_gtfs =
            self.build_routes_by_gtfs(all_routes, &route_trip_counts, &route_stop_counts);

        // Build stops data first (needed by route data for parent_stop_code lookup)
        let stops_by_gtfs =
            self.build_stops_by_gtfs(all_stops.clone(), &stop_regional_names_by_gtfs);

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

        // Update start and end points
        self.update_start_end_points(&mut routes_by_gtfs, &route_data_by_gtfs);

        // Fetch stops and build children mapping
        let children_by_parent = self.build_children_mapping(all_stops);

        // Compute data hashes
        let data_hash = self.compute_all_data_hashes(&routes_by_gtfs);

        temp_data.route_data_by_gtfs = route_data_by_gtfs;
        temp_data.stops_by_gtfs = stops_by_gtfs;
        temp_data.routes_by_gtfs = routes_by_gtfs;
        temp_data.children_by_parent = children_by_parent;
        temp_data.data_hash = data_hash;
        temp_data.stop_geojsons_by_gtfs = stop_geojsons_by_gtfs;
        temp_data.provider_stop_code_mapping = provider_stop_code_mapping;
        temp_data.stop_regional_names_by_gtfs = stop_regional_names_by_gtfs;
        temp_data.suburban_stop_info_by_gtfs = suburban_stop_info_by_gtfs;

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
                .last()
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
    ) -> HashMap<String, HashMap<String, NandiRoutesRes>> {
        let mut routes_by_gtfs: HashMap<String, HashMap<String, NandiRoutesRes>> = HashMap::new();
        for route in routes {
            let parts: Vec<&str> = route.id.split(':').collect();
            if parts.len() < 2 {
                continue;
            }
            let gtfs_id = parts[0];
            let route_code = parts[1];

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
            };
            routes_by_gtfs
                .entry(gtfs_id.to_string())
                .or_default()
                .insert(route_code.to_string(), route_res);
        }
        routes_by_gtfs
    }

    fn build_stops_by_gtfs(
        &self,
        stops: Vec<GTFSStop>,
        stop_regional_names_by_gtfs: &HashMap<String, HashMap<String, StopRegionalNameRecord>>,
    ) -> HashMap<String, GTFSStopData> {
        let mut stops_by_gtfs: HashMap<String, GTFSStopData> = HashMap::new();

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
                };
                stop_data
                    .stops
                    .insert(stop.cluster.clone().unwrap(), cluster_stop_res);
            }

            stop_data.stops.insert(stop_code.to_string(), stop_res);
        }

        stops_by_gtfs
    }

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

            let vehicle_type = routes_by_gtfs
                .get(gtfs_id)
                .and_then(|r| r.get(route_code))
                .map(|route| route.mode.clone())
                .unwrap_or_else(|| "UNKNOWN".to_string());

            let route_data = route_data_by_gtfs.entry(gtfs_id.to_string()).or_default();
            let mut visited_mapping: HashMap<String, bool> = HashMap::new();

            for (seq, stop) in longest_pattern.stops.iter().enumerate() {
                let stop_geojson = stop_geojsons_by_gtfs
                    .get(gtfs_id)
                    .and_then(|g| g.get(&stop.code))
                    .map(|geojson| geojson.clone());

                // Find provider stop code for this stop code
                let provider_stop_code =
                    provider_stop_code_mapping.get(gtfs_id).and_then(|mapping| {
                        // Find the provider_stop_code that maps to this stop_code
                        mapping
                            .iter()
                            .find(|(_, stop_code)| stop_code == &&stop.code)
                            .map(|(provider_stop_code, _)| provider_stop_code.clone())
                    });

                // Get platform from suburban stop info if available
                let platform = suburban_stop_info_by_gtfs
                    .get(gtfs_id)
                    .and_then(|stops| stops.get(&stop.code))
                    .and_then(|suburban_stop| {
                        // For now, we'll use the first platform's platform number
                        // You might want to implement more sophisticated logic based on your needs
                        suburban_stop
                            .platforms
                            .first()
                            .map(|platform_info| platform_info.platforms.clone())
                    });

                let mapping = Arc::new(RouteStopMapping {
                    estimated_travel_time_from_previous_stop: None,
                    provider_code: provider_stop_code.unwrap_or("GTFS".to_string()),
                    route_code: route_code.to_string(),
                    sequence_num: (seq + 1) as i32,
                    stop_code: stop.code.clone(),
                    stop_name: stop.name.clone(),
                    stop_point: LatLong {
                        lat: stop.lat,
                        lon: stop.lon,
                    },
                    parent_stop_code: stops_by_gtfs
                        .get(gtfs_id)
                        .and_then(|stops_data| stops_data.stops.get(&stop.code))
                        .and_then(|gtfs_stop| gtfs_stop.station_id.as_ref())
                        .and_then(|station_id| station_id.split(':').last())
                        .filter(|s| !s.is_empty())
                        .map(|parent_code| parent_code.to_string()),
                    vehicle_type: vehicle_type.clone(),
                    geo_json: stop_geojson.as_ref().map(|s| s.geo_json.clone()),
                    gates: stop_geojson.as_ref().and_then(|s| s.gates.clone()),
                    hindi_name: stop_regional_names_by_gtfs
                        .get(gtfs_id)
                        .and_then(|m| m.get(&stop.code))
                        .map(|r| r.hindi_name.clone()),
                    regional_name: stop_regional_names_by_gtfs
                        .get(gtfs_id)
                        .and_then(|m| m.get(&stop.code))
                        .map(|r| r.regional_name.clone()),
                    platform,
                });
                let hash = get_sha256_hash(&mapping);
                if visited_mapping.contains_key(&hash) {
                    continue;
                }
                visited_mapping.insert(hash, true);

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
                let stop_code = stop.id.split(':').last().unwrap_or_default();
                let parent_code = station_id.split(':').last().unwrap_or_default();
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
                    info!("Changes detected, updating data...");
                    let mut data = self.data.write().await;
                    data.update_data(new_data);

                    let mut last_update = self.last_update.write().await;
                    *last_update = Utc::now();
                    let duration = start_time.elapsed();
                    info!("Data updated successfully in {:?}", duration);

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
        let current_data = self.data.read().await;
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
                        error!("HTTP request failed with status {}: {}", status, body);
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
        let data = self.data.read().await;
        data.routes_by_gtfs
            .get(clean_identifier(gtfs_id).as_str())
            .and_then(|r| r.get(clean_identifier(route_id).as_str()))
            .cloned()
            .ok_or_else(|| AppError::NotFound("Route not found".to_string()))
    }

    pub async fn get_routes(&self, gtfs_id: &str) -> AppResult<Vec<NandiRoutesRes>> {
        let data = self.data.read().await;
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
        let data = self.data.read().await;
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
        let data = self.data.read().await;
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
                                if let Some(stop_info) = suburban_stop_info.get(&mapping.stop_code)
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
        let data = self.data.read().await;
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
                                if let Some(stop_info) = suburban_stop_info.get(&mapping.stop_code)
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
        let data = self.data.read().await;
        let gtfs_id = clean_identifier(gtfs_id);

        if let Some(route_data) = data.route_data_by_gtfs.get(&gtfs_id) {
            let mut stops: Vec<Arc<RouteStopMapping>> = Vec::new();
            let regional_names_by_stop = data.stop_regional_names_by_gtfs.get(&gtfs_id);
            for indices in route_data.by_stop.values() {
                if let Some(&i) = indices.first() {
                    if let Some(mapping) = route_data.mappings.get(i) {
                        let mut mapping = (**mapping).clone();
                        // Populate hindi_name and regional_name if available
                        if let Some(regional_names) = regional_names_by_stop {
                            if let Some(regional_record) = regional_names.get(&mapping.stop_code) {
                                mapping.hindi_name = Some(regional_record.hindi_name.clone());
                                mapping.regional_name = Some(regional_record.regional_name.clone());
                            }
                        }
                        stops.push(Arc::new(mapping));
                    }
                }
            }
            return Ok(stops);
        }
        Err(AppError::NotFound("GTFS ID not found".to_string()))
    }

    pub async fn get_stop(
        &self,
        gtfs_id: &str,
        stop_code: &str,
    ) -> AppResult<(GTFSStop, Option<Arc<RouteStopMapping>>)> {
        let data = self.data.read().await;
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
        let data = self.data.read().await;
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
        let data = self.data.read().await;
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
        let data = self.data.read().await;
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
        let data = self.data.read().await;
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
        let data = self.data.read().await;
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
        let data = self.data.read().await;
        let gtfs_id = clean_identifier(gtfs_id);
        let provider_stop_code = clean_identifier(provider_stop_code);

        data.provider_stop_code_mapping
            .get(&gtfs_id)
            .and_then(|mapping| mapping.get(&provider_stop_code))
            .cloned()
            .ok_or_else(|| AppError::NotFound("Provider stop code not found".to_string()))
    }

    // Memory monitoring utility
    pub async fn get_memory_stats(&self) -> std::collections::HashMap<String, usize> {
        let data = self.data.read().await;
        let mut stats = std::collections::HashMap::new();

        stats.insert(
            "routes_by_gtfs_count".to_string(),
            data.routes_by_gtfs.len(),
        );
        stats.insert(
            "route_data_by_gtfs_count".to_string(),
            data.route_data_by_gtfs.len(),
        );
        stats.insert("stops_by_gtfs_count".to_string(), data.stops_by_gtfs.len());
        stats.insert(
            "children_by_parent_count".to_string(),
            data.children_by_parent.len(),
        );
        stats.insert("data_hash_count".to_string(), data.data_hash.len());

        let total_routes = data.routes_by_gtfs.values().map(|r| r.len()).sum::<usize>();
        stats.insert("total_routes".to_string(), total_routes);

        let (total_mappings, total_by_route, total_by_stop) =
            data.route_data_by_gtfs.values().fold((0, 0, 0), |acc, d| {
                (
                    acc.0 + d.mappings.len(),
                    acc.1 + d.by_route.len(),
                    acc.2 + d.by_stop.len(),
                )
            });

        stats.insert("total_mappings".to_string(), total_mappings);
        stats.insert("total_by_route_keys".to_string(), total_by_route);
        stats.insert("total_by_stop_keys".to_string(), total_by_stop);

        let total_stops = data
            .stops_by_gtfs
            .values()
            .map(|s| s.stops.len())
            .sum::<usize>();
        stats.insert("total_stops".to_string(), total_stops);

        stats
    }

    pub async fn get_all_cached_data(&self) -> CachedDataResponse {
        let data = self.data.read().await;
        CachedDataResponse {
            route_data_by_gtfs: data.route_data_by_gtfs.clone(),
            stops_by_gtfs: data.stops_by_gtfs.clone(),
            stop_geojsons_by_gtfs: data.stop_geojsons_by_gtfs.clone(),
        }
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
}
