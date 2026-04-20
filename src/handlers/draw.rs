use actix_web::{
    web::{Data, Path},
    HttpResponse,
};

use crate::environment::AppState;
use crate::tools::error::AppResult;

/// Handler for /draw/{gtfs_id}/{route_code}
/// Renders an HTML page with a Leaflet map showing all stops for a route
pub async fn draw_route_map(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, route_code) = path.into_inner();

    // Fetch route stop mappings
    let mappings = app_state
        .gtfs_service
        .get_route_stop_mapping_by_route(&gtfs_id, &route_code)
        .await?;

    // Generate HTML with Leaflet map
    let html = generate_leaflet_html(&gtfs_id, &route_code, &mappings);

    Ok(HttpResponse::Ok()
        .content_type("text/html")
        .body(html))
}

/// Escape special HTML characters to prevent XSS attacks
fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Generate HTML string with embedded Leaflet map
fn generate_leaflet_html(
    gtfs_id: &str,
    route_code: &str,
    mappings: &[std::sync::Arc<crate::models::RouteStopMapping>],
) -> String {
    // Sanitize user inputs to prevent XSS
    let safe_gtfs_id = html_escape(gtfs_id);
    let safe_route_code = html_escape(route_code);
    // Build stops data for JavaScript
    let mut stops_js = String::new();
    for (idx, mapping) in mappings.iter().enumerate() {
        if idx > 0 {
            stops_js.push_str(",\n");
        }
        stops_js.push_str(&format!(
            "            {{ lat: {}, lng: {}, name: {:?}, code: {:?}, sequence: {} }}",
            mapping.stop_point.lat,
            mapping.stop_point.lon,
            mapping.stop_name.as_ref(),
            mapping.stop_code.as_ref(),
            mapping.sequence_num
        ));
    }

    // Calculate center point (average of all stops)
    let (center_lat, center_lng) = if mappings.is_empty() {
        (19.0760, 72.8777) // Default to Mumbai coordinates
    } else {
        let sum_lat: f64 = mappings.iter().map(|m| m.stop_point.lat).sum();
        let sum_lng: f64 = mappings.iter().map(|m| m.stop_point.lon).sum();
        (
            sum_lat / mappings.len() as f64,
            sum_lng / mappings.len() as f64,
        )
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Route Map - {route_code}</title>
    <link rel="stylesheet" href="/static/leaflet.css"/>
    <script src="/static/leaflet.js"></script>
    <style>
        body {{
            margin: 0;
            padding: 0;
            font-family: Arial, sans-serif;
        }}
        #header {{
            background-color: #2c3e50;
            color: white;
            padding: 15px;
            text-align: center;
        }}
        #header h1 {{
            margin: 0;
            font-size: 24px;
        }}
        #header p {{
            margin: 5px 0 0 0;
            font-size: 14px;
            opacity: 0.9;
        }}
        #map {{
            height: calc(100vh - 80px);
            width: 100%;
        }}
        .stop-popup {{
            font-size: 14px;
        }}
        .stop-popup strong {{
            color: #2c3e50;
        }}
    </style>
</head>
<body>
    <div id="header">
        <h1>Route {route_code}</h1>
        <p>GTFS ID: {gtfs_id} | Total Stops: {total_stops}</p>
    </div>
    <div id="map"></div>
    <script>
        // Initialize the map
        var map = L.map('map').setView([{center_lat}, {center_lng}], 13);

        // Add OpenStreetMap tile layer
        L.tileLayer('https://{{s}}.tile.openstreetmap.org/{{z}}/{{x}}/{{y}}.png', {{
            attribution: '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors'
        }}).addTo(map);

        // Stops data
        var stops = [
{stops_js}
        ];

        // Add markers for each stop
        var bounds = L.latLngBounds();
        stops.forEach(function(stop, index) {{
            var marker = L.marker([stop.lat, stop.lng])
                .addTo(map)
                .bindPopup('<div class="stop-popup"><strong>' + stop.name + '</strong><br/>' +
                          'Code: ' + stop.code + '<br/>' +
                          'Sequence: ' + stop.sequence + '</div>');
            
            // Add tooltip showing sequence number
            marker.bindTooltip(String(index + 1), {{
                permanent: true,
                direction: 'top',
                className: 'stop-sequence-label'
            }});
            
            bounds.extend([stop.lat, stop.lng]);
        }});

        // Fit map to show all stops
        if (stops.length > 0) {{
            map.fitBounds(bounds, {{ padding: [50, 50] }});
        }}

        // Draw polyline connecting stops in sequence order
        var sortedStops = stops.slice().sort(function(a, b) {{
            return a.sequence - b.sequence;
        }});
        
        if (sortedStops.length > 1) {{
            var latlngs = sortedStops.map(function(stop) {{
                return [stop.lat, stop.lng];
            }});
            
            var polyline = L.polyline(latlngs, {{
                color: '#3498db',
                weight: 4,
                opacity: 0.8,
                dashArray: '10, 10'
            }}).addTo(map);
        }}
    </script>
</body>
</html>"#,
        route_code = safe_route_code,
        gtfs_id = safe_gtfs_id,
        total_stops = mappings.len(),
        center_lat = center_lat,
        center_lng = center_lng,
        stops_js = stops_js
    )
}
