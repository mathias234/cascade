pub mod api;
pub mod common;
pub mod playlist;
pub mod segment;

use crate::manager::StreamManager;
use axum::{
    body::Body,
    extract::{Path, Query, Request},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use common::{ContextQuery, HlsRequestType, ensure_stream_ready, parse_hls_request};
use std::sync::Arc;
use tracing::{debug, error};

// Re-export commonly used items
pub use api::{dashboard, health_check, status};

/// Main HLS content handler that routes based on path pattern
pub async fn serve_hls_content(
    Path(path): Path<String>,
    Query(query): Query<ContextQuery>,
    _req: Request,
    manager: Arc<StreamManager>,
) -> impl IntoResponse {
    debug!(
        "HLS request for path: {} with context: {:?}",
        path, query.hls_ctx
    );

    // Parse the request into a structured type
    let request = match parse_hls_request(&path, query.hls_ctx.clone()) {
        Ok(req) => req,
        Err(err) => {
            error!("Failed to parse HLS request for path '{}': {}", path, err);
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(err))
                .unwrap();
        }
    };

    // Route based on request type
    match request {
        HlsRequestType::InitialRequest { stream_key } => {
            // Create session and redirect to appropriate playlist
            playlist::handle_initial_request_with_session(&stream_key, manager).await
        }

        HlsRequestType::Playlist {
            stream_key,
            playlist_path,
            session,
            needs_url_rewrite
        } => {
            // Ensure stream is ready
            if !ensure_stream_ready(&stream_key, &manager).await {
                return Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .header("Retry-After", "5")
                    .body(Body::from("Stream not available"))
                    .unwrap();
            }

            // Track session if provided
            if let Some(sid) = &session {
                manager.session_manager.update_session(sid);
            }

            // Serve the playlist
            playlist::serve_playlist(&stream_key, playlist_path, needs_url_rewrite, manager).await
        }

        HlsRequestType::Segment { stream_key, segment_path } => {
            // Serve the segment
            segment::serve_segment_file(&stream_key, segment_path, manager).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::common::*;
    use std::path::PathBuf;

    #[test]
    fn test_parse_initial_request() {
        // Initial request without session should trigger redirect
        let result = parse_hls_request("stream123/index.m3u8", None);
        assert!(result.is_ok());

        match result.unwrap() {
            HlsRequestType::InitialRequest { stream_key } => {
                assert_eq!(stream_key, "stream123");
            }
            _ => panic!("Expected InitialRequest"),
        }
    }

    #[test]
    fn test_parse_playlist_with_session() {
        // Request with session should serve actual playlist
        let result = parse_hls_request("stream123/index.m3u8", Some("session456".to_string()));
        assert!(result.is_ok());

        match result.unwrap() {
            HlsRequestType::Playlist { stream_key, playlist_path, session, needs_url_rewrite } => {
                assert_eq!(stream_key, "stream123");
                assert_eq!(playlist_path, PathBuf::from("index.m3u8"));
                assert_eq!(session, Some("session456".to_string()));
                assert!(needs_url_rewrite); // Single bitrate needs rewriting
            }
            _ => panic!("Expected Playlist"),
        }
    }

    #[test]
    fn test_parse_abr_master_playlist() {
        let result = parse_hls_request("stream123/master.m3u8", Some("session789".to_string()));
        assert!(result.is_ok());

        match result.unwrap() {
            HlsRequestType::Playlist { stream_key, playlist_path, needs_url_rewrite, .. } => {
                assert_eq!(stream_key, "stream123");
                assert_eq!(playlist_path, PathBuf::from("master.m3u8"));
                assert!(!needs_url_rewrite); // ABR master doesn't need rewriting
            }
            _ => panic!("Expected Playlist for master.m3u8"),
        }
    }

    #[test]
    fn test_parse_variant_playlist() {
        let result = parse_hls_request("stream123/720p/index.m3u8", None);
        assert!(result.is_ok());

        match result.unwrap() {
            HlsRequestType::Playlist { stream_key, playlist_path, needs_url_rewrite, .. } => {
                assert_eq!(stream_key, "stream123");
                assert_eq!(playlist_path, PathBuf::from("720p/index.m3u8"));
                assert!(!needs_url_rewrite); // Variant playlists served as-is
            }
            _ => panic!("Expected Playlist for variant"),
        }
    }

    #[test]
    fn test_parse_segments() {
        // Single bitrate segment
        let result = parse_hls_request("stream123/segment001.ts", None);
        assert!(result.is_ok());

        match result.unwrap() {
            HlsRequestType::Segment { stream_key, segment_path } => {
                assert_eq!(stream_key, "stream123");
                assert_eq!(segment_path, PathBuf::from("segment001.ts"));
            }
            _ => panic!("Expected Segment"),
        }

        // Variant segment
        let result = parse_hls_request("stream123/1080p/segment042.ts", None);
        assert!(result.is_ok());

        match result.unwrap() {
            HlsRequestType::Segment { stream_key, segment_path } => {
                assert_eq!(stream_key, "stream123");
                assert_eq!(segment_path, PathBuf::from("1080p/segment042.ts"));
            }
            _ => panic!("Expected Segment for variant"),
        }
    }

    #[test]
    fn test_invalid_paths() {
        assert!(parse_hls_request("", None).is_err());
        assert!(parse_hls_request("just_stream", None).is_err());
        assert!(parse_hls_request("stream/unknown.xyz", None).is_err());
        assert!(parse_hls_request("a/b/c/d/too/deep.m3u8", None).is_err());
    }

    #[test]
    fn test_rewrite_playlist_urls() {
        let content = r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:10
#EXTINF:10.0,
segment000.ts
#EXTINF:10.0,
segment001.ts
#EXTINF:10.0,
segment002.ts
#EXT-X-ENDLIST"#;

        let rewritten = rewrite_playlist_urls(content, "mystream");

        assert!(rewritten.contains("/live/mystream/segment000.ts"));
        assert!(rewritten.contains("/live/mystream/segment001.ts"));
        assert!(rewritten.contains("/live/mystream/segment002.ts"));

        // Headers should remain unchanged
        assert!(rewritten.contains("#EXTM3U"));
        assert!(rewritten.contains("#EXT-X-VERSION:3"));
        assert!(rewritten.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn test_rewrite_preserves_non_ts_lines() {
        let content = r#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000000
720p/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2000000
1080p/index.m3u8"#;

        let rewritten = rewrite_playlist_urls(content, "stream");

        // m3u8 files should NOT be rewritten (only .ts files)
        assert!(rewritten.contains("720p/index.m3u8"));
        assert!(rewritten.contains("1080p/index.m3u8"));
        assert!(!rewritten.contains("/live/stream/720p"));
    }
}
