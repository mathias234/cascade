# Use cargo-chef for efficient Docker layer caching
FROM lukemathwalker/cargo-chef:latest-rust-alpine3.20 AS chef
WORKDIR /app

# Planner stage - prepare the build recipe
FROM chef AS planner
COPY . ./
RUN cargo chef prepare --recipe-path recipe.json

# Builder stage
FROM chef AS builder
RUN apk add --no-cache musl-dev

# Copy recipe from planner
COPY --from=planner /app/recipe.json recipe.json

# Build dependencies - this is the caching Docker layer!
RUN cargo chef cook --release --recipe-path recipe.json

# Copy the actual source code and build the application
COPY . ./

# Build the release binary (with SSL feature if supported)
# The dependencies are already built by cargo-chef, this should be fast
RUN cargo build --release --features ssl || cargo build --release

# Runtime stage
FROM alpine:3.20

RUN apk add --no-cache \
    ffmpeg \
    supervisor \
    curl \
    libgcc

# Create directories
RUN mkdir -p /hls /var/log/supervisor

# Copy the compiled binary from builder
COPY --from=builder /app/target/release/cascade /cascade

# Copy dashboard HTML file
COPY dashboard.html /dashboard.html

# Make it executable
RUN chmod +x /cascade

# Copy supervisor config
COPY supervisor/supervisord-rust.conf /etc/supervisor/conf.d/supervisord.conf

CMD ["/usr/bin/supervisord", "-c", "/etc/supervisor/conf.d/supervisord.conf"]
