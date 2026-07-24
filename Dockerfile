# Preprocessed GTFS data comes from a versioned data image (built by the Nandi pipeline), not git.
ARG DATA_IMAGE=gtfs-data:latest
FROM ${DATA_IMAGE} AS gtfsdata

# Multi-stage build for Rust application
FROM rust:slim as builder

# Install build dependencies
RUN apt-get update && apt-get install -y \
    curl \
    htop \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /app

# Copy Cargo files
COPY Cargo.toml Cargo.lock* ./
COPY ./log-processor log-processor

# Create a dummy main.rs to build dependencies
RUN mkdir -p assets && mkdir -p src && echo "fn main() {}" > src/main.rs

# Build dependencies (this layer will be cached)
RUN cargo build --release && rm -rf target/release/.fingerprint/gtfs-routes-service-*

# Remove dummy main.rs and copy actual source code
RUN rm src/main.rs
COPY src ./src
# Committed assets = CSVs only; the preprocessed JSON/shards come from the data image below.
COPY assets ./assets

# Build the application
RUN cargo build --release

COPY dhall-configs ./dhall-configs

# Preprocessed data from the pinned image, after the build so a data change doesn't bust the Rust cache.
COPY --from=gtfsdata /data/ ./assets/

# Pre-build snapshot.bin for fast boot (best-effort; pod falls back to JSON if skipped).
RUN DHALL_CONFIG=./dhall-configs/dev/build_snapshot.dhall \
    ./target/release/gtfs-routes-service --build-snapshot \
    || echo "WARN: snapshot build skipped (data image had no preprocessed JSON); pod will use JSON path"

# Runtime stage
FROM ubuntu:24.04

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -r -s /bin/false app

# Set working directory
WORKDIR /app

# Copy binary from builder stage
COPY --from=builder /app/target/release/gtfs-routes-service /app/gtfs-routes-service
COPY --from=builder /app/assets /app/assets
COPY --from=builder /app/dhall-configs /app/dhall-configs
COPY --from=builder /app/log-processor /usr/sbin/log-processor

# Default to preprocessed mode; k8s overrides DHALL_CONFIG with a mounted config.
ENV DHALL_CONFIG=./dhall-configs/dev/build_snapshot.dhall

# Set proper permissions
RUN chmod +x /usr/sbin/log-processor

# Expose port
EXPOSE 8000

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8000/ready || exit 1

# Run the application
CMD ["./gtfs-routes-service"]
