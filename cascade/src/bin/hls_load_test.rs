use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Base URL of the HLS server
    #[arg(short = 'u', long, default_value = "http://localhost:8080")]
    base_url: String,

    /// Stream keys to test (comma-separated)
    #[arg(short, long, value_delimiter = ',')]
    streams: Vec<String>,

    /// Number of concurrent viewers per stream
    #[arg(short = 'v', long, default_value = "10")]
    viewers: usize,

    /// Test duration in seconds (0 for infinite)
    #[arg(short, long, default_value = "60")]
    duration: u64,

    /// Ramp-up time in seconds
    #[arg(short, long, default_value = "0")]
    ramp_up: u64,

    /// Playback behavior
    #[arg(short = 'b', long, value_enum, default_value = "steady")]
    behavior: PlaybackBehavior,

    /// Enable detailed metrics output
    #[arg(long)]
    detailed_metrics: bool,

    /// Metrics reporting interval in seconds
    #[arg(long, default_value = "5")]
    metrics_interval: u64,
}

#[derive(Debug, Clone, ValueEnum)]
enum PlaybackBehavior {
    /// Viewers join and stay for the duration
    Steady,
    /// Viewers randomly join and leave
    Random,
    /// Sudden spike of viewers
    Spike,
    /// Gradual increase then decrease
    Wave,
}

/// HLS playlist representation
#[derive(Debug, Clone)]
struct HlsPlaylist {
    segments: Vec<Segment>,
    target_duration: f64,
    media_sequence: u64,
    is_live: bool,
}

#[derive(Debug, Clone)]
struct Segment {
    uri: String,
    duration: f64,
    sequence: u64,
}

/// Performance metrics
#[derive(Debug, Default)]
struct Metrics {
    requests_sent: AtomicU64,
    requests_success: AtomicU64,
    requests_failed: AtomicU64,
    bytes_downloaded: AtomicU64,
    playlist_fetches: AtomicU64,
    segment_fetches: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    active_viewers: AtomicUsize,
    latencies: RwLock<Vec<Duration>>,
}

impl Metrics {
    async fn record_latency(&self, latency: Duration) {
        let mut latencies = self.latencies.write().await;
        latencies.push(latency);
        // Keep only last 10000 samples to avoid memory issues
        if latencies.len() > 10000 {
            latencies.drain(0..5000);
        }
    }

    async fn get_latency_percentiles(&self) -> (Duration, Duration, Duration) {
        let mut latencies = self.latencies.read().await.clone();
        if latencies.is_empty() {
            return (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        }

        latencies.sort_unstable();
        let p50_idx = latencies.len() / 2;
        let p95_idx = (latencies.len() * 95) / 100;
        let p99_idx = (latencies.len() * 99) / 100;

        (
            latencies[p50_idx.min(latencies.len() - 1)],
            latencies[p95_idx.min(latencies.len() - 1)],
            latencies[p99_idx.min(latencies.len() - 1)],
        )
    }

    fn get_throughput(&self, elapsed: Duration) -> f64 {
        let bytes = self.bytes_downloaded.load(Ordering::Relaxed) as f64;
        let seconds = elapsed.as_secs_f64();
        bytes / seconds / 1_048_576.0 // MB/s
    }
}

/// HLS Client simulator
struct HlsClient {
    base_url: String,
    stream_key: String,
    session_id: Option<String>,
    http_client: reqwest::Client,
    metrics: Arc<Metrics>,
    last_sequence: u64,
}

impl HlsClient {
    fn new(base_url: String, stream_key: String, metrics: Arc<Metrics>) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        Self {
            base_url,
            stream_key,
            session_id: None,
            http_client,
            metrics,
            last_sequence: 0,
        }
    }

    /// Fetch master playlist and get session context
    async fn fetch_master_playlist(&mut self) -> Result<()> {
        let url = format!("{}/live/{}/index.m3u8", self.base_url, self.stream_key);
        let start = Instant::now();

        debug!("Fetching master playlist: {}", url);
        self.metrics.requests_sent.fetch_add(1, Ordering::Relaxed);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch master playlist")?;

        let latency = start.elapsed();
        self.metrics.record_latency(latency).await;

        if !response.status().is_success() {
            self.metrics.requests_failed.fetch_add(1, Ordering::Relaxed);
            return Err(anyhow::anyhow!("HTTP {}", response.status()));
        }

        self.metrics
            .requests_success
            .fetch_add(1, Ordering::Relaxed);
        self.metrics
            .playlist_fetches
            .fetch_add(1, Ordering::Relaxed);

        let content = response.text().await?;
        self.metrics
            .bytes_downloaded
            .fetch_add(content.len() as u64, Ordering::Relaxed);

        // Parse master playlist to extract the actual playlist URL with context
        for line in content.lines() {
            if line.starts_with("/live/") && line.contains("hls_ctx=") {
                // Extract session ID from the URL
                if let Some(ctx_start) = line.find("hls_ctx=") {
                    let ctx = &line[ctx_start + 8..];
                    self.session_id = Some(ctx.to_string());
                    debug!("Got session ID: {}", ctx);
                    break;
                }
            }
        }

        Ok(())
    }

    /// Fetch the actual playlist with segments
    async fn fetch_playlist(&mut self) -> Result<HlsPlaylist> {
        let url = if let Some(session_id) = &self.session_id {
            format!(
                "{}/live/{}/index.m3u8?hls_ctx={}",
                self.base_url, self.stream_key, session_id
            )
        } else {
            // Shouldn't happen in normal flow, but handle it
            format!("{}/live/{}/index.m3u8", self.base_url, self.stream_key)
        };

        let start = Instant::now();
        debug!("Fetching playlist: {}", url);
        self.metrics.requests_sent.fetch_add(1, Ordering::Relaxed);

        let response = self.http_client.get(&url).send().await?;
        let latency = start.elapsed();
        self.metrics.record_latency(latency).await;

        if !response.status().is_success() {
            self.metrics.requests_failed.fetch_add(1, Ordering::Relaxed);
            return Err(anyhow::anyhow!("HTTP {}", response.status()));
        }

        // Check for cache hit header
        if let Some(cache_status) = response.headers().get("x-cache-status") {
            if cache_status == "HIT" {
                self.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
            } else {
                self.metrics.cache_misses.fetch_add(1, Ordering::Relaxed);
            }
        }

        self.metrics
            .requests_success
            .fetch_add(1, Ordering::Relaxed);
        self.metrics
            .playlist_fetches
            .fetch_add(1, Ordering::Relaxed);

        let content = response.text().await?;
        self.metrics
            .bytes_downloaded
            .fetch_add(content.len() as u64, Ordering::Relaxed);

        self.parse_playlist(&content)
    }

    /// Parse M3U8 playlist content
    fn parse_playlist(&self, content: &str) -> Result<HlsPlaylist> {
        let mut segments = Vec::new();
        let mut target_duration = 6.0;
        let mut media_sequence = 0;
        let mut is_live = true;
        let mut current_duration = 0.0;
        let mut sequence_counter = 0;

        for line in content.lines() {
            if line.starts_with("#EXT-X-TARGETDURATION:") {
                target_duration = line[22..].parse().unwrap_or(6.0);
            } else if line.starts_with("#EXT-X-MEDIA-SEQUENCE:") {
                media_sequence = line[22..].parse().unwrap_or(0);
                sequence_counter = media_sequence;
            } else if line.starts_with("#EXTINF:") {
                let duration_str = line[8..].split(',').next().unwrap_or("0");
                current_duration = duration_str.parse().unwrap_or(0.0);
            } else if line.starts_with("#EXT-X-ENDLIST") {
                is_live = false;
            } else if line.ends_with(".ts") {
                segments.push(Segment {
                    uri: line.to_string(),
                    duration: current_duration,
                    sequence: sequence_counter,
                });
                sequence_counter += 1;
                current_duration = 0.0;
            }
        }

        Ok(HlsPlaylist {
            segments,
            target_duration,
            media_sequence,
            is_live,
        })
    }

    /// Download a segment
    async fn download_segment(&self, segment: &Segment) -> Result<()> {
        let url = format!("{}{}", self.base_url, segment.uri);
        let start = Instant::now();

        debug!(
            "Downloading segment: {} (seq: {})",
            segment.uri, segment.sequence
        );
        self.metrics.requests_sent.fetch_add(1, Ordering::Relaxed);

        let response = self.http_client.get(&url).send().await?;
        let latency = start.elapsed();
        self.metrics.record_latency(latency).await;

        if !response.status().is_success() {
            self.metrics.requests_failed.fetch_add(1, Ordering::Relaxed);
            return Err(anyhow::anyhow!("HTTP {}", response.status()));
        }

        // Check for cache hit
        if let Some(cache_status) = response.headers().get("x-cache-status") {
            if cache_status == "HIT" {
                self.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
            } else {
                self.metrics.cache_misses.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Download the segment data (but don't decode it)
        let bytes = response.bytes().await?;

        self.metrics
            .requests_success
            .fetch_add(1, Ordering::Relaxed);
        self.metrics.segment_fetches.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .bytes_downloaded
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);

        debug!(
            "Downloaded segment {} ({} bytes)",
            segment.sequence,
            bytes.len()
        );
        Ok(())
    }

    /// Simulate HLS playback
    async fn play(&mut self, duration: Duration) -> Result<()> {
        info!("Starting playback for stream: {}", self.stream_key);
        self.metrics.active_viewers.fetch_add(1, Ordering::Relaxed);

        // Fetch master playlist first
        self.fetch_master_playlist().await?;

        let start_time = Instant::now();
        let mut playlist_refresh_interval = interval(Duration::from_secs(6));

        while start_time.elapsed() < duration || duration.is_zero() {
            // Fetch playlist
            let playlist = match self.fetch_playlist().await {
                Ok(p) => p,
                Err(e) => {
                    warn!("Failed to fetch playlist: {}", e);
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            // Update refresh interval based on target duration
            playlist_refresh_interval =
                interval(Duration::from_secs_f64(playlist.target_duration / 2.0));

            // Download new segments
            for segment in &playlist.segments {
                // Skip segments we've already downloaded
                if segment.sequence <= self.last_sequence {
                    continue;
                }

                // Download segment
                if let Err(e) = self.download_segment(segment).await {
                    warn!("Failed to download segment {}: {}", segment.sequence, e);
                    continue;
                }

                self.last_sequence = segment.sequence;

                // Simulate playback timing - wait for segment duration
                // In real playback, this would be filling a buffer
                sleep(Duration::from_secs_f64(segment.duration)).await;

                // Check if we should stop
                if !duration.is_zero() && start_time.elapsed() >= duration {
                    break;
                }
            }

            // Wait for next playlist refresh for live streams
            if playlist.is_live {
                playlist_refresh_interval.tick().await;
            } else {
                // VOD playlist, we're done
                break;
            }
        }

        self.metrics.active_viewers.fetch_sub(1, Ordering::Relaxed);
        info!("Playback ended for stream: {}", self.stream_key);
        Ok(())
    }
}

/// Spawn viewers according to behavior pattern
async fn spawn_viewers(
    args: &Args,
    stream: String,
    metrics: Arc<Metrics>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();

    match args.behavior {
        PlaybackBehavior::Steady => {
            // All viewers join at once
            for i in 0..args.viewers {
                let base_url = args.base_url.clone();
                let stream_key = stream.clone();
                let metrics = metrics.clone();
                let duration = Duration::from_secs(args.duration);

                let handle = tokio::spawn(async move {
                    let mut client = HlsClient::new(base_url, stream_key, metrics);
                    if let Err(e) = client.play(duration).await {
                        error!("Viewer {} playback error: {}", i, e);
                    }
                });

                handles.push(handle);

                // Small delay between viewer starts to avoid thundering herd
                if args.ramp_up > 0 {
                    let delay = Duration::from_secs(args.ramp_up) / args.viewers as u32;
                    sleep(delay).await;
                }
            }
        }
        PlaybackBehavior::Random => {
            // Viewers join and leave randomly
            for i in 0..args.viewers {
                let base_url = args.base_url.clone();
                let stream_key = stream.clone();
                let metrics = metrics.clone();
                let max_duration = Duration::from_secs(args.duration);

                let handle = tokio::spawn(async move {
                    // Random delay before joining
                    let join_delay = Duration::from_secs(rand::random::<u64>() % 10);
                    sleep(join_delay).await;

                    // Random playback duration
                    let play_duration = Duration::from_secs(
                        30 + (rand::random::<u64>() % max_duration.as_secs().max(30)),
                    );

                    let mut client = HlsClient::new(base_url, stream_key, metrics);
                    if let Err(e) = client.play(play_duration).await {
                        error!("Viewer {} playback error: {}", i, e);
                    }
                });

                handles.push(handle);
            }
        }
        PlaybackBehavior::Spike => {
            // Half viewers join immediately, other half joins after delay
            let spike_delay = Duration::from_secs(args.duration / 4);
            let duration = Duration::from_secs(args.duration);

            for i in 0..args.viewers {
                let base_url = args.base_url.clone();
                let stream_key = stream.clone();
                let metrics = metrics.clone();
                let is_spike = i >= args.viewers / 2;

                let handle = tokio::spawn(async move {
                    if is_spike {
                        sleep(spike_delay).await;
                    }

                    let mut client = HlsClient::new(base_url, stream_key, metrics);
                    if let Err(e) = client.play(duration).await {
                        error!("Viewer {} playback error: {}", i, e);
                    }
                });

                handles.push(handle);
            }
        }
        PlaybackBehavior::Wave => {
            // Gradual increase then decrease
            let wave_period = Duration::from_secs(args.duration / 3);
            let duration = Duration::from_secs(args.duration);

            for i in 0..args.viewers {
                let base_url = args.base_url.clone();
                let stream_key = stream.clone();
                let metrics = metrics.clone();

                // Stagger joins across the first third of the test
                let join_delay = (wave_period * i as u32) / args.viewers as u32;

                let handle = tokio::spawn(async move {
                    sleep(join_delay).await;

                    // Play for varying durations to create wave effect
                    let play_duration = duration - join_delay;

                    let mut client = HlsClient::new(base_url, stream_key, metrics);
                    if let Err(e) = client.play(play_duration).await {
                        error!("Viewer {} playback error: {}", i, e);
                    }
                });

                handles.push(handle);
            }
        }
    }

    handles
}

/// Print metrics report
async fn print_metrics(metrics: &Arc<Metrics>, elapsed: Duration, detailed: bool) {
    let (p50, p95, p99) = metrics.get_latency_percentiles().await;
    let throughput = metrics.get_throughput(elapsed);

    println!("\n=== Performance Metrics ===");
    println!("Test Duration: {:.1}s", elapsed.as_secs_f64());
    println!(
        "Active Viewers: {}",
        metrics.active_viewers.load(Ordering::Relaxed)
    );

    println!("\n--- Requests ---");
    println!("Total: {}", metrics.requests_sent.load(Ordering::Relaxed));
    println!(
        "Success: {}",
        metrics.requests_success.load(Ordering::Relaxed)
    );
    println!(
        "Failed: {}",
        metrics.requests_failed.load(Ordering::Relaxed)
    );
    println!(
        "Success Rate: {:.2}%",
        (metrics.requests_success.load(Ordering::Relaxed) as f64
            / metrics.requests_sent.load(Ordering::Relaxed).max(1) as f64)
            * 100.0
    );

    println!("\n--- Content ---");
    println!(
        "Playlists: {}",
        metrics.playlist_fetches.load(Ordering::Relaxed)
    );
    println!(
        "Segments: {}",
        metrics.segment_fetches.load(Ordering::Relaxed)
    );
    println!(
        "Data Downloaded: {:.2} MB",
        metrics.bytes_downloaded.load(Ordering::Relaxed) as f64 / 1_048_576.0
    );
    println!("Throughput: {:.2} MB/s", throughput);

    println!("\n--- Cache ---");
    let cache_total =
        metrics.cache_hits.load(Ordering::Relaxed) + metrics.cache_misses.load(Ordering::Relaxed);
    if cache_total > 0 {
        println!("Cache Hits: {}", metrics.cache_hits.load(Ordering::Relaxed));
        println!(
            "Cache Misses: {}",
            metrics.cache_misses.load(Ordering::Relaxed)
        );
        println!(
            "Hit Rate: {:.2}%",
            (metrics.cache_hits.load(Ordering::Relaxed) as f64 / cache_total as f64) * 100.0
        );
    }

    println!("\n--- Latency ---");
    println!("P50: {:.1}ms", p50.as_secs_f64() * 1000.0);
    println!("P95: {:.1}ms", p95.as_secs_f64() * 1000.0);
    println!("P99: {:.1}ms", p99.as_secs_f64() * 1000.0);

    if detailed {
        println!("\n--- Request Rate ---");
        let requests_per_sec =
            metrics.requests_sent.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64();
        println!("Requests/sec: {:.1}", requests_per_sec);

        let segments_per_sec =
            metrics.segment_fetches.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64();
        println!("Segments/sec: {:.1}", segments_per_sec);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    if args.streams.is_empty() {
        eprintln!("Error: At least one stream key must be provided");
        std::process::exit(1);
    }

    info!("Starting HLS load test");
    info!("Target: {}", args.base_url);
    info!("Streams: {:?}", args.streams);
    info!("Viewers per stream: {}", args.viewers);
    info!("Duration: {}s", args.duration);
    info!("Behavior: {:?}", args.behavior);

    let metrics = Arc::new(Metrics::default());
    let start_time = Instant::now();

    // Start metrics reporter
    let metrics_clone = metrics.clone();
    let metrics_interval = args.metrics_interval;
    let detailed = args.detailed_metrics;
    let metrics_handle = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(metrics_interval));
        loop {
            interval.tick().await;
            let elapsed = start_time.elapsed();
            print_metrics(&metrics_clone, elapsed, detailed).await;
        }
    });

    // Spawn viewers for each stream
    let mut all_handles = Vec::new();
    for stream in args.streams.iter() {
        info!("Starting viewers for stream: {}", stream);
        let handles = spawn_viewers(&args, stream.clone(), metrics.clone()).await;
        all_handles.extend(handles);
    }

    // Wait for test duration or all viewers to finish
    if args.duration > 0 {
        sleep(Duration::from_secs(args.duration)).await;
    } else {
        // Wait for all viewers
        for handle in all_handles {
            let _ = handle.await;
        }
    }

    // Final metrics
    metrics_handle.abort();
    let elapsed = start_time.elapsed();
    println!("\n\n=== FINAL REPORT ===");
    print_metrics(&metrics, elapsed, true).await;

    Ok(())
}
