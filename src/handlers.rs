use crate::manager::StreamManager;
use crate::models::{HealthResponse, StatusResponse, StreamStatus};
use axum::{
    body::Body,
    extract::{Path, Query, Request},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Json, Response},
};
use chrono::Utc;
use serde::Deserialize;
use std::sync::{Arc, atomic::Ordering};
use tracing::warn;
use tracing::{debug, error, info};

#[derive(Debug, Deserialize)]
pub struct ContextQuery {
    hls_ctx: Option<String>,
}

/// Main HLS content handler that routes based on path pattern
pub async fn serve_hls_content(
    Path(path): Path<String>,
    Query(query): Query<ContextQuery>,
    _req: Request,
    manager: Arc<StreamManager>,
) -> impl IntoResponse {
    debug!(
        "HLS request for path: {} with context: {:?}",
        path, query.hls_ctx
    );

    // Parse the path to determine what's being requested
    let parts: Vec<&str> = path.split('/').collect();

    if path.ends_with(".m3u8") {
        // Playlist request
        match parts.len() {
            2 => {
                // Format: stream_key/file.m3u8
                let stream_key = parts[0];
                let filename = parts[1];

                if filename == "master.m3u8" {
                    // ABR master playlist
                    serve_abr_master_playlist(stream_key, manager).await
                } else if filename == "index.m3u8" {
                    // Single bitrate or legacy format
                    if query.hls_ctx.is_none() {
                        // No context - serve with redirect for session tracking
                        serve_master_playlist(stream_key, manager).await
                    } else {
                        // Has context - serve actual playlist and track session
                        serve_actual_playlist(stream_key, query.hls_ctx, manager).await
                    }
                } else {
                    Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::from("Unknown playlist file"))
                        .unwrap()
                }
            }
            3 => {
                // Format: stream_key/variant/index.m3u8
                let stream_key = parts[0];
                let variant = parts[1];
                let filename = parts[2];

                if filename == "index.m3u8" {
                    // Serve variant playlist
                    serve_variant_playlist(stream_key, variant, manager).await
                } else {
                    Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::from("Invalid variant playlist path"))
                        .unwrap()
                }
            }
            _ => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Invalid playlist path"))
                .unwrap(),
        }
    } else if path.ends_with(".ts") {
        // Segment file
        match parts.len() {
            2 => {
                // Format: stream_key/segment.ts (single bitrate)
                let stream_key = parts[0];
                let segment = parts[1];
                serve_segment(stream_key, segment, manager).await
            }
            3 => {
                // Format: stream_key/variant/segment.ts (ABR)
                let stream_key = parts[0];
                let variant = parts[1];
                let segment = parts[2];
                serve_variant_segment(stream_key, variant, segment, manager).await
            }
            _ => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Invalid segment path"))
                .unwrap(),
        }
    } else {
        Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("Invalid file type"))
            .unwrap()
    }
}

/// Serve master playlist that redirects to actual playlist with context
async fn serve_master_playlist(stream_key: &str, manager: Arc<StreamManager>) -> Response<Body> {
    debug!(
        "Master playlist request for stream: {} (no context)",
        stream_key
    );

    // Ensure stream is started
    if !manager.wait_for_stream(stream_key.to_string()).await {
        // Check if stream failed (RTMP source doesn't exist)
        if manager.failed_streams.contains_key(stream_key) {
            info!("Stream {} failed - RTMP source not found", stream_key);
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Stream not found"))
                .unwrap();
        }

        // Stream couldn't start for other reasons
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("Retry-After", "5")
            .body(Body::from("Stream not available"))
            .unwrap();
    }

    // Update stats
    manager.stats.requests.fetch_add(1, Ordering::Relaxed);

    // Generate a new session for this viewer
    let session_id = manager.session_manager.create_session(stream_key);

    // Check if this is an ABR stream (master.m3u8 exists)
    let master_path = manager.hls_path.join(stream_key).join("master.m3u8");
    let redirect_path = if master_path.exists() {
        // ABR stream - redirect to the real master.m3u8
        format!("/live/{}/master.m3u8?hls_ctx={}", stream_key, session_id)
    } else {
        // Single bitrate - redirect to index.m3u8
        format!("/live/{}/index.m3u8?hls_ctx={}", stream_key, session_id)
    };

    // Generate master playlist with redirect
    let master_playlist = format!(
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,AVERAGE-BANDWIDTH=1\n{}",
        redirect_path
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(master_playlist))
        .unwrap()
}

/// Serve actual playlist with context tracking
async fn serve_actual_playlist(
    stream_key: &str,
    session_id: Option<String>,
    manager: Arc<StreamManager>,
) -> Response<Body> {
    debug!(
        "Actual playlist request for stream: {} with session: {:?}",
        stream_key, session_id
    );

    // Update session if context provided
    if let Some(sid) = &session_id {
        manager.session_manager.update_session(sid);
    }

    // Update stats and stream access
    manager.stats.requests.fetch_add(1, Ordering::Relaxed);
    manager.update_stream_access(stream_key).await;

    // Read the actual playlist file generated by FFmpeg
    let playlist_path = manager.hls_path.join(stream_key).join("index.m3u8");

    match tokio::fs::read(&playlist_path).await {
        Ok(data) => {
            // Convert the playlist content to string to modify segment URLs
            let content = String::from_utf8_lossy(&data);

            // Add the stream_key prefix to segment URLs in the playlist
            let modified_content = content
                .lines()
                .map(|line| {
                    if line.ends_with(".ts") {
                        // This is a segment filename, prepend the path
                        format!("/live/{}/{}", stream_key, line)
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            let content_size = modified_content.len();
            debug!(
                "Serving playlist for stream {} ({} bytes)",
                stream_key, content_size
            );

            // Track stats
            manager
                .stats
                .playlists_served
                .fetch_add(1, Ordering::Relaxed);
            manager
                .stats
                .bytes_served
                .fetch_add(content_size as u64, Ordering::Relaxed);

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
                .header(header::CACHE_CONTROL, "no-cache")
                .body(Body::from(modified_content))
                .unwrap()
        }
        Err(e) => {
            error!("Failed to read playlist file {:?}: {}", playlist_path, e);
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Playlist not found"))
                .unwrap()
        }
    }
}

/// Serve segment files
async fn serve_segment(
    stream_key: &str,
    segment: &str,
    manager: Arc<StreamManager>,
) -> Response<Body> {
    debug!(
        "Segment request for stream: {}, segment: {}",
        stream_key, segment
    );

    // Update stats
    manager.stats.requests.fetch_add(1, Ordering::Relaxed);

    // Update stream access asynchronously (fire and forget)
    let manager_clone = manager.clone();
    let stream_key_clone = stream_key.to_string();
    tokio::spawn(async move {
        manager_clone.update_stream_access(&stream_key_clone).await;
    });

    // Build the segment file path
    let segment_path = manager.hls_path.join(stream_key).join(segment);

    // Use cache for segments
    let cache_key = format!("{}/{}", stream_key, segment);
    match manager.cache.get_or_load(&cache_key, &segment_path).await {
        Ok(Some((cached_segment, was_cached))) => {
            let data_size = cached_segment.data.len();
            if was_cached {
                debug!("Cache hit: {} ({} bytes)", segment, data_size);
            } else {
                info!("Cache miss: {} ({} bytes)", segment, data_size);
            }

            // Track stats
            manager
                .stats
                .segments_served
                .fetch_add(1, Ordering::Relaxed);
            manager
                .stats
                .bytes_served
                .fetch_add(data_size as u64, Ordering::Relaxed);

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, cached_segment.content_type)
                .header(header::CACHE_CONTROL, "max-age=10")
                .body(Body::from(cached_segment.data))
                .unwrap()
        }
        Ok(None) => {
            error!("Segment not found: {} at path {:?}", segment, segment_path);
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Segment not found"))
                .unwrap()
        }
        Err(e) => {
            error!("Error serving segment {}: {}", segment, e);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Internal server error"))
                .unwrap()
        }
    }
}

pub async fn health_check(manager: Arc<StreamManager>) -> impl IntoResponse {
    let active_count = manager.active_streams.len();
    let pending_count = manager.pending_streams.len();

    // Get stats snapshot and update viewer count
    manager.stats.total_viewers.store(
        manager.session_manager.get_total_viewer_count() as u64,
        Ordering::Relaxed,
    );
    let stats = manager.stats.to_snapshot();

    let healthy = active_count < manager.max_concurrent_streams;

    let response = HealthResponse {
        status: if healthy {
            "healthy".to_string()
        } else {
            "unhealthy".to_string()
        },
        active_streams: active_count,
        pending_streams: pending_count,
        max_streams: manager.max_concurrent_streams,
        stats,
    };

    if healthy {
        (StatusCode::OK, Json(response))
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(response))
    }
}

pub async fn status(manager: Arc<StreamManager>) -> impl IntoResponse {
    let now = Utc::now();

    let mut active_streams = Vec::new();
    for entry in manager.active_streams.iter() {
        let key = entry.key();
        let info = entry.value();
        let last_accessed = info.last_accessed.read().await;

        active_streams.push(StreamStatus {
            key: key.clone(),
            pid: info.pid,
            uptime: now.signed_duration_since(info.started_at).num_seconds(),
            last_accessed: now.signed_duration_since(*last_accessed).num_seconds(),
            viewers: manager.session_manager.get_stream_viewer_count(key),
        });
    }

    let pending_streams: Vec<String> = manager
        .pending_streams
        .iter()
        .map(|entry| entry.key().clone())
        .collect();

    let failed_streams: Vec<String> = manager
        .failed_streams
        .iter()
        .map(|entry| entry.key().clone())
        .collect();

    manager.stats.total_viewers.store(
        manager.session_manager.get_total_viewer_count() as u64,
        Ordering::Relaxed,
    );
    let stats = manager.stats.to_snapshot();

    let cache_stats = Some(manager.cache.stats().await);
    let uptime_seconds = now
        .signed_duration_since(manager.server_started_at)
        .num_seconds();

    // Get throughput and metrics history
    let throughput = manager.metrics_history.get_current_throughput().await;
    let metrics_history = manager.metrics_history.get_history().await;

    Json(StatusResponse {
        active_streams,
        pending_streams,
        failed_streams,
        stats,
        cache_stats,
        uptime_seconds,
        throughput,
        metrics_history,
    })
}

pub async fn dashboard() -> impl IntoResponse {
    // Try multiple paths to support both local development and Docker
    let paths = [
        "dashboard.html",  // Local development (relative to where cargo run is executed)
        "/dashboard.html", // Docker container root
        "cascade/dashboard.html", // Alternative local path
    ];

    for path in &paths {
        match tokio::fs::read_to_string(path).await {
            Ok(html) => {
                debug!("Successfully loaded dashboard from {}", path);
                return Html(html).into_response();
            }
            Err(_) => continue,
        }
    }

    error!("Failed to read dashboard.html from any known location");
    (StatusCode::INTERNAL_SERVER_ERROR, "Dashboard not available").into_response()
}

/// Serve ABR master playlist
async fn serve_abr_master_playlist(
    stream_key: &str,
    manager: Arc<StreamManager>,
) -> Response<Body> {
    debug!("ABR master playlist request for stream: {}", stream_key);

    // Update stream access for viewer tracking
    manager.update_stream_access(stream_key).await;

    // Track session if context is provided (viewer tracking)
    // The context would have been set by the initial redirect from index.m3u8

    // Check if stream exists
    if !manager.active_streams.contains_key(stream_key) {
        // Try to start the stream
        match manager.start_stream(stream_key.to_string()).await {
            Ok(started) => {
                if !started {
                    return Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .body(Body::from("Stream not available"))
                        .unwrap();
                }
            }
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("Failed to start stream"))
                    .unwrap();
            }
        }
    }

    // Path to master playlist
    let master_path = manager.hls_path.join(stream_key).join("master.m3u8");

    // Read and serve the master playlist
    match tokio::fs::read_to_string(&master_path).await {
        Ok(content) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/vnd.apple.mpegurl")
            .header("Cache-Control", "no-cache")
            .body(Body::from(content))
            .unwrap(),
        Err(e) => {
            error!("Failed to read master playlist for {}: {}", stream_key, e);
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Master playlist not found"))
                .unwrap()
        }
    }
}

/// Serve variant-specific playlist
async fn serve_variant_playlist(
    stream_key: &str,
    variant: &str,
    manager: Arc<StreamManager>,
) -> Response<Body> {
    debug!(
        "Variant playlist request for stream: {}, variant: {}",
        stream_key, variant
    );

    // Update access time
    manager.update_stream_access(stream_key).await;

    // Path to variant playlist
    let playlist_path = manager
        .hls_path
        .join(stream_key)
        .join(variant)
        .join("index.m3u8");

    // Read and serve the playlist (no caching for playlists)
    match tokio::fs::read_to_string(&playlist_path).await {
        Ok(content) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/vnd.apple.mpegurl")
            .header("Cache-Control", "no-cache")
            .body(Body::from(content))
            .unwrap(),
        Err(e) => {
            error!(
                "Failed to read variant playlist for {}/{}: {}",
                stream_key, variant, e
            );
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Variant playlist not found"))
                .unwrap()
        }
    }
}

/// Serve variant-specific segment
async fn serve_variant_segment(
    stream_key: &str,
    variant: &str,
    segment: &str,
    manager: Arc<StreamManager>,
) -> Response<Body> {
    debug!(
        "Variant segment request: {}/{}/{}",
        stream_key, variant, segment
    );

    // Update access time
    manager.update_stream_access(stream_key).await;

    // Build the segment file path
    let segment_path = manager
        .hls_path
        .join(stream_key)
        .join(variant)
        .join(segment);

    // Use cache for segments
    let cache_key = format!("{}/{}/{}", stream_key, variant, segment);
    match manager.cache.get_or_load(&cache_key, &segment_path).await {
        Ok(Some((cached_segment, was_cached))) => {
            let data_size = cached_segment.data.len();
            if was_cached {
                debug!("Cache hit: {}/{} ({} bytes)", variant, segment, data_size);
            } else {
                info!("Cache miss: {}/{} ({} bytes)", variant, segment, data_size);
            }

            manager
                .stats
                .bytes_served
                .fetch_add(data_size as u64, std::sync::atomic::Ordering::Relaxed);
            manager
                .stats
                .segments_served
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "video/mp2t")
                .header("Content-Length", data_size.to_string())
                .header("Cache-Control", "public, max-age=31536000")
                .body(Body::from(cached_segment.data))
                .unwrap()
        }
        Ok(None) => {
            warn!("Segment not found: {}/{}/{}", stream_key, variant, segment);
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Segment not found"))
                .unwrap()
        }
        Err(e) => {
            error!(
                "Failed to load segment {}/{}/{}: {}",
                stream_key, variant, segment, e
            );
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Failed to load segment"))
                .unwrap()
        }
    }
}
