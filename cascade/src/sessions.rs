use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use std::{env, time::Duration};
use tracing::{debug, info};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct ViewerSession {
    pub session_id: String,
    pub stream_key: String,
    pub created_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub request_count: u64,
}

pub struct SessionManager {
    // Map: session_id -> ViewerSession
    sessions: DashMap<String, ViewerSession>,
    // Map: stream_key -> Set of session_ids
    stream_sessions: DashMap<String, DashMap<String, ()>>,
    session_timeout: Duration,
    tracking_enabled: bool,
}

impl SessionManager {
    pub fn new() -> Self {
        let session_timeout = Duration::from_secs(
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
            "Session tracking: {} (timeout: {:?})",
            if tracking_enabled { "enabled" } else { "disabled" },
            session_timeout
        );

        Self {
            sessions: DashMap::new(),
            stream_sessions: DashMap::new(),
            session_timeout,
            tracking_enabled,
        }
    }

    /// Generate a new UUID session ID
    pub fn generate_session_id() -> String {
        Uuid::new_v4().to_string()
    }

    /// Create a new viewer session
    pub fn create_session(&self, stream_key: &str) -> String {
        if !self.tracking_enabled {
            return Self::generate_session_id();
        }

        let session_id = Self::generate_session_id();
        let now = Utc::now();

        let session = ViewerSession {
            session_id: session_id.clone(),
            stream_key: stream_key.to_string(),
            created_at: now,
            last_seen: now,
            request_count: 1,
        };

        // Add to main sessions map
        self.sessions.insert(session_id.clone(), session.clone());

        // Add to stream-specific tracking
        let stream_sessions = self.stream_sessions
            .entry(stream_key.to_string())
            .or_insert_with(DashMap::new);
        stream_sessions.insert(session_id.clone(), ());

        debug!("Created new session {} for stream {}", session_id, stream_key);
        session_id
    }

    /// Update an existing session
    pub fn update_session(&self, session_id: &str) -> bool {
        if !self.tracking_enabled {
            return true;
        }

        if let Some(mut session) = self.sessions.get_mut(session_id) {
            session.last_seen = Utc::now();
            session.request_count += 1;
            true
        } else {
            false
        }
    }

    /// Get viewer count for a specific stream
    pub fn get_stream_viewer_count(&self, stream_key: &str) -> usize {
        if !self.tracking_enabled {
            return 0;
        }

        if let Some(stream_sessions) = self.stream_sessions.get(stream_key) {
            let now = Utc::now();
            stream_sessions
                .iter()
                .filter(|entry| {
                    if let Some(session) = self.sessions.get(entry.key()) {
                        let idle_duration = now.signed_duration_since(session.last_seen);
                        idle_duration.num_seconds() <= self.session_timeout.as_secs() as i64
                    } else {
                        false
                    }
                })
                .count()
        } else {
            0
        }
    }

    /// Get total viewer count across all streams
    pub fn get_total_viewer_count(&self) -> usize {
        if !self.tracking_enabled {
            return 0;
        }

        let now = Utc::now();
        self.sessions
            .iter()
            .filter(|entry| {
                let idle_duration = now.signed_duration_since(entry.value().last_seen);
                idle_duration.num_seconds() <= self.session_timeout.as_secs() as i64
            })
            .count()
    }
    
    /// Get viewer counts for all streams
    pub fn get_all_stream_viewers(&self) -> std::collections::HashMap<String, usize> {
        use std::collections::HashMap;
        
        if !self.tracking_enabled {
            return HashMap::new();
        }
        
        let now = Utc::now();
        let mut stream_counts = HashMap::new();
        
        for entry in self.sessions.iter() {
            let session = entry.value();
            let idle_duration = now.signed_duration_since(session.last_seen);
            
            // Only count active sessions
            if idle_duration.num_seconds() <= self.session_timeout.as_secs() as i64 {
                *stream_counts.entry(session.stream_key.clone()).or_insert(0) += 1;
            }
        }
        
        stream_counts
    }

    /// Clean up expired sessions
    pub fn cleanup_expired_sessions(&self) {
        if !self.tracking_enabled {
            return;
        }

        let now = Utc::now();
        let mut removed_count = 0;

        // Collect expired session IDs
        let expired_sessions: Vec<(String, String)> = self.sessions
            .iter()
            .filter_map(|entry| {
                let session = entry.value();
                let idle_duration = now.signed_duration_since(session.last_seen);
                if idle_duration.num_seconds() > self.session_timeout.as_secs() as i64 {
                    Some((entry.key().clone(), session.stream_key.clone()))
                } else {
                    None
                }
            })
            .collect();

        // Remove expired sessions
        for (session_id, stream_key) in expired_sessions {
            // Remove from main map
            self.sessions.remove(&session_id);

            // Remove from stream-specific map
            if let Some(stream_sessions) = self.stream_sessions.get(&stream_key) {
                stream_sessions.remove(&session_id);
            }

            removed_count += 1;
        }

        // Clean up empty stream entries
        self.stream_sessions.retain(|_, sessions| !sessions.is_empty());

        if removed_count > 0 {
            debug!("Cleaned up {} expired sessions", removed_count);
        }
    }

    /// Clear all sessions for a specific stream
    pub fn clear_stream_sessions(&self, stream_key: &str) {
        if !self.tracking_enabled {
            return;
        }

        if let Some((_, stream_sessions)) = self.stream_sessions.remove(stream_key) {
            // Remove all sessions for this stream from the main map
            for session_id in stream_sessions.iter() {
                self.sessions.remove(session_id.key());
            }

            let count = stream_sessions.len();
            if count > 0 {
                info!("Cleared {} sessions from stream {}", count, stream_key);
            }
        }
    }

    /// Check if a session exists and is valid
    pub fn validate_session(&self, session_id: &str) -> Option<String> {
        if !self.tracking_enabled {
            return None;
        }

        if let Some(session) = self.sessions.get(session_id) {
            let now = Utc::now();
            let idle_duration = now.signed_duration_since(session.last_seen);
            if idle_duration.num_seconds() <= self.session_timeout.as_secs() as i64 {
                Some(session.stream_key.clone())
            } else {
                None
            }
        } else {
            None
        }
    }
}
