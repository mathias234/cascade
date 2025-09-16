use anyhow::Result;
use bytes::Bytes;
use moka::future::Cache;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tracing::{debug, error, info};

#[derive(Clone)]
pub enum SegmentData {
    Memory(Bytes),
}

impl SegmentData {
    pub fn to_bytes(&self) -> Bytes {
        match self {
            SegmentData::Memory(bytes) => bytes.clone(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            SegmentData::Memory(bytes) => bytes.len(),
        }
    }
}

#[derive(Clone)]
pub struct CachedSegment {
    pub data: SegmentData,
    pub content_type: &'static str,
}

pub struct CacheStats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub memory_bytes: AtomicU64,
}

impl CacheStats {
    fn new() -> Self {
        Self {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            memory_bytes: AtomicU64::new(0),
        }
    }
}

pub struct SegmentCache {
    // Moka cache handles all concurrency internally - no locks needed!
    cache: Cache<String, CachedSegment>,
    // Cache configuration
    max_file_size: usize,
    // Statistics (public for direct access)
    pub stats: Arc<CacheStats>,
}

impl SegmentCache {
    pub fn new(max_entries: usize, max_segment_size: usize) -> Self {
        info!(
            "Initializing segment cache with {} entries, max segment size: {} bytes",
            max_entries, max_segment_size
        );

        // Build moka cache with TTL and max capacity
        let cache = Cache::builder()
            .max_capacity(max_entries as u64)
            .time_to_live(Duration::from_secs(300))
            .build();

        Self {
            cache,
            max_file_size: 100 * 1024 * 1024, // 100MB max file size
            stats: Arc::new(CacheStats::new()),
        }
    }

    pub async fn get_or_load(
        &self,
        key: &str,
        file_path: &Path,
    ) -> Result<Option<(CachedSegment, bool)>> {
        // First check if it's already cached
        if let Some(segment) = self.cache.get(key).await {
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
            debug!("Cache HIT: {}", key);
            return Ok(Some((segment, true)));
        }

        // Not cached, need to load from disk
        // Clone necessary data for the async closure
        let file_path = file_path.to_path_buf();
        let max_file_size = self.max_file_size;
        let stats = Arc::clone(&self.stats);
        let key_string = key.to_string();

        // Use try_get_with for atomic load-or-compute
        // Moka ensures only one load happens even with concurrent requests
        let result = self
            .cache
            .try_get_with(key_string.clone(), async move {
                // Get file metadata (also checks if file exists)
                let metadata = match tokio::fs::metadata(&file_path).await {
                    Ok(m) => m,
                    Err(e) => {
                        debug!("File not found: {:?} - {}", file_path, e);
                        return Err(anyhow::anyhow!("File not found"));
                    }
                };
                let file_size = metadata.len() as usize;

                // Validate file size
                if file_size > max_file_size {
                    return Err(anyhow::anyhow!(
                        "File exceeds maximum size limit: {} > {}",
                        file_size,
                        max_file_size
                    ));
                }

                // Determine content type based on extension
                let content_type = match file_path.extension().and_then(|s| s.to_str()) {
                    Some("ts") => "video/MP2T",
                    _ => "application/octet-stream",
                };

                let data = tokio::fs::read(&file_path).await?;
                stats
                    .memory_bytes
                    .fetch_add(file_size as u64, Ordering::Relaxed);

                let segment = CachedSegment {
                    data: SegmentData::Memory(Bytes::from(data)),
                    content_type,
                };

                Ok(segment)
            })
            .await;

        // We only get here if it was a cache miss (new load)
        match result {
            Ok(segment) => {
                self.stats.misses.fetch_add(1, Ordering::Relaxed);
                Ok(Some((segment, false))) // false = was not cached
            }
            Err(e) => {
                error!("Failed to load {}: {}", key, e);
                Ok(None)
            }
        }
    }

    pub async fn invalidate_stream(&self, stream_key: &str) {
        let prefix = format!("{}_", stream_key);
        let stream_key_owned = stream_key.to_string();

        // Use atomics for tracking since the closure must be Fn, not FnMut
        let invalidated_count = Arc::new(AtomicU64::new(0));
        let invalidated_memory = Arc::new(AtomicU64::new(0));

        // Clone for the closure
        let stats = Arc::clone(&self.stats);
        let count_clone = Arc::clone(&invalidated_count);
        let memory_clone = Arc::clone(&invalidated_memory);

        // Invalidate all cache entries that belong to this stream
        // This includes the main playlist and all segment files
        let _ = self.cache.invalidate_entries_if(move |key, segment| {
            // Check if this key belongs to the stream we're invalidating
            // Match either exact stream key (for m3u8) or prefix (for segments)
            if key == &stream_key_owned || key.starts_with(&prefix) {
                count_clone.fetch_add(1, Ordering::Relaxed);

                // Update memory stats based on what we're removing
                match &segment.data {
                    SegmentData::Memory(bytes) => {
                        let size = bytes.len() as u64;
                        memory_clone.fetch_add(size, Ordering::Relaxed);
                        stats.memory_bytes.fetch_sub(size, Ordering::Relaxed);
                    }
                }

                true // Invalidate this entry
            } else {
                false // Keep this entry
            }
        });

        let count = invalidated_count.load(Ordering::Relaxed);
        let memory = invalidated_memory.load(Ordering::Relaxed);

        if count > 0 {
            info!(
                "Invalidated {} cache entries for stream '{}' ({}B memory)",
                count, stream_key, memory
            );
        } else {
            debug!("No cache entries found for stream '{}'", stream_key);
        }
    }

    pub async fn stats(&self) -> CacheStatsSnapshot {
        CacheStatsSnapshot {
            total_entries: self.cache.entry_count() as usize,
            hits: self.stats.hits.load(Ordering::Relaxed),
            misses: self.stats.misses.load(Ordering::Relaxed),
            memory_bytes: self.stats.memory_bytes.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheStatsSnapshot {
    pub total_entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub memory_bytes: u64,
}

impl CacheStatsSnapshot {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}
