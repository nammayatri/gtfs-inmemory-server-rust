use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use chrono::{NaiveTime, Timelike, Utc};
use tracing::info;

use crate::models::{
    clean_identifier, GtfsDataQualityReport, ScheduledArrival, ScheduledDeparture, ScheduledTrip,
};
use crate::services::gtfs_service::GTFSService;
use crate::tools::error::{AppError, AppResult};

/// Index entry: maps seconds-from-midnight → list of (trip_id, stop_index) pairs.
type TimeIndex = BTreeMap<i32, Vec<(String, usize)>>;

pub struct ScheduleService {
    gtfs_service: Arc<GTFSService>,
}

impl ScheduleService {
    pub fn new(gtfs_service: Arc<GTFSService>) -> Self {
        Self { gtfs_service }
    }

    // ── helpers ──────────────────────────────────────────────────────────

    fn parse_time(time_str: &str) -> AppResult<NaiveTime> {
        // GTFS allows times > 24:00:00 for next-day trips, but NaiveTime only handles 0-23.
        // For times >= 24:00, we wrap around (25:30:00 → 01:30:00) and handle via seconds math.
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return Err(AppError::BadRequest(format!(
                "Invalid time format '{}', expected HH:MM or HH:MM:SS",
                time_str
            )));
        }
        // Validate each part is numeric
        for part in &parts {
            if part.parse::<u32>().is_err() {
                return Err(AppError::BadRequest(format!(
                    "Invalid time format '{}': non-numeric component",
                    time_str
                )));
            }
        }
        let hours: u32 = parts[0].parse().unwrap();
        let minutes: u32 = parts[1].parse().unwrap();
        let seconds: u32 = if parts.len() == 3 { parts[2].parse().unwrap() } else { 0 };

        if minutes >= 60 || seconds >= 60 {
            return Err(AppError::BadRequest(format!(
                "Invalid time '{}': minutes and seconds must be < 60",
                time_str
            )));
        }
        // For GTFS times > 24h, wrap hours
        let wrapped_hours = hours % 24;
        NaiveTime::from_hms_opt(wrapped_hours, minutes, seconds).ok_or_else(|| {
            AppError::BadRequest(format!("Cannot construct time from '{}'", time_str))
        })
    }

    fn time_to_seconds(t: &NaiveTime) -> i32 {
        t.num_seconds_from_midnight() as i32
    }

    fn seconds_to_time_string(secs: i32) -> String {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{:02}:{:02}:{:02}", h, m, s)
    }

    /// Build a departure time index for a given gtfs_id.
    /// Returns BTreeMap<departure_seconds, Vec<(trip_id, stop_index)>>.
    async fn build_departure_index(&self, gtfs_id: &str) -> AppResult<TimeIndex> {
        let data = self.gtfs_service.data().read().await;
        let trip_map = data
            .route_example_trip_details_by_gtfs
            .get(gtfs_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("No trip data found for gtfs_id '{}'", gtfs_id))
            })?;

        let mut index: TimeIndex = BTreeMap::new();
        for (route_code, trip) in trip_map {
            for (i, stop) in trip.stops.iter().enumerate() {
                index
                    .entry(stop.scheduled_departure)
                    .or_default()
                    .push((route_code.clone(), i));
            }
        }
        Ok(index)
    }

    /// Build an arrival time index for a given gtfs_id.
    async fn build_arrival_index(&self, gtfs_id: &str) -> AppResult<TimeIndex> {
        let data = self.gtfs_service.data().read().await;
        let trip_map = data
            .route_example_trip_details_by_gtfs
            .get(gtfs_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("No trip data found for gtfs_id '{}'", gtfs_id))
            })?;

        let mut index: TimeIndex = BTreeMap::new();
        for (route_code, trip) in trip_map {
            for (i, stop) in trip.stops.iter().enumerate() {
                index
                    .entry(stop.scheduled_arrival)
                    .or_default()
                    .push((route_code.clone(), i));
            }
        }
        Ok(index)
    }

    fn lookup_route_info(
        &self,
        data: &crate::models::GTFSData,
        gtfs_id: &str,
        route_code: &str,
    ) -> (Option<String>, Option<String>) {
        let route_info = data
            .routes_by_gtfs
            .get(gtfs_id)
            .and_then(|m| m.get(route_code));
        match route_info {
            Some(r) => (r.short_name.clone(), Some(r.mode.clone())),
            None => (None, None),
        }
    }

    // ── public API ──────────────────────────────────────────────────────

    pub async fn get_departures_at_stop(
        &self,
        gtfs_id: &str,
        stop_code: &str,
        time_str: Option<&str>,
        _date_str: Option<&str>,
        window_minutes: u32,
        limit: usize,
    ) -> AppResult<Vec<ScheduledDeparture>> {
        let gtfs_id = clean_identifier(gtfs_id);
        let stop_code = clean_identifier(stop_code);

        let base_time = match time_str {
            Some(t) => Self::parse_time(t)?,
            None => Utc::now().time(),
        };
        let start_secs = Self::time_to_seconds(&base_time);
        let end_secs = start_secs + (window_minutes as i32) * 60;

        info!(
            "Querying departures at stop {} for gtfs_id {}, window {}s-{}s",
            stop_code, gtfs_id, start_secs, end_secs
        );

        let dep_index = self.build_departure_index(&gtfs_id).await?;
        let data = self.gtfs_service.data().read().await;
        let trip_map = data
            .route_example_trip_details_by_gtfs
            .get(gtfs_id.as_str())
            .ok_or_else(|| {
                AppError::NotFound(format!("No trip data found for gtfs_id '{}'", gtfs_id))
            })?;

        let mut results: Vec<ScheduledDeparture> = Vec::new();

        for (_dep_secs, entries) in dep_index.range(start_secs..=end_secs) {
            for (route_code, stop_idx) in entries {
                if let Some(trip) = trip_map.get(route_code) {
                    if let Some(stop) = trip.stops.get(*stop_idx) {
                        if stop.stop_code == stop_code {
                            let (short_name, mode) =
                                self.lookup_route_info(&data, &gtfs_id, route_code);
                            let headsign = stop
                                .headsign
                                .as_str()
                                .map(|s| s.to_string());
                            results.push(ScheduledDeparture {
                                trip_id: trip.trip_id.clone(),
                                route_id: route_code.clone(),
                                route_short_name: short_name,
                                stop_code: stop.stop_code.clone(),
                                stop_name: stop.stop_name.clone(),
                                departure_time: Self::seconds_to_time_string(
                                    stop.scheduled_departure,
                                ),
                                headsign,
                                stop_sequence: stop.stop_position,
                                mode,
                            });
                        }
                    }
                }
            }
            if results.len() >= limit {
                break;
            }
        }

        results.truncate(limit);
        Ok(results)
    }

    pub async fn get_arrivals_at_stop(
        &self,
        gtfs_id: &str,
        stop_code: &str,
        time_str: Option<&str>,
        _date_str: Option<&str>,
        window_minutes: u32,
        limit: usize,
    ) -> AppResult<Vec<ScheduledArrival>> {
        let gtfs_id = clean_identifier(gtfs_id);
        let stop_code = clean_identifier(stop_code);

        let base_time = match time_str {
            Some(t) => Self::parse_time(t)?,
            None => Utc::now().time(),
        };
        let start_secs = Self::time_to_seconds(&base_time);
        let end_secs = start_secs + (window_minutes as i32) * 60;

        info!(
            "Querying arrivals at stop {} for gtfs_id {}, window {}s-{}s",
            stop_code, gtfs_id, start_secs, end_secs
        );

        let arr_index = self.build_arrival_index(&gtfs_id).await?;
        let data = self.gtfs_service.data().read().await;
        let trip_map = data
            .route_example_trip_details_by_gtfs
            .get(gtfs_id.as_str())
            .ok_or_else(|| {
                AppError::NotFound(format!("No trip data found for gtfs_id '{}'", gtfs_id))
            })?;

        let mut results: Vec<ScheduledArrival> = Vec::new();

        for (_arr_secs, entries) in arr_index.range(start_secs..=end_secs) {
            for (route_code, stop_idx) in entries {
                if let Some(trip) = trip_map.get(route_code) {
                    if let Some(stop) = trip.stops.get(*stop_idx) {
                        if stop.stop_code == stop_code {
                            let (short_name, mode) =
                                self.lookup_route_info(&data, &gtfs_id, route_code);
                            let headsign = stop
                                .headsign
                                .as_str()
                                .map(|s| s.to_string());
                            results.push(ScheduledArrival {
                                trip_id: trip.trip_id.clone(),
                                route_id: route_code.clone(),
                                route_short_name: short_name,
                                stop_code: stop.stop_code.clone(),
                                stop_name: stop.stop_name.clone(),
                                arrival_time: Self::seconds_to_time_string(
                                    stop.scheduled_arrival,
                                ),
                                headsign,
                                stop_sequence: stop.stop_position,
                                mode,
                            });
                        }
                    }
                }
            }
            if results.len() >= limit {
                break;
            }
        }

        results.truncate(limit);
        Ok(results)
    }

    pub async fn get_trips_between_stops(
        &self,
        gtfs_id: &str,
        origin_stop_code: &str,
        destination_stop_code: &str,
        _date_str: Option<&str>,
        depart_after: Option<&str>,
        arrive_before: Option<&str>,
        window_minutes: u32,
    ) -> AppResult<Vec<ScheduledTrip>> {
        let gtfs_id = clean_identifier(gtfs_id);
        let origin = clean_identifier(origin_stop_code);
        let destination = clean_identifier(destination_stop_code);

        let depart_after_secs = match depart_after {
            Some(t) => Some(Self::time_to_seconds(&Self::parse_time(t)?)),
            None => None,
        };
        let arrive_before_secs = match arrive_before {
            Some(t) => Some(Self::time_to_seconds(&Self::parse_time(t)?)),
            None => None,
        };
        let window_secs = (window_minutes as i32) * 60;

        info!(
            "Querying trips between {} → {} for gtfs_id {}",
            origin, destination, gtfs_id
        );

        let data = self.gtfs_service.data().read().await;
        let trip_map = data
            .route_example_trip_details_by_gtfs
            .get(gtfs_id.as_str())
            .ok_or_else(|| {
                AppError::NotFound(format!("No trip data found for gtfs_id '{}'", gtfs_id))
            })?;

        // Build stop_code → Vec<(route_code, stop_index)> index for O(1) lookups
        let mut stop_index: HashMap<&str, Vec<(&str, usize)>> = HashMap::new();
        for (route_code, trip) in trip_map {
            for (i, stop) in trip.stops.iter().enumerate() {
                stop_index
                    .entry(stop.stop_code.as_str())
                    .or_default()
                    .push((route_code.as_str(), i));
            }
        }

        let mut results: Vec<ScheduledTrip> = Vec::new();

        // Get all trips visiting the origin stop
        let origin_entries = match stop_index.get(origin.as_str()) {
            Some(entries) => entries.clone(),
            None => return Ok(results),
        };

        // Build a set of route_codes that visit destination, with their stop index
        let dest_entries: HashMap<&str, usize> = stop_index
            .get(destination.as_str())
            .map(|entries| entries.iter().map(|(rc, idx)| (*rc, *idx)).collect())
            .unwrap_or_default();

        for (route_code, origin_stop_idx) in &origin_entries {
            // O(1) lookup: does this route also visit the destination?
            if let Some(&dest_stop_idx) = dest_entries.get(route_code) {
                // Origin must come before destination
                if *origin_stop_idx >= dest_stop_idx {
                    continue;
                }

                let trip = &trip_map[*route_code];
                let dep_secs = trip.stops[*origin_stop_idx].scheduled_departure;
                let arr_secs = trip.stops[dest_stop_idx].scheduled_arrival;

                // Apply forward constraint (depart_after)
                if let Some(da) = depart_after_secs {
                    if dep_secs < da || dep_secs > da + window_secs {
                        continue;
                    }
                }

                // Apply backward constraint (arrive_before)
                if let Some(ab) = arrive_before_secs {
                    if arr_secs > ab || arr_secs < ab - window_secs {
                        continue;
                    }
                }

                let (short_name, mode) =
                    self.lookup_route_info(&data, &gtfs_id, route_code);

                results.push(ScheduledTrip {
                    trip_id: trip.trip_id.clone(),
                    route_id: route_code.to_string(),
                    route_short_name: short_name,
                    origin_stop_code: origin.clone(),
                    origin_stop_name: trip.stops[*origin_stop_idx].stop_name.clone(),
                    destination_stop_code: destination.clone(),
                    destination_stop_name: trip.stops[dest_stop_idx].stop_name.clone(),
                    departure_time: Self::seconds_to_time_string(dep_secs),
                    arrival_time: Self::seconds_to_time_string(arr_secs),
                    mode,
                    num_stops: dest_stop_idx - origin_stop_idx + 1,
                });
            }
        }

        // Sort by departure time
        results.sort_by(|a, b| a.departure_time.cmp(&b.departure_time));
        Ok(results)
    }

    pub async fn get_next_services(
        &self,
        gtfs_id: &str,
        stop_code: &str,
        _date_str: Option<&str>,
        time_str: Option<&str>,
        modes: Option<&[String]>,
        limit: usize,
    ) -> AppResult<Vec<ScheduledDeparture>> {
        let gtfs_id = clean_identifier(gtfs_id);
        let stop_code = clean_identifier(stop_code);

        let base_time = match time_str {
            Some(t) => Self::parse_time(t)?,
            None => Utc::now().time(),
        };
        let start_secs = Self::time_to_seconds(&base_time);

        info!(
            "Querying next services at stop {} for gtfs_id {}, from {}s",
            stop_code, gtfs_id, start_secs
        );

        let dep_index = self.build_departure_index(&gtfs_id).await?;
        let data = self.gtfs_service.data().read().await;
        let trip_map = data
            .route_example_trip_details_by_gtfs
            .get(gtfs_id.as_str())
            .ok_or_else(|| {
                AppError::NotFound(format!("No trip data found for gtfs_id '{}'", gtfs_id))
            })?;

        let mut results: Vec<ScheduledDeparture> = Vec::new();

        // Scan from start_secs to end of day
        for (_dep_secs, entries) in dep_index.range(start_secs..) {
            for (route_code, stop_idx) in entries {
                if let Some(trip) = trip_map.get(route_code) {
                    if let Some(stop) = trip.stops.get(*stop_idx) {
                        if stop.stop_code != stop_code {
                            continue;
                        }
                        let (short_name, mode) =
                            self.lookup_route_info(&data, &gtfs_id, route_code);

                        // Filter by mode if specified
                        if let Some(mode_filter) = modes {
                            if let Some(ref m) = mode {
                                if !mode_filter.iter().any(|f| f.eq_ignore_ascii_case(m)) {
                                    continue;
                                }
                            } else {
                                continue;
                            }
                        }

                        let headsign = stop
                            .headsign
                            .as_str()
                            .map(|s| s.to_string());
                        results.push(ScheduledDeparture {
                            trip_id: trip.trip_id.clone(),
                            route_id: route_code.clone(),
                            route_short_name: short_name,
                            stop_code: stop.stop_code.clone(),
                            stop_name: stop.stop_name.clone(),
                            departure_time: Self::seconds_to_time_string(
                                stop.scheduled_departure,
                            ),
                            headsign,
                            stop_sequence: stop.stop_position,
                            mode,
                        });
                    }
                }
            }
            if results.len() >= limit {
                break;
            }
        }

        results.truncate(limit);
        Ok(results)
    }

    pub async fn get_quality_report(&self, gtfs_id: &str) -> AppResult<GtfsDataQualityReport> {
        let gtfs_id = clean_identifier(gtfs_id);
        let data = self.gtfs_service.data().read().await;

        let total_routes = data
            .routes_by_gtfs
            .get(gtfs_id.as_str())
            .map(|m| m.len())
            .unwrap_or(0);

        let total_stops = data
            .stops_by_gtfs
            .get(gtfs_id.as_str())
            .map(|s| s.stops.len())
            .unwrap_or(0);

        let trip_map = data
            .route_example_trip_details_by_gtfs
            .get(gtfs_id.as_str());
        let total_trips = trip_map.map(|m| m.len()).unwrap_or(0);

        // Routes with at least one example trip
        let route_codes_with_trips: HashSet<&str> = trip_map
            .map(|m| m.keys().map(|k| k.as_str()).collect())
            .unwrap_or_default();
        let routes_with_trips = route_codes_with_trips.len();
        let routes_without_trips = total_routes.saturating_sub(routes_with_trips);

        // Stops that appear in at least one trip
        let mut stops_in_trips: HashSet<String> = HashSet::new();
        if let Some(tm) = trip_map {
            for trip in tm.values() {
                for stop in &trip.stops {
                    stops_in_trips.insert(stop.stop_code.clone());
                }
            }
        }
        let stops_with_departures = stops_in_trips.len();

        // Stops in stop data but not in any trip
        let all_stop_codes: HashSet<String> = data
            .stops_by_gtfs
            .get(gtfs_id.as_str())
            .map(|s| s.stops.keys().cloned().collect())
            .unwrap_or_default();
        let orphaned_stops = all_stop_codes
            .difference(&stops_in_trips)
            .count();
        let stops_without_departures = total_stops.saturating_sub(stops_with_departures);

        let data_hash = data.data_hash.get(gtfs_id.as_str()).cloned();

        Ok(GtfsDataQualityReport {
            gtfs_id: gtfs_id.to_string(),
            total_routes,
            total_stops,
            total_trips,
            routes_with_trips,
            routes_without_trips,
            stops_with_departures,
            stops_without_departures,
            orphaned_stops,
            data_hash,
            generated_at: Utc::now().to_rfc3339(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        GTFSData, GTFSStop, GTFSStopData, NandiRoutesRes, TripDetails, TripStopDetail,
    };
    use std::collections::HashMap;

    const TEST_GTFS_ID: &str = "test_gtfs";
    const STOP_A: &str = "STOP_A";
    const STOP_B: &str = "STOP_B";
    const STOP_C: &str = "STOP_C";
    const STOP_D: &str = "STOP_D";
    const ROUTE_1: &str = "ROUTE_1";
    const ROUTE_2: &str = "ROUTE_2";

    /// Builds a TripStopDetail with the given parameters.
    fn make_stop(
        stop_code: &str,
        stop_name: &str,
        arrival_secs: i32,
        departure_secs: i32,
        position: i32,
    ) -> TripStopDetail {
        TripStopDetail {
            stop_id: format!("id_{}", stop_code),
            stop_code: stop_code.to_string(),
            stop_name: Some(stop_name.to_string()),
            platform_code: None,
            lat: 12.97,
            lon: 77.59,
            scheduled_arrival: arrival_secs,
            scheduled_departure: departure_secs,
            headsign: serde_json::Value::String("City Center".to_string()),
            stop_position: position,
        }
    }

    /// Creates a GTFSData populated with two routes/trips for testing.
    ///
    /// Route 1: STOP_A (08:00) -> STOP_B (08:15) -> STOP_C (08:30)
    /// Route 2: STOP_B (09:00) -> STOP_C (09:20) -> STOP_D (09:40)
    fn build_test_data() -> GTFSData {
        let mut data = GTFSData::new();

        // Trip details for ROUTE_1
        let trip1 = TripDetails {
            trip_id: "trip_1".to_string(),
            stops: vec![
                make_stop(STOP_A, "Stop A Station", 28800, 28800, 1),   // 08:00:00
                make_stop(STOP_B, "Stop B Station", 29700, 29700, 2),   // 08:15:00
                make_stop(STOP_C, "Stop C Station", 30600, 30600, 3),   // 08:30:00
            ],
        };

        // Trip details for ROUTE_2
        let trip2 = TripDetails {
            trip_id: "trip_2".to_string(),
            stops: vec![
                make_stop(STOP_B, "Stop B Station", 32400, 32400, 1),   // 09:00:00
                make_stop(STOP_C, "Stop C Station", 33600, 33600, 2),   // 09:20:00
                make_stop(STOP_D, "Stop D Station", 34800, 34800, 3),   // 09:40:00
            ],
        };

        let mut trip_map = HashMap::new();
        trip_map.insert(ROUTE_1.to_string(), trip1);
        trip_map.insert(ROUTE_2.to_string(), trip2);
        data.route_example_trip_details_by_gtfs
            .insert(TEST_GTFS_ID.to_string(), trip_map);

        // Route metadata
        let mut route_map = HashMap::new();
        route_map.insert(
            ROUTE_1.to_string(),
            NandiRoutesRes {
                id: ROUTE_1.to_string(),
                short_name: Some("R1".to_string()),
                long_name: Some("Route One".to_string()),
                mode: "BUS".to_string(),
                agency_name: Some("Test Agency".to_string()),
                trip_count: Some(1),
                stop_count: Some(3),
                start_point: None,
                end_point: None,
                service_tier_type: None,
            },
        );
        route_map.insert(
            ROUTE_2.to_string(),
            NandiRoutesRes {
                id: ROUTE_2.to_string(),
                short_name: Some("R2".to_string()),
                long_name: Some("Route Two".to_string()),
                mode: "METRO".to_string(),
                agency_name: Some("Test Agency".to_string()),
                trip_count: Some(1),
                stop_count: Some(3),
                start_point: None,
                end_point: None,
                service_tier_type: None,
            },
        );
        data.routes_by_gtfs
            .insert(TEST_GTFS_ID.to_string(), route_map);

        // Stop data
        let mut stop_map = HashMap::new();
        for (code, name) in [
            (STOP_A, "Stop A Station"),
            (STOP_B, "Stop B Station"),
            (STOP_C, "Stop C Station"),
            (STOP_D, "Stop D Station"),
        ] {
            stop_map.insert(
                code.to_string(),
                GTFSStop {
                    id: format!("id_{}", code),
                    code: code.to_string(),
                    name: name.to_string(),
                    lat: 12.97,
                    lon: 77.59,
                    station_id: None,
                    cluster: None,
                    hindi_name: None,
                    regional_name: None,
                },
            );
        }
        data.stops_by_gtfs.insert(
            TEST_GTFS_ID.to_string(),
            GTFSStopData { stops: stop_map },
        );

        data.data_hash
            .insert(TEST_GTFS_ID.to_string(), "abc123hash".to_string());

        data
    }

    fn create_schedule_service(data: GTFSData) -> ScheduleService {
        let gtfs_service = Arc::new(GTFSService::new_with_data(data));
        ScheduleService::new(gtfs_service)
    }

    // ── parse_time / seconds helpers ────────────────────────────────────

    #[test]
    fn test_parse_time_valid() {
        let t = ScheduleService::parse_time("08:30:00").unwrap();
        assert_eq!(ScheduleService::time_to_seconds(&t), 30600);
    }

    #[test]
    fn test_parse_time_midnight() {
        let t = ScheduleService::parse_time("00:00:00").unwrap();
        assert_eq!(ScheduleService::time_to_seconds(&t), 0);
    }

    #[test]
    fn test_parse_time_invalid_format() {
        assert!(ScheduleService::parse_time("not_a_time").is_err());
        assert!(ScheduleService::parse_time("").is_err());
        // Short format "8:30" is not valid for %H:%M:%S
        assert!(ScheduleService::parse_time("8:30").is_err());
        // Over-24h times are not valid NaiveTime
        assert!(ScheduleService::parse_time("25:30:00").is_err());
    }

    #[test]
    fn test_seconds_to_time_string() {
        assert_eq!(ScheduleService::seconds_to_time_string(0), "00:00:00");
        assert_eq!(ScheduleService::seconds_to_time_string(3661), "01:01:01");
        assert_eq!(ScheduleService::seconds_to_time_string(86399), "23:59:59");
    }

    // ── get_departures_at_stop ──────────────────────────────────────────

    #[tokio::test]
    async fn test_departures_valid_stop_within_window() {
        let svc = create_schedule_service(build_test_data());
        // STOP_A departs at 08:00:00 (28800s). Query from 07:50 with 20-min window.
        let results = svc
            .get_departures_at_stop(TEST_GTFS_ID, STOP_A, Some("07:50:00"), None, 20, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].stop_code, STOP_A);
        assert_eq!(results[0].departure_time, "08:00:00");
        assert_eq!(results[0].route_short_name, Some("R1".to_string()));
        assert_eq!(results[0].mode, Some("BUS".to_string()));
    }

    #[tokio::test]
    async fn test_departures_stop_not_in_any_trip() {
        let svc = create_schedule_service(build_test_data());
        // UNKNOWN_STOP does not appear in any trip – should return empty.
        let results = svc
            .get_departures_at_stop(TEST_GTFS_ID, "UNKNOWN_STOP", Some("08:00:00"), None, 60, 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_departures_invalid_gtfs_id() {
        let svc = create_schedule_service(build_test_data());
        let result = svc
            .get_departures_at_stop("nonexistent", STOP_A, Some("08:00:00"), None, 60, 10)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_departures_empty_results_outside_window() {
        let svc = create_schedule_service(build_test_data());
        // Query a window that does not include any departure at STOP_A (08:00).
        let results = svc
            .get_departures_at_stop(TEST_GTFS_ID, STOP_A, Some("10:00:00"), None, 30, 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_departures_time_window_boundary_inclusive() {
        let svc = create_schedule_service(build_test_data());
        // STOP_A at exactly 08:00:00. Query starts at 08:00:00 with 1-min window.
        // The range is 28800..=28860, and STOP_A departs at 28800 – should be included.
        let results = svc
            .get_departures_at_stop(TEST_GTFS_ID, STOP_A, Some("08:00:00"), None, 1, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].departure_time, "08:00:00");
    }

    #[tokio::test]
    async fn test_departures_limit_parameter() {
        let svc = create_schedule_service(build_test_data());
        // STOP_B has departures on ROUTE_1 (08:15) and ROUTE_2 (09:00).
        // With a wide window, both should appear – but limit=1 truncates.
        let results = svc
            .get_departures_at_stop(TEST_GTFS_ID, STOP_B, Some("08:00:00"), None, 120, 1)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_departures_multiple_at_stop() {
        let svc = create_schedule_service(build_test_data());
        // STOP_B has departures from both routes within a 2-hour window.
        let results = svc
            .get_departures_at_stop(TEST_GTFS_ID, STOP_B, Some("08:00:00"), None, 120, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_departures_bad_time_format() {
        let svc = create_schedule_service(build_test_data());
        let result = svc
            .get_departures_at_stop(TEST_GTFS_ID, STOP_A, Some("invalid"), None, 60, 10)
            .await;
        assert!(result.is_err());
    }

    // ── get_arrivals_at_stop ────────────────────────────────────────────

    #[tokio::test]
    async fn test_arrivals_valid_stop() {
        let svc = create_schedule_service(build_test_data());
        // STOP_C arrives at 08:30 (ROUTE_1) and 09:20 (ROUTE_2).
        let results = svc
            .get_arrivals_at_stop(TEST_GTFS_ID, STOP_C, Some("08:00:00"), None, 120, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.stop_code == STOP_C));
    }

    #[tokio::test]
    async fn test_arrivals_invalid_stop() {
        let svc = create_schedule_service(build_test_data());
        let results = svc
            .get_arrivals_at_stop(TEST_GTFS_ID, "NO_SUCH_STOP", Some("08:00:00"), None, 60, 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_arrivals_invalid_gtfs_id() {
        let svc = create_schedule_service(build_test_data());
        let result = svc
            .get_arrivals_at_stop("bad_id", STOP_C, Some("08:00:00"), None, 60, 10)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_arrivals_outside_window() {
        let svc = create_schedule_service(build_test_data());
        let results = svc
            .get_arrivals_at_stop(TEST_GTFS_ID, STOP_C, Some("12:00:00"), None, 30, 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_arrivals_limit() {
        let svc = create_schedule_service(build_test_data());
        let results = svc
            .get_arrivals_at_stop(TEST_GTFS_ID, STOP_C, Some("08:00:00"), None, 120, 1)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_arrivals_boundary_inclusive() {
        let svc = create_schedule_service(build_test_data());
        // STOP_C arrives at 08:30:00 = 30600s. Query from exactly 08:30:00 with 1-min window.
        let results = svc
            .get_arrivals_at_stop(TEST_GTFS_ID, STOP_C, Some("08:30:00"), None, 1, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].arrival_time, "08:30:00");
    }

    // ── get_trips_between_stops ─────────────────────────────────────────

    #[tokio::test]
    async fn test_trips_between_valid_route() {
        let svc = create_schedule_service(build_test_data());
        // ROUTE_1 goes A -> B -> C. Querying A -> C should find it.
        let results = svc
            .get_trips_between_stops(
                TEST_GTFS_ID, STOP_A, STOP_C, None, None, None, 120,
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].origin_stop_code, STOP_A);
        assert_eq!(results[0].destination_stop_code, STOP_C);
        assert_eq!(results[0].departure_time, "08:00:00");
        assert_eq!(results[0].arrival_time, "08:30:00");
        assert_eq!(results[0].num_stops, 3); // A, B, C
        assert_eq!(results[0].route_short_name, Some("R1".to_string()));
    }

    #[tokio::test]
    async fn test_trips_between_no_direct_route() {
        let svc = create_schedule_service(build_test_data());
        // STOP_A -> STOP_D: no single route covers this.
        let results = svc
            .get_trips_between_stops(
                TEST_GTFS_ID, STOP_A, STOP_D, None, None, None, 120,
            )
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_trips_between_reverse_direction() {
        let svc = create_schedule_service(build_test_data());
        // C -> A is reverse direction on ROUTE_1. Should yield no results.
        let results = svc
            .get_trips_between_stops(
                TEST_GTFS_ID, STOP_C, STOP_A, None, None, None, 120,
            )
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_trips_between_depart_after_filter() {
        let svc = create_schedule_service(build_test_data());
        // ROUTE_1 departs A at 08:00. depart_after=09:00 with 60-min window should miss it.
        let results = svc
            .get_trips_between_stops(
                TEST_GTFS_ID,
                STOP_A,
                STOP_C,
                None,
                Some("09:00:00"),
                None,
                60,
            )
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_trips_between_depart_after_match() {
        let svc = create_schedule_service(build_test_data());
        // depart_after=07:30 with 60-min window should include 08:00.
        let results = svc
            .get_trips_between_stops(
                TEST_GTFS_ID,
                STOP_A,
                STOP_C,
                None,
                Some("07:30:00"),
                None,
                60,
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_trips_between_arrive_before_filter() {
        let svc = create_schedule_service(build_test_data());
        // ROUTE_1 arrives at C at 08:30. arrive_before=08:00 with 60-min window (07:00-08:00).
        // 08:30 > 08:00 so it should be excluded.
        let results = svc
            .get_trips_between_stops(
                TEST_GTFS_ID,
                STOP_A,
                STOP_C,
                None,
                None,
                Some("08:00:00"),
                60,
            )
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_trips_between_arrive_before_match() {
        let svc = create_schedule_service(build_test_data());
        // arrive_before=09:00 with 60-min window (08:00-09:00). 08:30 is within range.
        let results = svc
            .get_trips_between_stops(
                TEST_GTFS_ID,
                STOP_A,
                STOP_C,
                None,
                None,
                Some("09:00:00"),
                60,
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_trips_between_invalid_gtfs_id() {
        let svc = create_schedule_service(build_test_data());
        let result = svc
            .get_trips_between_stops("bad_gtfs", STOP_A, STOP_C, None, None, None, 120)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_trips_between_multiple_results() {
        let svc = create_schedule_service(build_test_data());
        // B -> C appears on both ROUTE_1 and ROUTE_2.
        let results = svc
            .get_trips_between_stops(
                TEST_GTFS_ID, STOP_B, STOP_C, None, None, None, 120,
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        // Should be sorted by departure time.
        assert!(results[0].departure_time <= results[1].departure_time);
    }

    // ── get_next_services ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_next_services_single() {
        let svc = create_schedule_service(build_test_data());
        // STOP_A only has departures at 08:00 (ROUTE_1). From 07:50 should find it.
        let results = svc
            .get_next_services(TEST_GTFS_ID, STOP_A, None, Some("07:50:00"), None, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].stop_code, STOP_A);
        assert_eq!(results[0].departure_time, "08:00:00");
    }

    #[tokio::test]
    async fn test_next_services_multiple() {
        let svc = create_schedule_service(build_test_data());
        // STOP_B has departures at 08:15 (R1) and 09:00 (R2). From 08:00 should find both.
        let results = svc
            .get_next_services(TEST_GTFS_ID, STOP_B, None, Some("08:00:00"), None, 10)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_next_services_mode_filter() {
        let svc = create_schedule_service(build_test_data());
        // STOP_B: R1=BUS, R2=METRO. Filter to METRO only.
        let modes = vec!["METRO".to_string()];
        let results = svc
            .get_next_services(
                TEST_GTFS_ID,
                STOP_B,
                None,
                Some("08:00:00"),
                Some(&modes),
                10,
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].mode, Some("METRO".to_string()));
    }

    #[tokio::test]
    async fn test_next_services_mode_filter_case_insensitive() {
        let svc = create_schedule_service(build_test_data());
        let modes = vec!["bus".to_string()];
        let results = svc
            .get_next_services(
                TEST_GTFS_ID,
                STOP_B,
                None,
                Some("08:00:00"),
                Some(&modes),
                10,
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].mode, Some("BUS".to_string()));
    }

    #[tokio::test]
    async fn test_next_services_no_future_departures() {
        let svc = create_schedule_service(build_test_data());
        // Query from 23:00 – no departures after that.
        let results = svc
            .get_next_services(TEST_GTFS_ID, STOP_A, None, Some("23:00:00"), None, 10)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_next_services_limit() {
        let svc = create_schedule_service(build_test_data());
        let results = svc
            .get_next_services(TEST_GTFS_ID, STOP_B, None, Some("08:00:00"), None, 1)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_next_services_invalid_gtfs() {
        let svc = create_schedule_service(build_test_data());
        let result = svc
            .get_next_services("wrong", STOP_A, None, Some("08:00:00"), None, 10)
            .await;
        assert!(result.is_err());
    }

    // ── get_quality_report ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_quality_report_with_data() {
        let svc = create_schedule_service(build_test_data());
        let report = svc.get_quality_report(TEST_GTFS_ID).await.unwrap();
        assert_eq!(report.gtfs_id, TEST_GTFS_ID);
        assert_eq!(report.total_routes, 2);
        assert_eq!(report.total_stops, 4);
        assert_eq!(report.total_trips, 2);
        assert_eq!(report.routes_with_trips, 2);
        assert_eq!(report.routes_without_trips, 0);
        // All 4 stops appear in trips.
        assert_eq!(report.stops_with_departures, 4);
        assert_eq!(report.stops_without_departures, 0);
        assert_eq!(report.orphaned_stops, 0);
        assert_eq!(report.data_hash, Some("abc123hash".to_string()));
    }

    #[tokio::test]
    async fn test_quality_report_with_orphaned_stops() {
        let mut data = build_test_data();
        // Add an extra stop that does not appear in any trip.
        if let Some(stop_data) = data.stops_by_gtfs.get_mut(TEST_GTFS_ID) {
            stop_data.stops.insert(
                "ORPHAN_STOP".to_string(),
                GTFSStop {
                    id: "id_orphan".to_string(),
                    code: "ORPHAN_STOP".to_string(),
                    name: "Orphan Station".to_string(),
                    lat: 13.0,
                    lon: 77.6,
                    station_id: None,
                    cluster: None,
                    hindi_name: None,
                    regional_name: None,
                },
            );
        }
        let svc = create_schedule_service(data);
        let report = svc.get_quality_report(TEST_GTFS_ID).await.unwrap();
        assert_eq!(report.total_stops, 5);
        assert_eq!(report.orphaned_stops, 1);
        assert_eq!(report.stops_without_departures, 1);
    }

    #[tokio::test]
    async fn test_quality_report_routes_without_trips() {
        let mut data = build_test_data();
        // Add a route with no corresponding trip.
        if let Some(route_map) = data.routes_by_gtfs.get_mut(TEST_GTFS_ID) {
            route_map.insert(
                "ROUTE_ORPHAN".to_string(),
                NandiRoutesRes {
                    id: "ROUTE_ORPHAN".to_string(),
                    short_name: Some("RO".to_string()),
                    long_name: None,
                    mode: "BUS".to_string(),
                    agency_name: None,
                    trip_count: None,
                    stop_count: None,
                    start_point: None,
                    end_point: None,
                    service_tier_type: None,
                },
            );
        }
        let svc = create_schedule_service(data);
        let report = svc.get_quality_report(TEST_GTFS_ID).await.unwrap();
        assert_eq!(report.total_routes, 3);
        assert_eq!(report.routes_with_trips, 2);
        assert_eq!(report.routes_without_trips, 1);
    }

    #[tokio::test]
    async fn test_quality_report_empty_data() {
        let mut data = GTFSData::new();
        // Create empty entries so the report doesn't return NotFound.
        data.route_example_trip_details_by_gtfs
            .insert(TEST_GTFS_ID.to_string(), HashMap::new());
        let svc = create_schedule_service(data);
        let report = svc.get_quality_report(TEST_GTFS_ID).await.unwrap();
        assert_eq!(report.total_routes, 0);
        assert_eq!(report.total_stops, 0);
        assert_eq!(report.total_trips, 0);
    }

    // ── clean_identifier ────────────────────────────────────────────────

    #[test]
    fn test_clean_identifier_plain() {
        assert_eq!(clean_identifier("STOP_A"), "STOP_A");
    }

    #[test]
    fn test_clean_identifier_with_prefix() {
        assert_eq!(clean_identifier("gtfs_id:STOP_A"), "STOP_A");
    }

    #[test]
    fn test_clean_identifier_url_encoded() {
        assert_eq!(clean_identifier("STOP%20A"), "STOP A");
    }

    // ── Response field validation ───────────────────────────────────────

    #[tokio::test]
    async fn test_departure_response_fields() {
        let svc = create_schedule_service(build_test_data());
        let results = svc
            .get_departures_at_stop(TEST_GTFS_ID, STOP_A, Some("07:50:00"), None, 20, 10)
            .await
            .unwrap();
        let dep = &results[0];
        assert_eq!(dep.trip_id, "trip_1");
        assert_eq!(dep.route_id, ROUTE_1);
        assert_eq!(dep.route_short_name, Some("R1".to_string()));
        assert_eq!(dep.stop_code, STOP_A);
        assert_eq!(dep.stop_name, Some("Stop A Station".to_string()));
        assert_eq!(dep.departure_time, "08:00:00");
        assert_eq!(dep.headsign, Some("City Center".to_string()));
        assert_eq!(dep.stop_sequence, 1);
        assert_eq!(dep.mode, Some("BUS".to_string()));
    }

    #[tokio::test]
    async fn test_arrival_response_fields() {
        let svc = create_schedule_service(build_test_data());
        let results = svc
            .get_arrivals_at_stop(TEST_GTFS_ID, STOP_C, Some("08:25:00"), None, 10, 10)
            .await
            .unwrap();
        let arr = &results[0];
        assert_eq!(arr.trip_id, "trip_1");
        assert_eq!(arr.route_id, ROUTE_1);
        assert_eq!(arr.stop_code, STOP_C);
        assert_eq!(arr.arrival_time, "08:30:00");
        assert_eq!(arr.stop_sequence, 3);
    }

    #[tokio::test]
    async fn test_trip_response_fields() {
        let svc = create_schedule_service(build_test_data());
        let results = svc
            .get_trips_between_stops(
                TEST_GTFS_ID, STOP_A, STOP_B, None, None, None, 120,
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        let trip = &results[0];
        assert_eq!(trip.trip_id, "trip_1");
        assert_eq!(trip.origin_stop_name, Some("Stop A Station".to_string()));
        assert_eq!(
            trip.destination_stop_name,
            Some("Stop B Station".to_string())
        );
        assert_eq!(trip.num_stops, 2); // A and B
        assert_eq!(trip.mode, Some("BUS".to_string()));
    }
}
