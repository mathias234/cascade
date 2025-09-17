use anyhow::Result;
use bytes::Bytes;
use moka::future::{Cache, FutureExt};
use moka::notification::{ListenerFuture, RemovalCause};
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
pub struct CachedSegment {
    pub data: Bytes,
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

        let stats = Arc::new(CacheStats::new());
        let stats_clone = Arc::clone(&stats);

        let eviction_listener =
            move |key: Arc<String>, value: CachedSegment, cause: RemovalCause| -> ListenerFuture {
                debug!(
                    "Cache entry evicted: {} (cause: {:?}, size: {} bytes)",
                    key,
                    cause,
                    value.data.len()
                );

                let size = value.data.len() as u64;
                stats_clone.memory_bytes.fetch_sub(size, Ordering::Relaxed);

                async {}.boxed()
            };

        let cache = Cache::builder()
            .max_capacity(max_entries as u64)
            .time_to_live(Duration::from_secs(300))
            .async_eviction_listener(eviction_listener)
            .build();

        Self {
            cache,
            max_file_size: 100 * 1024 * 1024,
            stats,
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

        let file_path = file_path.to_path_buf();
        let max_file_size = self.max_file_size;
        let stats = Arc::clone(&self.stats);
        let key_string = key.to_string();

        let result = self
            .cache
            .try_get_with(key_string.clone(), async move {
                let metadata = match tokio::fs::metadata(&file_path).await {
                    Ok(m) => m,
                    Err(e) => {
                        debug!("File not found: {:?} - {}", file_path, e);
                        return Err(anyhow::anyhow!("File not found"));
                    }
                };
                let file_size = metadata.len() as usize;

                if file_size > max_file_size {
                    return Err(anyhow::anyhow!(
                        "File exceeds maximum size limit: {} > {}",
                        file_size,
                        max_file_size
                    ));
                }

                let content_type = match file_path.extension().and_then(|s| s.to_str()) {
                    Some("ts") => "video/MP2T",
                    _ => "application/octet-stream",
                };

                let data = tokio::fs::read(&file_path).await?;
                let data = Bytes::from(data);
                stats
                    .memory_bytes
                    .fetch_add(file_size as u64, Ordering::Relaxed);

                let segment = CachedSegment { data, content_type };

                Ok(segment)
            })
            .await;

        match result {
            Ok(segment) => {
                self.stats.misses.fetch_add(1, Ordering::Relaxed);
                Ok(Some((segment, false)))
            }
            Err(e) => {
                error!("Failed to load {}: {}", key, e);
                Ok(None)
            }
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheStatsSnapshot {
    pub total_entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub memory_bytes: u64,
}
