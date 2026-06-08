/// A* Shortest Path Router for Metro Transit
///
/// Uses A* algorithm with haversine distance heuristic to find the
/// optimal path between two metro stops. Supports both:
/// - Time-independent routing (uses minimum edge weights)
/// - Time-dependent routing (uses scheduled departure times)

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use crate::services::metro_graph::{
    haversine_distance_meters, MetroGraph, MetroRouteResult, MetroRouteSegment, MetroStopInfo,
};
use crate::tools::error::{AppError, AppResult};

/// Maximum assumed metro speed in m/s (~80 km/h) for the A* heuristic.
/// This ensures the heuristic is admissible (never overestimates).
const MAX_METRO_SPEED_MPS: f64 = 22.22;

/// A* node in the open set priority queue.
#[derive(Debug, Clone)]
struct AStarNode {
    /// Total estimated cost: g(n) + h(n)
    f_score: i64,
    /// Cost from start to this node (stored for priority queue ordering)
    _g_score: i64,
    /// Stop ID
    stop_id: String,
}

impl PartialEq for AStarNode {
    fn eq(&self, other: &Self) -> bool {
        self.stop_id == other.stop_id
    }
}
impl Eq for AStarNode {}

impl PartialOrd for AStarNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AStarNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // We use Reverse wrapping in the BinaryHeap, so this gives us a min-heap
        self.f_score.cmp(&other.f_score)
    }
}

/// Predecessor record for path reconstruction.
#[derive(Debug, Clone)]
struct Predecessor {
    from_stop_id: String,
    route_id: Option<String>,
    route_name: Option<String>,
    is_transfer: bool,
    travel_time_secs: i32,
    departure_time_secs: Option<i32>,
    arrival_time_secs: Option<i32>,
}

/// Find the shortest path from `from_stop_id` to `to_stop_id`.
///
/// - If `departure_time` is Some, uses time-dependent routing.
/// - If `departure_time` is None, uses minimum travel times.
pub fn find_shortest_path(
    graph: &MetroGraph,
    from_stop_id: &str,
    to_stop_id: &str,
    departure_time: Option<i32>,
) -> AppResult<MetroRouteResult> {
    // Validate inputs
    let from_node = graph.get_node(from_stop_id).ok_or_else(|| {
        AppError::NotFound(format!("Source stop not found: {}", from_stop_id))
    })?;
    let to_node = graph.get_node(to_stop_id).ok_or_else(|| {
        AppError::NotFound(format!("Destination stop not found: {}", to_stop_id))
    })?;

    if from_stop_id == to_stop_id {
        return Ok(MetroRouteResult {
            total_time_secs: 0,
            total_stops: 1,
            num_transfers: 0,
            segments: Vec::new(),
            from_stop: MetroStopInfo {
                stop_id: from_stop_id.to_string(),
                stop_name: from_node.stop_name.clone(),
                lat: from_node.lat,
                lon: from_node.lon,
            },
            to_stop: MetroStopInfo {
                stop_id: to_stop_id.to_string(),
                stop_name: to_node.stop_name.clone(),
                lat: to_node.lat,
                lon: to_node.lon,
            },
        });
    }

    let dest_lat = to_node.lat;
    let dest_lon = to_node.lon;

    // g_score: best known cost from start to each node
    let mut g_score: HashMap<String, i64> = HashMap::new();
    // predecessor map for path reconstruction
    let mut predecessors: HashMap<String, Predecessor> = HashMap::new();
    // track current time at each node (for time-dependent routing)
    let mut time_at_node: HashMap<String, i32> = HashMap::new();

    // Open set as a min-heap (using Reverse for min ordering)
    let mut open_set: BinaryHeap<Reverse<AStarNode>> = BinaryHeap::new();
    // Closed set
    let mut closed_set: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Initialize
    let h_start = heuristic(from_node.lat, from_node.lon, dest_lat, dest_lon);
    g_score.insert(from_stop_id.to_string(), 0);
    if let Some(dt) = departure_time {
        time_at_node.insert(from_stop_id.to_string(), dt);
    }

    open_set.push(Reverse(AStarNode {
        f_score: h_start,
        _g_score: 0,
        stop_id: from_stop_id.to_string(),
    }));

    while let Some(Reverse(current)) = open_set.pop() {
        let current_stop = &current.stop_id;

        // Goal check
        if current_stop == to_stop_id {
            // Reconstruct path
            return reconstruct_path(
                graph,
                from_stop_id,
                to_stop_id,
                &predecessors,
            );
        }

        // Skip if already processed
        if closed_set.contains(current_stop) {
            continue;
        }
        closed_set.insert(current_stop.clone());

        let current_g = *g_score.get(current_stop).unwrap_or(&i64::MAX);
        let current_time = time_at_node.get(current_stop).copied();

        // Expand neighbors
        for edge in graph.get_edges(current_stop) {
            if closed_set.contains(&edge.to_stop_id) {
                continue;
            }

            let (travel_time, dep_time, arr_time) = if let Some(ct) = current_time {
                // Time-dependent: find next departure
                if edge.is_transfer || edge.schedules.is_empty() {
                    // Transfer edges: can walk at any time
                    let arrival = ct + edge.min_travel_time_secs;
                    (edge.min_travel_time_secs as i64, Some(ct), Some(arrival))
                } else {
                    // Scheduled edge: find next departure at or after current_time
                    match MetroGraph::next_departure(edge, ct) {
                        Some(schedule) => {
                            let wait_time = (schedule.departure_time - ct).max(0);
                            let total_time = wait_time + (schedule.arrival_time - schedule.departure_time).max(0);
                            (
                                total_time as i64,
                                Some(schedule.departure_time),
                                Some(schedule.arrival_time),
                            )
                        }
                        None => {
                            // No more departures today, use min travel time as fallback
                            let arrival = ct + edge.min_travel_time_secs;
                            (
                                edge.min_travel_time_secs as i64,
                                Some(ct),
                                Some(arrival),
                            )
                        }
                    }
                }
            } else {
                // Time-independent: use minimum travel times
                (edge.min_travel_time_secs as i64, None, None)
            };

            let tentative_g = current_g + travel_time;

            if tentative_g < *g_score.get(&edge.to_stop_id).unwrap_or(&i64::MAX) {
                g_score.insert(edge.to_stop_id.clone(), tentative_g);

                if let Some(a) = arr_time {
                    time_at_node.insert(edge.to_stop_id.clone(), a);
                }

                predecessors.insert(
                    edge.to_stop_id.clone(),
                    Predecessor {
                        from_stop_id: current_stop.clone(),
                        route_id: edge.route_id.clone(),
                        route_name: edge.route_name.clone(),
                        is_transfer: edge.is_transfer,
                        travel_time_secs: travel_time as i32,
                        departure_time_secs: dep_time,
                        arrival_time_secs: arr_time,
                    },
                );

                // Calculate heuristic for neighbor
                if let Some(neighbor_node) = graph.get_node(&edge.to_stop_id) {
                    let h =
                        heuristic(neighbor_node.lat, neighbor_node.lon, dest_lat, dest_lon);
                    open_set.push(Reverse(AStarNode {
                        f_score: tentative_g + h,
                        _g_score: tentative_g,
                        stop_id: edge.to_stop_id.clone(),
                    }));
                }
            }
        }
    }

    Err(AppError::NotFound(format!(
        "No path found from {} to {}",
        from_stop_id, to_stop_id
    )))
}

/// Heuristic function: lower-bound travel time based on haversine distance
/// and maximum possible metro speed.
fn heuristic(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> i64 {
    let dist_meters = haversine_distance_meters(lat1, lon1, lat2, lon2);
    (dist_meters / MAX_METRO_SPEED_MPS) as i64
}

/// Reconstruct the path from predecessors map and build MetroRouteResult.
fn reconstruct_path(
    graph: &MetroGraph,
    from_stop_id: &str,
    to_stop_id: &str,
    predecessors: &HashMap<String, Predecessor>,
) -> AppResult<MetroRouteResult> {
    // Walk back from destination to source
    let mut path: Vec<(String, Predecessor)> = Vec::new();
    let mut current = to_stop_id.to_string();

    while current != from_stop_id {
        let pred = predecessors.get(&current).ok_or_else(|| {
            AppError::Internal("Path reconstruction failed: missing predecessor".to_string())
        })?;
        path.push((current.clone(), pred.clone()));
        current = pred.from_stop_id.clone();
    }

    path.reverse();

    // Merge consecutive edges on the same route into segments
    let mut segments: Vec<MetroRouteSegment> = Vec::new();
    let mut total_time = 0i32;
    let mut num_transfers = 0;

    let mut i = 0;
    while i < path.len() {
        let (ref stop_id, ref pred) = path[i];
        let from_stop = pred.from_stop_id.clone();
        let from_node = graph.get_node(&from_stop);
        let to_node = graph.get_node(stop_id);

        if pred.is_transfer {
            // Transfer segment (single hop)
            let segment = MetroRouteSegment {
                from_stop_id: from_stop.clone(),
                from_stop_name: from_node.map(|n| n.stop_name.clone()).unwrap_or_default(),
                to_stop_id: stop_id.clone(),
                to_stop_name: to_node.map(|n| n.stop_name.clone()).unwrap_or_default(),
                route_id: None,
                route_name: Some("Transfer / Walk".to_string()),
                is_transfer: true,
                departure_time_secs: pred.departure_time_secs,
                arrival_time_secs: pred.arrival_time_secs,
                travel_time_secs: pred.travel_time_secs,
                intermediate_stops: Vec::new(),
            };
            total_time += pred.travel_time_secs;
            num_transfers += 1;
            segments.push(segment);
            i += 1;
        } else {
            // Riding segment: merge consecutive edges on the same route
            let route_id = pred.route_id.clone();
            let route_name = pred.route_name.clone();
            let segment_start_stop = from_stop.clone();
            let mut segment_end_stop = stop_id.clone();
            let mut segment_time = pred.travel_time_secs;
            let segment_departure = pred.departure_time_secs;
            let mut segment_arrival = pred.arrival_time_secs;
            let mut intermediate = Vec::new();

            // Collect intermediate stops
            let mut j = i + 1;
            while j < path.len() {
                let (ref next_stop, ref next_pred) = path[j];
                if !next_pred.is_transfer && next_pred.route_id == route_id {
                    // Still on the same route — this stop is intermediate
                    let intermediate_node = graph.get_node(&segment_end_stop);
                    if let Some(node) = intermediate_node {
                        intermediate.push(MetroStopInfo {
                            stop_id: segment_end_stop.clone(),
                            stop_name: node.stop_name.clone(),
                            lat: node.lat,
                            lon: node.lon,
                        });
                    }
                    segment_end_stop = next_stop.clone();
                    segment_time += next_pred.travel_time_secs;
                    segment_arrival = next_pred.arrival_time_secs;
                    j += 1;
                } else {
                    break;
                }
            }

            let from_node_info = graph.get_node(&segment_start_stop);
            let to_node_info = graph.get_node(&segment_end_stop);

            let segment = MetroRouteSegment {
                from_stop_id: segment_start_stop,
                from_stop_name: from_node_info
                    .map(|n| n.stop_name.clone())
                    .unwrap_or_default(),
                to_stop_id: segment_end_stop,
                to_stop_name: to_node_info
                    .map(|n| n.stop_name.clone())
                    .unwrap_or_default(),
                route_id,
                route_name,
                is_transfer: false,
                departure_time_secs: segment_departure,
                arrival_time_secs: segment_arrival,
                travel_time_secs: segment_time,
                intermediate_stops: intermediate,
            };
            total_time += segment_time;
            segments.push(segment);
            i = j;
        }
    }

    // Count total stops
    let mut total_stops = if segments.is_empty() { 1 } else { 1 }; // start stop
    for seg in &segments {
        total_stops += 1 + seg.intermediate_stops.len(); // end stop + intermediates
    }

    let from_node = graph.get_node(from_stop_id);
    let to_node = graph.get_node(to_stop_id);

    Ok(MetroRouteResult {
        total_time_secs: total_time,
        total_stops,
        num_transfers,
        segments,
        from_stop: MetroStopInfo {
            stop_id: from_stop_id.to_string(),
            stop_name: from_node.map(|n| n.stop_name.clone()).unwrap_or_default(),
            lat: from_node.map(|n| n.lat).unwrap_or(0.0),
            lon: from_node.map(|n| n.lon).unwrap_or(0.0),
        },
        to_stop: MetroStopInfo {
            stop_id: to_stop_id.to_string(),
            stop_name: to_node.map(|n| n.stop_name.clone()).unwrap_or_default(),
            lat: to_node.map(|n| n.lat).unwrap_or(0.0),
            lon: to_node.map(|n| n.lon).unwrap_or(0.0),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heuristic_admissible() {
        // Two points ~10km apart
        let h = heuristic(28.6139, 77.2090, 28.7041, 77.1025);
        // At 80 km/h, 10km should take ~450 seconds
        // Heuristic should be <= actual travel time
        assert!(h > 0);
        assert!(h < 600); // Should be reasonable lower bound
    }
}
