use crate::config::ElasticsearchConfig;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use elasticsearch::{
    http::transport::Transport,
    BulkOperation, BulkParts, Elasticsearch,
    indices::IndicesPutIndexTemplateParts,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsDocument {
    #[serde(rename = "@timestamp")]
    pub timestamp: DateTime<Utc>,
    pub server_name: String,
    pub bytes_per_second: f64,
    pub requests_per_second: f64,
    pub segments_per_second: f64,
    pub viewers: usize,
    pub active_streams: usize,
    pub cache_hit_rate: f64,
    pub cache_memory_mb: f64,
    pub cache_entries: usize,
    pub mbps: f64,
    pub stream_viewers: HashMap<String, usize>,
}

#[derive(Debug)]
struct BufferedMetrics {
    buffer: Vec<MetricsDocument>,
    last_flush: DateTime<Utc>,
}

pub struct ElasticsearchClient {
    client: Option<Elasticsearch>,
    index_prefix: String,
    batch_size: usize,
    flush_interval_seconds: u64,
    buffer: Arc<RwLock<BufferedMetrics>>,
    server_name: String,
}

impl ElasticsearchClient {
    pub fn new(config: &ElasticsearchConfig) -> Result<Self> {
        let client = if config.enabled {
            let url = config.url.as_deref().unwrap_or("http://localhost:9200");
            let transport = Transport::single_node(url)
                .context("Failed to create Elasticsearch transport")?;
            let client = Elasticsearch::new(transport);
            debug!("Elasticsearch client initialized with URL: {}", url);
            Some(client)
        } else {
            debug!("Elasticsearch indexing disabled");
            None
        };

        // Use provided server_name or generate a default
        let server_name = config.server_name.clone().unwrap_or_else(|| {
            format!("cascade-{}", Utc::now().timestamp())
        });

        if config.enabled {
            debug!("Elasticsearch client initialized with server_name: {}", server_name);
        }

        Ok(Self {
            client,
            index_prefix: config.index_prefix.clone(),
            batch_size: config.batch_size,
            flush_interval_seconds: config.flush_interval_seconds,
            buffer: Arc::new(RwLock::new(BufferedMetrics {
                buffer: Vec::with_capacity(config.batch_size),
                last_flush: Utc::now(),
            })),
            server_name,
        })
    }

    pub async fn index_metrics(&self, mut metrics: MetricsDocument) {
        // Ensure the metrics document has the correct server_name
        metrics.server_name = self.server_name.clone();
        if self.client.is_none() {
            return;
        }

        let mut buffer = self.buffer.write().await;
        buffer.buffer.push(metrics);

        let should_flush = buffer.buffer.len() >= self.batch_size
            || Utc::now()
                .signed_duration_since(buffer.last_flush)
                .num_seconds()
                >= self.flush_interval_seconds as i64;

        if should_flush {
            let metrics_to_flush: Vec<MetricsDocument> = buffer.buffer.drain(..).collect();
            buffer.last_flush = Utc::now();
            drop(buffer); // Release lock before async flush

            if !metrics_to_flush.is_empty() {
                let client = self.client.clone();
                let index_prefix = self.index_prefix.clone();

                tokio::spawn(async move {
                    if let Some(client) = client {
                        if let Err(e) =
                            Self::flush_batch(&client, &metrics_to_flush, &index_prefix).await
                        {
                            error!("Failed to flush metrics to Elasticsearch: {}", e);
                        }
                    }
                });
            }
        }
    }

    async fn flush_batch(
        client: &Elasticsearch,
        metrics: &[MetricsDocument],
        index_prefix: &str,
    ) -> Result<()> {
        if metrics.is_empty() {
            return Ok(());
        }

        let date = Utc::now().format("%Y.%m.%d");
        let index_name = format!("{}-{}", index_prefix, date);

        // Use bulk API for better performance
        let mut body: Vec<BulkOperation<_>> = Vec::new();

        for metric in metrics {
            body.push(BulkOperation::index(metric).into());
        }

        let response = client
            .bulk(BulkParts::Index(&index_name))
            .body(body)
            .send()
            .await?;

        let response_body = response.json::<serde_json::Value>().await?;

        if response_body["errors"].as_bool().unwrap_or(false) {
            warn!("Some metrics failed to index: {:?}", response_body);
        } else {
            debug!("Successfully indexed {} metrics", metrics.len());
        }

        Ok(())
    }

    pub async fn flush(&self) {
        if self.client.is_none() {
            return;
        }

        let mut buffer = self.buffer.write().await;
        let metrics_to_flush: Vec<MetricsDocument> = buffer.buffer.drain(..).collect();
        buffer.last_flush = Utc::now();
        drop(buffer);

        if !metrics_to_flush.is_empty() {
            if let Some(client) = &self.client {
                if let Err(e) =
                    Self::flush_batch(client, &metrics_to_flush, &self.index_prefix).await
                {
                    error!("Failed to flush metrics on shutdown: {}", e);
                }
            }
        }
    }

    pub async fn create_index_template(&self) -> Result<()> {
        if let Some(client) = &self.client {
            let template_name = format!("{}-template", self.index_prefix);
            let index_pattern = format!("{}*", self.index_prefix);

            let template_body = json!({
                "index_patterns": [index_pattern],
                "template": {
                    "settings": {
                        "number_of_shards": 1,
                        "number_of_replicas": 0,
                        "refresh_interval": "5s"
                    },
                    "mappings": {
                        "properties": {
                            "@timestamp": {
                                "type": "date"
                            },
                            "server_name": {
                                "type": "keyword"
                            },
                            "bytes_per_second": {
                                "type": "double"
                            },
                            "requests_per_second": {
                                "type": "double"
                            },
                            "segments_per_second": {
                                "type": "double"
                            },
                            "viewers": {
                                "type": "long"
                            },
                            "active_streams": {
                                "type": "long"
                            },
                            "cache_hit_rate": {
                                "type": "double"
                            },
                            "cache_memory_mb": {
                                "type": "double"
                            },
                            "cache_entries": {
                                "type": "long"
                            },
                            "mbps": {
                                "type": "double"
                            },
                            "stream_viewers": {
                                "type": "object",
                                "enabled": true
                            }
                        }
                    }
                }
            });

            client
                .indices()
                .put_index_template(IndicesPutIndexTemplateParts::Name(&template_name))
                .body(template_body)
                .send()
                .await
                .context("Failed to create index template")?;

            debug!("Created Elasticsearch index template: {}", template_name);
        }

        Ok(())
    }
}