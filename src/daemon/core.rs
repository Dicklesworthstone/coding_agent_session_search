//! Daemon server core for the semantic model daemon.
//!
//! This module provides the server that listens on a Unix Domain Socket
//! and handles embedding/reranking requests using loaded models.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tracing::{debug, error, info, warn};

use super::models::ModelManager;
use super::protocol::{
    EmbedResponse, EmbeddingJobDetail, EmbeddingJobInfo, ErrorCode, ErrorResponse, FramedMessage,
    HealthStatus, ModelInfo, PROTOCOL_VERSION, Request, RerankResponse, Response, StatusResponse,
    decode_message, default_socket_path, encode_message,
};
use super::resource::ResourceMonitor;
use super::worker::{EmbeddingJobConfig, EmbeddingWorker, EmbeddingWorkerHandle};

/// Configuration for the daemon server.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Path to the Unix socket.
    pub socket_path: PathBuf,
    /// Maximum concurrent connections.
    pub max_connections: usize,
    /// Request timeout.
    pub request_timeout: Duration,
    /// Idle shutdown timeout (0 = never shutdown).
    pub idle_timeout: Duration,
    /// Memory limit in bytes (0 = unlimited).
    pub memory_limit: u64,
    /// Nice value for process priority (-20 to 19).
    pub nice_value: i32,
    /// IO priority class (0-3).
    pub ionice_class: u32,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            max_connections: 16,
            request_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(0), // Never shutdown by default
            memory_limit: 0,                      // Unlimited
            nice_value: 10,                       // Low priority
            ionice_class: 2,                      // Best-effort
        }
    }
}

impl DaemonConfig {
    /// Load config from environment variables.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(path) = dotenvy::var("CASS_DAEMON_SOCKET") {
            cfg.socket_path = PathBuf::from(path);
        }

        if let Ok(val) = dotenvy::var("CASS_DAEMON_MAX_CONNECTIONS")
            && let Ok(n) = val.parse()
        {
            cfg.max_connections = n;
        }

        if let Ok(val) = dotenvy::var("CASS_DAEMON_REQUEST_TIMEOUT_SECS")
            && let Ok(secs) = val.parse()
        {
            cfg.request_timeout = Duration::from_secs(secs);
        }

        if let Ok(val) = dotenvy::var("CASS_DAEMON_IDLE_TIMEOUT_SECS")
            && let Ok(secs) = val.parse()
        {
            cfg.idle_timeout = Duration::from_secs(secs);
        }

        if let Ok(val) = dotenvy::var("CASS_DAEMON_MEMORY_LIMIT")
            && let Ok(bytes) = val.parse()
        {
            cfg.memory_limit = bytes;
        }

        if let Ok(val) = dotenvy::var("CASS_DAEMON_NICE")
            && let Ok(n) = val.parse()
        {
            cfg.nice_value = n;
        }

        if let Ok(val) = dotenvy::var("CASS_DAEMON_IONICE_CLASS")
            && let Ok(n) = val.parse()
        {
            cfg.ionice_class = n;
        }

        cfg
    }
}

/// Daemon server state.
pub struct ModelDaemon {
    config: DaemonConfig,
    models: Arc<ModelManager>,
    resources: ResourceMonitor,
    start_time: Instant,
    total_requests: AtomicU64,
    active_connections: AtomicU64,
    shutdown: AtomicBool,
    last_activity: RwLock<Instant>,
    worker_handle: parking_lot::Mutex<Option<EmbeddingWorkerHandle>>,
}

impl ModelDaemon {
    /// Create a new daemon with the given configuration.
    pub fn new(config: DaemonConfig, models: ModelManager) -> Self {
        Self {
            config,
            models: Arc::new(models),
            resources: ResourceMonitor::new(),
            start_time: Instant::now(),
            total_requests: AtomicU64::new(0),
            active_connections: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            last_activity: RwLock::new(Instant::now()),
            worker_handle: parking_lot::Mutex::new(None),
        }
    }

    /// Create daemon with default config and models from data directory.
    pub fn with_defaults(data_dir: &Path) -> Self {
        let config = DaemonConfig::from_env();
        let models = ModelManager::new(data_dir);
        Self::new(config, models)
    }

    /// Get current uptime in seconds.
    pub fn uptime_secs(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    /// Check if daemon should shutdown due to idle timeout.
    fn should_shutdown_idle(&self) -> bool {
        if self.config.idle_timeout.is_zero() {
            return false;
        }
        let last = *self.last_activity.read();
        last.elapsed() > self.config.idle_timeout
    }

    /// Update last activity timestamp.
    fn touch_activity(&self) {
        *self.last_activity.write() = Instant::now();
    }

    /// Initialize the background embedding worker thread.
    fn init_worker(&self) {
        let (worker, handle) = EmbeddingWorker::new();
        match std::thread::Builder::new()
            .name("embedding-worker".into())
            .spawn(move || worker.run())
        {
            Ok(_) => {
                *self.worker_handle.lock() = Some(handle);
                info!("Embedding worker initialized");
            }
            Err(e) => {
                error!(
                    error = %e,
                    "Failed to spawn embedding worker - background jobs will be unavailable"
                );
                // Continue without worker - daemon can still handle other requests
            }
        }
    }

    /// Start the daemon server.
    pub fn run(&self) -> std::io::Result<()> {
        // Apply resource limits
        self.resources.apply_nice(self.config.nice_value);
        self.resources.apply_ionice(self.config.ionice_class);

        // Remove stale socket if exists
        if self.config.socket_path.exists() {
            std::fs::remove_file(&self.config.socket_path)?;
        }

        // Create parent directory if needed
        if let Some(parent) = self.config.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(&self.config.socket_path)?;
        listener.set_nonblocking(true)?;

        info!(
            socket = %self.config.socket_path.display(),
            max_connections = self.config.max_connections,
            "Daemon listening"
        );

        // Pre-warm models if available
        info!("Pre-warming models...");
        if let Err(e) = self.models.warm_embedder() {
            warn!(error = %e, "Failed to pre-warm embedder");
        }
        if let Err(e) = self.models.warm_reranker() {
            warn!(error = %e, "Failed to pre-warm reranker");
        }
        info!("Model pre-warming complete");

        // Start background embedding worker
        self.init_worker();

        loop {
            // Check for shutdown
            if self.shutdown.load(Ordering::SeqCst) {
                info!("Shutdown requested, stopping daemon");
                break;
            }

            // Check for idle shutdown
            if self.should_shutdown_idle() {
                info!(
                    idle_secs = self.config.idle_timeout.as_secs(),
                    "Idle timeout reached, shutting down"
                );
                break;
            }

            // Accept new connections
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let active = self.active_connections.fetch_add(1, Ordering::SeqCst);
                    if active >= self.config.max_connections as u64 {
                        self.active_connections.fetch_sub(1, Ordering::SeqCst);
                        warn!(
                            active = active,
                            max = self.config.max_connections,
                            "Max connections reached, rejecting"
                        );
                        continue;
                    }

                    self.touch_activity();
                    if let Err(e) = self.handle_connection(stream) {
                        debug!(error = %e, "Connection error");
                    }
                    self.active_connections.fetch_sub(1, Ordering::SeqCst);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No pending connections, sleep briefly
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => {
                    error!(error = %e, "Accept error");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }

        // Shutdown embedding worker
        if let Some(handle) = self.worker_handle.lock().take()
            && let Err(e) = handle.shutdown()
        {
            warn!(error = %e, "Failed to send shutdown to embedding worker");
        }

        // Cleanup
        if self.config.socket_path.exists() {
            let _ = std::fs::remove_file(&self.config.socket_path);
        }

        info!("Daemon stopped");
        Ok(())
    }

    /// Handle a single client connection.
    fn handle_connection(&self, mut stream: UnixStream) -> std::io::Result<()> {
        stream.set_read_timeout(Some(self.config.request_timeout))?;
        stream.set_write_timeout(Some(self.config.request_timeout))?;

        loop {
            // Read length prefix
            let mut len_buf = [0u8; 4];
            match stream.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    debug!("Client disconnected");
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    debug!("Connection timed out");
                    return Ok(());
                }
                Err(e) => return Err(e),
            }

            let len = u32::from_be_bytes(len_buf) as usize;
            if len > 100 * 1024 * 1024 {
                warn!(len = len, "Request too large, closing connection");
                return Ok(());
            }

            // Read request payload
            let mut payload = vec![0u8; len];
            stream.read_exact(&mut payload)?;

            // Decode and handle request
            let response = match decode_message::<Request>(&payload) {
                Ok(msg) => {
                    self.total_requests.fetch_add(1, Ordering::Relaxed);
                    self.touch_activity();
                    let response = self.handle_request(msg.request_id.clone(), msg.payload);
                    FramedMessage::new(msg.request_id, response)
                }
                Err(e) => {
                    warn!(error = %e, "Failed to decode request");
                    FramedMessage::new(
                        "error",
                        Response::Error(ErrorResponse {
                            code: ErrorCode::InvalidInput,
                            message: format!("decode error: {}", e),
                            retryable: false,
                            retry_after_ms: None,
                        }),
                    )
                }
            };

            // Send response
            let encoded =
                encode_message(&response).map_err(|e| std::io::Error::other(e.to_string()))?;
            stream.write_all(&encoded)?;

            // Check if this was a shutdown request
            if matches!(response.payload, Response::Shutdown { .. }) {
                return Ok(());
            }
        }
    }

    /// Handle a single request.
    fn handle_request(&self, request_id: String, request: Request) -> Response {
        let start = Instant::now();

        match request {
            Request::Health => Response::Health(HealthStatus {
                uptime_secs: self.uptime_secs(),
                version: PROTOCOL_VERSION,
                ready: self.models.is_ready(),
                memory_bytes: self.resources.memory_usage(),
            }),

            Request::Embed {
                texts,
                model,
                dims: _,
            } => {
                debug!(
                    request_id = %request_id,
                    batch_size = texts.len(),
                    model = %model,
                    "Processing embed request"
                );

                match self.models.embed_batch(&texts) {
                    Ok(embeddings) => Response::Embed(EmbedResponse {
                        embeddings,
                        model: self.models.embedder_id().to_string(),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                    }),
                    Err(e) => Response::Error(ErrorResponse {
                        code: ErrorCode::ModelLoadFailed,
                        message: e.to_string(),
                        retryable: true,
                        retry_after_ms: Some(1000),
                    }),
                }
            }

            Request::Rerank {
                query,
                documents,
                model,
            } => {
                debug!(
                    request_id = %request_id,
                    doc_count = documents.len(),
                    model = %model,
                    "Processing rerank request"
                );

                match self.models.rerank(&query, &documents) {
                    Ok(scores) => Response::Rerank(RerankResponse {
                        scores,
                        model: self.models.reranker_id().to_string(),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                    }),
                    Err(e) => Response::Error(ErrorResponse {
                        code: ErrorCode::ModelLoadFailed,
                        message: e.to_string(),
                        retryable: true,
                        retry_after_ms: Some(1000),
                    }),
                }
            }

            Request::Status => {
                let embedder_info = ModelInfo {
                    id: self.models.embedder_id().to_string(),
                    name: self.models.embedder_name().to_string(),
                    dimension: Some(self.models.embedder_dimension()),
                    loaded: self.models.embedder_loaded(),
                    memory_bytes: 0, // Would need model-specific tracking
                };

                let reranker_info = ModelInfo {
                    id: self.models.reranker_id().to_string(),
                    name: self.models.reranker_name().to_string(),
                    dimension: None,
                    loaded: self.models.reranker_loaded(),
                    memory_bytes: 0,
                };

                Response::Status(StatusResponse {
                    uptime_secs: self.uptime_secs(),
                    version: PROTOCOL_VERSION,
                    embedders: vec![embedder_info],
                    rerankers: vec![reranker_info],
                    memory_bytes: self.resources.memory_usage(),
                    total_requests: self.total_requests.load(Ordering::Relaxed),
                })
            }

            Request::SubmitEmbeddingJob {
                db_path,
                index_path,
                two_tier,
                fast_model,
                quality_model,
            } => {
                let config = EmbeddingJobConfig {
                    db_path,
                    index_path,
                    two_tier,
                    fast_model,
                    quality_model,
                };
                let guard = self.worker_handle.lock();
                match guard.as_ref() {
                    Some(handle) => match handle.submit(config) {
                        Ok(()) => Response::JobSubmitted {
                            job_id: request_id.clone(),
                            message: "embedding job submitted".to_string(),
                        },
                        Err(e) => Response::Error(ErrorResponse {
                            code: ErrorCode::Internal,
                            message: format!("failed to submit job: {e}"),
                            retryable: true,
                            retry_after_ms: Some(1000),
                        }),
                    },
                    None => Response::Error(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: "embedding worker not initialized".to_string(),
                        retryable: true,
                        retry_after_ms: Some(1000),
                    }),
                }
            }

            Request::EmbeddingJobStatus { db_path } => {
                match crate::storage::sqlite::SqliteStorage::open(std::path::Path::new(&db_path)) {
                    Ok(storage) => match storage.get_embedding_jobs(&db_path) {
                        Ok(rows) => {
                            let jobs = rows
                                .into_iter()
                                .map(|r| EmbeddingJobDetail {
                                    job_id: r.id,
                                    model_id: r.model_id,
                                    status: r.status,
                                    total_docs: r.total_docs,
                                    completed_docs: r.completed_docs,
                                    error_message: r.error_message,
                                })
                                .collect();
                            Response::JobStatus(EmbeddingJobInfo { jobs })
                        }
                        Err(e) => Response::Error(ErrorResponse {
                            code: ErrorCode::Internal,
                            message: format!("failed to query jobs: {e}"),
                            retryable: false,
                            retry_after_ms: None,
                        }),
                    },
                    Err(e) => Response::Error(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: format!("failed to open database: {e}"),
                        retryable: false,
                        retry_after_ms: None,
                    }),
                }
            }

            Request::CancelEmbeddingJob { db_path, model_id } => {
                // Send cancel to worker
                let guard = self.worker_handle.lock();
                if let Some(handle) = guard.as_ref() {
                    let _ = handle.cancel(db_path.clone(), model_id.clone());
                }

                // Also cancel in database
                match crate::storage::sqlite::SqliteStorage::open(std::path::Path::new(&db_path)) {
                    Ok(storage) => {
                        match storage.cancel_embedding_jobs(&db_path, model_id.as_deref()) {
                            Ok(count) => Response::JobCancelled {
                                cancelled: count,
                                message: format!("cancelled {count} job(s)"),
                            },
                            Err(e) => Response::Error(ErrorResponse {
                                code: ErrorCode::Internal,
                                message: format!("failed to cancel jobs: {e}"),
                                retryable: false,
                                retry_after_ms: None,
                            }),
                        }
                    }
                    Err(e) => Response::Error(ErrorResponse {
                        code: ErrorCode::Internal,
                        message: format!("failed to open database: {e}"),
                        retryable: false,
                        retry_after_ms: None,
                    }),
                }
            }

            Request::Shutdown => {
                info!(request_id = %request_id, "Shutdown requested");
                self.shutdown.store(true, Ordering::SeqCst);
                Response::Shutdown {
                    message: "daemon shutting down".to_string(),
                }
            }
        }
    }

    /// Request the daemon to shutdown.
    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_data_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[test]
    fn test_config_defaults() {
        let config = DaemonConfig::default();
        assert_eq!(config.max_connections, 16);
        assert_eq!(config.nice_value, 10);
        assert_eq!(config.ionice_class, 2);
    }

    #[test]
    fn test_daemon_uptime() {
        let config = DaemonConfig::default();
        let models = ModelManager::new(&test_data_dir());
        let daemon = ModelDaemon::new(config, models);

        // Uptime should be 0 or 1 second initially
        let initial = daemon.uptime_secs();
        std::thread::sleep(Duration::from_millis(50));
        let after = daemon.uptime_secs();
        // Uptime should not decrease
        assert!(after >= initial);
    }

    #[test]
    fn test_activity_tracking() {
        let config = DaemonConfig::default();
        let models = ModelManager::new(&test_data_dir());
        let daemon = ModelDaemon::new(config, models);

        let before = *daemon.last_activity.read();
        std::thread::sleep(Duration::from_millis(10));
        daemon.touch_activity();
        let after = *daemon.last_activity.read();

        assert!(after > before);
    }

    #[test]
    fn test_shutdown_flag() {
        let config = DaemonConfig::default();
        let models = ModelManager::new(&test_data_dir());
        let daemon = ModelDaemon::new(config, models);

        assert!(!daemon.shutdown.load(Ordering::SeqCst));
        daemon.request_shutdown();
        assert!(daemon.shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn test_idle_timeout_disabled_by_default() {
        let config = DaemonConfig::default();
        let models = ModelManager::new(&test_data_dir());
        let daemon = ModelDaemon::new(config, models);

        // With idle_timeout = 0, should never trigger idle shutdown
        assert!(!daemon.should_shutdown_idle());
    }
}
