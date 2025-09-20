use crate::manager::StreamManager;
use crate::models::{HealthResponse, StatusResponse, StreamStatus};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
};
use chrono::Utc;
use std::sync::{Arc, atomic::Ordering};

pub async fn health_check(manager: Arc<StreamManager>) -> impl IntoResponse {
    let active_count = manager.active_streams.len();
    let pending_count = manager.pending_streams.len();

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
        let last_metrics_timestamp = info.last_metrics_timestamp.read().await;

        // Calculate current rates
        let time_elapsed = now
            .signed_duration_since(*last_metrics_timestamp)
            .num_milliseconds() as f64
            / 1000.0;
        let time_elapsed = if time_elapsed > 0.0 {
            time_elapsed
        } else {
            1.0
        };

        let bytes = info.bytes_served.load(Ordering::Relaxed);
        let requests = info.requests_served.load(Ordering::Relaxed);
        let segments = info.segments_served.load(Ordering::Relaxed);
        let cache_hits = info.cache_hits.load(Ordering::Relaxed);
        let cache_misses = info.cache_misses.load(Ordering::Relaxed);

        let bytes_per_second = bytes as f64 / time_elapsed;
        let requests_per_second = requests as f64 / time_elapsed;
        let segments_per_second = segments as f64 / time_elapsed;

        let cache_hit_rate = if cache_hits + cache_misses > 0 {
            (cache_hits as f64 / (cache_hits + cache_misses) as f64) * 100.0
        } else {
            0.0
        };

        let bits_per_second = bytes_per_second * 8.0;

        active_streams.push(StreamStatus {
            key: key.clone(),
            uptime: now.signed_duration_since(info.started_at).num_seconds(),
            last_accessed: now.signed_duration_since(*last_accessed).num_seconds(),
            viewers: manager.session_manager.get_stream_viewer_count(key),
            bytes_per_second,
            requests_per_second,
            segments_per_second,
            cache_hit_rate,
            bits_per_second,
        });
    }

    let pending_streams: Vec<String> = manager
        .pending_streams
        .iter()
        .map(|entry| entry.key().clone())
        .collect();

    let uptime_seconds = now
        .signed_duration_since(manager.server_started_at)
        .num_seconds();

    Json(StatusResponse {
        active_streams,
        pending_streams,
        uptime_seconds,
    })
}

pub async fn dashboard() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "Dashboard has been removed").into_response()
}
