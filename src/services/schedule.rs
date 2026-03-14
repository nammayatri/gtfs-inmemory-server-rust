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
