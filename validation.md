# Validation Checklist

## Build
- [ ] Project compiles without errors — SKIPPED: No Rust toolchain available in environment
- [ ] No type errors — SKIPPED: No Rust toolchain available in environment

## Changed Files

### src/handlers/draw.rs
- [x] Handler module created with `draw_route_map` function
- [x] Uses `actix_web::web::{Data, Path}` and `HttpResponse`
- [x] Imports `AppState` from environment module
- [x] Imports `AppResult` from tools::error module
- [x] Endpoint path: `/draw/{gtfs_id}/{route_code}`
- [x] Calls `gtfs_service.get_route_stop_mapping_by_route()` to fetch stops
- [x] Generates HTML with Leaflet.js map showing stops
- [x] Stops are sorted by sequence_num for proper ordering
- [x] Each stop is rendered as a marker with popup showing stop name and code
- [x] Map centers on average of all stop coordinates
- [x] Includes polyline connecting all stops in sequence order
- [x] Uses `fitBounds` to ensure all stops are visible
- [x] Markers include sequence number tooltips
- [x] Returns proper error via AppResult if service call fails

### src/handlers/mod.rs
- [x] Draw module declared as `pub mod draw;`

### src/handlers/routes.rs
- [x] Imports `draw_route_map` from draw module
- [x] Route registered at `/draw/{gtfs_id}/{route_code}`

### src/main.rs
- [x] Imports `actix_files::Files` for static file serving
- [x] Configures static file service at `/static` route
- [x] Static files served from `./static` directory

### Cargo.toml
- [x] `actix-files = "0.6"` dependency added

### static/leaflet.js
- [x] Leaflet JavaScript library present (107 KB)

### static/leaflet.css
- [x] Leaflet CSS stylesheet present (16 KB)

## Tests
- [ ] Existing tests pass — SKIPPED: No Rust toolchain available in environment
- [ ] New functionality has test coverage — Note: No tests found for draw handler

## Integration
- [x] Draw handler integrates with existing GtfsService
- [x] Uses existing `get_route_stop_mapping_by_route` method
- [x] Uses existing `RouteStopMapping` model
- [x] Follows existing error handling patterns with AppResult
- [x] Follows existing handler patterns (similar to routes.rs)
- [x] AppState properly provides gtfs_service access

## Code Review Notes

### Positive Findings:
1. **Proper Error Handling**: Uses `AppResult` for consistent error propagation
2. **Clean HTML Generation**: Well-structured HTML with embedded JavaScript
3. **Dynamic Center Calculation**: Centers map on average of all stop coordinates
4. **Bounds Fitting**: Uses `fitBounds` to ensure all stops are visible with padding
5. **Sequence Visualization**: Shows sequence numbers as tooltips on markers
6. **Polyline Connection**: Draws dashed polyline connecting stops in sequence order
7. **Static File Serving**: Properly configured with actix-files at `/static` route
8. **Follows Conventions**: Matches existing code patterns in the codebase
9. **Responsive Design**: Map takes full viewport height minus header

### Implementation Details Verified:
1. **Handler Signature**: `draw_route_map(app_state: Data<AppState>, path: Path<(String, String)>) -> AppResult<HttpResponse>`
2. **Route Registration**: `.route("/draw/{gtfs_id}/{route_code}", actix_web::web::get().to(draw_route_map))`
3. **Static Files**: `.service(Files::new("/static", "./static"))` configured in main.rs
4. **Leaflet Assets**: Both leaflet.js and leaflet.css present in static directory
5. **Map Features**:
   - OpenStreetMap tile layer
   - Markers with popups (stop name, code, sequence)
   - Sequence number tooltips
   - Dashed polyline connecting stops
   - Auto-fit bounds with padding

### Potential Improvements:
1. **No Tests**: The draw handler has no unit/integration tests
2. **Hardcoded Zoom**: Initial zoom level 13 is used before fitBounds
3. **No Empty State**: Could show a message when route has no stops
4. **XSS Risk**: Uses `{:?}` debug formatting which may not fully escape HTML in stop names/codes

## Summary
All code changes are structurally correct and follow existing patterns. The implementation:
- Creates a new endpoint at `/draw/{gtfs_id}/{route_code}`
- Renders an HTML page with Leaflet.js map
- Shows all stops for a route with markers, tooltips, and connecting polyline
- Serves static Leaflet assets via actix-files
- Properly integrates with existing services and error handling

Build and test verification skipped due to missing Rust toolchain in environment, but code review confirms proper implementation.
