use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use chrono::{Duration, Utc};
use tracing::info;

use crate::services::gtfs_service::GTFSService;
use crate::graphql::{
    TripQueryVariables,
    TripGraphQLResponse, TripApiResponse, TripStopResponse, TripScheduleResponse,
    TripCacheEntry, TripCacheStats, get_trip_query,
};
use crate::tools::error::{AppError, AppResult};

pub struct TripService {
    gtfs_service: Arc<GTFSService>,
    cache: Arc<RwLock<HashMap<String, TripCacheEntry>>>,
    stats: Arc<RwLock<TripCacheStats>>,
    cache_ttl_hours: u64,
}

impl TripService {
    pub fn new(gtfs_service: Arc<GTFSService>) -> Self {
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
        }
    }

    pub async fn get_trip_data(&self, trip_id: &str, gtfs_id: Option<String>, city: Option<String>) -> AppResult<TripApiResponse> {
        // Create a stable cache key that doesn't include the date
        let cache_key = format!("{}:{}:{}", 
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

        // Cache miss - fetch from GraphQL
        info!("Cache miss, fetching from GraphQL for key: {}", cache_key);
        let mut trip_data = self.fetch_trip_from_graphql(trip_id, gtfs_id, city).await?;
        
        // Change source to "cache" before storing
        trip_data.source = "cache".to_string();
        
        // Store in cache
        self.store_in_cache(&cache_key, &trip_data).await?;
        
        Ok(trip_data)
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
        
        info!("Stored in cache with key: {} (total cached: {})", cache_key, stats.total_cached_trips);
        
        Ok(())
    }

    async fn fetch_trip_from_graphql(&self, trip_id: &str, gtfs_id: Option<String>, city: Option<String>) -> AppResult<TripApiResponse> {
        let variables = TripQueryVariables {
            trip_id: trip_id.to_string(),
            service_date: chrono::Utc::now().format("%Y%m%d").to_string(),
        };
        
        let (query, variables_json) = get_trip_query(variables);
        
        // Execute GraphQL query
        let response = self.gtfs_service
            .execute_graphql_query(
                city.as_deref().unwrap_or("default"),
                &query,
                Some(variables_json.clone()),
                Some("TipPlan".to_string()),
                gtfs_id,
            )
            .await?;

        // Parse the response
        let trip_response: TripGraphQLResponse = serde_json::from_value(response.clone())
            .map_err(|e| AppError::Internal(format!("Failed to parse GraphQL response: {}. Raw response: {:?}", e, response)))?;

        // Check for GraphQL errors
        if let Some(errors) = trip_response.errors {
            let error_messages: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
            return Err(AppError::Internal(format!("GraphQL errors: {}", error_messages.join(", "))));
        }

        // Extract trip data
        let trip_data = trip_response.data
            .and_then(|d| d.trip)
            .ok_or_else(|| {
                // Try to get more information about the response
                let response_str = serde_json::to_string_pretty(&response)
                    .unwrap_or_else(|_| "Could not serialize response".to_string());
                AppError::NotFound(format!("Trip {} not found. Response: {}", trip_id, response_str))
            })?;

        // Convert to our internal format
        let stops: Vec<TripStopResponse> = trip_data.stoptimes_for_date.iter().map(|stoptime| TripStopResponse {
            stop_id: stoptime.stop.id.clone(),
            stop_code: stoptime.stop.code.clone(),
            stop_name: stoptime.stop.name.clone(),
            sequence: stoptime.stop_sequence,
            lat: stoptime.stop.lat,
            lon: stoptime.stop.lon,
        }).collect();

        // Create schedule from stoptimes
        let schedule: Vec<TripScheduleResponse> = trip_data.stoptimes_for_date.iter().map(|stoptime| {
            let arrival_time = stoptime.realtime_arrival
                .or(stoptime.scheduled_arrival)
                .map(|time| format!("{:02}:{:02}", time / 60, time % 60));
            
            let departure_time = stoptime.realtime_departure
                .or(stoptime.scheduled_departure)
                .map(|time| format!("{:02}:{:02}", time / 60, time % 60));
            
            TripScheduleResponse {
                stop_code: stoptime.stop.code.clone(),
                arrival_time,
                departure_time,
                sequence: stoptime.stop_sequence,
            }
        }).collect();

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