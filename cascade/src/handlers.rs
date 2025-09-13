use crate::ClientInfo;
use crate::manager::StreamManager;
use crate::models::{HealthResponse, StatusResponse, StreamStatus};
use axum::{
    body::Body,
    extract::{Path, Request},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
};
use chrono::Utc;
use std::sync::Arc;
use tracing::{debug, error, info};

pub async fn serve_hls(
    Path(file_name): Path<String>,
    req: Request,
    manager: Arc<StreamManager>,
) -> impl IntoResponse {
    debug!("HLS request for file: {}", file_name);
    
    let (stream_key, file_type) = if let Some(pos) = file_name.rfind('.') {
        let key = &file_name[..pos];
        let ext = &file_name[pos + 1..];
        
        // Extract base stream key (without segment number for .ts files)
        let base_key = if ext == "ts" {
            // Remove segment number suffix (e.g., "stream_001" -> "stream")
            if let Some(underscore_pos) = key.rfind('_') {
                &key[..underscore_pos]
            } else {
                key
            }
        } else {
            key
        };
        
        debug!("Parsed - full key: {}, base_key: {}, extension: {}", key, base_key, ext);
        (base_key.to_string(), ext.to_string())
    } else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("Invalid file format"))
            .unwrap();
    };

    // Update access time and stats
    {
        let mut stats = manager.stats.write().await;
        stats.requests += 1;
    }
    
    manager.update_stream_access(&stream_key).await;

    // For .m3u8 files, ensure stream is started and track viewer
    if file_type == "m3u8" {
        debug!("M3U8 request for stream: {}", stream_key);
        if !manager.wait_for_stream(stream_key.clone()).await {
            return Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header("Retry-After", "5")
                .body(Body::from("Stream not available"))
                .unwrap();
        }

        // Track viewer for m3u8 requests only
        if let Some(client_info) = req.extensions().get::<ClientInfo>() {
            manager.track_viewer(
                &stream_key,
                &client_info.ip,
                client_info.user_agent.as_deref(),
            );
        }
    }

    // For m3u8 playlists, always read fresh (no caching)
    let file_path = manager.hls_path.join(&file_name);
    
    if file_type == "m3u8" {
        // Always read m3u8 files fresh - they change constantly
        match tokio::fs::read(&file_path).await {
            Ok(data) => {
                debug!("Serving FRESH playlist: {} ({} bytes)", file_name, data.len());
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .body(Body::from(data))
                    .unwrap()
            }
            Err(e) => {
                error!("Failed to read m3u8 file {:?}: {}", file_path, e);
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("File not found"))
                    .unwrap()
            }
        }
    } else {
        // For TS segments, use caching
        match manager.cache.get_or_load(&file_name, &file_path).await {
            Ok(Some((cached_segment, was_cached))) => {
                let data_size = cached_segment.data.len();
                if was_cached {
                    debug!("Serving from CACHE: {} ({} bytes)", file_name, data_size);
                } else {
                    info!("Serving NEWLY LOADED: {} ({} bytes)", file_name, data_size);
                }
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, cached_segment.content_type)
                    .header(header::CACHE_CONTROL, "max-age=10")
                    .body(Body::from(cached_segment.data.to_bytes()))
                    .unwrap()
            }
            Ok(None) => {
                error!("File not found: {} at path {:?}", file_name, file_path);
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("File not found"))
                    .unwrap()
            }
            Err(e) => {
                error!("Error serving file {}: {}", file_name, e);
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("Internal server error"))
                    .unwrap()
            }
        }
    }
}

pub async fn health_check(manager: Arc<StreamManager>) -> impl IntoResponse {
    let active_count = manager.active_streams.read().await.len();
    let pending_count = manager.pending_streams.read().await.len();
    let mut stats = manager.stats.read().await.clone();

    // Update total viewer count
    stats.total_viewers = manager.get_total_viewer_count();

    let healthy = active_count < manager.max_concurrent_streams;

    let response = HealthResponse {
        status: if healthy { "healthy".to_string() } else { "unhealthy".to_string() },
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
    let active = manager.active_streams.read().await;
    let now = Utc::now();

    let mut active_streams = Vec::new();
    for (key, info) in active.iter() {
        let last_accessed = info.last_accessed.read().await;

        active_streams.push(StreamStatus {
            key: key.clone(),
            pid: info.pid,
            uptime: now.signed_duration_since(info.started_at).num_seconds(),
            last_accessed: now.signed_duration_since(*last_accessed).num_seconds(),
            viewers: manager.get_stream_viewer_count(key),
        });
    }

    let pending_streams: Vec<String> = manager.pending_streams.read().await
        .keys().cloned().collect();

    let failed_streams: Vec<String> = manager.failed_streams.read().await
        .keys().cloned().collect();

    let mut stats = manager.stats.read().await.clone();
    stats.total_viewers = manager.get_total_viewer_count();

    Json(StatusResponse {
        active_streams,
        pending_streams,
        failed_streams,
        stats,
    })
}