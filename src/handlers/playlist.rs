use crate::handlers::common::{rewrite_playlist_urls, track_request_stats};
use crate::manager::StreamManager;
use axum::{
    body::Body,
    http::{StatusCode, header},
    response::Response,
};
use std::path::PathBuf;
use std::sync::{Arc, atomic::Ordering};
use tracing::{debug, error};

/// Unified function to serve playlist files from disk
pub async fn serve_playlist(
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
pub async fn handle_initial_request_with_session(
    stream_key: &str,
    manager: Arc<StreamManager>,
) -> Response<Body> {
    debug!(
        "Initial request for stream: {} (creating session)",
        stream_key
    );

    // Ensure stream is started
    if !crate::handlers::common::ensure_stream_ready(stream_key, &manager).await {
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