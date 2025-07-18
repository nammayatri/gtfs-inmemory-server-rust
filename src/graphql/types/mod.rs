use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripQuery {
    pub trip_id: String,
    pub gtfs_id: Option<String>,
    pub city: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripQueryParams {
    pub gtfs_id: Option<String>,
    pub city: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripData {
    #[serde(rename = "tripId")]
    pub trip_id: String,
    #[serde(rename = "routeId")]
    pub route_id: String,
    pub direction: Option<i32>,
    pub stops: Vec<TripStop>,
    pub schedule: Vec<TripSchedule>,
    #[serde(rename = "lastUpdated")]
    pub last_updated: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripStop {
    #[serde(rename = "stopId")]
    pub stop_id: String,
    #[serde(rename = "stopCode")]
    pub stop_code: String,
    #[serde(rename = "stopName")]
    pub stop_name: String,
    pub sequence: i32,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripSchedule {
    pub stop_id: String,
    #[serde(rename = "arrivalTime")]
    pub arrival_time: Option<i32>,
    #[serde(rename = "departureTime")]
    pub departure_time: Option<i32>,
    pub sequence: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripQueryVariables {
    pub trip_id: String,
    pub service_date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripGraphQLError {
    pub message: String,
    pub locations: Option<Vec<TripGraphQLLocation>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripGraphQLLocation {
    pub line: i32,
    pub column: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripGraphQLResponse {
    pub data: Option<TripGraphQLData>,
    pub errors: Option<Vec<TripGraphQLError>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripGraphQLData {
    pub trip: Option<TripGraphQLTrip>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripGraphQLTrip {
    #[serde(rename = "gtfsId")]
    pub gtfs_id: String,
    pub route: TripGraphQLRoute,
    #[serde(rename = "stoptimesForDate")]
    pub stoptimes_for_date: Vec<TripGraphQLStopTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripGraphQLRoute {
    pub id: String,
    #[serde(rename = "gtfsId")]
    pub gtfs_id: String,
    #[serde(rename = "longName")]
    pub long_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripGraphQLStopTime {
    pub stop: TripGraphQLStop,
    #[serde(rename = "stopPosition")]
    pub stop_sequence: i32,
    #[serde(rename = "realtimeArrival")]
    pub realtime_arrival: Option<i32>,
    #[serde(rename = "scheduledArrival")]
    pub scheduled_arrival: Option<i32>,
    #[serde(rename = "scheduledDeparture")]
    pub scheduled_departure: Option<i32>,
    #[serde(rename = "realtimeDeparture")]
    pub realtime_departure: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripGraphQLStop {
    pub id: String,
    pub code: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}
