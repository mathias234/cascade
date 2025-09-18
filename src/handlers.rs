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
use std::path::PathBuf;
use std::sync::{Arc, atomic::Ordering};
use tracing::{debug, error, info};

#[derive(Debug, Deserialize)]
pub struct ContextQuery {
    hls_ctx: Option<String>,
}

/// Represents different types of HLS requests
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) enum HlsRequestType {
    /// Initial request without session - needs redirect for viewer tracking
    InitialRequest {
        stream_key: String,
    },

    /// Playlist request (master.m3u8, index.m3u8, or variant/index.m3u8)
    Playlist {
        stream_key: String,
        playlist_path: PathBuf,
        session: Option<String>,
        needs_url_rewrite: bool,
    },

    /// Segment request (.ts files)
    Segment {
        stream_key: String,
        segment_path: PathBuf,
    },
}

/// Parse the incoming path into a structured HLS request type
pub(crate) fn parse_hls_request(path: &str, session: Option<String>) -> Result<HlsRequestType, &'static str> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if parts.is_empty() {
        return Err("Invalid path");
    }

    let stream_key = parts[0];

    // Handle different path patterns
    match parts.len() {
        1 => {
            // Just stream_key - shouldn't happen with our routing
            Err("Invalid path format")
        }
        2 => {
            let filename = parts[1];

            if filename.ends_with(".m3u8") {
                // Could be master.m3u8 or index.m3u8
                if filename == "index.m3u8" && session.is_none() {
                    // Initial request without session - need redirect
                    Ok(HlsRequestType::InitialRequest {
                        stream_key: stream_key.to_string(),
                    })
                } else if filename == "master.m3u8" {
                    // ABR master playlist
                    Ok(HlsRequestType::Playlist {
                        stream_key: stream_key.to_string(),
                        playlist_path: PathBuf::from(filename),
                        session,
                        needs_url_rewrite: false, // ABR master doesn't need URL rewriting
                    })
                } else if filename == "index.m3u8" {
                    // Single bitrate playlist with session
                    Ok(HlsRequestType::Playlist {
                        stream_key: stream_key.to_string(),
                        playlist_path: PathBuf::from(filename),
                        session,
                        needs_url_rewrite: true, // Single bitrate needs URL rewriting
                    })
                } else {
                    Err("Unknown playlist file")
                }
            } else if filename.ends_with(".ts") {
                // Single bitrate segment
                Ok(HlsRequestType::Segment {
                    stream_key: stream_key.to_string(),
                    segment_path: PathBuf::from(filename),
                })
            } else {
                Err("Invalid file type")
            }
        }
        3 => {
            let variant = parts[1];
            let filename = parts[2];

            if filename == "index.m3u8" {
                // Variant playlist
                Ok(HlsRequestType::Playlist {
                    stream_key: stream_key.to_string(),
                    playlist_path: PathBuf::from(format!("{}/{}", variant, filename)),
                    session,
                    needs_url_rewrite: false, // Variant playlists served as-is
                })
            } else if filename.ends_with(".ts") {
                // Variant segment
                Ok(HlsRequestType::Segment {
                    stream_key: stream_key.to_string(),
                    segment_path: PathBuf::from(format!("{}/{}", variant, filename)),
                })
            } else {
                Err("Invalid variant file")
            }
        }
        _ => Err("Invalid path depth")
    }
}

/// Ensure a stream is ready for serving
async fn ensure_stream_ready(stream_key: &str, manager: &Arc<StreamManager>) -> bool {
    // Wait for stream to be ready, starting it if necessary
    if !manager.wait_for_stream(stream_key.to_string()).await {
        // Check if stream failed (RTMP source doesn't exist)
        if manager.failed_streams.contains_key(stream_key) {
            info!("Stream {} failed - RTMP source not found", stream_key);
            return false;
        }
        // Stream couldn't start for other reasons
        return false;
    }
    true
}

/// Rewrite playlist URLs to include the stream key prefix
pub(crate) fn rewrite_playlist_urls(content: &str, stream_key: &str) -> String {
    content
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
        .join("\n")
}

/// Track request statistics
fn track_request_stats(manager: &Arc<StreamManager>, bytes_served: usize) {
    manager.stats.requests.fetch_add(1, Ordering::Relaxed);
    manager.stats.bytes_served.fetch_add(bytes_served as u64, Ordering::Relaxed);
}

/// Unified function to serve playlist files from disk
async fn serve_playlist(
    stream_key: &str,
    playlist_path: PathBuf,
    needs_url_rewrite: bool,
    manager: Arc<StreamManager>,
) -> Response<Body> {
    debug!(
        "Serving playlist for stream: {}, path: {:?}, rewrite: {}",
        stream_key, playlist_path, needs_url_rewrite
    );

    // Update stream access
    manager.update_stream_access(stream_key).await;

    // Build the full file path
    let file_path = manager.hls_path.join(stream_key).join(&playlist_path);

    // Read the playlist file
    match tokio::fs::read_to_string(&file_path).await {
        Ok(content) => {
            // Optionally rewrite URLs in the playlist
            let final_content = if needs_url_rewrite {
                rewrite_playlist_urls(&content, stream_key)
            } else {
                content
            };

            let content_size = final_content.len();
            debug!(
                "Serving playlist {} ({} bytes)",
                playlist_path.display(),
                content_size
            );

            // Track stats
            manager.stats.playlists_served.fetch_add(1, Ordering::Relaxed);
            track_request_stats(&manager, content_size);

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
                .header(header::CACHE_CONTROL, "no-cache")
                .body(Body::from(final_content))
                .unwrap()
        }
        Err(e) => {
            error!(
                "Failed to read playlist file {:?}: {}",
                file_path, e
            );
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Playlist not found"))
                .unwrap()
        }
    }
}

/// Handle initial request and create session for viewer tracking
async fn handle_initial_request_with_session(
    stream_key: &str,
    manager: Arc<StreamManager>,
) -> Response<Body> {
    debug!(
        "Initial request for stream: {} (creating session)",
        stream_key
    );

    // Ensure stream is started
    if !ensure_stream_ready(stream_key, &manager).await {
        if manager.failed_streams.contains_key(stream_key) {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Stream not found"))
                .unwrap();
        }
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("Retry-After", "5")
            .body(Body::from("Stream not available"))
            .unwrap();
    }

    // Update stats
    track_request_stats(&manager, 0);

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

    // Generate redirect playlist
    let redirect_playlist = format!(
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,AVERAGE-BANDWIDTH=1\n{}",
        redirect_path
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(redirect_playlist))
        .unwrap()
}

/// Unified function to serve segment files
async fn serve_segment_file(
    stream_key: &str,
    segment_path: PathBuf,
    manager: Arc<StreamManager>,
) -> Response<Body> {
    debug!(
        "Segment request for stream: {}, path: {:?}",
        stream_key, segment_path
    );

    // Update stats
    track_request_stats(&manager, 0); // Will update with actual size later

    // Update stream access asynchronously (fire and forget)
    let manager_clone = manager.clone();
    let stream_key_clone = stream_key.to_string();
    tokio::spawn(async move {
        manager_clone.update_stream_access(&stream_key_clone).await;
    });

    // Build the segment file path
    let file_path = manager.hls_path.join(stream_key).join(&segment_path);

    // Use cache for segments
    let cache_key = format!("{}/{}", stream_key, segment_path.display());
    match manager.cache.get_or_load(&cache_key, &file_path).await {
        Ok(Some((cached_segment, was_cached))) => {
            let data_size = cached_segment.data.len();
            if was_cached {
                debug!("Cache hit: {:?} ({} bytes)", segment_path, data_size);
            } else {
                info!("Cache miss: {:?} ({} bytes)", segment_path, data_size);
            }

            // Track stats
            manager.stats.segments_served.fetch_add(1, Ordering::Relaxed);
            manager.stats.bytes_served.fetch_add(data_size as u64, Ordering::Relaxed);

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, cached_segment.content_type)
                .header(header::CACHE_CONTROL, "max-age=10")
                .body(Body::from(cached_segment.data))
                .unwrap()
        }
        Ok(None) => {
            error!("Segment not found: {:?} at path {:?}", segment_path, file_path);
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Segment not found"))
                .unwrap()
        }
        Err(e) => {
            error!("Error serving segment {:?}: {}", segment_path, e);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Internal server error"))
                .unwrap()
        }
    }
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

    // Parse the request into a structured type
    let request = match parse_hls_request(&path, query.hls_ctx.clone()) {
        Ok(req) => req,
        Err(err) => {
            error!("Failed to parse HLS request for path '{}': {}", path, err);
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(err))
                .unwrap();
        }
    };

    // Route based on request type
    match request {
        HlsRequestType::InitialRequest { stream_key } => {
            // Create session and redirect to appropriate playlist
            handle_initial_request_with_session(&stream_key, manager).await
        }

        HlsRequestType::Playlist {
            stream_key,
            playlist_path,
            session,
            needs_url_rewrite
        } => {
            // Ensure stream is ready
            if !ensure_stream_ready(&stream_key, &manager).await {
                if manager.failed_streams.contains_key(&stream_key) {
                    return Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::from("Stream not found"))
                        .unwrap();
                }
                return Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header("Retry-After", "5")
                    .body(Body::from("Stream not available"))
                    .unwrap();
            }

            // Track session if provided
            if let Some(sid) = &session {
                manager.session_manager.update_session(sid);
            }

            // Serve the playlist
            serve_playlist(&stream_key, playlist_path, needs_url_rewrite, manager).await
        }

        HlsRequestType::Segment { stream_key, segment_path } => {
            // Serve the segment
            serve_segment_file(&stream_key, segment_path, manager).await
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

#[cfg(test)]
#[path = "handlers_test.rs"]
mod handlers_test;
