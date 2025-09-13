mod cache;
mod handlers;
mod manager;
mod models;
mod sessions;
mod viewers;

use anyhow::Result;
use axum::{
    extract::Request,
    http::{header, Method},
    middleware::{self, Next},
    response::Response,
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

#[derive(Clone)]
pub struct ClientInfo {
    pub ip: String,
    pub user_agent: Option<String>,
}

async fn extract_client_info(mut req: Request, next: Next) -> Response {
    // Extract IP from X-Forwarded-For header or connection info
    let ip = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next()) // Take first IP if multiple
        .map(|s| s.trim().to_string())
        .or_else(|| {
            req.extensions()
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|connect_info| connect_info.0.ip().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    // Extract User-Agent
    let user_agent = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let client_info = ClientInfo { ip, user_agent };
    req.extensions_mut().insert(client_info);

    next.run(req).await
}

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
        // Combined route for all HLS files - we'll parse the path in the handler
        .route("/live/{*path}", get({
            let manager = manager.clone();
            move |path, query, req| handlers::serve_hls_content(path, query, req, manager.clone())
        }))
        .route("/health", get({
            let manager = manager.clone();
            move || handlers::health_check(manager.clone())
        }))
        .route("/status", get({
            let manager = manager.clone();
            move || handlers::status(manager.clone())
        }))
        .layer(middleware::from_fn(extract_client_info))
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
