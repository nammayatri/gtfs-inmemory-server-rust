# GTFS In Memory Service (Rust)

A high-performance GTFS (General Transit Feed Specification) in memory service written in Rust, providing real-time access to transit data with automatic polling and caching.

## Features

- **High Performance**: Built with Rust and Axum for maximum performance
- **Automatic Data Polling**: Continuously fetches and updates GTFS data
- **Intelligent Caching**: In-memory caching with change detection
- **RESTful API**: Comprehensive REST endpoints for transit data
- **Error Handling**: Robust error handling with proper HTTP status codes
- **Logging**: Structured logging with tracing
- **Configuration**: Environment-based configuration
- **Health Checks**: Readiness probe endpoint

## Prerequisites

- Rust 1.70+ (install via [rustup](https://rustup.rs/))
- GTFS data source (OTP server or similar)

## Installation

1. Clone the repository:

```bash
git clone <repository-url>
cd gtfs-routes-service-rust
```

2. Build the project:

```bash
cargo build --release
```

3. Create a `.env` file with your configuration:

```env
GTFS_BASE_URL=http://localhost:8080
GTFS_POLLING_INTERVAL=30
GTFS_API_HOST=0.0.0.0
GTFS_API_PORT=8000
GTFS_MAX_RETRIES=3
GTFS_RETRY_DELAY=5
GTFS_CPU_THRESHOLD=80.0
GTFS_MEMORY_THRESHOLD=5000
```

4. Run the service:

```bash
cargo run --release
```

## Configuration

| Environment Variable    | Default                 | Description                          |
| ----------------------- | ----------------------- | ------------------------------------ |
| `GTFS_BASE_URL`         | `http://localhost:8080` | Base URL of the GTFS data source     |
| `GTFS_POLLING_INTERVAL` | `30`                    | Polling interval in seconds          |
| `GTFS_API_HOST`         | `0.0.0.0`               | Host to bind the API server          |
| `GTFS_API_PORT`         | `8000`                  | Port to bind the API server          |
| `GTFS_MAX_RETRIES`      | `3`                     | Maximum retry attempts for API calls |
| `GTFS_RETRY_DELAY`      | `5`                     | Delay between retries in seconds     |
| `GTFS_CPU_THRESHOLD`    | `80.0`                  | CPU usage threshold percentage       |
| `GTFS_MEMORY_THRESHOLD` | `5000`                  | Memory usage threshold in MB         |

## API Endpoints

### Health Check

- `GET /ready` - Readiness probe

### Routes

- `GET /route/{gtfs_id}/{route_id}` - Get specific route
- `GET /routes/{gtfs_id}` - Get all routes for a GTFS ID
- `GET /routes/{gtfs_id}/fuzzy/{query}?limit={limit}` - Fuzzy search routes

### Route-Stop Mappings

- `GET /route-stop-mapping/{gtfs_id}/route/{route_code}?direction={direction}` - Get stops for a route (with optional direction filter)
- `GET /route-stop-mapping/{gtfs_id}/stop/{stop_code}?direction={direction}` - Get routes for a stop (with optional direction filter)

### Stops

- `GET /stops/{gtfs_id}` - Get all stops for a GTFS ID
- `GET /stop/{gtfs_id}/{stop_code}` - Get specific stop
- `GET /stops/{gtfs_id}/fuzzy/{query}?limit={limit}` - Fuzzy search stops

### Stop Code From Provider Stop Code

- `GET /stop-code/{gtfs_id}/provider_stop_code}` - Get stop code from provider stop code

### Station Children

- `GET /station-children/{gtfs_id}/{stop_code}` - Get child stations

### Version

- `GET /version/{gtfs_id}` - Get data version hash

## Suburban Stop Info Integration

The system now supports direction-based filtering for route stop mappings using data from the `suburban_stop_info.csv` file. This feature allows you to filter route stop mappings by direction (e.g., "MSB", "CGL") when querying the API.

For detailed information about this feature, see [Suburban Stop Info Documentation](docs/SUBURBAN_STOP_INFO.md).

## Example Usage

### Get all routes for a GTFS ID

```bash
curl http://localhost:8000/routes/chennai_data
```

### Get specific route

```bash
curl http://localhost:8000/route/chennai_data/12345
```

### Fuzzy search routes

```bash
curl "http://localhost:8000/routes/chennai_data/fuzzy/bus?limit=10"
```

### Get stops for a route

```bash
# Get all stops for a route
curl http://localhost:8000/route-stop-mapping/chennai_data/route/12345

# Get stops for a route with direction filter
curl "http://localhost:8000/route-stop-mapping/chennai_data/route/12345?direction=MSB"
```

### Get routes for a stop

```bash
# Get all routes for a stop
curl http://localhost:8000/route-stop-mapping/chennai_data/stop/STOP001

# Get routes for a stop with direction filter
curl "http://localhost:8000/route-stop-mapping/chennai_data/stop/STOP001?direction=MSB"
```

### Get stop code from provider stop code

```bash
curl http://localhost:8000/stop-code/chennai_data/PROVIDER001
```

## Testing with Newman

[Newman](https://github.com/postmanlabs/newman) is a command-line collection runner for Postman. You can use it to run the included Postman collection for comprehensive API testing.

### Prerequisites

Install Newman globally:

```bash
npm install -g newman
```

Port Forward To Nandi:

```bash
kubectl port-forward service/beckn-nandi-master 8090:8080 -n atlas
```

### Running the Collection

Run the entire test collection with the local environment:

```bash
newman run GIMS.postman_collection.json --env-var "gtfs-inmemory=http://localhost:8085"
```

### Running Specific Tests

You can also run specific requests from the collection:

```bash
# Run only the "Get All Routes" test
newman run GIMS.postman_collection.json --folder "Get All Routes" --env-var "gtfs-inmemory=http://localhost:8085"
```

### Environment Variables

The collection uses the following environment variable:

- `gtfs-inmemory` - Base URL for the GTFS service (default: `http://localhost:8085`)

### Sample Collection Tests

The collection includes tests for:

- GraphQL queries for trips with stop codes
- Getting all routes and specific routes
- Fuzzy matching for routes and stops
- Route-stop mappings
- Station information and children
- Provider stop code lookups

## Response Format

### Route Response

```json
{
  "id": "12345",
  "short_name": "1A",
  "long_name": "Route 1A - Downtown Express",
  "mode": "BUS",
  "agency_name": "Transit Agency",
  "trip_count": 15,
  "stop_count": 25,
  "start_point": {
    "lat": 12.9716,
    "lon": 77.5946
  },
  "end_point": {
    "lat": 13.0827,
    "lon": 77.5877
  }
}
```

### Route-Stop Mapping Response

```json
[
  {
    "estimated_travel_time_from_previous_stop": null,
    "provider_code": "GTFS",
    "route_code": "12345",
    "sequence_num": 0,
    "stop_code": "STOP001",
    "stop_name": "Central Station",
    "stop_point": {
      "lat": 12.9716,
      "lon": 77.5946
    },
    "vehicle_type": "BUS"
  }
]
```

### Provider Stop Code Response

```json
{
  "gtfs_id": "chennai_data",
  "provider_stop_code": "PROVIDER001",
  "stop_code": "STOP001"
}
```

## Error Handling

The service returns appropriate HTTP status codes:

- `200` - Success
- `404` - Resource not found
- `503` - Service not ready
- `500` - Internal server error

Error responses include a JSON object with error details:

```json
{
  "error": "Route not found: chennai_data/99999",
  "status": 404
}
```

## Performance Considerations

- **Memory Usage**: The service keeps all GTFS data in memory for fast access
- **Change Detection**: Uses SHA-256 hashing to detect data changes and avoid unnecessary updates
- **Connection Pooling**: HTTP client uses connection pooling for efficient API calls
- **Rate Limiting**: Built-in rate limiting to prevent overwhelming the data source

## Development

### Running Tests

```bash
cargo test
```

### Running with Debug Logging

```bash
RUST_LOG=debug cargo run
```

### Building for Production

```bash
cargo build --release
```

## Docker

### Build Docker Image

```bash
docker build -t gtfs-routes-service .
```

### Run with Docker

```bash
docker run -p 8000:8000 --env-file .env gtfs-routes-service
```

## Monitoring

The service provides several monitoring endpoints:

- `/ready` - Health check for load balancers
- Logging includes performance metrics and error tracking
- Memory and CPU usage monitoring

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Add tests
5. Submit a pull request

## License

This project is licensed under the MIT License - see the LICENSE file for details.
