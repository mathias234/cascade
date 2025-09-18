use crate::manager::StreamManager;
use crate::models::{HealthResponse, StatusResponse, StreamStatus};
use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Json},
};
use chrono::Utc;
use std::sync::{Arc, atomic::Ordering};
use tracing::{debug, error};

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