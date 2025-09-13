use crate::cache::SegmentCache;
use crate::models::{Stats, StreamInfo};
use crate::sessions::SessionManager;
use crate::viewers::ViewerTracker;
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::{
    collections::HashMap,
    env,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    fs,
    process::Command,
    sync::{Mutex, RwLock},
    time::{self, Instant},
};
use tracing::{debug, error, info, warn};

pub struct StreamManager {
    pub srs_host: String,
    pub srs_port: String,
    pub hls_path: PathBuf,
    pub stream_timeout: Duration,
    pub max_concurrent_streams: usize,
    pub stream_start_timeout: Duration,
    pub active_streams: Arc<RwLock<HashMap<String, StreamInfo>>>,
    pub pending_streams: Arc<RwLock<HashMap<String, DateTime<Utc>>>>,
    pub failed_streams: Arc<RwLock<HashMap<String, DateTime<Utc>>>>,
    pub stats: Arc<RwLock<Stats>>,
    pub cache: Arc<SegmentCache>,
    pub viewer_tracker: Arc<ViewerTracker>,
    pub session_manager: Arc<SessionManager>,
}

impl StreamManager {
    pub fn new() -> Result<Self> {
        let srs_host = env::var("SOURCE_HOST").unwrap_or_else(|_| "rtmp.example.com".to_string());
        let srs_port = env::var("SOURCE_PORT").unwrap_or_else(|_| "1935".to_string());
        let hls_path = PathBuf::from(env::var("HLS_PATH").unwrap_or_else(|_| "./hls".to_string()));
        
        let stream_timeout = Duration::from_secs(
            env::var("STREAM_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30)
        );
        
        let max_concurrent_streams = env::var("MAX_CONCURRENT_STREAMS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(50);
            
        let stream_start_timeout = Duration::from_secs(
            env::var("STREAM_START_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(15)
        );

        info!("Stream Manager starting...");
        info!("Source: rtmp://{}:{}/live/", srs_host, srs_port);
        info!("Output: {:?}", hls_path);
        info!("Max concurrent streams: {}", max_concurrent_streams);
        info!("Stream timeout: {:?}", stream_timeout);

        // Cache configuration
        let cache_entries = env::var("CACHE_MAX_ENTRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        let max_segment_size = env::var("CACHE_MAX_SEGMENT_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_485_760); // 10MB default

        Ok(StreamManager {
            srs_host,
            srs_port,
            hls_path,
            stream_timeout,
            max_concurrent_streams,
            stream_start_timeout,
            active_streams: Arc::new(RwLock::new(HashMap::new())),
            pending_streams: Arc::new(RwLock::new(HashMap::new())),
            failed_streams: Arc::new(RwLock::new(HashMap::new())),
            stats: Arc::new(RwLock::new(Stats {
                started: 0,
                stopped: 0,
                failed: 0,
                requests: 0,
                total_viewers: 0,
            })),
            cache: Arc::new(SegmentCache::new(cache_entries, max_segment_size)),
            viewer_tracker: Arc::new(ViewerTracker::new()),
            session_manager: Arc::new(SessionManager::new()),
        })
    }

    pub async fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.hls_path).await?;
        self.cleanup_all_streams().await?;
        Ok(())
    }

    async fn cleanup_all_streams(&self) -> Result<()> {
        info!("Cleaning up HLS directory...");
        let mut count = 0;
        
        let mut entries = fs::read_dir(&self.hls_path).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "m3u8" || ext == "ts" {
                    fs::remove_file(path).await.ok();
                    count += 1;
                }
            }
        }
        
        if count > 0 {
            info!("Cleaned up {} files from previous runs", count);
        }
        Ok(())
    }

    pub async fn start_stream(&self, stream_key: String) -> Result<bool> {
        {
            let active = self.active_streams.read().await;
            if active.contains_key(&stream_key) {
                debug!("Stream {} already active", stream_key);
                return Ok(true);
            }
        }

        {
            let pending = self.pending_streams.read().await;
            if pending.contains_key(&stream_key) {
                debug!("Stream {} already pending", stream_key);
                return Ok(true);
            }
        }

        {
            let active = self.active_streams.read().await;
            if active.len() >= self.max_concurrent_streams {
                warn!("Max concurrent streams ({}) reached, cannot start {}", 
                    self.max_concurrent_streams, stream_key);
                return Ok(false);
            }
        }

        {
            let failed = self.failed_streams.read().await;
            if let Some(fail_time) = failed.get(&stream_key) {
                if Utc::now().signed_duration_since(*fail_time).num_seconds() < 30 {
                    warn!("Stream {} recently failed, waiting before retry", stream_key);
                    return Ok(false);
                }
            }
        }

        {
            let mut pending = self.pending_streams.write().await;
            pending.insert(stream_key.clone(), Utc::now());
        }

        debug!("Spawning FFmpeg for stream: {}", stream_key);

        let rtmp_url = format!("rtmp://{}:{}/live/{}", self.srs_host, self.srs_port, stream_key);

        // Clean up any old files first
        self.cleanup_stream_files(&stream_key).await?;

        // Create stream-specific directory
        let stream_dir = self.hls_path.join(&stream_key);
        tokio::fs::create_dir_all(&stream_dir).await.ok();

        // Output paths in subdirectory
        let m3u8_path = stream_dir.join("index.m3u8");
        let segment_path = stream_dir.join(format!("{}_%03d.ts", stream_key));

        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-nostdin")
            .arg("-loglevel").arg("warning")
            .arg("-i").arg(&rtmp_url)
            .arg("-c").arg("copy")
            .arg("-f").arg("hls")
            .arg("-hls_time").arg("5")
            .arg("-hls_list_size").arg("20")
            .arg("-hls_flags").arg("delete_segments+append_list")
            .arg("-hls_segment_filename").arg(segment_path.to_str().unwrap())
            .arg(m3u8_path.to_str().unwrap())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id().unwrap_or(0);
                info!("FFmpeg process spawned for {}, PID: {}", stream_key, pid);

                let stream_info = StreamInfo {
                    pid,
                    process: Arc::new(Mutex::new(child)),
                    started_at: Utc::now(),
                    last_accessed: Arc::new(RwLock::new(Utc::now())),
                };

                {
                    let mut active = self.active_streams.write().await;
                    active.insert(stream_key.clone(), stream_info);
                }

                {
                    let mut pending = self.pending_streams.write().await;
                    pending.remove(&stream_key);
                }

                {
                    let mut stats = self.stats.write().await;
                    stats.started += 1;
                }

                Ok(true)
            }
            Err(e) => {
                error!("Failed to start stream {}: {}", stream_key, e);
                
                {
                    let mut pending = self.pending_streams.write().await;
                    pending.remove(&stream_key);
                }
                
                {
                    let mut failed = self.failed_streams.write().await;
                    failed.insert(stream_key, Utc::now());
                }
                
                {
                    let mut stats = self.stats.write().await;
                    stats.failed += 1;
                }
                
                Ok(false)
            }
        }
    }

    pub async fn stop_stream(&self, stream_key: &str) -> Result<()> {
        info!("Stopping stream: {}", stream_key);

        let stream_info = {
            let mut active = self.active_streams.write().await;
            active.remove(stream_key)
        };

        if let Some(info) = stream_info {
            let mut process = info.process.lock().await;
            
            if let Err(e) = process.kill().await {
                error!("Error killing stream {}: {}", stream_key, e);
            }
            
            let _ = process.wait().await;
            
            {
                let mut stats = self.stats.write().await;
                stats.stopped += 1;
            }
        }

        // Invalidate cache entries for this stream
        self.cache.invalidate_stream(stream_key).await;

        // Clear viewers and sessions for this stream
        self.viewer_tracker.clear_stream_viewers(stream_key);
        self.session_manager.clear_stream_sessions(stream_key);

        self.cleanup_stream_files(stream_key).await?;
        Ok(())
    }

    async fn cleanup_stream_files(&self, stream_key: &str) -> Result<()> {
        // Remove the stream-specific directory
        let stream_dir = self.hls_path.join(stream_key);
        if stream_dir.exists() {
            fs::remove_dir_all(stream_dir).await.ok();
        }

        Ok(())
    }

    async fn stream_ready(&self, stream_key: &str) -> bool {
        let m3u8_path = self.hls_path.join(stream_key).join("index.m3u8");

        if let Ok(content) = fs::read_to_string(&m3u8_path).await {
            let segments = content.matches(".ts").count();
            return segments >= 1;
        }

        false
    }

    pub async fn wait_for_stream(&self, stream_key: String) -> bool {
        // Check if stream is already active and ready
        {
            let active = self.active_streams.read().await;
            if active.contains_key(&stream_key) {
                // Stream is already active, check if it's ready
                if self.stream_ready(&stream_key).await {
                    if let Some(stream) = active.get(&stream_key) {
                        let mut last_accessed = stream.last_accessed.write().await;
                        *last_accessed = Utc::now();
                    }
                    debug!("Stream {} already active and ready", stream_key);
                    return true;
                }
                // Stream is active but not ready yet, fall through to wait
                debug!("Stream {} is active but not ready yet", stream_key);
            }
        }

        // Try to start the stream if it's not already starting
        let need_to_start = {
            let pending = self.pending_streams.read().await;
            let active = self.active_streams.read().await;
            !pending.contains_key(&stream_key) && !active.contains_key(&stream_key)
        };

        if need_to_start {
            info!("Starting stream: {}", stream_key);
            if !self.start_stream(stream_key.clone()).await.unwrap_or(false) {
                return false;
            }
        } else {
            debug!("Stream {} is already starting or active, waiting for it to be ready", stream_key);
        }

        // Wait for stream to become ready (regardless of who started it)
        let start = Instant::now();
        let mut check_interval = Duration::from_millis(200);

        while start.elapsed() < self.stream_start_timeout {
            if self.stream_ready(&stream_key).await {
                let active = self.active_streams.read().await;
                if let Some(stream) = active.get(&stream_key) {
                    let mut last_accessed = stream.last_accessed.write().await;
                    *last_accessed = Utc::now();
                }
                info!("Stream {} became ready after {:?}", stream_key, start.elapsed());
                return true;
            }

            {
                let failed = self.failed_streams.read().await;
                if failed.contains_key(&stream_key) {
                    error!("Stream {} failed to start", stream_key);
                    return false;
                }
            }

            time::sleep(check_interval).await;
            check_interval = std::cmp::min(
                Duration::from_millis((check_interval.as_millis() as f64 * 1.2) as u64),
                Duration::from_secs(1)
            );
        }

        warn!("Timeout waiting for stream {} after {:?}", stream_key, self.stream_start_timeout);
        false
    }

    pub async fn cleanup_idle_streams(&self) {
        info!("Cleanup thread started");
        let mut interval = time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;

            let mut streams_to_stop = Vec::new();

            {
                let active = self.active_streams.read().await;
                info!("Checking {} active streams for health", active.len());

                for (stream_key, stream_info) in active.iter() {
                    let last_accessed = stream_info.last_accessed.read().await;
                    let idle_duration = Utc::now().signed_duration_since(*last_accessed);

                    if idle_duration.num_seconds() > self.stream_timeout.as_secs() as i64 {
                        streams_to_stop.push(stream_key.clone());
                    }
                }
            }

            for stream_key in streams_to_stop {
                info!("Stream {} idle for more than {:?}, stopping", stream_key, self.stream_timeout);
                if let Err(e) = self.stop_stream(&stream_key).await {
                    error!("Error stopping idle stream {}: {}", stream_key, e);
                }
            }

            {
                let mut failed = self.failed_streams.write().await;
                let now = Utc::now();
                failed.retain(|_, time| {
                    now.signed_duration_since(*time).num_seconds() < 300
                });
            }

            // Clean up inactive viewers and sessions
            self.viewer_tracker.cleanup_inactive_viewers();
            self.session_manager.cleanup_expired_sessions();
        }
    }

    pub async fn update_stream_access(&self, stream_key: &str) {
        let active = self.active_streams.read().await;
        if let Some(stream) = active.get(stream_key) {
            let mut last_accessed = stream.last_accessed.write().await;
            *last_accessed = Utc::now();
            debug!("Updated access time for stream {}", stream_key);
        }
    }

    pub fn track_viewer(&self, stream_key: &str, ip: &str, user_agent: Option<&str>) {
        self.viewer_tracker.track_viewer(stream_key, ip, user_agent);
    }

    pub fn get_stream_viewer_count(&self, stream_key: &str) -> usize {
        self.viewer_tracker.get_viewer_count(stream_key)
    }

    pub fn get_total_viewer_count(&self) -> usize {
        self.viewer_tracker.get_total_viewer_count()
    }

    pub async fn graceful_shutdown(&self) {
        info!("Starting graceful shutdown...");
        
        {
            let mut pending = self.pending_streams.write().await;
            pending.clear();
        }

        let active_keys: Vec<String> = {
            let active = self.active_streams.read().await;
            active.keys().cloned().collect()
        };

        for stream_key in active_keys {
            if let Err(e) = self.stop_stream(&stream_key).await {
                error!("Error stopping stream {} during shutdown: {}", stream_key, e);
            }
        }

        info!("Graceful shutdown complete");
    }
}
