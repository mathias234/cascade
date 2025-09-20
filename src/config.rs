use config::{Config as ConfigBuilder, ConfigError, File};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub rtmp: RtmpConfig,
    pub stream: StreamConfig,
    pub ffmpeg: FfmpegConfig,
    pub abr: AbrConfig,
    pub cache: CacheConfig,
    pub viewer: ViewerConfig,
    pub ssl: SslConfig,
    pub logging: LoggingConfig,
    pub elasticsearch: ElasticsearchConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ServerConfig {
    pub port: u16,
    pub hls_path: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RtmpConfig {
    pub source_host: String,
    pub source_port: u16,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct StreamConfig {
    pub max_concurrent_streams: usize,
    pub stream_timeout: u64,
    pub stream_start_timeout: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FfmpegConfig {
    pub hls_time: u64,
    pub hls_list_size: u64,
    pub rw_timeout: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AbrConfig {
    pub enabled: bool,
    pub variants: Vec<StreamVariant>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct StreamVariant {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub bitrate: u32,
    pub audio_bitrate: u32,
    pub framerate: u32,
    pub profile: String,
    pub preset: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CacheConfig {
    pub max_entries: usize,
    pub max_segment_size: usize,
    pub ttl_seconds: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ViewerConfig {
    pub tracking_enabled: bool,
    pub timeout_seconds: u64,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SslConfig {
    pub enabled: bool,
    pub domains: Vec<String>,
    pub email: String,
    pub staging: bool,
    pub cert_cache_dir: String,
    pub port: u16,
    pub http_redirect: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LoggingConfig {
    pub level: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ElasticsearchConfig {
    pub enabled: bool,
    pub url: Option<String>,
    pub index_prefix: String,
    pub batch_size: usize,
    pub flush_interval_seconds: u64,
    pub server_name: Option<String>,
    pub username: String,
    pub password: String,
}

impl Config {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let config = ConfigBuilder::builder()
            .add_source(File::from(path.as_ref()))
            .build()?;

        config.try_deserialize()
    }

    pub fn default() -> Self {
        Self {
            server: ServerConfig {
                port: 8080,
                hls_path: "/hls".to_string(),
            },
            rtmp: RtmpConfig {
                source_host: "rtmp.example.com".to_string(),
                source_port: 1935,
            },
            stream: StreamConfig {
                max_concurrent_streams: 50,
                stream_timeout: 30,
                stream_start_timeout: 15,
            },
            ffmpeg: FfmpegConfig {
                hls_time: 1,
                hls_list_size: 20,
                rw_timeout: 100000,
            },
            abr: AbrConfig {
                enabled: true,
                variants: vec![
                    StreamVariant {
                        name: "360p".to_string(),
                        width: 640,
                        height: 360,
                        bitrate: 800,
                        audio_bitrate: 96,
                        framerate: 30,
                        profile: "main".to_string(),
                        preset: "veryfast".to_string(),
                    },
                    StreamVariant {
                        name: "480p".to_string(),
                        width: 854,
                        height: 480,
                        bitrate: 1400,
                        audio_bitrate: 128,
                        framerate: 30,
                        profile: "main".to_string(),
                        preset: "veryfast".to_string(),
                    },
                    StreamVariant {
                        name: "720p".to_string(),
                        width: 1280,
                        height: 720,
                        bitrate: 2800,
                        audio_bitrate: 128,
                        framerate: 30,
                        profile: "main".to_string(),
                        preset: "veryfast".to_string(),
                    },
                    StreamVariant {
                        name: "1080p".to_string(),
                        width: 1920,
                        height: 1080,
                        bitrate: 5000,
                        audio_bitrate: 192,
                        framerate: 30,
                        profile: "high".to_string(),
                        preset: "veryfast".to_string(),
                    },
                ],
            },
            cache: CacheConfig {
                max_entries: 200,
                max_segment_size: 10485760,
                ttl_seconds: 300,
            },
            viewer: ViewerConfig {
                tracking_enabled: true,
                timeout_seconds: 30,
            },
            ssl: SslConfig {
                enabled: false,
                domains: vec!["example.com".to_string(), "www.example.com".to_string()],
                email: "admin@example.com".to_string(),
                staging: true,
                cert_cache_dir: "/etc/cascade/certs".to_string(),
                port: 443,
                http_redirect: true,
            },
            logging: LoggingConfig {
                level: "info".to_string(),
            },
            elasticsearch: ElasticsearchConfig {
                enabled: false,
                url: Some("http://localhost:9200".to_string()),
                index_prefix: "cascade-metrics".to_string(),
                batch_size: 10,
                flush_interval_seconds: 10,
                server_name: None,
                username: "elastic".to_string(),
                password: "admin".to_string(),
            },
        }
    }
}
