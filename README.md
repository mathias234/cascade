# Cascade

A high-performance HLS (HTTP Live Streaming) edge server built in Rust. Cascade converts RTMP streams to HLS format and serves them with optimized caching and performance features.

## Features

- **RTMP to HLS Conversion**: Seamlessly converts RTMP streams to HLS format using FFmpeg
- **Smart Caching**: In-memory caching system for optimal performance
- **Viewer Tracking**: Anonymous session-based viewer counting with no PII storage
- **High Performance**: Built with Rust and Tokio for excellent concurrent performance
- **Auto-scaling**: Manages multiple concurrent streams with configurable limits
- **Health Monitoring**: Built-in health checks and stream status endpoints with viewer metrics

## Quick Start

### Using Docker Compose

1. Clone the repository
2. Copy the environment configuration:
   ```bash
   cp .env.example .env
   ```
3. Edit `.env` with your RTMP source server details
4. Start the service:
   ```bash
   docker-compose up -d
   ```

### Development

Build and run locally:
```bash
cargo build --release
cargo run
```

## Configuration

Configure Cascade using environment variables in your `.env` file:

- `SOURCE_HOST` - RTMP source server hostname
- `SOURCE_PORT` - RTMP source server port (default: 1935)
- `STREAM_TIMEOUT` - Idle stream timeout in seconds (default: 30)
- `MAX_CONCURRENT_STREAMS` - Maximum concurrent streams (default: 50)
- `HLS_PATH` - Directory for HLS output (default: /hls)
- `PORT` - HTTP server port (default: 8080)
- `VIEWER_TRACKING_ENABLED` - Enable viewer tracking (default: true)
- `VIEWER_TIMEOUT_SECONDS` - Seconds before marking viewer inactive (default: 30)

## API Endpoints

- `GET /live/{stream_key}.m3u8` - Master playlist with session tracking
- `GET /live/{stream_key}/index.m3u8?hls_ctx={session}` - Actual HLS playlist
- `GET /live/{stream_key}/{segment}.ts` - HLS video segments
- `GET /health` - Health check endpoint with total viewer count
- `GET /status` - Detailed status of all active streams including viewer counts

## Architecture

### Caching Strategy
Cascade uses a sophisticated caching strategy:
- **M3U8 Playlists**: Always served fresh from disk (never cached)
- **TS Segments**: In-memory caching for fast segment delivery

### Viewer Tracking
- **Anonymous**: Random session IDs, no personal data collected
- **Accurate**: Each player instance gets a unique session
- **Automatic cleanup**: Sessions expire after configurable timeout

## License

MIT
