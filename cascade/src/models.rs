use chrono::{DateTime, Utc};
use serde::Serialize;
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
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

pub struct Stats {
    pub started: AtomicU64,
    pub stopped: AtomicU64,
    pub failed: AtomicU64,
    pub requests: AtomicU64,
    pub total_viewers: AtomicU64,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            started: AtomicU64::new(0),
            stopped: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            total_viewers: AtomicU64::new(0),
        }
    }

    pub fn to_snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            started: self.started.load(Ordering::Relaxed) as usize,
            stopped: self.stopped.load(Ordering::Relaxed) as usize,
            failed: self.failed.load(Ordering::Relaxed) as usize,
            requests: self.requests.load(Ordering::Relaxed) as usize,
            total_viewers: self.total_viewers.load(Ordering::Relaxed) as usize,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsSnapshot {
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
    pub stats: StatsSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub active_streams: Vec<StreamStatus>,
    pub pending_streams: Vec<String>,
    pub failed_streams: Vec<String>,
    pub stats: StatsSnapshot,
}