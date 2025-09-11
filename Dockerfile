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
COPY assets ./assets

# Build the application
RUN cargo build --release

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
COPY --from=builder /app/log-processor /usr/sbin/log-processor

# Set proper permissions
RUN chmod +x /usr/sbin/log-processor

# Expose port
EXPOSE 8000

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8000/ready || exit 1

# Run the application
CMD ["./gtfs-routes-service"]
