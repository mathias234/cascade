use anyhow::{Context, Result};
use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct SslConfig {
    pub enabled: bool,
    pub domains: Vec<String>,
    pub email: String,
    pub staging: bool,
    pub cert_cache_dir: PathBuf,
    pub ssl_port: u16,
    pub http_redirect: bool,
}

impl SslConfig {
    pub fn from_env() -> Result<Self> {
        let enabled = env::var("SSL_ENABLED")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .context("Invalid SSL_ENABLED value")?;

        if !enabled {
            return Ok(Self {
                enabled: false,
                domains: vec![],
                email: String::new(),
                staging: true,
                cert_cache_dir: PathBuf::from("/etc/cascade/certs"),
                ssl_port: 443,
                http_redirect: false,
            });
        }

        let domains_str =
            env::var("SSL_DOMAINS").context("SSL_DOMAINS must be set when SSL is enabled")?;
        let domains: Vec<String> = domains_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if domains.is_empty() {
            anyhow::bail!("SSL_DOMAINS must contain at least one domain");
        }

        let email = env::var("SSL_EMAIL").context("SSL_EMAIL must be set when SSL is enabled")?;

        if !email.contains('@') {
            anyhow::bail!("SSL_EMAIL must be a valid email address");
        }

        let staging = env::var("SSL_STAGING")
            .unwrap_or_else(|_| "true".to_string())
            .parse::<bool>()
            .context("Invalid SSL_STAGING value")?;

        let cert_cache_dir = env::var("SSL_CERT_CACHE_DIR")
            .unwrap_or_else(|_| "/etc/cascade/certs".to_string())
            .into();

        let ssl_port = env::var("SSL_PORT")
            .unwrap_or_else(|_| "443".to_string())
            .parse::<u16>()
            .context("Invalid SSL_PORT value")?;

        let http_redirect = env::var("HTTP_REDIRECT")
            .unwrap_or_else(|_| "true".to_string())
            .parse::<bool>()
            .context("Invalid HTTP_REDIRECT value")?;

        Ok(Self {
            enabled,
            domains,
            email,
            staging,
            cert_cache_dir,
            ssl_port,
            http_redirect,
        })
    }

    pub fn acme_directory_url(&self) -> &str {
        if self.staging {
            "https://acme-staging-v02.api.letsencrypt.org/directory"
        } else {
            "https://acme-v02.api.letsencrypt.org/directory"
        }
    }
}
