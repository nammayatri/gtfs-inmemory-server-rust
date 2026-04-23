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
    operator::{DBOperatorService, MockOperatorService, OperatorService},
    osrtc_station_cache::OsrtcStationCache,
    trip_service::TripService,
};
use crate::tools::dhall::read_dhall_config as dhall_read_config;
use crate::tools::error::AppError;
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
    pub phone_number_hash_key: String,
    /// Enable schedule-based active trip reconciliation (default: false)
    pub enable_schedule_reconciliation: bool,
    pub osrtc_base_url: Option<String>,
    pub osrtc_username: Option<String>,
    pub osrtc_secret_key: Option<String>,
    pub osrtc_station_refresh_interval_hours: u64,
    pub osrtc_feed_key: Option<String>,
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
    pub conductor_details: Arc<HashMap<String, crate::models::MinimalEmployee>>,
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
                        let employee_reader =
                            Arc::new(DBEmployeeReader::new(pool.clone(), &app_config));
                        // operator and internal reader use ONLY the internal pool
                        let (operator_svc, vehicle_reader_internal, fleet_op_svc): (
                            Arc<dyn OperatorService>,
                            Arc<dyn VehicleDataReaderInternal>,
                            Arc<dyn FleetOperatorService>,
                        ) = if let Some(internal_url) = &app_config.internal_database_url {
                            info!("Connecting to internal database (local)...");
                            match create_pool_from_url(internal_url, &app_config).await {
                                Ok(internal_pool) => {
                                    info!("Internal database pool created successfully.");
                                    (
                                        Arc::new(DBOperatorService::new(internal_pool.clone())),
                                        Arc::new(DBVehicleReaderInternal::new(
                                            internal_pool.clone(),
                                        )),
                                        Arc::new(DBFleetOperatorService::new(internal_pool)),
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
                            )
                        };
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
                let employee_reader = Arc::new(DBEmployeeReader::new(pool.clone(), &app_config));
                // operator and internal reader use ONLY the internal pool
                let (operator_svc, vehicle_reader_internal, fleet_op_svc): (
                    Arc<dyn OperatorService>,
                    Arc<dyn VehicleDataReaderInternal>,
                    Arc<dyn FleetOperatorService>,
                ) = if let Some(internal_url) = &app_config.internal_database_url {
                    info!("Connecting to internal database (production)...");
                    let internal_pool = create_pool_from_url(internal_url, &app_config)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to create internal database pool: {}", e)
                        })?;
                    (
                        Arc::new(DBOperatorService::new(internal_pool.clone())),
                        Arc::new(DBVehicleReaderInternal::new(internal_pool.clone())),
                        Arc::new(DBFleetOperatorService::new(internal_pool)),
                    )
                } else {
                    info!("No internal_database_url set; using mock internal services.");
                    (
                        Arc::new(MockOperatorService::new()),
                        Arc::new(MockDBVehicleReaderInternal::new())
                            as Arc<dyn VehicleDataReaderInternal>,
                        Arc::new(MockFleetOperatorService::new()) as Arc<dyn FleetOperatorService>,
                    )
                };
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
                    error!("OSRTC station cache initial load failed (will retry in background): {}", e);
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

        // Load conductor details from CSV
        let conductor_details = Arc::new(Self::load_conductor_details().await?);

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
            conductor_details,
            depot_manager_details,
            fleet_tag_list: Arc::new(Self::load_fleet_tag_list().await?),
            chennai_service_type_cache: Arc::new(RwLock::new(HashMap::new())),
        };

        Ok(app_state)
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

    async fn load_conductor_details() -> Result<HashMap<String, crate::models::MinimalEmployee>> {
        use crate::models::MinimalEmployee;
        let file_path = "./assets/conductor_details.csv";

        let mut file = match File::open(file_path).await {
            Ok(file) => file,
            Err(_) => {
                info!("conductor_details.csv file not found, proceeding without conductor details data");
                return Ok(HashMap::new());
            }
        };

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read conductor_details.csv: {}", e))?;

        let mut reader = ReaderBuilder::new()
            .has_headers(true)
            .from_reader(contents.as_bytes());

        let mut phone_to_employee: HashMap<String, MinimalEmployee> = HashMap::new();

        for result in reader.records() {
            match result {
                Ok(record) => {
                    // CSV columns: Token No(0), Phone Number(1), UPI ID(2), Depot Name(3), Name(4)
                    let (Some(token_no), Some(phone_number), Some(depot_name), Some(name)) =
                        (record.get(0), record.get(1), record.get(3), record.get(4))
                    else {
                        continue;
                    };

                    // CSV stores pre-computed HMAC-SHA256 hash of the phone number
                    let phone_hash = phone_number.trim().to_string();
                    let token = token_no.trim().to_string();
                    let full_name = name.trim();
                    let depot = depot_name.trim().to_string();

                    if phone_hash.is_empty() || token.is_empty() {
                        continue;
                    }

                    let depot_opt = if depot.is_empty() || depot == "nan" {
                        None
                    } else {
                        Some(depot)
                    };

                    // Split name into first and last
                    let (first_name, last_name) = match full_name.split_once(' ') {
                        Some((first, last)) => (first.to_string(), Some(last.to_string())),
                        None => (full_name.to_string(), None),
                    };

                    let employee = MinimalEmployee {
                        token_no: Some(token),
                        first_name,
                        last_name,
                        mobile_no: None,
                        depot_name: depot_opt,
                    };

                    phone_to_employee.insert(phone_hash, employee);
                }
                Err(e) => {
                    error!("Error parsing conductor_details CSV row: {}", e);
                }
            }
        }

        info!(
            "Loaded conductor details for {} conductors",
            phone_to_employee.len()
        );

        Ok(phone_to_employee)
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
