mod cache;
mod config;
mod handlers;
mod manager;
mod metrics;
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
    compression::CompressionLayer,
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
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&config.logging.level))
        .init();

    let config = Arc::new(config);
    let manager = Arc::new(StreamManager::new(config.clone())?);
    manager.init().await?;

    let manager_cleanup = manager.clone();
    tokio::spawn(async move {
        manager_cleanup.cleanup_idle_streams().await;
    });

    // Start metrics collection task
    let manager_metrics = manager.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;

            // Get current stats
            let bytes = manager_metrics.stats.bytes_served.load(Ordering::Relaxed);
            let requests = manager_metrics.stats.requests.load(Ordering::Relaxed);
            let segments = manager_metrics
                .stats
                .segments_served
                .load(Ordering::Relaxed);
            let viewers = manager_metrics.session_manager.get_total_viewer_count();
            let active_streams = manager_metrics.active_streams.len();

            // Get cache stats
            let cache_stats = manager_metrics.cache.stats().await;

            // Get per-stream viewer counts
            let stream_viewers = manager_metrics.session_manager.get_all_stream_viewers();

            // Record metrics point
            manager_metrics
                .metrics_history
                .record_point(
                    bytes,
                    requests,
                    segments,
                    viewers,
                    active_streams,
                    cache_stats.hits,
                    cache_stats.misses,
                    cache_stats.memory_bytes,
                    cache_stats.total_entries,
                    stream_viewers,
                )
                .await;
        }
    });

    // Performance optimization layers
    let middleware = ServiceBuilder::new()
        // Timeout requests after 30 seconds
        .layer(TimeoutLayer::new(Duration::from_secs(30)))
        // Enable gzip compression for responses
        .layer(CompressionLayer::new())
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
