use chrono::{DateTime, Utc};
use ez_ffmpeg::FfmpegScheduler;
use ez_ffmpeg::core::scheduler::ffmpeg_scheduler::Running;
use serde::Serialize;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Mutex, RwLock};

#[derive(Clone)]
pub struct StreamInfo {
    pub scheduler_handle: Arc<Mutex<Option<FfmpegScheduler<Running>>>>,
    pub started_at: DateTime<Utc>,
    pub last_accessed: Arc<RwLock<DateTime<Utc>>>,
    // Per-stream metrics
    pub bytes_served: Arc<AtomicU64>,
    pub requests_served: Arc<AtomicU64>,
    pub segments_served: Arc<AtomicU64>,
    pub playlists_served: Arc<AtomicU64>,
    pub cache_hits: Arc<AtomicU64>,
    pub cache_misses: Arc<AtomicU64>,
    pub last_metrics_timestamp: Arc<RwLock<DateTime<Utc>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamStatus {
    pub key: String,
    pub uptime: i64,
    pub last_accessed: i64,
    pub viewers: usize,
    pub bytes_per_second: f64,
    pub requests_per_second: f64,
    pub segments_per_second: f64,
    pub cache_hit_rate: f64,
    pub mbps: f64,
}

pub struct Stats {
    pub started: AtomicU64,
    pub stopped: AtomicU64,
    pub failed: AtomicU64,
    pub requests: AtomicU64,
    pub total_viewers: AtomicU64,
    pub bytes_served: AtomicU64,
    pub segments_served: AtomicU64,
    pub playlists_served: AtomicU64,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            started: AtomicU64::new(0),
            stopped: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            total_viewers: AtomicU64::new(0),
            bytes_served: AtomicU64::new(0),
            segments_served: AtomicU64::new(0),
            playlists_served: AtomicU64::new(0),
        }
    }

    pub fn to_snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            started: self.started.load(Ordering::Relaxed) as usize,
            stopped: self.stopped.load(Ordering::Relaxed) as usize,
            failed: self.failed.load(Ordering::Relaxed) as usize,
            requests: self.requests.load(Ordering::Relaxed) as usize,
            total_viewers: self.total_viewers.load(Ordering::Relaxed) as usize,
            bytes_served: self.bytes_served.load(Ordering::Relaxed),
            segments_served: self.segments_served.load(Ordering::Relaxed),
            playlists_served: self.playlists_served.load(Ordering::Relaxed),
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
    pub bytes_served: u64,
    pub segments_served: u64,
    pub playlists_served: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub active_streams: usize,
    pub pending_streams: usize,
    pub max_streams: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusResponse {
    pub active_streams: Vec<StreamStatus>,
    pub pending_streams: Vec<String>,
    pub uptime_seconds: i64,
}
