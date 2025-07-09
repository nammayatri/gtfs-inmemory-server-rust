use axum::{
    extract::Request,
    middleware::{self, Next},
    response::Response,
};
use std::env;
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

mod config;
mod errors;
mod handlers;
mod models;
mod services;

use config::AppConfig;
use handlers::routes;
use services::{
    db_vehicle_reader::{DBVehicleReader, MockDBVehicleReader, VehicleDataReader},
    gtfs_service::GTFSService,
    otp_manager::OtpManager,
};
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub gtfs_service: Arc<GTFSService>,
    pub db_vehicle_reader: Arc<dyn VehicleDataReader>,
    pub otp_manager: Arc<OtpManager>,
    pub config: AppConfig,
}

// Middleware to log all API requests and responses
async fn logging_middleware(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let start = std::time::Instant::now();

    info!("API Request: {} {}", method, uri);

    let response = next.run(request).await;
    let latency = start.elapsed();

    info!(
        "API Response: {} {} - Status: {} - Latency: {:?}",
        method,
        uri,
        response.status(),
        latency
    );

    response
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load environment variables
    dotenv::dotenv().ok();

    // Initialize logging
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=debug"));

    let subscriber = FmtSubscriber::builder()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_file(true)
        .with_line_number(true)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    info!("Starting GTFS Routes Service...");

    // Load configuration
    let config = AppConfig::from_env()?;
    info!("Configuration loaded successfully");

    // Print all environment variables after successful bootup
    info!("=== Environment Variables After Bootup ===");
    let mut env_vars: Vec<(String, String)> = env::vars().collect();
    env_vars.sort_by(|a, b| a.0.cmp(&b.0)); // Sort alphabetically by key

    for (key, value) in env_vars {
        // Hide sensitive values for security
        if key.to_uppercase().contains("PASSWORD")
            || key.to_uppercase().contains("SECRET")
            || key.to_uppercase().contains("KEY")
            || key.to_uppercase().contains("TOKEN")
        {
            info!("{}=***HIDDEN***", key);
        } else {
            info!("{}={}", key, value);
        }
    }
    info!("=== End Environment Variables ===");

    // Initialize services
    let gtfs_service = Arc::new(GTFSService::new(config.clone()).await?);
    let db_vehicle_reader: Arc<dyn VehicleDataReader> = if let Some(db_url) = &config.database_url {
        if db_url.contains("localhost") {
            // For local development, fall back to mock reader on connection failure
            match DBVehicleReader::new(&config).await {
                Ok(reader) => {
                    info!("Successfully connected to the local database.");
                    Arc::new(reader)
                }
                Err(e) => {
                    error!("Failed to connect to the local database: {}. Falling back to mock DB reader.", e);
                    Arc::new(MockDBVehicleReader::new())
                }
            }
        } else {
            // For non-local (production) environments, require a valid DB connection
            info!("Connecting to production database...");
            let reader = DBVehicleReader::new(&config).await?;
            Arc::new(reader)
        }
    } else {
        // If no DATABASE_URL is provided, use the mock reader
        info!("No DATABASE_URL found, using mock DB reader");
        Arc::new(MockDBVehicleReader::new())
    };
    info!("Services initialized");

    // Start background polling task
    let gtfs_service_clone = gtfs_service.clone();
    tokio::spawn(async move {
        if let Err(e) = gtfs_service_clone.start_polling().await {
            error!("Polling task failed: {}", e);
        }
    });

    // Initialize OTP manager
    let otp_manager = Arc::new(OtpManager::new(config.otp_instances.clone()));
    info!("OTP manager initialized");

    // Create application state
    let app_state = AppState {
        gtfs_service,
        db_vehicle_reader,
        otp_manager,
        config: config.clone(),
    };

    // Create and run the web server
    let app = routes::create_router(app_state)
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn(logging_middleware));

    info!("Starting server on {}:{}", config.api_host, config.api_port);

    let listener =
        tokio::net::TcpListener::bind(format!("{}:{}", config.api_host, config.api_port)).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
