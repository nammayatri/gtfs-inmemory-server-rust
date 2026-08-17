use crate::environment::AppConfig;
use crate::models::{
    cast_vehicle_type, clean_identifier, CachedDataResponse, ClusterRouteConnection, GTFSData,
    GTFSRouteData, GTFSStop, GTFSStopData, LatLong, NandiPattern, NandiPatternDetails,
    NandiRoutesRes, PlatformInfo, ProviderStopCodeRecord, RouteServiceTierRecord, RouteStopMapping,
    SeatLayoutMappingRecord, ServiceTierType, StaticFleetInfo, StaticFleetInfoRecord, StopGeojson,
    StopGeojsonRecord, StopRegionalNameRecord, SuburbanStopInfo, SuburbanStopInfoRecord,
};
use crate::models::{GTFSAlternateStopData, TripDetails, TripStopDetail};
use crate::services::operator::{OperatorService, SUPPORTED_OPERATOR_GTFS_IDS};
use crate::tools::error::{AppError, AppResult};
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use csv::ReaderBuilder;
use futures::future::join_all;
use reqwest::Method;
use serde::{Deserialize, Serialize};
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

/// Per-file entry in the preprocessed-data manifest (`metadata.json`).
#[derive(Debug, Clone, serde::Deserialize)]
struct ManifestFile {
    sha256: String,
    count: i64,
}

/// `metadata.json` produced by the Nandi GTFS preprocessor. The preprocessor
/// writes this file last, so its presence means every data file is complete.
/// Unknown fields (build_commit, stats, generated_at, ...) are ignored.
#[derive(Debug, Clone, serde::Deserialize)]
struct PreprocessedManifest {
    version: String,
    #[serde(default)]
    gtfs_feeds: Vec<String>,
    #[serde(default)]
    files: HashMap<String, ManifestFile>,
}

const SNAPSHOT_FILE: &str = "snapshot.bin";
const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// Fully-built, ready-to-serve data baked at image-build time; `gated_sha` (metadata.json hash) gates staleness.
#[derive(Serialize, Deserialize)]
struct Snapshot {
    format_version: u32,
    manifest_version: String,
    gated_sha: String,
    data: GTFSData,
    trip_details_cache: HashMap<String, HashMap<String, TripDetails>>,
}

/// SHA-256 of a file's bytes on disk (hex). Used to verify preprocessed data
/// against the checksum the preprocessor recorded in `metadata.json`.
async fn sha256_of_file(path: &std::path::Path) -> AppResult<String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to read {}: {}", path.display(), e)))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
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
    /// Set once at startup so refreshes can enrich routes with DB-only fields (e.g. encoded_polyline).
    operator_service: std::sync::OnceLock<Arc<dyn OperatorService>>,
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
            operator_service: std::sync::OnceLock::new(),
        };

        Ok(service)
    }

    pub fn set_operator_service(&self, op: Arc<dyn OperatorService>) {
        let _ = self.operator_service.set(op);
    }

    pub async fn load_initial_data(&self) -> AppResult<()> {
        info!("Loading initial GTFS data...");
        let start_time = std::time::Instant::now();

        // Preprocessed mode: load the pre-built snapshot; fall back to JSON if absent/stale.
        let temp_data = if self.config.use_preprocessed_data {
            match self.try_load_snapshot().await {
                Ok(Some(data)) => {
                    info!(
                        "Loaded pre-built snapshot in {:?} (skipped JSON parse + index build)",
                        start_time.elapsed()
                    );
                    data
                }
                Ok(None) => {
                    info!("No usable snapshot found; building data from preprocessed JSON");
                    self.fetch_and_process_data().await?
                }
                Err(e) => {
                    warn!(
                        "Snapshot load failed ({}); building data from preprocessed JSON",
                        e
                    );
                    self.fetch_and_process_data().await?
                }
            }
        } else {
            self.fetch_and_process_data().await?
        };

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

    /// Parse+index once and serialize `GTFSData` (+ trip cache) to snapshot.bin (for `--build-snapshot`). No Nandi; DB polylines re-applied at boot.
    pub async fn build_snapshot(&self) -> AppResult<std::path::PathBuf> {
        if !self.config.use_preprocessed_data {
            return Err(AppError::Internal(
                "build_snapshot requires use_preprocessed_data=true".to_string(),
            ));
        }
        let dir = std::path::Path::new(&self.config.preprocessed_data_dir);
        let manifest = self.verify_preprocessed_manifest(dir).await?;
        let gated_sha = sha256_of_file(&dir.join("metadata.json")).await?;

        info!("Building snapshot: parsing preprocessed JSON and constructing indices...");
        let build_start = std::time::Instant::now();
        let data = self.fetch_and_process_data().await?;
        let trip_details_cache = self.trip_details_cache.read().await.clone();
        info!(
            "Snapshot data built in {:?}, serializing...",
            build_start.elapsed()
        );

        let snapshot = Snapshot {
            format_version: SNAPSHOT_FORMAT_VERSION,
            manifest_version: manifest.version,
            gated_sha,
            data,
            trip_details_cache,
        };
        // Map-encoded MessagePack: keyed fields round-trip skip/rename/default like JSON.
        let mut bytes: Vec<u8> = Vec::new();
        let mut ser = rmp_serde::Serializer::new(&mut bytes).with_struct_map();
        snapshot
            .serialize(&mut ser)
            .map_err(|e| AppError::Internal(format!("Failed to serialize snapshot: {}", e)))?;
        let out_path = dir.join(SNAPSHOT_FILE);
        tokio::fs::write(&out_path, &bytes)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to write snapshot: {}", e)))?;
        info!(
            "Snapshot written: {} ({:.1} MB)",
            out_path.display(),
            bytes.len() as f64 / 1_048_576.0
        );
        Ok(out_path)
    }

    /// Load snapshot.bin: `Some` if valid+current (populates trip cache, re-applies polylines); `None` if absent/stale.
    async fn try_load_snapshot(&self) -> AppResult<Option<GTFSData>> {
        let dir = std::path::Path::new(&self.config.preprocessed_data_dir);
        let snap_path = dir.join(SNAPSHOT_FILE);
        if !snap_path.exists() {
            return Ok(None);
        }

        let bytes = tokio::fs::read(&snap_path)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to read snapshot: {}", e)))?;
        let snapshot: Snapshot = rmp_serde::from_slice(&bytes)
            .map_err(|e| AppError::Internal(format!("Failed to decode snapshot: {}", e)))?;

        if snapshot.format_version != SNAPSHOT_FORMAT_VERSION {
            warn!(
                "Snapshot format {} != expected {}; ignoring snapshot",
                snapshot.format_version, SNAPSHOT_FORMAT_VERSION
            );
            return Ok(None);
        }

        // Staleness gate: metadata.json must be byte-identical to build time.
        let current_sha = sha256_of_file(&dir.join("metadata.json")).await?;
        if current_sha != snapshot.gated_sha {
            warn!(
                "Snapshot is stale (metadata.json changed since build: {} != {}); ignoring snapshot",
                current_sha, snapshot.gated_sha
            );
            return Ok(None);
        }

        info!(
            "Snapshot accepted (manifest version={}); populating trip-details cache",
            snapshot.manifest_version
        );
        *self.trip_details_cache.write().await = snapshot.trip_details_cache;

        let mut data = snapshot.data;
        // Re-apply DB polylines (not Nandi) so a snapshot boot matches a JSON boot.
        self.enrich_routes_with_polylines(&mut data.routes_by_gtfs)
            .await;
        self.enrich_routes_with_db_service_tiers(&mut data.routes_by_gtfs)
            .await;
        // Recompute hash after enrich.
        data.data_hash = self.compute_all_data_hashes(&data.routes_by_gtfs);
        // Rebuild the #[serde(skip)] field.
        data.pre_computed_stops_by_gtfs =
            Self::pre_compute_stops(&data.route_data_by_gtfs, &data.stop_regional_names_by_gtfs);

        Ok(Some(data))
    }

    async fn fetch_and_process_data(&self) -> AppResult<GTFSData> {
        let mut temp_data = GTFSData::new();
        let mut all_pattern_details = Vec::new();
        let mut all_routes = Vec::new();
        let mut all_stops = Vec::new();
        let mut already_visited: HashSet<String> = HashSet::new();

        // Track which gtfs_ids have been loaded from preprocessed files
        let mut preprocessed_gtfs_ids: HashSet<String> = HashSet::new();

        // ── When use_preprocessed_data is true: prefer preprocessed files; fall back to Nandi only on issue ──
        if self.config.use_preprocessed_data {
            let preprocessed_dir = &self.config.preprocessed_data_dir;
            info!(
                "Loading GTFS data from preprocessed files in {} (priority source)",
                preprocessed_dir
            );

            match self.load_preprocessed_data(preprocessed_dir).await {
                Ok((routes, patterns, stops, loaded_gtfs_ids)) => {
                    if loaded_gtfs_ids.is_empty() {
                        warn!(
                            "Preprocessed data directory had no feeds. Falling back to Nandi APIs."
                        );
                    } else {
                        preprocessed_gtfs_ids = loaded_gtfs_ids;
                        all_routes.extend(routes);
                        all_pattern_details.extend(patterns);
                        all_stops.extend(stops);
                        info!(
                            "Using preprocessed data only for {} GTFS feeds: {:?} (Nandi not called)",
                            preprocessed_gtfs_ids.len(),
                            preprocessed_gtfs_ids
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to load preprocessed data: {}. Falling back to Nandi APIs for all feeds.",
                        e
                    );
                }
            }
        }

        // ── Fetch from Nandi/OTP APIs only when we have no preprocessed data ─────────────────
        // (i.e. use_preprocessed_data is false, or preprocessed load failed, or returned no feeds)
        if preprocessed_gtfs_ids.is_empty() {
            info!("Fetching GTFS data from Nandi/OTP APIs...");
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

                let routes = self.fetch_routes(base_url).await?;
                all_routes.extend(routes);

                let stops = self.fetch_stops(base_url).await?;
                all_stops.extend(stops);
            }
        }
        info!("Fetched {} patterns total", all_pattern_details.len());

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

        let csv_route_service_tiers = self.read_route_service_tiers_csv().await?;
        info!(
            "Loaded route service tiers for {} GTFS IDs from CSV",
            csv_route_service_tiers.len()
        );
        let route_service_tiers_by_gtfs = self
            .build_route_service_tiers(csv_route_service_tiers, &all_routes)
            .await;
        info!(
            "Resolved route service tiers for {} GTFS IDs",
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
        // Only fetch from Nandi for feeds NOT loaded from preprocessed files
        let route_example_trip_by_gtfs = if preprocessed_gtfs_ids.is_empty() {
            self.fetch_route_example_trip_for_all_feeds().await?
        } else {
            // For preprocessed feeds, build example trip map from patterns (no GraphQL)
            let (preprocessed_trip_map, preprocessed_trip_details) =
                Self::build_example_trip_from_patterns(
                    &all_pattern_details,
                    &preprocessed_gtfs_ids,
                );

            let mut trip_map = preprocessed_trip_map;

            // Still fetch from Nandi for non-preprocessed feeds
            if self
                .config
                .otp_instances
                .get_all_instances()
                .iter()
                .any(|inst| !preprocessed_gtfs_ids.contains(&inst.identifier))
            {
                match self.fetch_route_example_trip_for_all_feeds().await {
                    Ok(api_trips) => {
                        for (gtfs_id, trips) in api_trips {
                            if !preprocessed_gtfs_ids.contains(&gtfs_id) {
                                trip_map.insert(gtfs_id, trips);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to fetch example trips from Nandi: {}. Continuing without trip data.", e);
                    }
                }
            }

            // Cache trip details for preprocessed feeds so get_example_trip returns without GraphQL
            *self.trip_details_cache.write().await = preprocessed_trip_details;

            trip_map
        };

        // Update start and end points
        self.update_start_end_points(&mut routes_by_gtfs, &route_data_by_gtfs);

        // Enrich with encoded_polyline from route_internal (DB-only field)
        self.enrich_routes_with_polylines(&mut routes_by_gtfs).await;

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

    /// Load GTFS data from preprocessed JSON files produced by the Nandi
    /// preprocessor pipeline. Returns (routes, patterns, stops, loaded_gtfs_ids).
    async fn load_preprocessed_data(
        &self,
        preprocessed_dir: &str,
    ) -> AppResult<(
        Vec<NandiRoutesRes>,
        Vec<NandiPatternDetails>,
        Vec<GTFSStop>,
        HashSet<String>,
    )> {
        let dir = std::path::Path::new(preprocessed_dir);

        // Check if directory exists
        if !dir.exists() {
            return Err(AppError::Internal(format!(
                "Preprocessed data directory not found: {}",
                preprocessed_dir
            )));
        }

        let manifest = self.verify_preprocessed_manifest(dir).await?;
        info!(
            "Preprocessed manifest verified: version={}, {} feeds",
            manifest.version,
            manifest.gtfs_feeds.len()
        );

        let mut all_routes = Vec::new();
        let mut all_patterns = Vec::new();
        let mut all_stops = Vec::new();
        let mut loaded_gtfs_ids = HashSet::new();

        // Load routes.json
        let routes_path = dir.join("routes.json");
        if routes_path.exists() {
            let json = tokio::fs::read_to_string(&routes_path)
                .await
                .map_err(|e| AppError::Internal(format!("Failed to read routes.json: {}", e)))?;
            let routes_by_gtfs: HashMap<String, Vec<NandiRoutesRes>> = serde_json::from_str(&json)
                .map_err(|e| AppError::Internal(format!("Failed to parse routes.json: {}", e)))?;
            for (gtfs_id, routes) in routes_by_gtfs {
                loaded_gtfs_ids.insert(gtfs_id.clone());
                info!(
                    "Loaded {} routes for gtfs_id={} from preprocessed files",
                    routes.len(),
                    gtfs_id
                );
                all_routes.extend(routes);
            }
        } else {
            warn!("routes.json not found in {}", preprocessed_dir);
        }

        // Load stops.json
        let stops_path = dir.join("stops.json");
        if stops_path.exists() {
            let json = tokio::fs::read_to_string(&stops_path)
                .await
                .map_err(|e| AppError::Internal(format!("Failed to read stops.json: {}", e)))?;
            let stops_by_gtfs: HashMap<String, Vec<GTFSStop>> = serde_json::from_str(&json)
                .map_err(|e| AppError::Internal(format!("Failed to parse stops.json: {}", e)))?;
            for (gtfs_id, stops) in stops_by_gtfs {
                loaded_gtfs_ids.insert(gtfs_id.clone());
                info!(
                    "Loaded {} stops for gtfs_id={} from preprocessed files",
                    stops.len(),
                    gtfs_id
                );
                all_stops.extend(stops);
            }
        } else {
            warn!("stops.json not found in {}", preprocessed_dir);
        }

        // Load patterns.json
        let patterns_path = dir.join("patterns.json");
        if patterns_path.exists() {
            let json = tokio::fs::read_to_string(&patterns_path)
                .await
                .map_err(|e| AppError::Internal(format!("Failed to read patterns.json: {}", e)))?;
            let patterns_by_gtfs: HashMap<String, Vec<NandiPatternDetails>> =
                serde_json::from_str(&json).map_err(|e| {
                    AppError::Internal(format!("Failed to parse patterns.json: {}", e))
                })?;
            for (gtfs_id, patterns) in patterns_by_gtfs {
                loaded_gtfs_ids.insert(gtfs_id.clone());
                info!(
                    "Loaded {} patterns for gtfs_id={} from preprocessed files",
                    patterns.len(),
                    gtfs_id
                );
                all_patterns.extend(patterns);
            }
        } else {
            warn!("patterns.json not found in {}", preprocessed_dir);
        }

        let missing_feeds: Vec<&String> = manifest
            .gtfs_feeds
            .iter()
            .filter(|f| !loaded_gtfs_ids.contains(*f))
            .collect();
        if !missing_feeds.is_empty() {
            return Err(AppError::Internal(format!(
                "Preprocessed data incomplete: manifest lists feeds not found in data: {:?}",
                missing_feeds
            )));
        }

        info!(
            "Preprocessed data loaded: {} routes, {} stops, {} patterns for {} GTFS feeds (manifest version={})",
            all_routes.len(),
            all_stops.len(),
            all_patterns.len(),
            loaded_gtfs_ids.len(),
            manifest.version
        );

        Ok((all_routes, all_patterns, all_stops, loaded_gtfs_ids))
    }

    async fn verify_preprocessed_manifest(
        &self,
        dir: &std::path::Path,
    ) -> AppResult<PreprocessedManifest> {
        let manifest_path = dir.join("metadata.json");
        if !manifest_path.exists() {
            return Err(AppError::Internal(format!(
                "metadata.json not found in {} — preprocessed copy is incomplete",
                dir.display()
            )));
        }

        let raw = tokio::fs::read_to_string(&manifest_path)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to read metadata.json: {}", e)))?;
        let manifest: PreprocessedManifest = serde_json::from_str(&raw)
            .map_err(|e| AppError::Internal(format!("Failed to parse metadata.json: {}", e)))?;

        if manifest.files.is_empty() {
            return Err(AppError::Internal(
                "metadata.json lists no files".to_string(),
            ));
        }

        for (name, meta) in &manifest.files {
            let file_path = dir.join(name);
            if !file_path.exists() {
                return Err(AppError::Internal(format!(
                    "metadata.json lists {} but it is missing on disk",
                    name
                )));
            }
            if meta.count <= 0 {
                return Err(AppError::Internal(format!(
                    "Preprocessed file {} has zero records per manifest",
                    name
                )));
            }
            let actual = sha256_of_file(&file_path).await?;
            if actual != meta.sha256 {
                return Err(AppError::Internal(format!(
                    "Checksum mismatch for {}: manifest={}, actual={} — corrupt or truncated copy",
                    name,
                    &meta.sha256.get(..12).unwrap_or(&meta.sha256),
                    &actual.get(..12).unwrap_or(&actual),
                )));
            }
        }

        Ok(manifest)
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

    async fn build_route_service_tiers(
        &self,
        csv_map: HashMap<String, HashMap<String, ServiceTierType>>,
        all_routes: &[NandiRoutesRes],
    ) -> HashMap<String, HashMap<String, ServiceTierType>> {
        let Some(op_svc) = self.operator_service.get() else {
            return csv_map;
        };

        let mut routes_by_gtfs: HashMap<String, HashSet<String>> = HashMap::new();
        for route in all_routes {
            let Some((gtfs_id, route_code)) = route.id.split_once(':') else {
                continue;
            };
            if !SUPPORTED_OPERATOR_GTFS_IDS.contains(&gtfs_id) {
                continue;
            }
            routes_by_gtfs
                .entry(gtfs_id.to_string())
                .or_default()
                .insert(route_code.to_string());
        }

        // Prior snapshot's tiers — used to carry state forward on DB failure so a blip
        // doesn't strip serviceTierType from the published data.
        let prior = self.data.load();
        let mut merged = csv_map;
        for (gtfs_id, all_route_ids) in routes_by_gtfs {
            let uncovered: Vec<String> = match merged.get(&gtfs_id) {
                Some(csv_inner) => all_route_ids
                    .into_iter()
                    .filter(|r| !csv_inner.contains_key(r))
                    .collect(),
                None => all_route_ids.into_iter().collect(),
            };
            if uncovered.is_empty() {
                continue;
            }

            match op_svc
                .get_service_tiers_for_routes(&gtfs_id, &uncovered)
                .await
            {
                Ok(db_map) => {
                    info!(
                        "Route service tiers for {}: DB fallback resolved {}/{} uncovered routes",
                        gtfs_id,
                        db_map.len(),
                        uncovered.len()
                    );
                    if db_map.is_empty() {
                        continue;
                    }
                    let inner = merged.entry(gtfs_id).or_default();
                    for (route_id, tier) in db_map {
                        inner.entry(route_id).or_insert(tier);
                    }
                }
                Err(e) => {
                    let prior_inner = prior.route_service_tiers_by_gtfs.get(&gtfs_id);
                    let carried = prior_inner
                        .map(|m| uncovered.iter().filter(|r| m.contains_key(*r)).count())
                        .unwrap_or(0);
                    warn!(
                        "Route service tiers for {}: DB fallback failed ({}); carrying {} prior-cycle tiers forward",
                        gtfs_id, e, carried
                    );
                    if let Some(prior_inner) = prior_inner {
                        let inner = merged.entry(gtfs_id).or_default();
                        for route_id in &uncovered {
                            if let Some(tier) = prior_inner.get(route_id) {
                                inner.entry(route_id.clone()).or_insert(tier.clone());
                            }
                        }
                    }
                }
            }
        }
        merged
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
                color: route.color,
                trip_count: route
                    .trip_count
                    .or_else(|| trip_counts.get(route_code).copied()),
                stop_count: route.stop_count.or_else(|| {
                    stop_counts
                        .get(gtfs_id)
                        .and_then(|r| r.get(route_code))
                        .copied()
                        .map(|c| c as i32)
                }),
                start_point: route.start_point,
                end_point: route.end_point,
                service_tier_type,
                encoded_polyline: None,
                // Waybill-derived; filled in per request by the handler, never
                // held in the in-memory GTFS snapshot.
                applicable_service_types: None,
            };
            routes_by_gtfs
                .entry(gtfs_id.to_string())
                .or_default()
                .insert(route_code.to_string(), route_res);
        }
        routes_by_gtfs
    }

    async fn enrich_routes_with_polylines(
        &self,
        routes_by_gtfs: &mut HashMap<String, HashMap<String, NandiRoutesRes>>,
    ) {
        let Some(op_svc) = self.operator_service.get() else {
            return;
        };
        for (gtfs_id, routes) in routes_by_gtfs.iter_mut() {
            let rows = match op_svc.get_routes_list(gtfs_id).await {
                Ok(r) => r,
                Err(e) => {
                    warn!(
                        "enrich_routes_with_polylines: get_routes_list({}) failed: {}",
                        gtfs_id, e
                    );
                    continue;
                }
            };
            for row in rows {
                if row.encoded_polyline.is_none() {
                    continue;
                }
                let route_key = row.route_id.to_string();
                if let Some(route) = routes.get_mut(&route_key) {
                    route.encoded_polyline = row.encoded_polyline;
                }
            }
        }
    }

    // Snapshot-boot equivalent of build_route_service_tiers: fills service_tier_type
    // on routes that shipped without one from CSV, so snapshot boots match a JSON boot.
    async fn enrich_routes_with_db_service_tiers(
        &self,
        routes_by_gtfs: &mut HashMap<String, HashMap<String, NandiRoutesRes>>,
    ) {
        let Some(op_svc) = self.operator_service.get() else {
            return;
        };
        for (gtfs_id, routes) in routes_by_gtfs.iter_mut() {
            if !SUPPORTED_OPERATOR_GTFS_IDS.contains(&gtfs_id.as_str()) {
                continue;
            }
            let uncovered: Vec<String> = routes
                .iter()
                .filter_map(|(rid, r)| r.service_tier_type.is_none().then(|| rid.clone()))
                .collect();
            if uncovered.is_empty() {
                continue;
            }
            match op_svc
                .get_service_tiers_for_routes(gtfs_id, &uncovered)
                .await
            {
                Ok(db_map) => {
                    info!(
                        "enrich_routes_with_db_service_tiers({}): resolved {}/{} uncovered routes",
                        gtfs_id,
                        db_map.len(),
                        uncovered.len()
                    );
                    for (route_id, tier) in db_map {
                        if let Some(route) = routes.get_mut(&route_id) {
                            route.service_tier_type = Some(tier);
                        }
                    }
                }
                Err(e) => warn!(
                    "enrich_routes_with_db_service_tiers({}): DB fallback failed ({}); leaving snapshot values as-is",
                    gtfs_id, e
                ),
            }
        }
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
            // Key the stops map by the stop *code*, not the id segment. OTP
            // stop ids happen to carry the code as their second segment, but
            // preprocessed data builds ids from the raw GTFS stop_id — for
            // metro platforms that stop_id differs from the stop_code, so the
            // id segment is unsafe as a key. `stop.code` is the real code on
            // every path.
            let stop_code = stop.code.as_str();

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

            if let Some(cid) = &cluster_id {
                by_cluster_set
                    .entry(gtfs_id.to_string())
                    .or_default()
                    .entry(cid.clone())
                    .or_default()
                    .insert(stop_code.to_string());
            }

            // A station and its child platforms share one stop_code. Keep the
            // entry that carries a station_id (a child pointing at its parent)
            // so parent_stop_code resolution works — a bare station entry has
            // no station_id and must not clobber a resolved child.
            match stop_data.stops.get(stop_code) {
                Some(existing)
                    if existing.station_id.is_some() && stop_res.station_id.is_none() =>
                {
                    // Existing entry already resolves a parent; don't clobber it.
                }
                _ => {
                    stop_data.stops.insert(stop_code.to_string(), stop_res);
                }
            }
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

        // Longest pattern per route, in sorted key order so route_data is deterministic (stable /stops across boots).
        let mut patterns_by_route: Vec<(String, Vec<&NandiPatternDetails>)> =
            patterns_by_route.into_iter().collect();
        patterns_by_route.sort_by(|a, b| a.0.cmp(&b.0));
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
                    cluster_id: stops_by_gtfs
                        .get(gtfs_id)
                        .and_then(|stops_data| stops_data.stops.get(&stop.code))
                        .and_then(|gtfs_stop| gtfs_stop.cluster_id.as_deref())
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

    /// Whether the service is configured to serve from preprocessed data.
    pub fn use_preprocessed_data(&self) -> bool {
        self.config.use_preprocessed_data
    }

    /// Directory containing the preprocessed JSON files (and trip_stoptimes/ shards).
    pub fn preprocessed_data_dir(&self) -> String {
        self.config.preprocessed_data_dir.clone()
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

    /// Direct routes from one stop to another, matched at cluster granularity.
    ///
    /// Takes stop_codes rather than cluster ids: callers hold stop codes (see the
    /// rider-app's fetchPossibleRoutes), so resolving the cluster here saves them a
    /// lookup per request and keeps their cache keys on stop codes.
    ///
    /// Each stop is widened to its cluster siblings via `by_cluster_id`, then each
    /// sibling is resolved to `(route_code, sequence_num)` pairs through `by_stop`.
    /// A route qualifies when it serves both ends and the destination sits
    /// downstream of the source. No `by_route` scan is needed — `by_stop` already
    /// carries the sequence numbers.
    ///
    /// A stop with no cluster_id (unclustered, or at the sentinel coord) falls back
    /// to a single-stop match on its own code, so the result is never worse than an
    /// exact stop_code join.
    ///
    /// Direct routes only; transfers are not considered. Like the destinations
    /// endpoint, this walks the representative (longest) pattern per route built at
    /// startup, so shorter patterns on the same route are not enumerated.
    pub fn get_routes_between_stops(
        &self,
        gtfs_id: &str,
        from_stop_code: &str,
        to_stop_code: &str,
    ) -> AppResult<Vec<ClusterRouteConnection>> {
        let data = self.data.load_full();
        let gtfs_id = clean_identifier(gtfs_id);
        let from_stop_code = clean_identifier(from_stop_code);
        let to_stop_code = clean_identifier(to_stop_code);

        // Unknown gtfs_id is a configuration error → 404. Unknown stop_codes
        // inside a known feed are semantically "no connecting routes" → 200 [].
        let stops_data = data.stops_by_gtfs.get(&gtfs_id).ok_or_else(|| {
            AppError::NotFound(format!("Stops data not found for gtfs_id: {}", gtfs_id))
        })?;

        // Widen a stop to its cluster siblings, or to just itself when it carries
        // no cluster_id. An unknown stop_code yields nothing to match on.
        let widen = |stop_code: &str, side: &str| -> Option<(Vec<String>, Option<String>)> {
            let stop = match stops_data.stops.get(stop_code) {
                Some(s) => s,
                None => {
                    info!(
                        gtfs_id = %gtfs_id,
                        stop_code = %stop_code,
                        side = %side,
                        "routes between stops: stop not found in feed",
                    );
                    return None;
                }
            };
            match stop.cluster_id.as_ref() {
                Some(cid) => stops_data
                    .by_cluster_id
                    .get(cid)
                    .cloned()
                    .map(|siblings| (siblings, Some(cid.clone())))
                    .or_else(|| Some((vec![stop_code.to_string()], Some(cid.clone())))),
                None => {
                    debug!(
                        gtfs_id = %gtfs_id,
                        stop_code = %stop_code,
                        side = %side,
                        "routes between stops: no cluster_id, falling back to single-stop match",
                    );
                    Some((vec![stop_code.to_string()], None))
                }
            }
        };

        let (src_siblings, src_cluster) = match widen(&from_stop_code, "source") {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };
        let (dst_siblings, dst_cluster) = match widen(&to_stop_code, "destination") {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };

        // Same cluster (or literally the same stop) is not a journey. Compared on
        // cluster identity so the two sides of one junction don't count either.
        let same_place = match (&src_cluster, &dst_cluster) {
            (Some(a), Some(b)) => a == b,
            _ => from_stop_code == to_stop_code,
        };
        if same_place {
            info!(
                gtfs_id = %gtfs_id,
                from_stop_code = %from_stop_code,
                to_stop_code = %to_stop_code,
                "routes between stops: source and destination are the same place, returning empty list",
            );
            return Ok(Vec::new());
        }

        let src_siblings = &src_siblings;
        let dst_siblings = &dst_siblings;

        let route_data = match data.route_data_by_gtfs.get(&gtfs_id) {
            Some(r) => r,
            None => return Ok(Vec::new()),
        };

        // Earliest stop of the source cluster on each route — boarding as early as
        // possible maximises what is reachable downstream.
        let mut src_by_route: HashMap<Arc<str>, (Arc<str>, i32)> = HashMap::new();
        for sib in src_siblings {
            if let Some(idxs) = route_data.by_stop.get(sib) {
                for &i in idxs {
                    if let Some(m) = route_data.mappings.get(i) {
                        src_by_route
                            .entry(m.route_code.clone())
                            .and_modify(|existing| {
                                if m.sequence_num < existing.1 {
                                    *existing = (m.stop_code.clone(), m.sequence_num);
                                }
                            })
                            .or_insert((m.stop_code.clone(), m.sequence_num));
                    }
                }
            }
        }

        if src_by_route.is_empty() {
            return Ok(Vec::new());
        }

        // Probe the source index with the destination cluster's stops, keeping the
        // earliest qualifying alighting point downstream of boarding.
        let mut best_by_route: HashMap<Arc<str>, ClusterRouteConnection> = HashMap::new();
        for sib in dst_siblings {
            let idxs = match route_data.by_stop.get(sib) {
                Some(v) => v,
                None => continue,
            };
            for &i in idxs {
                let m = match route_data.mappings.get(i) {
                    Some(m) => m,
                    None => continue,
                };
                let (src_code, src_seq) = match src_by_route.get(&m.route_code) {
                    Some(v) => v,
                    None => continue,
                };
                if m.sequence_num <= *src_seq {
                    continue;
                }
                let candidate = ClusterRouteConnection {
                    route_code: m.route_code.to_string(),
                    source_stop_code: src_code.to_string(),
                    source_sequence_num: *src_seq,
                    destination_stop_code: m.stop_code.to_string(),
                    destination_sequence_num: m.sequence_num,
                };
                best_by_route
                    .entry(m.route_code.clone())
                    .and_modify(|existing| {
                        if candidate.destination_sequence_num < existing.destination_sequence_num {
                            *existing = candidate.clone();
                        }
                    })
                    .or_insert(candidate);
            }
        }

        let mut out: Vec<ClusterRouteConnection> = best_by_route.into_values().collect();
        out.sort_by(|a, b| a.route_code.cmp(&b.route_code));
        info!(
            gtfs_id = %gtfs_id,
            from_stop_code = %from_stop_code,
            to_stop_code = %to_stop_code,
            source_cluster_id = ?src_cluster,
            destination_cluster_id = ?dst_cluster,
            src_siblings = src_siblings.len(),
            dst_siblings = dst_siblings.len(),
            routes = out.len(),
            "routes between stops: result",
        );
        Ok(out)
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

        // Preprocessed mode: never call Nandi — a cache miss is NotFound.
        if self.config.use_preprocessed_data {
            warn!(
                "Example trip details for gtfs_id={} route={} (trip_feed={}) not in preprocessed cache; \
                 not calling Nandi (use_preprocessed_data=true)",
                clean_gtfs, clean_route, trip_feed
            );
            return Err(AppError::NotFound(format!(
                "Example trip details not found for route {}",
                route_code
            )));
        }

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

    /// Build route → example trip and trip details from pattern data for preprocessed feeds.
    /// This allows example-trip API to work without calling Nandi GraphQL when using preprocessed data.
    fn build_example_trip_from_patterns(
        all_pattern_details: &[NandiPatternDetails],
        preprocessed_gtfs_ids: &HashSet<String>,
    ) -> (
        HashMap<String, HashMap<String, String>>,
        HashMap<String, HashMap<String, TripDetails>>,
    ) {
        let mut trip_map: HashMap<String, HashMap<String, String>> = HashMap::new();
        let mut details_map: HashMap<String, HashMap<String, TripDetails>> = HashMap::new();

        for pattern in all_pattern_details {
            let (gtfs_id, route_code) = match pattern.route_id.split_once(':') {
                Some((g, r)) => (g.to_string(), r.to_string()),
                None => continue,
            };
            if !preprocessed_gtfs_ids.contains(&gtfs_id) {
                continue;
            }
            let clean_gtfs = clean_identifier(&gtfs_id);
            let clean_route = clean_identifier(&route_code);

            let trip_id = match pattern.trips.first() {
                Some(t) => clean_identifier(&t.id),
                None => continue,
            };
            if trip_id.is_empty() {
                continue;
            }

            // One example per route: first pattern wins
            trip_map
                .entry(clean_gtfs.clone())
                .or_default()
                .entry(clean_route.clone())
                .or_insert_with(|| trip_id.clone());

            // Build trip details from pattern stops so get_example_trip can
            // return without GraphQL. Schedule times come from the example
            // trip the preprocessor recorded (seconds since midnight, matching
            // OTP's scheduledArrival/Departure). Older data without times
            // defaults to 0 — same as before this field existed.
            let stops: Vec<TripStopDetail> = pattern
                .stops
                .iter()
                .enumerate()
                .map(|(i, s)| TripStopDetail {
                    stop_id: s.id.clone(),
                    stop_code: s.code.clone(),
                    stop_name: Some(s.name.clone()),
                    platform_code: s.platform_code.clone(),
                    lat: s.lat,
                    lon: s.lon,
                    scheduled_arrival: s.arrival_time.unwrap_or(0),
                    scheduled_departure: s.departure_time.unwrap_or(0),
                    headsign: s
                        .headsign
                        .as_deref()
                        .map(|h| serde_json::Value::String(h.to_string()))
                        .unwrap_or(serde_json::Value::Null),
                    stop_position: s.stop_sequence.unwrap_or(i as i32),
                })
                .collect();

            let details = TripDetails {
                trip_id: trip_id.clone(),
                stops,
            };
            details_map
                .entry(clean_gtfs.clone())
                .or_default()
                .entry(clean_route.clone())
                .or_insert(details);
        }

        (trip_map, details_map)
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
