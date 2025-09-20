# Use cargo-chef for efficient Docker layer caching
FROM rust:1.90 AS chef
WORKDIR /app

# Planner stage - prepare the build recipe
FROM chef AS planner
COPY . ./
RUN cargo chef prepare --recipe-path recipe.json

# Builder stage
FROM chef AS builder
COPY . .

RUN apt-get update && apt-get install -y \
    musl-dev \
    libssl-dev \
    pkg-config \
    libavcodec-dev \
    libavformat-dev \
    libavutil-dev \
    libswscale-dev \
    libavdevice-dev \
    libavfilter-dev \
    pkg-config \
    libclang-dev \
    clang

RUN cargo build --release --features ssl --bin cascade

# Runtime stage
FROM rust:1.90

RUN apt-get update && apt-get install -y \
    ffmpeg \
    supervisor \
    curl \
    gcc \
    ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Create directories
RUN mkdir -p /hls /var/log/supervisor

# Copy the compiled binary from builder
COPY --from=builder /app/target/release/cascade /cascade

# Make it executable
RUN chmod +x /cascade

# Copy supervisor config
COPY supervisor/supervisord-rust.conf /etc/supervisor/conf.d/supervisord.conf

CMD ["/usr/bin/supervisord", "-c", "/etc/supervisor/conf.d/supervisord.conf"]
