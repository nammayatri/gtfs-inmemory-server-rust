use async_trait::async_trait;
use serde_json::Value;
use sqlx::postgres::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::info;

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

pub const SUPPORTED_OPERATOR_GTFS_IDS: &[&str] = &["chennai_bus"];

pub const MAX_QUERY_LIMIT: i64 = 1000;
pub const MAX_QUERY_FILTERS: usize = 5;

#[derive(Debug, serde::Deserialize)]
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
    pub service_type_id: i64,
    pub service_type_code: Option<String>,
    pub service_type_name: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct RouteRow {
    pub route_id: i64,
    pub route_number: Option<String>,
    pub route_name: Option<String>,
    pub route_direction: Option<String>,
    pub start_point_id: i64,
    pub end_point_id: i64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct DepotRow {
    pub entity_id: i64,
    pub entity_name: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct ScheduleNumberRow {
    pub schedule_id: i64,
    pub schedule_number: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct FleetRow {
    pub vehicle_id: i64,
    pub vehicle_no: Option<String>,
    pub fleet_no: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct EmployeeRow {
    pub emp_id: i64,
    pub first_name: String,
    pub last_name: Option<String>,
    pub token_no: Option<String>,
    pub mobile_no: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct TripDetailRow {
    pub schedule_trip_detail_id: i64,
    pub trip_number: i32,
    pub trip_order: i32,
    pub trip_type: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub break_time: Option<String>,
    pub break_type: Option<String>,
    pub shift_type_name: Option<String>,
    pub distance: f32,
    pub schedule_trip_id: i64,
    pub is_active_trip: bool,
    pub route_id: i32,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct RouteInternal {
    pub route_id: i64,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub description: Option<String>,
    pub route_direction: Option<String>,
    pub route_group: Option<String>,
    pub route_name: Option<String>,
    pub route_number: Option<String>,
    pub route_string: Option<String>,
    pub route_type_id: i64,
    pub status: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub via: Option<String>,
    pub bus_service_type_id: i64,
    pub end_point_id: i64,
    pub start_point_id: i64,
    pub route_distance: Option<f64>,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct RoutePointInternal {
    pub route_points_id: i64,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub fare_stage: Option<String>,
    pub point_status: Option<String>,
    pub route_order: i64,
    pub stage_no: Option<i64>,
    pub sub_stage: Option<String>,
    pub travel_distance: i64,
    pub travel_time: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub bus_stop_id: i64,
    pub route_id: i64,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct BusScheduleInternal {
    pub schedule_id: i64,
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
    pub entity_id: i64,
    pub route_id: i64,
    pub service_type_id: i64,
    pub schedule_type_id: i64,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct BusScheduleTripInternal {
    pub schedule_trip_id: i64,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub effective_end_date: Option<chrono::DateTime<chrono::Utc>>,
    pub effective_start_date: Option<chrono::DateTime<chrono::Utc>>,
    pub no_trip: i64,
    pub schedule_number_name: Option<String>,
    pub start_time: Option<String>,
    pub status: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub calendar_id: i64,
    pub schedule_id: i64,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct BusScheduleTripDetailInternal {
    pub schedule_trip_detail_id: i64,
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
    pub calendar_id: i64,
    pub route_number_id: i64,
    pub schedule_trip_id: i64,
    pub is_active_trip: bool,
    pub trip_end_time: Option<String>,
    pub trip_start_time: Option<String>,
    pub sync_end_time: Option<String>,
    pub sync_start_time: Option<String>,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct BusScheduleTripFlexiInternal {
    pub schedule_trip_flexi_id: i64,
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
    pub calendar_id: i64,
    pub route_number_id: i64,
    pub schedule_trip_id: i64,
    pub waybill_id: i64,
    pub is_active_trip: bool,
    pub trip_end_time: Option<String>,
    pub trip_start_time: Option<String>,
    pub sync_end_time: Option<String>,
    pub sync_start_time: Option<String>,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct ServiceTypeInternal {
    pub service_type_id: i64,
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
    pub bus_stop_id: i64,
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
    pub stop_direction: Option<String>,
    pub stop_group_id: Option<i64>,
    pub stop_type_id: i64,
    pub sub_stage: Option<String>,
    pub toll_fee: Option<i64>,
    pub toll_zone: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct DesignationsInternal {
    pub designation_id: i64,
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
    pub emp_id: i64,
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
    pub department_id: i64,
    pub designation_id: i64,
    pub entity_id: i64,
    pub organization_id: i64,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct EntitiesInternal {
    pub entity_id: i64,
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
    pub organization_id: i64,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct VehiclesInternal {
    pub vehicle_id: i64,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub fleet_no: Option<String>,
    pub status: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub vehicle_no: Option<String>,
    pub bus_service_type_id: i64,
    pub entity_id: i64,
    pub organization_id: i64,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct WaybillDeviceInternal {
    pub waybill_device_id: i64,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
    pub device_serial_no: Option<String>,
    pub is_audited: Option<bool>,
    pub is_primary: Option<bool>,
    pub is_uploaded: Option<bool>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub waybill_id: i64,
    pub gtfs_id: String,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct FleetEtmMappingInternal {
    pub fleet_etm_mapping_id: i64,
    pub vehicle_no: String,
    pub gtfs_id: String,
    pub etm_serial_no: String,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct FleetObuMappingInternal {
    pub fleet_obu_mapping_id: i64,
    pub vehicle_no: String,
    pub gtfs_id: String,
    pub obu_id: String,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deleted: bool,
}
#[derive(Debug, serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone)]
pub struct WaybillsInternal {
    pub waybill_id: i64,
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
    pub schedule_id: i64,
    pub schedule_no: Option<String>,
    pub schedule_trip_name: Option<String>,
    pub schedule_type: Option<String>,
    pub service_type: Option<String>,
    pub schedule_start_time: Option<String>,
    pub status: Option<String>,
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub vehicle_no: Option<String>,
    pub waybill_no: Option<String>,
    pub entity_id: i64,
    pub schedule_trip_id: i64,
    pub service_type_id: i64,
    pub shift_type_id: i64,
    pub tablet_id: Option<String>,
    pub gtfs_id: String,
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
        waybill_id: i64,
        status: &str,
    ) -> AppResult<u64>;
    async fn update_waybill_fleet_number(
        &self,
        gtfs_id: &str,
        waybill_id: i64,
        fleet_no: &str,
    ) -> AppResult<u64>;
    async fn update_waybill_tablet_id(
        &self,
        gtfs_id: &str,
        waybill_id: i64,
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
        _waybill_id: i64,
        _status: &str,
    ) -> AppResult<u64> {
        mock_err!()
    }

    async fn update_waybill_fleet_number(
        &self,
        _gtfs_id: &str,
        _waybill_id: i64,
        _fleet_no: &str,
    ) -> AppResult<u64> {
        mock_err!()
    }

    async fn update_waybill_tablet_id(
        &self,
        _gtfs_id: &str,
        _waybill_id: i64,
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
}

struct DeviceIdsCache {
    etm_ids: HashMap<String, (Vec<String>, SystemTime)>,
}

struct TabletIdsCache {
    tablet_ids: HashMap<String, (Vec<String>, SystemTime)>,
}

// Designation cache: name (lowercase) → id
struct DesignationCache {
    map: Option<HashMap<String, i64>>,
}

const DEVICE_CACHE_SECS: u64 = 3600; // 1 hour

pub struct DBOperatorService {
    pool: PgPool,
    device_ids_cache: Arc<RwLock<DeviceIdsCache>>,
    tablet_ids_cache: Arc<RwLock<TabletIdsCache>>,
    designation_cache: Arc<RwLock<DesignationCache>>,
}

impl DBOperatorService {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            device_ids_cache: Arc::new(RwLock::new(DeviceIdsCache {
                etm_ids: HashMap::new(),
            })),
            tablet_ids_cache: Arc::new(RwLock::new(TabletIdsCache {
                tablet_ids: HashMap::new(),
            })),
            designation_cache: Arc::new(RwLock::new(DesignationCache { map: None })),
        }
    }

    fn cache_expired(ts: SystemTime, secs: u64) -> bool {
        ts.elapsed().unwrap_or_default() >= Duration::from_secs(secs)
    }

    /// Load designation name→id map if not already loaded (infinite TTL).
    async fn ensure_designation_cache(&self) -> AppResult<()> {
        {
            let cache = self.designation_cache.read().await;
            if cache.map.is_some() {
                return Ok(());
            }
        }

        info!("Loading designation cache from DB");
        let rows = sqlx::query_as::<_, (i64, String)>(
            "SELECT designation_id, LOWER(designation_name) FROM designations_internal WHERE deleted = false",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("designation cache load: {}", e)))?;

        let map: HashMap<String, i64> = rows.into_iter().map(|(id, name)| (name, id)).collect();
        info!("Designation cache loaded with {} entries", map.len());

        let mut cache = self.designation_cache.write().await;
        cache.map = Some(map);
        Ok(())
    }

    async fn designation_id_for(&self, role: &str) -> AppResult<i64> {
        self.ensure_designation_cache().await?;
        let cache = self.designation_cache.read().await;
        let map = cache.map.as_ref().unwrap();
        // role could be "conductors" or "drivers"; strip trailing 's' for matching
        let search = role.trim_end_matches('s').to_lowercase();
        // Find partial match (e.g. "conductor" matches "conductor", "driver" matches "driver")
        map.iter()
            .find(|(name, _)| name.contains(&search))
            .map(|(_, id)| *id)
            .ok_or_else(|| AppError::NotFound(format!("No designation found for role: {}", role)))
    }
}

#[async_trait]
impl OperatorService for DBOperatorService {
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

        let extra_clause: String = cols
            .iter()
            .enumerate()
            .map(|(i, col)| format!("{} = ${}", col, i + 2))
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

        let sql = format!(
            "SELECT row_to_json(t) FROM (SELECT * FROM public.{} WHERE gtfs_id = $1 AND deleted = false ORDER BY 1 LIMIT $2 OFFSET $3) t",
            table
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
            where_parts.push(format!("{} {} ${}", f.col, f.sql_op, param_idx));
            bind_vals.push(f.val1.clone());
            param_idx += 1;
        }

        let limit = body.limit.unwrap_or(15).min(MAX_QUERY_LIMIT);
        let offset = body.offset.unwrap_or(0);

        let sql = format!(
            "SELECT row_to_json(t) FROM (SELECT * FROM public.{} WHERE gtfs_id = $1 AND deleted = false AND {} ORDER BY 1 LIMIT ${} OFFSET ${}) t",
            table,
            where_parts.join(" AND "),
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
                .ok_or_else(|| AppError::BadRequest("ID must be an integer".to_string()))?,
            Value::String(s) => s
                .parse::<i64>()
                .map_err(|_| AppError::BadRequest("ID must be an integer string".to_string()))?,
            _ => return Err(AppError::BadRequest("ID must be an integer".to_string())),
        };

        let sql = format!(
            "UPDATE public.{} SET deleted = true, updated_at = now() WHERE {} = $1 AND gtfs_id = $2 AND deleted = false",
            table, pk
        );

        let result = sqlx::query(&sql)
            .bind(id)
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

        let mut q = sqlx::query_scalar::<_, Value>(&sql);

        for val in &arr {
            let obj = val.as_object().ok_or_else(|| {
                AppError::BadRequest("Array must contain JSON objects".to_string())
            })?;
            // bind all cols except gtfs_id first
            for col in cols.iter().filter(|c| **c != "gtfs_id") {
                let col_val = obj.get(*col).unwrap_or(&Value::Null);
                match col_val {
                    Value::String(s) => {
                        if col.ends_with("_at") || col.starts_with("effective_") {
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
                        if let Some(i) = n.as_i64() {
                            q = q.bind(i);
                        } else if let Some(f) = n.as_f64() {
                            q = q.bind(f);
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
            "SELECT route_id, route_number, route_name, route_direction, start_point_id, end_point_id
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
                d.route_number_id::int as route_id,
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
        self.ensure_designation_cache().await?;
        let designation_id = self.designation_id_for("conductors").await?;

        sqlx::query_as::<_, EmployeeRow>(
            "SELECT emp_id, first_name, last_name, token_no, mobile_no
             FROM public.employees_internal
             WHERE token_no = $1
               AND designation_id = $2
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
        self.ensure_designation_cache().await?;
        let designation_id = self.designation_id_for("drivers").await?;

        sqlx::query_as::<_, EmployeeRow>(
            "SELECT emp_id, first_name, last_name, token_no, mobile_no
             FROM public.employees_internal
             WHERE token_no = $1
               AND designation_id = $2
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
        let designation_id = self.designation_id_for(role).await?;

        sqlx::query_as::<_, EmployeeRow>(
            "SELECT emp_id, first_name, last_name, token_no, mobile_no
             FROM public.employees_internal
             WHERE designation_id = $1
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
        waybill_id: i64,
        status: &str,
    ) -> AppResult<u64> {
        if !waybill_statuses().contains(&status) {
            return Err(AppError::BadRequest(format!(
                "Invalid status '{}'. Valid: {:?}",
                status,
                waybill_statuses()
            )));
        }

        let result = sqlx::query(
            "UPDATE public.waybills_internal SET status = $1, updated_at = now() WHERE waybill_id = $2 AND gtfs_id = $3",
        )
        .bind(status)
        .bind(waybill_id)
        .bind(gtfs_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("update_waybill_status: {}", e)))?;

        Ok(result.rows_affected())
    }

    async fn update_waybill_fleet_number(
        &self,
        gtfs_id: &str,
        waybill_id: i64,
        fleet_no: &str,
    ) -> AppResult<u64> {
        let result = sqlx::query(
            "UPDATE public.waybills_internal SET vehicle_no = $1, updated_at = now() WHERE waybill_id = $2 AND gtfs_id = $3",
        )
        .bind(fleet_no)
        .bind(waybill_id)
        .bind(gtfs_id)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::DbError(format!("update_waybill_fleet_number: {}", e)))?;

        Ok(result.rows_affected())
    }

    async fn update_waybill_tablet_id(
        &self,
        gtfs_id: &str,
        waybill_id: i64,
        tablet_id: &str,
    ) -> AppResult<u64> {
        let result = sqlx::query(
            "UPDATE public.waybills_internal SET tablet_id = $1, updated_at = now() WHERE waybill_id = $2 AND gtfs_id = $3",
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
}
