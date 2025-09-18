use crate::cache::SegmentCache;
use crate::config::Config;
use crate::elasticsearch::ElasticsearchClient;
use crate::metrics::MetricsHistory;
use crate::models::{Stats, StreamInfo};
use crate::sessions::SessionManager;
use anyhow::Result;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{Arc, atomic::Ordering},
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
    pub active_streams: Arc<DashMap<String, StreamInfo>>,
    pub pending_streams: Arc<DashMap<String, DateTime<Utc>>>,
    pub failed_streams: Arc<DashMap<String, DateTime<Utc>>>,
    pub stats: Arc<Stats>,
    pub cache: SegmentCache,
    pub session_manager: SessionManager,
    pub server_started_at: DateTime<Utc>,
    pub metrics_history: MetricsHistory,
    pub elasticsearch: ElasticsearchClient,
    // FFmpeg configuration
    pub ffmpeg_hls_time: u32,
    pub ffmpeg_hls_list_size: u32,
    pub ffmpeg_rw_timeout: u32,
    // ABR configuration
    pub abr_config: crate::config::AbrConfig,
}

impl StreamManager {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        let srs_host = config.rtmp.source_host.clone();
        let srs_port = config.rtmp.source_port.to_string();
        let hls_path = PathBuf::from(&config.server.hls_path);

        let stream_timeout = Duration::from_secs(config.stream.stream_timeout);
        let max_concurrent_streams = config.stream.max_concurrent_streams;
        let stream_start_timeout = Duration::from_secs(config.stream.stream_start_timeout);

        info!("Stream Manager starting...");
        info!("Source: rtmp://{}:{}/live/", srs_host, srs_port);
        info!("Output: {:?}", hls_path);
        info!("Max concurrent streams: {}", max_concurrent_streams);
        info!("Stream timeout: {:?}", stream_timeout);

        // Cache configuration
        let cache_entries = config.cache.max_entries;
        let max_segment_size = config.cache.max_segment_size;
        let cache_ttl_seconds = config.cache.ttl_seconds;

        // FFmpeg configuration
        let ffmpeg_hls_time = config.ffmpeg.hls_time as u32;
        let ffmpeg_hls_list_size = config.ffmpeg.hls_list_size as u32;
        let ffmpeg_rw_timeout = config.ffmpeg.rw_timeout as u32;

        info!(
            "FFmpeg config: HLS time={}, list size={}, rw_timeout={}",
            ffmpeg_hls_time, ffmpeg_hls_list_size, ffmpeg_rw_timeout
        );

        let elasticsearch = ElasticsearchClient::new(&config.elasticsearch)?;

        Ok(StreamManager {
            srs_host,
            srs_port,
            hls_path,
            stream_timeout,
            max_concurrent_streams,
            stream_start_timeout,
            active_streams: Arc::new(DashMap::new()),
            pending_streams: Arc::new(DashMap::new()),
            failed_streams: Arc::new(DashMap::new()),
            stats: Arc::new(Stats::new()),
            cache: SegmentCache::new(cache_entries, max_segment_size, cache_ttl_seconds),
            session_manager: SessionManager::new(config.clone()),
            server_started_at: Utc::now(),
            metrics_history: MetricsHistory::new(),
            elasticsearch,
            ffmpeg_hls_time,
            ffmpeg_hls_list_size,
            ffmpeg_rw_timeout,
            abr_config: config.abr.clone(),
        })
    }

    pub async fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.hls_path).await?;
        self.cleanup_all_streams().await?;

        // Initialize Elasticsearch index template
        if let Err(e) = self.elasticsearch.create_index_template().await {
            warn!("Failed to create Elasticsearch index template: {}", e);
        }

        Ok(())
    }

    async fn cleanup_all_streams(&self) -> Result<()> {
        info!("Cleaning up HLS directory...");
        let mut count = 0;

        let mut entries = fs::read_dir(&self.hls_path).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if let Some(ext) = path.extension()
                && (ext == "m3u8" || ext == "ts")
            {
                fs::remove_file(path).await.ok();
                count += 1;
            }
        }

        if count > 0 {
            info!("Cleaned up {} files from previous runs", count);
        }
        Ok(())
    }

    pub async fn start_stream(&self, stream_key: String) -> Result<bool> {
        if self.active_streams.contains_key(&stream_key) {
            debug!("Stream {} already active", stream_key);
            return Ok(true);
        }

        if self.pending_streams.contains_key(&stream_key) {
            debug!("Stream {} already pending", stream_key);
            return Ok(true);
        }

        if self.active_streams.len() >= self.max_concurrent_streams {
            warn!(
                "Max concurrent streams ({}) reached, cannot start {}",
                self.max_concurrent_streams, stream_key
            );
            return Ok(false);
        }

        // Remove from failed streams to allow immediate retry
        self.failed_streams.remove(&stream_key);

        self.pending_streams.insert(stream_key.clone(), Utc::now());

        debug!("Spawning FFmpeg for stream: {}", stream_key);

        let rtmp_url = format!(
            "rtmp://{}:{}/live/{}",
            self.srs_host, self.srs_port, stream_key
        );

        // Clean up any old files first
        self.cleanup_stream_files(&stream_key).await?;

        // Create stream-specific directory
        let stream_dir = self.hls_path.join(&stream_key);
        tokio::fs::create_dir_all(&stream_dir).await.ok();

        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-nostdin")
            .arg("-loglevel")
            .arg("warning")
            .arg("-rw_timeout")
            .arg(self.ffmpeg_rw_timeout.to_string())
            .arg("-i")
            .arg(&rtmp_url);

        if self.abr_config.enabled && !self.abr_config.variants.is_empty() {
            // ABR mode: Generate multiple variants using a single FFmpeg command
            info!(
                "Starting ABR stream {} with {} variants",
                stream_key,
                self.abr_config.variants.len()
            );

            // Create directories for all variants
            for variant in &self.abr_config.variants {
                let variant_dir = stream_dir.join(&variant.name);
                tokio::fs::create_dir_all(&variant_dir).await.ok();
            }

            // Build filter complex with split for each variant
            let variant_count = self.abr_config.variants.len();
            let mut filter_complex = format!("[0:v]split={}", variant_count);

            // Add split outputs
            for i in 0..variant_count {
                filter_complex.push_str(&format!("[v{}]", i));
            }
            filter_complex.push_str("; ");

            // Add scaling for each variant
            for (i, variant) in self.abr_config.variants.iter().enumerate() {
                if i > 0 {
                    filter_complex.push_str("; ");
                }
                filter_complex.push_str(&format!(
                    "[v{}]scale={}:{},fps={}[v{}out]",
                    i, variant.width, variant.height, variant.framerate, i
                ));
            }

            cmd.arg("-filter_complex").arg(&filter_complex);

            // Map video and audio for each variant
            for (i, variant) in self.abr_config.variants.iter().enumerate() {
                // Map the scaled video stream
                cmd.arg("-map").arg(format!("[v{}out]", i));

                // Video encoding settings
                cmd.arg(format!("-c:v:{}", i))
                    .arg("libx264")
                    .arg(format!("-preset:v:{}", i))
                    .arg(&variant.preset)
                    .arg(format!("-profile:v:{}", i))
                    .arg(&variant.profile)
                    .arg(format!("-b:v:{}", i))
                    .arg(format!("{}k", variant.bitrate))
                    .arg(format!("-maxrate:v:{}", i))
                    .arg(format!("{}k", (variant.bitrate as f32 * 1.1) as u32))
                    .arg(format!("-bufsize:v:{}", i))
                    .arg(format!("{}k", variant.bitrate * 2))
                    .arg(format!("-g:v:{}", i))
                    .arg((variant.framerate * 2).to_string())
                    .arg(format!("-keyint_min:v:{}", i))
                    .arg(variant.framerate.to_string())
                    .arg(format!("-sc_threshold:v:{}", i))
                    .arg("0");
            }

            // Map audio for each variant
            for (i, variant) in self.abr_config.variants.iter().enumerate() {
                cmd.arg("-map").arg("0:a");

                // Audio encoding settings
                cmd.arg(format!("-c:a:{}", i))
                    .arg("aac")
                    .arg(format!("-b:a:{}", i))
                    .arg(format!("{}k", variant.audio_bitrate))
                    .arg(format!("-ac:{}", i))
                    .arg("2");
            }

            // HLS output settings (single format for all variants)
            cmd.arg("-f")
                .arg("hls")
                .arg("-hls_time")
                .arg(self.ffmpeg_hls_time.to_string())
                .arg("-hls_list_size")
                .arg(self.ffmpeg_hls_list_size.to_string())
                .arg("-hls_flags")
                .arg("delete_segments+append_list+independent_segments")
                .arg("-hls_segment_type")
                .arg("mpegts")
                .arg("-hls_segment_filename")
                .arg(stream_dir.join("%v/segment_%03d.ts").to_str().unwrap())
                .arg("-master_pl_name")
                .arg("master.m3u8");

            // Build var_stream_map
            let mut var_stream_map = String::new();
            for i in 0..variant_count {
                if i > 0 {
                    var_stream_map.push(' ');
                }
                var_stream_map.push_str(&format!(
                    "v:{},a:{},name:{}",
                    i, i, self.abr_config.variants[i].name
                ));
            }

            cmd.arg("-var_stream_map")
                .arg(var_stream_map)
                .arg(stream_dir.join("%v/index.m3u8").to_str().unwrap());
        } else {
            // Single bitrate mode: Just copy the stream
            info!("Starting single bitrate stream {}", stream_key);
            let m3u8_path = stream_dir.join("index.m3u8");
            let segment_path = stream_dir.join(format!("{}_%03d.ts", stream_key));

            cmd.arg("-c")
                .arg("copy")
                .arg("-f")
                .arg("hls")
                .arg("-hls_time")
                .arg(self.ffmpeg_hls_time.to_string())
                .arg("-hls_list_size")
                .arg(self.ffmpeg_hls_list_size.to_string())
                .arg("-hls_flags")
                .arg("delete_segments+append_list")
                .arg("-hls_segment_filename")
                .arg(segment_path.to_str().unwrap())
                .arg(m3u8_path.to_str().unwrap());
        }

        debug!("FFMPEG Command {:#?}", cmd);

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        match cmd.spawn() {
            Ok(mut child) => {
                let pid = child.id().unwrap_or(0);
                info!("FFmpeg process spawned for {}, PID: {}", stream_key, pid);

                // Spawn a task to read and log FFmpeg stderr
                let failed_streams_clone = self.failed_streams.clone();
                let active_streams_clone = self.active_streams.clone();
                let pending_streams_clone = self.pending_streams.clone();
                if let Some(stderr) = child.stderr.take() {
                    let stream_key_clone = stream_key.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncBufReadExt, BufReader};
                        let reader = BufReader::new(stderr);
                        let mut lines = reader.lines();

                        while let Ok(Some(line)) = lines.next_line().await {
                            debug!("FFmpeg stderr [{}]: {}", stream_key_clone, line);

                            // Check for FFmpeg I/O errors (RTMP source unavailable or disconnected)
                            if line.contains("Error opening input: I/O error")
                                || line.contains("Error opening input files: I/O error")
                                || line.contains("Error during demuxing: I/O error")
                                || line
                                    .contains("Error retrieving a packet from demuxer: I/O error")
                            {
                                error!(
                                    "FFmpeg I/O error for stream {} (source unavailable or disconnected): {}",
                                    stream_key_clone, line
                                );

                                // Remove from pending if still there
                                pending_streams_clone.remove(&stream_key_clone);

                                // Remove from active streams
                                if let Some((_, stream_info)) =
                                    active_streams_clone.remove(&stream_key_clone)
                                {
                                    let mut process = stream_info.process.lock().await;
                                    let _ = process.kill().await;
                                }

                                // Mark stream as failed
                                failed_streams_clone.insert(stream_key_clone.clone(), Utc::now());

                                break;
                            }
                        }
                    });
                }

                // Spawn a task to read and log FFmpeg stdout
                if let Some(stdout) = child.stdout.take() {
                    let stream_key_clone = stream_key.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncBufReadExt, BufReader};
                        let reader = BufReader::new(stdout);
                        let mut lines = reader.lines();

                        while let Ok(Some(line)) = lines.next_line().await {
                            debug!("FFmpeg stdout [{}]: {}", stream_key_clone, line);
                        }
                    });
                }

                let stream_info = StreamInfo {
                    pid,
                    process: Arc::new(Mutex::new(child)),
                    started_at: Utc::now(),
                    last_accessed: Arc::new(RwLock::new(Utc::now())),
                };

                self.active_streams.insert(stream_key.clone(), stream_info);

                self.pending_streams.remove(&stream_key);

                self.stats.started.fetch_add(1, Ordering::Relaxed);

                Ok(true)
            }
            Err(e) => {
                error!("Failed to start stream {}: {}", stream_key, e);

                self.pending_streams.remove(&stream_key);

                self.failed_streams.insert(stream_key, Utc::now());

                self.stats.failed.fetch_add(1, Ordering::Relaxed);

                Ok(false)
            }
        }
    }

    pub async fn stop_stream(&self, stream_key: &str) -> Result<()> {
        info!("Stopping stream: {}", stream_key);

        let stream_info = self.active_streams.remove(stream_key).map(|(_, v)| v);

        if let Some(info) = stream_info {
            let mut process = info.process.lock().await;

            if let Err(e) = process.kill().await {
                error!("Error killing stream {}: {}", stream_key, e);
            }

            let _ = process.wait().await;

            self.stats.stopped.fetch_add(1, Ordering::Relaxed);
        }

        // Clear sessions for this stream
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
        // Check for ABR master playlist first
        if self.abr_config.enabled && !self.abr_config.variants.is_empty() {
            let master_path = self.hls_path.join(stream_key).join("master.m3u8");
            if master_path.exists() {
                // Check if at least one variant has segments
                for variant in &self.abr_config.variants {
                    let variant_m3u8 = self
                        .hls_path
                        .join(stream_key)
                        .join(&variant.name)
                        .join("index.m3u8");
                    if let Ok(content) = fs::read_to_string(&variant_m3u8).await {
                        if content.matches(".ts").count() >= 1 {
                            return true;
                        }
                    }
                }
            }
        } else {
            // Single bitrate mode
            let m3u8_path = self.hls_path.join(stream_key).join("index.m3u8");
            if let Ok(content) = fs::read_to_string(&m3u8_path).await {
                let segments = content.matches(".ts").count();
                return segments >= 1;
            }
        }

        false
    }

    pub async fn wait_for_stream(&self, stream_key: String) -> bool {
        // Check if stream is already active and ready
        if let Some(stream) = self.active_streams.get(&stream_key) {
            // Stream is already active, check if it's ready
            if self.stream_ready(&stream_key).await {
                let mut last_accessed = stream.last_accessed.write().await;
                *last_accessed = Utc::now();
                debug!("Stream {} already active and ready", stream_key);
                return true;
            }
            // Stream is active but not ready yet, fall through to wait
            debug!("Stream {} is active but not ready yet", stream_key);
        }

        // Try to start the stream if it's not already starting
        let need_to_start = !self.pending_streams.contains_key(&stream_key)
            && !self.active_streams.contains_key(&stream_key);

        if need_to_start {
            info!("Starting stream: {}", stream_key);
            if !self.start_stream(stream_key.clone()).await.unwrap_or(false) {
                return false;
            }
        } else {
            debug!(
                "Stream {} is already starting or active, waiting for it to be ready",
                stream_key
            );
        }

        // Wait for stream to become ready (regardless of who started it)
        let start = Instant::now();
        let mut check_interval = Duration::from_millis(200);

        while start.elapsed() < self.stream_start_timeout {
            if self.stream_ready(&stream_key).await {
                if let Some(stream) = self.active_streams.get(&stream_key) {
                    let mut last_accessed = stream.last_accessed.write().await;
                    *last_accessed = Utc::now();
                }
                info!(
                    "Stream {} became ready after {:?}",
                    stream_key,
                    start.elapsed()
                );
                return true;
            }

            if self.failed_streams.contains_key(&stream_key) {
                error!("Stream {} failed to start", stream_key);
                return false;
            }

            time::sleep(check_interval).await;
            check_interval = std::cmp::min(
                Duration::from_millis((check_interval.as_millis() as f64 * 1.2) as u64),
                Duration::from_secs(1),
            );
        }

        warn!(
            "Timeout waiting for stream {} after {:?}",
            stream_key, self.stream_start_timeout
        );
        false
    }

    pub async fn cleanup_idle_streams(&self) {
        info!("Cleanup thread started");
        let mut interval = time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;

            let mut streams_to_stop = Vec::new();

            info!(
                "Checking {} active streams for health",
                self.active_streams.len()
            );

            for entry in self.active_streams.iter() {
                let stream_key = entry.key();
                let stream_info = entry.value();
                let last_accessed = stream_info.last_accessed.read().await;
                let idle_duration = Utc::now().signed_duration_since(*last_accessed);

                if idle_duration.num_seconds() > self.stream_timeout.as_secs() as i64 {
                    streams_to_stop.push(stream_key.clone());
                }
            }

            for stream_key in streams_to_stop {
                info!(
                    "Stream {} idle for more than {:?}, stopping",
                    stream_key, self.stream_timeout
                );
                if let Err(e) = self.stop_stream(&stream_key).await {
                    error!("Error stopping idle stream {}: {}", stream_key, e);
                }
            }

            // No need to clean up failed streams since they're removed on retry

            // Clean up inactive viewers and sessions
            self.session_manager.cleanup_expired_sessions();
        }
    }

    pub async fn update_stream_access(&self, stream_key: &str) {
        if let Some(stream) = self.active_streams.get(stream_key) {
            let mut last_accessed = stream.last_accessed.write().await;
            *last_accessed = Utc::now();
            debug!("Updated access time for stream {}", stream_key);
        }
    }

    pub async fn graceful_shutdown(&self) {
        info!("Starting graceful shutdown...");

        // Flush any pending Elasticsearch metrics
        self.elasticsearch.flush().await;

        self.pending_streams.clear();

        let active_keys: Vec<String> = self
            .active_streams
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for stream_key in active_keys {
            if let Err(e) = self.stop_stream(&stream_key).await {
                error!(
                    "Error stopping stream {} during shutdown: {}",
                    stream_key, e
                );
            }
        }

        info!("Graceful shutdown complete");
    }
}
