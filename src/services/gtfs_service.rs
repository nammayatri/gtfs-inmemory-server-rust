use chrono::{DateTime, Utc};
use futures::future::join_all;
use serde_json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::config::AppConfig;
use crate::errors::{AppError, AppResult};
use crate::models::{
    cast_vehicle_type, clean_identifier, GTFSData, GTFSRouteData, GTFSStop, LatLong, NandiPattern,
    NandiPatternDetails, NandiRoutesRes, RouteStopMapping,
};

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

        // Fetch patterns
        let patterns = self.fetch_patterns().await?;
        info!("Fetched {} patterns", patterns.len());

        // Fetch pattern details in batches
        let pattern_details = self.fetch_pattern_details_batch(&patterns).await?;

        // Calculate trip counts
        let route_trip_counts = self.calculate_trip_counts(&pattern_details);

        // Calculate stop counts
        let route_stop_counts = self.calculate_stop_counts(&pattern_details);

        // Fetch routes
        let routes = self.fetch_routes().await?;
        let mut routes_by_gtfs =
            self.build_routes_by_gtfs(routes, &route_trip_counts, &route_stop_counts);

        // Build route data
        let route_data_by_gtfs = self.build_route_data(&pattern_details, &routes_by_gtfs);

        // Update start and end points
        self.update_start_end_points(&mut routes_by_gtfs, &route_data_by_gtfs);

        // Fetch stops and build children mapping
        let stops = self.fetch_stops().await?;
        let children_by_parent = self.build_children_mapping(stops);

        // Compute data hashes
        let data_hash = self.compute_all_data_hashes(&routes_by_gtfs);

        temp_data.route_data_by_gtfs = route_data_by_gtfs;
        temp_data.routes_by_gtfs = routes_by_gtfs;
        temp_data.children_by_parent = children_by_parent;
        temp_data.data_hash = data_hash;

        Ok(temp_data)
    }

    async fn fetch_pattern_details_batch(
        &self,
        patterns: &[NandiPattern],
    ) -> AppResult<Vec<NandiPatternDetails>> {
        let mut pattern_details = Vec::new();
        let chunks = patterns.chunks(self.config.process_batch_size);

        for chunk in chunks {
            let futures = chunk.iter().map(|p| self.fetch_pattern_details(&p.id));
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

    fn build_route_data(
        &self,
        pattern_details: &[NandiPatternDetails],
        routes_by_gtfs: &HashMap<String, HashMap<String, NandiRoutesRes>>,
    ) -> HashMap<String, GTFSRouteData> {
        let mut route_data_by_gtfs: HashMap<String, GTFSRouteData> = HashMap::new();

        for pattern in pattern_details {
            let parts: Vec<&str> = pattern.route_id.split(':').collect();
            if parts.len() < 2 {
                continue;
            }
            let gtfs_id = parts[0];
            let route_code = parts[1];

            let vehicle_type = routes_by_gtfs
                .get(gtfs_id)
                .and_then(|r| r.get(route_code))
                .map(|route| route.mode.clone())
                .unwrap_or_else(|| "UNKNOWN".to_string());

            let route_data = route_data_by_gtfs.entry(gtfs_id.to_string()).or_default();

            for (seq, stop) in pattern.stops.iter().enumerate() {
                let mapping = Arc::new(RouteStopMapping {
                    estimated_travel_time_from_previous_stop: None,
                    provider_code: "GTFS".to_string(),
                    route_code: route_code.to_string(),
                    sequence_num: seq as i32,
                    stop_code: stop.code.clone(),
                    stop_name: stop.name.clone(),
                    stop_point: LatLong {
                        lat: stop.lat,
                        lon: stop.lon,
                    },
                    vehicle_type: vehicle_type.clone(),
                });

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
    ) -> HashMap<String, HashMap<String, Vec<String>>> {
        let mut children_by_parent: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
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
                        .push(stop_code.to_string());
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

    async fn fetch_with_retry<T>(&self, url: &str) -> AppResult<T>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        for attempt in 0..self.config.max_retries {
            match self.http_client.get(url).send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        return response.json::<T>().await.map_err(|e| {
                            AppError::Internal(format!("Failed to deserialize response: {}", e))
                        });
                    } else if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
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
                        return Err(AppError::Internal(format!(
                            "HTTP request failed: {} - {}",
                            status, body
                        )));
                    }
                }
                Err(e) => {
                    error!("Error fetching {}: {}", url, e);
                    if attempt < self.config.max_retries - 1 {
                        sleep(Duration::from_secs(
                            self.config.retry_delay * (attempt as u64 + 1),
                        ))
                        .await;
                    } else {
                        return Err(AppError::HttpRequest(e));
                    }
                }
            }
        }
        Err(AppError::Internal("All retry attempts failed".to_string()))
    }

    async fn fetch_patterns(&self) -> AppResult<Vec<NandiPattern>> {
        let url = format!(
            "{}/otp/routers/default/index/patterns",
            self.config.base_url
        );
        self.fetch_with_retry(&url).await
    }

    async fn fetch_pattern_details(&self, pattern_id: &str) -> AppResult<NandiPatternDetails> {
        let url = format!(
            "{}/otp/routers/default/index/patterns/{}",
            self.config.base_url, pattern_id
        );
        self.fetch_with_retry(&url).await
    }

    async fn fetch_routes(&self) -> AppResult<Vec<NandiRoutesRes>> {
        let url = format!("{}/otp/routers/default/index/routes", self.config.base_url);
        self.fetch_with_retry(&url).await
    }

    async fn fetch_stops(&self) -> AppResult<Vec<GTFSStop>> {
        let url = format!("{}/otp/routers/default/index/stops", self.config.base_url);
        self.fetch_with_retry(&url).await
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

    pub async fn get_route_stop_mapping_by_route(
        &self,
        gtfs_id: &str,
        route_code: &str,
    ) -> AppResult<Vec<RouteStopMapping>> {
        let data = self.data.read().await;
        let gtfs_id = clean_identifier(gtfs_id);
        let route_code = clean_identifier(route_code);

        if let Some(route_data) = data.route_data_by_gtfs.get(&gtfs_id) {
            if let Some(indices) = route_data.by_route.get(&route_code) {
                return Ok(indices
                    .iter()
                    .filter_map(|&i| route_data.mappings.get(i).map(|m| m.as_ref().clone()))
                    .collect());
            }
        }
        Err(AppError::NotFound("Route not found".to_string()))
    }

    pub async fn get_route_stop_mapping_by_stop(
        &self,
        gtfs_id: &str,
        stop_code: &str,
    ) -> AppResult<Vec<RouteStopMapping>> {
        let data = self.data.read().await;
        let gtfs_id = clean_identifier(gtfs_id);
        let stop_code = clean_identifier(stop_code);

        if let Some(route_data) = data.route_data_by_gtfs.get(&gtfs_id) {
            if let Some(indices) = route_data.by_stop.get(&stop_code) {
                return Ok(indices
                    .iter()
                    .filter_map(|&i| route_data.mappings.get(i).map(|m| m.as_ref().clone()))
                    .collect());
            }
        }
        Err(AppError::NotFound("Stop not found".to_string()))
    }

    pub async fn get_stops(&self, gtfs_id: &str) -> AppResult<Vec<RouteStopMapping>> {
        let data = self.data.read().await;
        let gtfs_id = clean_identifier(gtfs_id);

        if let Some(route_data) = data.route_data_by_gtfs.get(&gtfs_id) {
            return Ok(route_data
                .mappings
                .iter()
                .map(|m| m.as_ref().clone())
                .collect());
        }
        Err(AppError::NotFound("GTFS ID not found".to_string()))
    }

    pub async fn get_stop(&self, gtfs_id: &str, stop_code: &str) -> AppResult<RouteStopMapping> {
        let data = self.data.read().await;
        let gtfs_id = clean_identifier(gtfs_id);
        let stop_code = clean_identifier(stop_code);

        if let Some(route_data) = data.route_data_by_gtfs.get(&gtfs_id) {
            if let Some(indices) = route_data.by_stop.get(&stop_code) {
                if let Some(&index) = indices.first() {
                    if let Some(mapping) = route_data.mappings.get(index) {
                        return Ok(mapping.as_ref().clone());
                    }
                }
            }
        }
        Err(AppError::NotFound("Stop not found".to_string()))
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
            .unwrap_or_default())
    }

    pub async fn get_version(&self, gtfs_id: &str) -> AppResult<String> {
        let data = self.data.read().await;
        data.data_hash
            .get(clean_identifier(gtfs_id).as_str())
            .cloned()
            .ok_or_else(|| AppError::NotFound("GTFS ID not found".to_string()))
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

        stats
    }

    // GraphQL query execution
    pub async fn execute_graphql_query(
        &self,
        query: &str,
        variables: Option<serde_json::Value>,
        operation_name: Option<String>,
    ) -> AppResult<serde_json::Value> {
        let url = format!("{}/otp/gtfs/v1", self.config.base_url);

        let mut request_body = serde_json::json!({
            "query": query
        });

        if let Some(vars) = variables {
            request_body["variables"] = vars;
        }

        if let Some(op_name) = operation_name {
            request_body["operationName"] = serde_json::Value::String(op_name);
        }

        let response = self
            .http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| AppError::HttpRequest(e))?;

        if response.status().is_success() {
            let result: serde_json::Value = response.json().await.map_err(|e| {
                AppError::Internal(format!("Failed to deserialize GraphQL response: {}", e))
            })?;
            Ok(result)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(AppError::Internal(format!(
                "GraphQL request failed: {} - {}",
                status, body
            )))
        }
    }
}
