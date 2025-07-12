use axum::{
    extract::{Path, Query, State},
    response::Json,
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::models::{GTFSStop, NandiRoutesRes, RouteStopMapping, VehicleServiceTypeResponse};
use crate::AppState;
use crate::{config::AppConfig, models::StopCodeFromProviderStopCodeResponse};
use crate::{
    errors::{AppError, AppResult},
    models::LatLong,
};

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    limit: Option<i32>,
}

pub fn create_router(app_state: AppState) -> Router {
    Router::new()
        .route("/route/:gtfs_id/:route_id", get(get_route))
        .route("/routes/:gtfs_id", get(get_routes))
        .route(
            "/route-stop-mapping/:gtfs_id/route/:route_code",
            get(get_route_stop_mapping_by_route),
        )
        .route(
            "/route-stop-mapping/:gtfs_id/stop/:stop_code",
            get(get_route_stop_mapping_by_stop),
        )
        .route("/routes/:gtfs_id/fuzzy/:query", get(get_routes_fuzzy))
        .route("/stops/:gtfs_id", get(get_stops))
        .route("/stop/:gtfs_id/:stop_code", get(get_stop))
        .route("/stops/:gtfs_id/fuzzy/:query", get(get_stops_fuzzy))
        .route(
            "/stop-code/:gtfs_id/:provider_stop_code",
            get(get_stop_code_from_provider_stop_code),
        )
        .route(
            "/station-children/:gtfs_id/:stop_code",
            get(get_station_children),
        )
        .route("/ready", get(readiness_probe))
        .route("/version/:gtfs_id", get(get_version))
        .route(
            "/vehicle/:vehicle_no/service-type",
            get(get_service_type_by_vehicle),
        )
        .route("/memory-stats", get(get_memory_stats))
        .route("/cached-data", get(get_all_cached_data))
        .route("/config", get(get_config))
        .route("/graphql", post(graphql_query))
        .route("/connection-stats", get(get_connection_stats))
        .with_state(app_state)
}

async fn get_route(
    State(app_state): State<AppState>,
    Path((gtfs_id, route_id)): Path<(String, String)>,
) -> AppResult<Json<NandiRoutesRes>> {
    let route = app_state
        .gtfs_service
        .get_route(&gtfs_id, &route_id)
        .await?;
    Ok(Json(route))
}

async fn get_routes(
    State(app_state): State<AppState>,
    Path(gtfs_id): Path<String>,
) -> AppResult<Json<Vec<NandiRoutesRes>>> {
    let routes = app_state.gtfs_service.get_routes(&gtfs_id).await?;
    Ok(Json(routes))
}

async fn get_route_stop_mapping_by_route(
    State(app_state): State<AppState>,
    Path((gtfs_id, route_code)): Path<(String, String)>,
) -> AppResult<Json<Vec<Arc<RouteStopMapping>>>> {
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mapping_by_route(&gtfs_id, &route_code)
        .await?;
    // let max_sequence_mappings = get_max_sequence_route_stop_mapping(mappings);
    Ok(Json(mappings))
}

fn get_max_sequence_route_stop_mapping(
    all_mappings: Vec<Arc<RouteStopMapping>>,
) -> Vec<Arc<RouteStopMapping>> {
    if all_mappings.is_empty() {
        return Vec::new();
    }

    let mut seq_map: HashMap<i32, Arc<RouteStopMapping>> = HashMap::new();
    for mapping in all_mappings {
        seq_map.insert(mapping.sequence_num, mapping);
    }

    let mut result: Vec<Arc<RouteStopMapping>> = seq_map.into_values().collect();
    result.sort_by_key(|m| m.sequence_num);

    result
}

async fn get_route_stop_mapping_by_stop(
    State(app_state): State<AppState>,
    Path((gtfs_id, stop_code)): Path<(String, String)>,
) -> AppResult<Json<Vec<Arc<RouteStopMapping>>>> {
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mapping_by_stop(&gtfs_id, &stop_code)
        .await?;
    Ok(Json(mappings))
}

async fn get_routes_fuzzy(
    State(app_state): State<AppState>,
    Path((gtfs_id, query)): Path<(String, String)>,
    Query(params): Query<LimitQuery>,
) -> AppResult<Json<Vec<NandiRoutesRes>>> {
    let routes = app_state.gtfs_service.get_routes(&gtfs_id).await?;
    let query_lower = query.to_lowercase();

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
            if let Some(limit) = params.limit {
                if unique_routes.len() >= limit as usize {
                    break;
                }
            }
        }
    }

    Ok(Json(unique_routes.into_values().collect()))
}

async fn get_stops(
    State(app_state): State<AppState>,
    Path(gtfs_id): Path<String>,
) -> AppResult<Json<Vec<Arc<RouteStopMapping>>>> {
    let stops = app_state.gtfs_service.get_stops(&gtfs_id).await?;
    Ok(Json(stops))
}

async fn get_stop(
    State(app_state): State<AppState>,
    Path((gtfs_id, stop_code)): Path<(String, String)>,
) -> AppResult<Json<RouteStopMapping>> {
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
    Ok(Json(stop))
}

async fn get_stops_fuzzy(
    State(app_state): State<AppState>,
    Path((gtfs_id, query)): Path<(String, String)>,
    Query(params): Query<LimitQuery>,
) -> AppResult<Json<Vec<Arc<RouteStopMapping>>>> {
    let stops = app_state.gtfs_service.get_stops(&gtfs_id).await?;
    let query_lower = query.to_lowercase();

    let mut unique_stops: HashMap<String, Arc<RouteStopMapping>> = HashMap::new();

    for stop in stops {
        let matches = stop.stop_name.to_lowercase().contains(&query_lower)
            || stop.stop_code.to_lowercase().contains(&query_lower);

        if matches {
            unique_stops.insert(stop.stop_code.clone(), stop.clone());
            if let Some(limit) = params.limit {
                if unique_stops.len() >= limit as usize {
                    break;
                }
            }
        }
    }

    Ok(Json(unique_stops.into_values().collect()))
}

async fn get_stop_code_from_provider_stop_code(
    State(app_state): State<AppState>,
    Path((gtfs_id, provider_stop_code)): Path<(String, String)>,
) -> AppResult<Json<StopCodeFromProviderStopCodeResponse>> {
    let stop_code = app_state
        .gtfs_service
        .get_provider_stop_code(&gtfs_id, &provider_stop_code)
        .await?;
    Ok(Json(StopCodeFromProviderStopCodeResponse { stop_code }))
}

async fn get_station_children(
    State(app_state): State<AppState>,
    Path((gtfs_id, stop_code)): Path<(String, String)>,
) -> AppResult<Json<Vec<String>>> {
    let children = app_state
        .gtfs_service
        .get_station_children(&gtfs_id, &stop_code)
        .await?;
    Ok(Json(children))
}

async fn readiness_probe(State(app_state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    if !app_state.gtfs_service.is_ready().await {
        return Err(AppError::NotReady(
            "Service not ready - still loading initial data".to_string(),
        ));
    }

    Ok(Json(serde_json::json!({
        "status": "ok",
        "message": "Service is ready to handle requests"
    })))
}

async fn get_version(
    State(app_state): State<AppState>,
    Path(gtfs_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let version = app_state.gtfs_service.get_version(&gtfs_id).await?;
    Ok(Json(serde_json::json!({
        "gtfs_id": gtfs_id,
        "version": version
    })))
}

async fn get_service_type_by_vehicle(
    State(app_state): State<AppState>,
    Path(vehicle_no): Path<String>,
) -> AppResult<Json<VehicleServiceTypeResponse>> {
    let vehicle_data = app_state
        .db_vehicle_reader
        .get_vehicle_data(&vehicle_no)
        .await?;
    Ok(Json(VehicleServiceTypeResponse {
        vehicle_no: vehicle_data.vehicle_no,
        service_type: vehicle_data.service_type,
        waybill_id: Some(vehicle_data.waybill_id),
        schedule_no: Some(vehicle_data.schedule_no),
        last_updated: vehicle_data.last_updated,
    }))
}

async fn get_memory_stats(State(app_state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let stats = app_state.gtfs_service.get_memory_stats().await;
    Ok(Json(serde_json::json!(stats)))
}

async fn get_all_cached_data(
    State(app_state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    let cached_data = app_state.gtfs_service.get_all_cached_data().await;
    Ok(Json(serde_json::to_value(cached_data).map_err(|e| {
        AppError::Internal(format!("Failed to serialize cached data: {}", e))
    })?))
}

async fn get_config(State(app_state): State<AppState>) -> AppResult<Json<AppConfig>> {
    Ok(Json(app_state.config.clone()))
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
    State(app_state): State<AppState>,
    Json(payload): Json<GraphQLRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let city = payload.city.unwrap_or_else(|| "default".to_string());
    let result = app_state
        .gtfs_service
        .execute_graphql_query(
            &city,
            &payload.query,
            payload.variables,
            payload.operation_name,
            payload.gtfs_id,
        )
        .await?;
    Ok(Json(result))
}

async fn get_connection_stats(State(app_state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
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

    Ok(Json(serde_json::json!({
        "connection_stats": {
            "database": db_stats["database"],
            "http_client": http_stats["http_client"],
            "tcp_optimizations": tcp_stats["tcp_optimizations"]
        }
    })))
}
