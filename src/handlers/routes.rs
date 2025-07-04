use axum::{
    extract::{Path, Query, State},
    response::Json,
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use std::collections::HashMap;

use crate::errors::{AppError, AppResult};
use crate::models::{NandiRoutesRes, RouteStopMapping, VehicleServiceTypeResponse};
use crate::AppState;

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
        .route("/graphql", post(graphql_query))
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
) -> AppResult<Json<Vec<RouteStopMapping>>> {
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mapping_by_route(&gtfs_id, &route_code)
        .await?;
    let max_sequence_mappings = get_max_sequence_route_stop_mapping(mappings);
    Ok(Json(max_sequence_mappings))
}

fn get_max_sequence_route_stop_mapping(
    all_mappings: Vec<RouteStopMapping>,
) -> Vec<RouteStopMapping> {
    if all_mappings.is_empty() {
        return Vec::new();
    }

    let mut seq_map: HashMap<i32, RouteStopMapping> = HashMap::new();
    for mapping in all_mappings {
        seq_map.insert(mapping.sequence_num, mapping);
    }

    let max_seq = seq_map.keys().max().unwrap_or(&0);
    let mut result = Vec::new();

    for seq in 0..=*max_seq {
        if let Some(mapping) = seq_map.get(&seq) {
            result.push(mapping.clone());
        }
    }

    result
}

async fn get_route_stop_mapping_by_stop(
    State(app_state): State<AppState>,
    Path((gtfs_id, stop_code)): Path<(String, String)>,
) -> AppResult<Json<Vec<RouteStopMapping>>> {
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
) -> AppResult<Json<Vec<RouteStopMapping>>> {
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
        .await?;
    Ok(Json(stop))
}

async fn get_stops_fuzzy(
    State(app_state): State<AppState>,
    Path((gtfs_id, query)): Path<(String, String)>,
    Query(params): Query<LimitQuery>,
) -> AppResult<Json<Vec<RouteStopMapping>>> {
    let stops = app_state.gtfs_service.get_stops(&gtfs_id).await?;
    let query_lower = query.to_lowercase();

    let mut unique_stops: HashMap<String, RouteStopMapping> = HashMap::new();

    for stop in stops {
        let matches = stop.stop_name.to_lowercase().contains(&query_lower)
            || stop.stop_code.to_lowercase().contains(&query_lower);

        if matches {
            unique_stops.insert(stop.stop_code.clone(), stop);
            if let Some(limit) = params.limit {
                if unique_stops.len() >= limit as usize {
                    break;
                }
            }
        }
    }

    Ok(Json(unique_stops.into_values().collect()))
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

#[derive(Debug, serde::Deserialize)]
struct GraphQLRequest {
    query: String,
    variables: Option<serde_json::Value>,
    operation_name: Option<String>,
}

async fn graphql_query(
    State(app_state): State<AppState>,
    Json(payload): Json<GraphQLRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let result = app_state
        .gtfs_service
        .execute_graphql_query(&payload.query, payload.variables, payload.operation_name)
        .await?;
    Ok(Json(result))
}
