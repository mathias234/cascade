use crate::cache::SegmentCache;
use crate::config::Config;
use crate::elasticsearch::ElasticsearchClient;
use crate::metrics::MetricsHistory;
use crate::models::{Stats, StreamInfo};
use crate::sessions::SessionManager;
use anyhow::Result;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use ez_ffmpeg::{FfmpegContext, FfmpegScheduler, Input, Output};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{
    fs,
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
    pub stats: Arc<Stats>,
    pub cache: SegmentCache,
    pub session_manager: Arc<SessionManager>,
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
            stats: Arc::new(Stats::new()),
            cache: SegmentCache::new(cache_entries, max_segment_size, cache_ttl_seconds),
            session_manager: Arc::new(SessionManager::new(config.clone())),
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

        // Configure input with timeout (rw_timeout is already in microseconds from config)
        let input_opts = vec![
            ("rw_timeout", self.ffmpeg_rw_timeout.to_string()),
            ("analyzeduration", "2000000".to_string()), // 2 seconds
            ("probesize", "1M".to_string()),
        ];

        let input = Input::from(rtmp_url.clone())
            .set_input_opts(input_opts);

        // Build FFmpeg context with filter and output configuration
        let (filter_desc, outputs) = if self.abr_config.enabled && !self.abr_config.variants.is_empty() {
            // ABR mode: Generate multiple variants
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

            // Build filter complex with split and scale for each variant
            let variant_count = self.abr_config.variants.len();
            let mut filter_complex = format!("[0:v]split={}", variant_count);

            // Add labeled outputs for the split filter
            for i in 0..variant_count {
                filter_complex.push_str(&format!("[split{}]", i));
            }

            // Add scaling filters for each variant with properly labeled outputs
            for (i, variant) in self.abr_config.variants.iter().enumerate() {
                filter_complex.push_str(&format!(
                    ";[split{}]scale={}:{},fps={}[vout{}]",
                    i, variant.width, variant.height, variant.framerate, i
                ));
            }

            // Build outputs for each variant
            let mut outputs = Vec::new();

            for (i, variant) in self.abr_config.variants.iter().enumerate() {
                let variant_output_path = stream_dir.join(&variant.name).join("index.m3u8");

                let video_codec_opts = vec![
                    ("preset", variant.preset.clone()),
                    ("profile", variant.profile.clone()),
                    ("b", format!("{}k", variant.bitrate)),
                    ("maxrate", format!("{}k", (variant.bitrate as f32 * 1.1) as u32)),
                    ("bufsize", format!("{}k", variant.bitrate * 2)),
                    ("g", (variant.framerate * 2).to_string()),
                    ("keyint_min", variant.framerate.to_string()),
                    ("sc_threshold", "0".to_string()),
                ];

                let audio_codec_opts = vec![
                    ("b", format!("{}k", variant.audio_bitrate)),
                    ("ac", "2".to_string()),
                ];

                let format_opts = vec![
                    ("hls_time", self.ffmpeg_hls_time.to_string()),
                    ("hls_list_size", self.ffmpeg_hls_list_size.to_string()),
                    ("hls_flags", "delete_segments+append_list".to_string()),
                    ("hls_segment_type", "mpegts".to_string()),
                    ("hls_segment_filename",
                        stream_dir.join(&variant.name).join("segment_%03d.ts").to_str().unwrap().to_string()),
                ];

                let output = Output::from(variant_output_path.to_str().unwrap())
                    .add_stream_map(&format!("vout{}", i))  // Map the scaled video from filter
                    .add_stream_map("0:a")  // Map audio directly from input
                    .set_format("hls")
                    .set_video_codec("libx264")
                    .set_audio_codec("aac")
                    .set_video_codec_opts(video_codec_opts)
                    .set_audio_codec_opts(audio_codec_opts)
                    .set_format_opts(format_opts);

                outputs.push(output);
            }

            // Create master playlist after the stream starts
            let master_playlist_path = stream_dir.join("master.m3u8");
            self.create_master_playlist(&master_playlist_path, &self.abr_config.variants).await?;

            (filter_complex, outputs)
        } else {
            // Single bitrate mode: Just copy the stream
            info!("Starting single bitrate stream {}", stream_key);
            let m3u8_path = stream_dir.join("index.m3u8");
            let segment_path = stream_dir.join(format!("{}_%03d.ts", stream_key));

            let format_opts = vec![
                ("hls_time", self.ffmpeg_hls_time.to_string()),
                ("hls_list_size", self.ffmpeg_hls_list_size.to_string()),
                ("hls_flags", "delete_segments+append_list".to_string()),
                ("hls_segment_filename", segment_path.to_str().unwrap().to_string()),
            ];

            let output = Output::from(m3u8_path.to_str().unwrap())
                .add_stream_map("0:v")  // Map video stream from input
                .add_stream_map("0:a")  // Map audio stream from input
                .set_format("hls")
                .set_video_codec("copy")
                .set_audio_codec("copy")
                .set_format_opts(format_opts);

            ("".to_string(), vec![output])  // Empty string means no filter
        };

        // Build FFmpeg context
        let mut builder = FfmpegContext::builder()
            .input(input);

        // Add filter if provided and not empty
        if !filter_desc.is_empty() {
            builder = builder.filter_desc(filter_desc);
        }

        // Add outputs
        for output in outputs {
            builder = builder.output(output);
        }

        let context = match builder.build() {
            Ok(ctx) => ctx,
            Err(e) => {
                error!("Failed to create FFmpeg context for stream {}: {}", stream_key, e);
                self.pending_streams.remove(&stream_key);
                self.stats.failed.fetch_add(1, Ordering::Relaxed);
                return Ok(false);
            }
        };

        debug!("Created FFmpeg context for stream: {}", stream_key);

        // Create and start the FFmpeg scheduler
        let scheduler = FfmpegScheduler::new(context);
        let running_scheduler = match scheduler.start() {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to start FFmpeg scheduler for stream {}: {}", stream_key, e);
                self.pending_streams.remove(&stream_key);
                self.stats.failed.fetch_add(1, Ordering::Relaxed);
                return Ok(false);
            }
        };

        let stream_info = StreamInfo {
            scheduler_handle: Arc::new(Mutex::new(Some(running_scheduler))),
            started_at: Utc::now(),
            last_accessed: Arc::new(RwLock::new(Utc::now())),
        };

        self.active_streams.insert(stream_key.clone(), stream_info);
        self.pending_streams.remove(&stream_key);
        self.stats.started.fetch_add(1, Ordering::Relaxed);

        info!("FFmpeg context started for stream: {}", stream_key);

        // Spawn a task to monitor the FFmpeg job
        let stream_key_monitor = stream_key.clone();
        let active_streams_monitor = self.active_streams.clone();
        let stats_monitor = self.stats.clone();
        let session_manager_monitor = self.session_manager.clone();
        let hls_path_monitor = self.hls_path.clone();

        tokio::spawn(async move {
            // Check periodically if the job has ended
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;

                if let Some(stream_info) = active_streams_monitor.get(&stream_key_monitor) {
                    let scheduler_lock = stream_info.scheduler_handle.lock().await;
                    if let Some(ref scheduler) = *scheduler_lock {
                        if scheduler.is_ended() {
                            drop(scheduler_lock);
                            drop(stream_info);

                            // FFmpeg job ended (likely failed) - clean up everything
                            error!("FFmpeg job ended unexpectedly for stream: {}", stream_key_monitor);

                            // Remove from active streams
                            active_streams_monitor.remove(&stream_key_monitor);

                            // Update stats
                            stats_monitor.failed.fetch_add(1, Ordering::Relaxed);

                            // Clear sessions for this stream
                            session_manager_monitor.clear_stream_sessions(&stream_key_monitor);

                            // Clean up all stream files
                            let stream_dir = hls_path_monitor.join(&stream_key_monitor);
                            if stream_dir.exists() {
                                if let Err(e) = fs::remove_dir_all(&stream_dir).await {
                                    error!("Failed to clean up stream directory for {}: {}", stream_key_monitor, e);
                                }
                            }

                            info!("Cleaned up failed stream {}, ready for restart", stream_key_monitor);
                            break;
                        }
                    } else {
                        // Scheduler was taken (stream was stopped gracefully)
                        break;
                    }
                } else {
                    // Stream was removed
                    break;
                }
            }
        });

        Ok(true)
    }

    pub async fn stop_stream(&self, stream_key: &str) -> Result<()> {
        info!("Stopping stream: {}", stream_key);

        let stream_info = self.active_streams.remove(stream_key).map(|(_, v)| v);

        if let Some(info) = stream_info {
            let mut scheduler_lock = info.scheduler_handle.lock().await;

            // Take the scheduler out of the option and abort it
            if let Some(scheduler) = scheduler_lock.take() {
                // Abort the FFmpeg job
                scheduler.abort();

                // The abort() method consumes the scheduler and signals it to end
                // No need to wait since abort() handles cleanup
            }

            self.stats.stopped.fetch_add(1, Ordering::Relaxed);
        }

        // Clear sessions for this stream
        self.session_manager.clear_stream_sessions(stream_key);

        // Add a small delay to ensure FFmpeg has released file handles
        tokio::time::sleep(Duration::from_millis(500)).await;

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

    async fn create_master_playlist(&self, path: &std::path::Path, variants: &[crate::config::StreamVariant]) -> Result<()> {
        let mut content = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");

        for variant in variants {
            content.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={},RESOLUTION={}x{},FRAMERATE={}\n",
                variant.bitrate * 1000 + variant.audio_bitrate * 1000, // Total bandwidth in bits
                variant.width,
                variant.height,
                variant.framerate
            ));
            content.push_str(&format!("{}/index.m3u8\n", variant.name));
        }

        fs::write(path, content).await?;
        debug!("Created master playlist at {:?}", path);
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
