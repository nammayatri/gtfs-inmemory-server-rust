use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use utoipa::ToSchema;

/// An identifier column that may live as either an integer or text.
///
/// The operator `*_internal` id columns are mid-migration from `bigint` to
/// `text`: in prod most are still `bigint`, locally most are already `text`, and
/// values are either numeric strings (`"5"`) or, eventually, UUID strings.
///
/// - **Reads** tolerate both: serde deserializes from a JSON number *or* string,
///   and sqlx decodes from a Postgres `int2/int4/int8` *or* `text/varchar`
///   column. So GIMS never 500s on the bigint/text split.
/// - **Writes (serialize) are numeric-coalescing**: emit a JSON *number* whenever
///   the id is numerically representable (a real int, or a numeric string like
///   `"5"`), and a string only for genuinely non-numeric ids (UUIDs). This keeps
///   a still-deployed `Int64`-typed consumer (the rider-app backend, mid-move to
///   Aeson `Value`) decoding successfully for as long as every id is numeric —
///   which is the case until the frontend starts minting UUIDs. It also makes the
///   mixed schema consistent: the same logical id serializes identically whether
///   its column is `bigint` or already-migrated `text`.
///
/// Caveat: coalescing a numeric string drops leading zeros (`"007"` -> `7`).
/// That's fine for sequence/UUID `*_id` columns but would corrupt a zero-padded
/// code — none of the id columns are such.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IdValue {
    Int(i64),
    Text(String),
}

impl std::fmt::Display for IdValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdValue::Int(n) => write!(f, "{}", n),
            IdValue::Text(s) => write!(f, "{}", s),
        }
    }
}

impl Serialize for IdValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            IdValue::Int(n) => serializer.serialize_i64(*n),
            IdValue::Text(s) => match s.parse::<i64>() {
                Ok(n) => serializer.serialize_i64(n),
                Err(_) => serializer.serialize_str(s),
            },
        }
    }
}

impl<'de> Deserialize<'de> for IdValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct IdValueVisitor;
        impl<'de> serde::de::Visitor<'de> for IdValueVisitor {
            type Value = IdValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an integer or string id")
            }
            fn visit_i64<E>(self, v: i64) -> Result<IdValue, E> {
                Ok(IdValue::Int(v))
            }
            fn visit_u64<E>(self, v: u64) -> Result<IdValue, E> {
                Ok(IdValue::Int(v as i64))
            }
            fn visit_f64<E>(self, v: f64) -> Result<IdValue, E>
            where
                E: serde::de::Error,
            {
                // ids are never fractional; a whole float is tolerated
                Ok(IdValue::Int(v as i64))
            }
            fn visit_str<E>(self, v: &str) -> Result<IdValue, E> {
                Ok(IdValue::Text(v.to_string()))
            }
            fn visit_string<E>(self, v: String) -> Result<IdValue, E> {
                Ok(IdValue::Text(v))
            }
        }
        deserializer.deserialize_any(IdValueVisitor)
    }
}

impl sqlx::Type<sqlx::Postgres> for IdValue {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }
    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <String as sqlx::Type<sqlx::Postgres>>::compatible(ty)
            || <i64 as sqlx::Type<sqlx::Postgres>>::compatible(ty)
            || <i32 as sqlx::Type<sqlx::Postgres>>::compatible(ty)
            || <i16 as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Postgres> for IdValue {
    fn decode(
        value: sqlx::postgres::PgValueRef<'r>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        use sqlx::{TypeInfo, ValueRef};
        match value.type_info().name() {
            "INT8" => Ok(IdValue::Int(<i64 as sqlx::Decode<sqlx::Postgres>>::decode(
                value,
            )?)),
            "INT4" => Ok(IdValue::Int(
                <i32 as sqlx::Decode<sqlx::Postgres>>::decode(value)? as i64,
            )),
            "INT2" => Ok(IdValue::Int(
                <i16 as sqlx::Decode<sqlx::Postgres>>::decode(value)? as i64,
            )),
            _ => Ok(IdValue::Text(
                <String as sqlx::Decode<sqlx::Postgres>>::decode(value)?,
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Gate {
    #[serde(rename = "gateName")]
    pub gate_name: String,
    #[serde(rename = "stopCode")]
    pub stop_code: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Default, ToSchema)]
#[schema(as = String, example = "ORDINARY")]
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
    SHUTTLE,
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
            ServiceTierType::SHUTTLE => "SHUTTLE",
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
            "SHUTTLE" | "Shuttle" => Ok(ServiceTierType::SHUTTLE),
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

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
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
    #[sqlx(default)]
    pub is_active_trip: Option<bool>,
    #[sqlx(default)]
    pub is_completed: Option<bool>,
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
    pub schedule_details: Option<HashMap<String, Vec<BusSchedule>>>,
    pub db_start_time: Option<String>,
    pub db_end_time: Option<String>,
    #[sqlx(default)]
    #[serde(rename = "seatLayoutId")]
    pub seat_layout_id: Option<String>,
    #[sqlx(default)]
    #[serde(skip)]
    pub waybill_status: Option<WaybillStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct DepotVehicleSummary {
    pub fleet_no: String,
    pub status: Option<String>,
    pub vehicle_no: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct VehicleOperationData {
    pub waybill_id: Option<String>,
    pub waybill_no: Option<String>,
    pub depot_id: String,
    pub conductor_code: Option<String>,
    pub driver_code: Option<String>,
    pub schedule_no: Option<String>,
    pub depot_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
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
    pub schedule_trip_id: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct VehicleServiceTypeResponse {
    pub vehicle_no: String,
    pub service_type: Option<String>,
    /// Deprecated: holds the waybill *number*, not the waybill id. Use `waybill_no`.
    pub waybill_id: Option<String>,
    pub waybill_no: Option<String>,
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
    #[serde(rename = "busTagNumber")]
    pub bus_tag_number: Option<String>,
    pub waybill_status: Option<WaybillStatus>,
    pub is_historic: bool,
    pub schedule_based_active_trip: Option<bool>,
    /// Start time of the first/active trip (for schedule-based reconciliation)
    #[serde(rename = "dbStartTime")]
    pub db_start_time: Option<String>,
    /// End time of the first/active trip (for schedule-based reconciliation)
    #[serde(rename = "dbEndTime")]
    pub db_end_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct VehicleMetadataResponse {
    #[serde(rename = "serviceType")]
    pub service_type: Option<String>,
    #[serde(rename = "serviceSubTypes")]
    pub service_sub_types: Option<Vec<String>>,
    #[serde(rename = "busTagNumber")]
    pub bus_tag_number: Option<String>,
    pub is_actually_valid: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WaybillMetadataResponse {
    pub waybill_no: String,
    pub vehicle_no: String,
    #[serde(rename = "serviceType")]
    pub service_type: String,
    pub driver_id: Option<String>,
    #[serde(rename = "driverName")]
    pub driver_name: Option<String>,
    #[serde(rename = "driverMobileNumber")]
    pub driver_mobile_number: Option<String>,
    #[serde(rename = "busTagNumber")]
    pub bus_tag_number: Option<String>,
}

/// Granular, update-only body for editing a waybill's mutable operational fields (crew, fleet, devices)
/// and optionally its status. Every field is optional -> only provided (non-null) fields are written
/// (COALESCE), and identity/schedule columns (schedule_no, trip_name, duty_date, ...) are never touched,
/// so they are immutable by construction. `waybill_id` selects the row (its internal PK).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateWaybillDetailsBody {
    #[schema(value_type = String)]
    pub waybill_id: IdValue,
    pub vehicle_no: Option<String>,
    pub driver_token_no: Option<String>,
    pub driver_name: Option<String>,
    pub conductor_token_no: Option<String>,
    pub conductor_name: Option<String>,
    pub no_of_device: Option<i64>,
    pub device_serial_number: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WaybillTripInfo {
    pub waybill_no: String,
    pub vehicle_no: String,
    pub service_type: String,
    pub driver_token_no: Option<String>,
    pub driver_first_name: Option<String>,
    pub driver_last_name: Option<String>,
    pub driver_mobile_number: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum WaybillStatus {
    Online,
    Processed,
    New,
    Closed,
    Audited,
    NotFound,
}

impl WaybillStatus {
    pub fn from_db_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "online" => Self::Online,
            "processed" => Self::Processed,
            "new" => Self::New,
            "closed" => Self::Closed,
            "audited" => Self::Audited,
            _ => Self::NotFound,
        }
    }

    pub fn is_historic(&self) -> bool {
        matches!(self, Self::Closed | Self::Audited)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LatLong {
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NandiStop {
    pub id: String,
    pub code: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Example-trip schedule times (seconds since midnight) from the
    /// preprocessor's patterns.json. Absent in OTP responses and in older
    /// preprocessed data, so default to None.
    #[serde(rename = "arrivalTime", default)]
    pub arrival_time: Option<i32>,
    #[serde(rename = "departureTime", default)]
    pub departure_time: Option<i32>,
    #[serde(rename = "stopSequence", default)]
    pub stop_sequence: Option<i32>,
    /// GTFS platform_code for this stop (verbatim from the preprocessor's
    /// patterns.json / stops.txt). Absent in OTP responses and in older
    /// preprocessed data, so default to None. Mirrors the platformCode the
    /// GraphQL example-trip path reads from OTP.
    #[serde(rename = "platformCode", default)]
    pub platform_code: Option<String>,
    /// stop_headsign verbatim from GTFS. For Chennai-style feeds this is a
    /// Python-dict-shaped string like
    /// "{'fareStageNumber': '3', 'isStageStop': true}" that the backend's
    /// TripStopDetail FromJSON sanitizes into ExtraInfo. The FRFS fare-stage
    /// lookup (getFareThroughGTFS) needs this field — without it, PREMIUM bus
    /// quotes come back empty.
    #[serde(default)]
    pub headsign: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NandiTrip {
    pub id: String,
    pub direction: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NandiPattern {
    pub id: String,
    pub desc: String,
    #[serde(rename = "routeId")]
    pub route_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NandiPatternDetails {
    pub id: String,
    pub desc: Option<String>,
    #[serde(rename = "routeId")]
    pub route_id: String,
    pub stops: Vec<NandiStop>,
    pub trips: Vec<NandiTrip>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NandiRoutesRes {
    pub id: String,
    #[serde(rename = "shortName")]
    pub short_name: Option<String>,
    #[serde(rename = "longName")]
    pub long_name: Option<String>,
    pub mode: String,
    #[serde(rename = "agencyName")]
    pub agency_name: Option<String>,
    /// GTFS route_color, normalized to "#RRGGBB" by the preprocessor. Absent
    /// in OTP responses and in older preprocessed data, so default to None.
    #[serde(default)]
    pub color: Option<String>,
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
    #[serde(
        rename = "encodedPolyline",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub encoded_polyline: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RouteStopMapping {
    #[serde(rename = "estimatedTravelTimeFromPreviousStop")]
    pub estimated_travel_time_from_previous_stop: Option<i32>,
    #[serde(rename = "providerCode")]
    #[schema(value_type = String)]
    pub provider_code: Arc<str>,
    #[serde(rename = "routeCode")]
    #[schema(value_type = String)]
    pub route_code: Arc<str>,
    #[serde(rename = "sequenceNum")]
    pub sequence_num: i32,
    #[serde(rename = "stopCode")]
    #[schema(value_type = String)]
    pub stop_code: Arc<str>,
    #[serde(rename = "stopName")]
    #[schema(value_type = String)]
    pub stop_name: Arc<str>,
    #[serde(rename = "stopPoint")]
    pub stop_point: LatLong,
    #[serde(rename = "vehicleType")]
    #[schema(value_type = String)]
    pub vehicle_type: Arc<str>,
    #[serde(rename = "geoJson")]
    #[schema(value_type = Option<Object>)]
    pub geo_json: Option<serde_json::Value>,
    #[serde(rename = "gates")]
    pub gates: Option<Vec<Gate>>,
    #[serde(rename = "hindiName")]
    #[schema(value_type = Option<String>)]
    pub hindi_name: Option<Arc<str>>,
    #[serde(rename = "regionalName")]
    #[schema(value_type = Option<String>)]
    pub regional_name: Option<Arc<str>>,
    #[serde(rename = "platform")]
    #[schema(value_type = Option<String>)]
    pub platform: Option<Arc<str>>,
    #[serde(rename = "parentStopCode")]
    #[schema(value_type = Option<String>)]
    pub parent_stop_code: Option<Arc<str>>,
    #[serde(rename = "clusterId")]
    #[schema(value_type = Option<String>)]
    pub cluster_id: Option<Arc<str>>,
}

/// One direct route connecting a source cluster to a destination cluster, with
/// the specific stop_codes on that route that each cluster resolves to.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ClusterRouteConnection {
    #[serde(rename = "routeCode")]
    pub route_code: String,
    #[serde(rename = "sourceStopCode")]
    pub source_stop_code: String,
    #[serde(rename = "sourceSequenceNum")]
    pub source_sequence_num: i32,
    #[serde(rename = "destinationStopCode")]
    pub destination_stop_code: String,
    #[serde(rename = "destinationSequenceNum")]
    pub destination_sequence_num: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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
    #[serde(rename = "infoJson", default, skip_serializing)]
    pub info_json: Option<String>,
    #[serde(rename = "clusterId", default, skip_serializing_if = "Option::is_none")]
    pub cluster_id: Option<String>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GTFSRouteData {
    pub mappings: Vec<Arc<RouteStopMapping>>,
    pub by_route: HashMap<String, Vec<usize>>,
    pub by_stop: HashMap<String, Vec<usize>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GTFSStopData {
    pub stops: HashMap<String, GTFSStop>,
    #[serde(default)]
    pub by_cluster_id: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GTFSAlternateStopData {
    pub alternate_stops: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStopCodeRecord {
    pub gtfs_id: String,
    pub provider_stop_code: String,
    pub stop_code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StopCodeFromProviderStopCodeResponse {
    pub stop_code: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CachedDataResponse {
    pub route_data_by_gtfs: HashMap<String, GTFSRouteData>,
    pub stops_by_gtfs: HashMap<String, GTFSStopData>,
    pub stop_geojsons_by_gtfs: HashMap<String, HashMap<String, StopGeojson>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
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
    pub alternate_stop_by_gtfs: HashMap<String, GTFSAlternateStopData>,
    pub route_service_tiers_by_gtfs: HashMap<String, HashMap<String, ServiceTierType>>,
    pub seat_layout_mapping_by_gtfs: HashMap<String, HashMap<String, String>>,
    /// Pre-computed unique stops list per GTFS (avoids recomputation on every /stops request)
    #[serde(skip)]
    pub pre_computed_stops_by_gtfs: HashMap<String, Vec<Arc<RouteStopMapping>>>,
}

impl GTFSData {
    pub fn new() -> Self {
        Self::default()
    }

    /// Estimate heap memory usage in bytes for monitoring
    pub fn memory_usage_bytes(&self) -> MemoryUsageStats {
        let mut stats = MemoryUsageStats::default();

        // Routes
        for (gtfs_id, routes) in &self.routes_by_gtfs {
            stats.routes_bytes += gtfs_id.len() + 24; // String overhead
            for (code, route) in routes {
                stats.routes_bytes += code.len() + 24;
                stats.routes_bytes += route.id.len() + 24;
                stats.routes_bytes += route.short_name.as_ref().map_or(0, |s| s.len() + 24);
                stats.routes_bytes += route.long_name.as_ref().map_or(0, |s| s.len() + 24);
                stats.routes_bytes += route.mode.len() + 24;
                stats.routes_bytes += 64; // LatLong, counts, etc.
            }
        }

        // Route data (mappings + indices)
        for route_data in self.route_data_by_gtfs.values() {
            // Each Arc<RouteStopMapping> pointer is 8 bytes
            stats.route_data_bytes += route_data.mappings.len() * 8;
            // Estimate each RouteStopMapping's heap size
            for mapping in &route_data.mappings {
                stats.route_data_bytes += mapping.stop_code.len()
                    + mapping.stop_name.len()
                    + mapping.route_code.len()
                    + mapping.vehicle_type.len()
                    + mapping.provider_code.len();
                stats.route_data_bytes += mapping.hindi_name.as_ref().map_or(0, |s| s.len());
                stats.route_data_bytes += mapping.regional_name.as_ref().map_or(0, |s| s.len());
                stats.route_data_bytes += mapping.platform.as_ref().map_or(0, |s| s.len());
                stats.route_data_bytes += mapping.parent_stop_code.as_ref().map_or(0, |s| s.len());
                // Arc overhead (strong + weak counts) + struct fields
                stats.route_data_bytes += 16 + 64;
                // geo_json estimate
                if mapping.geo_json.is_some() {
                    stats.route_data_bytes += 256; // rough estimate
                }
            }
            // Index maps
            for (key, indices) in &route_data.by_route {
                stats.route_data_bytes += key.len() + 24 + indices.len() * 8;
            }
            for (key, indices) in &route_data.by_stop {
                stats.route_data_bytes += key.len() + 24 + indices.len() * 8;
            }
        }

        // Stops
        for stop_data in self.stops_by_gtfs.values() {
            for (code, stop) in &stop_data.stops {
                stats.stops_bytes += code.len() + 24;
                stats.stops_bytes += stop.id.len() + stop.code.len() + stop.name.len() + 72;
                stats.stops_bytes += stop.station_id.as_ref().map_or(0, |s| s.len() + 24);
                stats.stops_bytes += stop.cluster.as_ref().map_or(0, |s| s.len() + 24);
                stats.stops_bytes += 16; // lat, lon
            }
        }

        // Pre-computed stops (Arc pointers only, data shared with route_data)
        for stops in self.pre_computed_stops_by_gtfs.values() {
            stats.pre_computed_stops_bytes += stops.len() * 8; // Arc pointers
        }

        // Children mapping
        for parents in self.children_by_parent.values() {
            for (parent, children) in parents {
                stats.children_bytes += parent.len() + 24;
                for child in children {
                    stats.children_bytes += child.len() + 24;
                }
            }
        }

        // Stop geojsons (rough estimate)
        for geojsons in self.stop_geojsons_by_gtfs.values() {
            stats.geojson_bytes += geojsons.len() * 512; // rough estimate per geojson
        }

        // Provider mapping
        for mapping in self.provider_stop_code_mapping.values() {
            for (k, v) in mapping {
                stats.provider_mapping_bytes += k.len() + v.len() + 48;
            }
        }

        stats.total_bytes = stats.routes_bytes
            + stats.route_data_bytes
            + stats.stops_bytes
            + stats.pre_computed_stops_bytes
            + stats.children_bytes
            + stats.geojson_bytes
            + stats.provider_mapping_bytes;

        stats
    }
}

#[derive(Debug, Default, Clone, Serialize, ToSchema)]
pub struct MemoryUsageStats {
    pub total_bytes: usize,
    pub routes_bytes: usize,
    pub route_data_bytes: usize,
    pub stops_bytes: usize,
    pub pre_computed_stops_bytes: usize,
    pub children_bytes: usize,
    pub geojson_bytes: usize,
    pub provider_mapping_bytes: usize,
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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
    #[schema(value_type = Object)]
    pub headsign: serde_json::Value,
    #[serde(rename = "stopPosition")]
    pub stop_position: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BusScheduleDetail {
    pub eta: Vec<BusStopETA>,
    #[serde(rename = "vehicle_no")]
    pub vehicle_no: String,
    #[serde(rename = "service_tier")]
    pub service_tier: String,
    #[serde(rename = "trip_number")]
    pub trip_number: Option<i32>,
    #[serde(rename = "is_active_trip")]
    pub is_active_trip: Option<bool>,
    #[serde(rename = "waybill_no")]
    pub waybill_no: Option<String>,
    #[serde(rename = "is_completed", skip_serializing_if = "Option::is_none")]
    pub is_completed: Option<bool>,
}

pub type BusScheduleDetails = Vec<BusScheduleDetail>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct RouteLastScheduleTime {
    #[serde(rename = "routeId")]
    pub route_id: String,
    // The SQL `MAX(end_time)` can return NULL when end_time is NULL for every
    // row in a route's group. We keep the field nullable internally (sqlx
    // requires it to match the SQL column nullability) but serialize as ""
    // so the backend's `Text`-typed Aeson decoder doesn't fail on null.
    #[serde(
        rename = "lastScheduleTime",
        serialize_with = "serialize_option_as_empty_string"
    )]
    pub last_schedule_time: Option<String>,
}

pub fn serialize_option_as_empty_string<S>(
    value: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(value.as_deref().unwrap_or(""))
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
