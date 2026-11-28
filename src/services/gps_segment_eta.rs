use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};

const CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_millis(800);

pub type SegmentSecondsByHour = HashMap<(String, String), Vec<u32>>;

#[derive(Debug, Deserialize)]
struct SegmentTravelTime {
    from_stop_id: String,
    to_stop_id: String,
    seconds_by_hour: Vec<u32>,
}

#[derive(Debug, Deserialize)]
struct RouteSegmentTravelTimes {
    model_version: Option<String>,
    segments: Vec<SegmentTravelTime>,
}

type CacheKey = (String, String, u8);

pub struct GpsSegmentEtaClient {
    base_url: Option<String>,
    http: reqwest::Client,
    cache: RwLock<HashMap<CacheKey, (Instant, Arc<SegmentSecondsByHour>)>>,
}

impl GpsSegmentEtaClient {
    pub fn new(base_url: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            base_url: base_url.filter(|u| !u.trim().is_empty()),
            http,
            cache: RwLock::new(HashMap::new()),
        }
    }

    pub async fn segment_seconds(
        &self,
        gtfs_id: &str,
        route_id: &str,
        day_of_week: u8,
    ) -> Option<Arc<SegmentSecondsByHour>> {
        let base_url = self.base_url.as_deref()?;
        let key = (gtfs_id.to_string(), route_id.to_string(), day_of_week);

        let stale = match self.cache.read().await.get(&key) {
            Some((fetched_at, v)) if fetched_at.elapsed() < CACHE_TTL => return Some(v.clone()),
            Some((_, v)) => Some(v.clone()),
            None => None,
        };

        let (fetched_at, value) = match self.fetch(base_url, gtfs_id, route_id, day_of_week).await {
            Ok(v) => (Instant::now(), Arc::new(v)),
            Err(e) => {
                warn!(
                    "gps segment travel times failed gtfs_id={} route_id={} dow={}: {}",
                    gtfs_id, route_id, day_of_week, e
                );
                (
                    Instant::now()
                        .checked_sub(CACHE_TTL - RETRY_AFTER_ERROR)
                        .unwrap_or_else(Instant::now),
                    stale.unwrap_or_default(),
                )
            }
        };

        let mut cache = self.cache.write().await;
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        cache.insert(key, (fetched_at, value.clone()));
        Some(value)
    }

    async fn fetch(
        &self,
        base_url: &str,
        gtfs_id: &str,
        route_id: &str,
        day_of_week: u8,
    ) -> Result<SegmentSecondsByHour, String> {
        let mut url = reqwest::Url::parse(base_url).map_err(|e| e.to_string())?;
        url.path_segments_mut()
            .map_err(|_| "gps_processor_url cannot be a base".to_string())?
            .pop_if_empty()
            .extend([
                "api",
                "v1",
                gtfs_id,
                "data",
                "routes",
                route_id,
                "segment-travel-times",
            ]);
        url.query_pairs_mut()
            .append_pair("day_of_week", &day_of_week.to_string());

        let started = Instant::now();
        let resp = self.http.get(url).send().await.map_err(|e| e.to_string())?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            info!(
                "gps segment travel times not found gtfs_id={} route_id={} dow={} took_ms={}",
                gtfs_id,
                route_id,
                day_of_week,
                started.elapsed().as_millis()
            );
            return Ok(HashMap::new());
        }
        let body: RouteSegmentTravelTimes = resp
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let segments: SegmentSecondsByHour = body
            .segments
            .into_iter()
            .filter(|s| s.seconds_by_hour.len() == 24)
            .map(|s| ((s.from_stop_id, s.to_stop_id), s.seconds_by_hour))
            .collect();
        info!(
            "gps segment travel times fetched gtfs_id={} route_id={} dow={} model_version={} segments={} took_ms={}",
            gtfs_id,
            route_id,
            day_of_week,
            body.model_version.as_deref().unwrap_or(""),
            segments.len(),
            started.elapsed().as_millis()
        );
        Ok(segments)
    }
}
