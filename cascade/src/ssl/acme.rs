use anyhow::{Context, Result};
use futures::StreamExt;
use rustls::ServerConfig;
use rustls_acme::{caches::DirCache, AcmeConfig};
use std::path::Path;
use std::sync::Arc;
use tokio::fs;
use tracing::{error, info, warn};

use super::config::SslConfig;

pub async fn create_acme_components(config: &SslConfig) -> Result<(rustls_acme::axum::AxumAcceptor, Arc<ServerConfig>)> {
    ensure_cert_cache_dir(&config.cert_cache_dir).await?;

    let mut acme_config = AcmeConfig::new(&config.domains)
        .contact(vec![format!("mailto:{}", config.email)])
        .cache(DirCache::new(config.cert_cache_dir.clone()))
        .directory(config.acme_directory_url());

    if config.staging {
        warn!(
            "Using Let's Encrypt STAGING environment. Certificates will NOT be trusted by browsers!"
        );
        warn!("Set SSL_STAGING=false for production certificates.");
    } else {
        info!("Using Let's Encrypt PRODUCTION environment");
    }

    info!(
        "Initializing ACME for domains: {}",
        config.domains.join(", ")
    );

    let mut state = acme_config.state();
    let acceptor = state.acceptor();
    let rustls_config = state.default_rustls_config();

    tokio::spawn(async move {
        loop {
            match state.next().await {
                Some(Ok(event)) => {
                    info!("ACME event: {:?}", event);
                }
                Some(Err(err)) => {
                    error!("ACME error: {:?}", err);
                }
                None => {
                    error!("ACME state stream ended unexpectedly");
                    break;
                }
            }
        }
    });

    let axum_acceptor = rustls_acme::axum::AxumAcceptor::new(acceptor, rustls_config.clone());

    info!("ACME initialization complete");
    Ok((axum_acceptor, rustls_config))
}

async fn ensure_cert_cache_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)
            .await
            .with_context(|| format!("Failed to create certificate cache directory: {:?}", path))?;
        info!("Created certificate cache directory: {:?}", path);
    }

    let metadata = fs::metadata(path)
        .await
        .with_context(|| format!("Failed to read metadata for: {:?}", path))?;

    if !metadata.is_dir() {
        anyhow::bail!("Certificate cache path exists but is not a directory: {:?}", path);
    }

    Ok(())
}