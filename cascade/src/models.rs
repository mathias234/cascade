use chrono::{DateTime, Utc};
use serde::Serialize;
use std::sync::Arc;
use tokio::{
    process::Child,
    sync::{Mutex, RwLock},
};

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub pid: u32,
    pub process: Arc<Mutex<Child>>,
    pub started_at: DateTime<Utc>,
    pub last_accessed: Arc<RwLock<DateTime<Utc>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamStatus {
    pub key: String,
    pub pid: u32,
    pub uptime: i64,
    pub last_accessed: i64,
    pub viewers: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub started: usize,
    pub stopped: usize,
    pub failed: usize,
    pub requests: usize,
    pub total_viewers: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub active_streams: usize,
    pub pending_streams: usize,
    pub max_streams: usize,
    pub stats: Stats,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub active_streams: Vec<StreamStatus>,
    pub pending_streams: Vec<String>,
    pub failed_streams: Vec<String>,
    pub stats: Stats,
}