use crate::tools::error::{AppError, AppResult};
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};

// --- API response types (private) ---

#[derive(Deserialize)]
struct GenerateAccessResponse {
    data: Vec<AccessTokenData>,
    issuccess: bool,
}

#[derive(Deserialize)]
struct AccessTokenData {
    #[serde(rename = "strAccessToken")]
    access_token: String,
    #[serde(rename = "dteAccessTokenExpirationTime")]
    expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct GetStationListResponse {
    data: Vec<OsrtcApiStation>,
    issuccess: bool,
}

#[derive(Deserialize)]
struct OsrtcApiStation {
    #[serde(rename = "intStationID")]
    station_id: i64,
    #[serde(rename = "strStationName")]
    station_name: String,
    #[serde(rename = "strStationCode")]
    station_code: String,
}

// --- Internal token cache ---

struct OsrtcToken {
    access_token: String,
    expires_at: DateTime<Utc>,
}

// --- Public types ---

#[derive(Clone)]
pub struct OsrtcStation {
    pub station_id: i64,
    pub station_name: String,
    pub station_code: String, // strStationCode, kept for observability. Not actually used by the frontend.
}

pub struct OsrtcStationCache {
    http_client: Client,
    base_url: String,
    username: String,
    secret_key: String,
    token: Arc<RwLock<Option<OsrtcToken>>>,
    // Keyed by intStationID.to_string() — the identifier used by frontend/backend
    stations: Arc<RwLock<HashMap<String, OsrtcStation>>>,
    station_refresh_interval_secs: u64,
}

impl OsrtcStationCache {
    pub fn new(
        base_url: String,
        username: String,
        secret_key: String,
        station_refresh_interval_secs: u64,
    ) -> AppResult<Self> {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| AppError::Internal(format!("Failed to create HTTP client: {}", e)))?;

        Ok(Self {
            http_client,
            base_url,
            username,
            secret_key,
            token: Arc::new(RwLock::new(None)),
            stations: Arc::new(RwLock::new(HashMap::new())),
            station_refresh_interval_secs,
        })
    }

    async fn refresh_token(&self) -> AppResult<String> {
        info!("Refreshing OSRTC auth token...");
        let url = format!("{}/api/Auth/GenerateAccess", self.base_url);
        let body = serde_json::json!({
            "UserName": self.username,
            "SecretKey": self.secret_key,
        });

        let response = self
            .http_client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("OSRTC GenerateAccess request failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(AppError::Internal(format!(
                "OSRTC GenerateAccess returned status: {}",
                response.status()
            )));
        }

        let parsed: GenerateAccessResponse = response.json().await.map_err(|e| {
            AppError::Internal(format!("Failed to parse OSRTC GenerateAccess response: {}", e))
        })?;

        if !parsed.issuccess {
            return Err(AppError::Internal(
                "OSRTC GenerateAccess returned issuccess=false".to_string(),
            ));
        }

        let token_data = parsed.data.into_iter().next().ok_or_else(|| {
            AppError::Internal("OSRTC GenerateAccess returned empty data array".to_string())
        })?;

        let mut token_lock = self.token.write().await;
        let token = OsrtcToken { access_token: token_data.access_token, expires_at: token_data.expires_at };                                                                                                                                             
        let access_token = token.access_token.clone();                                                                                                                                                                                                   
        *token_lock = Some(token);                                                                                                                                                                                                                            

        info!("OSRTC auth token refreshed successfully");
        Ok(access_token)
    }

    // Lazy refresh: returns cached token if it has >5 min left, otherwise re-fetches.
    async fn get_valid_token(&self) -> AppResult<String> {
        {
            let token = self.token.read().await;
            if let Some(t) = &*token {
                if t.expires_at > Utc::now() + Duration::minutes(5) {
                    return Ok(t.access_token.clone());
                }
            }
        }
        self.refresh_token().await
    }

    async fn refresh_stations(&self) -> AppResult<()> {
        info!("Refreshing OSRTC station list...");
        let token = self.get_valid_token().await?;
        let url = format!(
            "{}/api/List/GetStationList?intStationID=0",
            self.base_url
        );

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("content-type", "application/json")
            .send()
            .await
            .map_err(|e| {
                AppError::Internal(format!("OSRTC GetStationList request failed: {}", e))
            })?;

        if !response.status().is_success() {
            return Err(AppError::Internal(format!(
                "OSRTC GetStationList returned status: {}",
                response.status()
            )));
        }

        let parsed: GetStationListResponse = response.json().await.map_err(|e| {
            AppError::Internal(format!("Failed to parse OSRTC GetStationList response: {}", e))
        })?;

        if !parsed.issuccess {
            return Err(AppError::Internal(
                "OSRTC GetStationList returned issuccess=false".to_string(),
            ));
        }

        let mut new_stations: HashMap<String, OsrtcStation> =
            HashMap::with_capacity(parsed.data.len());
        for api_station in parsed.data {
            new_stations.insert(
                api_station.station_id.to_string(),
                OsrtcStation {
                    station_id: api_station.station_id,
                    station_name: api_station.station_name,
                    station_code: api_station.station_code,
                },
            );
        }

        let count = new_stations.len();
        let mut stations_lock = self.stations.write().await;
        *stations_lock = new_stations;

        info!("OSRTC station list refreshed: {} stations loaded", count);
        Ok(())
    }

    pub async fn initialize(&self) -> AppResult<()> {
        info!("Initializing OSRTC station cache...");
        self.refresh_stations().await?;
        info!("OSRTC station cache initialized successfully");
        Ok(())
    }

    pub async fn get_station_by_id(&self, id: &str) -> Option<OsrtcStation> {
        let stations = self.stations.read().await;
        stations.get(id).cloned()
    }

    pub async fn get_all_stations(&self) -> Vec<OsrtcStation> {
        let stations = self.stations.read().await;
        stations.values().cloned().collect()
    }

    pub async fn start_background_refresh_task(self: Arc<Self>) {
        let interval_secs = self.station_refresh_interval_secs;
        info!(
            "Starting OSRTC station cache background refresh task with interval: {}s",
            interval_secs
        );
        let duration = std::time::Duration::from_secs(interval_secs);
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + duration, duration);
        loop {
            interval.tick().await;
            if let Err(e) = self.refresh_stations().await {
                error!("Failed to refresh OSRTC station cache: {}", e);
            }
        }
    }
}

// --- Conversion helper ---

use crate::models::{LatLong, RouteStopMapping};

pub fn osrtc_station_to_route_stop_mapping(station: &OsrtcStation) -> RouteStopMapping {
    let id_str: Arc<str> = station.station_id.to_string().into();
    RouteStopMapping {
        stop_code: id_str.clone(),
        provider_code: id_str,
        stop_name: station.station_name.as_str().into(),
        stop_point: LatLong { lat: 0.0, lon: 0.0 }, // Doesn't matter, and won't be consumed. Better to do this than object structure
        route_code: Arc::from(""),
        sequence_num: 0,
        vehicle_type: Arc::from("BUS"),
        estimated_travel_time_from_previous_stop: None,
        geo_json: None,
        gates: None,
        hindi_name: None,
        regional_name: None,
        platform: None,
        parent_stop_code: None,
    }
}

