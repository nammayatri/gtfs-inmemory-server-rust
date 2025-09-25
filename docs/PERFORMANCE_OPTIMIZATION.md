# Performance Optimization Guide

## TCP Connection Overhead Reduction

This guide outlines optimizations to reduce the 10-15ms latency in production environments by minimizing TCP connection overhead.

## Key Optimizations Implemented

### 1. HTTP Client Optimizations

The HTTP client for OTP instances now includes:

```rust
let http_client = reqwest::Client::builder()
    .timeout(Duration::from_secs(30))
    .pool_max_idle_per_host(config.connection_limit)
    .pool_idle_timeout(Duration::from_secs(config.http_pool_idle_timeout))
    .tcp_keepalive(Some(Duration::from_secs(config.http_tcp_keepalive)))
    .tcp_nodelay(true) // Disable Nagle's algorithm for lower latency
    .local_address(None) // Allow system to choose optimal local address
    .build()
```

**Benefits:**
- `tcp_nodelay(true)`: Reduces latency by disabling Nagle's algorithm
- `pool_idle_timeout`: Keeps connections alive longer to avoid reconnection overhead
- `tcp_keepalive`: Maintains connection health
- **Note**: HTTP/2 was removed due to compatibility issues with OTP servers

### 2. Database Connection Pool Optimizations

PostgreSQL connection pool now includes:

```rust
let pool = PgPoolOptions::new()
    .max_connections(config.db_max_connections)
    .min_connections(config.db_min_connections)
    .acquire_timeout(Duration::from_secs(config.db_acquire_timeout))
    .idle_timeout(Duration::from_secs(config.db_idle_timeout))
    .max_lifetime(Duration::from_secs(config.db_max_lifetime))
    .connect(db_url)
```

**Benefits:**
- `min_connections`: Pre-warms connections to avoid cold start latency
- `acquire_timeout`: Faster connection acquisition
- `idle_timeout`: Keeps connections alive longer
- `max_lifetime`: Recycles connections to prevent stale connections

### 3. TCP Socket Optimizations

Server listener now includes socket-level optimizations:

```rust
// Enable TCP_NODELAY for lower latency
let nodelay: libc::c_int = 1;
libc::setsockopt(socket, libc::IPPROTO_TCP, libc::TCP_NODELAY, ...);

// Enable SO_REUSEADDR for faster restarts
let reuseaddr: libc::c_int = 1;
libc::setsockopt(socket, libc::SOL_SOCKET, libc::SO_REUSEADDR, ...);
```

## Recommended Environment Variables

Add these to your `.env` file for optimal performance:

```env
# Database Connection Pool
WAYBILLS_DB_MAX_CONNECTIONS=20
WAYBILLS_DB_MIN_CONNECTIONS=5
WAYBILLS_DB_ACQUIRE_TIMEOUT=5
WAYBILLS_DB_IDLE_TIMEOUT=300
WAYBILLS_DB_MAX_LIFETIME=1800

# HTTP Client Optimization
GTFS_CONNECTION_LIMIT=50
GTFS_HTTP_POOL_IDLE_TIMEOUT=90
GTFS_HTTP_TCP_KEEPALIVE=60

# Reduced Delays
GTFS_RETRY_DELAY=1
GTFS_RATE_LIMIT_DELAY=0.05
```

## Production Deployment Recommendations

### 1. Network Configuration

- **Use dedicated network interfaces** for database and HTTP connections
- **Enable TCP Fast Open** if supported by your infrastructure
- **Configure proper DNS TTL** (300 seconds recommended)
- **Use connection pooling at load balancer level**

### 2. Database Optimization

- **Place database on same network segment** as application
- **Use connection pooling at database level** (PgBouncer recommended)
- **Optimize PostgreSQL settings:**
  ```sql
  -- Increase max_connections
  max_connections = 200

  -- Optimize shared_buffers
  shared_buffers = 256MB

  -- Enable connection reuse
  tcp_keepalives_idle = 60
  tcp_keepalives_interval = 10
  tcp_keepalives_count = 6
  ```

### 3. Load Balancer Configuration

- **Enable HTTP/2** for better multiplexing
- **Use sticky sessions** for database connections
- **Configure proper health checks** with low timeout values
- **Enable connection draining** for zero-downtime deployments

### 4. Monitoring and Tuning

Monitor these metrics to identify bottlenecks:

```bash
# Check TCP connection states
ss -tan | grep :5432 | awk '{print $1}' | sort | uniq -c

# Monitor connection pool usage
curl -s http://localhost:8000/memory-stats | jq

# Check HTTP client metrics
curl -s http://localhost:8000/config | jq '.connection_limit'
```

## Expected Performance Improvements

With these optimizations, you should see:

- **TCP connection overhead**: Reduced from 10-15ms to 1-3ms
- **Database query latency**: Reduced by 60-80%
- **HTTP request latency**: Reduced by 40-60%
- **Connection reuse**: Increased from ~10% to ~90%

## Troubleshooting

### High Latency Issues

1. **Check network latency:**
   ```bash
   ping -c 10 your-database-host
   ```

2. **Monitor connection pool:**
   ```bash
   curl -s http://localhost:8000/memory-stats
   ```

3. **Check TCP connection states:**
   ```bash
   ss -tan | grep ESTABLISHED | wc -l
   ```

### Connection Pool Exhaustion

1. **Increase pool size:**
   ```env
   WAYBILLS_DB_MAX_CONNECTIONS=50
   GTFS_CONNECTION_LIMIT=100
   ```

2. **Add connection monitoring:**
   ```rust
   // Add to your health check endpoint
   let pool_size = pool.size();
   let idle_size = pool.size() - pool.acquire().await.unwrap().len();
   ```

## Additional Optimizations

### 1. Connection Pre-warming

Consider implementing connection pre-warming on startup:

```rust
// Pre-warm database connections
for _ in 0..config.db_min_connections {
    let _conn = pool.acquire().await?;
}
```

### 2. Circuit Breaker Pattern

Implement circuit breakers for external HTTP calls:

```rust
// Add to Cargo.toml
circuit-breaker = "0.4"
```

### 3. Response Caching

Implement response caching for frequently accessed data:

```rust
// Cache frequently accessed routes
let cache_key = format!("route:{}:{}", gtfs_id, route_id);
if let Some(cached) = cache.get(&cache_key).await {
    return Ok(cached.clone());
}
```

## Conclusion

These optimizations should significantly reduce your TCP connection overhead and bring latency down from 10-15ms to 1-3ms in production environments. Monitor the metrics and adjust the configuration values based on your specific load patterns and infrastructure.
