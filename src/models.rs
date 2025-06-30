use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use chrono::{DateTime, Utc, NaiveDate};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct VehicleData {
    pub waybill_id: String,
    pub service_type: String,
    pub vehicle_no: String,
    pub schedule_no: String,
    pub last_updated: Option<DateTime<Utc>>,
    pub duty_date: Option<NaiveDate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VehicleServiceTypeResponse {
    pub vehicle_no: String,
    pub service_type: String,
    pub waybill_id: Option<String>,
    pub schedule_no: Option<String>,
    pub last_updated: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatLong {
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NandiStop {
    pub id: String,
    pub code: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NandiTrip {
    pub id: String,
    pub direction: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NandiPattern {
    pub id: String,
    pub desc: String,
    #[serde(rename = "routeId")]
    pub route_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NandiPatternDetails {
    pub id: String,
    pub desc: Option<String>,
    #[serde(rename = "routeId")]
    pub route_id: String,
    pub stops: Vec<NandiStop>,
    pub trips: Vec<NandiTrip>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NandiRoutesRes {
    pub id: String,
    #[serde(rename = "shortName")]
    pub short_name: Option<String>,
    #[serde(rename = "longName")]
    pub long_name: Option<String>,
    pub mode: String,
    #[serde(rename = "agencyName")]
    pub agency_name: Option<String>,
    #[serde(rename = "tripCount")]
    pub trip_count: Option<i32>,
    #[serde(rename = "stopCount")]
    pub stop_count: Option<i32>,
    #[serde(rename = "startPoint")]
    pub start_point: Option<LatLong>,
    #[serde(rename = "endPoint")]
    pub end_point: Option<LatLong>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteStopMapping {
    #[serde(rename = "estimatedTravelTimeFromPreviousStop")]
    pub estimated_travel_time_from_previous_stop: Option<i32>,
    #[serde(rename = "providerCode")]
    pub provider_code: String,
    #[serde(rename = "routeCode")]
    pub route_code: String,
    #[serde(rename = "sequenceNum")]
    pub sequence_num: i32,
    #[serde(rename = "stopCode")]
    pub stop_code: String,
    #[serde(rename = "stopName")]
    pub stop_name: String,
    #[serde(rename = "stopPoint")]
    pub stop_point: LatLong,
    #[serde(rename = "vehicleType")]
    pub vehicle_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GTFSStop {
    pub id: String,
    pub code: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    #[serde(rename = "stationId")]
    pub station_id: Option<String>,
    pub cluster: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GTFSData {
    pub routes: BTreeMap<String, NandiRoutesRes>,
    pub route_stop_map: BTreeMap<String, BTreeMap<String, Vec<RouteStopMapping>>>,
    pub stop_route_map: BTreeMap<String, BTreeMap<String, Vec<RouteStopMapping>>>,
    pub routes_by_gtfs: BTreeMap<String, BTreeMap<String, NandiRoutesRes>>,
    pub children_by_parent: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    pub data_hash: BTreeMap<String, String>,
}

impl GTFSData {
    pub fn new() -> Self {
        Self {
            routes: BTreeMap::new(),
            route_stop_map: BTreeMap::new(),
            stop_route_map: BTreeMap::new(),
            routes_by_gtfs: BTreeMap::new(),
            children_by_parent: BTreeMap::new(),
            data_hash: BTreeMap::new(),
        }
    }

    pub fn update_data(&mut self, temp_data: GTFSData) {
        self.routes = temp_data.routes;
        self.route_stop_map = temp_data.route_stop_map;
        self.stop_route_map = temp_data.stop_route_map;
        self.routes_by_gtfs = temp_data.routes_by_gtfs;
        self.children_by_parent = temp_data.children_by_parent;
        self.data_hash = temp_data.data_hash;
    }
}

impl Default for GTFSData {
    fn default() -> Self {
        Self::new()
    }
}

pub fn cast_vehicle_type(vehicle_type: &str) -> String {
    if vehicle_type == "RAIL" {
        "METRO".to_string()
    } else {
        vehicle_type.to_string()
    }
}

pub fn clean_identifier(identifier: &str) -> String {
    // URL decode and remove GTFS ID prefix if present
    let decoded = urlencoding::decode(identifier)
        .unwrap_or_else(|_| identifier.to_string().into());
    
    // Remove GTFS ID prefix if present (format: gtfs_id:code)
    decoded.split(':').last().unwrap_or(&decoded).to_string()
}
