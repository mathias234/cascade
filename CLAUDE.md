# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Cascade is a high-performance HLS (HTTP Live Streaming) edge server built in Rust. It converts RTMP streams to HLS format and serves them with optimized caching and performance features.

## Commands

### Development
```bash
# Build the Rust application
cargo build

# Run in development mode
cargo run

# Run tests
cargo test

# Check code (linting and type checking)
cargo check
cade && cargo clippy

# Format code
cargo fmt
```

### Docker/Production
```bash
# Build and run with Docker Compose
docker-compose up --build

# Run in background
docker-compose up -d

# View logs
docker-compose logs -f cascade
```

## Architecture

### Core Components

1. **Stream Manager** (`cascade/src/manager.rs`): 
   - Manages FFmpeg processes for RTMP-to-HLS conversion
   - Handles stream lifecycle (start, monitor, cleanup)
   - Maintains stream state and statistics

2. **HTTP Server** (`cascade/src/main.rs` & `handlers.rs`):
   - Axum-based web server on port 8080
   - Serves HLS playlists (.m3u8) and segments (.ts)
   - Provides health and status endpoints

3. **Caching System** (`cascade/src/cache.rs`):
   - Selective caching strategy:
     - **M3U8 playlists**: Never cached - always read fresh from disk (they change constantly)
     - **TS segments**: In-memory caching for all segments
   - Built on Moka cache with:
     - 30-second TTL for automatic expiration
     - LRU eviction policy with configurable max capacity
     - Lock-free concurrent access
   - Performance features:
     - Atomic load-or-compute prevents duplicate disk reads
   - Cache statistics tracking (hits/misses/memory usage)

4. **Viewer Tracking** (`cascade/src/sessions.rs`):
   - Privacy-first session-based viewer counting
   - No storage of IP addresses or User-Agents
   - Random session IDs via HLS context parameters
   - DashMap-based storage for lock-free concurrent access
   - Automatic cleanup of expired sessions (30s timeout)

### Key Endpoints

- `GET /live/{stream_key}.m3u8` - Master playlist that redirects with session context
- `GET /live/{stream_key}/index.m3u8?hls_ctx={session}` - Actual playlist with viewer tracking
- `GET /live/{stream_key}/{segment}.ts` - Video segment files
- `GET /health` - Health check with stream capacity info and total viewer count
- `GET /status` - Detailed status of all streams including viewer counts per stream

### Environment Configuration

Key environment variables (set in .env file):
- `SOURCE_HOST` - RTMP source server hostname
- `SOURCE_PORT` - RTMP port (default: 1935)
- `HLS_PATH` - Directory for HLS output (default: /hls)
- `MAX_CONCURRENT_STREAMS` - Stream limit (default: 50)
- `STREAM_TIMEOUT` - Idle stream timeout in seconds
- `PORT` - HTTP server port (default: 8080)
- `VIEWER_TRACKING_ENABLED` - Enable viewer tracking (default: true)
- `VIEWER_TIMEOUT_SECONDS` - Seconds before marking viewer inactive (default: 30)

### FFmpeg Process Management

The system spawns FFmpeg processes with:
- Input: RTMP stream from source server
- Output: HLS segments in `/hls/{stream_key}/`
- Automatic cleanup on stream timeout or failure

## Development Notes

- The project uses Rust 2024 edition with async/await via Tokio
- All file I/O operations should be async when possible
- Cache operations are performance-critical - test thoroughly
- Stream lifecycle events are logged at debug level
- Failed streams are tracked and have retry mechanisms

### Cache Implementation Details

- **M3U8 handling**: Playlists bypass cache entirely (`handlers.rs:64-86`) - served with `no-cache` header
- **TS segment caching**: Only .ts video segments are cached
- **Cache size limits**: 100MB max file size, configurable max entries
- **Invalidation**: Stream-specific cache invalidation using `invalidate_entries_if` - removes all segments for a stream
- **Statistics**: Real-time tracking of cache hits, misses, and memory usage for monitoring

### Viewer Tracking Implementation

- **Session-based**: Each viewer gets a unique random session ID via HLS context parameter
- **Privacy-first**: No IP addresses or User-Agents stored anywhere
- **Master playlist redirect**: Initial request generates session and redirects to actual playlist
- **Storage**: DashMap for lock-free concurrent access (no global locks)
- **Cleanup**: Automatic removal of expired sessions after timeout
- **API Integration**: Viewer counts exposed in `/health` and `/status` endpoints
