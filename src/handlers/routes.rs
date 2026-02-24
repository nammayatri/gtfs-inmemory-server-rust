use actix_web::{
    web::{self, Data, Json, Path, Query},
    HttpResponse,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info};

use crate::environment::AppState;
use crate::graphql::TripQueryParams;
use crate::models::{
    BusScheduleDetails, GTFSStop, NandiRoutesRes, RouteStopMapping,
    StopCodeFromProviderStopCodeResponse, VehicleServiceTypeResponse,
};
// alias for query param map (string->string)
type MapStringString = std::collections::HashMap<String, String>;
use crate::{
    models::LatLong,
    tools::error::{AppError, AppResult},
};

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    limit: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct DirectionQuery {
    direction: Option<String>,
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
                "/bus-route-schedule/{gtfs_id}/{route_id}",
                actix_web::web::get().to(get_bus_route_schedule),
            )
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
            ) // deprecate above API after migration
            .route(
                "/vehicle/{gtfs_id}/service-type/{vehicle_no}",
                actix_web::web::get().to(get_service_type_by_vehicle_by_gtfs_id),
            )
            .route(
                "/vehicle/{gtfs_id}/{vehicle_no}/info",
                actix_web::web::get().to(get_vehicle_info),
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
            .route("/trip/{trip_id}", actix_web::web::get().to(get_trip_data))
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
            .route(
                "/getRoutesByIds/{gtfs_id}",
                actix_web::web::post().to(get_routes_by_ids),
            )
            .route(
                "/example-trip/{gtfs_id}/{route_code}",
                actix_web::web::get().to(get_example_trip),
            )
            .route(
                "/example-trip-map",
                actix_web::web::get().to(get_example_trip_map),
            )
            .route(
                "/getConductor/byNumber/{phoneNumber}",
                actix_web::web::get().to(get_conductor_by_phone_number),
            )
            .route(
                "/getVehiclesFrom",
                actix_web::web::get().to(get_vehicles_by_depot_query),
            )
            .route("/depotNames", actix_web::web::get().to(get_depot_names))
            .route("/depotIds", actix_web::web::get().to(get_depot_ids))
            .route(
                "/getDepotNameById/{depot_id}",
                actix_web::web::get().to(get_depot_name_by_id),
            )
            .route(
                "/depotDataCache/clear",
                actix_web::web::post().to(clear_depot_cache),
            )
            .route(
                "/vehicle-operation-data/{fleet_no}",
                actix_web::web::get().to(get_vehicle_operation_data),
            )
            .route(
                "/getVehicle/{vehicle_no}",
                actix_web::web::get().to(get_vehicle_data_eta),
            )
            .route(
                "/vehicles/{gtfs_id}/list/service-tier/{serviceTier}",
                actix_web::web::get().to(get_vehicles_by_service_tier),
            )
            .route(
                "/alternateStops/{gtfs_id}/{stop_code}",
                actix_web::web::get().to(get_alternate_stops),
            )
            .route(
                "/cache-data/{gtfs_id}",
                actix_web::web::get().to(get_cache_data_by_gtfs_id),
            ),
    );
}

async fn get_example_trip(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, route_code) = path.into_inner();
    let details = app_state
        .gtfs_service
        .get_example_trip(&gtfs_id, &route_code)
        .await?;
    Ok(HttpResponse::Ok().json(details))
}

async fn get_example_trip_map(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let map = app_state.gtfs_service.get_route_example_trip_map().await;
    Ok(HttpResponse::Ok().json(map))
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

async fn get_routes_by_ids(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<Vec<String>>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let route_ids = body.into_inner();
    let routes = app_state
        .gtfs_service
        .get_routes_by_ids(&gtfs_id, route_ids)
        .await?;
    Ok(HttpResponse::Ok().json(routes))
}

async fn get_vehicle_data_eta(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let vehicle_no = path.into_inner();

    let mut vehicle_data = app_state
        .db_vehicle_reader
        .get_vehicle_data(&vehicle_no, None)
        .await?;

    let seat_layout_id = app_state
        .gtfs_service
        .get_seat_layout_id_by_fleet_id(&vehicle_no)
        .await;
    vehicle_data.seat_layout_id = seat_layout_id;

    Ok(HttpResponse::Ok().json(vehicle_data))
}

async fn get_conductor_by_phone_number(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let phone_number = path.into_inner();

    // First check static CSV data (faster O(1) lookup)
    if let Some(emp) = app_state.conductor_details.get(&phone_number) {
        return Ok(HttpResponse::Ok().json(emp));
    }

    // Fallback to DB lookup
    let employee_data = app_state
        .db_employee_reader
        .get_employee_by_phone(&phone_number)
        .await?;
    match employee_data {
        Some(emp) => Ok(HttpResponse::Ok().json(emp)),
        None => {
            Ok(HttpResponse::NotFound()
                .body(format!("No employee found for phone: {}", phone_number)))
        }
    }
}

async fn get_routes(app_state: Data<AppState>, path: Path<String>) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let routes = app_state.gtfs_service.get_routes(&gtfs_id).await?;
    Ok(HttpResponse::Ok().json(routes))
}

// Helper to strip surrounding double quotes if present
fn strip_surrounding_quotes(s: &str) -> String {
    s.trim()
        .trim_start_matches('"')
        .trim_end_matches('"')
        .to_string()
}

async fn get_vehicles_by_depot_query(
    app_state: Data<AppState>,
    query: Query<MapStringString>,
) -> AppResult<HttpResponse> {
    // Accept raw query map so we can sanitize values (clients sometimes send quoted strings)
    let q = query.into_inner();

    if let Some(depot_name_raw) = q.get("depotName") {
        let depot_name = strip_surrounding_quotes(depot_name_raw);
        info!(
            "getVehiclesFrom depotName='{}' (raw='{}')",
            depot_name, depot_name_raw
        );
        let vehicles = app_state
            .db_vehicle_reader
            .get_vehicles_by_depot_name(&depot_name)
            .await?;
        info!(
            "handler received {} vehicles for depotName='{}'",
            vehicles.len(),
            depot_name
        );
        return Ok(HttpResponse::Ok().json(vehicles));
    }

    if let Some(depot_id_raw) = q.get("depotId") {
        let depot_id_str = strip_surrounding_quotes(depot_id_raw);
        info!(
            "getVehiclesFrom depotId_raw='{}' depotId_str='{}'",
            depot_id_raw, depot_id_str
        );
        let vehicles = app_state
            .db_vehicle_reader
            .get_vehicles_by_depot_id(&depot_id_str)
            .await?;
        info!(
            "handler received {} vehicles for depotId={}",
            vehicles.len(),
            depot_id_str
        );
        return Ok(HttpResponse::Ok().json(vehicles));
    }

    Ok(HttpResponse::BadRequest().body("Please provide depotName or depotId as query parameter"))
}

async fn get_depot_names(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let names = app_state.db_vehicle_reader.get_depot_names().await?;
    Ok(HttpResponse::Ok().json(names))
}

async fn get_depot_ids(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let ids = app_state.db_vehicle_reader.get_depot_ids().await?;
    Ok(HttpResponse::Ok().json(ids))
}

async fn get_depot_name_by_id(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let depot_id_str = path.into_inner();
    let depot_id: String = strip_surrounding_quotes(&depot_id_str).to_string();
    let depot_name = app_state
        .db_vehicle_reader
        .get_depot_name_by_id(depot_id)
        .await?;
    Ok(HttpResponse::Ok().json(depot_name))
}

async fn clear_depot_cache(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    app_state.db_vehicle_reader.clear_depot_cache().await?;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "message": "Depot cache cleared successfully"
    })))
}

async fn get_vehicle_operation_data(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let fleet_no = path.into_inner();
    let operation_data = app_state
        .db_vehicle_reader
        .get_vehicle_operation_data(&fleet_no)
        .await?;
    Ok(HttpResponse::Ok().json(operation_data))
}

async fn get_route_stop_mapping_by_route(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    query: Query<DirectionQuery>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, route_code) = path.into_inner();
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mapping_by_route_with_direction(
            &gtfs_id,
            &route_code,
            query.direction.as_deref(),
        )
        .await?;
    Ok(HttpResponse::Ok().json(mappings))
}

async fn get_route_stop_mapping_by_stop(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    query: Query<DirectionQuery>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, stop_code) = path.into_inner();
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mapping_by_stop_with_direction(
            &gtfs_id,
            &stop_code,
            query.direction.as_deref(),
        )
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

pub fn merge_stop_and_mapping(
    stop: GTFSStop,
    mapping: Option<Arc<RouteStopMapping>>,
) -> RouteStopMapping {
    let mapping_ref = mapping.as_deref();
    RouteStopMapping {
        stop_code: stop.code,
        stop_name: stop.name,
        stop_point: LatLong {
            lat: stop.lat,
            lon: stop.lon,
        },
        estimated_travel_time_from_previous_stop: None,
        geo_json: mapping_ref.and_then(|m| m.geo_json.clone()),
        gates: mapping_ref.and_then(|m| m.gates.clone()),
        provider_code: "GTFS".to_string(),
        route_code: "UNKNOWN".to_string(),
        vehicle_type: mapping_ref
            .map(|m| m.vehicle_type.clone())
            .unwrap_or_else(|| "BUS".to_string()),
        sequence_num: 0,
        hindi_name: stop.hindi_name,
        regional_name: stop.regional_name,
        platform: mapping_ref.and_then(|m| m.platform.clone()),
        parent_stop_code: stop
            .station_id
            .as_ref()
            .and_then(|station_id| station_id.split(':').next_back())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
    }
}

async fn get_stop(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, stop_code) = path.into_inner();
    let (stop, maybe_mapping) = app_state
        .gtfs_service
        .get_stop(&gtfs_id, &stop_code)
        .await?;
    let merged_stop = merge_stop_and_mapping(stop, maybe_mapping);
    Ok(HttpResponse::Ok().json(merged_stop))
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
    Ok(HttpResponse::Ok().json(version))
}

#[derive(Deserialize)]
struct TripQuery {
    trip_number: Option<i32>,
    #[serde(rename = "passVerifyReq")]
    pass_verify_req: Option<bool>,
}

async fn get_service_type_by_vehicle(
    app_state: Data<AppState>,
    path: Path<String>,
    params: web::Query<TripQuery>,
) -> AppResult<HttpResponse> {
    let vehicle_no = path.into_inner().replace("\"", "");
    get_service_type_by_vehicle_impl(app_state, None, &vehicle_no, params).await
}

async fn get_service_type_by_vehicle_by_gtfs_id(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    params: web::Query<TripQuery>,
) -> AppResult<HttpResponse> {
    let (gtfs_id_string, vehicle_no) = path.into_inner();
    let vehicle_no = vehicle_no.replace("\"", "");
    get_service_type_by_vehicle_impl(
        app_state,
        Some(gtfs_id_string.as_str()),
        &vehicle_no,
        params,
    )
    .await
}

async fn get_service_type_by_vehicle_impl(
    app_state: Data<AppState>,
    gtfs_id: Option<&str>,
    path: &str,
    params: web::Query<TripQuery>,
) -> AppResult<HttpResponse> {
    let gtfs_id = gtfs_id.unwrap_or("chennai_bus"); // todo: remove this once API is migrated
    let pass_verify_req = params.pass_verify_req.unwrap_or(false);

    // First, try to get vehicle_no from bus registration mapping using gtfs_id and short_name (path)
    let vehicle_no = if let Some(gtfs_mapping) = app_state.bus_registration_mapping.get(gtfs_id) {
        if let Some(mapped_vehicle_no) = gtfs_mapping.get(path) {
            info!(
                "Found vehicle_no {} for gtfs_id {} and short_name {} in mapping",
                mapped_vehicle_no, gtfs_id, path
            );
            mapped_vehicle_no.as_str()
        } else {
            // Not found in mapping, use path as vehicle_no (existing behavior)
            path
        }
    } else {
        // No mapping for this gtfs_id, use path as vehicle_no (existing behavior)
        path
    };

    // Get vehicle verification if requested
    let is_valid = if pass_verify_req {
        app_state
            .db_vehicle_reader
            .verify_vehicle(vehicle_no)
            .await?
    } else {
        false // Default value when verification is not requested
    };

    // Check if this is a CHALO-based city and use cache instead of DB
    let chalo_gtfs_ids = ["bhubaneshwar_bus", "sambalpur_bus"];
    info!(
        "chalo_gtfs_ids: {:?}, gtfs_id: {:?}, contains: {:?}",
        chalo_gtfs_ids,
        gtfs_id,
        chalo_gtfs_ids.contains(&gtfs_id)
    );
    if chalo_gtfs_ids.contains(&gtfs_id) {
        info!(
            "Using CHALO vehicle cache for gtfs_id={}, vehicle_no={}",
            gtfs_id, vehicle_no
        );
        if let Some(cached_data) = app_state
            .chalo_vehicle_cache
            .get_vehicle_data(gtfs_id, vehicle_no)
            .await
        {
            // Populate stops_count for route if route_id is available
            let mut remaining_trip_details = None;
            if let Some(ref route_id) = cached_data.route_id {
                let stops_len: i32 = match app_state
                    .gtfs_service
                    .get_route_stop_mapping_by_route(gtfs_id, route_id)
                    .await
                {
                    Ok(mappings) => mappings.len() as i32,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to fetch route stop mapping for gtfs_id={} route_code={}: {}",
                            gtfs_id,
                            route_id,
                            e
                        );
                        0
                    }
                };

                // Create a BusSchedule entry for the route
                remaining_trip_details = Some(vec![crate::models::BusSchedule {
                    schedule_number: cached_data.schedule_no.clone().unwrap_or_default(),
                    route_id: route_id.clone(),
                    route_name: None,
                    org_name: None,
                    trip_number: cached_data.trip_number,
                    route_number: cached_data.route_number.clone(),
                    stops_count: Some(stops_len),
                    is_active_trip: Some(cached_data.is_active_trip),
                    schedule_trip_id: cached_data.schedule_trip_id,
                    start_time: cached_data.start_time,
                    end_time: cached_data.end_time,
                    deleted: cached_data.deleted,
                    trip_order: cached_data.trip_order,
                    db_start_time: cached_data.db_start_time.clone(),
                    db_end_time: cached_data.db_end_time.clone(),
                }]);
            }

            let eligible_pass_ids = app_state
                .fleet_list
                .get(gtfs_id)
                .and_then(|by_vehicle| by_vehicle.get(&cached_data.vehicle_no))
                .cloned();

            let service_sub_types = app_state
                .vehicle_service_sub_types
                .get(gtfs_id)
                .and_then(|by_vehicle| by_vehicle.get(&cached_data.vehicle_no))
                .cloned();

            let seat_layout_id = app_state
                .gtfs_service
                .get_seat_layout_id(gtfs_id, &cached_data.vehicle_no)
                .await;

            return Ok(HttpResponse::Ok().json(VehicleServiceTypeResponse {
                vehicle_no: cached_data.vehicle_no,
                service_type: cached_data.service_type,
                waybill_id: cached_data.waybill_no,
                schedule_no: cached_data.schedule_no,
                last_updated: cached_data.last_updated,
                route_id: cached_data.route_id,
                route_number: cached_data.route_number,
                is_active_trip: cached_data.is_active_trip,
                trip_number: cached_data.trip_number,
                depot_no: cached_data.depot,
                remaining_trip_details,
                is_actually_valid: None,
                driver_id: cached_data.driver_id,
                conductor_id: cached_data.conductor_id,
                eligible_pass_ids,
                service_sub_types,
                seat_layout_id,
            }));
        } else {
            // Vehicle not found in cache, try to get service type from fleet
            if let Some(service_type) = app_state
                .gtfs_service
                .get_fleet_service_type(gtfs_id, &vehicle_no)
                .await
            {
                // Return response with service type from fleet
                let service_sub_types = app_state
                    .vehicle_service_sub_types
                    .get(gtfs_id)
                    .and_then(|by_vehicle| by_vehicle.get(vehicle_no))
                    .cloned();

                let seat_layout_id = app_state
                    .gtfs_service
                    .get_seat_layout_id(gtfs_id, vehicle_no)
                    .await;

                return Ok(HttpResponse::Ok().json(VehicleServiceTypeResponse {
                    vehicle_no: vehicle_no.to_string(),
                    service_type: Some(service_type),
                    waybill_id: None,
                    schedule_no: None,
                    last_updated: None,
                    route_id: None,
                    route_number: None,
                    is_active_trip: false,
                    trip_number: None,
                    depot_no: None,
                    remaining_trip_details: None,
                    is_actually_valid: None,
                    driver_id: None,
                    conductor_id: None,
                    eligible_pass_ids: app_state
                        .fleet_list
                        .get(gtfs_id)
                        .and_then(|by_vehicle| by_vehicle.get(vehicle_no))
                        .cloned(),
                    service_sub_types,
                    seat_layout_id,
                }));
            }
            // Vehicle not found in cache and no service type from fleet, return not found
            return Err(crate::tools::error::AppError::NotFound(format!(
                "Vehicle {} not found in cache",
                vehicle_no
            )));
        }
    }

    // ── Internal-table check ─────────────────────────────────────────────────
    // If vehicle_no exists in vehicles_internal for this gtfs_id, use the
    // _internal tables instead of the standard reader.
    if app_state
        .db_vehicle_reader_internal
        .is_vehicle_in_internal(vehicle_no, gtfs_id)
        .await
    {
        info!(
            "vehicle_no={} found in vehicles_internal, using internal reader (gtfs_id={})",
            vehicle_no, gtfs_id
        );

        let mut vehicle_data = app_state
            .db_vehicle_reader_internal
            .get_vehicle_data(vehicle_no, gtfs_id, params.trip_number)
            .await?;

        // Populate stops_count on each remaining trip
        if let Some(ref mut details) = vehicle_data.remaining_trip_details {
            for d in details.iter_mut() {
                let route_code = d.route_id.as_str();
                let stops_len: i32 = match app_state
                    .gtfs_service
                    .get_route_stop_mapping_by_route(gtfs_id, route_code)
                    .await
                {
                    Ok(mappings) => mappings.len() as i32,
                    Err(e) => {
                        tracing::warn!(
                            "Internal: failed to fetch stop mapping gtfs_id={} route={}: {}",
                            gtfs_id, route_code, e
                        );
                        0
                    }
                };
                d.stops_count = Some(stops_len);
            }
        }

        let mut service_type = match vehicle_data.service_type.clone() {
            Some(s) => Some(s),
            None => {
                app_state
                    .gtfs_service
                    .get_fleet_service_type(gtfs_id, &vehicle_data.vehicle_no)
                    .await
            }
        };

        let mut is_actually_valid = None;
        if pass_verify_req && service_type.is_none() {
            if is_valid {
                service_type = Some("Ordinary".to_string());
            } else {
                service_type = Some("Ordinary".to_string());
                is_actually_valid = Some(false);
            }
        }

        let depot_no = vehicle_data.entity_remark.or(vehicle_data.depot);

        let eligible_pass_ids = app_state
            .fleet_list
            .get(gtfs_id)
            .and_then(|by_vehicle| by_vehicle.get(&vehicle_data.vehicle_no))
            .cloned();

        let service_sub_types = app_state
            .vehicle_service_sub_types
            .get(gtfs_id)
            .and_then(|by_vehicle| by_vehicle.get(&vehicle_data.vehicle_no))
            .cloned();

        let seat_layout_id = app_state
            .gtfs_service
            .get_seat_layout_id(gtfs_id, &vehicle_data.vehicle_no)
            .await;

        return Ok(HttpResponse::Ok().json(VehicleServiceTypeResponse {
            vehicle_no: vehicle_data.vehicle_no,
            service_type,
            waybill_id: vehicle_data.waybill_no,
            schedule_no: vehicle_data.schedule_no,
            last_updated: vehicle_data.last_updated,
            route_id: vehicle_data.route_id,
            route_number: vehicle_data.route_number,
            is_active_trip: vehicle_data.is_active_trip,
            trip_number: vehicle_data.trip_number,
            depot_no,
            remaining_trip_details: vehicle_data.remaining_trip_details,
            is_actually_valid,
            driver_id: vehicle_data.driver_code,
            conductor_id: vehicle_data.conductor_code,
            eligible_pass_ids,
            service_sub_types,
            seat_layout_id,
        }));
    }

    // For other gtfs_id, use the existing DB logic
    let mut vehicle_data = app_state
        .db_vehicle_reader
        .get_vehicle_data(vehicle_no, params.trip_number)
        .await?;

    // Populate stops_count for each route in remaining_trip_details using its own route_number
    if let Some(ref mut details) = vehicle_data.remaining_trip_details {
        for d in details.iter_mut() {
            let route_code = d.route_id.as_str();
            let stops_len: i32 = match app_state
                .gtfs_service
                .get_route_stop_mapping_by_route(gtfs_id, route_code)
                .await
            {
                Ok(mappings) => mappings.len() as i32,
                Err(e) => {
                    tracing::warn!(
                        "Failed to fetch route stop mapping for gtfs_id={} route_code={}: {}",
                        gtfs_id,
                        route_code,
                        e
                    );
                    0
                }
            };
            d.stops_count = Some(stops_len);
        }
    }

    let mut service_type = match vehicle_data.service_type.clone() {
        Some(s) => Some(s),
        None => {
            app_state
                .gtfs_service
                .get_fleet_service_type(gtfs_id, &vehicle_data.vehicle_no)
                .await
        }
    };

    let mut is_actually_valid = None;

    // Apply service tier fallback logic when passVerifyReq is true
    if pass_verify_req && service_type.is_none() {
        if is_valid {
            service_type = Some("Ordinary".to_string());
        } else {
            service_type = Some("Ordinary".to_string());
            is_actually_valid = Some(false);
        }
    }

    info!("Using depot for depot_no: {:?}", vehicle_data.depot);
    info!(
        "Using entity_remark for depot_no: {:?}",
        vehicle_data.entity_remark
    );
    let depot_no = vehicle_data.entity_remark.or(vehicle_data.depot);

    let eligible_pass_ids = app_state
        .fleet_list
        .get(gtfs_id)
        .and_then(|by_vehicle| by_vehicle.get(&vehicle_data.vehicle_no))
        .cloned();

    let service_sub_types = app_state
        .vehicle_service_sub_types
        .get(gtfs_id)
        .and_then(|by_vehicle| by_vehicle.get(&vehicle_data.vehicle_no))
        .cloned();

    let seat_layout_id = app_state
        .gtfs_service
        .get_seat_layout_id(gtfs_id, &vehicle_data.vehicle_no)
        .await;

    Ok(HttpResponse::Ok().json(VehicleServiceTypeResponse {
        vehicle_no: vehicle_data.vehicle_no,
        service_type,
        waybill_id: vehicle_data.waybill_no,
        schedule_no: vehicle_data.schedule_no,
        last_updated: vehicle_data.last_updated,
        route_id: vehicle_data.route_id,
        route_number: vehicle_data.route_number,
        is_active_trip: vehicle_data.is_active_trip,
        trip_number: vehicle_data.trip_number,
        depot_no,
        remaining_trip_details: vehicle_data.remaining_trip_details,
        is_actually_valid,
        driver_id: vehicle_data.driver_code,
        conductor_id: vehicle_data.conductor_code,
        eligible_pass_ids,
        service_sub_types,
        seat_layout_id,
    }))
}

#[derive(serde::Serialize)]
struct VehicleInfoResponse {
    #[serde(rename = "driverCode")]
    driver_code: Option<String>,
    #[serde(rename = "conductorCode")]
    conductor_code: Option<String>,
    #[serde(rename = "waybillNo")]
    waybill_no: Option<String>,
    #[serde(rename = "depotName")]
    depot_name: Option<String>,
    #[serde(rename = "scheduleNo")]
    schedule_no: Option<String>,
    #[serde(rename = "seatLayoutId")]
    seat_layout_id: Option<String>,
}

async fn get_vehicle_info(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (_gtfs_id, vehicle_no) = path.into_inner();
    let vehicle_no = vehicle_no.replace("\"", "");

    let vehicle_data = app_state
        .db_vehicle_reader
        .get_vehicle_data(&vehicle_no, None)
        .await?;

    let depot_name = vehicle_data.entity_remark.or(vehicle_data.depot);

    let seat_layout_id = app_state
        .gtfs_service
        .get_seat_layout_id_by_fleet_id(&vehicle_no)
        .await;

    let resp = VehicleInfoResponse {
        driver_code: vehicle_data.driver_code,
        conductor_code: vehicle_data.conductor_code,
        waybill_no: vehicle_data.waybill_no,
        depot_name,
        schedule_no: vehicle_data.schedule_no,
        seat_layout_id,
    };

    Ok(HttpResponse::Ok().json(resp))
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
    // Get feeds loaded in memory from routes
    let feeds_in_memory = app_state.gtfs_service.get_feeds_in_memory().await;

    let response = serde_json::json!({
        "config": app_state.config.clone(),
        "feeds_loaded": feeds_in_memory
    });

    Ok(HttpResponse::Ok().json(response))
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

/// Calculate arrival time and ETA based on haversine distance between stops
/// Assumes constant speed of 25 km/hr
fn calculate_eta_from_haversine_distance(
    route_stop_mappings: &[std::sync::Arc<RouteStopMapping>],
    trip_start_time: Option<i64>,
) -> Vec<crate::models::BusStopETA> {
    const SPEED_KM_PER_HOUR: f64 = 25.0;
    const EARTH_RADIUS_KM: f64 = 6371.0; // Earth radius in kilometers

    // Haversine formula to calculate distance between two points
    fn haversine_distance(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
        let d_lat = (lat2 - lat1).to_radians();
        let d_lon = (lon2 - lon1).to_radians();

        let a = (d_lat / 2.0).sin().powi(2)
            + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
        let c = 2.0 * a.sqrt().asin();

        EARTH_RADIUS_KM * c
    }

    let mut bus_stop_etas: Vec<crate::models::BusStopETA> = Vec::new();
    let now = chrono::Utc::now();

    // Calculate cumulative travel time to each stop
    let mut cumulative_time_seconds: f64 = 0.0;

    for (idx, mapping) in route_stop_mappings.iter().enumerate() {
        if idx > 0 {
            // Calculate distance from previous stop to current stop
            let prev_mapping = &route_stop_mappings[idx - 1];
            let distance_km = haversine_distance(
                prev_mapping.stop_point.lat,
                prev_mapping.stop_point.lon,
                mapping.stop_point.lat,
                mapping.stop_point.lon,
            );

            // Calculate time to travel this distance at 25 km/hr
            let time_hours = distance_km / SPEED_KM_PER_HOUR;
            let time_seconds = time_hours * 3600.0;
            cumulative_time_seconds += time_seconds;
        }

        // Calculate arrival time
        let arrival_time = if let Some(start_epoch_millis) = trip_start_time {
            // Calculate arrival time from trip start time
            let start_utc =
                chrono::DateTime::<chrono::Utc>::from_timestamp_millis(start_epoch_millis).unwrap();
            start_utc + chrono::Duration::seconds(cumulative_time_seconds as i64)
        } else {
            // If no start time, use current time + cumulative time
            now + chrono::Duration::seconds(cumulative_time_seconds as i64)
        };

        let arrival_epoch = arrival_time.timestamp();

        // Calculate ETA in seconds (time until arrival)
        let eta_seconds = if arrival_time > now {
            Some((arrival_time - now).num_seconds())
        } else {
            None
        };

        bus_stop_etas.push(crate::models::BusStopETA {
            stop_code: mapping.stop_code.clone(),
            arrival_time: arrival_epoch,
            eta_seconds,
            stop_name: Some(mapping.stop_name.clone()),
        });
    }

    bus_stop_etas
}

#[derive(Debug, serde::Deserialize)]
pub struct BusRouteScheduleQuery {
    #[serde(rename = "justInternal")]
    pub just_internal: Option<bool>,
    #[serde(rename = "justExternal")]
    pub just_external: Option<bool>,
}

async fn get_bus_route_schedule(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    query: web::Query<BusRouteScheduleQuery>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, route_id) = path.into_inner();

    // chennai_bus guy
    // Single join query returns waybills + trip (bstd & bstf) times
    // No per-vehicle get_vehicle_data call req
    if gtfs_id == "chennai_bus" {
        let just_internal = query.just_internal.unwrap_or(false);
        let just_external = query.just_external.unwrap_or(false);

        let route_stop_mappings = app_state
            .gtfs_service
            .get_route_stop_mapping_by_route(&gtfs_id, &route_id)
            .await
            .unwrap_or_default();

        let mut all_rows = Vec::new();

        // 1. Fetch from external (existing) tables unless justInternal is strictly true
        if !just_internal {
            let mut ext_rows = app_state
                .db_vehicle_reader
                .get_chennai_waybills_by_route_id(&route_id)
                .await?;
            all_rows.append(&mut ext_rows);
        }

        // 2. Fetch from internal (_internal) tables unless justExternal is strictly true
        if !just_external {
            let mut int_rows = app_state
                .db_vehicle_reader_internal
                .get_chennai_waybills_by_route_id(&route_id, &gtfs_id)
                .await?;
            all_rows.append(&mut int_rows);
        }

        let schedule_details: BusScheduleDetails = all_rows
            .into_iter()
            .map(|row| {
                // Resolve trip start time from (db_start_time HH:MM + duty_date) or
                // fall back to the stored epoch-millis in start_time_epoch.
                let trip_start_time: Option<i64> =
                    if let (Some(hhmm), Some(duty)) =
                        (row.db_start_time.as_deref(), row.duty_date.as_deref())
                    {
                        let date =
                            chrono::NaiveDate::parse_from_str(duty, "%Y-%m-%d").ok();
                        let time =
                            chrono::NaiveTime::parse_from_str(hhmm, "%H:%M").ok();
                        if let (Some(d), Some(t)) = (date, time) {
                            let dt = chrono::NaiveDateTime::new(d, t);
                            if let Some(offset) =
                                chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60)
                            {
                                use chrono::TimeZone;
                                offset
                                    .from_local_datetime(&dt)
                                    .single()
                                    .map(|dt_tz| dt_tz.timestamp_millis())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        row.start_time_epoch
                            .as_deref()
                            .and_then(|s| s.parse::<i64>().ok())
                    };

                let bus_stop_etas =
                    calculate_eta_from_haversine_distance(&route_stop_mappings, trip_start_time);

                crate::models::BusScheduleDetail {
                    eta: bus_stop_etas,
                    vehicle_no: row.vehicle_no,
                    service_tier: row.service_type,
                    trip_number: row.trip_number,
                    waybill_no: Some(row.waybill_no),
                }
            })
            .collect();

        return Ok(HttpResponse::Ok().json(schedule_details));
    }

    // ── CHALO-based cities ───────────────────────────────────────────────────
    let chalo_gtfs_ids = ["bhubaneshwar_bus", "sambalpur_bus"];
    let waybills = if chalo_gtfs_ids.contains(&gtfs_id.as_str()) {
        // Get vehicles from cache filtered by route_id
        let cached_vehicles = app_state
            .chalo_vehicle_cache
            .get_vehicles_by_route_id(&gtfs_id, &route_id)
            .await;

        if !cached_vehicles.is_empty() {
            info!(
                "Found {} waybills for bhubaneshwar_bus route_id={} from cache",
                cached_vehicles.len(),
                route_id
            );
            // Convert CachedVehicleData to VehicleData
            cached_vehicles
                .into_iter()
                .map(|cached_data| crate::models::VehicleData {
                    waybill_id: cached_data.waybill_no.clone().unwrap_or_default(),
                    waybill_no: cached_data.waybill_no.clone().unwrap_or_default(),
                    service_type: cached_data.service_type.clone().unwrap_or_default(),
                    vehicle_no: cached_data.vehicle_no.clone(),
                    schedule_no: cached_data.schedule_no.clone().unwrap_or_default(),
                    last_updated: cached_data.last_updated,
                    duty_date: None,
                    schedule_trip_id: cached_data.schedule_trip_id.map(|id| id.to_string()),
                    entity_remark: cached_data.depot.clone(),
                    driver_code: None,
                    conductor_code: None,
                    deleted: cached_data.deleted,
                    status: Some("Online".to_string()),
                    is_flexi: None,
                    db_start_time: None,
                    start_time_epoch: None,
                    trip_number: None,
                })
                .collect()
        } else {
            // Fallback to database query
            app_state
                .db_vehicle_reader
                .get_waybills_by_route_id(&gtfs_id, &route_id)
                .await?
        }
    } else {
        // For other gtfs_ids, query database
        app_state
            .db_vehicle_reader
            .get_waybills_by_route_id(&gtfs_id, &route_id)
            .await?
    };

    let mut schedule_details: BusScheduleDetails = Vec::new();

    // Get route stop mapping to get stops for this route
    let route_stop_mappings = app_state
        .gtfs_service
        .get_route_stop_mapping_by_route(&gtfs_id, &route_id)
        .await
        .unwrap_or_default();

    for waybill in waybills {
        // Get trip details for this waybill
        let vehicle_data = app_state
            .db_vehicle_reader
            .get_vehicle_data(&waybill.vehicle_no, None)
            .await;

        let mut bus_stop_etas: Vec<crate::models::BusStopETA> = Vec::new();

        // Find the trip that matches the required route_id
        if let Ok(vehicle_data) = vehicle_data {
            // First, try to find trip in remaining_trip_details that matches route_id
            let matching_trip =
                vehicle_data
                    .remaining_trip_details
                    .as_ref()
                    .and_then(|trip_details| {
                        trip_details.iter().find(|trip| trip.route_id == route_id)
                    });

            // If not found in remaining_trip_details, check schedule_details
            let matching_trip = matching_trip.or_else(|| {
                vehicle_data.schedule_details.as_ref().and_then(|details| {
                    details
                        .values()
                        .flatten()
                        .find(|trip| trip.route_id == route_id)
                })
            });

            // Get trip start time from the matching trip
            let trip_start_time: Option<i64> = matching_trip.and_then(|trip| {
                // Try to construct time from db_start_time and waybill duty_date
                if let (Some(db_start_time), Some(duty_date)) =
                    (&trip.db_start_time, &vehicle_data.duty_date)
                {
                    // Parse duty_date (YYYY-MM-DD)
                    let date = chrono::NaiveDate::parse_from_str(duty_date, "%Y-%m-%d").ok();

                    // Parse db_start_time (HH:MM)
                    let time = chrono::NaiveTime::parse_from_str(db_start_time, "%H:%M").ok();

                    if let (Some(date), Some(time)) = (date, time) {
                        let dt = chrono::NaiveDateTime::new(date, time);
                        // Assume IST (UTC+5:30)
                        if let Some(offset) = chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60) {
                            use chrono::TimeZone;
                            if let Some(dt_with_tz) = offset.from_local_datetime(&dt).single() {
                                return Some(dt_with_tz.timestamp_millis());
                            }
                        }
                    }
                }

                // Fallback to existing start_time logic
                trip.start_time.as_ref().and_then(|s| s.parse::<i64>().ok())
            });

            // Calculate ETAs using haversine distance function
            bus_stop_etas =
                calculate_eta_from_haversine_distance(&route_stop_mappings, trip_start_time);
        }

        // If no trip details, calculate ETAs using haversine distance without start time
        if bus_stop_etas.is_empty() {
            bus_stop_etas = calculate_eta_from_haversine_distance(&route_stop_mappings, None);
        }

        schedule_details.push(crate::models::BusScheduleDetail {
            eta: bus_stop_etas,
            vehicle_no: waybill.vehicle_no.clone(),
            service_tier: waybill.service_type.clone(),
            trip_number: None,
            waybill_no: None,
        });
    }

    Ok(HttpResponse::Ok().json(schedule_details))
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

async fn get_vehicles_by_service_tier(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, service_tier) = path.into_inner();
    let vehicles = app_state
        .db_vehicle_reader
        .get_vehicles_by_service_tier(&gtfs_id, &service_tier)
        .await?;

    Ok(HttpResponse::Ok().json(vehicles))
}

async fn get_alternate_stops(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, stop_id) = path.into_inner();
    let alternate_stops = app_state
        .gtfs_service
        .get_alternate_stops(&gtfs_id, &stop_id)
        .await?;
    let merged_stops: Vec<RouteStopMapping> = alternate_stops
        .into_iter()
        .map(|stop| merge_stop_and_mapping((*stop).clone(), None))
        .collect();

    Ok(HttpResponse::Ok().json(merged_stops))
}

async fn get_cache_data_by_gtfs_id(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let cached_vehicles = app_state
        .chalo_vehicle_cache
        .get_all_vehicles_by_gtfs_id(&gtfs_id)
        .await;

    Ok(HttpResponse::Ok().json(cached_vehicles))
}
