//! Daemon server core for the semantic model daemon.
//!
//! This module provides the server that listens on a Unix Domain Socket
//! and handles embedding/reranking requests using loaded models.

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fs2::FileExt;
use parking_lot::RwLock;
use tracing::{debug, error, info, warn};

use super::daemon_run_lock_path;
use super::models::ModelManager;
use super::protocol::{
    EmbedResponse, EmbeddingJobDetail, EmbeddingJobInfo, ErrorCode, ErrorResponse, FramedMessage,
    HealthStatus, ModelInfo, PROTOCOL_VERSION, Request, RerankResponse, Response, StatusResponse,
    decode_message, default_socket_path, encode_message,
};
use super::resource::ResourceMonitor;
use super::worker::{EmbeddingJobConfig, EmbeddingWorker, EmbeddingWorkerHandle};

struct BoundDaemonSocket {
    listener: UnixListener,
    public_path: PathBuf,
    bind_path: PathBuf,
}

fn create_owner_only_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn parent_dir_is_owner_only(path: &Path) -> std::io::Result<bool> {
    let Some(parent) = path.parent() else {
        return Ok(false);
    };

    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("socket parent is not a directory: {}", parent.display()),
        ));
    }

    Ok(metadata.permissions().mode() & 0o077 == 0)
}

fn private_runtime_dir_for_socket(socket_path: &Path) -> std::io::Result<PathBuf> {
    let parent = socket_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = socket_path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("socket path has no file name: {}", socket_path.display()),
        )
    })?;

    let mut runtime_name = OsString::from(".");
    runtime_name.push(file_name);
    runtime_name.push(".runtime");
    Ok(parent.join(runtime_name))
}

fn remove_stale_socket_path(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_socket() || file_type.is_symlink() {
                fs::remove_file(path)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "refusing to remove non-socket daemon path: {}",
                        path.display()
                    ),
                ))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn bind_owner_only_unix_listener(socket_path: &Path) -> std::io::Result<BoundDaemonSocket> {
    if let Some(parent) = socket_path.parent()
        && !parent.exists()
    {
        create_owner_only_dir_all(parent)?;
    }

    let bind_path = if parent_dir_is_owner_only(socket_path)? {
        socket_path.to_path_buf()
    } else {
        let runtime_dir = private_runtime_dir_for_socket(socket_path)?;
        create_owner_only_dir_all(&runtime_dir)?;
        runtime_dir.join("daemon.sock")
    };

    remove_stale_socket_path(&bind_path)?;
    if bind_path != socket_path {
        remove_stale_socket_path(socket_path)?;
    }

    let listener = UnixListener::bind(&bind_path)?;
    fs::set_permissions(&bind_path, fs::Permissions::from_mode(0o600))?;

    if bind_path != socket_path {
        std::os::unix::fs::symlink(&bind_path, socket_path)?;
    }

    Ok(BoundDaemonSocket {
        listener,
        public_path: socket_path.to_path_buf(),
        bind_path,
    })
}

fn cleanup_bound_socket(public_path: &Path, bind_path: &Path) {
    let _ = remove_stale_socket_path(public_path);
    if bind_path != public_path {
        let _ = remove_stale_socket_path(bind_path);
    }
}

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

    fn embedder_model_info(&self) -> ModelInfo {
        ModelInfo {
            id: self.models.embedder_id().to_string(),
            name: self.models.embedder_name().to_string(),
            dimension: Some(self.models.embedder_dimension()),
            loaded: self.models.embedder_loaded(),
            memory_bytes: 0, // Would need model-specific tracking
        }
    }

    fn reranker_model_info(&self) -> ModelInfo {
        ModelInfo {
            id: self.models.reranker_id().to_string(),
            name: self.models.reranker_name().to_string(),
            dimension: None,
            loaded: self.models.reranker_loaded(),
            memory_bytes: 0,
        }
    }

    fn status_response(&self) -> StatusResponse {
        StatusResponse {
            uptime_secs: self.uptime_secs(),
            version: PROTOCOL_VERSION,
            embedders: vec![self.embedder_model_info()],
            rerankers: vec![self.reranker_model_info()],
            memory_bytes: self.resources.memory_usage(),
            total_requests: self.total_requests.load(Ordering::Relaxed),
        }
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

    /// Check whether configured memory limit is exceeded.
    fn memory_limit_exceeded(&self) -> bool {
        if self.config.memory_limit == 0 {
            return false;
        }
        let memory_bytes = self.resources.memory_usage();
        memory_bytes > self.config.memory_limit
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
        // Use a file lock to ensure only one daemon instance runs for this socket path
        let lock_path = daemon_run_lock_path(&self.config.socket_path);
        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        // Acquire exclusive lock (non-blocking to fail fast if another daemon is already running)
        if lock_file.try_lock_exclusive().is_err() {
            warn!(
                socket = %self.config.socket_path.display(),
                "Another daemon is already running for this socket path"
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "Another daemon is already running",
            ));
        }

        // Apply resource limits
        if !self.resources.apply_nice(self.config.nice_value) {
            warn!(
                nice = self.config.nice_value,
                "Failed to apply configured daemon nice value"
            );
        }
        if !self.resources.apply_ionice(self.config.ionice_class) {
            warn!(
                ionice_class = self.config.ionice_class,
                "Failed to apply configured daemon ionice class"
            );
        }

        let BoundDaemonSocket {
            listener,
            public_path,
            bind_path,
        } = bind_owner_only_unix_listener(&self.config.socket_path)?;
        listener.set_nonblocking(true)?;

        info!(
            socket = %self.config.socket_path.display(),
            bound_socket = %bind_path.display(),
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

        std::thread::scope(|s| {
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

                // Enforce configured memory limit when enabled.
                if self.memory_limit_exceeded() {
                    let memory_bytes = self.resources.memory_usage();
                    error!(
                        memory_bytes = memory_bytes,
                        memory_limit = self.config.memory_limit,
                        "Daemon memory limit exceeded, shutting down"
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
                        s.spawn(move || {
                            if let Err(e) = self.handle_connection(stream) {
                                debug!(error = %e, "Connection error");
                            }
                            self.active_connections.fetch_sub(1, Ordering::SeqCst);
                        });
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
        });

        // Shutdown embedding worker
        if let Some(handle) = self.worker_handle.lock().take()
            && let Err(e) = handle.shutdown()
        {
            warn!(error = %e, "Failed to send shutdown to embedding worker");
        }

        // Cleanup
        cleanup_bound_socket(&public_path, &bind_path);

        info!("Daemon stopped");
        Ok(())
    }

    fn read_frame_bytes_with_shutdown(
        &self,
        stream: &mut UnixStream,
        buf: &mut [u8],
        poll_timeout: Duration,
        request_timeout: Duration,
        reset_timeout_on_progress: bool,
    ) -> std::io::Result<bool> {
        if buf.is_empty() {
            return Ok(true);
        }

        stream.set_read_timeout(Some(poll_timeout))?;
        let started_at = Instant::now();
        let mut last_progress_at = started_at;
        let mut filled = 0usize;

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                debug!("Shutdown requested, closing connection read");
                return Ok(false);
            }

            match stream.read(&mut buf[filled..]) {
                Ok(0) => {
                    debug!("Client disconnected");
                    return Ok(false);
                }
                Ok(n) => {
                    filled += n;
                    last_progress_at = Instant::now();
                    if filled == buf.len() {
                        return Ok(true);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    let timeout_started_at = if reset_timeout_on_progress {
                        last_progress_at
                    } else {
                        started_at
                    };
                    if timeout_started_at.elapsed() >= request_timeout {
                        debug!("Connection timed out");
                        return Ok(false);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Handle a single client connection.
    fn handle_connection(&self, mut stream: UnixStream) -> std::io::Result<()> {
        // Bounded idle-poll interval so `std::thread::scope` shutdown does
        // not stall behind a client that opened the socket and never sent
        // bytes. The configured `request_timeout` still bounds the total
        // idle wait; this just breaks the single long blocking read into
        // short chunks and checks `self.shutdown` between them.
        const IDLE_SHUTDOWN_POLL: Duration = Duration::from_millis(250);
        let request_timeout = self.config.request_timeout;
        let idle_poll = IDLE_SHUTDOWN_POLL.min(request_timeout);
        stream.set_write_timeout(Some(request_timeout))?;

        loop {
            // Idle read (length prefix): short-poll so shutdown cancels
            // promptly. Track `filled` manually because `read_exact`
            // discards partial bytes on timeout.
            let mut len_buf = [0u8; 4];
            if !self.read_frame_bytes_with_shutdown(
                &mut stream,
                &mut len_buf,
                idle_poll,
                request_timeout,
                false,
            )? {
                return Ok(());
            }

            let len = u32::from_be_bytes(len_buf) as usize;
            if len > 10 * 1024 * 1024 {
                warn!(
                    len = len,
                    "Request too large (max 10MB), closing connection"
                );
                return Ok(());
            }

            // Payload read: bytes are in flight, so keep the timeout as an
            // idle-progress budget while still short-polling shutdown.
            let mut payload = vec![0u8; len];
            if !self.read_frame_bytes_with_shutdown(
                &mut stream,
                &mut payload,
                idle_poll,
                request_timeout,
                true,
            )? {
                return Ok(());
            }

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

            Request::Status => Response::Status(self.status_response()),

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
                match crate::storage::sqlite::FrankenStorage::open(std::path::Path::new(&db_path)) {
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
                match crate::storage::sqlite::FrankenStorage::open(std::path::Path::new(&db_path)) {
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
    use tempfile::TempDir;

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
    fn status_response_projects_default_model_fields() {
        let config = DaemonConfig::default();
        let models = ModelManager::new(&test_data_dir());
        let daemon = ModelDaemon::new(config, models);

        let status = daemon.status_response();

        assert_eq!(status.version, PROTOCOL_VERSION);
        assert_eq!(status.total_requests, 0);
        assert_eq!(status.embedders.len(), 1);
        assert_eq!(status.rerankers.len(), 1);

        let embedder = &status.embedders[0];
        assert_eq!(embedder.id, "hash-384");
        assert_eq!(embedder.name, "not-loaded");
        assert_eq!(embedder.dimension, Some(384));
        assert!(!embedder.loaded);
        assert_eq!(embedder.memory_bytes, 0);

        let reranker = &status.rerankers[0];
        assert_eq!(reranker.id, "none");
        assert_eq!(reranker.name, "not-loaded");
        assert_eq!(reranker.dimension, None);
        assert!(!reranker.loaded);
        assert_eq!(reranker.memory_bytes, 0);
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

    #[test]
    fn test_daemon_run_lock_path_is_stable() {
        let socket = PathBuf::from("/tmp/cass-semantic.sock");
        assert_eq!(
            daemon_run_lock_path(&socket),
            PathBuf::from("/tmp/cass-semantic.spawnlock")
        );
    }

    #[test]
    fn test_owner_only_bind_uses_private_runtime_dir_for_public_parent() {
        let temp_dir = TempDir::new().unwrap();
        let public_dir = temp_dir.path().join("public");
        fs::create_dir(&public_dir).unwrap();
        fs::set_permissions(&public_dir, fs::Permissions::from_mode(0o777)).unwrap();
        let public_socket = public_dir.join("daemon.sock");

        let BoundDaemonSocket {
            listener,
            public_path,
            bind_path,
        } = bind_owner_only_unix_listener(&public_socket).unwrap();

        assert_eq!(public_path, public_socket);
        assert_ne!(bind_path, public_socket);
        assert!(
            fs::symlink_metadata(&public_socket)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let runtime_dir = bind_path.parent().unwrap();
        assert_eq!(
            fs::symlink_metadata(runtime_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::symlink_metadata(&bind_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let accept_thread = std::thread::spawn(move || listener.accept().map(|_| ()));
        let client = UnixStream::connect(&public_socket).unwrap();
        drop(client);
        accept_thread.join().unwrap().unwrap();

        cleanup_bound_socket(&public_path, &bind_path);
    }

    /// `coding_agent_session_search-a5z57`: before the short-poll fix,
    /// an idle client holding a connection open without sending bytes
    /// would pin `handle_connection` inside `read_exact` for the full
    /// `request_timeout` — 60s in the default config. Because the
    /// connection handlers run inside `std::thread::scope` in
    /// `ModelDaemon::run`, shutdown could not complete until every
    /// such handler bled out its idle read, so a single idle peer
    /// made `systemctl stop` / SIGTERM feel like a 60-second hang.
    ///
    /// This test pins the fix contract: with `request_timeout` set to
    /// a value much larger than the handler's effective shutdown
    /// latency, setting `self.shutdown` must cause an idle handler to
    /// return promptly (well under the configured timeout).
    #[test]
    fn handle_connection_returns_promptly_when_shutdown_set_during_idle_read() {
        use std::os::unix::net::UnixStream;
        use std::sync::Arc;
        use std::time::Instant;

        // 10s request_timeout is plenty big to catch a regression: if
        // the handler falls back to the old single-blocking-read path,
        // shutdown latency would be ~10s, not the sub-second target
        // asserted below.
        let config = DaemonConfig {
            request_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let models = ModelManager::new(&test_data_dir());
        let daemon = Arc::new(ModelDaemon::new(config, models));

        let (server_side, _client_side) = UnixStream::pair().expect("create socketpair");

        // Drive handle_connection on the server side in a worker thread;
        // client side stays open but sends nothing, emulating the idle
        // peer that used to block shutdown.
        let handler_daemon = Arc::clone(&daemon);
        let handler_thread =
            std::thread::spawn(move || handler_daemon.handle_connection(server_side));

        // Let the handler settle into its idle read loop before
        // requesting shutdown (the first read poll arms at 250ms).
        std::thread::sleep(Duration::from_millis(100));

        let shutdown_requested_at = Instant::now();
        daemon.request_shutdown();

        // Join with a generous safety bound that is still well below
        // the 10s request_timeout — a regression to the old behavior
        // would exceed this.
        let join_budget = Duration::from_secs(3);
        let join_deadline = Instant::now() + join_budget;
        let mut joined = false;
        while Instant::now() < join_deadline {
            if handler_thread.is_finished() {
                joined = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        assert!(
            joined,
            "handle_connection must observe shutdown within {join_budget:?}; \
             regression suggests the idle read is no longer short-polled"
        );
        let shutdown_latency = shutdown_requested_at.elapsed();
        assert!(
            shutdown_latency < Duration::from_secs(2),
            "shutdown latency {shutdown_latency:?} is too high; short-poll \
             interval is supposed to cap it near IDLE_SHUTDOWN_POLL (~250ms)"
        );
        let result = handler_thread
            .join()
            .expect("handle_connection thread panicked");
        assert!(
            result.is_ok(),
            "handler must return Ok on shutdown-during-idle; got {result:?}"
        );
    }

    #[test]
    fn handle_connection_returns_promptly_when_shutdown_set_during_partial_payload_read() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        use std::sync::Arc;
        use std::time::Instant;

        let config = DaemonConfig {
            request_timeout: Duration::from_secs(10),
            ..Default::default()
        };
        let models = ModelManager::new(&test_data_dir());
        let daemon = Arc::new(ModelDaemon::new(config, models));

        let (server_side, mut client_side) = UnixStream::pair().expect("create socketpair");
        client_side
            .write_all(&4u32.to_be_bytes())
            .expect("write length prefix only");

        let handler_daemon = Arc::clone(&daemon);
        let handler_thread =
            std::thread::spawn(move || handler_daemon.handle_connection(server_side));

        std::thread::sleep(Duration::from_millis(100));

        let shutdown_requested_at = Instant::now();
        daemon.request_shutdown();

        let join_budget = Duration::from_secs(3);
        let join_deadline = Instant::now() + join_budget;
        let mut joined = false;
        while Instant::now() < join_deadline {
            if handler_thread.is_finished() {
                joined = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        assert!(
            joined,
            "handle_connection must observe shutdown while waiting for a partial payload"
        );
        let shutdown_latency = shutdown_requested_at.elapsed();
        assert!(
            shutdown_latency < Duration::from_secs(2),
            "partial-payload shutdown latency {shutdown_latency:?} is too high"
        );
        let result = handler_thread
            .join()
            .expect("handle_connection thread panicked");
        assert!(
            result.is_ok(),
            "handler must return Ok on shutdown-during-partial-payload; got {result:?}"
        );
    }
}
