use anyhow::Result;
use axum::{
    http::{header, Uri},
    response::Redirect,
    Router,
};
use std::{net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;
use tracing::{error, info};

use super::{acme::create_acme_components, config::SslConfig};
use crate::manager::StreamManager;

pub async fn run_tls_server(
    app: Router,
    manager: Arc<StreamManager>,
    ssl_config: SslConfig,
) -> Result<()> {
    let (acceptor, _rustls_config) = create_acme_components(&ssl_config).await?;

    // Spawn HTTPS server
    let tls_app = app.clone();
    let tls_handle = tokio::spawn(run_https_server(
        tls_app,
        acceptor,
        ssl_config.ssl_port,
    ));

    // Spawn HTTP server (for redirect or to serve regular HTTP)
    let http_handle = if ssl_config.http_redirect {
        let redirect_handle = tokio::spawn(run_http_redirect_server(
            ssl_config.domains[0].clone(),
            ssl_config.ssl_port,
        ));
        Some(redirect_handle)
    } else {
        // Run regular HTTP server on port 80
        let http_app = app.clone();
        let port = std::env::var("PORT").unwrap_or_else(|_| "80".to_string()).parse::<u16>().unwrap_or(80);
        let http_handle = tokio::spawn(run_http_server(http_app, port));
        Some(http_handle)
    };

    tokio::select! {
        result = tls_handle => {
            if let Err(e) = result {
                error!("HTTPS server error: {}", e);
            }
        }
        result = async {
            if let Some(handle) = http_handle {
                handle.await
            } else {
                std::future::pending::<Result<Result<()>, tokio::task::JoinError>>().await
            }
        } => {
            if let Err(e) = result {
                error!("HTTP server error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down gracefully...");
            manager.graceful_shutdown().await;
        }
    }

    Ok(())
}

async fn run_https_server(app: Router, acceptor: rustls_acme::axum::AxumAcceptor, port: u16) -> Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("HTTPS server listening on {}", addr);

    // Use axum-server with the ACME acceptor
    axum_server::bind(addr)
        .acceptor(acceptor)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

async fn run_http_server(app: Router, port: u16) -> Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;
    info!("HTTP server listening on {}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

async fn run_http_redirect_server(primary_domain: String, https_port: u16) -> Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], 80));

    let redirect_app = Router::new()
        .fallback(move |uri: Uri, headers: axum::http::HeaderMap| async move {
            let host = headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(&primary_domain);

            let https_uri = if https_port == 443 {
                format!("https://{}{}", host, uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/"))
            } else {
                format!("https://{}:{}{}", host, https_port, uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/"))
            };

            Redirect::permanent(&https_uri)
        });

    let listener = TcpListener::bind(addr).await?;
    info!("HTTP redirect server listening on {} (redirecting to HTTPS)", addr);

    axum::serve(listener, redirect_app).await?;

    Ok(())
}