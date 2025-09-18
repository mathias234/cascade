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
2. Copy the configuration file:
   ```bash
   cp config.toml.example config.toml
   ```
3. Edit `config.toml` with your RTMP source server details
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

Configure Cascade using the `config.toml` file. See `config.toml.example` for all available options:

### Key Configuration Sections:

**Server Configuration**
- `server.port` - HTTP server port (default: 8080)
- `server.hls_path` - Directory for HLS output (default: /hls)

**RTMP Source**
- `rtmp.source_host` - RTMP source server hostname
- `rtmp.source_port` - RTMP source server port (default: 1935)

**Stream Management**
- `stream.max_concurrent_streams` - Maximum concurrent streams (default: 50)
- `stream.stream_timeout` - Idle stream timeout in seconds (default: 30)
- `stream.stream_start_timeout` - Stream start timeout in seconds (default: 15)

**Cache Settings**
- `cache.max_entries` - Maximum cached segments (default: 200)
- `cache.max_segment_size` - Max segment size in bytes (default: 10MB)
- `cache.ttl_seconds` - Cache TTL in seconds (default: 300)

**Viewer Tracking**
- `viewer.tracking_enabled` - Enable viewer tracking (default: true)
- `viewer.timeout_seconds` - Viewer timeout in seconds (default: 30)

**Elasticsearch (Optional)**
- `elasticsearch.enabled` - Enable metrics indexing (default: false)
- `elasticsearch.url` - Elasticsearch URL (default: http://localhost:9200)
- `elasticsearch.index_prefix` - Index prefix (default: cascade-metrics)

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
