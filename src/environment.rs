use anyhow::Result;
use csv::ReaderBuilder;
use serde::{Deserialize, Serialize};
use serde_json;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::RwLock;

use crate::services::{
    chalo_vehicle_cache::ChaloVehicleCache,
    db_employee_reader::{DBEmployeeReader, EmployeeReader, MockEmployeeReader},
    db_vehicle_reader::{DBVehicleReader, MockDBVehicleReader, VehicleDataReader},
    db_vehicle_reader_internal::{
        DBVehicleReaderInternal, MockDBVehicleReaderInternal, VehicleDataReaderInternal,
    },
    fleet_operator::{DBFleetOperatorService, FleetOperatorService, MockFleetOperatorService},
    gtfs_service::GTFSService,
    metro_graph::MetroGraph,
    operator::{DBOperatorService, MockOperatorService, OperatorService},
    osrtc_station_cache::OsrtcStationCache,
    service_hopper::{load_all as load_service_hopper, FeedHoppers},
    trip_service::TripService,
};
use crate::tools::dhall::read_dhall_config as dhall_read_config;
use crate::tools::error::AppError;
use arc_swap::ArcSwap;
use shared::tools::logger::LoggerConfig;
use tracing::{error, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtpInstance {
    pub url: String,
    pub identifier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtpConfig {
    pub city_based_instances: Vec<OtpInstance>,
    pub gtfs_id_based_instances: Vec<OtpInstance>,
    pub default_instance: OtpInstance,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub logger_cfg: LoggerConfig,
    pub database_url: Option<String>,
    pub internal_database_url: Option<String>,
    pub db_max_connections: u32,
    pub db_min_connections: u32,
    pub db_acquire_timeout: u64,
    pub db_idle_timeout: u64,
    pub db_max_lifetime: u64,
    pub cache_duration: u64,
    pub otp_instances: OtpConfig,
    pub polling_enabled: bool,
    pub polling_interval: u64,
    pub process_batch_size: usize,
    pub port: u16,
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
    pub ignored_trip_ids: Vec<String>,
    pub bhubaneswar_cache_update_interval: u64,
    pub bhubaneswar_external_auth: Option<String>,
    /// When true, load GTFS data from preprocessed JSON files (produced by
    /// Nandi preprocessor) instead of calling Nandi/OTP APIs.
    #[serde(default)]
    pub use_preprocessed_data: bool,
    /// Directory containing preprocessed JSON files (routes.json, stops.json,
    /// patterns.json, route_stops.json, metadata.json).
    /// Defaults to "./assets" if not set.
    #[serde(default = "default_preprocessed_data_dir")]
    pub preprocessed_data_dir: String,
    pub phone_number_hash_key: String,
    /// Enable schedule-based active trip reconciliation (default: false)
    pub enable_schedule_reconciliation: bool,
    pub osrtc_base_url: Option<String>,
    pub osrtc_username: Option<String>,
    pub osrtc_secret_key: Option<String>,
    pub osrtc_station_refresh_interval_hours: u64,
    pub osrtc_feed_key: Option<String>,
    /// Base URL of an OSRM server (e.g. "http://localhost:5050") used to compute
    /// route polylines during reprocess. Absent/empty ⇒ polyline step is skipped.
    #[serde(default)]
    pub osrm_url: Option<String>,
    /// Static fallback list of feeds served from the `gtfs_*` tables over
    /// `internal_database_url` instead of the preprocessed files (trips still
    /// come from the preprocessed data either way). This is no longer the
    /// only way to turn DB mode on: `gtfs_feed.data_source` in Postgres is now
    /// the live, per-feed, authoritative setting (docs/gtfs-editor.md "Feed
    /// data source"), editable without a restart from the `/internal/gtfs-editor`
    /// dashboard. This list only matters for a feed that has no `gtfs_feed`
    /// row yet - it is then treated as `data_source = 'db'` from boot, so an
    /// operator relying on this old config still gets DB mode without first
    /// creating a row by hand. Once a row exists for a feed, that row wins,
    /// even if it says `preprocessed` and the feed is still named here. Empty
    /// (the default) keeps every feed without a row on preprocessed data.
    #[serde(default)]
    pub gtfs_db_feeds: Vec<String>,
    /// How often each pod checks `gtfs_feed.version` for the DB feeds and
    /// rebuilds the ones that moved.
    #[serde(default = "default_gtfs_version_poll_seconds")]
    pub gtfs_version_poll_seconds: u64,
    /// GTFS metadata editor (docs/gtfs-editor.md). Every field is optional so
    /// existing dhall configs still parse; the editor stays off unless enabled.
    #[serde(default)]
    pub gtfs_editor_enabled: bool,
    /// JWKS of the Pomerium that fronts the editor: https://... or file:///...
    #[serde(default)]
    pub gtfs_editor_pomerium_jwks_url: Option<String>,
    /// Expected `aud` of the Pomerium JWT: the dashboard host.
    #[serde(default)]
    pub gtfs_editor_audience: Option<String>,
    /// Emails created as admins on their first authenticated request.
    #[serde(default)]
    pub gtfs_editor_bootstrap_admins: Vec<String>,
    /// Base64 32-byte key that encrypts TOTP secrets (secrets dhall).
    #[serde(default)]
    pub gtfs_editor_totp_key: Option<String>,
    #[serde(default)]
    pub gtfs_editor_session_hours: Option<u64>,
    /// Directory the dashboard's static files are served from.
    #[serde(default)]
    pub gtfs_editor_ui_dir: Option<String>,
    /// Outbound webhooks (docs/gtfs-editor.md section 12): GIMS calls a
    /// configured URL when something happens to a DB feed - most usefully once
    /// every pod is serving a committed edit, which is when a downstream cache
    /// such as the S3/CloudFront frontline layer can safely be rebuilt.
    ///
    /// Off by default. Turning it on also needs a non-empty
    /// `gtfs_webhook_allowed_hosts`: see that field.
    ///
    /// Only the **seed**: a saved `gtfs_webhook_settings` row supersedes it,
    /// and from then on this value does nothing (docs section 12.5).
    #[serde(default)]
    pub gtfs_webhooks_enabled: bool,
    /// Hosts a webhook may call. An entry written `.example.com` matches that
    /// domain and its subdomains.
    ///
    /// Empty (the default) means no webhook can fire, even with
    /// `gtfs_webhooks_enabled = True`: the feature fails closed.
    ///
    /// Only the **seed**, on the same terms as `gtfs_webhooks_enabled`: this
    /// is the list in force until an admin saves one from the dashboard, and
    /// is ignored afterwards.
    #[serde(default)]
    pub gtfs_webhook_allowed_hosts: Vec<String>,
    /// How this pod identifies itself when it reports which feed version it is
    /// serving. Defaults to the `POD_NAME` environment variable (the downward
    /// API), then the hostname. Two pods must never share it: a shared id makes
    /// the fleet look smaller than it is, which would fire a webhook early.
    #[serde(default)]
    pub gtfs_pod_id: Option<String>,
    /// Where the GTFS editor reads bus pings to suggest a route's map line
    /// from GPS (docs/gtfs-editor.md section 17). Absent - the default - and
    /// that endpoint answers 503 `gps_unavailable`; nothing else changes.
    #[serde(default)]
    pub gtfs_gps: Option<GtfsGpsConfig>,
    /// The password of `gtfs_gps.user` (secrets dhall).
    #[serde(default)]
    pub gtfs_gps_clickhouse_password: Option<String>,
    /// Longest an ops ETA override may run. Bounds the failure mode where an override set
    /// during a disruption outlives it because nobody came back to clear it.
    #[serde(default)]
    pub max_eta_override_seconds: Option<u64>,
}

/// The GPS block of [`AppConfig`]. Only `url` and `user` are required.
///
/// The ClickHouse cluster behind it is production and shared, and its user
/// may well have write rights: the editor reads it only through
/// `services::clickhouse_reader`, which sends `readonly=2` with every query,
/// allows nothing but a bounded SELECT, and runs one query at a time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GtfsGpsConfig {
    /// ClickHouse's HTTP interface, e.g. "https://clickhouse.internal:8443"
    /// (8123/8443, not the native 9000/9440).
    pub url: String,
    pub user: String,
    /// `database.table` holding the pings. Default `atlas_kafka.amnex_direct_data`.
    #[serde(default)]
    pub table: Option<String>,
    /// How many days back a suggestion may read, at most. Default 14. It reads
    /// today first and stops going back once `enough_bus_days` are found.
    #[serde(default)]
    pub days: Option<u32>,
    /// Stop reading further back once this many bus-days (a bus that carried
    /// the route number on a day) are found. Default 12.
    #[serde(default)]
    pub enough_bus_days: Option<u32>,
    /// The feeds whose routes these pings describe. Default `["chennai_bus"]`.
    #[serde(default)]
    pub feeds: Option<Vec<String>>,
    /// Bus-days read per suggestion, spread over the days. Default 30.
    #[serde(default)]
    pub max_bus_days: Option<u32>,
    /// Rows per ClickHouse answer. Default 100: some network paths to the
    /// cluster stall on answers of a few hundred rows.
    #[serde(default)]
    pub page_rows: Option<u32>,
    /// The whole suggestion, OSRM included. Default 45 s and at most 50: a
    /// proxy in front of the editor gives a request 60.
    #[serde(default)]
    pub timeout_seconds: Option<u32>,
}

impl AppConfig {
    /// The deployment's webhook policy, which seeds
    /// `services::webhook::LivePolicy` and is what is in force only while no
    /// `gtfs_webhook_settings` row has been saved.
    pub fn webhook_policy(&self) -> crate::services::webhook::WebhookPolicy {
        crate::services::webhook::WebhookPolicy {
            enabled: self.gtfs_webhooks_enabled,
            allowed_hosts: self
                .gtfs_webhook_allowed_hosts
                .iter()
                .map(|h| h.trim().to_ascii_lowercase())
                .filter(|h| !h.is_empty())
                .collect(),
        }
    }

    pub fn max_eta_override_seconds(&self) -> u64 {
        self.max_eta_override_seconds
            .unwrap_or(MAX_ETA_OVERRIDE_SECONDS_DEFAULT)
    }
}

/// 12 hours — longer than any single disruption an ops shift would sit through, short enough
/// that a forgotten override cannot survive into the next day.
const MAX_ETA_OVERRIDE_SECONDS_DEFAULT: u64 = 43200;

fn default_preprocessed_data_dir() -> String {
    "./assets".to_string()
}

fn default_gtfs_version_poll_seconds() -> u64 {
    5
}

impl OtpConfig {
    pub fn find_instance_by_gtfs_id(&self, gtfs_id: &str) -> Option<&OtpInstance> {
        self.gtfs_id_based_instances
            .iter()
            .find(|instance| instance.identifier == gtfs_id)
    }

    pub fn find_instance_by_city(&self, city: &str) -> Option<&OtpInstance> {
        self.city_based_instances
            .iter()
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

pub fn read_dhall_config(dhall_config_path: &str) -> Result<AppConfig> {
    dhall_read_config(dhall_config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read Dhall config: {}", e))
}

async fn create_database_pool(config: &AppConfig) -> Result<PgPool, AppError> {
    let db_url = config
        .database_url
        .as_ref()
        .ok_or_else(|| AppError::Internal("DATABASE_URL is not set".to_string()))?;
    create_pool_from_url(db_url, config).await
}

async fn create_pool_from_url(db_url: &str, config: &AppConfig) -> Result<PgPool, AppError> {
    let pool = PgPoolOptions::new()
        .max_connections(config.db_max_connections)
        .min_connections(config.db_min_connections)
        .acquire_timeout(Duration::from_secs(config.db_acquire_timeout))
        .idle_timeout(Duration::from_secs(config.db_idle_timeout))
        .max_lifetime(Duration::from_secs(config.db_max_lifetime))
        .connect(db_url)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to connect to database: {}", e)))?;

    info!("Database connection pool created successfully.");
    Ok(pool)
}

#[derive(Clone)]
pub struct AppState {
    pub gtfs_service: Arc<GTFSService>,
    pub db_vehicle_reader: Arc<dyn VehicleDataReader>,
    pub db_vehicle_reader_internal: Arc<dyn VehicleDataReaderInternal>,
    pub db_employee_reader: Arc<dyn EmployeeReader>,
    pub operator_service: Arc<dyn OperatorService>,
    pub fleet_operator_service: Arc<dyn FleetOperatorService>,
    pub trip_service: Arc<TripService>,
    pub chalo_vehicle_cache: Arc<ChaloVehicleCache>,
    pub osrtc_cache: Option<Arc<OsrtcStationCache>>,
    pub config: AppConfig,
    pub bus_registration_mapping: Arc<HashMap<String, HashMap<String, String>>>,
    pub fleet_list: Arc<HashMap<String, HashMap<String, Vec<String>>>>,
    pub vehicle_service_sub_types: Arc<HashMap<String, HashMap<String, Vec<String>>>>,
    /// Metro transit graphs loaded from preprocessed JSON (gtfs_id -> MetroGraph)
    pub metro_graphs: Arc<HashMap<String, MetroGraph>>,
    /// Precomputed metro interchange indexes (gtfs_id -> per-day-type graphs).
    ///
    /// Loaded at startup from the Nandi planner's `metro_hops.json` and swapped
    /// wholesale when the preprocessed data refreshes, so readers never block
    /// and never observe a half-loaded index. See
    /// [`crate::services::service_hopper`].
    pub service_hopper: Arc<ArcSwap<HashMap<String, Arc<FeedHoppers>>>>,
    pub depot_manager_details: Arc<HashMap<String, crate::models::DepotManagerDetails>>,
    pub fleet_tag_list: Arc<HashMap<String, HashMap<String, String>>>,
    pub chennai_service_type_cache: Arc<RwLock<HashMap<String, (Instant, Option<String>)>>>,
}

impl AppState {
    pub async fn new(app_config: AppConfig) -> Result<AppState> {
        // Initialize services
        let gtfs_service = Arc::new(GTFSService::new(app_config.clone()).await?);

        // Create shared database pool or use mock readers
        #[allow(clippy::type_complexity)]
        let (
            db_vehicle_reader,
            db_employee_reader,
            operator_service,
            db_vehicle_reader_internal,
            fleet_operator_service,
        ): (
            Arc<dyn VehicleDataReader>,
            Arc<dyn EmployeeReader>,
            Arc<dyn OperatorService>,
            Arc<dyn VehicleDataReaderInternal>,
            Arc<dyn FleetOperatorService>,
        ) = if let Some(db_url) = &app_config.database_url {
            if db_url.contains("localhost") {
                // For local development, fall back to mock readers on connection failure
                match create_database_pool(&app_config).await {
                    Ok(pool) => {
                        info!("Successfully connected to the local database.");
                        let vehicle_reader =
                            Arc::new(DBVehicleReader::new(pool.clone(), &app_config));
                        // operator and internal reader use ONLY the internal pool
                        let (
                            operator_svc,
                            vehicle_reader_internal,
                            fleet_op_svc,
                            internal_pool_opt,
                        ): (
                            Arc<dyn OperatorService>,
                            Arc<dyn VehicleDataReaderInternal>,
                            Arc<dyn FleetOperatorService>,
                            Option<PgPool>,
                        ) = if let Some(internal_url) = &app_config.internal_database_url {
                            info!("Connecting to internal database (local)...");
                            match create_pool_from_url(internal_url, &app_config).await {
                                Ok(internal_pool) => {
                                    info!("Internal database pool created successfully.");
                                    (
                                        Arc::new(DBOperatorService::new(
                                            internal_pool.clone(),
                                            app_config.osrm_url.clone(),
                                        )),
                                        Arc::new(DBVehicleReaderInternal::new(
                                            internal_pool.clone(),
                                        )),
                                        Arc::new(DBFleetOperatorService::new(
                                            internal_pool.clone(),
                                        )),
                                        Some(internal_pool),
                                    )
                                }
                                Err(e) => {
                                    error!("Failed to connect to internal database: {}. Using mock internal services.", e);
                                    (
                                        Arc::new(MockOperatorService::new()),
                                        Arc::new(MockDBVehicleReaderInternal::new())
                                            as Arc<dyn VehicleDataReaderInternal>,
                                        Arc::new(MockFleetOperatorService::new())
                                            as Arc<dyn FleetOperatorService>,
                                        None,
                                    )
                                }
                            }
                        } else {
                            info!("No internal_database_url set; using mock internal services.");
                            (
                                Arc::new(MockOperatorService::new()),
                                Arc::new(MockDBVehicleReaderInternal::new())
                                    as Arc<dyn VehicleDataReaderInternal>,
                                Arc::new(MockFleetOperatorService::new())
                                    as Arc<dyn FleetOperatorService>,
                                None,
                            )
                        };
                        let employee_reader = Arc::new(DBEmployeeReader::new(
                            pool.clone(),
                            internal_pool_opt,
                            &app_config,
                        ));
                        (
                            vehicle_reader,
                            employee_reader,
                            operator_svc,
                            vehicle_reader_internal,
                            fleet_op_svc,
                        )
                    }
                    Err(e) => {
                        error!("Failed to connect to the local database: {}. Falling back to mock DB readers.", e);
                        (
                            Arc::new(MockDBVehicleReader::new()),
                            Arc::new(MockEmployeeReader::new()),
                            Arc::new(MockOperatorService::new()),
                            Arc::new(MockDBVehicleReaderInternal::new())
                                as Arc<dyn VehicleDataReaderInternal>,
                            Arc::new(MockFleetOperatorService::new())
                                as Arc<dyn FleetOperatorService>,
                        )
                    }
                }
            } else {
                // For non-local (production) environments, require a valid DB connection
                info!("Connecting to production database...");
                let pool = create_database_pool(&app_config)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to create database pool: {}", e))?;
                let vehicle_reader = Arc::new(DBVehicleReader::new(pool.clone(), &app_config));
                // operator and internal reader use ONLY the internal pool
                let (operator_svc, vehicle_reader_internal, fleet_op_svc, internal_pool_opt): (
                    Arc<dyn OperatorService>,
                    Arc<dyn VehicleDataReaderInternal>,
                    Arc<dyn FleetOperatorService>,
                    Option<PgPool>,
                ) = if let Some(internal_url) = &app_config.internal_database_url {
                    info!("Connecting to internal database (production)...");
                    let internal_pool = create_pool_from_url(internal_url, &app_config)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to create internal database pool: {}", e)
                        })?;
                    (
                        Arc::new(DBOperatorService::new(
                            internal_pool.clone(),
                            app_config.osrm_url.clone(),
                        )),
                        Arc::new(DBVehicleReaderInternal::new(internal_pool.clone())),
                        Arc::new(DBFleetOperatorService::new(internal_pool.clone())),
                        Some(internal_pool),
                    )
                } else {
                    info!("No internal_database_url set; using mock internal services.");
                    (
                        Arc::new(MockOperatorService::new()),
                        Arc::new(MockDBVehicleReaderInternal::new())
                            as Arc<dyn VehicleDataReaderInternal>,
                        Arc::new(MockFleetOperatorService::new()) as Arc<dyn FleetOperatorService>,
                        None,
                    )
                };
                let employee_reader = Arc::new(DBEmployeeReader::new(
                    pool.clone(),
                    internal_pool_opt,
                    &app_config,
                ));
                (
                    vehicle_reader,
                    employee_reader,
                    operator_svc,
                    vehicle_reader_internal,
                    fleet_op_svc,
                )
            }
        } else {
            // If no DATABASE_URL is provided, use the mock readers
            info!("No DATABASE_URL found, using mock DB readers");
            (
                Arc::new(MockDBVehicleReader::new()),
                Arc::new(MockEmployeeReader::new()),
                Arc::new(MockOperatorService::new()),
                Arc::new(MockDBVehicleReaderInternal::new()) as Arc<dyn VehicleDataReaderInternal>,
                Arc::new(MockFleetOperatorService::new()) as Arc<dyn FleetOperatorService>,
            )
        };

        gtfs_service.set_operator_service(operator_service.clone());
        gtfs_service.load_initial_data().await?;

        let trip_service = Arc::new(TripService::new(gtfs_service.clone()));

        let mut chalo_vehicle_cache = ChaloVehicleCache::new(
            app_config.bhubaneswar_external_auth.clone(),
            gtfs_service.clone(),
        )?;
        chalo_vehicle_cache.set_update_interval(app_config.bhubaneswar_cache_update_interval);
        let chalo_vehicle_cache = Arc::new(chalo_vehicle_cache);

        let osrtc_cache = match (
            &app_config.osrtc_base_url,
            &app_config.osrtc_username,
            &app_config.osrtc_secret_key,
        ) {
            (Some(base_url), Some(username), Some(secret_key)) => {
                let cache = OsrtcStationCache::new(
                    base_url.clone(),
                    username.clone(),
                    secret_key.clone(),
                    app_config.osrtc_station_refresh_interval_hours * 3600,
                )?;
                if let Err(e) = cache.initialize().await {
                    error!(
                        "OSRTC station cache initial load failed (will retry in background): {}",
                        e
                    );
                }
                Some(Arc::new(cache))
            }
            _ => {
                info!("OSRTC credentials not configured; OSRTC station cache disabled");
                None
            }
        };

        // Load bus registration mapping from CSV
        let bus_registration_mapping = Arc::new(Self::load_bus_registration_mapping().await?);
        let metro_graphs = Arc::new(Self::load_metro_graphs(&app_config));

        let service_hopper = Arc::new(ArcSwap::from_pointee(load_service_hopper(
            &app_config.preprocessed_data_dir,
        )));

        // Load depot manager details from CSV
        let depot_manager_details = Arc::new(Self::load_depot_manager_details().await?);

        let app_state = AppState {
            gtfs_service,
            db_vehicle_reader,
            db_vehicle_reader_internal,
            db_employee_reader,
            operator_service,
            fleet_operator_service,
            trip_service,
            chalo_vehicle_cache,
            osrtc_cache,
            config: app_config,
            bus_registration_mapping,
            fleet_list: Arc::new(Self::load_fleet_list().await?),
            vehicle_service_sub_types: Arc::new(Self::load_vehicle_service_sub_types().await?),
            metro_graphs,
            service_hopper,
            depot_manager_details,
            fleet_tag_list: Arc::new(Self::load_fleet_tag_list().await?),
            chennai_service_type_cache: Arc::new(RwLock::new(HashMap::new())),
        };

        Ok(app_state)
    }

    /// Re-read `metro_hops.json` and swap the indexes in atomically.
    ///
    /// Called by the background watcher after the feed refreshes. The planner
    /// writes the artifact in the same preprocessor run that produces the rest
    /// of the preprocessed data, so a GTFS refresh is the signal that a newer
    /// artifact may be on disk. Reloading is milliseconds for a metro-sized
    /// feed, so this is cheaper than reasoning about what changed.
    pub fn rebuild_service_hopper(&self) {
        let indexes = load_service_hopper(&self.config.preprocessed_data_dir);
        self.service_hopper.store(Arc::new(indexes));
    }

    async fn load_fleet_tag_list() -> Result<HashMap<String, HashMap<String, String>>> {
        let file_path = "./assets/fleet_tag_list.csv";

        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                info!("fleet_tag_list.csv file not found, proceeding without fleet tag list data");
                return Ok(HashMap::new());
            }
        };

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read fleet_tag_list.csv: {}", e))?;

        let mut reader = ReaderBuilder::new()
            .has_headers(true)
            .from_reader(contents.as_bytes());

        // gtfs_id -> vehicle_no -> tag_number
        let mut fleet_map: HashMap<String, HashMap<String, String>> = HashMap::new();

        for result in reader.records() {
            match result {
                Ok(record) => {
                    // Expected columns: gtfs_id, vehicle_no, tag_number
                    let (Some(gtfs_id), Some(vehicle_no), Some(tag_number)) =
                        (record.get(0), record.get(1), record.get(2))
                    else {
                        continue;
                    };

                    if tag_number.is_empty() {
                        continue;
                    }

                    let by_gtfs = fleet_map
                        .entry(gtfs_id.trim().to_string())
                        .or_insert_with(HashMap::new);

                    by_gtfs.insert(vehicle_no.trim().to_string(), tag_number.trim().to_string());
                }
                Err(e) => {
                    error!("Error parsing fleet_tag_list CSV row: {}", e);
                }
            }
        }

        info!("Loaded fleet tag list for {} GTFS feeds", fleet_map.len());

        Ok(fleet_map)
    }

    async fn load_fleet_list() -> Result<HashMap<String, HashMap<String, Vec<String>>>> {
        let file_path = "./assets/fleet_list.csv";

        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                info!("fleet_list.csv file not found, proceeding without fleet list data");
                return Ok(HashMap::new());
            }
        };

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read fleet_list.csv: {}", e))?;

        let mut reader = ReaderBuilder::new()
            .has_headers(true)
            .from_reader(contents.as_bytes());

        // gtfs_id -> vehicle_no -> eligible_pass_ids
        let mut fleet_map: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();

        for result in reader.records() {
            match result {
                Ok(record) => {
                    // Expected columns: gtfs_id, vehicle_no, eligible_pass_ids
                    let (Some(gtfs_id), Some(vehicle_no), Some(ids_str)) =
                        (record.get(0), record.get(1), record.get(2))
                    else {
                        continue;
                    };

                    let ids_list: Vec<String> = ids_str
                        .split('|')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();

                    if ids_list.is_empty() {
                        continue;
                    }

                    let by_gtfs = fleet_map.entry(gtfs_id.trim().to_string()).or_default();

                    by_gtfs.insert(vehicle_no.trim().to_string(), ids_list);
                }
                Err(e) => {
                    error!("Error parsing fleet_list CSV row: {}", e);
                }
            }
        }

        info!(
            "Loaded fleet pass configurations for {} GTFS feeds",
            fleet_map.len()
        );

        Ok(fleet_map)
    }

    async fn load_bus_registration_mapping() -> Result<HashMap<String, HashMap<String, String>>> {
        use crate::models::BusRegistrationMappingRecord;

        let file_path = "./assets/bus_registration_mapping.csv";

        // Check if file exists, if not return empty HashMap
        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                info!("bus_registration_mapping.csv file not found, proceeding without bus registration mapping data");
                return Ok(HashMap::new());
            }
        };

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read CSV file: {}", e))?;

        let mut reader = ReaderBuilder::new()
            .has_headers(true)
            .from_reader(contents.as_bytes());

        let mut mapping: HashMap<String, HashMap<String, String>> = HashMap::new();
        for result in reader.deserialize() {
            match result {
                Ok(record) => {
                    let record: BusRegistrationMappingRecord = record;
                    mapping
                        .entry(record.gtfs_id)
                        .or_default()
                        .insert(record.short_name, record.vehicle_no);
                }
                Err(e) => {
                    error!("Error parsing CSV row: {}", e);
                }
            }
        }

        info!(
            "Loaded bus registration mapping for {} GTFS IDs from CSV",
            mapping.len()
        );
        Ok(mapping)
    }

    async fn load_vehicle_service_sub_types(
    ) -> Result<HashMap<String, HashMap<String, Vec<String>>>> {
        let file_path = "./assets/vehicle_service_sub_types.csv";

        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                info!("vehicle_service_sub_types.csv file not found, proceeding without vehicle service sub types data");
                return Ok(HashMap::new());
            }
        };

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read vehicle_service_sub_types.csv: {}", e))?;

        let mut reader = ReaderBuilder::new()
            .has_headers(true)
            .from_reader(contents.as_bytes());

        // gtfs_id -> vehicle_no -> service_sub_types
        let mut sub_types_map: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();

        for result in reader.records() {
            match result {
                Ok(record) => {
                    // Expected columns: gtfs_id, vehicle_no, service_sub_types
                    let (Some(gtfs_id), Some(vehicle_no), Some(sub_types_str)) =
                        (record.get(0), record.get(1), record.get(2))
                    else {
                        continue;
                    };

                    let sub_types: Vec<String> = match serde_json::from_str(sub_types_str) {
                        Ok(vec) => vec,
                        Err(e) => {
                            error!(
                                "Error parsing service_sub_types JSON for vehicle {}: {}",
                                vehicle_no, e
                            );
                            continue;
                        }
                    };

                    let by_gtfs = sub_types_map.entry(gtfs_id.trim().to_string()).or_default();

                    by_gtfs.insert(vehicle_no.trim().to_string(), sub_types);
                }
                Err(e) => {
                    error!("Error parsing vehicle_service_sub_types CSV row: {}", e);
                }
            }
        }

        info!(
            "Loaded vehicle service sub types for {} GTFS feeds",
            sub_types_map.len()
        );

        Ok(sub_types_map)
    }

    /// Load metro transit graphs from preprocessed JSON files in the
    /// preprocessed data directory.
    ///
    /// Looks for `metro_graph.json` which contains a map of gtfs_id -> MetroGraph.
    /// If the file doesn't exist, returns an empty map (metro routing won't
    /// be available but all other APIs work fine).
    fn load_metro_graphs(config: &AppConfig) -> HashMap<String, MetroGraph> {
        let graph_file =
            std::path::Path::new(&config.preprocessed_data_dir).join("metro_graph.json");

        if !graph_file.exists() {
            info!(
                "No metro_graph.json found in {}, metro routing will not be available",
                config.preprocessed_data_dir
            );
            return HashMap::new();
        }

        info!("Loading metro graphs from {}", graph_file.display());

        match std::fs::read_to_string(&graph_file) {
            Ok(json) => match serde_json::from_str::<HashMap<String, MetroGraph>>(&json) {
                Ok(graphs) => {
                    for (gtfs_id, graph) in &graphs {
                        info!(
                            "Loaded metro graph for {}: {} nodes, {} routes",
                            gtfs_id,
                            graph.nodes.len(),
                            graph.route_stop_sequences.len()
                        );
                    }
                    info!("Loaded {} metro transit graphs", graphs.len());
                    graphs
                }
                Err(e) => {
                    error!(
                        "Failed to deserialize metro_graph.json: {}. Metro routing will not be available.",
                        e
                    );
                    HashMap::new()
                }
            },
            Err(e) => {
                error!(
                    "Failed to read metro_graph.json: {}. Metro routing will not be available.",
                    e
                );
                HashMap::new()
            }
        }
    }

    async fn load_depot_manager_details(
    ) -> Result<HashMap<String, crate::models::DepotManagerDetails>> {
        use crate::models::DepotManagerDetails;
        let file_path = "./assets/depot_manager_details.csv";

        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                info!("depot_manager_details.csv file not found, proceeding without depot manager data");
                return Ok(HashMap::new());
            }
        };

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read depot_manager_details.csv: {}", e))?;

        let mut reader = ReaderBuilder::new()
            .has_headers(true)
            .from_reader(contents.as_bytes());

        // phone_number -> DepotManagerDetails
        let mut phone_to_manager: HashMap<String, DepotManagerDetails> = HashMap::new();

        for result in reader.records() {
            match result {
                Ok(record) => {
                    // CSV columns: S. No.(0), Depot Code(1), Depot Name(2), Phone Number(3)
                    let (Some(depot_code), Some(depot_name), Some(phone_number)) =
                        (record.get(1), record.get(2), record.get(3))
                    else {
                        continue;
                    };

                    // CSV stores pre-computed HMAC-SHA256 hash of the phone number
                    let phone_hash = phone_number.trim().to_string();
                    let code = depot_code.trim().to_string();
                    let name = depot_name.trim().to_string();

                    if phone_hash.is_empty() || code.is_empty() || name.is_empty() {
                        continue;
                    }

                    let manager = DepotManagerDetails {
                        depot_code: code,
                        depot_name: name,
                        phone_number: phone_hash.clone(),
                    };

                    phone_to_manager.insert(phone_hash, manager);
                }
                Err(e) => {
                    error!("Error parsing depot_manager_details CSV row: {}", e);
                }
            }
        }

        info!(
            "Loaded depot manager details for {} managers",
            phone_to_manager.len()
        );

        Ok(phone_to_manager)
    }
}
