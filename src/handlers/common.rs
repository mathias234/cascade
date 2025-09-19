use crate::manager::StreamManager;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, atomic::Ordering};

#[derive(Debug, Deserialize)]
pub struct ContextQuery {
    pub hls_ctx: Option<String>,
}

/// Represents different types of HLS requests
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum HlsRequestType {
    /// Initial request without session - needs redirect for viewer tracking
    InitialRequest {
        stream_key: String,
    },

    /// Playlist request (master.m3u8, index.m3u8, or variant/index.m3u8)
    Playlist {
        stream_key: String,
        playlist_path: PathBuf,
        session: Option<String>,
        needs_url_rewrite: bool,
    },

    /// Segment request (.ts files)
    Segment {
        stream_key: String,
        segment_path: PathBuf,
    },
}

/// Parse the incoming path into a structured HLS request type
pub fn parse_hls_request(path: &str, session: Option<String>) -> Result<HlsRequestType, &'static str> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if parts.is_empty() {
        return Err("Invalid path");
    }

    let stream_key = parts[0];

    // Handle different path patterns
    match parts.len() {
        1 => {
            // Just stream_key - shouldn't happen with our routing
            Err("Invalid path format")
        }
        2 => {
            let filename = parts[1];

            if filename.ends_with(".m3u8") {
                // Could be master.m3u8 or index.m3u8
                if filename == "index.m3u8" && session.is_none() {
                    // Initial request without session - need redirect
                    Ok(HlsRequestType::InitialRequest {
                        stream_key: stream_key.to_string(),
                    })
                } else if filename == "master.m3u8" {
                    // ABR master playlist
                    Ok(HlsRequestType::Playlist {
                        stream_key: stream_key.to_string(),
                        playlist_path: PathBuf::from(filename),
                        session,
                        needs_url_rewrite: false, // ABR master doesn't need URL rewriting
                    })
                } else if filename == "index.m3u8" {
                    // Single bitrate playlist with session
                    Ok(HlsRequestType::Playlist {
                        stream_key: stream_key.to_string(),
                        playlist_path: PathBuf::from(filename),
                        session,
                        needs_url_rewrite: true, // Single bitrate needs URL rewriting
                    })
                } else {
                    Err("Unknown playlist file")
                }
            } else if filename.ends_with(".ts") {
                // Single bitrate segment
                Ok(HlsRequestType::Segment {
                    stream_key: stream_key.to_string(),
                    segment_path: PathBuf::from(filename),
                })
            } else {
                Err("Invalid file type")
            }
        }
        3 => {
            let variant = parts[1];
            let filename = parts[2];

            if filename == "index.m3u8" {
                // Variant playlist
                Ok(HlsRequestType::Playlist {
                    stream_key: stream_key.to_string(),
                    playlist_path: PathBuf::from(format!("{}/{}", variant, filename)),
                    session,
                    needs_url_rewrite: false, // Variant playlists served as-is
                })
            } else if filename.ends_with(".ts") {
                // Variant segment
                Ok(HlsRequestType::Segment {
                    stream_key: stream_key.to_string(),
                    segment_path: PathBuf::from(format!("{}/{}", variant, filename)),
                })
            } else {
                Err("Invalid variant file")
            }
        }
        _ => Err("Invalid path depth")
    }
}

/// Ensure a stream is ready for serving
pub async fn ensure_stream_ready(stream_key: &str, manager: &Arc<StreamManager>) -> bool {
    // Wait for stream to be ready, starting it if necessary
    if !manager.wait_for_stream(stream_key.to_string()).await {
        // Stream couldn't start for other reasons
        return false;
    }
    true
}

/// Rewrite playlist URLs to include the stream key prefix
pub fn rewrite_playlist_urls(content: &str, stream_key: &str) -> String {
    content
        .lines()
        .map(|line| {
            if line.ends_with(".ts") {
                // This is a segment filename, prepend the path
                format!("/live/{}/{}", stream_key, line)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Track request statistics
pub fn track_request_stats(manager: &Arc<StreamManager>, bytes_served: usize) {
    manager.stats.requests.fetch_add(1, Ordering::Relaxed);
    manager.stats.bytes_served.fetch_add(bytes_served as u64, Ordering::Relaxed);
}
