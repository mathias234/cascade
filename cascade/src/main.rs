mod cache;
mod handlers;
mod manager;
mod models;

use anyhow::Result;
use axum::{
    http::{header, Method},
    routing::get,
    Router,
};
use manager::StreamManager;
use std::{env, sync::Arc, time::Duration};
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
        )
        .init();

    let manager = Arc::new(StreamManager::new()?);
    manager.init().await?;

    let manager_cleanup = manager.clone();
    tokio::spawn(async move {
        manager_cleanup.cleanup_idle_streams().await;
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
                .allow_headers([header::CONTENT_TYPE, header::RANGE])
        );

    let app = Router::new()
        // HLS file serving route - handles both .m3u8 and .ts files
        .route("/live/{file_name}", get({
            let manager = manager.clone();
            move |path| handlers::serve_hls(path, manager.clone())
        }))
        .route("/health", get({
            let manager = manager.clone();
            move || handlers::health_check(manager.clone())
        }))
        .route("/status", get({
            let manager = manager.clone();
            move || handlers::status(manager.clone())
        }))
        .layer(middleware);

    let port = env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!("HTTP server listening on {}", addr);
    info!("Serving HLS streams at http://{}/live/{{stream_key}}.m3u8", addr);

    let server = axum::serve(listener, app);

    let manager_shutdown = manager.clone();
    tokio::select! {
        result = server => {
            if let Err(e) = result {
                tracing::error!("Server error: {}", e);
            }
        }
        _ = signal::ctrl_c() => {
            info!("Received SIGINT, shutting down gracefully...");
            manager_shutdown.graceful_shutdown().await;
        }
    }

    Ok(())
}
