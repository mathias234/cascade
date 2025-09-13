use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{env, sync::Arc, time::Duration};
use tracing::{debug, info};

#[derive(Debug, Clone, Serialize)]
pub struct ViewerInfo {
    pub viewer_hash: String,  // SHA-256 hash of IP + User-Agent + Accept-Language
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub request_count: u64,
}

pub struct ViewerTracker {
    // Outer map: stream_key -> Inner map: viewer_hash -> ViewerInfo
    viewers: Arc<DashMap<String, Arc<DashMap<String, ViewerInfo>>>>,
    viewer_timeout: Duration,
    tracking_enabled: bool,
}

impl ViewerTracker {
    pub fn new() -> Self {
        let viewer_timeout = Duration::from_secs(
            env::var("VIEWER_TIMEOUT_SECONDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
        );

        let tracking_enabled = env::var("VIEWER_TRACKING_ENABLED")
            .ok()
            .and_then(|s| s.parse::<bool>().ok())
            .unwrap_or(true);

        info!(
            "Viewer tracking: {} (timeout: {:?})",
            if tracking_enabled { "enabled" } else { "disabled" },
            viewer_timeout
        );

        Self {
            viewers: Arc::new(DashMap::new()),
            viewer_timeout,
            tracking_enabled,
        }
    }

    /// Generate a unique viewer hash from IP and User-Agent
    pub fn generate_viewer_hash(ip: &str, user_agent: Option<&str>) -> String {
        let mut hasher = Sha256::new();
        hasher.update(ip.as_bytes());
        if let Some(ua) = user_agent {
            hasher.update(ua.as_bytes());
        }
        format!("{:x}", hasher.finalize())
    }

    /// Track a viewer for a specific stream
    pub fn track_viewer(
        &self,
        stream_key: &str,
        ip: &str,
        user_agent: Option<&str>,
    ) {
        if !self.tracking_enabled {
            return;
        }

        let viewer_hash = Self::generate_viewer_hash(ip, user_agent);
        let now = Utc::now();

        // Get or create the stream's viewer map
        let stream_viewers = self.viewers
            .entry(stream_key.to_string())
            .or_insert_with(|| Arc::new(DashMap::new()));

        // Update or insert viewer info
        stream_viewers
            .entry(viewer_hash.clone())
            .and_modify(|viewer| {
                viewer.last_seen = now;
                viewer.request_count += 1;
            })
            .or_insert_with(|| {
                debug!("New viewer {} for stream {}", &viewer_hash[..8], stream_key);
                ViewerInfo {
                    viewer_hash: viewer_hash.clone(),
                    first_seen: now,
                    last_seen: now,
                    request_count: 1,
                }
            });
    }

    /// Get the count of active viewers for a stream
    pub fn get_viewer_count(&self, stream_key: &str) -> usize {
        if !self.tracking_enabled {
            return 0;
        }

        self.viewers
            .get(stream_key)
            .map(|stream_viewers| {
                let now = Utc::now();
                stream_viewers
                    .iter()
                    .filter(|entry| {
                        let idle_duration = now.signed_duration_since(entry.last_seen);
                        idle_duration.num_seconds() <= self.viewer_timeout.as_secs() as i64
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// Get total viewer count across all streams
    pub fn get_total_viewer_count(&self) -> usize {
        if !self.tracking_enabled {
            return 0;
        }

        let now = Utc::now();
        self.viewers
            .iter()
            .map(|stream_entry| {
                stream_entry
                    .value()
                    .iter()
                    .filter(|viewer_entry| {
                        let idle_duration = now.signed_duration_since(viewer_entry.last_seen);
                        idle_duration.num_seconds() <= self.viewer_timeout.as_secs() as i64
                    })
                    .count()
            })
            .sum()
    }

    /// Clean up inactive viewers across all streams
    pub fn cleanup_inactive_viewers(&self) {
        if !self.tracking_enabled {
            return;
        }

        let now = Utc::now();
        let mut total_removed = 0;

        for stream_entry in self.viewers.iter() {
            let _stream_key = stream_entry.key();
            let stream_viewers = stream_entry.value();

            // Remove inactive viewers
            stream_viewers.retain(|_viewer_hash, viewer_info| {
                let idle_duration = now.signed_duration_since(viewer_info.last_seen);
                let should_keep = idle_duration.num_seconds() <= self.viewer_timeout.as_secs() as i64;
                if !should_keep {
                    total_removed += 1;
                }
                should_keep
            });

            // If stream has no more viewers, we could remove it from the outer map
            // but keeping it around is fine as it will be cleaned up when stream stops
        }

        if total_removed > 0 {
            debug!("Cleaned up {} inactive viewers", total_removed);
        }
    }

    /// Clear all viewers for a specific stream
    pub fn clear_stream_viewers(&self, stream_key: &str) {
        if !self.tracking_enabled {
            return;
        }

        if let Some((_, stream_viewers)) = self.viewers.remove(stream_key) {
            let count = stream_viewers.len();
            if count > 0 {
                info!("Cleared {} viewers from stream {}", count, stream_key);
            }
        }
    }

    /// Get detailed viewer information for a stream
    pub fn get_stream_viewers(&self, stream_key: &str) -> Vec<ViewerInfo> {
        if !self.tracking_enabled {
            return Vec::new();
        }

        self.viewers
            .get(stream_key)
            .map(|stream_viewers| {
                let now = Utc::now();
                stream_viewers
                    .iter()
                    .filter(|entry| {
                        let idle_duration = now.signed_duration_since(entry.last_seen);
                        idle_duration.num_seconds() <= self.viewer_timeout.as_secs() as i64
                    })
                    .map(|entry| entry.value().clone())
                    .collect()
            })
            .unwrap_or_else(Vec::new)
    }
}