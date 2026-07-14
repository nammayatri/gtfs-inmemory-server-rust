use chrono::{Duration, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

use crate::graphql::{
    get_trip_query, TripApiResponse, TripCacheEntry, TripCacheStats, TripGraphQLResponse,
    TripQueryVariables, TripScheduleResponse, TripStopResponse,
};
use crate::services::gtfs_service::GTFSService;
use crate::tools::error::{AppError, AppResult};

/// One stop's schedule within a preprocessed trip shard
/// (trip_stoptimes/<gtfs_id>.json). Mirrors the preprocessor's output.
#[derive(serde::Deserialize)]
struct ShardStop {
    #[serde(rename = "stopId")]
    stop_id: String,
    #[serde(rename = "stopCode")]
    stop_code: String,
    #[serde(rename = "stopName")]
    stop_name: String,
    lat: f64,
    lon: f64,
    #[serde(rename = "arrivalTime")]
    arrival_time: Option<i32>,
    #[serde(rename = "departureTime")]
    departure_time: Option<i32>,
    sequence: i32,
}

#[derive(serde::Deserialize)]
struct ShardTrip {
    #[serde(rename = "tripId")]
    trip_id: String,
    #[serde(rename = "routeId")]
    route_id: String,
    direction: Option<i32>,
    stops: Vec<ShardStop>,
}

pub struct TripService {
    gtfs_service: Arc<GTFSService>,
    cache: Arc<RwLock<HashMap<String, TripCacheEntry>>>,
    stats: Arc<RwLock<TripCacheStats>>,
    cache_ttl_hours: u64,
    /// When true, /trip is served from preprocessed trip_stoptimes shards
    /// (falling back to OTP only if the shard/trip isn't found).
    use_preprocessed_data: bool,
    preprocessed_data_dir: String,
}

impl TripService {
    pub fn new(gtfs_service: Arc<GTFSService>) -> Self {
        let use_preprocessed_data = gtfs_service.use_preprocessed_data();
        let preprocessed_data_dir = gtfs_service.preprocessed_data_dir();
        Self {
            gtfs_service,
            cache: Arc::new(RwLock::new(HashMap::new())),
            stats: Arc::new(RwLock::new(TripCacheStats {
                total_cached_trips: 0,
                cache_hits: 0,
                cache_misses: 0,
                last_cache_cleanup: None,
            })),
            cache_ttl_hours: 24, // Cache for 24 hours by default
            use_preprocessed_data,
            preprocessed_data_dir,
        }
    }

    pub async fn get_trip_data(
        &self,
        trip_id: &str,
        gtfs_id: Option<String>,
        city: Option<String>,
    ) -> AppResult<TripApiResponse> {
        // Create a stable cache key that doesn't include the date
        let cache_key = format!(
            "{}:{}:{}",
            trip_id,
            gtfs_id.as_deref().unwrap_or("default"),
            city.as_deref().unwrap_or("default")
        );

        info!("Generated cache key: {}", cache_key);

        // Check cache first
        if let Some(cached_data) = self.get_from_cache(&cache_key).await? {
            info!("Returning cached data for key: {}", cache_key);
            return Ok(cached_data);
        }

        // Preprocessed mode: try the per-feed trip_stoptimes shard before OTP.
        // We cache the parsed trip so the shard file is only read on a cold
        // miss for that feed; warm trips come from `cache` above. Falls through
        // to GraphQL if the shard or trip isn't found (e.g. unknown trip_id, or
        // a feed not covered by preprocessed data).
        if self.use_preprocessed_data {
            if let Some(gid) = gtfs_id.as_deref() {
                match self.load_trip_from_shard(trip_id, gid).await {
                    Ok(Some(trip_data)) => {
                        let mut cached_data = trip_data.clone();
                        cached_data.source = "cache".to_string();
                        self.store_in_cache(&cache_key, &cached_data).await?;
                        return Ok(trip_data);
                    }
                    Ok(None) => {
                        info!(
                            "Trip {} not in preprocessed shard for {}, falling back to GraphQL",
                            trip_id, gid
                        );
                    }
                    Err(e) => {
                        info!(
                            "Preprocessed shard read failed for {} ({}): {} — falling back to GraphQL",
                            trip_id, gid, e
                        );
                    }
                }
            }
        }

        // Cache miss - fetch from GraphQL
        info!("Cache miss, fetching from GraphQL for key: {}", cache_key);
        let trip_data = self.fetch_trip_from_graphql(trip_id, gtfs_id, city).await?;

        // Store in cache (with source = "cache")
        let mut cached_data = trip_data.clone();
        cached_data.source = "cache".to_string();
        self.store_in_cache(&cache_key, &cached_data).await?;

        // Return original data with source = "graphql"
        Ok(trip_data)
    }

    /// Read one trip from the preprocessed `trip_stoptimes/<gtfs_id>.json`
    /// shard. Returns Ok(None) if the shard exists but has no such trip, or if
    /// the shard file is absent. Errs only on read/parse failure (caller falls
    /// back to GraphQL in every non-success case anyway).
    async fn load_trip_from_shard(
        &self,
        trip_id: &str,
        gtfs_id: &str,
    ) -> AppResult<Option<TripApiResponse>> {
        let path = std::path::Path::new(&self.preprocessed_data_dir)
            .join("trip_stoptimes")
            .join(format!("{}.json", gtfs_id));
        if !path.exists() {
            return Ok(None);
        }

        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            AppError::Internal(format!(
                "Failed to read trip shard {}: {}",
                path.display(),
                e
            ))
        })?;
        // Shard is a JSON object: { trip_id -> ShardTrip }. Deserialize the
        // whole feed's trips, then pick the one we need. The shard is per-feed
        // so this is bounded by one feed's size, not the global dataset.
        let trips: HashMap<String, ShardTrip> = serde_json::from_slice(&bytes).map_err(|e| {
            AppError::Internal(format!(
                "Failed to parse trip shard {}: {}",
                path.display(),
                e
            ))
        })?;

        let trip = match trips.get(trip_id) {
            Some(t) => t,
            None => return Ok(None),
        };

        let stops: Vec<TripStopResponse> = trip
            .stops
            .iter()
            .map(|s| TripStopResponse {
                stop_id: s.stop_id.clone(),
                stop_code: s.stop_code.clone(),
                stop_name: s.stop_name.clone(),
                sequence: s.sequence,
                lat: s.lat,
                lon: s.lon,
            })
            .collect();
        let schedule: Vec<TripScheduleResponse> = trip
            .stops
            .iter()
            .map(|s| TripScheduleResponse {
                stop_code: s.stop_code.clone(),
                arrival_time: s.arrival_time,
                departure_time: s.departure_time,
                sequence: s.sequence,
            })
            .collect();

        // Resolve route_name from the in-memory route table so the backend's
        // non-Maybe `Text` decoder doesn't see null. The shard only carries the
        // feed-scoped routeId (e.g. "kolkata_bus:AC54L-D") — strip the prefix
        // and look up the route's long_name; fall back to empty string if the
        // route isn't found or has no long_name.
        let route_name = {
            let code = trip
                .route_id
                .split(':')
                .next_back()
                .unwrap_or(&trip.route_id);
            match self.gtfs_service.get_route(gtfs_id, code).await {
                Ok(r) => r.long_name,
                Err(_) => None,
            }
        };

        Ok(Some(TripApiResponse {
            trip_id: trip.trip_id.clone(),
            route_id: trip.route_id.clone(),
            route_name,
            direction: trip.direction,
            stops,
            schedule,
            last_updated: Utc::now(),
            source: "preprocessed".to_string(),
        }))
    }

    async fn get_from_cache(&self, cache_key: &str) -> AppResult<Option<TripApiResponse>> {
        let cache = self.cache.read().await;

        if let Some(entry) = cache.get(cache_key) {
            // Check if cache entry is still valid
            if Utc::now() < entry.expires_at {
                // Update stats
                let mut stats = self.stats.write().await;
                stats.cache_hits += 1;

                info!("Cache HIT for key: {}", cache_key);
                return Ok(Some(entry.trip_data.clone()));
            } else {
                info!("Cache entry expired for key: {}", cache_key);
            }
        } else {
            info!("Cache MISS for key: {}", cache_key);
        }

        // Update stats
        let mut stats = self.stats.write().await;
        stats.cache_misses += 1;

        Ok(None)
    }

    async fn store_in_cache(&self, cache_key: &str, trip_data: &TripApiResponse) -> AppResult<()> {
        let mut cache = self.cache.write().await;
        let mut stats = self.stats.write().await;

        let expires_at = Utc::now() + Duration::hours(self.cache_ttl_hours as i64);

        let entry = TripCacheEntry {
            trip_data: trip_data.clone(),
            cached_at: Utc::now(),
            expires_at,
        };

        cache.insert(cache_key.to_string(), entry);
        stats.total_cached_trips = cache.len();

        info!(
            "Stored in cache with key: {} (total cached: {})",
            cache_key, stats.total_cached_trips
        );

        Ok(())
    }

    async fn fetch_trip_from_graphql(
        &self,
        trip_id: &str,
        gtfs_id: Option<String>,
        city: Option<String>,
    ) -> AppResult<TripApiResponse> {
        let variables = TripQueryVariables {
            trip_id: trip_id.to_string(),
            service_date: chrono::Utc::now().format("%Y%m%d").to_string(),
        };

        let (query, variables_json) = get_trip_query(variables);

        // Execute GraphQL query
        let response = self
            .gtfs_service
            .execute_graphql_query(
                city.as_deref().unwrap_or("default"),
                &query,
                Some(variables_json.clone()),
                Some("TipPlan".to_string()),
                gtfs_id,
            )
            .await?;

        // Parse the response
        let trip_response: TripGraphQLResponse =
            serde_json::from_value(response.clone()).map_err(|e| {
                AppError::Internal(format!(
                    "Failed to parse GraphQL response: {}. Raw response: {:?}",
                    e, response
                ))
            })?;

        // Check for GraphQL errors
        if let Some(errors) = trip_response.errors {
            let error_messages: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
            return Err(AppError::Internal(format!(
                "GraphQL errors: {}",
                error_messages.join(", ")
            )));
        }

        // Extract trip data
        let trip_data = trip_response.data.and_then(|d| d.trip).ok_or_else(|| {
            // Try to get more information about the response
            let response_str = serde_json::to_string_pretty(&response)
                .unwrap_or_else(|_| "Could not serialize response".to_string());
            AppError::NotFound(format!(
                "Trip {} not found. Response: {}",
                trip_id, response_str
            ))
        })?;

        // Convert to our internal format
        let stops: Vec<TripStopResponse> = trip_data
            .stoptimes_for_date
            .iter()
            .map(|stoptime| TripStopResponse {
                stop_id: stoptime.stop.id.clone(),
                stop_code: stoptime.stop.code.clone(),
                stop_name: stoptime.stop.name.clone(),
                sequence: stoptime.stop_sequence,
                lat: stoptime.stop.lat,
                lon: stoptime.stop.lon,
            })
            .collect();

        // Create schedule from stoptimes
        let schedule: Vec<TripScheduleResponse> = trip_data
            .stoptimes_for_date
            .iter()
            .map(|stoptime| {
                let arrival_time = stoptime.realtime_arrival;
                let departure_time = stoptime.realtime_departure;

                TripScheduleResponse {
                    stop_code: stoptime.stop.code.clone(),
                    arrival_time,
                    departure_time,
                    sequence: stoptime.stop_sequence,
                }
            })
            .collect();

        Ok(TripApiResponse {
            trip_id: trip_data.gtfs_id,
            route_id: trip_data.route.id,
            route_name: trip_data.route.long_name,
            direction: None, // Not available in new query
            stops,
            schedule,
            last_updated: Utc::now(),
            source: "graphql".to_string(),
        })
    }

    pub async fn get_cache_stats(&self) -> TripCacheStats {
        self.stats.read().await.clone()
    }

    pub async fn clear_cache(&self) -> AppResult<()> {
        let mut cache = self.cache.write().await;
        let mut stats = self.stats.write().await;

        cache.clear();
        stats.total_cached_trips = 0;
        stats.last_cache_cleanup = Some(Utc::now());

        Ok(())
    }

    pub async fn cleanup_expired_cache(&self) -> AppResult<usize> {
        let mut cache = self.cache.write().await;
        let mut stats = self.stats.write().await;

        let now = Utc::now();
        let initial_size = cache.len();

        cache.retain(|_, entry| entry.expires_at > now);

        let removed_count = initial_size - cache.len();
        stats.total_cached_trips = cache.len();
        stats.last_cache_cleanup = Some(now);

        Ok(removed_count)
    }

    pub async fn get_cached_trip_ids(&self) -> Vec<String> {
        let cache = self.cache.read().await;
        cache.keys().cloned().collect()
    }
}
