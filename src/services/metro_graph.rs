/// Metro Transit Graph Module
///
/// Contains the data structures for a metro transit graph where:
/// - Nodes = stops (metro stations)
/// - Edges = connections between consecutive stops on a trip (with travel time)
/// - Transfer edges = connections between stops at the same station or via transfers.txt
///
/// The graph is loaded from a preprocessed JSON file produced by the Nandi
/// preprocessor pipeline. All graph building is done at preprocessing time.
///
/// Supports time-dependent routing by storing departure schedules per edge.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Graph data structures ──────────────────────────────────────────────

/// A node in the metro graph representing a stop/station.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetroNode {
    pub stop_id: String,
    pub stop_name: String,
    pub lat: f64,
    pub lon: f64,
    pub parent_station: Option<String>,
    pub platform_code: Option<String>,
}

/// A scheduled departure on an edge (for time-dependent routing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeSchedule {
    /// Departure time from the origin stop in seconds since midnight
    pub departure_time: i32,
    /// Arrival time at the destination stop in seconds since midnight
    pub arrival_time: i32,
    /// Trip ID this schedule belongs to
    pub trip_id: String,
}

/// An edge in the metro graph connecting two stops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetroEdge {
    /// Destination stop_id
    pub to_stop_id: String,
    /// Minimum travel time in seconds (used for non-time-dependent queries)
    pub min_travel_time_secs: i32,
    /// Route ID this edge belongs to (None for transfer edges)
    pub route_id: Option<String>,
    /// Route short name / line name
    pub route_name: Option<String>,
    /// Whether this is a transfer/walking edge
    pub is_transfer: bool,
    /// All scheduled departures on this edge (sorted by departure_time)
    pub schedules: Vec<EdgeSchedule>,
}

/// A segment of a routing result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetroRouteSegment {
    pub from_stop_id: String,
    pub from_stop_name: String,
    pub to_stop_id: String,
    pub to_stop_name: String,
    pub route_id: Option<String>,
    pub route_name: Option<String>,
    pub is_transfer: bool,
    pub departure_time_secs: Option<i32>,
    pub arrival_time_secs: Option<i32>,
    pub travel_time_secs: i32,
    /// Intermediate stops on this segment (not including from/to)
    pub intermediate_stops: Vec<MetroStopInfo>,
}

/// Basic stop info for route results.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetroStopInfo {
    pub stop_id: String,
    pub stop_name: String,
    pub lat: f64,
    pub lon: f64,
}

/// The result of a metro route calculation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetroRouteResult {
    pub total_time_secs: i32,
    pub total_stops: usize,
    pub num_transfers: usize,
    pub segments: Vec<MetroRouteSegment>,
    pub from_stop: MetroStopInfo,
    pub to_stop: MetroStopInfo,
}

/// The complete metro transit graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetroGraph {
    pub gtfs_id: String,
    pub nodes: HashMap<String, MetroNode>,
    /// Adjacency list: stop_id -> list of edges
    pub adjacency: HashMap<String, Vec<MetroEdge>>,
    /// Map route_id -> route short name
    pub route_names: HashMap<String, String>,
    /// Map route_id -> ordered list of stop_ids (for the canonical/longest pattern)
    pub route_stop_sequences: HashMap<String, Vec<String>>,
}

impl MetroGraph {
    /// Get node info, returns None if not found.
    pub fn get_node(&self, stop_id: &str) -> Option<&MetroNode> {
        self.nodes.get(stop_id)
    }

    /// Get all edges from a given stop.
    pub fn get_edges(&self, stop_id: &str) -> &[MetroEdge] {
        self.adjacency
            .get(stop_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Find the next available departure on an edge at or after a given time.
    pub fn next_departure(edge: &MetroEdge, after_time: i32) -> Option<&EdgeSchedule> {
        // Binary search for the first departure >= after_time
        let idx = edge
            .schedules
            .partition_point(|s| s.departure_time < after_time);
        edge.schedules.get(idx)
    }

    /// Get all stop IDs in the graph.
    pub fn all_stop_ids(&self) -> Vec<&String> {
        self.nodes.keys().collect()
    }

    /// Find stops near a given lat/lon within a radius (in meters).
    pub fn find_nearby_stops(&self, lat: f64, lon: f64, radius_meters: f64) -> Vec<&MetroNode> {
        self.nodes
            .values()
            .filter(|node| {
                let dist = haversine_distance_meters(lat, lon, node.lat, node.lon);
                dist <= radius_meters
            })
            .collect()
    }
}

// ── Haversine distance utility ─────────────────────────────────────────

const EARTH_RADIUS_METERS: f64 = 6_371_000.0;

/// Calculate haversine distance between two points in meters.
pub fn haversine_distance_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();

    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();

    EARTH_RADIUS_METERS * c
}

// ── Time parsing utility ───────────────────────────────────────────────

/// Parse a GTFS/HH:MM:SS time string into seconds since midnight.
/// Supports hours >= 24 (for trips crossing midnight).
pub fn parse_time_str(s: &str) -> Option<i32> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 2 {
        return None;
    }
    let hours: i32 = parts[0].trim().parse().ok()?;
    let minutes: i32 = parts[1].trim().parse().ok()?;
    let seconds: i32 = if parts.len() > 2 {
        parts[2].trim().parse().unwrap_or(0)
    } else {
        0
    };
    Some(hours * 3600 + minutes * 60 + seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_haversine_distance() {
        // Delhi to nearby point (~1km)
        let dist = haversine_distance_meters(28.6139, 77.2090, 28.6229, 77.2090);
        assert!((dist - 1000.0).abs() < 50.0); // ~1km within 50m tolerance
    }

    #[test]
    fn test_parse_time_str() {
        assert_eq!(parse_time_str("08:30:00"), Some(30600));
        assert_eq!(parse_time_str("25:00:00"), Some(90000));
        assert_eq!(parse_time_str("08:30"), Some(30600));
        assert_eq!(parse_time_str("invalid"), None);
    }
}
