use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripApiResponse {
    #[serde(rename = "tripId")]
    pub trip_id: String,
    #[serde(rename = "routeId")]
    pub route_id: String,
    #[serde(rename = "routeName")]
    pub route_name: Option<String>,
    #[serde(rename = "direction")]
    pub direction: Option<i32>,
    #[serde(rename = "stops")]
    pub stops: Vec<TripStopResponse>,
    #[serde(rename = "schedule")]
    pub schedule: Vec<TripScheduleResponse>,
    #[serde(rename = "lastUpdated")]
    pub last_updated: DateTime<Utc>,
    pub source: String, // "cache" or "graphql"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripStopResponse {
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
pub struct TripScheduleResponse {
    #[serde(rename = "stopCode")]
    pub stop_code: String,
    #[serde(rename = "arrivalTime")]
    pub arrival_time: Option<String>,
    #[serde(rename = "departureTime")]
    pub departure_time: Option<String>,
    pub sequence: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripCacheEntry {
    #[serde(rename = "tripData")]
    pub trip_data: TripApiResponse,
    #[serde(rename = "cachedAt")]
    pub cached_at: DateTime<Utc>,
    #[serde(rename = "expiresAt")]
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TripCacheStats {
    #[serde(rename = "totalCachedTrips")]
    pub total_cached_trips: usize,
    #[serde(rename = "cacheHits")]
    pub cache_hits: u64,
    #[serde(rename = "cacheMisses")]
    pub cache_misses: u64,
    #[serde(rename = "lastCacheCleanup")]
    pub last_cache_cleanup: Option<DateTime<Utc>>,
} 