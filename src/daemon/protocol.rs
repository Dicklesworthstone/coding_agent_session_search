//! CASS semantic-model daemon protocol.
//!
//! CASS uses its own socket namespace because its frame envelope and response
//! model-identity contract differ from xf's daemon protocol. Pointing both
//! clients at the same socket would make the first daemon to start look
//! connectable to the other client even though their messages cannot be
//! decoded safely.
//!
//! Protocol uses MessagePack for efficient binary serialization over Unix Domain Sockets.

use serde::{Deserialize, Serialize};
use std::io::{self, Cursor, Write};
use std::path::PathBuf;

use frankensearch::{
    AttestedDaemonEmbeddingResponseV1, DaemonChallengeV1, DaemonConnectionIdentityV1,
    DaemonEmbeddingAttestationV1,
};

/// Protocol version for compatibility checks.
/// Clients and the CASS daemon must use the same version.
pub const PROTOCOL_VERSION: u32 = 2;

/// Maximum encoded payload size, excluding the four-byte length prefix.
/// Enforced on both directions and by direct codec callers, not just sockets.
pub const MAX_FRAME_BYTES: usize = 10 * 1024 * 1024;
const MAX_FRAME_DEPTH: usize = 64;

/// Default CASS-owned socket path.
pub fn default_socket_path() -> PathBuf {
    let user = dotenvy::var("USER").unwrap_or_else(|_| "unknown".into());
    let safe_user = sanitize_socket_user(&user);
    std::env::temp_dir().join(format!("cass-semantic-daemon-{safe_user}.sock"))
}

fn sanitize_socket_user(user: &str) -> String {
    let safe_user: String = user
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
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
#[serde(deny_unknown_fields)]
pub enum Request {
    /// Health check - returns daemon status.
    Health,

    /// Fetch the candidate connection identity used to construct a challenge.
    /// This response is not trusted until an HMAC-authenticated handshake for
    /// the exact identity succeeds.
    ConnectionIdentity,

    /// Authenticate the candidate connection identity with a fresh challenge.
    HandshakeAttested { challenge: DaemonChallengeV1 },

    /// Authenticate current daemon readiness with a fresh challenge.
    HealthAttested { challenge: DaemonChallengeV1 },

    /// Generate embeddings for texts.
    Embed {
        texts: Vec<String>,
        model: String,
        dims: Option<usize>,
    },

    /// Generate producer-authenticated embeddings for an ordered text batch.
    EmbedAttested {
        texts: Vec<String>,
        model: String,
        dims: Option<usize>,
        challenge: DaemonChallengeV1,
    },

    /// Rerank documents against a query.
    Rerank {
        query: String,
        documents: Vec<String>,
        model: String,
    },

    /// Generate producer-authenticated rerank scores.
    RerankAttested {
        query: String,
        documents: Vec<String>,
        model: String,
        challenge: DaemonChallengeV1,
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
#[serde(deny_unknown_fields)]
pub enum Response {
    /// Health check response.
    Health(HealthStatus),

    /// Candidate connection identity. It becomes authoritative only after a
    /// successful attested handshake under the locally pinned key.
    ConnectionIdentity(DaemonConnectionIdentityV1),

    /// Signed handshake or health proof with no vector payload.
    Attestation(DaemonEmbeddingAttestationV1),

    /// Embedding response with vectors.
    Embed(EmbedResponse),

    /// Signed embedding or rerank payload.
    AttestedEmbedding(AttestedDaemonEmbeddingResponseV1),

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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct EmbeddingJobInfo {
    pub jobs: Vec<EmbeddingJobDetail>,
}

/// Detail for a single embedding job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

/// Encode a message without ever materializing an oversized wire payload.
/// The caller still owns the input objects and its wall-clock budget.
pub fn encode_message<T: Serialize>(msg: &FramedMessage<T>) -> Result<Vec<u8>, EncodeError> {
    struct FrameWriter(Vec<u8>);
    impl Write for FrameWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let total = self
                .0
                .len()
                .checked_add(bytes.len())
                .filter(|total| *total <= MAX_FRAME_BYTES + 4)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "daemon frame exceeds 10 MiB")
                })?;
            if total > self.0.capacity() {
                let capacity = self
                    .0
                    .capacity()
                    .saturating_mul(2)
                    .max(total)
                    .min(MAX_FRAME_BYTES + 4);
                self.0
                    .try_reserve_exact(capacity - self.0.len())
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::OutOfMemory, "cannot allocate daemon frame")
                    })?;
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut writer = FrameWriter(vec![0; 4]);
    msg.serialize(&mut rmp_serde::Serializer::new(&mut writer))?;
    let length = u32::try_from(writer.0.len() - 4)
        .map_err(|_| EncodeError::Message("daemon frame size overflow".into()))?;
    writer.0[..4].copy_from_slice(&length.to_be_bytes());
    Ok(writer.0)
}

/// Admit exactly one complete message, not a valid prefix followed by ignored
/// bytes. Unknown fields in CASS envelopes/payloads are rejected by their typed
/// definitions. Byte/depth bounds are not a whole-process memory or time cap.
/// Version and request correlation remain explicit dispatcher/client checks.
pub fn decode_message<T: for<'de> Deserialize<'de>>(
    data: &[u8],
) -> Result<FramedMessage<T>, DecodeError> {
    if data.is_empty() || data.len() > MAX_FRAME_BYTES {
        return Err(DecodeError::Message(
            "empty or oversized daemon frame".into(),
        ));
    }
    let mut decoder = rmp_serde::Deserializer::new(Cursor::new(data));
    decoder.set_max_depth(MAX_FRAME_DEPTH);
    let decoded = FramedMessage::<T>::deserialize(&mut decoder)
        // Serde errors can contain unknown field names or rejected values from
        // the peer. Never put those bytes into daemon logs or error responses.
        .map_err(|_| DecodeError::Message("invalid daemon message structure".into()))?;
    if decoder.position() != data.len() as u64 {
        return Err(DecodeError::Message("trailing data in daemon frame".into()));
    }
    Ok(decoded)
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

    use frankensearch::{
        AttestedDaemonEmbeddingResponseV1, DAEMON_CONNECTION_IDENTITY_SCHEMA_V1, DaemonChallengeV1,
        DaemonConnectionIdentityV1, DaemonOperationV1, Embedder as _, HashAlgorithm, HashEmbedder,
        ModelCategory,
    };

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

    fn attested_test_connection() -> DaemonConnectionIdentityV1 {
        let embedder = HashEmbedder::new(3, HashAlgorithm::FnvModular);
        DaemonConnectionIdentityV1 {
            schema_version: DAEMON_CONNECTION_IDENTITY_SCHEMA_V1,
            endpoint_fingerprint: "11".repeat(32),
            executable_fingerprint: "22".repeat(32),
            protocol_revision: "cass-test-attested-v1".to_string(),
            key_id: "cass-test-key-v1".to_string(),
            generation: 1,
            embedding_identity: embedder.identity().expect("hash identity").clone(),
            model_category: ModelCategory::HashEmbedder,
        }
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
    fn attested_embed_request_and_response_round_trip_without_losing_proof() -> TestResult {
        let connection = attested_test_connection();
        let challenge = DaemonChallengeV1::for_inputs(
            "aa".repeat(32),
            DaemonOperationV1::Embed,
            &["hello"],
            &connection,
        )?;
        let request = FramedMessage::new(
            "attested-request",
            Request::EmbedAttested {
                texts: vec!["hello".to_string()],
                model: "default".to_string(),
                dims: None,
                challenge: challenge.clone(),
            },
        );
        let decoded: FramedMessage<Request> = decode_framed(&encode_message(&request)?)?;
        let Request::EmbedAttested {
            texts,
            challenge: decoded_challenge,
            ..
        } = decoded.payload
        else {
            return Err(test_error("expected attested embed request"));
        };
        ensure_eq(texts, vec!["hello".to_string()], "attested inputs")?;
        ensure_eq(decoded_challenge, challenge.clone(), "attested challenge")?;

        let key = [7_u8; 32];
        let signed = AttestedDaemonEmbeddingResponseV1::signed(
            challenge.clone(),
            connection.clone(),
            vec![vec![0.25, 0.5, 0.75]],
            &key,
        )?;
        let response = FramedMessage::new("attested-response", Response::AttestedEmbedding(signed));
        let decoded: FramedMessage<Response> = decode_framed(&encode_message(&response)?)?;
        let Response::AttestedEmbedding(decoded) = decoded.payload else {
            return Err(test_error("expected attested embed response"));
        };
        decoded
            .attestation
            .validate_against(&challenge, &connection, &decoded.vectors)?;
        decoded.attestation.authenticate_hmac_sha256(&key)?;
        ensure_eq(decoded.vectors, vec![vec![0.25, 0.5, 0.75]], "vectors")
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
            file_name.starts_with("cass-semantic-daemon-"),
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
            sanitize_socket_user("用户"),
            "unknown".to_string(),
            "non-ASCII usernames must not defeat the socket byte-length cap",
        )?;
        ensure_eq(
            sanitize_socket_user("a".repeat(80).as_str()).len(),
            64,
            "socket user length cap",
        )
    }

    #[test]
    fn test_embed_response_round_trip() -> TestResult {
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
    fn test_rerank_response_round_trip() -> TestResult {
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

    #[test]
    fn frame_rejects_both_junk_and_a_second_message_after_a_valid_request() -> TestResult {
        let encoded = encode_message(&FramedMessage::new("first", Request::Health))?;
        let second = encode_message(&FramedMessage::new("second", Request::Shutdown))?;
        for trailer in [&[0xc0][..], &second[4..], &[0xff, 0x00][..]] {
            let mut payload = encoded[4..].to_vec();
            payload.extend_from_slice(trailer);
            let error = decode_message::<Request>(&payload).unwrap_err();
            ensure_eq(
                error.to_string(),
                "decode error: trailing data in daemon frame".into(),
                "trailing data",
            )?;
        }
        Ok(())
    }

    #[test]
    fn every_truncated_prefix_of_a_complete_request_is_rejected() -> TestResult {
        let encoded = encode_message(&FramedMessage::new(
            "unicode",
            Request::Embed {
                texts: vec!["日本語 — café".into(), "line one\nline two".into()],
                model: "default".into(),
                dims: Some(384),
            },
        ))?;
        let bytes = &encoded[4..];
        for end in 0..bytes.len() {
            ensure(
                decode_message::<Request>(&bytes[..end]).is_err(),
                format!("accepted truncation at {end}"),
            )?;
        }
        ensure(
            decode_message::<Request>(bytes).is_ok(),
            "complete message rejected",
        )
    }

    #[test]
    fn named_and_compact_protocol_messages_have_identical_meaning() -> TestResult {
        let original = FramedMessage::new(
            "named",
            Request::EmbeddingJobStatus {
                db_path: "/tmp/会話.db".into(),
            },
        );
        let named = rmp_serde::to_vec_named(&original)?;
        let compact = encode_message(&original)?;
        let left = decode_message::<Request>(&named)?;
        let right = decode_message::<Request>(&compact[4..])?;
        ensure_eq(
            serde_json::to_value(left)?,
            serde_json::to_value(right)?,
            "wire representation",
        )
    }

    #[test]
    fn unknown_envelope_and_command_fields_are_rejected_without_echoing_them() -> TestResult {
        const PRIVATE: &str = "private-unknown-field-value";
        let original = FramedMessage::new(
            "strict",
            Request::Embed {
                texts: vec!["hello".into()],
                model: "default".into(),
                dims: None,
            },
        );
        for nested in [false, true] {
            let mut value = serde_json::to_value(&original)?;
            if nested {
                value["payload"]["Embed"][PRIVATE] = serde_json::json!(PRIVATE);
            } else {
                value[PRIVATE] = serde_json::json!(PRIVATE);
            }
            let bytes = rmp_serde::to_vec_named(&value)?;
            let error = decode_message::<Request>(&bytes).unwrap_err();
            ensure(
                !error.to_string().contains(PRIVATE),
                "peer bytes leaked into diagnostic",
            )?;
        }
        Ok(())
    }

    #[test]
    fn unknown_response_fields_are_not_silently_accepted() -> TestResult {
        let mut value = serde_json::to_value(FramedMessage::new(
            "reply",
            Response::Health(HealthStatus {
                uptime_secs: 0,
                version: PROTOCOL_VERSION,
                ready: true,
                memory_bytes: 0,
            }),
        ))?;
        value["payload"]["Health"]["unverified_authority"] = serde_json::json!(true);
        ensure(
            decode_message::<Response>(&rmp_serde::to_vec_named(&value)?).is_err(),
            "unknown response field accepted",
        )
    }

    #[test]
    fn codec_enforces_payload_extent_and_nesting_before_releasing_a_message() -> TestResult {
        ensure(
            decode_message::<Request>(&vec![0; super::MAX_FRAME_BYTES + 1]).is_err(),
            "oversized direct decode",
        )?;
        // A valid generic envelope whose payload is a deeply nested array.
        let mut nested = vec![0x93, PROTOCOL_VERSION as u8, 0xa1, b'r'];
        nested.extend(std::iter::repeat_n(0x91, super::MAX_FRAME_DEPTH + 8));
        nested.push(0xc0);
        ensure(
            decode_message::<serde_json::Value>(&nested).is_err(),
            "depth cap ignored",
        )
    }

    #[test]
    fn maximum_sized_frame_round_trips_and_the_next_byte_is_rejected() -> TestResult {
        // Compact [version, "r", str32]: 1 + 1 + 2 + 5 bytes of overhead.
        let content = "x".repeat(super::MAX_FRAME_BYTES - 9);
        let encoded = encode_message(&FramedMessage::new("r", &content))?;
        ensure_eq(
            encoded.len(),
            super::MAX_FRAME_BYTES + 4,
            "maximum encoded size",
        )?;
        let decoded = decode_message::<String>(&encoded[4..])?;
        ensure_eq(decoded.payload, content.clone(), "maximum payload")?;
        ensure(
            encode_message(&FramedMessage::new("r", content + "x")).is_err(),
            "oversized encode accepted",
        )
    }

    #[test]
    fn oversized_streaming_serialization_stops_before_materializing_every_item() -> TestResult {
        use serde::ser::SerializeSeq;
        struct Streaming(std::cell::Cell<usize>);
        impl serde::Serialize for Streaming {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut sequence = serializer.serialize_seq(Some(20_000))?;
                let block = "x".repeat(1024);
                for _ in 0..20_000 {
                    self.0.set(self.0.get() + 1);
                    sequence.serialize_element(&block)?;
                }
                sequence.end()
            }
        }
        let streaming = Streaming(std::cell::Cell::new(0));
        ensure(
            encode_message(&FramedMessage::new("stream", &streaming)).is_err(),
            "oversized serialization accepted",
        )?;
        ensure(
            streaming.0.get() > 0 && streaming.0.get() < 20_000,
            "serializer did not stop at the frame budget",
        )
    }
}
