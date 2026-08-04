//! Wire-compatible protocol for semantic model daemon.
//!
//! This protocol is designed to be wire-compatible with xf's daemon implementation,
//! allowing both tools to share a daemon if both are installed.
//!
//! Protocol uses MessagePack for efficient binary serialization over Unix Domain Sockets.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Protocol version for compatibility checks.
/// Both cass and xf must use the same version to share a daemon.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default socket path (shared between cass and xf).
pub fn default_socket_path() -> PathBuf {
    let user = dotenvy::var("USER").unwrap_or_else(|_| "unknown".into());
    let safe_user = sanitize_socket_user(&user);
    std::env::temp_dir().join(format!("semantic-daemon-{safe_user}.sock"))
}

fn sanitize_socket_user(user: &str) -> String {
    let safe_user: String = user
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();

    if safe_user.is_empty() {
        "unknown".to_string()
    } else {
        safe_user
    }
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

    /// Submit a background embedding job.
    SubmitEmbeddingJob {
        db_path: String,
        index_path: String,
        two_tier: bool,
        fast_model: Option<String>,
        quality_model: Option<String>,
    },

    /// Query embedding job status.
    EmbeddingJobStatus { db_path: String },

    /// Cancel embedding jobs.
    CancelEmbeddingJob {
        db_path: String,
        model_id: Option<String>,
    },

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

    /// Embedding job submitted.
    JobSubmitted { job_id: String, message: String },

    /// Embedding job status.
    JobStatus(EmbeddingJobInfo),

    /// Embedding jobs cancelled.
    JobCancelled { cancelled: usize, message: String },

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

/// Status information for embedding jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingJobInfo {
    pub jobs: Vec<EmbeddingJobDetail>,
}

/// Detail for a single embedding job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingJobDetail {
    pub job_id: i64,
    pub model_id: String,
    pub status: String,
    pub total_docs: i64,
    pub completed_docs: i64,
    pub error_message: Option<String>,
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
    let payload = rmp_serde::to_vec(msg)?;
    let len = u32::try_from(payload.len())
        .map_err(|_| EncodeError::Message("payload exceeds maximum size of 4GB".to_string()))?;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Decode a message from MessagePack bytes (without length prefix).
pub fn decode_message<T: for<'de> Deserialize<'de>>(
    data: &[u8],
) -> Result<FramedMessage<T>, DecodeError> {
    rmp_serde::from_slice(data).map_err(DecodeError::from)
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("encode error: {0}")]
    Message(String),
    #[error("encode error: {0}")]
    MessagePack(#[from] rmp_serde::encode::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("decode error: {0}")]
    Message(String),
    #[error("decode error: {0}")]
    MessagePack(#[from] rmp_serde::decode::Error),
}

#[cfg(test)]
mod tests {
    use super::{
        DecodeError, EmbedResponse, EncodeError, ErrorCode, ErrorResponse, FramedMessage,
        HealthStatus, PROTOCOL_VERSION, Request, RerankResponse, Response, decode_message,
        default_socket_path, encode_message, sanitize_socket_user,
    };
    use serde::de::DeserializeOwned;
    use std::error::Error;
    use std::fmt::Debug;

    type TestResult = Result<(), Box<dyn Error>>;

    fn test_error(message: impl Into<String>) -> Box<dyn Error> {
        std::io::Error::other(message.into()).into()
    }

    fn ensure(condition: bool, message: impl Into<String>) -> TestResult {
        if condition {
            Ok(())
        } else {
            Err(test_error(message))
        }
    }

    fn ensure_eq<T>(actual: T, expected: T, message: impl Into<String>) -> TestResult
    where
        T: Debug + PartialEq,
    {
        if actual == expected {
            Ok(())
        } else {
            Err(test_error(format!(
                "{}: expected {expected:?}, got {actual:?}",
                message.into()
            )))
        }
    }

    fn decode_framed<T>(encoded: &[u8]) -> Result<FramedMessage<T>, Box<dyn Error>>
    where
        T: DeserializeOwned,
    {
        let payload = encoded
            .get(4..)
            .ok_or_else(|| test_error("encoded frame should include a 4-byte length prefix"))?;
        decode_message(payload).map_err(|err| test_error(err.to_string()))
    }

    #[test]
    fn test_encode_decode_health_request() -> TestResult {
        let msg = FramedMessage::new("req-1", Request::Health);
        let encoded = encode_message(&msg)?;

        let decoded: FramedMessage<Request> = decode_framed(&encoded)?;
        ensure_eq(decoded.version, PROTOCOL_VERSION, "protocol version")?;
        ensure_eq(decoded.request_id, "req-1".to_string(), "request id")?;
        ensure(matches!(decoded.payload, Request::Health), "health payload")
    }

    #[test]
    fn test_protocol_error_display_strings_are_preserved() -> TestResult {
        let encode = EncodeError::Message("bad payload".to_string());
        ensure_eq(
            encode.to_string(),
            "encode error: bad payload".to_string(),
            "encode",
        )?;
        ensure(encode.source().is_none(), "encode")?;

        let decode = DecodeError::Message("bad frame".to_string());
        ensure_eq(
            decode.to_string(),
            "decode error: bad frame".to_string(),
            "decode",
        )?;
        ensure(decode.source().is_none(), "decode")?;
        Ok(())
    }

    #[test]
    fn test_encode_decode_embed_request() -> TestResult {
        let msg = FramedMessage::new(
            "req-2",
            Request::Embed {
                texts: vec!["hello".to_string(), "world".to_string()],
                model: "all-MiniLM-L6-v2".to_string(),
                dims: None,
            },
        );
        let encoded = encode_message(&msg)?;
        let decoded: FramedMessage<Request> = decode_framed(&encoded)?;

        let Request::Embed { texts, model, dims } = decoded.payload else {
            return Err(test_error("expected embed request payload"));
        };
        ensure_eq(
            texts,
            vec!["hello".to_string(), "world".to_string()],
            "embed texts",
        )?;
        ensure_eq(model, "all-MiniLM-L6-v2".to_string(), "embed model")?;
        ensure(dims.is_none(), "embed dims should be absent")
    }

    #[test]
    fn test_encode_decode_rerank_request() -> TestResult {
        let msg = FramedMessage::new(
            "req-3",
            Request::Rerank {
                query: "test query".to_string(),
                documents: vec!["doc1".to_string(), "doc2".to_string()],
                model: "ms-marco-MiniLM-L-6-v2".to_string(),
            },
        );
        let encoded = encode_message(&msg)?;
        let decoded: FramedMessage<Request> = decode_framed(&encoded)?;

        let Request::Rerank {
            query,
            documents,
            model,
        } = decoded.payload
        else {
            return Err(test_error("expected rerank request payload"));
        };
        ensure_eq(query, "test query".to_string(), "rerank query")?;
        ensure_eq(
            documents,
            vec!["doc1".to_string(), "doc2".to_string()],
            "rerank documents",
        )?;
        ensure_eq(model, "ms-marco-MiniLM-L-6-v2".to_string(), "rerank model")
    }

    #[test]
    fn test_encode_decode_health_response() -> TestResult {
        let msg = FramedMessage::new(
            "resp-1",
            Response::Health(HealthStatus {
                uptime_secs: 120,
                version: PROTOCOL_VERSION,
                ready: true,
                memory_bytes: 100_000_000,
            }),
        );
        let encoded = encode_message(&msg)?;
        let decoded: FramedMessage<Response> = decode_framed(&encoded)?;

        let Response::Health(status) = decoded.payload else {
            return Err(test_error("expected health response payload"));
        };
        ensure_eq(status.uptime_secs, 120, "health uptime")?;
        ensure(status.ready, "health response should be ready")
    }

    #[test]
    fn test_encode_decode_error_response() -> TestResult {
        let msg = FramedMessage::new(
            "resp-err",
            Response::Error(ErrorResponse {
                code: ErrorCode::Overloaded,
                message: "too many requests".to_string(),
                retryable: true,
                retry_after_ms: Some(1000),
            }),
        );
        let encoded = encode_message(&msg)?;
        let decoded: FramedMessage<Response> = decode_framed(&encoded)?;

        let Response::Error(err) = decoded.payload else {
            return Err(test_error("expected error response payload"));
        };
        ensure_eq(err.code, ErrorCode::Overloaded, "error code")?;
        ensure(err.retryable, "error should be retryable")?;
        ensure_eq(err.retry_after_ms, Some(1000), "retry delay")
    }

    #[test]
    fn test_default_socket_path() -> TestResult {
        let path = default_socket_path();
        ensure_eq(
            path.parent().map(std::path::Path::to_path_buf),
            Some(std::env::temp_dir()),
            "socket parent should honor the platform temp directory",
        )?;
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        ensure(
            file_name.starts_with("semantic-daemon-"),
            "socket file prefix",
        )?;
        ensure(file_name.ends_with(".sock"), "socket path suffix")
    }

    #[test]
    fn test_socket_user_sanitization() -> TestResult {
        ensure_eq(
            sanitize_socket_user("../bad user!"),
            "baduser".to_string(),
            "path traversal and punctuation should be removed",
        )?;
        ensure_eq(
            sanitize_socket_user(""),
            "unknown".to_string(),
            "empty user fallback",
        )?;
        ensure_eq(
            sanitize_socket_user("a".repeat(80).as_str()).len(),
            64,
            "socket user length cap",
        )
    }

    #[test]
    fn test_wire_compatibility_embed_response() -> TestResult {
        let msg = FramedMessage::new(
            "resp-embed",
            Response::Embed(EmbedResponse {
                embeddings: vec![vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]],
                model: "minilm-384".to_string(),
                elapsed_ms: 15,
            }),
        );
        let encoded = encode_message(&msg)?;
        let decoded: FramedMessage<Response> = decode_framed(&encoded)?;

        let Response::Embed(resp) = decoded.payload else {
            return Err(test_error("expected embed response payload"));
        };
        ensure_eq(resp.embeddings.len(), 2, "embedding count")?;
        let first = resp
            .embeddings
            .first()
            .ok_or_else(|| test_error("first embedding should exist"))?;
        ensure_eq(first.clone(), vec![0.1, 0.2, 0.3], "first embedding")?;
        ensure_eq(resp.model, "minilm-384".to_string(), "embedding model")
    }

    #[test]
    fn test_wire_compatibility_rerank_response() -> TestResult {
        let msg = FramedMessage::new(
            "resp-rerank",
            Response::Rerank(RerankResponse {
                scores: vec![0.95, 0.72, 0.31],
                model: "ms-marco".to_string(),
                elapsed_ms: 8,
            }),
        );
        let encoded = encode_message(&msg)?;
        let decoded: FramedMessage<Response> = decode_framed(&encoded)?;

        let Response::Rerank(resp) = decoded.payload else {
            return Err(test_error("expected rerank response payload"));
        };
        ensure_eq(resp.scores, vec![0.95, 0.72, 0.31], "rerank scores")
    }
}
