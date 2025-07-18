use actix_web::{
    web::{Data, Json, Path, Query},
    HttpResponse,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info};

use crate::environment::AppState;
use crate::models::{
    GTFSStop, NandiRoutesRes, RouteStopMapping, StopCodeFromProviderStopCodeResponse,
    VehicleServiceTypeResponse,
};
use crate::graphql::TripQueryParams;
use crate::{
    models::LatLong,
    tools::error::{AppError, AppResult},
};

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    limit: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct GetAllRoutesByIdsRequest {
    #[serde(rename = "gtfsId")]
    pub gtfs_id: String,
    #[serde(rename = "routeIds")]
    pub route_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct GetAllStopsByIdsRequest {
    #[serde(rename = "gtfsId")]
    pub gtfs_id: String,
    #[serde(rename = "stopIds")]
    pub stop_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct GetAllRouteStopMappingsByRouteCodesRequest {
    #[serde(rename = "gtfsId")]
    pub gtfs_id: String,
    #[serde(rename = "routeCodes")]
    pub route_codes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct GetAllRouteStopMappingsByStopCodesRequest {
    #[serde(rename = "gtfsId")]
    pub gtfs_id: String,
    #[serde(rename = "stopCodes")]
    pub stop_codes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct GetAllVehiclesByIdsRequest {
    #[serde(rename = "vehicleIds")]
    pub vehicle_ids: Vec<String>,
}

pub fn create_routes(cfg: &mut actix_web::web::ServiceConfig) {
    cfg.service(
        actix_web::web::scope("")
            .route(
                "/route/{gtfs_id}/{route_id}",
                actix_web::web::get().to(get_route),
            )
            .route("/routes/{gtfs_id}", actix_web::web::get().to(get_routes))
            .route(
                "/route-stop-mapping/{gtfs_id}/route/{route_code}",
                actix_web::web::get().to(get_route_stop_mapping_by_route),
            )
            .route(
                "/route-stop-mapping/{gtfs_id}/stop/{stop_code}",
                actix_web::web::get().to(get_route_stop_mapping_by_stop),
            )
            .route(
                "/routes/{gtfs_id}/fuzzy/{query}",
                actix_web::web::get().to(get_routes_fuzzy),
            )
            .route("/stops/{gtfs_id}", actix_web::web::get().to(get_stops))
            .route(
                "/stop/{gtfs_id}/{stop_code}",
                actix_web::web::get().to(get_stop),
            )
            .route(
                "/stops/{gtfs_id}/fuzzy/{query}",
                actix_web::web::get().to(get_stops_fuzzy),
            )
            .route(
                "/stop-code/{gtfs_id}/{provider_stop_code}",
                actix_web::web::get().to(get_stop_code_from_provider_stop_code),
            )
            .route(
                "/station-children/{gtfs_id}/{stop_code}",
                actix_web::web::get().to(get_station_children),
            )
            .route("/ready", actix_web::web::get().to(readiness_probe))
            .route("/version/{gtfs_id}", actix_web::web::get().to(get_version))
            .route(
                "/vehicle/{vehicle_no}/service-type",
                actix_web::web::get().to(get_service_type_by_vehicle),
            )
            .route("/memory-stats", actix_web::web::get().to(get_memory_stats))
            .route(
                "/cached-data",
                actix_web::web::get().to(get_all_cached_data),
            )
            .route("/config", actix_web::web::get().to(get_config))
            .route("/graphql", actix_web::web::post().to(graphql_query))
            .route(
                "/connection-stats",
                actix_web::web::get().to(get_connection_stats),
            )
            .route(
                "/trip/{trip_id}",
                actix_web::web::get().to(get_trip_data),
            )
            .route(
                "/trip-cache/stats",
                actix_web::web::get().to(get_trip_cache_stats),
            )
            .route(
                "/trip-cache/clear",
                actix_web::web::post().to(clear_trip_cache),
            )
            .route(
                "/refresh-data",
                actix_web::web::post().to(force_refresh_data),
            )
            .route(
                "/getAllRoutesByIds",
                actix_web::web::post().to(get_all_routes_by_ids),
            )
            .route(
                "/getAllStopsByIds",
                actix_web::web::post().to(get_all_stops_by_ids),
            )
            .route(
                "/getAllRouteStopMappingsByRouteCodes",
                actix_web::web::post().to(get_all_route_stop_mappings_by_route_codes),
            )
            .route(
                "/getAllRouteStopMappingsByStopCodes",
                actix_web::web::post().to(get_all_route_stop_mappings_by_stop_codes),
            )
            .route(
                "/getAllVehiclesByIds",
                actix_web::web::post().to(get_all_vehicles_by_ids),
            )

    );
}

async fn get_route(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, route_id) = path.into_inner();
    let route = app_state
        .gtfs_service
        .get_route(&gtfs_id, &route_id)
        .await?;
    Ok(HttpResponse::Ok().json(route))
}

async fn get_routes(app_state: Data<AppState>, path: Path<String>) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let routes = app_state.gtfs_service.get_routes(&gtfs_id).await?;
    Ok(HttpResponse::Ok().json(routes))
}

async fn get_route_stop_mapping_by_route(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, route_code) = path.into_inner();
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mapping_by_route(&gtfs_id, &route_code)
        .await?;
    Ok(HttpResponse::Ok().json(mappings))
}



async fn get_route_stop_mapping_by_stop(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, stop_code) = path.into_inner();
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mapping_by_stop(&gtfs_id, &stop_code)
        .await?;
    Ok(HttpResponse::Ok().json(mappings))
}

async fn get_routes_fuzzy(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    query: Query<LimitQuery>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, query_str) = path.into_inner();
    let routes = app_state.gtfs_service.get_routes(&gtfs_id).await?;
    let query_lower = query_str.to_lowercase();

    let mut unique_routes: HashMap<String, NandiRoutesRes> = HashMap::new();

    for route in routes {
        let matches = route
            .long_name
            .as_ref()
            .map(|n| n.to_lowercase().contains(&query_lower))
            .unwrap_or(false)
            || route
                .short_name
                .as_ref()
                .map(|n| n.to_lowercase().contains(&query_lower))
                .unwrap_or(false)
            || route.id.to_lowercase().contains(&query_lower);

        if matches {
            unique_routes.insert(route.id.clone(), route);
            if let Some(limit) = query.limit {
                if unique_routes.len() >= limit as usize {
                    break;
                }
            }
        }
    }

    Ok(HttpResponse::Ok().json(unique_routes.into_values().collect::<Vec<_>>()))
}

async fn get_stops(app_state: Data<AppState>, path: Path<String>) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let stops = app_state.gtfs_service.get_stops(&gtfs_id).await?;
    Ok(HttpResponse::Ok().json(stops))
}

async fn get_stop(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, stop_code) = path.into_inner();
    let stop = app_state
        .gtfs_service
        .get_stop(&gtfs_id, &stop_code)
        .await
        .map(
            |GTFSStop {
                 code,
                 name,
                 lat,
                 lon,
                 ..
             }| RouteStopMapping {
                stop_code: code.to_string(),
                stop_name: name.to_string(),
                stop_point: LatLong { lat: lat, lon: lon },
                estimated_travel_time_from_previous_stop: None,
                geo_json: None,
                gates: None,
                provider_code: "GTFS".to_string(),
                route_code: "UNKNOWN".to_string(),
                vehicle_type: "BUS".to_string(),
                sequence_num: 0,
            },
        )?;
    Ok(HttpResponse::Ok().json(stop))
}

async fn get_stops_fuzzy(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    query: Query<LimitQuery>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, query_str) = path.into_inner();
    let stops = app_state.gtfs_service.get_stops(&gtfs_id).await?;
    let query_lower = query_str.to_lowercase();

    let mut unique_stops: HashMap<String, Arc<RouteStopMapping>> = HashMap::new();

    for stop in stops {
        let matches = stop.stop_name.to_lowercase().contains(&query_lower)
            || stop.stop_code.to_lowercase().contains(&query_lower);

        if matches {
            unique_stops.insert(stop.stop_code.clone(), stop.clone());
            if let Some(limit) = query.limit {
                if unique_stops.len() >= limit as usize {
                    break;
                }
            }
        }
    }

    Ok(HttpResponse::Ok().json(unique_stops.into_values().collect::<Vec<_>>()))
}

async fn get_stop_code_from_provider_stop_code(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, provider_stop_code) = path.into_inner();
    let stop_code = app_state
        .gtfs_service
        .get_provider_stop_code(&gtfs_id, &provider_stop_code)
        .await?;
    Ok(HttpResponse::Ok().json(StopCodeFromProviderStopCodeResponse { stop_code }))
}

async fn get_station_children(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, stop_code) = path.into_inner();
    let children = app_state
        .gtfs_service
        .get_station_children(&gtfs_id, &stop_code)
        .await?;
    Ok(HttpResponse::Ok().json(children))
}

async fn readiness_probe(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    if !app_state.gtfs_service.is_ready().await {
        return Err(AppError::NotReady(
            "Service not ready - still loading initial data".to_string(),
        ));
    }

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "message": "Service is ready to handle requests"
    })))
}

async fn get_version(app_state: Data<AppState>, path: Path<String>) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let version = app_state.gtfs_service.get_version(&gtfs_id).await?;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "gtfs_id": gtfs_id,
        "version": version
    })))
}

async fn get_service_type_by_vehicle(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let vehicle_no = path.into_inner();
    let vehicle_data = app_state
        .db_vehicle_reader
        .get_vehicle_data(&vehicle_no)
        .await?;
    Ok(HttpResponse::Ok().json(VehicleServiceTypeResponse {
        vehicle_no: vehicle_data.vehicle_no,
        service_type: vehicle_data.service_type,
        waybill_id: Some(vehicle_data.waybill_id),
        schedule_no: Some(vehicle_data.schedule_no),
        last_updated: vehicle_data.last_updated,
        route_id: vehicle_data.route_id
    }))
}

async fn get_memory_stats(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let stats = app_state.gtfs_service.get_memory_stats().await;
    Ok(HttpResponse::Ok().json(serde_json::json!(stats)))
}

async fn get_all_cached_data(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let cached_data = app_state.gtfs_service.get_all_cached_data().await;
    Ok(HttpResponse::Ok().json(
        serde_json::to_value(cached_data)
            .map_err(|e| AppError::Internal(format!("Failed to serialize cached data: {}", e)))?,
    ))
}

async fn get_config(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    Ok(HttpResponse::Ok().json(app_state.config.clone()))
}

#[derive(Debug, serde::Deserialize)]
struct GraphQLRequest {
    query: String,
    variables: Option<serde_json::Value>,
    operation_name: Option<String>,
    city: Option<String>,
    #[serde(alias = "feedId")]
    gtfs_id: Option<String>, // accept "feedId" as "gtfs_id"
}

async fn graphql_query(
    app_state: Data<AppState>,
    payload: Json<GraphQLRequest>,
) -> AppResult<HttpResponse> {
    let city = payload
        .city
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let result = app_state
        .gtfs_service
        .execute_graphql_query(
            &city,
            &payload.query,
            payload.variables.clone(),
            payload.operation_name.clone(),
            payload.gtfs_id.clone(),
        )
        .await?;
    Ok(HttpResponse::Ok().json(result))
}

async fn get_connection_stats(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    // Get configuration-based connection stats
    let db_stats = serde_json::json!({
        "database": {
            "max_connections": app_state.config.db_max_connections,
            "min_connections": app_state.config.db_min_connections,
            "acquire_timeout": app_state.config.db_acquire_timeout,
            "idle_timeout": app_state.config.db_idle_timeout,
            "max_lifetime": app_state.config.db_max_lifetime
        }
    });

    // Get HTTP client stats
    let http_stats = serde_json::json!({
        "http_client": {
            "connection_limit": app_state.config.connection_limit,
            "pool_idle_timeout": app_state.config.http_pool_idle_timeout,
            "tcp_keepalive": app_state.config.http_tcp_keepalive
        }
    });

    // Get system TCP stats
    let tcp_stats = serde_json::json!({
        "tcp_optimizations": {
            "tcp_nodelay": true,
            "http2_enabled": false,
            "connection_reuse": true
        }
    });

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "connection_stats": {
            "database": db_stats["database"],
            "http_client": http_stats["http_client"],
            "tcp_optimizations": tcp_stats["tcp_optimizations"]
        }
    })))
}

async fn get_trip_data(
    app_state: Data<AppState>,
    path: Path<String>,
    query: Query<TripQueryParams>,
) -> AppResult<HttpResponse> {
    let trip_id = path.into_inner();
    let query_params = query.into_inner();
    
    let trip_data = app_state
        .trip_service
        .get_trip_data(&trip_id, query_params.gtfs_id, query_params.city)
        .await?;
    
    Ok(HttpResponse::Ok().json(trip_data))
}

async fn get_trip_cache_stats(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let stats = app_state.trip_service.get_cache_stats().await;
    Ok(HttpResponse::Ok().json(stats))
}

async fn clear_trip_cache(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    app_state.trip_service.clear_cache().await?;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "message": "Trip cache cleared successfully"
    })))
}

async fn force_refresh_data(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    // Trigger a background refresh of GTFS data
    let gtfs_service = app_state.gtfs_service.clone();
    
    // Spawn a background task to refresh data
    tokio::spawn(async move {
        info!("Starting forced GTFS data refresh...");
        match gtfs_service.force_refresh_data().await {
            Ok(_) => info!("GTFS data refresh completed successfully"),
            Err(e) => error!("GTFS data refresh failed: {}", e),
        }
    });
    
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "message": "GTFS data refresh initiated in background",
        "status": "started"
    })))
}

async fn get_all_routes_by_ids(
    app_state: Data<AppState>,
    payload: Json<GetAllRoutesByIdsRequest>,
) -> AppResult<HttpResponse> {
    let routes = app_state
        .gtfs_service
        .get_routes_by_ids(&payload.gtfs_id, payload.route_ids.clone())
        .await?;
    
    Ok(HttpResponse::Ok().json(routes))
}

async fn get_all_stops_by_ids(
    app_state: Data<AppState>,
    payload: Json<GetAllStopsByIdsRequest>,
) -> AppResult<HttpResponse> {
    let stops = app_state
        .gtfs_service
        .get_stops_by_ids(&payload.gtfs_id, payload.stop_ids.clone())
        .await?;
    
    Ok(HttpResponse::Ok().json(stops))
}

async fn get_all_route_stop_mappings_by_route_codes(
    app_state: Data<AppState>,
    payload: Json<GetAllRouteStopMappingsByRouteCodesRequest>,
) -> AppResult<HttpResponse> {
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mappings_by_route_codes(&payload.gtfs_id, payload.route_codes.clone())
        .await?;
    
    Ok(HttpResponse::Ok().json(mappings))
}

async fn get_all_route_stop_mappings_by_stop_codes(
    app_state: Data<AppState>,
    payload: Json<GetAllRouteStopMappingsByStopCodesRequest>,
) -> AppResult<HttpResponse> {
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mappings_by_stop_codes(&payload.gtfs_id, payload.stop_codes.clone())
        .await?;
    
    Ok(HttpResponse::Ok().json(mappings))
}

async fn get_all_vehicles_by_ids(
    app_state: Data<AppState>,
    payload: Json<GetAllVehiclesByIdsRequest>,
) -> AppResult<HttpResponse> {
    let vehicles = app_state
        .db_vehicle_reader
        .get_vehicles_by_ids(payload.vehicle_ids.clone())
        .await?;
    
    Ok(HttpResponse::Ok().json(vehicles))
}