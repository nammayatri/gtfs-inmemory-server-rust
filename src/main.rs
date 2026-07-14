use actix_web::{web, App, HttpServer};
use gtfs_routes_service::{
    environment, handlers::routes, middleware::*, swagger::ApiDoc,
    tools::prometheus::prometheus_metrics,
};
use shared::tools::logger::setup_tracing;
use std::{env, net::Ipv4Addr};
use tracing::{error, info};
use tracing_actix_web::TracingLogger;
use utoipa::OpenApi;
use utoipa_swagger_ui::{Config, SwaggerUi};

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    // Load configuration from Dhall
    let dhall_config_path = env::var("DHALL_CONFIG")
        .unwrap_or_else(|_| "./dhall-configs/dev/gtfs_in_memory_server_rust.dhall".to_string());
    let app_config = environment::read_dhall_config(&dhall_config_path).unwrap_or_else(|err| {
        println!("Dhall Config Reading Error: {}", err);
        std::process::exit(1);
    });
    info!("Configuration loaded successfully from Dhall");

    let _guard = setup_tracing(app_config.logger_cfg);

    // Create application state
    let port = app_config.port;
    let polling_enabled = app_config.polling_enabled;
    let app_state = environment::AppState::new(app_config).await?;

    // Start background polling task if enabled
    if polling_enabled {
        let gtfs_service_clone = app_state.gtfs_service.clone();
        tokio::spawn(async move {
            if let Err(e) = gtfs_service_clone.start_polling().await {
                error!("Polling task failed: {}", e);
            }
        });
    }

    // Start background task to update CHALO vehicle cache every 10 seconds
    let chalo_cache_clone = app_state.chalo_vehicle_cache.clone();
    tokio::spawn(async move {
        chalo_cache_clone.start_background_update_task().await;
    });

    // Start background task to periodically refresh OSRTC station list
    if let Some(osrtc_cache) = app_state.osrtc_cache.clone() {
        tokio::spawn(async move {
            osrtc_cache.start_background_refresh_task().await;
        });
    }

    let prometheus = prometheus_metrics();

    let openapi = ApiDoc::openapi();

    // Create and run the web server with performance optimizations
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(app_state.clone()))
            .wrap(IncomingRequestMetrics)
            .wrap(TracingLogger::<DomainRootSpanBuilder>::new())
            .wrap(prometheus.clone())
            .service(
                // The spec is *served* at the app-relative "/api-docs/openapi.json", but the
                // frontend is told to *fetch* it via a relative "../api-docs/openapi.json".
                // The browser resolves that against whatever subpath the UI is actually
                // mounted under (e.g. "/gtfs-inmem/swagger-ui/" behind the reverse proxy ->
                // "/gtfs-inmem/api-docs/openapi.json"), so no prefix has to be hardcoded and
                // it still works when served at the root. See Config::new docs: it only
                // changes the fetch URL, not where SwaggerUi::url exposes the document.
                SwaggerUi::new("/swagger-ui/{_:.*}")
                    .url("/api-docs/openapi.json", openapi.clone())
                    .config(Config::new(["../api-docs/openapi.json"])),
            )
            .configure(routes::create_routes)
    })
    .bind((Ipv4Addr::UNSPECIFIED, port))?
    .workers(num_cpus::get())
    .run()
    .await?;

    Ok(())
}
