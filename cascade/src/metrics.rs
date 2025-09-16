use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

const MAX_HISTORY_POINTS: usize = 60; // Keep last 60 data points (2 minutes at 2s intervals)

#[derive(Debug, Clone, Serialize)]
pub struct MetricPoint {
    pub timestamp: DateTime<Utc>,
    pub bytes_per_second: f64,
    pub requests_per_second: f64,
    pub segments_per_second: f64,
    pub viewers: usize,
    pub active_streams: usize,
    pub cache_hit_rate: f64,
    pub cache_memory_mb: f64,
    pub cache_entries: usize,
    pub stream_viewers: HashMap<String, usize>, // Per-stream viewer counts
}

struct MetricsState {
    history: VecDeque<MetricPoint>,
    last_bytes: u64,
    last_requests: u64,
    last_segments: u64,
    last_timestamp: DateTime<Utc>,
}

pub struct MetricsHistory {
    state: Arc<RwLock<MetricsState>>,
}

impl MetricsHistory {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(MetricsState {
                history: VecDeque::with_capacity(MAX_HISTORY_POINTS),
                last_bytes: 0,
                last_requests: 0,
                last_segments: 0,
                last_timestamp: Utc::now(),
            })),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record_point(
        &self,
        current_bytes: u64,
        current_requests: u64,
        current_segments: u64,
        viewers: usize,
        active_streams: usize,
        cache_hits: u64,
        cache_misses: u64,
        cache_memory_bytes: u64,
        cache_entries: usize,
        stream_viewers: HashMap<String, usize>,
    ) {
        let now = Utc::now();
        let mut state = self.state.write().await;
        
        let time_delta = now.signed_duration_since(state.last_timestamp).num_milliseconds() as f64 / 1000.0;
        
        if time_delta > 0.0 {
            let bytes_delta = current_bytes.saturating_sub(state.last_bytes);
            let requests_delta = current_requests.saturating_sub(state.last_requests);
            let segments_delta = current_segments.saturating_sub(state.last_segments);
            
            let bytes_per_second = bytes_delta as f64 / time_delta;
            let requests_per_second = requests_delta as f64 / time_delta;
            let segments_per_second = segments_delta as f64 / time_delta;
            
            let cache_total = cache_hits + cache_misses;
            let cache_hit_rate = if cache_total > 0 {
                (cache_hits as f64 / cache_total as f64) * 100.0
            } else {
                0.0
            };
            
            let cache_memory_mb = cache_memory_bytes as f64 / 1_048_576.0; // Convert to MB
            
            let point = MetricPoint {
                timestamp: now,
                bytes_per_second,
                requests_per_second,
                segments_per_second,
                viewers,
                active_streams,
                cache_hit_rate,
                cache_memory_mb,
                cache_entries,
                stream_viewers,
            };
            
            if state.history.len() >= MAX_HISTORY_POINTS {
                state.history.pop_front();
            }
            state.history.push_back(point);
            
            // Update last values
            state.last_bytes = current_bytes;
            state.last_requests = current_requests;
            state.last_segments = current_segments;
            state.last_timestamp = now;
        }
    }

    pub async fn get_history(&self) -> Vec<MetricPoint> {
        let state = self.state.read().await;
        state.history.iter().cloned().collect()
    }

    pub async fn get_current_throughput(&self) -> ThroughputMetrics {
        let state = self.state.read().await;
        
        if let Some(latest) = state.history.back() {
            ThroughputMetrics {
                bytes_per_second: latest.bytes_per_second,
                requests_per_second: latest.requests_per_second,
                segments_per_second: latest.segments_per_second,
                mbps: latest.bytes_per_second * 8.0 / 1_000_000.0, // Convert to Mbps
            }
        } else {
            ThroughputMetrics::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ThroughputMetrics {
    pub bytes_per_second: f64,
    pub requests_per_second: f64,
    pub segments_per_second: f64,
    pub mbps: f64,
}
