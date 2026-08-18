use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::models::IdValue;
use crate::services::field_generator;
use crate::tools::error::{AppError, AppResult};

pub fn shift_types() -> Vec<&'static str> {
    vec![
        "am",
        "pm",
        "full-shift",
        "pm-night",
        "pm-am",
        "night-halt",
        "general-shift",
    ]
}

pub fn day_types() -> Vec<&'static str> {
    vec!["weekdays", "weekend", "alldays"]
}

pub fn trip_types() -> Vec<&'static str> {
    vec!["cut-trip", "regular-trip", "dead-trip"]
}

pub fn break_types() -> Vec<&'static str> {
    vec!["no-break", "food-break", "tea-break"]
}

pub fn waybill_statuses() -> Vec<&'static str> {
    vec![
        "online",
        "upcoming",
        "new",
        "processed",
        "audited",
        "closed",
    ]
}

pub const SUPPORTED_OPERATOR_GTFS_IDS: &[&str] = &["chennai_bus", "kolkata_bus"];

/// gtfs_ids that should only use the internal reader (no external fetch)
pub const INTERNAL_ONLY_GTFS_IDS: &[&str] = &["kolkata_bus"];

/// gtfs_ids that should only use the external reader (no internal fetch)
pub const EXTERNAL_ONLY_GTFS_IDS: &[&str] = &[];

pub const MAX_QUERY_LIMIT: i64 = 1000;
pub const MAX_QUERY_FILTERS: usize = 5;

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct QueryBody {
    pub filters: Vec<Vec<String>>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub fn table_columns(table: &str) -> Option<&'static [&'static str]> {
    match table {
        "route_internal" => Some(&[
            "route_id",
            "created_at",
            "description",
            "route_direction",
            "route_group",
            "route_name",
            "route_number",
            "route_string",
            "route_type_id",
            "status",
            "updated_at",
            "via",
            "bus_service_type_id",
            "end_point_id",
            "start_point_id",
            "route_distance",
            "encoded_polyline",
            "gtfs_id",
        ]),
        "route_point_internal" => Some(&[
            "route_points_id",
            "created_at",
            "deleted",
            "fare_stage",
            "point_status",
            "route_order",
            "stage_no",
            "stage_name",
            "stop_type",
            "is_visible",
            "sub_stage",
            "travel_distance",
            "travel_time",
            "updated_at",
            "bus_stop_id",
            "route_id",
            "gtfs_id",
        ]),
        "bus_schedule_internal" => Some(&[
            "schedule_id",
            "created_at",
            "deleted",
            "effective_from",
            "effective_till",
            "route_code",
            "schedule_number",
            "service_code",
            "service_type_code",
            "schedule_type_code",
            "status",
            "updated_at",
            "entity_id",
            "route_id",
            "service_type_id",
            "schedule_type_id",
            "gtfs_id",
        ]),
        "bus_schedule_trip_internal" => Some(&[
            "schedule_trip_id",
            "created_at",
            "deleted",
            "effective_end_date",
            "effective_start_date",
            "no_trip",
            "schedule_number_name",
            "start_time",
            "status",
            "updated_at",
            "calendar_id",
            "schedule_id",
            "gtfs_id",
        ]),
        "bus_schedule_trip_detail_internal" => Some(&[
            "schedule_trip_detail_id",
            "break_time",
            "break_type",
            "created_at",
            "deleted",
            "distance",
            "end_time",
            "org_name",
            "running_time",
            "schedule_number",
            "shift_day_name",
            "shift_type_name",
            "start_time",
            "trip_number",
            "trip_order",
            "trip_type",
            "updated_at",
            "calendar_id",
            "route_number_id",
            "schedule_trip_id",
            "is_active_trip",
            "trip_end_time",
            "trip_start_time",
            "sync_end_time",
            "sync_start_time",
            "status",
            "gtfs_id",
        ]),
        "bus_schedule_trip_flexi_internal" => Some(&[
            "schedule_trip_flexi_id",
            "break_time",
            "break_type",
            "created_at",
            "deleted",
            "distance",
            "end_time",
            "org_name",
            "running_time",
            "schedule_number",
            "shift_day_name",
            "shift_type_name",
            "start_time",
            "trip_number",
            "trip_order",
            "trip_type",
            "updated_at",
            "calendar_id",
            "route_number_id",
            "schedule_trip_id",
            "waybill_id",
            "is_active_trip",
            "trip_end_time",
            "trip_start_time",
            "sync_end_time",
            "sync_start_time",
            "gtfs_id",
        ]),
        "service_type_internal" => Some(&[
            "service_type_id",
            "abbreviation",
            "created_at",
            "deleted",
            "service_type_code",
            "service_type_name",
            "status",
            "ticket_footer",
            "ticket_footer_local_lang",
            "updated_at",
            "gtfs_id",
        ]),
        "stop_internal" => Some(&[
            "bus_stop_id",
            "bus_stop_code",
            "bus_stop_name",
            "bus_stop_name_local_lang",
            "created_at",
            "deleted",
            "description",
            "fare_stage",
            "landmark",
            "latitude_current",
            "longitude_current",
            "route_status",
            "status",
            "source",
            "stop_direction",
            "stop_group_id",
            "stop_type_id",
            "sub_stage",
            "toll_fee",
            "toll_zone",
            "updated_at",
            "gtfs_id",
        ]),
        "designations_internal" => Some(&[
            "designation_id",
            "created_at",
            "deleted",
            "designation_name",
            "designation_remark",
            "designation_status",
            "is_default",
            "updated_at",
            "gtfs_id",
        ]),
        "employees_internal" => Some(&[
            "emp_id",
            "address",
            "basic_amount",
            "created_at",
            "da_amount",
            "dob",
            "deleted",
            "driving_license_expiry",
            "driving_license_number",
            "email",
            "father_name",
            "first_name",
            "gender",
            "last_name",
            "mobile_no",
            "status",
            "token_no",
            "updated_at",
            "week_off",
            "department_id",
            "designation_id",
            "entity_id",
            "organization_id",
            "gtfs_id",
        ]),
        "entities_internal" => Some(&[
            "entity_id",
            "created_at",
            "deleted",
            "entity_address",
            "entity_contact",
            "entity_email",
            "entity_name",
            "entity_name_local_lang",
            "entity_remark",
            "entity_status",
            "updated_at",
            "organization_id",
            "gtfs_id",
        ]),
        "vehicles_internal" => Some(&[
            "vehicle_id",
            "created_at",
            "deleted",
            "fleet_no",
            "status",
            "updated_at",
            "vehicle_no",
            "bus_service_type_id",
            "entity_id",
            "organization_id",
            "gtfs_id",
        ]),
        "waybill_device_internal" => Some(&[
            "waybill_device_id",
            "created_at",
            "deleted",
            "device_serial_no",
            "is_audited",
            "is_primary",
            "is_uploaded",
            "updated_at",
            "waybill_id",
            "gtfs_id",
        ]),
        "fleet_etm_mapping_internal" => Some(&[
            "fleet_etm_mapping_id",
            "vehicle_no",
            "gtfs_id",
            "etm_serial_no",
            "created_at",
            "updated_at",
            "deleted",
        ]),
        "fleet_obu_mapping_internal" => Some(&[
            "fleet_obu_mapping_id",
            "vehicle_no",
            "gtfs_id",
            "obu_id",
            "created_at",
            "updated_at",
            "deleted",
        ]),
        "waybills_internal" => Some(&[
            "waybill_id",
            "audited_date",
            "bag_master",
            "challan_no",
            "conductor_name",
            "conductor_token_no",
            "created_at",
            "dc_name",
            "dc_token_no",
            "deleted",
            "driver_name",
            "driver_token_no",
            "duty_date",
            "device_serial_number",
            "is_flexi",
            "no_of_device",
            "schedule_id",
            "schedule_no",
            "schedule_trip_name",
            "schedule_type",
            "service_type",
            "schedule_start_time",
            "status",
            "updated_at",
            "vehicle_no",
            "waybill_no",
            "entity_id",
            "schedule_trip_id",
            "service_type_id",
            "shift_type_id",
            "tablet_id",
            "gtfs_id",
        ]),
        "bus_shift_type_internal" => Some(&[
            "shift_type_id",
            "shift_type_code",
            "description",
            "gtfs_id",
            "deleted",
            "created_at",
            "updated_at",
        ]),
        "bus_schedule_type_internal" => Some(&[
            "schedule_type_id",
            "schedule_type_code",
            "schedule_type_name",
            "gtfs_id",
            "deleted",
            "created_at",
            "updated_at",
        ]),
        _ => None,
    }
}

pub fn table_pk(table: &str) -> Option<&'static str> {
    match table {
        "route_internal" => Some("route_id"),
        "route_point_internal" => Some("route_points_id"),
        "bus_schedule_internal" => Some("schedule_id"),
        "bus_schedule_trip_internal" => Some("schedule_trip_id"),
        "bus_schedule_trip_detail_internal" => Some("schedule_trip_detail_id"),
        "bus_schedule_trip_flexi_internal" => Some("schedule_trip_flexi_id"),
        "service_type_internal" => Some("service_type_id"),
        "stop_internal" => Some("bus_stop_id"),
        "designations_internal" => Some("designation_id"),
        "employees_internal" => Some("emp_id"),
        "entities_internal" => Some("entity_id"),
        "vehicles_internal" => Some("vehicle_id"),
        "waybill_device_internal" => Some("waybill_device_id"),
        "fleet_etm_mapping_internal" => Some("fleet_etm_mapping_id"),
        "fleet_obu_mapping_internal" => Some("fleet_obu_mapping_id"),
        "waybills_internal" => Some("waybill_id"),
        "bus_shift_type_internal" => Some("shift_type_id"),
        "bus_schedule_type_internal" => Some("schedule_type_id"),
        _ => None,
    }
}

fn allowed_tables() -> &'static [&'static str] {
    &[
        "route_internal",
        "route_point_internal",
        "bus_schedule_internal",
        "bus_schedule_trip_internal",
        "bus_schedule_trip_detail_internal",
        "bus_schedule_trip_flexi_internal",
        "service_type_internal",
        "stop_internal",
        "designations_internal",
        "employees_internal",
        "entities_internal",
        "vehicles_internal",
        "waybill_device_internal",
        "fleet_etm_mapping_internal",
        "fleet_obu_mapping_internal",
        "waybills_internal",
        "bus_shift_type_internal",
        "bus_schedule_type_internal",
    ]
}

fn validate_column_name(col: &str) -> AppResult<()> {
    if col.is_empty() || col.len() > 64 {
        return Err(AppError::BadRequest(format!(
            "Invalid column name: {}",
            col
        )));
    }
    let mut chars = col.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(AppError::BadRequest(format!(
            "Invalid column name: {}",
            col
        )));
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' {
            return Err(AppError::BadRequest(format!(
                "Invalid column name: {}",
                col
            )));
        }
    }
    Ok(())
}

fn validate_table(table: &str) -> AppResult<()> {
    if allowed_tables().contains(&table) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("Unknown table: {}", table)))
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct ServiceTypeRow {
    pub service_type_id: IdValue,
    pub service_type_code: Option<String>,
    pub service_type_name: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct RouteRow {
    pub route_id: IdValue,
    pub route_number: Option<String>,
    pub route_name: Option<String>,
    pub route_direction: Option<String>,
    pub start_point_id: IdValue,
    pub end_point_id: IdValue,
    pub encoded_polyline: Option<String>,
}

// ===== Stop & route management (clubber / editor) =====

/// Route point stop types. String values match the `stop_type` column / downstream script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopType {
    StageStop,
    IntermediateStop,
    RouteCorrection,
}

impl StopType {
    pub fn as_str(self) -> &'static str {
        match self {
            StopType::StageStop => "STAGE STOP",
            StopType::IntermediateStop => "INTERMEDIATE STOP",
            StopType::RouteCorrection => "ROUTE CORRECTION",
        }
    }

    /// Parse a `stop_type` value; anything not recognised defaults to INTERMEDIATE STOP.
    pub fn from_opt(s: Option<&str>) -> StopType {
        match s.map(|x| x.trim().to_uppercase()).as_deref() {
            Some("STAGE STOP") => StopType::StageStop,
            Some("ROUTE CORRECTION") => StopType::RouteCorrection,
            _ => StopType::IntermediateStop,
        }
    }
}

#[derive(Debug, serde::Serialize, Clone)]
pub struct StopRouteRef {
    pub route_id: String,
    pub route_number: Option<String>,
}

/// A stop enriched with route count / passing routes / source. Used by search & nearby.
#[derive(Debug, serde::Serialize)]
pub struct EnrichedStop {
    pub stop_id: String,
    pub code: Option<String>,
    pub name: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub source: Option<String>,
    pub status: Option<String>,
    pub route_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routes: Option<Vec<StopRouteRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_m: Option<f64>,
}

/// One stop within a route, joined with stop details (route stops editor + single-route export).
#[derive(Debug, serde::Serialize)]
pub struct RouteStopDetail {
    pub route_point_id: String,
    pub bus_stop_id: String,
    pub stop_name: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub route_order: i64,
    pub stage_no: Option<i64>,
    pub stage_name: Option<String>,
    pub stop_type: Option<String>,
    pub is_visible: Option<bool>,
    pub travel_distance: Option<i64>,
    pub travel_time: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct RouteStopsResponse {
    pub route_id: String,
    pub route_number: Option<String>,
    pub route_name: Option<String>,
    pub encoded_polyline: Option<String>,
    pub stops: Vec<RouteStopDetail>,
}

#[derive(Debug, serde::Serialize)]
pub struct BulkReplaceResult {
    pub rows_affected: u64,
    pub affected_route_ids: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct ReprocessResult {
    pub route_id: String,
    pub stops_renumbered: i64,
    pub stages: i64,
    pub route_name: Option<String>,
    pub polyline: Option<String>,
}

// ----- private FromRow helpers for the SQL above -----

#[derive(sqlx::FromRow)]
struct EnrichedStopRow {
    bus_stop_id: String,
    bus_stop_code: Option<String>,
    bus_stop_name: Option<String>,
    latitude_current: f64,
    longitude_current: f64,
    source: Option<String>,
    status: Option<String>,
    route_count: i64,
    distance_m: Option<f64>,
}

#[derive(sqlx::FromRow)]
struct StopRouteJoinRow {
    bus_stop_id: String,
    route_id: String,
    route_number: Option<String>,
}

#[derive(sqlx::FromRow)]
struct RouteStopJoinRow {
    route_points_id: String,
    bus_stop_id: String,
    stop_name: Option<String>,
    lat: f64,
    lon: f64,
    route_order: i64,
    stage_no: Option<i64>,
    stage_name: Option<String>,
    stop_type: Option<String>,
    is_visible: Option<bool>,
    travel_distance: Option<i64>,
    travel_time: Option<String>,
}

#[derive(sqlx::FromRow)]
struct ReprocessPointRow {
    route_points_id: String,
    stop_type: Option<String>,
    stop_name: Option<String>,
    /// Current stored stage_name — used to detect a user override so reprocess
    /// doesn't clobber a manually-set stage name.
    existing_stage_name: Option<String>,
    lat: f64,
    lon: f64,
}

/// Attach the passing-routes list (when `with_routes`) to a set of enriched-stop rows.
async fn enrich_stops(
    pool: &PgPool,
    gtfs_id: &str,
    rows: Vec<EnrichedStopRow>,
    with_routes: bool,
) -> AppResult<Vec<EnrichedStop>> {
    let routes_map: HashMap<String, Vec<StopRouteRef>> = if with_routes && !rows.is_empty() {
        let ids: Vec<String> = rows.iter().map(|r| r.bus_stop_id.clone()).collect();
        let jr = sqlx::query_as::<_, StopRouteJoinRow>(
            "SELECT DISTINCT rp.bus_stop_id, rp.route_id, r.route_number \
             FROM route_point_internal rp \
             JOIN route_internal r ON r.route_id = rp.route_id AND r.gtfs_id = rp.gtfs_id AND r.deleted = false \
             WHERE rp.gtfs_id = $1 AND rp.deleted = false AND rp.bus_stop_id = ANY($2)",
        )
        .bind(gtfs_id)
        .bind(&ids)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::DbError(format!("enrich_stops routes: {}", e)))?;
        let mut m: HashMap<String, Vec<StopRouteRef>> = HashMap::new();
        for row in jr {
            m.entry(row.bus_stop_id).or_default().push(StopRouteRef {
                route_id: row.route_id,
                route_number: row.route_number,
            });
        }
        m
    } else {
        HashMap::new()
    };

    Ok(rows
        .into_iter()
        .map(|r| {
            let routes = if with_routes {
                Some(routes_map.get(&r.bus_stop_id).cloned().unwrap_or_default())
            } else {
                None
            };
            EnrichedStop {
                stop_id: r.bus_stop_id,
                code: r.bus_stop_code,
                name: r.bus_stop_name,
                lat: r.latitude_current,
                lon: r.longitude_current,
                source: r.source,
                status: r.status,
                route_count: r.route_count,
                routes,
                distance_m: r.distance_m,
            }
        })
        .collect())
}

/// Call OSRM `/route` with all stop coords (lat, lon) → (encoded polyline, per-leg (distance_m, duration_s)).
/// Returns None if `base` is None/empty, fewer than 2 coords, or any request/parse error.
async fn osrm_route(
    base: Option<&str>,
    coords: &[(f64, f64)],
) -> Option<(String, Vec<(f64, f64)>)> {
    let base = base.filter(|s| !s.is_empty())?;
    if coords.len() < 2 {
        return None;
    }
    let coord_str = coords
        .iter()
        .map(|(lat, lon)| format!("{},{}", lon, lat))
        .collect::<Vec<_>>()
        .join(";");
    let url = format!(
        "{}/route/v1/driving/{}?overview=full&geometries=polyline&annotations=distance,duration",
        base.trim_end_matches('/'),
        coord_str
    );
    let json: serde_json::Value = tokio::time::timeout(Duration::from_secs(5), async {
        reqwest::get(&url).await?.json::<serde_json::Value>().await
    })
    .await
    .ok()?
    .ok()?;
    let route0 = json.get("routes")?.as_array()?.first()?;
    let geometry = route0.get("geometry")?.as_str()?.to_string();
    let legs = route0
        .get("legs")?
        .as_array()?
        .iter()
        .map(|l| {
            let d = l.get("distance").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let t = l.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
            (d, t)
        })
        .collect();
    Some((geometry, legs))
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct DepotRow {
    pub entity_id: IdValue,
    pub entity_name: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct ScheduleNumberRow {
    pub schedule_id: IdValue,
    pub schedule_number: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct FleetRow {
    pub vehicle_id: IdValue,
    pub vehicle_no: Option<String>,
    pub fleet_no: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct EmployeeRow {
    pub emp_id: IdValue,
    pub first_name: String,
    pub last_name: Option<String>,
    pub token_no: Option<String>,
    pub mobile_no: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct TripDetailRow {
    pub schedule_trip_detail_id: IdValue,
    pub trip_number: i32,
    pub trip_order: i32,
    pub trip_type: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub break_time: Option<String>,
    pub break_type: Option<String>,
    pub shift_type_name: Option<String>,
    pub distance: f32,
    pub schedule_trip_id: IdValue,
    pub is_active_trip: bool,
    pub route_id: IdValue,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct RouteInternal {
    pub route_id: IdValue,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub description: Option<String>,
    pub route_direction: Option<String>,
    pub route_group: Option<String>,
    pub route_name: Option<String>,
    pub route_number: Option<String>,
    pub route_string: Option<String>,
    pub route_type_id: IdValue,
    pub status: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub via: Option<String>,
    pub bus_service_type_id: IdValue,
    pub end_point_id: IdValue,
    pub start_point_id: IdValue,
    pub route_distance: Option<f64>,
    pub encoded_polyline: Option<String>,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct RoutePointInternal {
    pub route_points_id: IdValue,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub fare_stage: Option<String>,
    pub point_status: Option<String>,
    pub route_order: i64,
    pub stage_no: Option<i64>,
    pub stage_name: Option<String>,
    pub stop_type: Option<String>,
    pub is_visible: Option<bool>,
    pub sub_stage: Option<String>,
    pub travel_distance: Option<i64>,
    pub travel_time: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub bus_stop_id: IdValue,
    pub route_id: IdValue,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct BusScheduleInternal {
    pub schedule_id: IdValue,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub effective_from: Option<chrono::DateTime<chrono::Utc>>,
    pub effective_till: Option<chrono::DateTime<chrono::Utc>>,
    pub route_code: Option<String>,
    pub schedule_number: Option<String>,
    pub service_code: Option<String>,
    pub service_type_code: Option<String>,
    pub schedule_type_code: Option<String>,
    pub status: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub entity_id: IdValue,
    pub route_id: IdValue,
    pub service_type_id: IdValue,
    pub schedule_type_id: IdValue,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct BusScheduleTripInternal {
    pub schedule_trip_id: IdValue,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub effective_end_date: Option<chrono::DateTime<chrono::Utc>>,
    pub effective_start_date: Option<chrono::DateTime<chrono::Utc>>,
    pub no_trip: i64,
    pub schedule_number_name: Option<String>,
    pub start_time: Option<String>,
    pub status: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub calendar_id: IdValue,
    pub schedule_id: IdValue,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct BusScheduleTripDetailInternal {
    pub schedule_trip_detail_id: IdValue,
    pub break_time: Option<String>,
    pub break_type: Option<String>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub distance: f64,
    pub end_time: Option<String>,
    pub org_name: Option<String>,
    pub running_time: Option<String>,
    pub schedule_number: Option<String>,
    pub shift_day_name: Option<String>,
    pub shift_type_name: Option<String>,
    pub start_time: Option<String>,
    pub trip_number: i64,
    pub trip_order: i64,
    pub trip_type: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub calendar_id: IdValue,
    pub route_number_id: IdValue,
    pub schedule_trip_id: IdValue,
    pub is_active_trip: bool,
    pub is_completed: bool,
    pub trip_end_time: Option<i64>,
    pub trip_start_time: Option<i64>,
    pub sync_end_time: Option<i64>,
    pub sync_start_time: Option<i64>,
    pub gtfs_id: String,
    pub status: Option<String>,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct BusScheduleTripFlexiInternal {
    pub schedule_trip_flexi_id: IdValue,
    pub break_time: Option<String>,
    pub break_type: Option<String>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub distance: f64,
    pub end_time: Option<String>,
    pub org_name: Option<String>,
    pub running_time: Option<String>,
    pub schedule_number: Option<String>,
    pub shift_day_name: Option<String>,
    pub shift_type_name: Option<String>,
    pub start_time: Option<String>,
    pub trip_number: i64,
    pub trip_order: i64,
    pub trip_type: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub calendar_id: IdValue,
    pub route_number_id: IdValue,
    pub schedule_trip_id: IdValue,
    pub waybill_id: IdValue,
    pub is_active_trip: bool,
    pub trip_end_time: Option<i64>,
    pub trip_start_time: Option<i64>,
    pub sync_end_time: Option<i64>,
    pub sync_start_time: Option<i64>,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct ServiceTypeInternal {
    pub service_type_id: IdValue,
    pub abbreviation: Option<String>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub service_type_code: Option<String>,
    pub service_type_name: Option<String>,
    pub status: Option<String>,
    pub ticket_footer: Option<String>,
    pub ticket_footer_local_lang: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct StopInternal {
    pub bus_stop_id: IdValue,
    pub bus_stop_code: Option<String>,
    pub bus_stop_name: Option<String>,
    pub bus_stop_name_local_lang: Option<String>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub description: Option<String>,
    pub fare_stage: Option<String>,
    pub landmark: Option<String>,
    pub latitude_current: f64,
    pub longitude_current: f64,
    pub route_status: Option<String>,
    pub status: Option<String>,
    pub source: Option<String>,
    pub stop_direction: Option<String>,
    pub stop_group_id: Option<IdValue>,
    pub stop_type_id: IdValue,
    pub sub_stage: Option<String>,
    pub toll_fee: Option<i64>,
    pub toll_zone: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct DesignationsInternal {
    pub designation_id: IdValue,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub designation_name: String,
    pub designation_remark: Option<String>,
    pub designation_status: String,
    pub is_default: i64,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct EmployeesInternal {
    pub emp_id: IdValue,
    pub address: Option<String>,
    pub basic_amount: Option<f64>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub da_amount: Option<f64>,
    pub dob: Option<chrono::NaiveDate>,
    pub deleted: bool,
    pub driving_license_expiry: Option<String>,
    pub driving_license_number: Option<String>,
    pub email: Option<String>,
    pub father_name: Option<String>,
    pub first_name: String,
    pub gender: Option<String>,
    pub last_name: Option<String>,
    pub mobile_no: Option<String>,
    pub status: Option<String>,
    pub token_no: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub week_off: Option<String>,
    pub department_id: IdValue,
    pub designation_id: IdValue,
    pub entity_id: IdValue,
    pub organization_id: IdValue,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct EntitiesInternal {
    pub entity_id: IdValue,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub entity_address: Option<String>,
    pub entity_contact: Option<String>,
    pub entity_email: Option<String>,
    pub entity_name: String,
    pub entity_name_local_lang: Option<String>,
    pub entity_remark: Option<String>,
    pub entity_status: String,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub organization_id: IdValue,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct VehiclesInternal {
    pub vehicle_id: IdValue,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub fleet_no: Option<String>,
    pub status: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub vehicle_no: Option<String>,
    pub bus_service_type_id: IdValue,
    pub entity_id: IdValue,
    pub organization_id: IdValue,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct WaybillDeviceInternal {
    pub waybill_device_id: IdValue,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub device_serial_no: Option<String>,
    pub is_audited: Option<bool>,
    pub is_primary: Option<bool>,
    pub is_uploaded: Option<bool>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub waybill_id: IdValue,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct FleetEtmMappingInternal {
    pub fleet_etm_mapping_id: IdValue,
    pub vehicle_no: String,
    pub gtfs_id: String,
    pub etm_serial_no: String,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct FleetObuMappingInternal {
    pub fleet_obu_mapping_id: IdValue,
    pub vehicle_no: String,
    pub gtfs_id: String,
    pub obu_id: String,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct WaybillsInternal {
    pub waybill_id: IdValue,
    pub audited_date: Option<chrono::DateTime<chrono::Utc>>,
    pub bag_master: Option<String>,
    pub challan_no: Option<i64>,
    pub conductor_name: Option<String>,
    pub conductor_token_no: Option<String>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub dc_name: Option<String>,
    pub dc_token_no: Option<String>,
    pub deleted: bool,
    pub driver_name: Option<String>,
    pub driver_token_no: Option<String>,
    pub duty_date: Option<String>,
    pub device_serial_number: Option<String>,
    pub is_flexi: bool,
    pub no_of_device: i64,
    pub schedule_id: IdValue,
    pub schedule_no: Option<String>,
    pub schedule_trip_name: Option<String>,
    pub schedule_type: Option<String>,
    pub service_type: Option<String>,
    pub schedule_start_time: Option<String>,
    pub status: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub vehicle_no: Option<String>,
    pub waybill_no: Option<String>,
    pub entity_id: IdValue,
    pub schedule_trip_id: IdValue,
    pub service_type_id: IdValue,
    pub shift_type_id: IdValue,
    pub tablet_id: Option<IdValue>,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct BusShiftTypeInternal {
    pub shift_type_id: IdValue,
    pub shift_type_code: Option<String>,
    pub description: Option<String>,
    pub gtfs_id: String,
    pub deleted: Option<bool>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct BusScheduleTypeInternal {
    pub schedule_type_id: IdValue,
    pub schedule_type_code: Option<String>,
    pub schedule_type_name: Option<String>,
    pub gtfs_id: String,
    pub deleted: Option<bool>,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
#[serde(untagged)]
pub enum InternalRow {
    RouteInternal(RouteInternal),
    RoutePointInternal(RoutePointInternal),
    BusScheduleInternal(BusScheduleInternal),
    BusScheduleTripInternal(BusScheduleTripInternal),
    BusScheduleTripDetailInternal(BusScheduleTripDetailInternal),
    BusScheduleTripFlexiInternal(BusScheduleTripFlexiInternal),
    ServiceTypeInternal(ServiceTypeInternal),
    StopInternal(StopInternal),
    DesignationsInternal(DesignationsInternal),
    EmployeesInternal(EmployeesInternal),
    EntitiesInternal(EntitiesInternal),
    VehiclesInternal(VehiclesInternal),
    WaybillDeviceInternal(WaybillDeviceInternal),
    FleetEtmMappingInternal(FleetEtmMappingInternal),
    FleetObuMappingInternal(FleetObuMappingInternal),
    WaybillsInternal(WaybillsInternal),
    BusShiftTypeInternal(BusShiftTypeInternal),
    BusScheduleTypeInternal(BusScheduleTypeInternal),
}

/// Deserialize a raw `row_to_json` value into the correct `InternalRow` variant
/// for the given table name.  Using `serde_json::from_value::<InternalRow>`
/// directly is wrong because the enum is `#[serde(untagged)]` and serde would
/// pick the first variant whose fields happen to match – almost always wrong.
fn row_from_value(table: &str, v: serde_json::Value) -> AppResult<InternalRow> {
    macro_rules! parse {
        ($variant:ident, $ty:ty) => {{
            serde_json::from_value::<$ty>(v)
                .map(InternalRow::$variant)
                .map_err(|e| AppError::Internal(format!("Failed to parse row: {}", e)))
        }};
    }
    match table {
        "route_internal" => parse!(RouteInternal, RouteInternal),
        "route_point_internal" => parse!(RoutePointInternal, RoutePointInternal),
        "bus_schedule_internal" => parse!(BusScheduleInternal, BusScheduleInternal),
        "bus_schedule_trip_internal" => parse!(BusScheduleTripInternal, BusScheduleTripInternal),
        "bus_schedule_trip_detail_internal" => {
            parse!(BusScheduleTripDetailInternal, BusScheduleTripDetailInternal)
        }
        "bus_schedule_trip_flexi_internal" => {
            parse!(BusScheduleTripFlexiInternal, BusScheduleTripFlexiInternal)
        }
        "service_type_internal" => parse!(ServiceTypeInternal, ServiceTypeInternal),
        "stop_internal" => parse!(StopInternal, StopInternal),
        "designations_internal" => parse!(DesignationsInternal, DesignationsInternal),
        "employees_internal" => parse!(EmployeesInternal, EmployeesInternal),
        "entities_internal" => parse!(EntitiesInternal, EntitiesInternal),
        "vehicles_internal" => parse!(VehiclesInternal, VehiclesInternal),
        "waybill_device_internal" => parse!(WaybillDeviceInternal, WaybillDeviceInternal),
        "fleet_etm_mapping_internal" => parse!(FleetEtmMappingInternal, FleetEtmMappingInternal),
        "fleet_obu_mapping_internal" => parse!(FleetObuMappingInternal, FleetObuMappingInternal),
        "waybills_internal" => parse!(WaybillsInternal, WaybillsInternal),
        "bus_shift_type_internal" => parse!(BusShiftTypeInternal, BusShiftTypeInternal),
        "bus_schedule_type_internal" => parse!(BusScheduleTypeInternal, BusScheduleTypeInternal),
        other => Err(AppError::Internal(format!("Unknown table: {}", other))),
    }
}

#[async_trait]
pub trait OperatorService: Send + Sync {
    async fn get_one_row(
        &self,
        table: &str,
        gtfs_id: &str,
        query_params: HashMap<String, String>,
    ) -> AppResult<Option<InternalRow>>;

    async fn get_all_rows(
        &self,
        table: &str,
        gtfs_id: &str,
        limit: i64,
        offset: i64,
    ) -> AppResult<Vec<InternalRow>>;

    async fn delete_one_row(&self, table: &str, gtfs_id: &str, data: Value) -> AppResult<u64>;

    async fn upsert_one_row(
        &self,
        table: &str,
        gtfs_id: &str,
        data: Value,
        to_regen: Option<Vec<String>>,
    ) -> AppResult<Value>;

    async fn get_service_types_list(&self, gtfs_id: &str) -> AppResult<Vec<ServiceTypeRow>>;
    async fn get_routes_list(&self, gtfs_id: &str) -> AppResult<Vec<RouteRow>>;
    async fn get_depot_names_and_ids(&self, gtfs_id: &str) -> AppResult<Vec<DepotRow>>;

    async fn get_schedule_numbers(&self, gtfs_id: &str) -> AppResult<Vec<ScheduleNumberRow>>;

    async fn get_schedule_trip_details_by_schedule_number(
        &self,
        gtfs_id: &str,
        schedule_number: &str,
    ) -> AppResult<Vec<TripDetailRow>>;

    async fn get_fleets(&self, gtfs_id: &str) -> AppResult<Vec<FleetRow>>;

    async fn get_conductor_data(
        &self,
        gtfs_id: &str,
        token: &str,
    ) -> AppResult<Option<EmployeeRow>>;
    async fn get_driver_info(&self, gtfs_id: &str, token: &str) -> AppResult<Option<EmployeeRow>>;

    async fn get_device_ids(&self, gtfs_id: &str) -> AppResult<Vec<String>>;
    async fn get_tablet_ids(&self, gtfs_id: &str) -> AppResult<Vec<String>>;

    async fn get_operators(&self, gtfs_id: &str, role: &str) -> AppResult<Vec<EmployeeRow>>;

    async fn update_waybill_status(
        &self,
        gtfs_id: &str,
        waybill_id: String,
        status: &str,
    ) -> AppResult<u64>;
    async fn update_waybill_status_v2(
        &self,
        gtfs_id: &str,
        waybill_id: String,
        status: &str,
        reset_trips: bool,
    ) -> AppResult<u64>;
    async fn update_waybill_fleet_number(
        &self,
        gtfs_id: &str,
        waybill_id: String,
        fleet_no: &str,
    ) -> AppResult<u64>;
    /// Granular update of a waybill's mutable operational fields (crew/fleet/devices) and optional status.
    async fn update_waybill_details(
        &self,
        gtfs_id: &str,
        body: &crate::models::UpdateWaybillDetailsBody,
    ) -> AppResult<u64>;
    async fn update_waybill_tablet_id(
        &self,
        gtfs_id: &str,
        waybill_id: String,
        tablet_id: &str,
    ) -> AppResult<u64>;

    async fn get_waybills(
        &self,
        gtfs_id: &str,
        limit: i64,
        offset: i64,
    ) -> AppResult<Vec<WaybillsInternal>>;

    async fn query_rows(
        &self,
        table: &str,
        gtfs_id: &str,
        body: QueryBody,
    ) -> AppResult<Vec<InternalRow>>;

    // ===== Stop & route management =====

    /// Fuzzy stop search by name/code (ILIKE). `with_routes` adds passing-route list.
    async fn search_stops(
        &self,
        gtfs_id: &str,
        q: &str,
        limit: i64,
        with_routes: bool,
    ) -> AppResult<Vec<EnrichedStop>>;

    /// Stops within `radius_m` of (lat, lon), ordered by distance (PostGIS).
    async fn nearby_stops(
        &self,
        gtfs_id: &str,
        lat: f64,
        lon: f64,
        radius_m: f64,
        limit: i64,
        with_routes: bool,
    ) -> AppResult<Vec<EnrichedStop>>;

    /// Club stops: repoint every route_point from `from` ids to `to`, collapsing
    /// consecutive duplicates per affected route. Returns rows changed + affected routes.
    async fn bulk_replace_stops(
        &self,
        gtfs_id: &str,
        from: &[String],
        to: &str,
    ) -> AppResult<BulkReplaceResult>;

    /// Route points joined with stop details, ordered by route_order (editor feed).
    async fn get_route_stops(&self, gtfs_id: &str, route_id: &str)
        -> AppResult<RouteStopsResponse>;

    /// Insert a stop at position `position`, shifting existing route_order up by 1.
    async fn insert_route_stop(
        &self,
        gtfs_id: &str,
        route_id: &str,
        position: i64,
        data: Value,
    ) -> AppResult<Value>;

    /// Recompute route_order/stage_no/stage_name/route_name for each route; optionally polyline.
    async fn reprocess_routes(
        &self,
        gtfs_id: &str,
        route_ids: &[String],
        recompute_polyline: bool,
    ) -> AppResult<Vec<ReprocessResult>>;

    /// Full route-stop-mapping across all routes (for CSV export).
    async fn export_route_stop_mapping(&self, gtfs_id: &str) -> AppResult<Vec<Value>>;

    /// Returns the underlying pool for streaming responses. None for mock implementations.
    fn pool(&self) -> Option<&PgPool> {
        None
    }
}

pub struct MockOperatorService;

impl Default for MockOperatorService {
    fn default() -> Self {
        Self::new()
    }
}

impl MockOperatorService {
    pub fn new() -> Self {
        Self
    }
}

macro_rules! mock_err {
    () => {
        Err(AppError::NotFound(
            "Database is not connected in local testing mode.".to_string(),
        ))
    };
}

#[async_trait]
impl OperatorService for MockOperatorService {
    async fn get_one_row(
        &self,
        _table: &str,
        _gtfs_id: &str,
        _query_params: HashMap<String, String>,
    ) -> AppResult<Option<InternalRow>> {
        mock_err!()
    }

    async fn get_all_rows(
        &self,
        _table: &str,
        _gtfs_id: &str,
        _limit: i64,
        _offset: i64,
    ) -> AppResult<Vec<InternalRow>> {
        mock_err!()
    }

    async fn delete_one_row(&self, _table: &str, _gtfs_id: &str, _data: Value) -> AppResult<u64> {
        mock_err!()
    }

    async fn upsert_one_row(
        &self,
        _table: &str,
        _gtfs_id: &str,
        _data: Value,
        _to_regen: Option<Vec<String>>,
    ) -> AppResult<Value> {
        mock_err!()
    }

    async fn get_service_types_list(&self, _gtfs_id: &str) -> AppResult<Vec<ServiceTypeRow>> {
        mock_err!()
    }

    async fn get_routes_list(&self, _gtfs_id: &str) -> AppResult<Vec<RouteRow>> {
        mock_err!()
    }

    async fn get_depot_names_and_ids(&self, _gtfs_id: &str) -> AppResult<Vec<DepotRow>> {
        mock_err!()
    }

    async fn get_schedule_numbers(&self, _gtfs_id: &str) -> AppResult<Vec<ScheduleNumberRow>> {
        mock_err!()
    }

    async fn get_schedule_trip_details_by_schedule_number(
        &self,
        _gtfs_id: &str,
        _schedule_number: &str,
    ) -> AppResult<Vec<TripDetailRow>> {
        mock_err!()
    }

    async fn get_fleets(&self, _gtfs_id: &str) -> AppResult<Vec<FleetRow>> {
        mock_err!()
    }

    async fn get_conductor_data(
        &self,
        _gtfs_id: &str,
        _token: &str,
    ) -> AppResult<Option<EmployeeRow>> {
        mock_err!()
    }

    async fn get_driver_info(
        &self,
        _gtfs_id: &str,
        _token: &str,
    ) -> AppResult<Option<EmployeeRow>> {
        mock_err!()
    }

    async fn get_device_ids(&self, _gtfs_id: &str) -> AppResult<Vec<String>> {
        mock_err!()
    }

    async fn get_tablet_ids(&self, _gtfs_id: &str) -> AppResult<Vec<String>> {
        mock_err!()
    }

    async fn get_operators(&self, _gtfs_id: &str, _role: &str) -> AppResult<Vec<EmployeeRow>> {
        mock_err!()
    }

    async fn update_waybill_status(
        &self,
        _gtfs_id: &str,
        _waybill_id: String,
        _status: &str,
    ) -> AppResult<u64> {
        mock_err!()
    }

    async fn update_waybill_status_v2(
        &self,
        _gtfs_id: &str,
        _waybill_id: String,
        _status: &str,
        _reset_trips: bool,
    ) -> AppResult<u64> {
        mock_err!()
    }

    async fn update_waybill_fleet_number(
        &self,
        _gtfs_id: &str,
        _waybill_id: String,
        _fleet_no: &str,
    ) -> AppResult<u64> {
        mock_err!()
    }

    async fn update_waybill_details(
        &self,
        _gtfs_id: &str,
        _body: &crate::models::UpdateWaybillDetailsBody,
    ) -> AppResult<u64> {
        mock_err!()
    }

    async fn update_waybill_tablet_id(
        &self,
        _gtfs_id: &str,
        _waybill_id: String,
        _tablet_id: &str,
    ) -> AppResult<u64> {
        mock_err!()
    }

    async fn get_waybills(
        &self,
        _gtfs_id: &str,
        _limit: i64,
        _offset: i64,
    ) -> AppResult<Vec<WaybillsInternal>> {
        mock_err!()
    }

    async fn query_rows(
        &self,
        _table: &str,
        _gtfs_id: &str,
        _body: QueryBody,
    ) -> AppResult<Vec<InternalRow>> {
        mock_err!()
    }

    async fn search_stops(
        &self,
        _gtfs_id: &str,
        _q: &str,
        _limit: i64,
        _with_routes: bool,
    ) -> AppResult<Vec<EnrichedStop>> {
        mock_err!()
    }

    async fn nearby_stops(
        &self,
        _gtfs_id: &str,
        _lat: f64,
        _lon: f64,
        _radius_m: f64,
        _limit: i64,
        _with_routes: bool,
    ) -> AppResult<Vec<EnrichedStop>> {
        mock_err!()
    }

    async fn bulk_replace_stops(
        &self,
        _gtfs_id: &str,
        _from: &[String],
        _to: &str,
    ) -> AppResult<BulkReplaceResult> {
        mock_err!()
    }

    async fn get_route_stops(
        &self,
        _gtfs_id: &str,
        _route_id: &str,
    ) -> AppResult<RouteStopsResponse> {
        mock_err!()
    }

    async fn insert_route_stop(
        &self,
        _gtfs_id: &str,
        _route_id: &str,
        _position: i64,
        _data: Value,
    ) -> AppResult<Value> {
        mock_err!()
    }

    async fn reprocess_routes(
        &self,
        _gtfs_id: &str,
        _route_ids: &[String],
        _recompute_polyline: bool,
    ) -> AppResult<Vec<ReprocessResult>> {
        mock_err!()
    }

    async fn export_route_stop_mapping(&self, _gtfs_id: &str) -> AppResult<Vec<Value>> {
        mock_err!()
    }
}

struct DeviceIdsCache {
    etm_ids: HashMap<String, (Vec<String>, SystemTime)>,
}

struct TabletIdsCache {
    tablet_ids: HashMap<String, (Vec<String>, SystemTime)>,
}

// Designation cache: gtfs_id → (name(lowercase) → id, loaded_at)
struct DesignationCache {
    by_gtfs: HashMap<String, (HashMap<String, String>, SystemTime)>,
}

const DEVICE_CACHE_SECS: u64 = 3600; // 1 hour
const DESIGNATION_CACHE_SECS: u64 = 43200; // 12 hours

pub struct DBOperatorService {
    pool: PgPool,
    osrm_url: Option<String>,
    /// When true, auto-injected PKs are numeric (i64) instead of UUID — for DBs
    /// whose id columns are still `bigint` (pre bigint->text migration).
    gen_int_for_id: bool,
    device_ids_cache: Arc<RwLock<DeviceIdsCache>>,
    tablet_ids_cache: Arc<RwLock<TabletIdsCache>>,
    designation_cache: Arc<RwLock<DesignationCache>>,
}

impl DBOperatorService {
    pub fn new(pool: PgPool, osrm_url: Option<String>, gen_int_for_id: bool) -> Self {
        Self {
            pool,
            osrm_url: osrm_url.filter(|s| !s.is_empty()),
            gen_int_for_id,
            device_ids_cache: Arc::new(RwLock::new(DeviceIdsCache {
                etm_ids: HashMap::new(),
            })),
            tablet_ids_cache: Arc::new(RwLock::new(TabletIdsCache {
                tablet_ids: HashMap::new(),
            })),
            designation_cache: Arc::new(RwLock::new(DesignationCache {
                by_gtfs: HashMap::new(),
            })),
        }
    }

    fn cache_expired(ts: SystemTime, secs: u64) -> bool {
        ts.elapsed().unwrap_or_default() >= Duration::from_secs(secs)
    }

    /// Load the designation name→id map for a gtfs_id if missing or expired (12h TTL).
    async fn ensure_designation_cache(&self, gtfs_id: &str) -> AppResult<()> {
        {
            let cache = self.designation_cache.read().await;
            if let Some((_, ts)) = cache.by_gtfs.get(gtfs_id) {
                if !Self::cache_expired(*ts, DESIGNATION_CACHE_SECS) {
                    return Ok(());
                }
            }
        }

        info!("Loading designation cache from DB for gtfs_id={}", gtfs_id);
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT designation_id::text, LOWER(designation_name) FROM designations_internal WHERE deleted = false AND gtfs_id = $1",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("designation cache load: {}", e)))?;

        let map: HashMap<String, String> = rows.into_iter().map(|(id, name)| (name, id)).collect();
        info!(
            "Designation cache loaded with {} entries for gtfs_id={}",
            map.len(),
            gtfs_id
        );

        let mut cache = self.designation_cache.write().await;
        cache
            .by_gtfs
            .insert(gtfs_id.to_string(), (map, SystemTime::now()));
        Ok(())
    }

    async fn designation_id_for(&self, gtfs_id: &str, role: &str) -> AppResult<String> {
        self.ensure_designation_cache(gtfs_id).await?;
        let cache = self.designation_cache.read().await;
        let map = cache
            .by_gtfs
            .get(gtfs_id)
            .map(|(m, _)| m)
            .expect("cache populated by ensure_designation_cache");
        // role could be "conductors" or "drivers"; strip trailing 's' for matching
        let search = role.trim_end_matches('s').to_lowercase();
        // Find partial match (e.g. "conductor" matches "conductor", "driver" matches "driver")
        map.iter()
            .find(|(name, _)| name.contains(&search))
            .map(|(_, id)| id.clone())
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "No designation found for role '{}' in gtfs_id '{}'",
                    role, gtfs_id
                ))
            })
    }
}

/// Resolve (schedule_number, org_name, shift_type_name) for a schedule_trip_id by walking
/// schedule_trip → schedule → entity and schedule → schedule_type. Used to enrich
/// trip-detail inserts so those columns are never left null. shift_type_name comes from
/// bus_schedule_type_internal.schedule_type_name (the source of truth) — no hardcoded map.
async fn fetch_schedule_meta_for_trip(
    pool: &PgPool,
    schedule_trip_id: &str,
) -> AppResult<Option<(Option<String>, Option<String>, Option<String>)>> {
    sqlx::query_as(
        "SELECT s.schedule_number, e.entity_name, st.schedule_type_name \
         FROM bus_schedule_trip_internal t \
         JOIN bus_schedule_internal s ON s.schedule_id = t.schedule_id \
         LEFT JOIN entities_internal e ON e.entity_id = s.entity_id AND e.gtfs_id = s.gtfs_id AND e.deleted = false \
         LEFT JOIN bus_schedule_type_internal st ON st.schedule_type_id = s.schedule_type_id \
         WHERE t.schedule_trip_id::text = $1 LIMIT 1",
    )
    .bind(schedule_trip_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::DbError(format!("fetch_schedule_meta_for_trip: {}", e)))
}

#[async_trait]
impl OperatorService for DBOperatorService {
    fn pool(&self) -> Option<&PgPool> {
        Some(&self.pool)
    }

    async fn get_one_row(
        &self,
        table: &str,
        gtfs_id: &str,
        query_params: HashMap<String, String>,
    ) -> AppResult<Option<InternalRow>> {
        validate_table(table)?;

        if query_params.is_empty() {
            return Err(AppError::BadRequest(
                "At least one query param required".to_string(),
            ));
        }

        for col in query_params.keys() {
            validate_column_name(col)?;
        }

        // Build WHERE clause dynamically; gtfs_id is always $1
        let cols: Vec<&str> = query_params.keys().map(|s| s.as_str()).collect();
        let vals: Vec<&str> = query_params.values().map(|s| s.as_str()).collect();

        // `::text` cast so equality works whether the column is `bigint` or `text`
        // (filter values arrive as strings) — same as query_rows.
        let extra_clause: String = cols
            .iter()
            .enumerate()
            .map(|(i, col)| format!("{}::text = ${}", col, i + 2))
            .collect::<Vec<_>>()
            .join(" AND ");

        let sql = format!(
            "SELECT row_to_json(t) FROM (SELECT * FROM public.{} WHERE gtfs_id = $1 AND {} AND deleted = false LIMIT 1) t",
            table, extra_clause
        );

        let mut q = sqlx::query_scalar::<_, Value>(&sql);
        q = q.bind(gtfs_id);
        for val in &vals {
            q = q.bind(val);
        }

        let val: Option<Value> = q
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AppError::DbError(format!("get_one_row {}: {}", table, e)))?;

        match val {
            Some(v) => Ok(Some(row_from_value(table, v)?)),
            None => Ok(None),
        }
    }

    async fn get_all_rows(
        &self,
        table: &str,
        gtfs_id: &str,
        limit: i64,
        offset: i64,
    ) -> AppResult<Vec<InternalRow>> {
        validate_table(table)?;
        // PKs are text UUIDs now, so `ORDER BY 1` is no longer newest-first; order by
        // created_at with the PK as a unique tiebreaker for stable LIMIT/OFFSET
        // pagination. Served by (gtfs_id, created_at DESC, pk DESC) partial indexes.
        let pk = table_pk(table)
            .ok_or_else(|| AppError::BadRequest(format!("No PK mapping for table: {}", table)))?;

        let sql = format!(
            "SELECT row_to_json(t) FROM (SELECT * FROM public.{} WHERE gtfs_id = $1 AND deleted = false ORDER BY created_at DESC, {} DESC LIMIT $2 OFFSET $3) t",
            table, pk
        );

        let vals: Vec<Value> = sqlx::query_scalar::<_, Value>(&sql)
            .bind(gtfs_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::DbError(format!("get_all_rows {}: {}", table, e)))?;

        vals.into_iter().map(|v| row_from_value(table, v)).collect()
    }

    async fn query_rows(
        &self,
        table: &str,
        gtfs_id: &str,
        body: QueryBody,
    ) -> AppResult<Vec<InternalRow>> {
        validate_table(table)?;

        if body.filters.len() > MAX_QUERY_FILTERS {
            return Err(AppError::BadRequest(format!(
                "Too many filters: max {} allowed",
                MAX_QUERY_FILTERS
            )));
        }

        let known_cols: std::collections::HashSet<&str> = table_columns(table)
            .unwrap_or(&[])
            .iter()
            .copied()
            .collect();

        // Validated filters: (col, sql_op, val1)
        struct Filter {
            col: String,
            sql_op: &'static str,
            val1: String,
        }

        let mut valid_filters: Vec<Filter> = Vec::new();

        for f in &body.filters {
            if f.len() < 3 {
                return Err(AppError::BadRequest(
                    "Each filter must have at least 3 elements: [column, operator, value]"
                        .to_string(),
                ));
            }

            let col = &f[0];
            let op = f[1].as_str();

            let sql_op: &'static str = match op {
                "eq" => "=",
                "noteq" => "!=",
                "gt" => ">",
                "lt" => "<",
                "like" => "ILIKE",
                _ => {
                    return Err(AppError::BadRequest(format!(
                        "Unknown operator '{}'. Valid: eq, noteq, gt, lt, like",
                        op
                    )))
                }
            };

            // Automatically wrap the value with % wildcards for ILIKE
            let val1 = if op == "like" {
                format!("%{}%", f[2])
            } else {
                f[2].clone()
            };

            validate_column_name(col)?;

            if !known_cols.contains(col.as_str()) {
                return Err(AppError::BadRequest(format!(
                    "Unknown column '{}' for table '{}'",
                    col, table
                )));
            }

            valid_filters.push(Filter {
                col: col.clone(),
                sql_op,
                val1,
            });
        }

        if valid_filters.is_empty() {
            return Err(AppError::BadRequest(
                "At least one filter is required".to_string(),
            ));
        }

        // Build WHERE clause; gtfs_id = $1, filter params start at $2
        let mut param_idx: usize = 2;
        let mut where_parts: Vec<String> = Vec::new();
        let mut bind_vals: Vec<String> = Vec::new();

        for f in &valid_filters {
            where_parts.push(format!("{}::text {} ${}", f.col, f.sql_op, param_idx));
            bind_vals.push(f.val1.clone());
            param_idx += 1;
        }

        let limit = body.limit.unwrap_or(15).min(MAX_QUERY_LIMIT);
        let offset = body.offset.unwrap_or(0);

        // Same ordering rationale as get_all_rows: created_at + PK tiebreaker
        // instead of `ORDER BY 1` on a text-UUID primary key.
        let pk = table_pk(table)
            .ok_or_else(|| AppError::BadRequest(format!("No PK mapping for table: {}", table)))?;

        let sql = format!(
            "SELECT row_to_json(t) FROM (SELECT * FROM public.{} WHERE gtfs_id = $1 AND deleted = false AND {} ORDER BY created_at DESC, {} DESC LIMIT ${} OFFSET ${}) t",
            table,
            where_parts.join(" AND "),
            pk,
            param_idx,
            param_idx + 1,
        );

        let mut q = sqlx::query_scalar::<_, Value>(&sql);
        q = q.bind(gtfs_id);
        for v in &bind_vals {
            q = q.bind(v.as_str());
        }
        q = q.bind(limit).bind(offset);

        let vals: Vec<Value> = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::DbError(format!("query_rows {}: {}", table, e)))?;

        vals.into_iter().map(|v| row_from_value(table, v)).collect()
    }

    async fn delete_one_row(&self, table: &str, gtfs_id: &str, data: Value) -> AppResult<u64> {
        validate_table(table)?;
        let pk = table_pk(table)
            .ok_or_else(|| AppError::Internal(format!("No PK for table: {}", table)))?;

        let obj = data
            .as_object()
            .ok_or_else(|| AppError::BadRequest("Body must be a JSON object".to_string()))?;

        let pk_value = obj.get(pk).ok_or_else(|| {
            AppError::BadRequest(format!("Body must contain the primary key: {}", pk))
        })?;

        let id = match pk_value {
            Value::Number(n) => n
                .as_i64()
                .map(|i| i.to_string())
                .or_else(|| n.as_f64().map(|f| f.to_string()))
                .ok_or_else(|| AppError::BadRequest("ID must be a number".to_string()))?,
            Value::String(s) => s.clone(),
            _ => {
                return Err(AppError::BadRequest(
                    "ID must be a string or number".to_string(),
                ))
            }
        };

        // `::text` cast so the id predicate works whether the column is still
        // `bigint` (prod) or already migrated to `text` — bind the string either way.
        let sql = format!(
            "UPDATE public.{} SET deleted = true, updated_at = now() WHERE {}::text = $1 AND gtfs_id = $2 AND deleted = false",
            table, pk
        );

        let result = sqlx::query(&sql)
            .bind(&id)
            .bind(gtfs_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::DbError(format!("delete_one_row {}: {}", table, e)))?;

        Ok(result.rows_affected())
    }

    async fn upsert_one_row(
        &self,
        table: &str,
        gtfs_id: &str,
        mut data: Value,
        to_regen: Option<Vec<String>>,
    ) -> AppResult<Value> {
        validate_table(table)?;
        let pk = table_pk(table)
            .ok_or_else(|| AppError::Internal(format!("No PK for table: {}", table)))?;

        let is_array = data.is_array();
        let mut arr = if let Some(a) = data.as_array_mut() {
            std::mem::take(a)
        } else if data.is_object() {
            vec![data]
        } else {
            return Err(AppError::BadRequest(
                "Body must be a JSON object or array of objects".to_string(),
            ));
        };

        if arr.is_empty() {
            return Err(AppError::BadRequest("Body is empty".to_string()));
        }

        // Auto-inject PK with a random ID if missing or null
        for val in arr.iter_mut() {
            if let Some(obj) = val.as_object_mut() {
                let missing = obj.get(pk).map_or(true, |v| v.is_null());
                if missing {
                    // numeric id for still-`bigint` columns (pre-migration), else UUID
                    let id = if self.gen_int_for_id {
                        Value::from(field_generator::gen_random_int_id())
                    } else {
                        Value::String(field_generator::gen_random_id())
                    };
                    obj.insert(pk.to_string(), id);
                }
            }
        }

        // Enrich bus_schedule_trip_detail_internal: derive schedule_number / org_name /
        // shift_type_name from the parent schedule (via schedule_trip_id) when the caller
        // didn't supply them. The single-row create UI omits these, and the trip-detail
        // read path requires a non-null schedule_number, so we backfill here for all callers.
        if table == "bus_schedule_trip_detail_internal" {
            for val in arr.iter_mut() {
                let Some(obj) = val.as_object_mut() else {
                    continue;
                };
                let needs_enrich = ["schedule_number", "org_name", "shift_type_name"]
                    .iter()
                    .any(|k| obj.get(*k).map_or(true, |v| v.is_null()));
                let trip_id = obj
                    .get("schedule_trip_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if let (true, Some(trip_id)) = (needs_enrich, trip_id) {
                    if let Some((schedule_number, org_name, shift_type_name)) =
                        fetch_schedule_meta_for_trip(&self.pool, &trip_id).await?
                    {
                        let mut set_if_absent = |key: &str, value: Option<String>| {
                            if let Some(v) = value {
                                if obj.get(key).map_or(true, |x| x.is_null()) {
                                    obj.insert(key.to_string(), Value::String(v));
                                }
                            }
                        };
                        set_if_absent("schedule_number", schedule_number);
                        set_if_absent("org_name", org_name);
                        set_if_absent("shift_type_name", shift_type_name);
                    }
                }
            }
        }

        // Apply field regeneration if requested
        if let Some(ref regen_fields) = to_regen {
            if let Some(columns) = table_columns(table) {
                field_generator::apply_regeneration(&mut arr, regen_fields, columns)?;
            } else {
                return Err(AppError::BadRequest(format!(
                    "Cannot regenerate fields: unknown table '{}'",
                    table
                )));
            }
        }

        let first_obj = arr[0]
            .as_object()
            .ok_or_else(|| AppError::BadRequest("Array must contain JSON objects".to_string()))?;

        if first_obj.is_empty() {
            return Err(AppError::BadRequest("First object is empty".to_string()));
        }

        // Build column list from caller's keys, excluding gtfs_id (we inject it ourselves)
        // Also exclude keys whose value is JSON null — omit-null semantics so that a partial
        // update (e.g. just {waybill_id, duty_date}) never triggers NOT NULL violations on
        // columns the caller didn't intend to touch.
        let mut cols: Vec<&str> = first_obj
            .keys()
            .filter(|k| k.as_str() != "gtfs_id" && !first_obj[k.as_str()].is_null())
            .map(|s| s.as_str())
            .collect();
        cols.push("gtfs_id"); // gtfs_id always last

        let update_set: Vec<String> = cols
            .iter()
            .map(|c| format!("{} = EXCLUDED.{}", c, c))
            .collect();

        let mut placeholders = Vec::new();
        let mut bind_index = 1;

        for _ in 0..arr.len() {
            let row_placeholders: Vec<String> = (0..cols.len())
                .map(|_| {
                    let s = format!("${}", bind_index);
                    bind_index += 1;
                    s
                })
                .collect();
            placeholders.push(format!("({})", row_placeholders.join(", ")));
        }

        let sql = format!(
            "INSERT INTO public.{} ({}) VALUES {} ON CONFLICT ({}) DO UPDATE SET {} RETURNING row_to_json({}) AS result",
            table,
            cols.join(", "),
            placeholders.join(", "),
            pk,
            update_set.join(", "),
            table
        );

        // Fetch the table's real column types so we coerce ids correctly across the
        // bigint->text migration: a numeric id may target a still-`bigint` column
        // (bind i64) or an already-migrated `text` column (bind String), and the
        // caller (old or new frontend) may send either a JSON number or string.
        // (Per-call lookup; cache by table name if this ever gets hot.)
        let col_types: HashMap<String, String> = sqlx::query_as::<_, (String, String)>(
            "SELECT column_name, data_type FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = $1",
        )
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("upsert_one_row col types {}: {}", table, e)))?
        .into_iter()
        .collect();
        let is_int_col = |c: &str| {
            matches!(
                col_types.get(c).map(|s| s.as_str()),
                Some("bigint") | Some("integer") | Some("smallint")
            )
        };

        let mut q = sqlx::query_scalar::<_, Value>(&sql);

        for val in &arr {
            let obj = val.as_object().ok_or_else(|| {
                AppError::BadRequest("Array must contain JSON objects".to_string())
            })?;
            // bind all cols except gtfs_id first
            for col in cols.iter().filter(|c| **c != "gtfs_id") {
                let col_val = obj.get(*col).unwrap_or(&Value::Null);
                let int_col = is_int_col(col);
                match col_val {
                    Value::String(s) => {
                        if int_col {
                            // numeric string into a bigint/int column -> bind i64
                            match s.parse::<i64>() {
                                Ok(i) => q = q.bind(i),
                                Err(_) => {
                                    return Err(AppError::BadRequest(format!(
                                        "Column '{}' is integer but value '{}' is not numeric",
                                        col, s
                                    )))
                                }
                            }
                        } else if col.ends_with("_at") || col.starts_with("effective_") {
                            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                                q = q.bind(dt.with_timezone(&chrono::Utc));
                            } else if let Ok(dt) = s.parse::<chrono::DateTime<chrono::Utc>>() {
                                q = q.bind(dt);
                            } else {
                                q = q.bind(s.as_str());
                            }
                        } else {
                            q = q.bind(s.as_str());
                        }
                    }
                    Value::Number(n) => {
                        if int_col {
                            // bigint/int column -> bind the integer directly
                            if let Some(i) = n.as_i64() {
                                q = q.bind(i);
                            } else {
                                return Err(AppError::BadRequest(format!(
                                    "Column '{}': numeric value not representable as integer",
                                    col
                                )));
                            }
                        } else if col.ends_with("_id") {
                            // already-migrated text id column -> coerce number to string
                            if let Some(i) = n.as_i64() {
                                q = q.bind(i.to_string());
                            } else if let Some(f) = n.as_f64() {
                                q = q.bind(f.to_string());
                            } else {
                                return Err(AppError::BadRequest(format!(
                                    "Column '{}': numeric value not representable as string",
                                    col
                                )));
                            }
                        } else if let Some(i) = n.as_i64() {
                            q = q.bind(i);
                        } else if let Some(f) = n.as_f64() {
                            q = q.bind(f);
                        } else {
                            return Err(AppError::BadRequest(format!(
                                "Column '{}': numeric value not representable",
                                col
                            )));
                        }
                    }
                    Value::Bool(b) => q = q.bind(b),
                    Value::Null => q = q.bind(Option::<String>::None),
                    _ => {
                        return Err(AppError::BadRequest(
                            "Unsupported JSON value type for key".to_string(),
                        ))
                    }
                }
            }
            // inject gtfs_id last
            q = q.bind(gtfs_id);
        }

        let results = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::DbError(format!("upsert_one_row {}: {}", table, e)))?;

        let ret = if is_array {
            Value::Array(results)
        } else {
            results.into_iter().next().unwrap_or(Value::Null)
        };
        Ok(ret)
    }

    async fn get_service_types_list(&self, gtfs_id: &str) -> AppResult<Vec<ServiceTypeRow>> {
        sqlx::query_as::<_, ServiceTypeRow>(
            "SELECT service_type_id, service_type_code, service_type_name
             FROM public.service_type_internal
             WHERE deleted = false AND gtfs_id = $1
             ORDER BY service_type_name",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_service_types_list: {}", e)))
    }

    async fn get_routes_list(&self, gtfs_id: &str) -> AppResult<Vec<RouteRow>> {
        sqlx::query_as::<_, RouteRow>(
            "SELECT route_id, route_number, route_name, route_direction, start_point_id, end_point_id, encoded_polyline
             FROM public.route_internal
             WHERE deleted = false AND gtfs_id = $1
             ORDER BY route_number",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_routes_list: {}", e)))
    }

    async fn get_depot_names_and_ids(&self, gtfs_id: &str) -> AppResult<Vec<DepotRow>> {
        sqlx::query_as::<_, DepotRow>(
            "SELECT entity_id, entity_name
             FROM public.entities_internal
             WHERE deleted = false AND gtfs_id = $1
             ORDER BY entity_name",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_depot_names_and_ids: {}", e)))
    }

    async fn get_schedule_numbers(&self, gtfs_id: &str) -> AppResult<Vec<ScheduleNumberRow>> {
        sqlx::query_as::<_, ScheduleNumberRow>(
            "SELECT schedule_id, schedule_number
             FROM public.bus_schedule_internal
             WHERE deleted = false AND gtfs_id = $1
             ORDER BY schedule_number",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_schedule_numbers: {}", e)))
    }

    async fn get_schedule_trip_details_by_schedule_number(
        &self,
        gtfs_id: &str,
        schedule_number: &str,
    ) -> AppResult<Vec<TripDetailRow>> {
        sqlx::query_as::<_, TripDetailRow>(
            r#"
            SELECT
                d.schedule_trip_detail_id,
                d.trip_number,
                d.trip_order,
                d.trip_type,
                d.start_time,
                d.end_time,
                d.break_time,
                d.break_type,
                d.shift_type_name,
                d.distance,
                d.route_number_id as route_id,
                d.schedule_trip_id,
                d.is_active_trip
            FROM public.bus_schedule_trip_detail_internal d
            JOIN public.bus_schedule_trip_internal t USING (schedule_trip_id)
            JOIN public.bus_schedule_internal s USING (schedule_id)
            WHERE s.schedule_number = $1
              AND d.deleted = false
              AND d.gtfs_id = $2
            ORDER BY d.trip_order
            "#,
        )
        .bind(schedule_number)
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_schedule_trip_details: {}", e)))
    }

    async fn get_fleets(&self, gtfs_id: &str) -> AppResult<Vec<FleetRow>> {
        sqlx::query_as::<_, FleetRow>(
            "SELECT vehicle_id, vehicle_no, fleet_no
             FROM public.vehicles_internal
             WHERE deleted = false AND gtfs_id = $1
             ORDER BY vehicle_no",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_fleets: {}", e)))
    }

    async fn get_conductor_data(
        &self,
        gtfs_id: &str,
        token: &str,
    ) -> AppResult<Option<EmployeeRow>> {
        let designation_id = self.designation_id_for(gtfs_id, "conductors").await?;

        sqlx::query_as::<_, EmployeeRow>(
            "SELECT emp_id, first_name, last_name, token_no, mobile_no
             FROM public.employees_internal
             WHERE token_no = $1
               AND designation_id::text = $2
               AND deleted = false
               AND gtfs_id = $3
             LIMIT 1",
        )
        .bind(token)
        .bind(designation_id)
        .bind(gtfs_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_conductor_data: {}", e)))
    }

    async fn get_driver_info(&self, gtfs_id: &str, token: &str) -> AppResult<Option<EmployeeRow>> {
        let designation_id = self.designation_id_for(gtfs_id, "drivers").await?;

        sqlx::query_as::<_, EmployeeRow>(
            "SELECT emp_id, first_name, last_name, token_no, mobile_no
             FROM public.employees_internal
             WHERE token_no = $1
               AND designation_id::text = $2
               AND deleted = false
               AND gtfs_id = $3
             LIMIT 1",
        )
        .bind(token)
        .bind(designation_id)
        .bind(gtfs_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_driver_info: {}", e)))
    }

    async fn get_device_ids(&self, gtfs_id: &str) -> AppResult<Vec<String>> {
        {
            let cache = self.device_ids_cache.read().await;
            if let Some((ids, ts)) = cache.etm_ids.get(gtfs_id) {
                if !Self::cache_expired(*ts, DEVICE_CACHE_SECS) {
                    info!("device_ids cache HIT");
                    return Ok(ids.clone());
                }
            }
        }

        info!("device_ids cache MISS");
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT etm_serial_no
             FROM public.fleet_etm_mapping_internal
             WHERE deleted = false AND etm_serial_no IS NOT NULL AND gtfs_id = $1
             ORDER BY etm_serial_no",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_device_ids: {}", e)))?;

        let ids: Vec<String> = rows.into_iter().map(|(s,)| s).collect();

        let mut cache = self.device_ids_cache.write().await;
        cache
            .etm_ids
            .insert(gtfs_id.to_string(), (ids.clone(), SystemTime::now()));
        Ok(ids)
    }

    async fn get_tablet_ids(&self, gtfs_id: &str) -> AppResult<Vec<String>> {
        {
            let cache = self.tablet_ids_cache.read().await;
            if let Some((ids, ts)) = cache.tablet_ids.get(gtfs_id) {
                if !Self::cache_expired(*ts, DEVICE_CACHE_SECS) {
                    info!("tablet_ids cache HIT");
                    return Ok(ids.clone());
                }
            }
        }

        info!("tablet_ids cache MISS");
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT DISTINCT obu_id
             FROM public.fleet_obu_mapping_internal
             WHERE deleted = false AND obu_id IS NOT NULL AND gtfs_id = $1
             ORDER BY obu_id",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_tablet_ids: {}", e)))?;

        let ids: Vec<String> = rows.into_iter().map(|(s,)| s).collect();

        let mut cache = self.tablet_ids_cache.write().await;
        cache
            .tablet_ids
            .insert(gtfs_id.to_string(), (ids.clone(), SystemTime::now()));
        Ok(ids)
    }

    async fn get_operators(&self, gtfs_id: &str, role: &str) -> AppResult<Vec<EmployeeRow>> {
        let designation_id = self.designation_id_for(gtfs_id, role).await?;

        sqlx::query_as::<_, EmployeeRow>(
            "SELECT emp_id, first_name, last_name, token_no, mobile_no
             FROM public.employees_internal
             WHERE designation_id::text = $1
               AND deleted = false
               AND gtfs_id = $2
             ORDER BY first_name",
        )
        .bind(designation_id)
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_operators: {}", e)))
    }

    async fn update_waybill_status(
        &self,
        gtfs_id: &str,
        waybill_id: String,
        status: &str,
    ) -> AppResult<u64> {
        if !waybill_statuses().contains(&status) {
            return Err(AppError::BadRequest(format!(
                "Invalid status '{}'. Valid: {:?}",
                status,
                waybill_statuses()
            )));
        }

        // First, get the schedule_trip_id for this waybill.
        // `::text` casts make the id predicates work whether the column is still
        // `bigint` (prod) or already migrated to `text` (local) — bind a string either way.
        let schedule_trip_id: Option<String> = sqlx::query_scalar(
            "SELECT schedule_trip_id::text FROM public.waybills_internal WHERE waybill_id::text = $1 AND gtfs_id = $2",
        )
        .bind(&waybill_id)
        .bind(gtfs_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("Failed to get schedule_trip_id: {}", e)))?;

        // Update the waybill status
        let query_str = if status == "audited" {
            "UPDATE public.waybills_internal SET status = $1, updated_at = now(), audited_date = now() WHERE waybill_id::text = $2 AND gtfs_id = $3"
        } else {
            "UPDATE public.waybills_internal SET status = $1, updated_at = now() WHERE waybill_id::text = $2 AND gtfs_id = $3"
        };
        let result = sqlx::query(query_str)
            .bind(status)
            .bind(&waybill_id)
            .bind(gtfs_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::DbError(format!("update_waybill_status: {}", e)))?;

        // If status is 'closed' or 'audited' and we have a schedule_trip_id,
        // set all is_active_trip to false and is_completed to false in bus_schedule_trip_detail_internal
        if (status == "closed" || status == "audited") && schedule_trip_id.is_some() {
            sqlx::query(
                "UPDATE public.bus_schedule_trip_detail_internal SET is_active_trip = false, is_completed = false WHERE schedule_trip_id::text = $1 AND gtfs_id = $2",
            )
            .bind(schedule_trip_id)
            .bind(gtfs_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::DbError(format!("Failed to update is_active_trip/is_completed: {}", e)))?;
        }

        Ok(result.rows_affected())
    }

    async fn update_waybill_status_v2(
        &self,
        gtfs_id: &str,
        waybill_id: String,
        status: &str,
        reset_trips: bool,
    ) -> AppResult<u64> {
        if !waybill_statuses().contains(&status) {
            return Err(AppError::BadRequest(format!(
                "Invalid status '{}'. Valid: {:?}",
                status,
                waybill_statuses()
            )));
        }

        let query_str = if status == "audited" {
            "UPDATE public.waybills_internal SET status = $1, updated_at = now(), audited_date = now() WHERE waybill_id::text = $2 AND gtfs_id = $3"
        } else {
            "UPDATE public.waybills_internal SET status = $1, updated_at = now() WHERE waybill_id::text = $2 AND gtfs_id = $3"
        };
        let result = sqlx::query(query_str)
            .bind(status)
            .bind(&waybill_id)
            .bind(gtfs_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::DbError(format!("update_waybill_status_v2: {}", e)))?;

        if reset_trips {
            // Only look up the schedule_trip_id when we actually need to reset trips.
            let schedule_trip_id: Option<String> = sqlx::query_scalar(
                "SELECT schedule_trip_id::text FROM public.waybills_internal WHERE waybill_id::text = $1 AND gtfs_id = $2",
            )
            .bind(&waybill_id)
            .bind(gtfs_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AppError::DbError(format!("Failed to get schedule_trip_id: {}", e)))?;

            if let Some(stid) = schedule_trip_id {
                sqlx::query(
                    "UPDATE public.bus_schedule_trip_detail_internal SET is_active_trip = false, is_completed = false WHERE schedule_trip_id::text = $1 AND gtfs_id = $2",
                )
                .bind(stid)
                .bind(gtfs_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AppError::DbError(format!("reset_trips update failed: {}", e)))?;
            }
        }

        Ok(result.rows_affected())
    }

    async fn update_waybill_fleet_number(
        &self,
        gtfs_id: &str,
        waybill_id: String,
        fleet_no: &str,
    ) -> AppResult<u64> {
        let result = sqlx::query(
            "UPDATE public.waybills_internal SET vehicle_no = $1, updated_at = now() WHERE waybill_id::text = $2 AND gtfs_id = $3",
        )
        .bind(fleet_no)
        .bind(waybill_id)
        .bind(gtfs_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("update_waybill_fleet_number: {}", e)))?;

        Ok(result.rows_affected())
    }

    async fn update_waybill_details(
        &self,
        gtfs_id: &str,
        body: &crate::models::UpdateWaybillDetailsBody,
    ) -> AppResult<u64> {
        if let Some(status) = body.status.as_deref() {
            if !waybill_statuses().contains(&status) {
                return Err(AppError::BadRequest(format!(
                    "Invalid status '{}'. Valid: {:?}",
                    status,
                    waybill_statuses()
                )));
            }
        }
        // COALESCE => only provided (non-null) fields change; identity/schedule columns are never listed,
        // so they cannot be modified through this endpoint.
        let result = sqlx::query(
            "UPDATE public.waybills_internal SET \
               vehicle_no = COALESCE($1, vehicle_no), \
               driver_token_no = COALESCE($2, driver_token_no), \
               driver_name = COALESCE($3, driver_name), \
               conductor_token_no = COALESCE($4, conductor_token_no), \
               conductor_name = COALESCE($5, conductor_name), \
               no_of_device = COALESCE($6, no_of_device), \
               device_serial_number = COALESCE($7, device_serial_number), \
               status = COALESCE($8, status), \
               audited_date = CASE WHEN $8 = 'audited' THEN now() ELSE audited_date END, \
               updated_at = now() \
             WHERE waybill_id::text = $9 AND gtfs_id = $10",
        )
        .bind(body.vehicle_no.as_deref())
        .bind(body.driver_token_no.as_deref())
        .bind(body.driver_name.as_deref())
        .bind(body.conductor_token_no.as_deref())
        .bind(body.conductor_name.as_deref())
        .bind(body.no_of_device)
        .bind(body.device_serial_number.as_deref())
        .bind(body.status.as_deref())
        .bind(body.waybill_id.to_string())
        .bind(gtfs_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("update_waybill_details: {}", e)))?;

        // Mirror update_waybill_status: closing/auditing a waybill also clears its trips'
        // runtime flags. Only looked up when the status actually calls for it.
        let closes_waybill = matches!(body.status.as_deref(), Some("closed") | Some("audited"));
        if closes_waybill && result.rows_affected() > 0 {
            let schedule_trip_id: Option<String> = sqlx::query_scalar(
                "SELECT schedule_trip_id::text FROM public.waybills_internal WHERE waybill_id::text = $1 AND gtfs_id = $2",
            )
            .bind(body.waybill_id.to_string())
            .bind(gtfs_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| AppError::DbError(format!("Failed to get schedule_trip_id: {}", e)))?;

            if let Some(stid) = schedule_trip_id {
                sqlx::query(
                    "UPDATE public.bus_schedule_trip_detail_internal SET is_active_trip = false, is_completed = false WHERE schedule_trip_id::text = $1 AND gtfs_id = $2",
                )
                .bind(stid)
                .bind(gtfs_id)
                .execute(&self.pool)
                .await
                .map_err(|e| {
                    AppError::DbError(format!("Failed to update is_active_trip/is_completed: {}", e))
                })?;
            }
        }

        Ok(result.rows_affected())
    }

    async fn update_waybill_tablet_id(
        &self,
        gtfs_id: &str,
        waybill_id: String,
        tablet_id: &str,
    ) -> AppResult<u64> {
        let result = sqlx::query(
            "UPDATE public.waybills_internal SET tablet_id = $1, updated_at = now() WHERE waybill_id::text = $2 AND gtfs_id = $3",
        )
        .bind(tablet_id)
        .bind(waybill_id)
        .bind(gtfs_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("update_waybill_tablet_id: {}", e)))?;

        Ok(result.rows_affected())
    }

    async fn get_waybills(
        &self,
        gtfs_id: &str,
        limit: i64,
        offset: i64,
    ) -> AppResult<Vec<WaybillsInternal>> {
        let vals: Vec<Value> = sqlx::query_scalar::<_, Value>(
            "SELECT row_to_json(t) FROM (
                SELECT * FROM public.waybills_internal
                WHERE gtfs_id = $1
                ORDER BY waybill_id DESC
                LIMIT $2 OFFSET $3
             ) t",
        )
        .bind(gtfs_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_waybills: {}", e)))?;

        vals.into_iter()
            .map(|v| {
                serde_json::from_value(v)
                    .map_err(|e| AppError::Internal(format!("Failed to parse waybill: {}", e)))
            })
            .collect()
    }

    async fn search_stops(
        &self,
        gtfs_id: &str,
        q: &str,
        limit: i64,
        with_routes: bool,
    ) -> AppResult<Vec<EnrichedStop>> {
        let pattern = format!("%{}%", q);
        let rows = sqlx::query_as::<_, EnrichedStopRow>(
            "SELECT s.bus_stop_id, s.bus_stop_code, s.bus_stop_name, s.latitude_current, s.longitude_current, \
                s.source, s.status, \
                (SELECT COUNT(DISTINCT rp.route_id) FROM route_point_internal rp \
                   WHERE rp.bus_stop_id = s.bus_stop_id AND rp.gtfs_id = s.gtfs_id AND rp.deleted = false) AS route_count, \
                NULL::double precision AS distance_m \
             FROM stop_internal s \
             WHERE s.gtfs_id = $1 AND s.deleted = false AND LOWER(COALESCE(s.status,'active')) <> 'inactive' \
               AND (s.bus_stop_name ILIKE $2 OR s.bus_stop_code ILIKE $2) \
             ORDER BY s.bus_stop_name LIMIT $3",
        )
        .bind(gtfs_id)
        .bind(&pattern)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("search_stops: {}", e)))?;
        enrich_stops(&self.pool, gtfs_id, rows, with_routes).await
    }

    async fn nearby_stops(
        &self,
        gtfs_id: &str,
        lat: f64,
        lon: f64,
        radius_m: f64,
        limit: i64,
        with_routes: bool,
    ) -> AppResult<Vec<EnrichedStop>> {
        // No PostGIS dependency: a cheap lat/lon bounding-box prefilter narrows candidates,
        // then the great-circle (haversine) distance is computed in SQL for exact filtering and
        // ordering. Bounding box is computed here to keep the query index-friendly.
        // $1=gtfs_id $2=lat $3=lon $4=radius_m $5=limit $6..$9=bbox (min/max lat, min/max lon).
        const M_PER_DEG_LAT: f64 = 111_320.0;
        let lat_delta = radius_m / M_PER_DEG_LAT;
        // Guard against division by ~0 near the poles; clamp cos(lat) to a small floor.
        let cos_lat = lat.to_radians().cos().abs().max(1e-6);
        let lon_delta = radius_m / (M_PER_DEG_LAT * cos_lat);
        let (min_lat, max_lat) = (lat - lat_delta, lat + lat_delta);
        let (min_lon, max_lon) = (lon - lon_delta, lon + lon_delta);
        let rows = sqlx::query_as::<_, EnrichedStopRow>(
            "SELECT * FROM ( \
               SELECT s.bus_stop_id, s.bus_stop_code, s.bus_stop_name, s.latitude_current, s.longitude_current, \
                 s.source, s.status, \
                 (SELECT COUNT(DISTINCT rp.route_id) FROM route_point_internal rp \
                    WHERE rp.bus_stop_id = s.bus_stop_id AND rp.gtfs_id = s.gtfs_id AND rp.deleted = false) AS route_count, \
                 6371000.0 * acos(LEAST(1.0, GREATEST(-1.0, \
                   sin(radians($2)) * sin(radians(s.latitude_current)) \
                   + cos(radians($2)) * cos(radians(s.latitude_current)) \
                     * cos(radians(s.longitude_current) - radians($3)) \
                 ))) AS distance_m \
               FROM stop_internal s \
               WHERE s.gtfs_id = $1 AND s.deleted = false AND LOWER(COALESCE(s.status,'active')) <> 'inactive' \
                 AND s.latitude_current BETWEEN $6 AND $7 \
                 AND s.longitude_current BETWEEN $8 AND $9 \
             ) t \
             WHERE t.distance_m <= $4 \
             ORDER BY t.distance_m LIMIT $5",
        )
        .bind(gtfs_id)
        .bind(lat)
        .bind(lon)
        .bind(radius_m)
        .bind(limit)
        .bind(min_lat)
        .bind(max_lat)
        .bind(min_lon)
        .bind(max_lon)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("nearby_stops: {}", e)))?;
        enrich_stops(&self.pool, gtfs_id, rows, with_routes).await
    }

    async fn bulk_replace_stops(
        &self,
        gtfs_id: &str,
        from: &[String],
        to: &str,
    ) -> AppResult<BulkReplaceResult> {
        if from.is_empty() {
            return Err(AppError::BadRequest("from list is empty".to_string()));
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::DbError(format!("bulk_replace begin: {}", e)))?;

        let affected: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT route_id FROM route_point_internal \
             WHERE gtfs_id=$1 AND deleted=false AND bus_stop_id = ANY($2) AND bus_stop_id <> $3",
        )
        .bind(gtfs_id)
        .bind(from)
        .bind(to)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| AppError::DbError(format!("bulk_replace affected: {}", e)))?;
        let affected_route_ids: Vec<String> = affected.into_iter().map(|(r,)| r).collect();

        let res = sqlx::query(
            "UPDATE route_point_internal SET bus_stop_id=$3, updated_at=now() \
             WHERE gtfs_id=$1 AND deleted=false AND bus_stop_id = ANY($2) AND bus_stop_id <> $3",
        )
        .bind(gtfs_id)
        .bind(from)
        .bind(to)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppError::DbError(format!("bulk_replace update: {}", e)))?;
        let rows_affected = res.rows_affected();

        if !affected_route_ids.is_empty() {
            sqlx::query(
                "WITH ordered AS ( \
                   SELECT route_points_id, bus_stop_id, \
                     LAG(bus_stop_id) OVER (PARTITION BY route_id ORDER BY route_order, created_at, route_points_id) AS prev_stop \
                   FROM route_point_internal \
                   WHERE gtfs_id=$1 AND deleted=false AND route_id = ANY($2) \
                 ) \
                 UPDATE route_point_internal rp SET deleted=true, updated_at=now() \
                 FROM ordered o WHERE rp.route_points_id = o.route_points_id AND o.bus_stop_id = o.prev_stop",
            )
            .bind(gtfs_id)
            .bind(&affected_route_ids)
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::DbError(format!("bulk_replace collapse: {}", e)))?;
        }

        tx.commit()
            .await
            .map_err(|e| AppError::DbError(format!("bulk_replace commit: {}", e)))?;
        Ok(BulkReplaceResult {
            rows_affected,
            affected_route_ids,
        })
    }

    async fn get_route_stops(
        &self,
        gtfs_id: &str,
        route_id: &str,
    ) -> AppResult<RouteStopsResponse> {
        let header: Option<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT route_number, route_name, encoded_polyline FROM route_internal \
             WHERE gtfs_id=$1 AND route_id=$2 AND deleted=false LIMIT 1",
        )
        .bind(gtfs_id)
        .bind(route_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_route_stops header: {}", e)))?;
        let (route_number, route_name, encoded_polyline) =
            header.ok_or_else(|| AppError::NotFound(format!("route {} not found", route_id)))?;

        let rows = sqlx::query_as::<_, RouteStopJoinRow>(
            "SELECT rp.route_points_id, rp.bus_stop_id, s.bus_stop_name AS stop_name, \
                COALESCE(s.latitude_current,0)::double precision AS lat, COALESCE(s.longitude_current,0)::double precision AS lon, \
                rp.route_order::bigint AS route_order, rp.stage_no::bigint AS stage_no, \
                rp.stage_name, rp.stop_type, rp.is_visible, rp.travel_distance::bigint AS travel_distance, rp.travel_time \
             FROM route_point_internal rp \
             LEFT JOIN stop_internal s ON s.bus_stop_id = rp.bus_stop_id AND s.gtfs_id = rp.gtfs_id AND s.deleted=false \
             WHERE rp.gtfs_id=$1 AND rp.route_id=$2 AND rp.deleted=false \
             ORDER BY rp.route_order, rp.created_at, rp.route_points_id",
        )
        .bind(gtfs_id)
        .bind(route_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("get_route_stops: {}", e)))?;

        let stops = rows
            .into_iter()
            .map(|r| RouteStopDetail {
                route_point_id: r.route_points_id,
                bus_stop_id: r.bus_stop_id,
                stop_name: r.stop_name,
                lat: r.lat,
                lon: r.lon,
                route_order: r.route_order,
                stage_no: r.stage_no,
                stage_name: r.stage_name,
                stop_type: r.stop_type,
                is_visible: r.is_visible,
                travel_distance: r.travel_distance,
                travel_time: r.travel_time,
            })
            .collect();

        Ok(RouteStopsResponse {
            route_id: route_id.to_string(),
            route_number,
            route_name,
            encoded_polyline,
            stops,
        })
    }

    async fn insert_route_stop(
        &self,
        gtfs_id: &str,
        route_id: &str,
        position: i64,
        data: Value,
    ) -> AppResult<Value> {
        let bus_stop_id = data
            .get("bus_stop_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::BadRequest("bus_stop_id is required".to_string()))?;
        let travel_distance = data.get("travel_distance").and_then(|v| v.as_i64());
        let stop_type = data
            .get("stop_type")
            .and_then(|v| v.as_str())
            .unwrap_or("INTERMEDIATE STOP");
        let stage_no = data.get("stage_no").and_then(|v| v.as_i64());
        let stage_name = data.get("stage_name").and_then(|v| v.as_str());
        let travel_time = data.get("travel_time").and_then(|v| v.as_str());
        // New route stops are visible and active by default.
        let is_visible = data
            .get("is_visible")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let new_id = field_generator::gen_random_id();

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::DbError(format!("insert_route_stop begin: {}", e)))?;

        sqlx::query(
            "UPDATE route_point_internal SET route_order = route_order + 1, updated_at = now() \
             WHERE gtfs_id=$1 AND route_id=$2 AND deleted=false AND route_order >= $3",
        )
        .bind(gtfs_id)
        .bind(route_id)
        .bind(position)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppError::DbError(format!("insert_route_stop shift: {}", e)))?;

        sqlx::query(
            "INSERT INTO route_point_internal \
               (route_points_id, route_id, gtfs_id, route_order, bus_stop_id, travel_distance, travel_time, stop_type, stage_no, stage_name, is_visible, point_status, deleted) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'active',false)",
        )
        .bind(&new_id)
        .bind(route_id)
        .bind(gtfs_id)
        .bind(position)
        .bind(bus_stop_id)
        .bind(travel_distance)
        .bind(travel_time)
        .bind(stop_type)
        .bind(stage_no)
        .bind(stage_name)
        .bind(is_visible)
        .execute(&mut *tx)
        .await
        .map_err(|e| AppError::DbError(format!("insert_route_stop insert: {}", e)))?;

        tx.commit()
            .await
            .map_err(|e| AppError::DbError(format!("insert_route_stop commit: {}", e)))?;

        Ok(serde_json::json!({ "route_points_id": new_id, "route_order": position }))
    }

    async fn reprocess_routes(
        &self,
        gtfs_id: &str,
        route_ids: &[String],
        recompute_polyline: bool,
    ) -> AppResult<Vec<ReprocessResult>> {
        let mut out = Vec::new();
        for route_id in route_ids {
            let pts = sqlx::query_as::<_, ReprocessPointRow>(
                "SELECT rp.route_points_id, rp.stop_type, s.bus_stop_name AS stop_name, \
                    rp.stage_name AS existing_stage_name, \
                    COALESCE(s.latitude_current,0) AS lat, COALESCE(s.longitude_current,0) AS lon \
                 FROM route_point_internal rp \
                 LEFT JOIN stop_internal s ON s.bus_stop_id = rp.bus_stop_id AND s.gtfs_id = rp.gtfs_id AND s.deleted=false \
                 WHERE rp.gtfs_id=$1 AND rp.route_id=$2 AND rp.deleted=false \
                 ORDER BY rp.route_order, rp.created_at, rp.route_points_id",
            )
            .bind(gtfs_id)
            .bind(route_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::DbError(format!("reprocess fetch {}: {}", route_id, e)))?;

            if pts.is_empty() {
                out.push(ReprocessResult {
                    route_id: route_id.clone(),
                    stops_renumbered: 0,
                    stages: 0,
                    route_name: None,
                    polyline: None,
                });
                continue;
            }

            // Pass 1: assign each point a stage number. A new stage opens at the first
            // point and at every stage stop.
            let mut stage = 1i64;
            let mut max_stage = 1i64;
            let mut point_stage: Vec<i64> = Vec::with_capacity(pts.len());
            for (i, p) in pts.iter().enumerate() {
                let st = StopType::from_opt(p.stop_type.as_deref());
                if i == 0 {
                    stage = 1;
                } else if st == StopType::StageStop {
                    stage += 1;
                }
                max_stage = max_stage.max(stage);
                point_stage.push(stage);
            }

            // Stage names are always manual — never derived from the stop name. A stage's
            // name is the stored stage_name of the first point in that stage that has one;
            // stages nobody named stay blank until set manually.
            let mut stage_name_map: HashMap<i64, String> = HashMap::new();
            for (i, p) in pts.iter().enumerate() {
                if let Some(existing) = p.existing_stage_name.as_deref() {
                    let e = existing.trim();
                    if !e.is_empty() {
                        stage_name_map
                            .entry(point_stage[i])
                            .or_insert_with(|| e.to_string());
                    }
                }
            }

            // Pass 2: build the per-point updates, propagating each stage's manual name.
            // (route_points_id, new_order, stage_no, stage_name)
            let mut updates: Vec<(String, i64, i64, String)> = Vec::with_capacity(pts.len());
            for (i, p) in pts.iter().enumerate() {
                let s = point_stage[i];
                let name = stage_name_map.get(&s).cloned().unwrap_or_default();
                updates.push((p.route_points_id.clone(), (i as i64) + 1, s, name));
            }

            // route_name = "<first stop> - <last stop>". Fall back to whichever endpoint
            // has a name so a route with one unnamed terminal still gets named.
            let route_name = match (
                pts.first().and_then(|p| p.stop_name.clone()),
                pts.last().and_then(|p| p.stop_name.clone()),
            ) {
                (Some(a), Some(b)) => Some(format!("{} - {}", a, b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };

            let ids: Vec<&str> = updates.iter().map(|(id, _, _, _)| id.as_str()).collect();
            let orders: Vec<i64> = updates.iter().map(|(_, o, _, _)| *o).collect();
            let stage_nos: Vec<i64> = updates.iter().map(|(_, _, s, _)| *s).collect();
            let stage_names: Vec<&str> = updates.iter().map(|(_, _, _, n)| n.as_str()).collect();

            let mut tx = self
                .pool
                .begin()
                .await
                .map_err(|e| AppError::DbError(format!("reprocess begin: {}", e)))?;
            sqlx::query(
                "UPDATE route_point_internal AS rp \
                 SET route_order = u.route_order, stage_no = u.stage_no, stage_name = u.stage_name, updated_at = now() \
                 FROM (SELECT UNNEST($1::text[]) AS route_points_id, \
                              UNNEST($2::bigint[]) AS route_order, \
                              UNNEST($3::bigint[]) AS stage_no, \
                              UNNEST($4::text[]) AS stage_name) AS u \
                 WHERE rp.route_points_id = u.route_points_id AND rp.gtfs_id = $5",
            )
            .bind(&ids)
            .bind(&orders)
            .bind(&stage_nos)
            .bind(&stage_names)
            .bind(gtfs_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::DbError(format!("reprocess update points: {}", e)))?;
            if let Some(ref rn) = route_name {
                sqlx::query(
                    "UPDATE route_internal SET route_name=$2, updated_at=now() WHERE route_id=$1 AND gtfs_id=$3",
                )
                .bind(route_id)
                .bind(rn)
                .bind(gtfs_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| AppError::DbError(format!("reprocess route_name: {}", e)))?;
            }
            tx.commit()
                .await
                .map_err(|e| AppError::DbError(format!("reprocess commit: {}", e)))?;

            let mut polyline_out = None;
            if recompute_polyline {
                let coords: Vec<(f64, f64)> = pts.iter().map(|p| (p.lat, p.lon)).collect();
                match osrm_route(self.osrm_url.as_deref(), &coords).await {
                    Some((geom, legs)) => {
                        if let Err(e) = sqlx::query(
                            "UPDATE route_internal SET encoded_polyline=$2, updated_at=now() WHERE route_id=$1 AND gtfs_id=$3",
                        )
                        .bind(route_id)
                        .bind(&geom)
                        .bind(gtfs_id)
                        .execute(&self.pool)
                        .await
                        {
                            error!("reprocess_routes: failed to persist polyline for route {}: {}", route_id, e);
                        }
                        // leg[i] is between pts[i] and pts[i+1] → assign to pts[i+1].
                        // travel_distance is metres, travel_time is SECONDS -
                        // OSRM gives both in those units already, and seconds is
                        // the convention the ETA reader and the transit editor
                        // use. Older rows may still hold milliseconds;
                        // parse_leg_travel_seconds reads either.
                        for (i, (dist, dur)) in legs.iter().enumerate() {
                            if let Some(p) = pts.get(i + 1) {
                                if let Err(e) = sqlx::query(
                                    "UPDATE route_point_internal SET travel_distance=$2, travel_time=$3, updated_at=now() \
                                     WHERE route_points_id=$1 AND gtfs_id=$4",
                                )
                                .bind(&p.route_points_id)
                                .bind(*dist as i64)
                                .bind(format!("{}", dur.round() as i64))
                                .bind(gtfs_id)
                                .execute(&self.pool)
                                .await
                                {
                                    error!("reprocess_routes: failed to persist leg distance for point {}: {}", p.route_points_id, e);
                                }
                            }
                        }
                        polyline_out = Some(geom);
                    }
                    None => {
                        warn!(
                            "reprocess_routes: OSRM unavailable for route {}, polyline not updated",
                            route_id
                        );
                    }
                }
            }

            out.push(ReprocessResult {
                route_id: route_id.clone(),
                stops_renumbered: updates.len() as i64,
                stages: max_stage,
                route_name,
                polyline: polyline_out,
            });
        }
        Ok(out)
    }

    async fn export_route_stop_mapping(&self, gtfs_id: &str) -> AppResult<Vec<Value>> {
        let rows = sqlx::query_scalar::<_, Value>(
            "SELECT row_to_json(t) FROM ( \
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
             ) t",
        )
        .bind(gtfs_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("export_route_stop_mapping: {}", e)))?;
        Ok(rows)
    }
}
