//! Wire-compatible protocol for semantic model daemon.
//!
//! This protocol is designed to be wire-compatible with xf's daemon implementation,
//! allowing both tools to share a daemon if both are installed.
//!
//! Protocol uses MessagePack for efficient binary serialization over Unix Domain Sockets.

use serde::{Deserialize, Serialize};

/// Protocol version for compatibility checks.
/// Both cass and xf must use the same version to share a daemon.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default socket path (shared between cass and xf).
pub fn default_socket_path() -> std::path::PathBuf {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    // Sanitize: keep only alphanumeric, dash, underscore to prevent path traversal
    let safe_user: String = user
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    let safe_user = if safe_user.is_empty() {
        "unknown".to_string()
    } else {
        safe_user
    };
    std::path::PathBuf::from(format!("/tmp/semantic-daemon-{}.sock", safe_user))
}

/// Request types for the daemon protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Health check - returns daemon status.
    Health,

    /// Generate embeddings for texts.
    Embed {
        texts: Vec<String>,
        model: String,
        dims: Option<usize>,
    },

    /// Rerank documents against a query.
    Rerank {
        query: String,
        documents: Vec<String>,
        model: String,
    },

    /// Get daemon status and loaded models.
    Status,

    /// Request graceful shutdown.
    Shutdown,
}

/// Response types from the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// Health check response.
    Health(HealthStatus),

    /// Embedding response with vectors.
    Embed(EmbedResponse),

    /// Rerank response with scores.
    Rerank(RerankResponse),

    /// Status response with daemon info.
    Status(StatusResponse),

    /// Shutdown acknowledgement.
    Shutdown { message: String },

    /// Error response.
    Error(ErrorResponse),
}

/// Health status of the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Daemon uptime in seconds.
    pub uptime_secs: u64,
    /// Protocol version.
    pub version: u32,
    /// Whether models are loaded and ready.
    pub ready: bool,
    /// Current memory usage in bytes (approximate).
    pub memory_bytes: u64,
}

/// Response containing embeddings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedResponse {
    /// Embeddings as Vec<Vec<f32>>.
    pub embeddings: Vec<Vec<f32>>,
    /// Model ID used.
    pub model: String,
    /// Processing time in milliseconds.
    pub elapsed_ms: u64,
}

/// Response containing rerank scores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankResponse {
    /// Scores for each document (same order as input).
    pub scores: Vec<f32>,
    /// Model ID used.
    pub model: String,
    /// Processing time in milliseconds.
    pub elapsed_ms: u64,
}

/// Daemon status response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    /// Daemon uptime in seconds.
    pub uptime_secs: u64,
    /// Protocol version.
    pub version: u32,
    /// Loaded embedder models.
    pub embedders: Vec<ModelInfo>,
    /// Loaded reranker models.
    pub rerankers: Vec<ModelInfo>,
    /// Current memory usage in bytes.
    pub memory_bytes: u64,
    /// Total requests served.
    pub total_requests: u64,
}

/// Information about a loaded model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Model ID.
    pub id: String,
    /// Model name/path.
    pub name: String,
    /// Output dimension (for embedders).
    pub dimension: Option<usize>,
    /// Whether the model is currently loaded.
    pub loaded: bool,
    /// Approximate memory usage in bytes.
    pub memory_bytes: u64,
}

/// Error response from daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Error code for programmatic handling.
    pub code: ErrorCode,
    /// Human-readable error message.
    pub message: String,
    /// Whether the request can be retried.
    pub retryable: bool,
    /// Suggested retry delay in milliseconds (if retryable).
    pub retry_after_ms: Option<u64>,
}

/// Error codes for daemon errors.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ErrorCode {
    /// Unknown or internal error.
    Internal,
    /// Model not found or not loaded.
    ModelNotFound,
    /// Invalid request parameters.
    InvalidInput,
    /// Daemon is overloaded, try again later.
    Overloaded,
    /// Request timed out.
    Timeout,
    /// Model loading failed.
    ModelLoadFailed,
    /// Protocol version mismatch.
    VersionMismatch,
}

/// Framed message wrapper for length-prefixed protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FramedMessage<T> {
    /// Protocol version.
    pub version: u32,
    /// Request ID for correlation.
    pub request_id: String,
    /// Payload.
    pub payload: T,
}

impl<T> FramedMessage<T> {
    pub fn new(request_id: impl Into<String>, payload: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            payload,
        }
    }
}

/// Encode a message to MessagePack bytes with length prefix.
pub fn encode_message<T: Serialize>(msg: &FramedMessage<T>) -> Result<Vec<u8>, EncodeError> {
    let payload = rmp_serde::to_vec(msg).map_err(|e| EncodeError(e.to_string()))?;
    let len = u32::try_from(payload.len())
        .map_err(|_| EncodeError("payload exceeds maximum size of 4GB".to_string()))?;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Decode a message from MessagePack bytes (without length prefix).
pub fn decode_message<T: for<'de> Deserialize<'de>>(
    data: &[u8],
) -> Result<FramedMessage<T>, DecodeError> {
    rmp_serde::from_slice(data).map_err(|e| DecodeError(e.to_string()))
}

#[derive(Debug, Clone)]
pub struct EncodeError(pub String);

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "encode error: {}", self.0)
    }
}

impl std::error::Error for EncodeError {}

#[derive(Debug, Clone)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_health_request() {
        let msg = FramedMessage::new("req-1", Request::Health);
        let encoded = encode_message(&msg).unwrap();

        // Skip 4-byte length prefix
        let decoded: FramedMessage<Request> = decode_message(&encoded[4..]).unwrap();
        assert_eq!(decoded.version, PROTOCOL_VERSION);
        assert_eq!(decoded.request_id, "req-1");
        assert!(matches!(decoded.payload, Request::Health));
    }

    #[test]
    fn test_encode_decode_embed_request() {
        let msg = FramedMessage::new(
            "req-2",
            Request::Embed {
                texts: vec!["hello".to_string(), "world".to_string()],
                model: "all-MiniLM-L6-v2".to_string(),
                dims: None,
            },
        );
        let encoded = encode_message(&msg).unwrap();
        let decoded: FramedMessage<Request> = decode_message(&encoded[4..]).unwrap();

        if let Request::Embed { texts, model, dims } = decoded.payload {
            assert_eq!(texts, vec!["hello", "world"]);
            assert_eq!(model, "all-MiniLM-L6-v2");
            assert!(dims.is_none());
        } else {
            panic!("expected Embed request");
        }
    }

    #[test]
    fn test_encode_decode_rerank_request() {
        let msg = FramedMessage::new(
            "req-3",
            Request::Rerank {
                query: "test query".to_string(),
                documents: vec!["doc1".to_string(), "doc2".to_string()],
                model: "ms-marco-MiniLM-L-6-v2".to_string(),
            },
        );
        let encoded = encode_message(&msg).unwrap();
        let decoded: FramedMessage<Request> = decode_message(&encoded[4..]).unwrap();

        if let Request::Rerank {
            query,
            documents,
            model,
        } = decoded.payload
        {
            assert_eq!(query, "test query");
            assert_eq!(documents, vec!["doc1", "doc2"]);
            assert_eq!(model, "ms-marco-MiniLM-L-6-v2");
        } else {
            panic!("expected Rerank request");
        }
    }

    #[test]
    fn test_encode_decode_health_response() {
        let msg = FramedMessage::new(
            "resp-1",
            Response::Health(HealthStatus {
                uptime_secs: 120,
                version: PROTOCOL_VERSION,
                ready: true,
                memory_bytes: 100_000_000,
            }),
        );
        let encoded = encode_message(&msg).unwrap();
        let decoded: FramedMessage<Response> = decode_message(&encoded[4..]).unwrap();

        if let Response::Health(status) = decoded.payload {
            assert_eq!(status.uptime_secs, 120);
            assert!(status.ready);
        } else {
            panic!("expected Health response");
        }
    }

    #[test]
    fn test_encode_decode_error_response() {
        let msg = FramedMessage::new(
            "resp-err",
            Response::Error(ErrorResponse {
                code: ErrorCode::Overloaded,
                message: "too many requests".to_string(),
                retryable: true,
                retry_after_ms: Some(1000),
            }),
        );
        let encoded = encode_message(&msg).unwrap();
        let decoded: FramedMessage<Response> = decode_message(&encoded[4..]).unwrap();

        if let Response::Error(err) = decoded.payload {
            assert_eq!(err.code, ErrorCode::Overloaded);
            assert!(err.retryable);
            assert_eq!(err.retry_after_ms, Some(1000));
        } else {
            panic!("expected Error response");
        }
    }

    #[test]
    fn test_default_socket_path() {
        let path = default_socket_path();
        let path_str = path.to_string_lossy();
        assert!(path_str.starts_with("/tmp/semantic-daemon-"));
        assert!(path_str.ends_with(".sock"));
    }

    #[test]
    fn test_wire_compatibility_embed_response() {
        // Test that embed response can be serialized and deserialized
        let msg = FramedMessage::new(
            "resp-embed",
            Response::Embed(EmbedResponse {
                embeddings: vec![vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]],
                model: "minilm-384".to_string(),
                elapsed_ms: 15,
            }),
        );
        let encoded = encode_message(&msg).unwrap();
        let decoded: FramedMessage<Response> = decode_message(&encoded[4..]).unwrap();

        if let Response::Embed(resp) = decoded.payload {
            assert_eq!(resp.embeddings.len(), 2);
            assert_eq!(resp.embeddings[0], vec![0.1, 0.2, 0.3]);
            assert_eq!(resp.model, "minilm-384");
        } else {
            panic!("expected Embed response");
        }
    }

    #[test]
    fn test_wire_compatibility_rerank_response() {
        let msg = FramedMessage::new(
            "resp-rerank",
            Response::Rerank(RerankResponse {
                scores: vec![0.95, 0.72, 0.31],
                model: "ms-marco".to_string(),
                elapsed_ms: 8,
            }),
        );
        let encoded = encode_message(&msg).unwrap();
        let decoded: FramedMessage<Response> = decode_message(&encoded[4..]).unwrap();

        if let Response::Rerank(resp) = decoded.payload {
            assert_eq!(resp.scores, vec![0.95, 0.72, 0.31]);
        } else {
            panic!("expected Rerank response");
        }
    }
}
