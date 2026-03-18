use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gate {
    #[serde(rename = "gateName")]
    pub gate_name: String,
    #[serde(rename = "stopCode")]
    pub stop_code: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Default)]
pub enum ServiceTierType {
    #[default]
    Ordinary,
    Ac,
    NonAc,
    Express,
    Special,
    Executive,
    FirstClass,
    SecondClass,
    ThirdClass,
    AshokLeylandAc,
    MidiAc,
    VolvoAc,
    ElectricV,
    ElectricVPmi,
    AcEmuFirstClass,
    PREMIUM,
}

impl Serialize for ServiceTierType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = match self {
            ServiceTierType::Ordinary => "ORDINARY",
            ServiceTierType::Ac => "AC",
            ServiceTierType::NonAc => "NON_AC",
            ServiceTierType::Express => "EXPRESS",
            ServiceTierType::Special => "SPECIAL",
            ServiceTierType::Executive => "EXECUTIVE",
            ServiceTierType::FirstClass => "FIRST_CLASS",
            ServiceTierType::SecondClass => "SECOND_CLASS",
            ServiceTierType::ThirdClass => "THIRD_CLASS",
            ServiceTierType::AshokLeylandAc => "ASHOK_LEYLAND_AC",
            ServiceTierType::MidiAc => "MIDI_AC",
            ServiceTierType::VolvoAc => "VOLVO_AC",
            ServiceTierType::ElectricV => "ELECTRIC_V",
            ServiceTierType::ElectricVPmi => "ELECTRIC_V_PMI",
            ServiceTierType::AcEmuFirstClass => "AC_EMU_FIRST_CLASS",
            ServiceTierType::PREMIUM => "PREMIUM",
        };
        serializer.serialize_str(s)
    }
}

impl<'de> Deserialize<'de> for ServiceTierType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: String = Deserialize::deserialize(deserializer)?;
        match s.trim() {
            "Deluxe EV" => Ok(ServiceTierType::Executive),
            "Small Bus Express" => Ok(ServiceTierType::Express),
            "Small Bus Ordinary" => Ok(ServiceTierType::NonAc),
            "A/C" | "A/C EV" | "AC" => Ok(ServiceTierType::Ac),
            "Ordinary" | "ORDINARY" => Ok(ServiceTierType::Ordinary),
            "Express" | "EXPRESS" => Ok(ServiceTierType::Express),
            "Deluxe" | "EXECUTIVE" => Ok(ServiceTierType::Executive),
            "NON_AC" => Ok(ServiceTierType::NonAc),
            "SPECIAL" => Ok(ServiceTierType::Special),
            "FIRST_CLASS" => Ok(ServiceTierType::FirstClass),
            "SECOND_CLASS" => Ok(ServiceTierType::SecondClass),
            "THIRD_CLASS" => Ok(ServiceTierType::ThirdClass),
            "ASHOK_LEYLAND_AC" => Ok(ServiceTierType::AshokLeylandAc),
            "MIDI_AC" => Ok(ServiceTierType::MidiAc),
            "VOLVO_AC" => Ok(ServiceTierType::VolvoAc),
            "ELECTRIC_V" => Ok(ServiceTierType::ElectricV),
            "ELECTRIC_V_PMI" => Ok(ServiceTierType::ElectricVPmi),
            "AC_EMU_FIRST_CLASS" => Ok(ServiceTierType::AcEmuFirstClass),
            "PREMIUM" | "Premium" => Ok(ServiceTierType::PREMIUM),
            _ => {
                // Return Ordinary as default if parsing fails like some lenient setups or log warning? Let's just fail
                Err(serde::de::Error::custom(format!(
                    "Invalid Service Tier Type: {}",
                    s
                )))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteServiceTierRecord {
    pub gtfs_id: String,
    pub route_id: String,
    pub servicetier: ServiceTierType,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct VehicleData {
    pub waybill_id: String,
    pub waybill_no: String,
    pub service_type: String,
    pub vehicle_no: String,
    pub schedule_no: String,
    pub last_updated: Option<DateTime<Utc>>,
    pub duty_date: Option<String>,
    pub schedule_trip_id: Option<String>,
    pub entity_remark: Option<String>,
    pub driver_code: Option<String>,
    pub conductor_code: Option<String>,
    pub deleted: Option<bool>,
    pub status: Option<String>,
    pub is_flexi: Option<bool>,
    #[sqlx(default)]
    #[serde(skip)]
    pub db_start_time: Option<String>,
    #[sqlx(default)]
    #[serde(skip)]
    pub start_time_epoch: Option<String>,
    #[sqlx(default)]
    pub trip_number: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MinimalVehicleData {
    pub service_type: String,
    pub vehicle_no: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct VehicleDataWithRouteId {
    pub waybill_id: Option<String>,
    pub waybill_no: Option<String>,
    pub service_type: Option<String>,
    pub vehicle_no: String,
    pub schedule_no: Option<String>,
    pub last_updated: Option<DateTime<Utc>>,
    pub duty_date: Option<String>,
    pub route_id: Option<String>,
    pub route_number: Option<String>,
    pub depot: Option<String>,
    pub trip_number: Option<i32>,
    pub is_active_trip: bool,
    pub remaining_trip_details: Option<Vec<BusSchedule>>,
    pub entity_remark: Option<String>,
    pub driver_code: Option<String>,
    pub conductor_code: Option<String>,
    pub deleted: Option<bool>,
    pub status: Option<String>,
    pub schedule_details: Option<HashMap<i64, Vec<BusSchedule>>>,
    pub db_start_time: Option<String>,
    pub db_end_time: Option<String>,
    #[sqlx(default)]
    #[serde(rename = "seatLayoutId")]
    pub seat_layout_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct DepotVehicleSummary {
    pub fleet_no: String,
    pub status: Option<String>,
    pub vehicle_no: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct VehicleOperationData {
    pub waybill_id: Option<String>,
    pub waybill_no: Option<String>,
    pub depot_id: String,
    pub conductor_code: Option<String>,
    pub driver_code: Option<String>,
    pub schedule_no: Option<String>,
    pub depot_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct BusSchedule {
    pub schedule_number: String,
    pub route_id: String,
    #[sqlx(default)]
    pub route_name: Option<String>,
    pub org_name: Option<String>,
    pub trip_number: Option<i32>,
    #[sqlx(default)]
    pub route_number: Option<String>,
    #[sqlx(default)]
    pub stops_count: Option<i32>,
    #[sqlx(default)]
    pub is_active_trip: Option<bool>,
    #[serde(rename = "scheduleTripId")]
    pub schedule_trip_id: Option<i64>,
    #[serde(rename = "startTime")]
    pub start_time: Option<String>,
    #[serde(rename = "endTime")]
    pub end_time: Option<String>,
    pub deleted: Option<bool>,
    #[serde(rename = "tripOrder")]
    pub trip_order: Option<i32>,
    #[serde(rename = "dbStartTime")]
    pub db_start_time: Option<String>,
    #[serde(rename = "dbEndTime")]
    pub db_end_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VehicleServiceTypeResponse {
    pub vehicle_no: String,
    pub service_type: Option<String>,
    pub waybill_id: Option<String>,
    pub schedule_no: Option<String>,
    pub last_updated: Option<DateTime<Utc>>,
    pub route_id: Option<String>,
    pub route_number: Option<String>,
    pub is_active_trip: bool,
    pub trip_number: Option<i32>,
    #[serde(rename = "depot")]
    pub depot_no: Option<String>,
    pub remaining_trip_details: Option<Vec<BusSchedule>>,
    pub is_actually_valid: Option<bool>,
    pub driver_id: Option<String>,
    pub conductor_id: Option<String>,
    pub eligible_pass_ids: Option<Vec<String>>,
    pub service_sub_types: Option<Vec<String>>,
    #[serde(rename = "seatLayoutId")]
    pub seat_layout_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WaybillStatus {
    Online,
    ProcessedOrNew,
    NotFound,
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
    #[serde(rename = "serviceTierType")]
    pub service_tier_type: Option<ServiceTierType>,
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
    #[serde(rename = "geoJson")]
    pub geo_json: Option<serde_json::Value>,
    #[serde(rename = "gates")]
    pub gates: Option<Vec<Gate>>,
    #[serde(rename = "hindiName")]
    pub hindi_name: Option<String>,
    #[serde(rename = "regionalName")]
    pub regional_name: Option<String>,
    #[serde(rename = "platform")]
    pub platform: Option<String>,
    #[serde(rename = "parentStopCode")]
    pub parent_stop_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stop {
    #[serde(rename = "stopCode")]
    pub stop_code: String,
    #[serde(rename = "stopPoint")]
    pub stop_point: LatLong,
    #[serde(rename = "stopName")]
    pub stop_name: String,
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
    #[serde(rename = "hindiName")]
    pub hindi_name: Option<String>,
    #[serde(rename = "regionalName")]
    pub regional_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopGeojsonRecord {
    pub stop_code: String,
    pub gtfs_id: String,
    pub geo_json: serde_json::Value,
    #[serde(deserialize_with = "deserialize_gates_from_json_str")]
    pub gates: Option<Vec<Gate>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopGeojson {
    pub geo_json: serde_json::Value,
    pub gates: Option<Vec<Gate>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GTFSRouteData {
    pub mappings: Vec<Arc<RouteStopMapping>>,
    pub by_route: HashMap<String, Vec<usize>>,
    pub by_stop: HashMap<String, Vec<usize>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GTFSStopData {
    pub stops: HashMap<String, GTFSStop>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GTFSAlternateStopData {
    pub alternate_stops: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStopCodeRecord {
    pub gtfs_id: String,
    pub provider_stop_code: String,
    pub stop_code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopCodeFromProviderStopCodeResponse {
    pub stop_code: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CachedDataResponse {
    pub route_data_by_gtfs: HashMap<String, GTFSRouteData>,
    pub stops_by_gtfs: HashMap<String, GTFSStopData>,
    pub stop_geojsons_by_gtfs: HashMap<String, HashMap<String, StopGeojson>>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct GTFSData {
    pub routes_by_gtfs: HashMap<String, HashMap<String, NandiRoutesRes>>,
    pub route_data_by_gtfs: HashMap<String, GTFSRouteData>,
    pub stops_by_gtfs: HashMap<String, GTFSStopData>,
    pub children_by_parent: HashMap<String, HashMap<String, HashSet<String>>>,
    pub data_hash: HashMap<String, String>,
    pub stop_geojsons_by_gtfs: HashMap<String, HashMap<String, StopGeojson>>,
    pub provider_stop_code_mapping: HashMap<String, HashMap<String, String>>,
    pub stop_regional_names_by_gtfs: HashMap<String, HashMap<String, StopRegionalNameRecord>>,
    pub suburban_stop_info_by_gtfs: HashMap<String, HashMap<String, SuburbanStopInfo>>,
    pub route_example_trip_by_gtfs: HashMap<String, HashMap<String, String>>,
    pub static_fleet_info_by_gtfs: HashMap<String, HashMap<String, StaticFleetInfo>>,
    pub entity_id_name_mapping: HashMap<String, String>,
    pub route_example_trip_details_by_gtfs: HashMap<String, HashMap<String, TripDetails>>,
    pub alternate_stop_by_gtfs: HashMap<String, GTFSAlternateStopData>,
    pub route_service_tiers_by_gtfs: HashMap<String, HashMap<String, ServiceTierType>>,
    pub seat_layout_mapping_by_gtfs: HashMap<String, HashMap<String, String>>,
}

impl GTFSData {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopRegionalNameRecord {
    pub gtfs_id: String,
    pub stop_code: String,
    pub stop_name: String,
    pub hindi_name: String,
    pub regional_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuburbanStopInfoRecord {
    pub gtfs_id: String,
    pub stop_id: String,
    #[serde(rename = "Location Name")]
    pub location_name: String,
    pub platforms: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformInfo {
    pub platforms: String,
    pub direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuburbanStopInfo {
    pub stop_id: String,
    pub location_name: String,
    pub platforms: Vec<PlatformInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripStopDetail {
    #[serde(rename = "stopId")]
    pub stop_id: String,
    #[serde(rename = "stopCode")]
    pub stop_code: String,
    #[serde(rename = "stopName")]
    pub stop_name: Option<String>,
    #[serde(rename = "platformCode")]
    pub platform_code: Option<String>,
    pub lat: f64,
    pub lon: f64,
    #[serde(rename = "scheduledArrival")]
    pub scheduled_arrival: i32,
    #[serde(rename = "scheduledDeparture")]
    pub scheduled_departure: i32,
    #[serde(rename = "headsign")]
    pub headsign: serde_json::Value,
    #[serde(rename = "stopPosition")]
    pub stop_position: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripDetails {
    #[serde(rename = "tripId")]
    pub trip_id: String,
    pub stops: Vec<TripStopDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticFleetInfoRecord {
    pub gtfs_id: String,
    #[serde(alias = "vehicle_no", alias = "vehicleId", alias = "fleetId")]
    pub fleet_id: String,
    #[serde(default)]
    pub vehicle_type: Option<String>,
    #[serde(default)]
    pub capacity: Option<i32>,
    #[serde(default)]
    pub depot: Option<String>,
    #[serde(default)]
    pub service_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticFleetInfo {
    pub fleet_id: String,
    pub vehicle_type: Option<String>,
    pub capacity: Option<i32>,
    pub depot: Option<String>,
    pub service_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MinimalEmployee {
    pub token_no: Option<String>,
    pub first_name: String,
    pub last_name: Option<String>,
    pub mobile_no: Option<String>,
    pub depot_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepotManagerDetails {
    #[serde(rename = "depotCode")]
    pub depot_code: String,
    #[serde(rename = "depotName")]
    pub depot_name: String,
    #[serde(skip)]
    pub phone_number: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusRegistrationMappingRecord {
    pub gtfs_id: String,
    pub vehicle_no: String,
    pub short_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusStopETA {
    #[serde(rename = "stop_id")]
    pub stop_code: String,
    #[serde(rename = "arrival_time")]
    pub arrival_time: i64, // Epoch timestamp in seconds
    #[serde(rename = "eta_seconds")]
    pub eta_seconds: Option<i64>,
    #[serde(rename = "stop_name")]
    pub stop_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusScheduleDetail {
    pub eta: Vec<BusStopETA>,
    #[serde(rename = "vehicle_no")]
    pub vehicle_no: String,
    #[serde(rename = "service_tier")]
    pub service_tier: String,
    #[serde(rename = "trip_number")]
    pub trip_number: Option<i32>,
    #[serde(rename = "waybill_no")]
    pub waybill_no: Option<String>,
}

pub type BusScheduleDetails = Vec<BusScheduleDetail>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RouteLastScheduleTime {
    #[serde(rename = "routeId")]
    pub route_id: String,
    #[serde(rename = "lastScheduleTime")]
    pub last_schedule_time: Option<String>,
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
    let decoded = urlencoding::decode(identifier).unwrap_or_else(|_| identifier.to_string().into());

    // Remove GTFS ID prefix if present (format: gtfs_id:code)
    decoded
        .split(':')
        .next_back()
        .unwrap_or(&decoded)
        .to_string()
}

pub fn deserialize_gates_from_json_str<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<Gate>>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    match opt {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(&s)
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatLayoutMappingRecord {
    pub fleet_id: String,
    pub gtfs_id: String,
    pub seat_layout_id: String,
}

// ── Schedule / Reach-on-Time types ─────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepartureQuery {
    pub time: Option<String>,
    pub date: Option<String>,
    pub window_minutes: Option<u32>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArrivalQuery {
    pub time: Option<String>,
    pub date: Option<String>,
    pub window_minutes: Option<u32>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TripsBetweenRequest {
    pub origin_stop_code: String,
    pub destination_stop_code: String,
    pub date: Option<String>,
    pub depart_after: Option<String>,
    pub arrive_before: Option<String>,
    pub window_minutes: Option<u32>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NextServicesRequest {
    pub stop_code: String,
    pub date: Option<String>,
    pub time: Option<String>,
    pub modes: Option<Vec<String>>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledDeparture {
    pub trip_id: String,
    pub route_id: String,
    pub route_short_name: Option<String>,
    pub stop_code: String,
    pub stop_name: Option<String>,
    pub departure_time: String,
    pub headsign: Option<String>,
    pub stop_sequence: i32,
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledArrival {
    pub trip_id: String,
    pub route_id: String,
    pub route_short_name: Option<String>,
    pub stop_code: String,
    pub stop_name: Option<String>,
    pub arrival_time: String,
    pub headsign: Option<String>,
    pub stop_sequence: i32,
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledTrip {
    pub trip_id: String,
    pub route_id: String,
    pub route_short_name: Option<String>,
    pub origin_stop_code: String,
    pub origin_stop_name: Option<String>,
    pub destination_stop_code: String,
    pub destination_stop_name: Option<String>,
    pub departure_time: String,
    pub arrival_time: String,
    pub mode: Option<String>,
    pub num_stops: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GtfsDataQualityReport {
    pub gtfs_id: String,
    pub total_routes: usize,
    pub total_stops: usize,
    pub total_trips: usize,
    pub routes_with_trips: usize,
    pub routes_without_trips: usize,
    pub stops_with_departures: usize,
    pub stops_without_departures: usize,
    pub orphaned_stops: usize,
    pub data_hash: Option<String>,
    pub generated_at: String,
}
