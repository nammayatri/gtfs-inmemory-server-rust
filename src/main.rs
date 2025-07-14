use actix_web::{web, App, HttpServer};
use gtfs_routes_service::{
    environment, handlers::routes, middleware::*, tools::prometheus::prometheus_metrics,
};
use shared::tools::logger::setup_tracing;
use std::{env, net::Ipv4Addr};
use tracing::{error, info};
use tracing_actix_web::TracingLogger;

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    // Load configuration from Dhall
    let dhall_config_path = env::var("DHALL_CONFIG")
        .unwrap_or_else(|_| "./dhall-configs/dev/gtfs_in_memory_server_rust.dhall".to_string());
    let app_config = environment::read_dhall_config(&dhall_config_path).unwrap_or_else(|err| {
        error!("Dhall Config Reading Error: {}", err);
        std::process::exit(1);
    });
    info!("Configuration loaded successfully from Dhall");

    let _guard = setup_tracing(app_config.logger_cfg);

    // Create application state
    let port = app_config.port;
    let app_state = environment::AppState::new(app_config).await?;

    // Start background polling task
    let gtfs_service_clone = app_state.gtfs_service.clone();
    tokio::spawn(async move {
        if let Err(e) = gtfs_service_clone.start_polling().await {
            error!("Polling task failed: {}", e);
        }
    });

    let prometheus = prometheus_metrics();

    // Create and run the web server with performance optimizations
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(app_state.clone()))
            .wrap(IncomingRequestMetrics)
            .wrap(TracingLogger::<DomainRootSpanBuilder>::new())
            .wrap(prometheus.clone())
            .configure(routes::create_routes)
    })
    .bind((Ipv4Addr::UNSPECIFIED, port))?
    .workers(num_cpus::get())
    .run()
    .await?;

    Ok(())
}
