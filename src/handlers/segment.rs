use crate::manager::StreamManager;
use axum::{
    body::Body,
    http::{StatusCode, header},
    response::Response,
};
use std::path::PathBuf;
use std::sync::{Arc, atomic::Ordering};
use tracing::{debug, error, info};

/// Unified function to serve segment files
pub async fn serve_segment_file(
    stream_key: &str,
    segment_path: PathBuf,
    manager: Arc<StreamManager>,
) -> Response<Body> {
    debug!(
        "Segment request for stream: {}, path: {:?}",
        stream_key, segment_path
    );

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

            // Track per-stream metrics
            if let Some(stream_info) = manager.active_streams.get(stream_key) {
                stream_info.segments_served.fetch_add(1, Ordering::Relaxed);
                stream_info
                    .bytes_served
                    .fetch_add(data_size as u64, Ordering::Relaxed);
                stream_info.requests_served.fetch_add(1, Ordering::Relaxed);

                if was_cached {
                    debug!("Cache hit: {:?} ({} bytes)", segment_path, data_size);
                    stream_info.cache_hits.fetch_add(1, Ordering::Relaxed);
                } else {
                    info!("Cache miss: {:?} ({} bytes)", segment_path, data_size);
                    stream_info.cache_misses.fetch_add(1, Ordering::Relaxed);
                }
            }

            // Track global stats (for stream lifecycle tracking only)
            manager
                .stats
                .segments_served
                .fetch_add(1, Ordering::Relaxed);
            manager
                .stats
                .bytes_served
                .fetch_add(data_size as u64, Ordering::Relaxed);
            manager.stats.requests.fetch_add(1, Ordering::Relaxed);

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, cached_segment.content_type)
                .header(header::CACHE_CONTROL, "max-age=10")
                .body(Body::from(cached_segment.data))
                .unwrap()
        }
        Ok(None) => {
            error!(
                "Segment not found: {:?} at path {:?}",
                segment_path, file_path
            );
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
