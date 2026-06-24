use crate::services::fleet_operator::{
    EmployeeLoginRequest, EmployeeLoginResponse, EmployeeRegisterRequest, EmployeeRegisterResponse,
    TripAction, WaybillAnchor,
};
use crate::services::operator::{
    break_types, day_types, shift_types, trip_types, waybill_statuses, QueryBody,
    EXTERNAL_ONLY_GTFS_IDS, INTERNAL_ONLY_GTFS_IDS, SUPPORTED_OPERATOR_GTFS_IDS,
};
use actix_web::{
    web::{self, Data, Json, Path, Query},
    HttpResponse,
};
use serde::Deserialize;
use serde_json::{json, Value};
use utoipa::ToSchema;

use chrono::Timelike;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

use crate::environment::AppState;
use crate::graphql::TripQueryParams;
use crate::models::{
    BusScheduleDetail, BusScheduleDetails, GTFSStop, MemoryUsageStats, MinimalEmployee,
    NandiRoutesRes, RouteStopMapping, StopCodeFromProviderStopCodeResponse, TripDetails,
    VehicleData, VehicleMetadataResponse, VehicleOperationData, VehicleServiceTypeResponse,
};
use crate::services::db_vehicle_reader::{chalo_gtfs_ids, is_chalo_gtfs_id};
use crate::services::osrtc_station_cache::osrtc_station_to_route_stop_mapping;
// alias for query param map (string->string)
type MapStringString = std::collections::HashMap<String, String>;
use actix_web::web::Bytes;
use async_stream::stream as async_stream;
use futures::StreamExt;

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
pub struct IncludeClusterIdQuery {
    #[serde(rename = "includeClusterId")]
    include_cluster_id: Option<bool>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct GetAllRoutesByIdsRequest {
    #[serde(rename = "gtfsId")]
    pub gtfs_id: String,
    #[serde(rename = "routeIds")]
    pub route_ids: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct GetAllStopsByIdsRequest {
    #[serde(rename = "gtfsId")]
    pub gtfs_id: String,
    #[serde(rename = "stopIds")]
    pub stop_ids: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct GetAllRouteStopMappingsByRouteCodesRequest {
    #[serde(rename = "gtfsId")]
    pub gtfs_id: String,
    #[serde(rename = "routeCodes")]
    pub route_codes: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct GetAllRouteStopMappingsByStopCodesRequest {
    #[serde(rename = "gtfsId")]
    pub gtfs_id: String,
    #[serde(rename = "stopCodes")]
    pub stop_codes: Vec<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct GetAllVehiclesByIdsRequest {
    #[serde(rename = "vehicleIds")]
    pub vehicle_ids: Vec<String>,
}

#[derive(Deserialize, Debug, ToSchema)]
pub struct StationEtaUpsertRequest {
    #[serde(rename = "sourceStationCode")]
    pub source_station_code: String,
    #[serde(rename = "destinationStationCode")]
    pub destination_station_code: String,
    #[serde(rename = "etaInSeconds")]
    pub eta_in_seconds: i32,
}

pub fn create_routes(cfg: &mut actix_web::web::ServiceConfig) {
    cfg.service(
        actix_web::web::scope("")
            .service(
                web::scope("/internal/operator/{gtfs_id}")
                    .route("/crud/{table}", web::get().to(get_one_row))
                    .route("/crud/{table}/all", web::get().to(get_all_rows))
                    .route("/crud/{table}/query", web::post().to(query_rows_handler))
                    .route("/crud/{table}/delete", web::post().to(delete_one_row))
                    .route("/crud/{table}/upsert", web::post().to(upsert_one_row))
                    .route("/service-types", web::get().to(get_service_types))
                    .route("/routes", web::get().to(get_operator_routes))
                    .route("/depots", web::get().to(get_depots))
                    .route("/shift-types", web::get().to(get_shift_types))
                    .route("/schedule-numbers", web::get().to(get_schedule_numbers))
                    .route("/day-types", web::get().to(get_day_types))
                    .route("/trip-types", web::get().to(get_trip_types))
                    .route("/break-types", web::get().to(get_break_types_handler))
                    .route("/trip-details", web::get().to(get_trip_details))
                    .route("/fleets", web::get().to(get_fleets))
                    .route("/conductors", web::get().to(get_conductor_data))
                    .route("/drivers", web::get().to(get_driver_info))
                    .route("/device-ids", web::get().to(get_device_ids))
                    .route("/tablet-ids", web::get().to(get_tablet_ids))
                    .route("/operators", web::get().to(get_operators))
                    .route("/waybill/status", web::post().to(update_waybill_status))
                    .route("/waybill/fleet", web::post().to(update_waybill_fleet))
                    .route("/waybill/tablet", web::post().to(update_waybill_tablet))
                    .route("/waybills", web::get().to(get_waybills))
                    .route("/station-eta/upsert", web::post().to(upsert_station_eta))
                    // stop & route management (clubber / editor)
                    .route("/stops/search", web::get().to(search_stops))
                    .route("/stops/nearby", web::get().to(nearby_stops))
                    .route("/stops/bulk-replace", web::post().to(bulk_replace_stops))
                    .route("/routes/reprocess", web::post().to(reprocess_routes))
                    .route("/routes/{route_id}/stops", web::get().to(get_route_stops))
                    .route(
                        "/routes/{route_id}/stops/insert",
                        web::post().to(insert_route_stop),
                    )
                    .route(
                        "/export/route-stop-mapping",
                        web::get().to(export_route_stop_mapping),
                    ),
            )
            .service(
                web::scope("internal/fleet-operator/{gtfs_id}")
                    .route(
                        "/currentOperation",
                        web::post().to(fleet_operator_current_operation),
                    )
                    .route("/tripAction", web::post().to(fleet_operator_trip_action))
                    .route(
                        "/currentTripDetails",
                        web::post().to(fleet_operator_current_trip_details),
                    )
                    .route("/verify", web::post().to(fleet_operator_verify))
                    .route(
                        "/employee/login",
                        web::post().to(fleet_operator_employee_login),
                    )
                    .route(
                        "/employee/register",
                        web::post().to(fleet_operator_employee_register),
                    ),
            )
            .route(
                "/bus-route-schedule/{gtfs_id}/{route_id}",
                actix_web::web::get().to(get_bus_route_schedule),
            )
            .route(
                "/bus-trip-schedule/{gtfs_id}/{waybill_no}/{trip_number}/{route_id}",
                actix_web::web::get().to(get_bus_trip_schedule),
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
                "/route-stop-mapping/{gtfs_id}/route/{route_code}/draw",
                actix_web::web::get().to(get_route_stop_mapping_draw),
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
            .route(
                "/cluster/{gtfs_id}/destinations/{stop_code}",
                actix_web::web::get().to(get_cluster_destinations),
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
                "/vehicle/{gtfs_id}/metadata/{vehicle_no}",
                actix_web::web::get().to(get_vehicle_metadata_by_gtfs_id),
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
                "/waybill/{gtfs_id}/metadata/{waybill_no}",
                actix_web::web::get().to(get_waybill_metadata),
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
                "/getManager/byNumber/{phoneNumber}",
                actix_web::web::get().to(get_manager_by_phone_number),
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
            )
            .route(
                "/routes-served-today",
                actix_web::web::get().to(get_routes_served_today),
            )
            // ── Metro routing endpoints ──
            .route(
                "/metro/route-plan/{gtfs_id}",
                actix_web::web::get().to(metro_route_plan),
            )
            .route(
                "/metro/nearby-stops/{gtfs_id}",
                actix_web::web::get().to(metro_nearby_stops),
            )
            .route(
                "/metro/graph-info/{gtfs_id}",
                actix_web::web::get().to(metro_graph_info),
            ),
    );
}

#[utoipa::path(
    get,
    path = "/example-trip/{gtfs_id}/{route_code}",
    tag = "Trip",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("route_code" = String, Path, description = "Route code"),
    ),
    responses((status = 200, description = "Example trip details", body = TripDetails))
)]
pub async fn get_example_trip(
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

#[utoipa::path(
    get,
    path = "/example-trip-map",
    tag = "Trip",
    responses((status = 200, description = "Map of route codes to example trips"))
)]
pub async fn get_example_trip_map(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let map = app_state.gtfs_service.get_route_example_trip_map().await;
    Ok(HttpResponse::Ok().json(map))
}

#[utoipa::path(
    get,
    path = "/route/{gtfs_id}/{route_id}",
    tag = "Routes",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("route_id" = String, Path, description = "Route identifier"),
    ),
    responses((status = 200, description = "Route details", body = NandiRoutesRes))
)]
pub async fn get_route(
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

#[utoipa::path(
    post,
    path = "/getRoutesByIds/{gtfs_id}",
    tag = "Bulk",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    request_body = Vec<String>,
    responses((status = 200, description = "Routes matching IDs", body = Vec<NandiRoutesRes>))
)]
pub async fn get_routes_by_ids(
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

#[utoipa::path(
    get,
    path = "/getVehicle/{vehicle_no}",
    tag = "Vehicle",
    params(("vehicle_no" = String, Path, description = "Vehicle number")),
    responses((status = 200, description = "Vehicle data with ETA", body = VehicleData))
)]
pub async fn get_vehicle_data_eta(
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

#[utoipa::path(
    get,
    path = "/getConductor/byNumber/{phoneNumber}",
    tag = "Vehicle",
    params(("phoneNumber" = String, Path, description = "Phone number")),
    responses((status = 200, description = "Conductor details", body = MinimalEmployee))
)]
pub async fn get_conductor_by_phone_number(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let phone_number = path.into_inner();
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

#[utoipa::path(
    get,
    path = "/getManager/byNumber/{phoneNumber}",
    tag = "Vehicle",
    params(("phoneNumber" = String, Path, description = "Phone number")),
    responses((status = 200, description = "Depot manager details"))
)]
pub async fn get_manager_by_phone_number(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let phone_number = path.into_inner();
    let hash_key = &app_state.config.phone_number_hash_key;
    let phone_hash = crate::tools::hash::hash_phone_number(&phone_number, hash_key);

    match app_state.depot_manager_details.get(&phone_hash) {
        Some(manager) => Ok(HttpResponse::Ok().json(manager)),
        None => Ok(HttpResponse::NotFound().body(format!(
            "No depot manager found for phone: {}",
            phone_number
        ))),
    }
}

#[utoipa::path(
    get,
    path = "/routes/{gtfs_id}",
    tag = "Routes",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of routes", body = Vec<NandiRoutesRes>))
)]
pub async fn get_routes(app_state: Data<AppState>, path: Path<String>) -> AppResult<HttpResponse> {
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

#[utoipa::path(
    get,
    path = "/getVehiclesFrom",
    tag = "Vehicle",
    params(
        ("depotName" = Option<String>, Query, description = "Depot name"),
        ("depotId" = Option<String>, Query, description = "Depot ID"),
    ),
    responses((status = 200, description = "Vehicles from depot", body = Vec<VehicleData>))
)]
pub async fn get_vehicles_by_depot_query(
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

#[utoipa::path(
    get,
    path = "/depotNames",
    tag = "Vehicle",
    responses((status = 200, description = "List of depot names", body = Vec<String>))
)]
pub async fn get_depot_names(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let names = app_state.db_vehicle_reader.get_depot_names().await?;
    Ok(HttpResponse::Ok().json(names))
}

#[utoipa::path(
    get,
    path = "/depotIds",
    tag = "Vehicle",
    responses((status = 200, description = "List of depot IDs", body = Vec<String>))
)]
pub async fn get_depot_ids(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let ids = app_state.db_vehicle_reader.get_depot_ids().await?;
    Ok(HttpResponse::Ok().json(ids))
}

#[utoipa::path(
    get,
    path = "/getDepotNameById/{depot_id}",
    tag = "Vehicle",
    params(("depot_id" = String, Path, description = "Depot ID")),
    responses((status = 200, description = "Depot name", body = String))
)]
pub async fn get_depot_name_by_id(
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

#[utoipa::path(
    post,
    path = "/depotDataCache/clear",
    tag = "System",
    responses((status = 200, description = "Depot cache cleared"))
)]
pub async fn clear_depot_cache(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    app_state.db_vehicle_reader.clear_depot_cache().await?;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "message": "Depot cache cleared successfully"
    })))
}

#[utoipa::path(
    get,
    path = "/vehicle-operation-data/{fleet_no}",
    tag = "Vehicle",
    params(("fleet_no" = String, Path, description = "Fleet number")),
    responses((status = 200, description = "Vehicle operation data", body = VehicleOperationData))
)]
pub async fn get_vehicle_operation_data(
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

#[utoipa::path(
    get,
    path = "/route-stop-mapping/{gtfs_id}/route/{route_code}",
    tag = "Routes",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("route_code" = String, Path, description = "Route code"),
        ("direction" = Option<String>, Query, description = "Direction filter"),
    ),
    responses((status = 200, description = "Route-stop mappings for route", body = Vec<RouteStopMapping>))
)]
pub async fn get_route_stop_mapping_by_route(
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

#[utoipa::path(
    get,
    path = "/route-stop-mapping/{gtfs_id}/stop/{stop_code}",
    tag = "Stops",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("stop_code" = String, Path, description = "Stop code"),
        ("direction" = Option<String>, Query, description = "Direction filter"),
    ),
    responses((status = 200, description = "Route-stop mappings for stop", body = Vec<RouteStopMapping>))
)]
pub async fn get_route_stop_mapping_by_stop(
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

#[utoipa::path(
    get,
    path = "/route-stop-mapping/{gtfs_id}/route/{route_code}/draw",
    tag = "Routes",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("route_code" = String, Path, description = "Route code"),
        ("direction" = Option<String>, Query, description = "Direction filter"),
    ),
    responses((status = 200, description = "HTML page with Leaflet map", body = String))
)]
pub async fn get_route_stop_mapping_draw(
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

    let stops_json = serde_json::to_string(&mappings).unwrap_or_else(|_| "[]".to_string());
    let stops_json = stops_json.replace('<', "\\u003c");

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Route Stop Mapping Draw</title>
<link rel="stylesheet" href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css" />
<script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js"></script>
<style>
  html, body {{ margin: 0; padding: 0; height: 100%; font-family: Arial, sans-serif; overflow: hidden; }}
  #map {{ height: 100vh; width: 100%; }}
  #topbar {{ position: absolute; top: 10px; left: 10px; right: 10px; z-index: 1000; display: flex; gap: 10px; align-items: center; flex-wrap: wrap; background: rgba(255,255,255,0.95); padding: 8px 12px; border-radius: 6px; box-shadow: 0 2px 6px rgba(0,0,0,0.2); }}
  #filterPanel {{ position: absolute; top: 70px; left: 10px; z-index: 1000; max-height: 70vh; overflow-y: auto; background: rgba(255,255,255,0.95); padding: 10px; border-radius: 6px; box-shadow: 0 2px 6px rgba(0,0,0,0.2); min-width: 220px; display: none; }}
  #filterPanel.active {{ display: block; }}
  .filter-group {{ margin-bottom: 8px; }}
  .filter-group label {{ display: block; font-size: 12px; font-weight: bold; margin-bottom: 2px; }}
  .filter-group input[type="number"], .filter-group select {{ width: 100%; box-sizing: border-box; }}
  .error {{ color: #c0392b; font-size: 12px; }}
  .numbered-marker {{
    background: #2980b9;
    color: #fff;
    border-radius: 50%;
    width: 24px;
    height: 24px;
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 12px;
    font-weight: bold;
    border: 2px solid #fff;
    box-shadow: 0 1px 3px rgba(0,0,0,0.3);
  }}
</style>
</head>
<body>
<div id="topbar">
  <label>Upload CSV: <input type="file" id="csvInput" accept=".csv" /></label>
  <span id="csvError" class="error"></span>
  <button id="toggleFilters">Filters</button>
</div>
<div id="filterPanel"></div>
<script type="application/json" id="stops-data">{stops_json}</script>
<div id="map"></div>
<script>
(function() {{
  const serverStops = JSON.parse(document.getElementById('stops-data').textContent);
  let csvRows = [];
  let csvHeaders = [];
  let map = L.map('map');
  L.tileLayer('https://{{s}}.basemaps.cartocdn.com/light_all/{{z}}/{{x}}/{{y}}{{r}}.png', {{
    attribution: '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors &copy; <a href="https://carto.com/attributions">CARTO</a>',
    subdomains: 'abcd',
    maxZoom: 20
  }}).addTo(map);

  const routeStopsLayer = L.layerGroup().addTo(map);
  const csvPointsLayer = L.layerGroup().addTo(map);

  function makeNumberedIcon(num) {{
    return L.divIcon({{
      className: '',
      html: `<div class="numbered-marker">${{num}}</div>`,
      iconSize: [24, 24],
      iconAnchor: [12, 12]
    }});
  }}

  function buildPopup(s) {{
    const gates = (s.gates && s.gates.length)
      ? s.gates.map(g => g.gateName).join(', ')
      : 'None';
    return `
      <div style="font-size:13px;line-height:1.4;">
        <strong>${{s.stopName}}</strong><br/>
        <span style="color:#555;">Code:</span> ${{s.stopCode}}<br/>
        <span style="color:#555;">Sequence:</span> ${{s.sequenceNum}}<br/>
        <span style="color:#555;">Vehicle Type:</span> ${{s.vehicleType}}<br/>
        <span style="color:#555;">Platform:</span> ${{s.platform ?? '-'}}<br/>
        <span style="color:#555;">Gates:</span> ${{gates}}
      </div>
    `;
  }}

  function renderRouteStops(stops) {{
    routeStopsLayer.clearLayers();
    if (!stops || stops.length === 0) return;
    const latlngs = stops.map((s) => {{
      const lat = s.stopPoint?.latitude ?? s.stopPoint?.lat;
      const lng = s.stopPoint?.longitude ?? s.stopPoint?.lon;
      if (lat == null || lng == null) return null;
      const marker = L.marker([lat, lng], {{ icon: makeNumberedIcon(s.sequenceNum) }}).bindPopup(buildPopup(s));
      routeStopsLayer.addLayer(marker);
      return [lat, lng];
    }}).filter(Boolean);
    if (latlngs.length > 0) {{
      const polyline = L.polyline(latlngs, {{color: 'blue'}});
      routeStopsLayer.addLayer(polyline);
    }}
  }}

  function renderCsvPoints(rows) {{
    csvPointsLayer.clearLayers();
    if (!rows || rows.length === 0) return;
    rows.forEach((r, i) => {{
      const lat = r.stopPoint?.latitude ?? r.stopPoint?.lat;
      const lng = r.stopPoint?.longitude ?? r.stopPoint?.lon;
      if (lat == null || lng == null) return;
      const marker = L.circleMarker([lat, lng], {{
        radius: 6,
        color: '#e74c3c',
        fillColor: '#e74c3c',
        fillOpacity: 0.8
      }}).bindPopup(`CSV #${{i+1}} ${{r.stopName}} (${{r.stopCode}})`);
      csvPointsLayer.addLayer(marker);
    }});
  }}

  function parseCsv(text) {{
    const rows = [];
    let row = [];
    let field = '';
    let inQuotes = false;
    let i = 0;
    while (i < text.length) {{
      const char = text[i];
      const next = text[i + 1];
      if (inQuotes) {{
        if (char === '"') {{
          if (next === '"') {{
            field += '"';
            i += 2;
            continue;
          }} else {{
            inQuotes = false;
          }}
        }} else {{
          field += char;
        }}
      }} else {{
        if (char === '"') {{
          inQuotes = true;
        }} else if (char === ',') {{
          row.push(field);
          field = '';
        }} else if (char === '\r' && next === '\n') {{
          row.push(field);
          field = '';
          if (row.length > 1 || row[0] !== '') rows.push(row);
          row = [];
          i += 1;
        }} else if (char === '\n' || char === '\r') {{
          row.push(field);
          field = '';
          if (row.length > 1 || row[0] !== '') rows.push(row);
          row = [];
        }} else {{
          field += char;
        }}
      }}
      i += 1;
    }}
    row.push(field);
    if (row.length > 1 || row[0] !== '') rows.push(row);
    return rows;
  }}

  function validateCsv(parsedRows) {{
    if (!Array.isArray(parsedRows) || parsedRows.length < 2) throw new Error('CSV is empty or too short');
    const rawHeaders = parsedRows[0];
    const headers = rawHeaders.map(h => h.trim().toLowerCase());
    const latIdx = headers.findIndex(h => h === 'lat' || h === 'latitude');
    const lngIdx = headers.findIndex(h => h === 'lon' || h === 'lng' || h === 'longitude');
    if (latIdx === -1 || lngIdx === -1) throw new Error('CSV must contain latitude/longitude columns');
    const dataRows = parsedRows.slice(1);
    return dataRows.map((vals, i) => {{
      const obj = {{}};
      rawHeaders.forEach((h, idx) => obj[h.trim()] = (vals[idx] ?? '').trim());
      const lat = parseFloat(obj[rawHeaders[latIdx].trim()]);
      const lng = parseFloat(obj[rawHeaders[lngIdx].trim()]);
      if (isNaN(lat) || isNaN(lng)) {{
        throw new Error(`Row ${{i + 1}} has invalid latitude or longitude`);
      }}
      return {{
        stopPoint: {{ latitude: lat, longitude: lng }},
        stopName: obj.stopName ?? obj.name ?? '',
        stopCode: obj.stopCode ?? obj.code ?? String(i+1),
        sequenceNum: parseInt(obj.sequenceNum ?? obj.sequence ?? (i+1), 10),
        _raw: obj
      }};
    }}).sort((a,b) => a.sequenceNum - b.sequenceNum);
  }}

  function inferColumnTypes(rows, rawHeaders) {{
    const types = {{}};
    rawHeaders.forEach(h => {{
      const key = h.trim();
      let numeric = 0, bool = 0, total = 0;
      rows.forEach(r => {{
        const v = r[key];
        if (v === undefined || v === '') return;
        total++;
        if (v.toLowerCase() === 'true' || v.toLowerCase() === 'false') bool++;
        if (!isNaN(parseFloat(v)) && isFinite(v)) numeric++;
      }});
      if (total === 0) {{ types[key] = 'string'; }}
      else if (bool === total) {{ types[key] = 'boolean'; }}
      else if (numeric === total) {{ types[key] = 'number'; }}
      else {{ types[key] = 'string'; }}
    }});
    return types;
  }}

  function buildFilterControls(rows, rawHeaders) {{
    const panel = document.getElementById('filterPanel');
    panel.innerHTML = '';
    if (!rows.length) {{ panel.classList.remove('active'); return; }}
    const types = inferColumnTypes(rows, rawHeaders);
    rawHeaders.forEach(key => {{
      const k = key.trim();
      const t = types[k];
      const group = document.createElement('div');
      group.className = 'filter-group';
      const label = document.createElement('label');
      label.textContent = k;
      group.appendChild(label);
      if (t === 'number') {{
        const nums = rows.map(r => parseFloat(r[k])).filter(v => !isNaN(v));
        const min = Math.min(...nums);
        const max = Math.max(...nums);
        const minIn = document.createElement('input');
        minIn.type = 'number';
        minIn.placeholder = `min (${{min}})`;
        minIn.dataset.col = k;
        minIn.dataset.kind = 'min';
        const maxIn = document.createElement('input');
        maxIn.type = 'number';
        maxIn.placeholder = `max (${{max}})`;
        maxIn.dataset.col = k;
        maxIn.dataset.kind = 'max';
        group.appendChild(minIn);
        group.appendChild(maxIn);
      }} else if (t === 'boolean') {{
        const wrap = document.createElement('div');
        ['true','false'].forEach(val => {{
          const lbl = document.createElement('label');
          lbl.style.fontWeight = 'normal';
          lbl.style.fontSize = '12px';
          const cb = document.createElement('input');
          cb.type = 'checkbox';
          cb.value = val;
          cb.dataset.col = k;
          cb.dataset.kind = 'bool';
          lbl.appendChild(cb);
          lbl.appendChild(document.createTextNode(' ' + val));
          wrap.appendChild(lbl);
        }});
        group.appendChild(wrap);
      }} else {{
        const vals = [...new Set(rows.map(r => r[k]).filter(v => v !== ''))].sort();
        const sel = document.createElement('select');
        sel.multiple = true;
        sel.size = Math.min(4, vals.length);
        sel.dataset.col = k;
        sel.dataset.kind = 'select';
        const allOpt = document.createElement('option');
        allOpt.value = '__ALL__';
        allOpt.textContent = '(all)';
        allOpt.selected = true;
        sel.appendChild(allOpt);
        vals.forEach(v => {{
          const opt = document.createElement('option');
          opt.value = v;
          opt.textContent = v;
          sel.appendChild(opt);
        }});
        group.appendChild(sel);
      }}
      panel.appendChild(group);
    }});
    const applyBtn = document.createElement('button');
    applyBtn.textContent = 'Apply Filters';
    applyBtn.style.marginTop = '6px';
    applyBtn.addEventListener('click', applyFilters);
    panel.appendChild(applyBtn);
    panel.classList.add('active');
  }}

  function applyFilters() {{
    if (!csvRows.length) return;
    const panel = document.getElementById('filterPanel');
    const mins = {{}}, maxs = {{}}, bools = {{}}, selects = {{}};
    panel.querySelectorAll('input[data-kind="min"]').forEach(el => {{ if (el.value !== '') mins[el.dataset.col] = parseFloat(el.value); }});
    panel.querySelectorAll('input[data-kind="max"]').forEach(el => {{ if (el.value !== '') maxs[el.dataset.col] = parseFloat(el.value); }});
    panel.querySelectorAll('input[data-kind="bool"]:checked').forEach(el => {{
      if (!bools[el.dataset.col]) bools[el.dataset.col] = [];
      bools[el.dataset.col].push(el.value === 'true');
    }});
    panel.querySelectorAll('select[data-kind="select"]').forEach(el => {{
      const chosen = Array.from(el.selectedOptions).map(o => o.value).filter(v => v !== '__ALL__');
      if (chosen.length) selects[el.dataset.col] = chosen;
    }});
    const filtered = csvRows.filter(r => {{
      const raw = r._raw;
      for (const col in mins) {{ const v = parseFloat(raw[col]); if (isNaN(v) || v < mins[col]) return false; }}
      for (const col in maxs) {{ const v = parseFloat(raw[col]); if (isNaN(v) || v > maxs[col]) return false; }}
      for (const col in bools) {{ const v = raw[col]?.toLowerCase() === 'true'; if (!bools[col].includes(v)) return false; }}
      for (const col in selects) {{ if (!selects[col].includes(raw[col])) return false; }}
      return true;
    }});
    renderCsvPoints(filtered);
  }}

  document.getElementById('csvInput').addEventListener('change', function(e) {{
    const file = e.target.files[0];
    if (!file) return;
    const reader = new FileReader();
    reader.onload = function(ev) {{
      const text = ev.target.result;
      try {{
        const parsed = parseCsv(text);
        if (parsed.length < 2) {{ document.getElementById('csvError').textContent = 'CSV too short'; return; }}
        const rawHeaders = parsed[0];
        csvRows = validateCsv(parsed);
        csvHeaders = rawHeaders.map(h => h.trim());
        document.getElementById('csvError').textContent = '';
        renderCsvPoints(csvRows);
        buildFilterControls(csvRows.map(r => r._raw), rawHeaders);
      }} catch (err) {{
        document.getElementById('csvError').textContent = err.message;
      }}
    }};
    reader.readAsText(file);
  }});

  document.getElementById('toggleFilters').addEventListener('click', function() {{
    document.getElementById('filterPanel').classList.toggle('active');
  }});

  renderRouteStops(serverStops);
  const initLatLngs = serverStops.map(s => {{
    const lat = s.stopPoint?.latitude ?? s.stopPoint?.lat;
    const lng = s.stopPoint?.longitude ?? s.stopPoint?.lon;
    if (lat == null || lng == null) return null;
    return [lat, lng];
  }}).filter(Boolean);
  if (initLatLngs.length > 0) {{
    map.fitBounds(L.latLngBounds(initLatLngs), {{padding: [20, 20]}});
  }}
}})();
</script>
</body>
</html>"#
    );

    Ok(HttpResponse::Ok().content_type("text/html").body(html))
}

#[utoipa::path(
    get,
    path = "/cluster/{gtfs_id}/destinations/{stop_code}",
    tag = "Cluster",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("stop_code" = String, Path, description = "Source stop code"),
    ),
    responses((
        status = 200,
        description = "Destination stop_codes reachable downstream of the source, computed against the \
                       server's precomputed representative pattern per route (the longest pattern observed \
                       for each route at build time — see build_route_data). Deduplicated by H3 cluster \
                       so the response carries one representative stop_code per reachable cluster. \
                       Does NOT include destinations reachable via a transfer, and does NOT enumerate \
                       every trip pattern — patterns shorter than the longest for the same route are not \
                       walked. Falls back to a single-stop walk when the source stop has no cluster_id \
                       (logged at debug). Returns 404 on unknown gtfs_id; returns [] for an unknown \
                       stop_code or a stop with no outgoing routes on the representative pattern.",
        body = Vec<String>,
    ))
)]
pub async fn get_cluster_destinations(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, stop_code) = path.into_inner();
    let destinations = app_state
        .gtfs_service
        .get_cluster_destinations_for_stop(&gtfs_id, &stop_code)?;
    Ok(HttpResponse::Ok().json(destinations))
}


#[utoipa::path(
    get,
    path = "/routes/{gtfs_id}/fuzzy/{query}",
    tag = "Routes",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("query" = String, Path, description = "Fuzzy search query"),
        ("limit" = Option<i32>, Query, description = "Max results to return"),
    ),
    responses((status = 200, description = "Fuzzy matched routes", body = Vec<NandiRoutesRes>))
)]
pub async fn get_routes_fuzzy(
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

#[utoipa::path(
    get,
    path = "/stops/{gtfs_id}",
    tag = "Stops",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("includeClusterId" = Option<bool>, Query, description = "If true, include clusterId in each stop's response"),
    ),
    responses((status = 200, description = "List of stops", body = Vec<RouteStopMapping>))
)]
pub async fn get_stops(
    app_state: Data<AppState>,
    path: Path<String>,
    query: Query<IncludeClusterIdQuery>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let include_cluster_id = query.include_cluster_id.unwrap_or(false);
    if app_state.config.osrtc_feed_key.as_deref() == Some(gtfs_id.as_str()) {
        let cache = app_state
            .osrtc_cache
            .as_ref()
            .ok_or_else(|| AppError::Internal("OSRTC cache not configured".to_string()))?;
        let stations = cache.get_all_stations().await;
        let mappings: Vec<RouteStopMapping> = stations
            .iter()
            .map(osrtc_station_to_route_stop_mapping)
            .collect();
        return Ok(stops_response(&mappings, include_cluster_id)?);
    }
    let stops = app_state.gtfs_service.get_stops(&gtfs_id).await?;
    stops_response(stops.as_slice(), include_cluster_id)
}

fn stops_response<T: serde::Serialize>(
    stops: &[T],
    include_cluster_id: bool,
) -> AppResult<HttpResponse> {
    if include_cluster_id {
        return Ok(HttpResponse::Ok().json(stops));
    }
    let mut value = serde_json::to_value(stops)
        .map_err(|e| AppError::Internal(format!("Failed to serialize stops: {}", e)))?;
    if let Some(arr) = value.as_array_mut() {
        for item in arr {
            if let Some(obj) = item.as_object_mut() {
                obj.remove("clusterId");
            }
        }
    }
    Ok(HttpResponse::Ok().json(value))
}

pub fn merge_stop_and_mapping(
    stop: GTFSStop,
    mapping: Option<Arc<RouteStopMapping>>,
) -> RouteStopMapping {
    let mapping_ref = mapping.as_deref();
    RouteStopMapping {
        stop_code: Arc::from(stop.code.as_str()),
        stop_name: Arc::from(stop.name.as_str()),
        stop_point: LatLong {
            lat: stop.lat,
            lon: stop.lon,
        },
        estimated_travel_time_from_previous_stop: None,
        geo_json: mapping_ref.and_then(|m| m.geo_json.clone()),
        gates: mapping_ref.and_then(|m| m.gates.clone()),
        provider_code: Arc::from("GTFS"),
        route_code: Arc::from("UNKNOWN"),
        vehicle_type: mapping_ref
            .map(|m| m.vehicle_type.clone())
            .unwrap_or_else(|| Arc::from("BUS")),
        sequence_num: 0,
        hindi_name: stop.hindi_name.map(|s| Arc::from(s.as_str())),
        regional_name: stop.regional_name.map(|s| Arc::from(s.as_str())),
        platform: mapping_ref.and_then(|m| m.platform.clone()),
        parent_stop_code: stop
            .station_id
            .as_ref()
            .and_then(|station_id| station_id.split(':').next_back())
            .filter(|s| !s.is_empty())
            .map(Arc::from),
        cluster_id: stop.cluster_id.as_deref().map(Arc::from),
    }
}

#[utoipa::path(
    get,
    path = "/stop/{gtfs_id}/{stop_code}",
    tag = "Stops",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("stop_code" = String, Path, description = "Stop code"),
    ),
    responses((status = 200, description = "Stop details", body = RouteStopMapping))
)]
pub async fn get_stop(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, stop_code) = path.into_inner();
    if app_state.config.osrtc_feed_key.as_deref() == Some(gtfs_id.as_str()) {
        let cache = app_state
            .osrtc_cache
            .as_ref()
            .ok_or_else(|| AppError::Internal("OSRTC cache not configured".to_string()))?;
        let station = cache
            .get_station_by_id(&stop_code)
            .await
            .ok_or_else(|| AppError::NotFound(format!("OSRTC station not found: {stop_code}")))?;
        return Ok(HttpResponse::Ok().json(osrtc_station_to_route_stop_mapping(&station)));
    }
    let (stop, maybe_mapping) = app_state
        .gtfs_service
        .get_stop(&gtfs_id, &stop_code)
        .await?;
    let merged_stop = merge_stop_and_mapping(stop, maybe_mapping);
    Ok(HttpResponse::Ok().json(merged_stop))
}

#[utoipa::path(
    get,
    path = "/stops/{gtfs_id}/fuzzy/{query}",
    tag = "Stops",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("query" = String, Path, description = "Fuzzy search query"),
        ("limit" = Option<i32>, Query, description = "Max results to return"),
    ),
    responses((status = 200, description = "Fuzzy matched stops", body = Vec<GTFSStop>))
)]
pub async fn get_stops_fuzzy(
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
            unique_stops.insert(stop.stop_code.to_string(), stop.clone());
            if let Some(limit) = query.limit {
                if unique_stops.len() >= limit as usize {
                    break;
                }
            }
        }
    }

    Ok(HttpResponse::Ok().json(unique_stops.into_values().collect::<Vec<_>>()))
}

#[utoipa::path(
    get,
    path = "/stop-code/{gtfs_id}/{provider_stop_code}",
    tag = "Stops",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("provider_stop_code" = String, Path, description = "Provider stop code"),
    ),
    responses((status = 200, description = "Mapped stop code", body = StopCodeFromProviderStopCodeResponse))
)]
pub async fn get_stop_code_from_provider_stop_code(
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

#[utoipa::path(
    get,
    path = "/station-children/{gtfs_id}/{stop_code}",
    tag = "Stops",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("stop_code" = String, Path, description = "Parent station stop code"),
    ),
    responses((status = 200, description = "Child stops of station"))
)]
pub async fn get_station_children(
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

#[utoipa::path(
    get,
    path = "/ready",
    tag = "System",
    responses((status = 200, description = "Service is ready"))
)]
pub async fn readiness_probe(app_state: Data<AppState>) -> AppResult<HttpResponse> {
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

#[utoipa::path(
    get,
    path = "/version/{gtfs_id}",
    tag = "System",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "Data version hash"))
)]
pub async fn get_version(app_state: Data<AppState>, path: Path<String>) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let version = app_state.gtfs_service.get_version(&gtfs_id).await?;
    Ok(HttpResponse::Ok().json(version))
}

#[derive(Deserialize)]
pub struct TripQuery {
    trip_number: Option<i32>,
    #[serde(rename = "passVerifyReq")]
    pass_verify_req: Option<bool>,
    /// Override current time for schedule-based reconciliation (format: HH:MM, e.g., "10:20")
    cur_time: Option<String>,
}

#[utoipa::path(
    get,
    path = "/vehicle/{vehicle_no}/service-type",
    tag = "Vehicle",
    params(
        ("vehicle_no" = String, Path, description = "Vehicle number"),
        ("trip_number" = Option<i32>, Query, description = "Trip number"),
        ("passVerifyReq" = Option<bool>, Query, description = "Whether pass verification is required"),
        ("cur_time" = Option<String>, Query, description = "Override current time (HH:MM)"),
    ),
    responses((status = 200, description = "Vehicle service type info (deprecated)", body = VehicleServiceTypeResponse))
)]
pub async fn get_service_type_by_vehicle(
    app_state: Data<AppState>,
    path: Path<String>,
    params: web::Query<TripQuery>,
) -> AppResult<HttpResponse> {
    let vehicle_no = path.into_inner().replace("\"", "");
    get_service_type_by_vehicle_impl(app_state, None, &vehicle_no, params).await
}

#[utoipa::path(
    get,
    path = "/vehicle/{gtfs_id}/service-type/{vehicle_no}",
    tag = "Vehicle",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("vehicle_no" = String, Path, description = "Vehicle number"),
        ("trip_number" = Option<i32>, Query, description = "Trip number"),
        ("passVerifyReq" = Option<bool>, Query, description = "Whether pass verification is required"),
        ("cur_time" = Option<String>, Query, description = "Override current time (HH:MM)"),
    ),
    responses((status = 200, description = "Vehicle service type info", body = VehicleServiceTypeResponse))
)]
pub async fn get_service_type_by_vehicle_by_gtfs_id(
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

#[utoipa::path(
    get,
    path = "/vehicle/{gtfs_id}/metadata/{vehicle_no}",
    tag = "Vehicle",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("vehicle_no" = String, Path, description = "Vehicle number"),
    ),
    responses((status = 200, description = "Vehicle metadata", body = VehicleMetadataResponse))
)]
pub async fn get_vehicle_metadata_by_gtfs_id(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    params: web::Query<TripQuery>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, path_vehicle) = path.into_inner();
    let gtfs_id = gtfs_id.replace("\"", "");
    let path_vehicle = path_vehicle.replace("\"", "");

    // Support short_name -> vehicle_no mapping (same behavior as /service-type)
    let vehicle_no = app_state
        .bus_registration_mapping
        .get(&gtfs_id)
        .and_then(|m| m.get(&path_vehicle))
        .cloned()
        .unwrap_or(path_vehicle);

    let vehicle_tag_number = app_state
        .fleet_tag_list
        .get(&gtfs_id)
        .and_then(|by_vehicle| by_vehicle.get(&vehicle_no))
        .cloned();

    let service_sub_types = app_state
        .vehicle_service_sub_types
        .get(&gtfs_id)
        .and_then(|by_vehicle| by_vehicle.get(&vehicle_no))
        .cloned();

    let service_type = get_vehicle_service_type_with_optional_internal_cache(
        app_state.as_ref(),
        &gtfs_id,
        &vehicle_no,
    )
    .await?;

    let pass_verify_req = params.pass_verify_req.unwrap_or(false);
    let mut service_type = service_type;
    let mut is_actually_valid: Option<bool> = None;

    if pass_verify_req {
        if service_type.is_some() {
            is_actually_valid = Some(true);
        } else {
            service_type = Some("Ordinary".to_string());
            is_actually_valid = Some(false);
        }
    }

    Ok(HttpResponse::Ok().json(VehicleMetadataResponse {
        service_type,
        service_sub_types,
        bus_tag_number: vehicle_tag_number,
        is_actually_valid,
    }))
}

async fn get_vehicle_service_type_with_optional_internal_cache(
    app_state: &AppState,
    gtfs_id: &str,
    vehicle_no: &str,
) -> AppResult<Option<String>> {
    if !SUPPORTED_OPERATOR_GTFS_IDS.contains(&gtfs_id) {
        return fetch_vehicle_service_type(app_state, gtfs_id, vehicle_no).await;
    }

    let cache_key = format!("{}:{}", gtfs_id, vehicle_no);
    let now = Instant::now();
    {
        let cache = app_state.chennai_service_type_cache.read().await;
        if let Some((expires_at, cached)) = cache.get(&cache_key) {
            if *expires_at > now {
                return Ok(cached.clone());
            }
        }
    }
    {
        let mut cache = app_state.chennai_service_type_cache.write().await;
        if let Some((expires_at, _)) = cache.get(&cache_key) {
            if *expires_at <= now {
                cache.remove(&cache_key);
            }
        }
    }
    let computed = fetch_vehicle_service_type(app_state, gtfs_id, vehicle_no).await?;
    if computed.is_some() {
        let mut cache = app_state.chennai_service_type_cache.write().await;
        cache.insert(
            cache_key,
            (now + Duration::from_secs(60 * 60), computed.clone()),
        );
    }
    Ok(computed)
}

async fn fetch_vehicle_service_type(
    app_state: &AppState,
    gtfs_id: &str,
    vehicle_no: &str,
) -> AppResult<Option<String>> {
    // CHALO-based cities: service_type comes from cache (route mapping) with fleet fallback
    if is_chalo_gtfs_id(gtfs_id) {
        if let Some(cached) = app_state
            .chalo_vehicle_cache
            .get_vehicle_data(gtfs_id, vehicle_no)
            .await
        {
            // Cache hit: return whatever cache contains (even None).
            return Ok(cached.service_type);
        }
        // Cache miss: use fleet fallback; if still missing, return 404.
        let fleet = app_state
            .gtfs_service
            .get_fleet_service_type(gtfs_id, vehicle_no)
            .await;
        if fleet.is_some() {
            return Ok(fleet);
        }
        return Err(crate::tools::error::AppError::NotFound(format!(
            "Vehicle {} not found in cache",
            vehicle_no
        )));
    }

    // Internal-table check
    if app_state
        .db_vehicle_reader_internal
        .is_vehicle_in_internal(vehicle_no, gtfs_id)
        .await
    {
        let vehicle_data = app_state
            .db_vehicle_reader_internal
            .get_vehicle_data(vehicle_no, gtfs_id, None)
            .await?;
        if vehicle_data.service_type.is_some() {
            return Ok(vehicle_data.service_type);
        }
        return Ok(app_state
            .gtfs_service
            .get_fleet_service_type(gtfs_id, vehicle_no)
            .await);
    }

    // Standard DB path
    let vehicle_data = app_state
        .db_vehicle_reader
        .get_vehicle_data(vehicle_no, None)
        .await?;
    if vehicle_data.service_type.is_some() {
        return Ok(vehicle_data.service_type);
    }
    Ok(app_state
        .gtfs_service
        .get_fleet_service_type(gtfs_id, vehicle_no)
        .await)
}

/// Reconcile active trip for chennai_bus using schedule times.
/// Converts db_start_time/db_end_time to minutes, handles midnight crossing by adding 24h (1440 min),
/// and finds the active trip based on current IST time (or provided cur_time_override).
/// Cases:
/// 1. Waybill status is Online/New/Processed in external, or Online in internal, AND no trip has is_active_trip
/// 2. Waybill status is Closed/Audited in both readers (always apply)
fn reconcile_active_trip_by_schedule(
    response: &mut crate::models::VehicleServiceTypeResponse,
    waybill_status: &crate::models::WaybillStatus,
    cur_time_override: Option<&str>,
) {
    use tracing::info;

    // Determine if schedule-based reconciliation should apply
    let should_apply = match waybill_status {
        // Case 1: Online/New/Processed - apply only if no is_active_trip found
        crate::models::WaybillStatus::Online
        | crate::models::WaybillStatus::Processed
        | crate::models::WaybillStatus::New => {
            // Check if any trip has is_active_trip = true
            let has_any_active = response.is_active_trip
                || response
                    .remaining_trip_details
                    .as_ref()
                    .map(|details| details.iter().any(|t| t.is_active_trip.unwrap_or(false)))
                    .unwrap_or(false);
            !has_any_active // Apply if NO active trip found
        }
        // Case 2: Closed/Audited - always apply
        crate::models::WaybillStatus::Closed | crate::models::WaybillStatus::Audited => true,
        _ => false,
    };

    if !should_apply {
        response.schedule_based_active_trip = Some(false);
        return;
    }

    // Take ownership of remaining trips (into local variable, leaving response empty temporarily)
    let mut all_trips: Vec<crate::models::BusSchedule> =
        response.remaining_trip_details.take().unwrap_or_default();

    // If there's currently an "active" trip, we need to reconstruct it with proper schedule times.
    // Try to find a matching trip in all_trips by trip_number to get the schedule times.
    // If not found, we'll create a synthetic trip using the response's db_start_time/db_end_time.
    if response.is_active_trip && response.trip_number.is_some() {
        let current_trip_num = response.trip_number.unwrap();

        if let Some(pos) = all_trips
            .iter()
            .position(|t| t.trip_number == Some(current_trip_num))
        {
            // Found it - move it to the front
            let trip = all_trips.remove(pos);
            all_trips.insert(0, trip);
        } else {
            // Not found in remaining - create synthetic trip using response's db_start_time/db_end_time
            let current_trip = crate::models::BusSchedule {
                schedule_number: response.schedule_no.clone().unwrap_or_default(),
                route_id: response.route_id.clone().unwrap_or_default(),
                route_name: None,
                org_name: response.depot_no.clone(),
                trip_number: response.trip_number,
                route_number: response.route_number.clone(),
                stops_count: None,
                is_active_trip: Some(true),
                schedule_trip_id: None,
                start_time: None,
                end_time: None,
                deleted: Some(false),
                trip_order: response.trip_number,
                db_start_time: response.db_start_time.clone(),
                db_end_time: response.db_end_time.clone(),
            };
            all_trips.insert(0, current_trip);
        }
    }

    // If there's no active trip but we have db_start_time/db_end_time in the response,
    // create a synthetic trip for the first trip (this handles the case where first trip is not in remaining_trips)
    if !response.is_active_trip && response.trip_number.is_some() && !all_trips.is_empty() {
        // Check if the first trip (by trip_number order) is missing from all_trips
        let first_trip_num = response.trip_number.unwrap();
        let first_trip_exists = all_trips
            .iter()
            .any(|t| t.trip_number == Some(first_trip_num));

        if !first_trip_exists {
            // Create synthetic first trip using response's db_start_time/db_end_time
            let synthetic_first_trip = crate::models::BusSchedule {
                schedule_number: response.schedule_no.clone().unwrap_or_default(),
                route_id: response.route_id.clone().unwrap_or_default(),
                route_name: None,
                org_name: response.depot_no.clone(),
                trip_number: response.trip_number,
                route_number: response.route_number.clone(),
                stops_count: None,
                is_active_trip: Some(false),
                schedule_trip_id: None,
                start_time: None,
                end_time: None,
                deleted: Some(false),
                trip_order: response.trip_number,
                db_start_time: response.db_start_time.clone(),
                db_end_time: response.db_end_time.clone(),
            };
            all_trips.insert(0, synthetic_first_trip);
        }
    }

    if all_trips.is_empty() {
        response.remaining_trip_details = None;
        response.schedule_based_active_trip = Some(false);
        return;
    }

    // Parse time strings to minutes, handling HH:MM format
    fn parse_time_to_minutes(time_str: &str) -> Option<i32> {
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() != 2 {
            return None;
        }
        let hh: i32 = parts[0].parse().ok()?;
        let mm: i32 = parts[1].parse().ok()?;
        Some(hh * 60 + mm)
    }

    // Get current time in minutes (either from override or current IST time)
    let current_minutes = if let Some(cur_time) = cur_time_override {
        // Parse provided time (HH:MM format)
        parse_time_to_minutes(cur_time).unwrap_or_else(|| {
            // Fall back to current IST time if parsing fails
            let now = chrono::Utc::now();
            let ist_offset = chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
            let ist_now = now.with_timezone(&ist_offset);
            (ist_now.hour() * 60 + ist_now.minute()) as i32
        })
    } else {
        // Use current IST time
        let now = chrono::Utc::now();
        let ist_offset = chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
        let ist_now = now.with_timezone(&ist_offset);
        (ist_now.hour() * 60 + ist_now.minute()) as i32
    };

    // Convert trip times to minutes, handling midnight crossing
    let mut trip_times: Vec<(usize, i32, i32, i32)> = Vec::new(); // (index, start_min, end_min, trip_num)
    let mut last_end_time: Option<i32> = None;
    let mut has_shifted_trips = false;

    for (idx, trip) in all_trips.iter().enumerate() {
        let start_min = trip
            .db_start_time
            .as_deref()
            .and_then(parse_time_to_minutes);
        let end_min = trip.db_end_time.as_deref().and_then(parse_time_to_minutes);
        let trip_num = trip.trip_number.unwrap_or(0);

        if let (Some(mut start), Some(mut end)) = (start_min, end_min) {
            // Normalize: if this trip itself crosses midnight (end < start), shift end by 24h
            if end < start {
                end += 1440;
                has_shifted_trips = true;
            }

            // Handle midnight crossing relative to previous trip: if start < last_end, add 24h
            if let Some(last_end) = last_end_time {
                if start < last_end {
                    start += 1440; // Add 24h in minutes
                    end += 1440;
                    has_shifted_trips = true;
                }
            }
            trip_times.push((idx, start, end, trip_num));
            last_end_time = Some(end);
        }
    }

    if trip_times.is_empty() {
        // Restore original remaining trips if we can't process
        response.remaining_trip_details = Some(all_trips);
        response.schedule_based_active_trip = Some(false);
        return;
    }

    // Sort trip_times by start time
    trip_times.sort_by_key(|(_, start, _, _)| *start);

    // Adjust current_minutes for midnight comparison.
    // If we have shifted trips and current time is before the first unshifted trip,
    // add 1440 to current_minutes so we can compare in the same 0-2880 timeline.
    let first_unshifted_start = trip_times
        .iter()
        .find(|(_, start, _, _)| *start < 1440)
        .map(|(_, start, _, _)| *start);

    let adjusted_current_minutes =
        if has_shifted_trips && current_minutes < first_unshifted_start.unwrap_or(0) {
            current_minutes + 1440
        } else {
            current_minutes
        };

    // Find the active trip: the last trip whose start time is <= current time
    // If current time is before all trips, use the last trip (wrap around)
    let mut active_trip_idx: Option<usize> = None;

    // First pass: find last trip that has started
    for (idx, start_min, _, _) in &trip_times {
        if adjusted_current_minutes >= *start_min {
            active_trip_idx = Some(*idx);
        }
    }

    // If no active trip found (current time is before first trip),
    // use the last trip in the day (could be post-midnight)
    if active_trip_idx.is_none() && !trip_times.is_empty() {
        active_trip_idx = Some(trip_times.last().unwrap().0);
    }

    let Some(active_idx) = active_trip_idx else {
        response.remaining_trip_details = Some(all_trips);
        response.schedule_based_active_trip = Some(false);
        return;
    };

    // Get the active trip and its trip number
    let mut active_trip = all_trips.remove(active_idx);
    let active_trip_num = active_trip.trip_number.unwrap_or(0);

    // Normalize is_active_trip flags: set active to true, all others to false
    active_trip.is_active_trip = Some(true);

    // Filter remaining trips to only include those AFTER the active trip (by trip_number)
    // and ensure all have is_active_trip = false
    let remaining_trips: Vec<crate::models::BusSchedule> = all_trips
        .into_iter()
        .filter(|t| t.trip_number.unwrap_or(0) > active_trip_num)
        .map(|mut t| {
            t.is_active_trip = Some(false);
            t
        })
        .collect();

    // Update response
    response.is_active_trip = true;
    response.trip_number = active_trip.trip_number;
    response.route_id = Some(active_trip.route_id.clone());
    response.route_number = active_trip.route_number.clone();
    response.db_start_time = active_trip.db_start_time.clone();
    response.db_end_time = active_trip.db_end_time.clone();
    response.remaining_trip_details = if remaining_trips.is_empty() {
        None
    } else {
        Some(remaining_trips)
    };
    response.schedule_based_active_trip = Some(true);

    info!(
        "Schedule-based reconciliation applied: trip {} is active",
        response.trip_number.unwrap_or(0)
    );
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

    let tag_number = app_state
        .fleet_tag_list
        .get(gtfs_id)
        .and_then(|by_vehicle| by_vehicle.get(vehicle_no))
        .cloned();

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
    info!(
        "chalo_gtfs_ids: {:?}, gtfs_id: {:?}, contains: {:?}",
        chalo_gtfs_ids(),
        gtfs_id,
        is_chalo_gtfs_id(gtfs_id)
    );
    if is_chalo_gtfs_id(gtfs_id) {
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
                bus_tag_number: tag_number,
                waybill_status: None,
                is_historic: false,
                schedule_based_active_trip: None,
                db_start_time: cached_data.db_start_time.clone(),
                db_end_time: cached_data.db_end_time.clone(),
            }));
        } else {
            // Vehicle not found in cache, try to get service type from fleet
            if let Some(service_type) = app_state
                .gtfs_service
                .get_fleet_service_type(gtfs_id, vehicle_no)
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
                    bus_tag_number: tag_number,
                    waybill_status: None,
                    is_historic: false,
                    schedule_based_active_trip: None,
                    db_start_time: None,
                    db_end_time: None,
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

        let is_historic = vehicle_data
            .waybill_status
            .as_ref()
            .map(|s| s.is_historic())
            .unwrap_or(false);

        let mut response = VehicleServiceTypeResponse {
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
            bus_tag_number: tag_number,
            waybill_status: vehicle_data.waybill_status,
            is_historic,
            schedule_based_active_trip: None,
            db_start_time: vehicle_data.db_start_time.clone(),
            db_end_time: vehicle_data.db_end_time.clone(),
        };

        // Apply schedule-based reconciliation if enabled in config
        if app_state.config.enable_schedule_reconciliation {
            if let Some(status) = response.waybill_status.clone() {
                reconcile_active_trip_by_schedule(
                    &mut response,
                    &status,
                    params.cur_time.as_deref(),
                );
            }
        } else {
            // Old behavior: ensure trip 1 is the main trip when no actual active trip exists
            // and remaining_trip_details starts from trip 2
            // Only apply when: no active trip at top level AND no active trip in remaining
            let has_active_in_remaining = response
                .remaining_trip_details
                .as_ref()
                .map(|details| details.iter().any(|t| t.is_active_trip.unwrap_or(false)))
                .unwrap_or(false);

            if !response.is_active_trip
                && !has_active_in_remaining
                && response.trip_number != Some(1)
            {
                // Try to find trip 1 in schedule_details and promote it
                if let Some(ref schedule_map) = vehicle_data.schedule_details {
                    for (_, trips) in schedule_map.iter() {
                        if let Some(trip_1) = trips.iter().find(|t| t.trip_number == Some(1)) {
                            // Insert current main trip into remaining_trip_details
                            let current_main = crate::models::BusSchedule {
                                schedule_number: response.schedule_no.clone().unwrap_or_default(),
                                route_id: response.route_id.clone().unwrap_or_default(),
                                route_name: None,
                                org_name: response.depot_no.clone(),
                                trip_number: response.trip_number,
                                route_number: response.route_number.clone(),
                                stops_count: None,
                                is_active_trip: Some(false),
                                schedule_trip_id: None,
                                start_time: None,
                                end_time: None,
                                deleted: Some(false),
                                trip_order: response.trip_number,
                                db_start_time: response.db_start_time.clone(),
                                db_end_time: response.db_end_time.clone(),
                            };

                            // Update response to use trip 1
                            response.trip_number = trip_1.trip_number;
                            response.route_id = Some(trip_1.route_id.clone());
                            response.route_number = trip_1.route_number.clone();
                            response.db_start_time = trip_1.db_start_time.clone();
                            response.db_end_time = trip_1.db_end_time.clone();
                            response.is_active_trip = false;

                            // Rebuild remaining_trip_details with trip 1 excluded
                            let mut new_remaining: Vec<crate::models::BusSchedule> =
                                vec![current_main];
                            if let Some(ref existing) = response.remaining_trip_details {
                                for t in existing.iter() {
                                    if t.trip_number != Some(1) {
                                        new_remaining.push(t.clone());
                                    }
                                }
                            }
                            // Sort by trip_number
                            new_remaining.sort_by_key(|t| t.trip_number.unwrap_or(0));
                            response.remaining_trip_details = if new_remaining.is_empty() {
                                None
                            } else {
                                Some(new_remaining)
                            };
                            break;
                        }
                    }
                }
            } else if response.trip_number == Some(1) {
                // Trip 1 is already the main trip, just ensure remaining starts from trip 2
                // and no trip is marked as active (since none is actually active)
                if let Some(ref mut trips) = response.remaining_trip_details {
                    for t in trips.iter_mut() {
                        t.is_active_trip = Some(false);
                    }
                }
            }
        }

        return Ok(HttpResponse::Ok().json(response));
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

    let is_historic = vehicle_data
        .waybill_status
        .as_ref()
        .map(|s| s.is_historic())
        .unwrap_or(false);

    let mut response = VehicleServiceTypeResponse {
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
        bus_tag_number: tag_number,
        waybill_status: vehicle_data.waybill_status,
        is_historic,
        schedule_based_active_trip: None,
        db_start_time: vehicle_data.db_start_time.clone(),
        db_end_time: vehicle_data.db_end_time.clone(),
    };

    // Apply schedule-based reconciliation if enabled in config
    if app_state.config.enable_schedule_reconciliation {
        if let Some(status) = response.waybill_status.clone() {
            reconcile_active_trip_by_schedule(&mut response, &status, params.cur_time.as_deref());
        }
    } else {
        // Old behavior: ensure trip 1 is the main trip when no actual active trip exists
        // and remaining_trip_details starts from trip 2
        // Only apply when: no active trip at top level AND no active trip in remaining
        let has_active_in_remaining = response
            .remaining_trip_details
            .as_ref()
            .map(|details| details.iter().any(|t| t.is_active_trip.unwrap_or(false)))
            .unwrap_or(false);

        if !response.is_active_trip && !has_active_in_remaining && response.trip_number != Some(1) {
            // Try to find trip 1 in schedule_details and promote it
            if let Some(ref schedule_map) = vehicle_data.schedule_details {
                for (_, trips) in schedule_map.iter() {
                    if let Some(trip_1) = trips.iter().find(|t| t.trip_number == Some(1)) {
                        // Insert current main trip into remaining_trip_details
                        let current_main = crate::models::BusSchedule {
                            schedule_number: response.schedule_no.clone().unwrap_or_default(),
                            route_id: response.route_id.clone().unwrap_or_default(),
                            route_name: None,
                            org_name: response.depot_no.clone(),
                            trip_number: response.trip_number,
                            route_number: response.route_number.clone(),
                            stops_count: None,
                            is_active_trip: Some(false),
                            schedule_trip_id: None,
                            start_time: None,
                            end_time: None,
                            deleted: Some(false),
                            trip_order: response.trip_number,
                            db_start_time: response.db_start_time.clone(),
                            db_end_time: response.db_end_time.clone(),
                        };

                        // Update response to use trip 1
                        response.trip_number = trip_1.trip_number;
                        response.route_id = Some(trip_1.route_id.clone());
                        response.route_number = trip_1.route_number.clone();
                        response.db_start_time = trip_1.db_start_time.clone();
                        response.db_end_time = trip_1.db_end_time.clone();
                        response.is_active_trip = false;

                        // Rebuild remaining_trip_details with trip 1 excluded
                        let mut new_remaining: Vec<crate::models::BusSchedule> = vec![current_main];
                        if let Some(ref existing) = response.remaining_trip_details {
                            for t in existing.iter() {
                                if t.trip_number != Some(1) {
                                    new_remaining.push(t.clone());
                                }
                            }
                        }
                        // Sort by trip_number
                        new_remaining.sort_by_key(|t| t.trip_number.unwrap_or(0));
                        response.remaining_trip_details = if new_remaining.is_empty() {
                            None
                        } else {
                            Some(new_remaining)
                        };
                        break;
                    }
                }
            }
        } else if response.trip_number == Some(1) {
            // Trip 1 is already the main trip, just ensure remaining starts from trip 2
            // and no trip is marked as active (since none is actually active)
            if let Some(ref mut trips) = response.remaining_trip_details {
                for t in trips.iter_mut() {
                    t.is_active_trip = Some(false);
                }
            }
        }
    }

    Ok(HttpResponse::Ok().json(response))
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

#[utoipa::path(
    get,
    path = "/vehicle/{gtfs_id}/{vehicle_no}/info",
    tag = "Vehicle",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("vehicle_no" = String, Path, description = "Vehicle number"),
    ),
    responses((status = 200, description = "Vehicle info"))
)]
pub async fn get_vehicle_info(
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

#[utoipa::path(
    get,
    path = "/memory-stats",
    tag = "System",
    responses((status = 200, description = "Memory usage statistics", body = MemoryUsageStats))
)]
pub async fn get_memory_stats(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let stats = app_state.gtfs_service.get_memory_stats().await;
    Ok(HttpResponse::Ok().json(stats))
}

#[utoipa::path(
    get,
    path = "/cached-data",
    tag = "System",
    responses((status = 200, description = "All cached GTFS data"))
)]
pub async fn get_all_cached_data(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let bytes = app_state.gtfs_service.get_cached_data_bytes().await;
    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(bytes.to_vec()))
}

#[utoipa::path(
    get,
    path = "/config",
    tag = "System",
    responses((status = 200, description = "Current server configuration"))
)]
pub async fn get_config(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    // Get feeds loaded in memory from routes
    let feeds_in_memory = app_state.gtfs_service.get_feeds_in_memory().await;

    let response = serde_json::json!({
        "config": app_state.config.clone(),
        "feeds_loaded": feeds_in_memory
    });

    Ok(HttpResponse::Ok().json(response))
}

#[derive(Debug, serde::Deserialize, ToSchema)]
pub struct GraphQLRequest {
    pub query: String,
    pub variables: Option<serde_json::Value>,
    pub operation_name: Option<String>,
    pub city: Option<String>,
    #[serde(alias = "feedId")]
    pub gtfs_id: Option<String>,
}

#[utoipa::path(
    post,
    path = "/graphql",
    tag = "System",
    request_body = GraphQLRequest,
    responses((status = 200, description = "GraphQL query result"))
)]
pub async fn graphql_query(
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

#[utoipa::path(
    get,
    path = "/connection-stats",
    tag = "System",
    responses((status = 200, description = "Database and HTTP connection statistics"))
)]
pub async fn get_connection_stats(app_state: Data<AppState>) -> AppResult<HttpResponse> {
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

#[utoipa::path(
    get,
    path = "/trip/{trip_id}",
    tag = "Trip",
    params(
        ("trip_id" = String, Path, description = "Trip identifier"),
        ("gtfs_id" = Option<String>, Query, description = "GTFS feed identifier"),
        ("city" = Option<String>, Query, description = "City name"),
    ),
    responses((status = 200, description = "Trip data", body = TripDetails))
)]
pub async fn get_trip_data(
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

#[utoipa::path(
    get,
    path = "/waybill/{gtfs_id}/metadata/{waybill_no}",
    tag = "Waybill",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("waybill_no" = String, Path, description = "Waybill number"),
    ),
    responses((status = 200, description = "Waybill metadata with driver details", body = crate::models::WaybillMetadataResponse))
)]
pub async fn get_waybill_metadata(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, waybill_no) = path.into_inner();

    let waybill_metadata = app_state
        .db_vehicle_reader_internal
        .get_waybill_metadata(&gtfs_id, &waybill_no)
        .await?;

    Ok(HttpResponse::Ok().json(waybill_metadata))
}

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

fn calculate_eta_from_db(
    route_stop_mappings: &[std::sync::Arc<RouteStopMapping>],
    trip_start_time: Option<i64>,
    db_etas: &HashMap<(String, String), i32>,
) -> Vec<crate::models::BusStopETA> {
    let mut bus_stop_etas: Vec<crate::models::BusStopETA> = Vec::new();
    let now = chrono::Utc::now();

    // Calculate cumulative travel time to each stop
    let mut cumulative_time_seconds: f64 = 0.0;

    for (idx, mapping) in route_stop_mappings.iter().enumerate() {
        if idx > 0 {
            let prev_mapping = &route_stop_mappings[idx - 1];
            let pair = (
                prev_mapping.stop_code.to_string(),
                mapping.stop_code.to_string(),
            );

            let time_seconds = if let Some(&eta_secs) = db_etas.get(&pair) {
                // Get ETA for this consecutive pair from DB (value is already in seconds)
                let prev_eta = eta_secs as f64;
                info!(
                    "calculate_eta_from_db - using DB pair ({}, {}): {} secs",
                    prev_mapping.stop_code, mapping.stop_code, eta_secs
                );
                prev_eta
            } else {
                // If not found (new stop inserted), calculate distance from previous stop to current stop
                let distance_km = haversine_distance(
                    prev_mapping.stop_point.lat,
                    prev_mapping.stop_point.lon,
                    mapping.stop_point.lat,
                    mapping.stop_point.lon,
                );

                // Calculate time to travel this distance at 25 km/hr
                let time_hours = distance_km / SPEED_KM_PER_HOUR;
                let haversine_eta = time_hours * 3600.0;
                info!(
                    "calculate_eta_from_db - fallback haversine pair ({}, {}): {} km -> {} secs",
                    prev_mapping.stop_code, mapping.stop_code, distance_km, haversine_eta
                );
                haversine_eta
            };

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
            stop_code: mapping.stop_code.to_string(),
            arrival_time: arrival_epoch,
            eta_seconds,
            stop_name: Some(mapping.stop_name.to_string()),
        });
    }

    bus_stop_etas
}

/// Calculate arrival time and ETA based on haversine distance between stops
/// Assumes constant speed of 25 km/hr
fn calculate_eta_from_haversine_distance(
    route_stop_mappings: &[std::sync::Arc<RouteStopMapping>],
    trip_start_time: Option<i64>,
) -> Vec<crate::models::BusStopETA> {
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
            stop_code: mapping.stop_code.to_string(),
            arrival_time: arrival_epoch,
            eta_seconds,
            stop_name: Some(mapping.stop_name.to_string()),
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
    #[serde(rename = "vehicleNumber")]
    pub vehicle_number: Option<String>,
}

#[utoipa::path(
    get,
    path = "/bus-trip-schedule/{gtfs_id}/{waybill_no}/{trip_number}/{route_id}",
    tag = "Schedule",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("waybill_no" = String, Path, description = "Waybill number"),
        ("trip_number" = i32, Path, description = "Trip number"),
        ("route_id" = String, Path, description = "Route identifier"),
    ),
    responses((status = 200, description = "Bus trip schedule", body = Vec<BusScheduleDetail>))
)]
pub async fn get_bus_trip_schedule(
    app_state: Data<AppState>,
    path: Path<(String, String, i32, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, waybill_no, trip_number, route_id) = path.into_inner();

    let route_stop_mappings = app_state
        .gtfs_service
        .get_route_stop_mapping_by_route(&gtfs_id, &route_id)
        .await
        .unwrap_or_default();

    // kolkata_bus: internal only; chennai_bus: both external + internal
    let (external_rows, internal_rows) = if INTERNAL_ONLY_GTFS_IDS.contains(&gtfs_id.as_str()) {
        (
            vec![],
            app_state
                .db_vehicle_reader_internal
                .get_waybill_by_waybill_and_trip(&waybill_no, trip_number, &gtfs_id)
                .await
                .unwrap_or_default(),
        )
    } else {
        (
            app_state
                .db_vehicle_reader
                .get_chennai_waybill_by_waybill_and_trip(&waybill_no, trip_number)
                .await?,
            app_state
                .db_vehicle_reader_internal
                .get_waybill_by_waybill_and_trip(&waybill_no, trip_number, &gtfs_id)
                .await
                .unwrap_or_default(),
        )
    };

    let all: Vec<crate::models::VehicleData> = external_rows
        .into_iter()
        .chain(internal_rows.into_iter())
        .collect();

    let mut schedule_details: BusScheduleDetails = Vec::new();
    for row in all {
        let trip_start_time: Option<i64> = if let (Some(hhmm), Some(duty)) =
            (row.db_start_time.as_deref(), row.duty_date.as_deref())
        {
            let date = chrono::NaiveDate::parse_from_str(duty, "%Y-%m-%d").ok();
            let time = chrono::NaiveTime::parse_from_str(hhmm, "%H:%M").ok();
            if let (Some(d), Some(t)) = (date, time) {
                let dt = chrono::NaiveDateTime::new(d, t);
                if let Some(offset) = chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60) {
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

        // Calculate ETAs

        let bus_stop_etas = match app_state
            .db_vehicle_reader_internal
            .get_station_etas(&gtfs_id)
            .await
        {
            Ok(db_etas) if !db_etas.is_empty() => {
                calculate_eta_from_db(&route_stop_mappings, trip_start_time, &db_etas)
            }
            _ => calculate_eta_from_haversine_distance(&route_stop_mappings, trip_start_time),
        };

        schedule_details.push(crate::models::BusScheduleDetail {
            eta: bus_stop_etas,
            vehicle_no: row.vehicle_no,
            service_tier: row.service_type,
            trip_number: row.trip_number,
            is_active_trip: row.is_active_trip,
            waybill_no: Some(row.waybill_no),
            is_completed: row.is_completed,
        });
    }

    Ok(HttpResponse::Ok().json(schedule_details))
}

#[utoipa::path(
    get,
    path = "/bus-route-schedule/{gtfs_id}/{route_id}",
    tag = "Schedule",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("route_id" = String, Path, description = "Route identifier"),
        ("justInternal" = Option<bool>, Query, description = "Only internal vehicles"),
        ("justExternal" = Option<bool>, Query, description = "Only external vehicles"),
        ("vehicleNumber" = Option<String>, Query, description = "Filter by vehicle number"),
    ),
    responses((status = 200, description = "Bus route schedule", body = Vec<BusScheduleDetail>))
)]
pub async fn get_bus_route_schedule(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    query: web::Query<BusRouteScheduleQuery>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, route_id) = path.into_inner();

    // Operator gtfs_ids (e.g. chennai_bus, kolkata_bus) - internal DB flow
    // Single join query returns waybills + trip (bstd & bstf) times
    // No per-vehicle get_vehicle_data call req
    if SUPPORTED_OPERATOR_GTFS_IDS.contains(&gtfs_id.as_str()) {
        let just_internal = query.just_internal.unwrap_or(false);
        let just_external = query.just_external.unwrap_or(false);
        let vehicle_number = query.vehicle_number.as_deref();

        let route_stop_mappings = app_state
            .gtfs_service
            .get_route_stop_mapping_by_route(&gtfs_id, &route_id)
            .await
            .unwrap_or_default();

        let mut all_rows = Vec::new();

        // Fetch from external tables unless the query forces internal-only
        // or the gtfs_id is listed as internal-only.
        let fetch_external =
            !just_internal && !INTERNAL_ONLY_GTFS_IDS.contains(&gtfs_id.as_str());
        if fetch_external {
            let mut ext_rows = app_state
                .db_vehicle_reader
                .get_chennai_waybills_by_route_id(&route_id, vehicle_number)
                .await?;
            all_rows.append(&mut ext_rows);
        }

        // Fetch from internal tables unless the query forces external-only
        // or the gtfs_id is listed as external-only.
        let fetch_internal =
            !just_external && !EXTERNAL_ONLY_GTFS_IDS.contains(&gtfs_id.as_str());
        if fetch_internal {
            let mut int_rows = app_state
                .db_vehicle_reader_internal
                .get_waybills_by_route_id(&route_id, &gtfs_id, vehicle_number)
                .await?;
            all_rows.append(&mut int_rows);
        }

        let mut schedule_details: BusScheduleDetails = Vec::new();
        for row in all_rows {
            // Resolve trip start time from (db_start_time HH:MM + duty_date) or
            // fall back to the stored epoch-millis in start_time_epoch.
            let trip_start_time: Option<i64> = if let (Some(hhmm), Some(duty)) =
                (row.db_start_time.as_deref(), row.duty_date.as_deref())
            {
                let date = chrono::NaiveDate::parse_from_str(duty, "%Y-%m-%d").ok();
                let time = chrono::NaiveTime::parse_from_str(hhmm, "%H:%M").ok();
                if let (Some(d), Some(t)) = (date, time) {
                    let dt = chrono::NaiveDateTime::new(d, t);
                    if let Some(offset) = chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60) {
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

            let bus_stop_etas = match app_state
                .db_vehicle_reader_internal
                .get_station_etas(&gtfs_id)
                .await
            {
                Ok(db_etas) if !db_etas.is_empty() => {
                    calculate_eta_from_db(&route_stop_mappings, trip_start_time, &db_etas)
                }
                _ => calculate_eta_from_haversine_distance(&route_stop_mappings, trip_start_time),
            };

            let is_upcoming = row
                .status
                .as_deref()
                .map(|s| s.eq_ignore_ascii_case("upcoming"))
                .unwrap_or(false);
            schedule_details.push(crate::models::BusScheduleDetail {
                eta: bus_stop_etas,
                vehicle_no: row.vehicle_no,
                service_tier: row.service_type,
                trip_number: row.trip_number,
                is_active_trip: if is_upcoming {
                    Some(false)
                } else {
                    row.is_active_trip
                },
                waybill_no: Some(row.waybill_no),
                is_completed: row.is_completed,
            });
        }

        return Ok(HttpResponse::Ok().json(schedule_details));
    }

    let waybills = if is_chalo_gtfs_id(gtfs_id.as_str()) {
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
                    is_active_trip: None,
                    is_completed: None,
                })
                .collect()
        } else {
            // return null
            Vec::new()
        }
    } else {
        //return null
        Vec::new()
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
            is_active_trip: None,
            waybill_no: None,
            is_completed: None,
        });
    }

    Ok(HttpResponse::Ok().json(schedule_details))
}

#[utoipa::path(
    get,
    path = "/trip-cache/stats",
    tag = "System",
    responses((status = 200, description = "Trip cache statistics"))
)]
pub async fn get_trip_cache_stats(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let stats = app_state.trip_service.get_cache_stats().await;
    Ok(HttpResponse::Ok().json(stats))
}

#[utoipa::path(
    post,
    path = "/trip-cache/clear",
    tag = "System",
    responses((status = 200, description = "Trip cache cleared"))
)]
pub async fn clear_trip_cache(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    app_state.trip_service.clear_cache().await?;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "message": "Trip cache cleared successfully"
    })))
}

#[utoipa::path(
    post,
    path = "/refresh-data",
    tag = "System",
    responses((status = 200, description = "GTFS data refresh initiated"))
)]
pub async fn force_refresh_data(app_state: Data<AppState>) -> AppResult<HttpResponse> {
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

#[utoipa::path(
    post,
    path = "/getAllRoutesByIds",
    tag = "Bulk",
    request_body = GetAllRoutesByIdsRequest,
    responses((status = 200, description = "Routes matching IDs", body = Vec<NandiRoutesRes>))
)]
pub async fn get_all_routes_by_ids(
    app_state: Data<AppState>,
    payload: Json<GetAllRoutesByIdsRequest>,
) -> AppResult<HttpResponse> {
    let routes = app_state
        .gtfs_service
        .get_routes_by_ids(&payload.gtfs_id, payload.route_ids.clone())
        .await?;

    Ok(HttpResponse::Ok().json(routes))
}

#[utoipa::path(
    post,
    path = "/getAllStopsByIds",
    tag = "Bulk",
    request_body = GetAllStopsByIdsRequest,
    responses((status = 200, description = "Stops matching IDs", body = Vec<GTFSStop>))
)]
pub async fn get_all_stops_by_ids(
    app_state: Data<AppState>,
    payload: Json<GetAllStopsByIdsRequest>,
) -> AppResult<HttpResponse> {
    let stops = app_state
        .gtfs_service
        .get_stops_by_ids(&payload.gtfs_id, payload.stop_ids.clone())
        .await?;

    Ok(HttpResponse::Ok().json(stops))
}

#[utoipa::path(
    post,
    path = "/getAllRouteStopMappingsByRouteCodes",
    tag = "Bulk",
    request_body = GetAllRouteStopMappingsByRouteCodesRequest,
    responses((status = 200, description = "Route-stop mappings for route codes", body = Vec<RouteStopMapping>))
)]
pub async fn get_all_route_stop_mappings_by_route_codes(
    app_state: Data<AppState>,
    payload: Json<GetAllRouteStopMappingsByRouteCodesRequest>,
) -> AppResult<HttpResponse> {
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mappings_by_route_codes(&payload.gtfs_id, payload.route_codes.clone())
        .await?;

    Ok(HttpResponse::Ok().json(mappings))
}

#[utoipa::path(
    post,
    path = "/getAllRouteStopMappingsByStopCodes",
    tag = "Bulk",
    request_body = GetAllRouteStopMappingsByStopCodesRequest,
    responses((status = 200, description = "Route-stop mappings for stop codes", body = Vec<RouteStopMapping>))
)]
pub async fn get_all_route_stop_mappings_by_stop_codes(
    app_state: Data<AppState>,
    payload: Json<GetAllRouteStopMappingsByStopCodesRequest>,
) -> AppResult<HttpResponse> {
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mappings_by_stop_codes(&payload.gtfs_id, payload.stop_codes.clone())
        .await?;

    Ok(HttpResponse::Ok().json(mappings))
}

#[utoipa::path(
    post,
    path = "/getAllVehiclesByIds",
    tag = "Bulk",
    request_body = GetAllVehiclesByIdsRequest,
    responses((status = 200, description = "Vehicles matching IDs", body = Vec<VehicleData>))
)]
pub async fn get_all_vehicles_by_ids(
    app_state: Data<AppState>,
    payload: Json<GetAllVehiclesByIdsRequest>,
) -> AppResult<HttpResponse> {
    let vehicles = app_state
        .db_vehicle_reader
        .get_vehicles_by_ids(payload.vehicle_ids.clone())
        .await?;

    Ok(HttpResponse::Ok().json(vehicles))
}

#[utoipa::path(
    get,
    path = "/vehicles/{gtfs_id}/list/service-tier/{serviceTier}",
    tag = "Vehicle",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("serviceTier" = String, Path, description = "Service tier type"),
    ),
    responses((status = 200, description = "Vehicles by service tier", body = Vec<VehicleData>))
)]
pub async fn get_vehicles_by_service_tier(
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

#[utoipa::path(
    get,
    path = "/alternateStops/{gtfs_id}/{stop_code}",
    tag = "Stops",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("stop_code" = String, Path, description = "Stop code"),
    ),
    responses((status = 200, description = "Alternate stops", body = Vec<RouteStopMapping>))
)]
pub async fn get_alternate_stops(
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

#[utoipa::path(
    get,
    path = "/cache-data/{gtfs_id}",
    tag = "System",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "Cached vehicle data for feed"))
)]
pub async fn get_cache_data_by_gtfs_id(
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

fn check_gtfs_id(gtfs_id: &str) -> AppResult<()> {
    if SUPPORTED_OPERATOR_GTFS_IDS.contains(&gtfs_id) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "gtfs_id '{}' is not supported for operator APIs. Supported: {:?}",
            gtfs_id, SUPPORTED_OPERATOR_GTFS_IDS
        )))
    }
}

#[derive(Deserialize, ToSchema)]
pub struct PaginationQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
pub struct TokenQuery {
    pub token: String,
}

#[derive(Deserialize, ToSchema)]
pub struct ScheduleNumberQuery {
    #[serde(rename = "scheduleNumber")]
    pub schedule_number: String,
}

#[derive(Deserialize, ToSchema)]
pub struct RoleQuery {
    pub role: String,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateWaybillStatusBody {
    pub waybill_id: String,
    pub status: String,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateWaybillFleetBody {
    pub waybill_id: String,
    pub fleet_no: String,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateWaybillTabletBody {
    pub waybill_id: String,
    pub tablet_id: String,
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/crud/{table}",
    tag = "Internal Operator",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("table" = String, Path, description = "Table name"),
    ),
    responses((status = 200, description = "Single row from table"))
)]
pub async fn get_one_row(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    query: Query<HashMap<String, String>>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, table) = path.into_inner();
    check_gtfs_id(&gtfs_id)?;

    let result = app_state
        .operator_service
        .get_one_row(&table, &gtfs_id, query.into_inner())
        .await?;

    match result {
        Some(row) => Ok(HttpResponse::Ok().json(row)),
        None => Ok(HttpResponse::NotFound().json(json!({"error": "Row not found"}))),
    }
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/crud/{table}/all",
    tag = "Internal Operator",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("table" = String, Path, description = "Table name"),
        ("limit" = Option<i64>, Query, description = "Limit (default 15)"),
        ("offset" = Option<i64>, Query, description = "Offset (default 0)"),
    ),
    responses((status = 200, description = "All rows from table"))
)]
pub async fn get_all_rows(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    query: Query<PaginationQuery>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, table) = path.into_inner();
    check_gtfs_id(&gtfs_id)?;

    let limit = query.limit.unwrap_or(15);
    let offset = query.offset.unwrap_or(0);

    let rows = app_state
        .operator_service
        .get_all_rows(&table, &gtfs_id, limit, offset)
        .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post,
    path = "/internal/operator/{gtfs_id}/crud/{table}/query",
    tag = "Internal Operator",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("table" = String, Path, description = "Table name"),
    ),
    request_body = QueryBody,
    responses((status = 200, description = "Filtered rows from table"))
)]
pub async fn query_rows_handler(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    body: Json<QueryBody>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, table) = path.into_inner();
    check_gtfs_id(&gtfs_id)?;

    let rows = app_state
        .operator_service
        .query_rows(&table, &gtfs_id, body.into_inner())
        .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post,
    path = "/internal/operator/{gtfs_id}/crud/{table}/delete",
    tag = "Internal Operator",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("table" = String, Path, description = "Table name"),
    ),
    request_body = serde_json::Value,
    responses((status = 200, description = "Row deleted"))
)]
pub async fn delete_one_row(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    body: Json<Value>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, table) = path.into_inner();
    check_gtfs_id(&gtfs_id)?;

    let rows_affected = app_state
        .operator_service
        .delete_one_row(&table, &gtfs_id, body.into_inner())
        .await?;

    Ok(HttpResponse::Ok().json(json!({
        "message": "Row deleted successfully",
        "rows_affected": rows_affected
    })))
}

#[utoipa::path(
    post,
    path = "/internal/operator/{gtfs_id}/crud/{table}/upsert",
    tag = "Internal Operator",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("table" = String, Path, description = "Table name"),
    ),
    request_body = serde_json::Value,
    responses((status = 200, description = "Row upserted"))
)]
pub async fn upsert_one_row(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    body: Json<Value>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, table) = path.into_inner();
    check_gtfs_id(&gtfs_id)?;

    // Extract to_regen from request body if present
    let mut body_value = body.into_inner();
    let to_regen = body_value
        .get("to_regen")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<String>>()
        });

    // Remove to_regen from body before passing to service
    if body_value.get("to_regen").is_some() {
        body_value.as_object_mut().map(|obj| obj.remove("to_regen"));
    }

    let result = app_state
        .operator_service
        .upsert_one_row(&table, &gtfs_id, body_value, to_regen)
        .await?;

    Ok(HttpResponse::Ok().json(result))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/service-types",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of service types"))
)]
pub async fn get_service_types(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let list = app_state
        .operator_service
        .get_service_types_list(&gtfs_id)
        .await?;
    Ok(HttpResponse::Ok().json(list))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/routes",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of operator routes"))
)]
pub async fn get_operator_routes(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let list = app_state.operator_service.get_routes_list(&gtfs_id).await?;
    Ok(HttpResponse::Ok().json(list))
}

// ===== Stop & route management (clubber / editor) =====

#[derive(Debug, Deserialize)]
pub struct StopSearchQuery {
    pub q: String,
    pub limit: Option<i64>,
    #[serde(rename = "withRoutes")]
    pub with_routes: Option<bool>,
}

/// GET /internal/operator/{gtfs_id}/stops/search
pub async fn search_stops(
    app_state: Data<AppState>,
    path: Path<String>,
    query: Query<StopSearchQuery>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let limit = query.limit.unwrap_or(20).clamp(1, 200);
    let res = app_state
        .operator_service
        .search_stops(&gtfs_id, &query.q, limit, query.with_routes.unwrap_or(false))
        .await?;
    Ok(HttpResponse::Ok().json(res))
}

#[derive(Debug, Deserialize)]
pub struct NearbyQuery {
    pub lat: f64,
    pub lon: f64,
    pub radius: Option<f64>,
    pub limit: Option<i64>,
    #[serde(rename = "withRoutes")]
    pub with_routes: Option<bool>,
}

/// GET /internal/operator/{gtfs_id}/stops/nearby
pub async fn nearby_stops(
    app_state: Data<AppState>,
    path: Path<String>,
    query: Query<NearbyQuery>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let radius = query.radius.unwrap_or(500.0);
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let res = app_state
        .operator_service
        .nearby_stops(
            &gtfs_id,
            query.lat,
            query.lon,
            radius,
            limit,
            query.with_routes.unwrap_or(false),
        )
        .await?;
    Ok(HttpResponse::Ok().json(res))
}

#[derive(Debug, Deserialize)]
pub struct BulkReplaceBody {
    pub from: Vec<String>,
    pub to: String,
}

/// POST /internal/operator/{gtfs_id}/stops/bulk-replace
pub async fn bulk_replace_stops(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<BulkReplaceBody>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let body = body.into_inner();
    let res = app_state
        .operator_service
        .bulk_replace_stops(&gtfs_id, &body.from, &body.to)
        .await?;
    Ok(HttpResponse::Ok().json(res))
}

/// GET /internal/operator/{gtfs_id}/routes/{route_id}/stops
pub async fn get_route_stops(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, route_id) = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let res = app_state
        .operator_service
        .get_route_stops(&gtfs_id, &route_id)
        .await?;
    Ok(HttpResponse::Ok().json(res))
}

/// POST /internal/operator/{gtfs_id}/routes/{route_id}/stops/insert
pub async fn insert_route_stop(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    body: Json<Value>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, route_id) = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let data = body.into_inner();
    let position = data
        .get("position")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| AppError::BadRequest("position is required".to_string()))?;
    if position < 1 {
        return Err(AppError::BadRequest("position must be >= 1".to_string()));
    }
    let res = app_state
        .operator_service
        .insert_route_stop(&gtfs_id, &route_id, position, data)
        .await?;
    if let Err(e) = app_state
        .operator_service
        .reprocess_routes(&gtfs_id, &[route_id.clone()], false)
        .await
    {
        warn!(
            "insert_route_stop: stop inserted (route={}, gtfs={}) but reprocess failed — stage_no may be stale, trigger reprocess manually: {}",
            route_id, gtfs_id, e
        );
    }
    Ok(HttpResponse::Ok().json(res))
}

#[derive(Debug, Deserialize)]
pub struct ReprocessBody {
    #[serde(rename = "routeIds")]
    pub route_ids: Vec<String>,
    #[serde(rename = "recomputePolyline")]
    pub recompute_polyline: Option<bool>,
}

/// POST /internal/operator/{gtfs_id}/routes/reprocess
pub async fn reprocess_routes(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<ReprocessBody>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let body = body.into_inner();
    let res = app_state
        .operator_service
        .reprocess_routes(&gtfs_id, &body.route_ids, body.recompute_polyline.unwrap_or(false))
        .await?;
    Ok(HttpResponse::Ok().json(res))
}

/// GET /internal/operator/{gtfs_id}/export/route-stop-mapping
/// Streams a JSON array row-by-row — never materialises the full result set in memory.
pub async fn export_route_stop_mapping(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;

    if let Some(pool) = app_state.operator_service.pool() {
        let pool = pool.clone();
        let gtfs_id = gtfs_id.clone();
        let sql = "SELECT row_to_json(t) FROM ( \
               SELECT rp.route_id AS \"routeId\", r.route_number AS \"routeNumber\", \
                 rp.bus_stop_id AS \"stopId\", s.bus_stop_name AS \"stopName\", \
                 s.latitude_current AS latitude, s.longitude_current AS longitude, \
                 rp.stop_type AS \"stopType\", rp.stage_no AS \"stageNo\", \
                 rp.route_order AS \"stopSequence\", rp.stage_name AS \"stageName\", \
                 rp.route_id AS \"providerId\" \
               FROM route_point_internal rp \
               JOIN route_internal r ON r.route_id = rp.route_id AND r.gtfs_id = rp.gtfs_id AND r.deleted=false \
               LEFT JOIN stop_internal s ON s.bus_stop_id = rp.bus_stop_id AND s.gtfs_id = rp.gtfs_id AND s.deleted=false \
               WHERE rp.gtfs_id=$1 AND rp.deleted=false \
               ORDER BY rp.route_id, rp.route_order \
             ) t";
        let stream = async_stream! {
            let mut row_stream = sqlx::query_scalar::<_, serde_json::Value>(sql)
                .bind(gtfs_id)
                .fetch(&pool);
            let mut first = true;
            yield Ok::<Bytes, actix_web::Error>(Bytes::from("["));
            while let Some(result) = row_stream.next().await {
                match result {
                    Ok(val) => {
                        let chunk = if first {
                            first = false;
                            val.to_string()
                        } else {
                            format!(",{}", val)
                        };
                        yield Ok(Bytes::from(chunk));
                    }
                    Err(e) => {
                        yield Err(actix_web::error::ErrorInternalServerError(e));
                        return;
                    }
                }
            }
            yield Ok(Bytes::from("]"));
        };
        Ok(HttpResponse::Ok()
            .content_type("application/json")
            .streaming(stream))
    } else {
        // Mock service path: fall back to buffered response
        let rows = app_state
            .operator_service
            .export_route_stop_mapping(&gtfs_id)
            .await?;
        Ok(HttpResponse::Ok().json(rows))
    }
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/depots",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of depots"))
)]
pub async fn get_depots(app_state: Data<AppState>, path: Path<String>) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let list = app_state
        .operator_service
        .get_depot_names_and_ids(&gtfs_id)
        .await?;
    Ok(HttpResponse::Ok().json(list))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/shift-types",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of shift types"))
)]
pub async fn get_shift_types(path: Path<String>) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    Ok(HttpResponse::Ok().json(shift_types()))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/schedule-numbers",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of schedule numbers"))
)]
pub async fn get_schedule_numbers(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let list = app_state
        .operator_service
        .get_schedule_numbers(&gtfs_id)
        .await?;
    Ok(HttpResponse::Ok().json(list))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/day-types",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of day types"))
)]
pub async fn get_day_types(path: Path<String>) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    Ok(HttpResponse::Ok().json(day_types()))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/trip-types",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of trip types"))
)]
pub async fn get_trip_types(path: Path<String>) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    Ok(HttpResponse::Ok().json(trip_types()))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/break-types",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of break types"))
)]
pub async fn get_break_types_handler(path: Path<String>) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    Ok(HttpResponse::Ok().json(break_types()))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/trip-details",
    tag = "Internal Operator",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("scheduleNumber" = String, Query, description = "Schedule number"),
    ),
    responses((status = 200, description = "Trip details for schedule"))
)]
pub async fn get_trip_details(
    app_state: Data<AppState>,
    path: Path<String>,
    query: Query<ScheduleNumberQuery>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let details = app_state
        .operator_service
        .get_schedule_trip_details_by_schedule_number(&gtfs_id, &query.schedule_number)
        .await?;
    Ok(HttpResponse::Ok().json(details))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/fleets",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of fleets"))
)]
pub async fn get_fleets(app_state: Data<AppState>, path: Path<String>) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let list = app_state.operator_service.get_fleets(&gtfs_id).await?;
    Ok(HttpResponse::Ok().json(list))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/conductors",
    tag = "Internal Operator",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("token" = String, Query, description = "Conductor token"),
    ),
    responses((status = 200, description = "Conductor data"))
)]
pub async fn get_conductor_data(
    app_state: Data<AppState>,
    path: Path<String>,
    query: Query<TokenQuery>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let emp = app_state
        .operator_service
        .get_conductor_data(&gtfs_id, &query.token)
        .await?;
    match emp {
        Some(e) => Ok(HttpResponse::Ok().json(e)),
        None => Ok(HttpResponse::NotFound().json(json!({"error": "Conductor not found"}))),
    }
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/drivers",
    tag = "Internal Operator",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("token" = String, Query, description = "Driver token"),
    ),
    responses((status = 200, description = "Driver info"))
)]
pub async fn get_driver_info(
    app_state: Data<AppState>,
    path: Path<String>,
    query: Query<TokenQuery>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let emp = app_state
        .operator_service
        .get_driver_info(&gtfs_id, &query.token)
        .await?;
    match emp {
        Some(e) => Ok(HttpResponse::Ok().json(e)),
        None => Ok(HttpResponse::NotFound().json(json!({"error": "Driver not found"}))),
    }
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/device-ids",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of device IDs"))
)]
pub async fn get_device_ids(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let ids = app_state.operator_service.get_device_ids(&gtfs_id).await?;
    Ok(HttpResponse::Ok().json(ids))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/tablet-ids",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    responses((status = 200, description = "List of tablet IDs"))
)]
pub async fn get_tablet_ids(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;
    let ids = app_state.operator_service.get_tablet_ids(&gtfs_id).await?;
    Ok(HttpResponse::Ok().json(ids))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/operators",
    tag = "Internal Operator",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("role" = String, Query, description = "Role: 'drivers' or 'conductors'"),
    ),
    responses((status = 200, description = "List of operators"))
)]
pub async fn get_operators(
    app_state: Data<AppState>,
    path: Path<String>,
    query: Query<RoleQuery>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;

    let role = query.role.as_str();
    if role != "drivers" && role != "conductors" {
        return Err(AppError::BadRequest(
            "role must be 'drivers' or 'conductors'".to_string(),
        ));
    }

    let list = app_state
        .operator_service
        .get_operators(&gtfs_id, role)
        .await?;
    Ok(HttpResponse::Ok().json(list))
}

#[utoipa::path(
    post,
    path = "/internal/operator/{gtfs_id}/waybill/status",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    request_body = UpdateWaybillStatusBody,
    responses((status = 200, description = "Waybill status updated"))
)]
pub async fn update_waybill_status(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<UpdateWaybillStatusBody>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;

    let rows = app_state
        .operator_service
        .update_waybill_status(&gtfs_id, body.waybill_id.clone(), &body.status)
        .await?;

    Ok(HttpResponse::Ok().json(json!({
        "message": "waybill status updated",
        "rows_affected": rows,
        "valid_statuses": waybill_statuses()
    })))
}

#[utoipa::path(
    post,
    path = "/internal/operator/{gtfs_id}/waybill/fleet",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    request_body = UpdateWaybillFleetBody,
    responses((status = 200, description = "Waybill fleet number updated"))
)]
pub async fn update_waybill_fleet(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<UpdateWaybillFleetBody>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;

    let rows = app_state
        .operator_service
        .update_waybill_fleet_number(&gtfs_id, body.waybill_id.clone(), &body.fleet_no)
        .await?;

    Ok(HttpResponse::Ok().json(json!({
        "message": "waybill fleet number updated",
        "rows_affected": rows
    })))
}

#[utoipa::path(
    post,
    path = "/internal/operator/{gtfs_id}/waybill/tablet",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    request_body = UpdateWaybillTabletBody,
    responses((status = 200, description = "Waybill tablet ID updated"))
)]
pub async fn update_waybill_tablet(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<UpdateWaybillTabletBody>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;

    let rows = app_state
        .operator_service
        .update_waybill_tablet_id(&gtfs_id, body.waybill_id.clone(), &body.tablet_id)
        .await?;

    Ok(HttpResponse::Ok().json(json!({
        "message": "waybill tablet id updated",
        "rows_affected": rows
    })))
}

#[utoipa::path(
    get,
    path = "/internal/operator/{gtfs_id}/waybills",
    tag = "Internal Operator",
    params(
        ("gtfs_id" = String, Path, description = "GTFS feed identifier"),
        ("limit" = Option<i64>, Query, description = "Limit (default 15)"),
        ("offset" = Option<i64>, Query, description = "Offset (default 0)"),
    ),
    responses((status = 200, description = "List of waybills"))
)]
pub async fn get_waybills(
    app_state: Data<AppState>,
    path: Path<String>,
    query: Query<PaginationQuery>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    check_gtfs_id(&gtfs_id)?;

    let limit = query.limit.unwrap_or(15);
    let offset = query.offset.unwrap_or(0);

    if limit < 0 || offset < 0 || limit > 1000 || offset > 1000 {
        return Ok(HttpResponse::BadRequest().json(json!({
            "error": "limit and offset must be non-negative and less than 1000"
        })));
    }

    let rows = app_state
        .operator_service
        .get_waybills(&gtfs_id, limit, offset)
        .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    get,
    path = "/routes-served-today",
    tag = "Routes",
    responses((status = 200, description = "Routes served today"))
)]
pub async fn get_routes_served_today(app_state: Data<AppState>) -> AppResult<HttpResponse> {
    let routes = app_state
        .db_vehicle_reader
        .get_routes_served_today()
        .await?;
    Ok(HttpResponse::Ok().json(routes))
}

#[utoipa::path(
    post,
    path = "/internal/operator/{gtfs_id}/station-eta/upsert",
    tag = "Internal Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    request_body = StationEtaUpsertRequest,
    responses((status = 200, description = "Station ETA upserted"))
)]
pub async fn upsert_station_eta(
    app_state: Data<AppState>,
    path: Path<String>,
    req: web::Json<StationEtaUpsertRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let body = req.into_inner();

    app_state
        .db_vehicle_reader_internal
        .upsert_station_eta(
            &gtfs_id,
            &body.source_station_code,
            &body.destination_station_code,
            body.eta_in_seconds,
        )
        .await?;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "status": "success",
        "message": "Station ETA successfully upserted.",
    })))
}

// ─── Fleet operator ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct FleetAnchorRequest {
    pub conductor_token: Option<String>,
    pub driver_token: Option<String>,
    pub vehicle_number: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FleetTripActionRequest {
    pub action: String,
    pub trip_number: Option<i32>,
    pub timestamp: Option<i64>,
    pub conductor_token: Option<String>,
    pub driver_token: Option<String>,
    pub vehicle_number: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FleetCurrentTripDetailsRequest {
    pub previous_trip_number: i32,
    pub conductor_token: Option<String>,
    pub driver_token: Option<String>,
    pub vehicle_number: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct FleetVerifyRequest {
    pub operator_badge_token: String,
    pub device_serial_number: String,
}

fn parse_fleet_anchor(
    conductor_token: Option<String>,
    driver_token: Option<String>,
    vehicle_number: Option<String>,
) -> AppResult<WaybillAnchor> {
    let anchors_provided = conductor_token.is_some() as u8
        + driver_token.is_some() as u8
        + vehicle_number.is_some() as u8;

    if anchors_provided != 1 {
        return Err(AppError::BadRequest(
            "Exactly one of conductor_token, driver_token, or vehicle_number must be provided."
                .to_string(),
        ));
    }

    if let Some(token) = conductor_token {
        return Ok(WaybillAnchor::ConductorToken(token));
    }
    if let Some(token) = driver_token {
        return Ok(WaybillAnchor::DriverToken(token));
    }
    Ok(WaybillAnchor::VehicleNumber(vehicle_number.unwrap()))
}

#[utoipa::path(
    post,
    path = "/internal/fleet-operator/{gtfs_id}/currentOperation",
    tag = "Internal Fleet Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    request_body = FleetAnchorRequest,
    responses((status = 200, description = "Current fleet operation"))
)]
pub async fn fleet_operator_current_operation(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<FleetAnchorRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();
    let anchor = parse_fleet_anchor(req.conductor_token, req.driver_token, req.vehicle_number)?;
    let response = app_state
        .fleet_operator_service
        .current_operation(&gtfs_id, anchor)
        .await?;
    Ok(HttpResponse::Ok().json(response))
}

#[utoipa::path(
    post,
    path = "/internal/fleet-operator/{gtfs_id}/tripAction",
    tag = "Internal Fleet Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    request_body = FleetTripActionRequest,
    responses((status = 200, description = "Trip action result"))
)]
pub async fn fleet_operator_trip_action(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<FleetTripActionRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();

    let action = match req.action.as_str() {
        "start" => TripAction::Start,
        "end" => TripAction::End,
        "reset" => TripAction::Reset,
        "rollback" => TripAction::Rollback,
        other => {
            return Err(AppError::BadRequest(format!(
                "Invalid action '{}'. Must be 'start', 'end', 'reset', or 'rollback'.",
                other
            )))
        }
    };

    let trip_number = match action {
        TripAction::Reset => 0,
        _ => req.trip_number.ok_or_else(|| {
            AppError::BadRequest(
                "trip_number is required for 'start', 'end', and 'rollback' actions.".to_string(),
            )
        })?,
    };

    let anchor = parse_fleet_anchor(req.conductor_token, req.driver_token, req.vehicle_number)?;
    let response = app_state
        .fleet_operator_service
        .trip_action(&gtfs_id, anchor, action, trip_number, req.timestamp)
        .await?;
    Ok(HttpResponse::Ok().json(response))
}

#[utoipa::path(
    post,
    path = "/internal/fleet-operator/{gtfs_id}/currentTripDetails",
    tag = "Internal Fleet Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    request_body = FleetCurrentTripDetailsRequest,
    responses((status = 200, description = "Current trip details"))
)]
pub async fn fleet_operator_current_trip_details(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<FleetCurrentTripDetailsRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();
    let anchor = parse_fleet_anchor(req.conductor_token, req.driver_token, req.vehicle_number)?;
    let response = app_state
        .fleet_operator_service
        .current_trip_details(&gtfs_id, anchor, req.previous_trip_number)
        .await?;
    Ok(HttpResponse::Ok().json(response))
}

#[utoipa::path(
    post,
    path = "/internal/fleet-operator/{gtfs_id}/verify",
    tag = "Internal Fleet Operator",
    params(("gtfs_id" = String, Path, description = "GTFS feed identifier")),
    request_body = FleetVerifyRequest,
    responses((status = 200, description = "Fleet operator verification result"))
)]
pub async fn fleet_operator_verify(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<FleetVerifyRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();

    let response = app_state
        .fleet_operator_service
        .verify_without_device_serial_number(
            &gtfs_id,
            &req.operator_badge_token,
            &req.device_serial_number,
        )
        .await?;
    Ok(HttpResponse::Ok().json(response))
}

#[utoipa::path(
    post,
    path = "/internal/fleet-operator/{gtfs_id}/employee/login",
    tag = "Fleet Operator",
    params(("gtfs_id" = String, Path, description = "GTFS dataset identifier")),
    request_body = EmployeeLoginRequest,
    responses((status = 200, description = "Login response", body = EmployeeLoginResponse))
)]
pub async fn fleet_operator_employee_login(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<EmployeeLoginRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();

    let response = app_state
        .fleet_operator_service
        .login(&gtfs_id, &req)
        .await?;

    Ok(HttpResponse::Ok().json(response))
}

#[utoipa::path(
    post,
    path = "/internal/fleet-operator/{gtfs_id}/employee/register",
    tag = "Fleet Operator",
    params(("gtfs_id" = String, Path, description = "GTFS dataset identifier")),
    request_body = EmployeeRegisterRequest,
    responses((status = 200, description = "Registration response", body = EmployeeRegisterResponse))
)]
pub async fn fleet_operator_employee_register(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<EmployeeRegisterRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();

    let response = app_state
        .fleet_operator_service
        .register(&gtfs_id, &req)
        .await?;

    Ok(HttpResponse::Ok().json(response))
}

// ── Metro routing handlers ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MetroRoutePlanQuery {
    pub from: String,
    pub to: String,
    /// Optional departure time in "HH:MM:SS" format
    pub departure_time: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MetroNearbyStopsQuery {
    pub lat: f64,
    pub lon: f64,
    /// Search radius in meters (default: 1000)
    pub radius_m: Option<f64>,
}

/// GET /metro/route-plan/{gtfs_id}?from={stop_id}&to={stop_id}&departure_time={HH:MM:SS}
///
/// Finds the shortest path between two metro stops using A*.
async fn metro_route_plan(
    app_state: Data<AppState>,
    path: Path<String>,
    query: Query<MetroRoutePlanQuery>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let params = query.into_inner();

    let graph = app_state
        .metro_graphs
        .get(&gtfs_id)
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "No metro graph found for gtfs_id: {}. Available: {:?}",
                gtfs_id,
                app_state.metro_graphs.keys().collect::<Vec<_>>()
            ))
        })?;

    let departure_time = params
        .departure_time
        .as_deref()
        .and_then(crate::services::metro_graph::parse_time_str);

    let result = crate::services::astar_router::find_shortest_path(
        graph,
        &params.from,
        &params.to,
        departure_time,
    )?;

    Ok(HttpResponse::Ok().json(result))
}

/// GET /metro/nearby-stops/{gtfs_id}?lat={}&lon={}&radius_m={}
///
/// Find metro stops within a given radius of a lat/lon point.
async fn metro_nearby_stops(
    app_state: Data<AppState>,
    path: Path<String>,
    query: Query<MetroNearbyStopsQuery>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let params = query.into_inner();
    let radius = params.radius_m.unwrap_or(1000.0);

    let graph = app_state
        .metro_graphs
        .get(&gtfs_id)
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "No metro graph found for gtfs_id: {}",
                gtfs_id
            ))
        })?;

    let nearby = graph.find_nearby_stops(params.lat, params.lon, radius);

    let result: Vec<serde_json::Value> = nearby
        .iter()
        .map(|node| {
            let dist = crate::services::metro_graph::haversine_distance_meters(
                params.lat,
                params.lon,
                node.lat,
                node.lon,
            );
            serde_json::json!({
                "stopId": node.stop_id,
                "stopName": node.stop_name,
                "lat": node.lat,
                "lon": node.lon,
                "distanceMeters": (dist * 10.0).round() / 10.0,
                "parentStation": node.parent_station,
            })
        })
        .collect();

    Ok(HttpResponse::Ok().json(result))
}

/// GET /metro/graph-info/{gtfs_id}
///
/// Returns metadata about the metro graph (number of nodes, edges, routes).
async fn metro_graph_info(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();

    let graph = app_state
        .metro_graphs
        .get(&gtfs_id)
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "No metro graph found for gtfs_id: {}",
                gtfs_id
            ))
        })?;

    let total_edges: usize = graph.adjacency.values().map(|v| v.len()).sum();
    let transfer_edges: usize = graph
        .adjacency
        .values()
        .flat_map(|v| v.iter())
        .filter(|e| e.is_transfer)
        .count();

    let route_info: Vec<serde_json::Value> = graph
        .route_names
        .iter()
        .map(|(id, name)| {
            let stop_count = graph
                .route_stop_sequences
                .get(id)
                .map(|s| s.len())
                .unwrap_or(0);
            serde_json::json!({
                "routeId": id,
                "routeName": name,
                "stopCount": stop_count,
            })
        })
        .collect();

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "gtfsId": gtfs_id,
        "totalNodes": graph.nodes.len(),
        "totalEdges": total_edges,
        "transferEdges": transfer_edges,
        "routeEdges": total_edges - transfer_edges,
        "totalRoutes": graph.route_names.len(),
        "routes": route_info,
        "availableStops": graph.nodes.len(),
    })))
}
