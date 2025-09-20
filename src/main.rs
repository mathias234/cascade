mod cache;
mod config;
mod elasticsearch;
mod handlers;
mod manager;
mod models;
mod sessions;

#[cfg(feature = "ssl")]
mod ssl;

use anyhow::Result;
use axum::{
    Router,
    http::{Method, header},
    routing::get,
};
use config::Config;
use manager::StreamManager;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::signal;
use tower::ServiceBuilder;
use tower_http::{
    cors::{Any, CorsLayer},
    timeout::TimeoutLayer,
};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // Load configuration from file, fall back to defaults if not found
    let config = match Config::from_file("config.toml") {
        Ok(cfg) => cfg,
        Err(_) => {
            eprintln!("config.toml not found, using default configuration");
            Config::default()
        }
    };

    // Initialize logging
    // Build filter with base level and specific overrides for noisy crates
    let filter = format!("{},ez_ffmpeg=warn", &config.logging.level);
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .init();

    let config = Arc::new(config);
    let manager = Arc::new(StreamManager::new(config.clone())?);
    manager.init().await?;

    let manager_cleanup = manager.clone();
    tokio::spawn(async move {
        manager_cleanup.cleanup_idle_streams().await;
    });

    // Start per-stream metrics collection task
    let manager_metrics = manager.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;

            let now = chrono::Utc::now();
            let mut metrics_docs = Vec::new();

            // Iterate through active streams and calculate per-stream metrics
            for entry in manager_metrics.active_streams.iter() {
                let stream_key = entry.key();
                let stream_info = entry.value();

                // Get metrics since last timestamp
                let mut last_timestamp = stream_info.last_metrics_timestamp.write().await;
                let time_elapsed = now
                    .signed_duration_since(*last_timestamp)
                    .num_milliseconds() as f64
                    / 1000.0;

                if time_elapsed <= 0.0 {
                    continue; // Skip if no time has elapsed
                }

                // Calculate per-second rates
                let bytes = stream_info.bytes_served.swap(0, Ordering::Relaxed);
                let requests = stream_info.requests_served.swap(0, Ordering::Relaxed);
                let segments = stream_info.segments_served.swap(0, Ordering::Relaxed);
                let cache_hits = stream_info.cache_hits.swap(0, Ordering::Relaxed);
                let cache_misses = stream_info.cache_misses.swap(0, Ordering::Relaxed);

                let bytes_per_second = bytes as f64 / time_elapsed;
                let requests_per_second = requests as f64 / time_elapsed;
                let segments_per_second = segments as f64 / time_elapsed;

                let cache_hit_rate = if cache_hits + cache_misses > 0 {
                    (cache_hits as f64 / (cache_hits + cache_misses) as f64) * 100.0
                } else {
                    0.0
                };

                let mbps = bytes_per_second * 8.0 / 1_000_000.0;

                // Get viewer count for this stream
                let viewers = manager_metrics
                    .session_manager
                    .get_stream_viewer_count(stream_key);

                // Update last timestamp
                *last_timestamp = now;

                // Create metric document for this stream
                let metric_doc = crate::elasticsearch::MetricsDocument {
                    timestamp: now,
                    server_name: String::new(), // Will be filled by ElasticsearchClient
                    stream_name: stream_key.clone(),
                    bytes_per_second,
                    requests_per_second,
                    segments_per_second,
                    viewers,
                    cache_hit_rate,
                    mbps,
                };

                metrics_docs.push(metric_doc);
            }

            // Send all metrics to Elasticsearch
            if !metrics_docs.is_empty() {
                manager_metrics
                    .elasticsearch
                    .index_metrics(metrics_docs)
                    .await;
            }
        }
    });

    // Performance optimization layers
    let middleware = ServiceBuilder::new()
        // Timeout requests after 30 seconds
        .layer(TimeoutLayer::new(Duration::from_secs(30)))
        // CORS configuration for HLS playback
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::HEAD, Method::OPTIONS])
                .allow_headers([header::CONTENT_TYPE, header::RANGE]),
        );

    let app = Router::new()
        // Combined route for all HLS files - we'll parse the path in the handler
        .route(
            "/live/{*path}",
            get({
                let manager = manager.clone();
                move |path, query, req| {
                    handlers::serve_hls_content(path, query, req, manager.clone())
                }
            }),
        )
        .route(
            "/health",
            get({
                let manager = manager.clone();
                move || handlers::health_check(manager.clone())
            }),
        )
        .route(
            "/status",
            get({
                let manager = manager.clone();
                move || handlers::status(manager.clone())
            }),
        )
        .route("/", get(handlers::dashboard))
        .route("/dashboard", get(handlers::dashboard))
        .layer(middleware);

    #[cfg(feature = "ssl")]
    {
        if config.ssl.enabled {
            info!("SSL is enabled, starting TLS server");
            return ssl::run_tls_server(app, manager, config.clone()).await;
        }
    }

    let addr = format!("0.0.0.0:{}", config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("HTTP server listening on {}", addr);
    info!(
        "Serving HLS streams at http://{}/live/{{stream_key}}.m3u8",
        addr
    );

    let server = axum::serve(listener, app);

    tokio::select! {
        result = server => {
            if let Err(e) = result {
                tracing::error!("Server error: {}", e);
            }
        }
        _ = signal::ctrl_c() => {
            info!("Received SIGINT, shutting down gracefully...");
            manager.graceful_shutdown().await;
        }
    }

    Ok(())
}
