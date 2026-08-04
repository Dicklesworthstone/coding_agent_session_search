//! Daemon client for connecting to the semantic model daemon.
//!
//! This client connects via Unix Domain Socket and provides methods for
//! embedding and reranking. It implements the `DaemonClient` trait from
//! `search::daemon_client` for integration with the fallback wrappers.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fs2::FileExt;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use super::protocol::{
    EmbeddingJobInfo, ErrorCode, FramedMessage, HealthStatus, PROTOCOL_VERSION, Request, Response,
    decode_message, default_socket_path, encode_message,
};
use super::worker::EmbeddingJobConfig;
use super::{
    DaemonRunLockMetadata, daemon_run_lock_path, daemon_spawn_guard_lock_path,
    published_lexical_generation,
};
use crate::search::daemon_client::{DaemonClient, DaemonError};

/// Hard ceiling for the status/doctor daemon probe. The worker owns its socket
/// and may finish after a timeout, but the diagnostic caller never waits past
/// this bound and never auto-spawns or mutates daemon state.
pub const DEFAULT_RUNTIME_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug)]
struct RuntimeProbeSocketResult {
    socket_connectable: bool,
    responded_to_ping: bool,
    connect_error: Option<String>,
}

fn connection_not_established() -> DaemonError {
    DaemonError::Unavailable("connection not established".to_string())
}

fn unexpected_response(response: Response) -> DaemonError {
    DaemonError::Failed(format!("unexpected response: {response:?}"))
}

/// Configuration for the daemon client.
#[derive(Debug, Clone)]
pub struct DaemonClientConfig {
    /// Path to the Unix socket.
    pub socket_path: PathBuf,
    /// Connection timeout.
    pub connect_timeout: Duration,
    /// Request timeout.
    pub request_timeout: Duration,
    /// Whether to auto-spawn daemon if not running.
    pub auto_spawn: bool,
    /// Path to the daemon binary (if auto-spawn is enabled).
    pub daemon_binary: Option<PathBuf>,
    /// Embedder identity required by the caller's vector index.
    ///
    /// The daemon protocol reports the model that produced every embedding.
    /// When this is set, a response from a different model is rejected instead
    /// of feeding a same-width but semantically incompatible vector into the
    /// caller's index (#347).
    pub expected_embedder_id: Option<String>,
}

impl Default for DaemonClientConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(30),
            auto_spawn: true,
            daemon_binary: None, // Will use current executable with --daemon flag
            expected_embedder_id: None,
        }
    }
}

impl DaemonClientConfig {
    /// Load config from environment variables.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(path) = dotenvy::var("CASS_DAEMON_SOCKET") {
            cfg.socket_path = PathBuf::from(path);
        }

        if let Ok(val) = dotenvy::var("CASS_DAEMON_CONNECT_TIMEOUT_MS")
            && let Ok(ms) = val.parse::<u64>()
        {
            cfg.connect_timeout = Duration::from_millis(ms);
        }

        if let Ok(val) = dotenvy::var("CASS_DAEMON_REQUEST_TIMEOUT_MS")
            && let Ok(ms) = val.parse::<u64>()
        {
            cfg.request_timeout = Duration::from_millis(ms);
        }

        if let Ok(val) = dotenvy::var("CASS_DAEMON_AUTO_SPAWN") {
            cfg.auto_spawn = val.eq_ignore_ascii_case("true") || val == "1";
        }

        if let Ok(path) = dotenvy::var("CASS_DAEMON_BINARY") {
            cfg.daemon_binary = Some(PathBuf::from(path));
        }

        cfg
    }
}

/// Unix Domain Socket client for the semantic daemon.
pub struct UdsDaemonClient {
    config: DaemonClientConfig,
    connection: Mutex<Option<UnixStream>>,
    available: AtomicBool,
    request_counter: AtomicU64,
    last_health_check: Mutex<Option<Instant>>,
}

impl UdsDaemonClient {
    /// Create a new client with the given configuration.
    pub fn new(config: DaemonClientConfig) -> Self {
        Self {
            config,
            connection: Mutex::new(None),
            available: AtomicBool::new(false),
            request_counter: AtomicU64::new(0),
            last_health_check: Mutex::new(None),
        }
    }

    /// Create a client with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(DaemonClientConfig::from_env())
    }

    fn validate_embedder_id(&self, actual: &str) -> Result<(), DaemonError> {
        if let Some(expected) = self.config.expected_embedder_id.as_deref()
            && actual != expected
        {
            return Err(DaemonError::InvalidInput(format!(
                "daemon embedder mismatch: expected {expected}, received {actual}"
            )));
        }
        Ok(())
    }

    /// Connect to the daemon, optionally spawning it if not running.
    pub fn connect(&self) -> Result<(), DaemonError> {
        // Try to connect to existing daemon
        if let Ok(stream) = self.try_connect() {
            *self.connection.lock() = Some(stream);
            self.available.store(true, Ordering::SeqCst);
            debug!(socket = %self.config.socket_path.display(), "Connected to existing daemon");
            return Ok(());
        }

        // If auto-spawn is enabled and connection failed, try to spawn
        if self.config.auto_spawn {
            info!("Daemon not running, attempting to spawn");
            self.spawn_daemon()?;

            // Wait for daemon to start and retry connection
            for attempt in 0..10 {
                std::thread::sleep(Duration::from_millis(100 * (attempt + 1)));
                if let Ok(stream) = self.try_connect() {
                    *self.connection.lock() = Some(stream);
                    self.available.store(true, Ordering::SeqCst);
                    info!(
                        socket = %self.config.socket_path.display(),
                        attempts = attempt + 1,
                        "Connected to newly spawned daemon"
                    );
                    return Ok(());
                }
            }

            return Err(DaemonError::Unavailable(
                "daemon failed to start within timeout".to_string(),
            ));
        }

        Err(DaemonError::Unavailable(format!(
            "daemon not running at {}",
            self.config.socket_path.display()
        )))
    }

    /// Try to connect to the daemon socket.
    fn try_connect(&self) -> std::io::Result<UnixStream> {
        let stream = UnixStream::connect(&self.config.socket_path)?;
        stream.set_read_timeout(Some(self.config.request_timeout))?;
        stream.set_write_timeout(Some(self.config.request_timeout))?;
        Ok(stream)
    }

    /// Spawn the daemon process.
    fn spawn_daemon(&self) -> Result<(), DaemonError> {
        let binary = self
            .config
            .daemon_binary
            .clone()
            .or_else(|| std::env::current_exe().ok())
            .ok_or_else(|| {
                DaemonError::Unavailable("cannot determine daemon binary path".to_string())
            })?;

        // Use a file lock to prevent multiple processes from spawning the daemon simultaneously
        let lock_path = daemon_spawn_guard_lock_path(&self.config.socket_path);

        let lock_file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Prevent symlink attacks by refusing to open symlinks.
                if std::fs::symlink_metadata(&lock_path)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    return Err(DaemonError::Unavailable(
                        "refusing to open a symlink spawn lock".to_string(),
                    ));
                }
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&lock_path)
                    .map_err(|e| {
                        DaemonError::Unavailable(format!("failed to open spawn lock: {}", e))
                    })?
            }
            Err(e) => {
                return Err(DaemonError::Unavailable(format!(
                    "failed to create spawn lock: {}",
                    e
                )));
            }
        };

        // Acquire exclusive lock (blocks until available) so concurrent clients
        // don't all try to auto-spawn the daemon at once.
        lock_file.lock_exclusive().map_err(|e| {
            DaemonError::Unavailable(format!("failed to acquire spawn lock: {}", e))
        })?;

        // Re-check if daemon is already running now that we hold the lock
        if UnixStream::connect(&self.config.socket_path).is_ok() {
            debug!("Daemon already running, skipping spawn");
            return Ok(());
        }

        remove_stale_daemon_socket(&self.config.socket_path)?;

        // Spawn daemon in background
        let result = Command::new(&binary)
            .arg("daemon")
            .arg("--socket")
            .arg(&self.config.socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match result {
            Ok(mut child) => {
                info!(
                    pid = child.id(),
                    binary = %binary.display(),
                    socket = %self.config.socket_path.display(),
                    "Spawned daemon process"
                );
                self.wait_for_spawned_daemon_ready(&mut child)?;
                // Reap the child in a background thread to avoid zombie processes.
                // The daemon is long-lived, so we just detach and let it run.
                // ubs:ignore — detached reaper thread intentionally waits on the
                // spawned daemon child so an auto-started daemon does not become
                // a zombie when it eventually exits.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                Ok(())
            }
            Err(e) => Err(DaemonError::Unavailable(format!(
                "failed to spawn daemon: {}",
                e
            ))),
        }
    }

    fn wait_for_spawned_daemon_ready(&self, child: &mut Child) -> Result<(), DaemonError> {
        let ready_timeout = self.config.connect_timeout.max(Duration::from_secs(5));
        let started = Instant::now();
        while started.elapsed() < ready_timeout {
            if UnixStream::connect(&self.config.socket_path).is_ok() {
                return Ok(());
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(DaemonError::Unavailable(format!(
                        "spawned daemon exited before becoming ready: {}",
                        status
                    )));
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(
                        error = %error,
                        socket = %self.config.socket_path.display(),
                        "failed to poll spawned daemon status while waiting for readiness"
                    );
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    /// Get a fresh connection, reconnecting if needed.
    fn get_connection_locked(
        &self,
    ) -> Result<parking_lot::MutexGuard<'_, Option<UnixStream>>, DaemonError> {
        // Try to use existing connection
        let conn = self.connection.lock();
        let is_valid = conn.as_ref().is_some_and(|s| s.peer_addr().is_ok());

        if is_valid {
            return Ok(conn);
        }

        // Connection is stale or missing, release lock and reconnect
        drop(conn);

        // Reconnect
        self.available.store(false, Ordering::SeqCst);
        self.connect()?;

        let conn = self.connection.lock();
        if conn.is_some() {
            Ok(conn)
        } else {
            Err(connection_not_established())
        }
    }

    /// Send a request and receive a response.
    fn send_request(&self, request: Request) -> Result<Response, DaemonError> {
        let request_id = format!(
            "cass-{}",
            self.request_counter.fetch_add(1, Ordering::Relaxed)
        );
        let msg = FramedMessage::new(&request_id, request);

        let encoded = encode_message(&msg)
            .map_err(|e| DaemonError::Failed(format!("failed to encode request: {}", e)))?;

        let mut stream_guard = self.get_connection_locked()?;
        let stream = stream_guard
            .as_mut()
            .ok_or_else(connection_not_established)?;

        // Send request
        if let Err(e) = stream.write_all(&encoded) {
            *stream_guard = None;
            self.available.store(false, Ordering::SeqCst);
            return Err(DaemonError::Unavailable(format!(
                "failed to send request: {}",
                e
            )));
        }

        // Read length prefix
        let mut len_buf = [0u8; 4];
        if let Err(e) = stream.read_exact(&mut len_buf) {
            *stream_guard = None;
            self.available.store(false, Ordering::SeqCst);
            if e.kind() == std::io::ErrorKind::TimedOut {
                return Err(DaemonError::Timeout("response timeout".to_string()));
            } else {
                return Err(DaemonError::Unavailable(format!(
                    "failed to read response length: {}",
                    e
                )));
            }
        }

        let len = u32::from_be_bytes(len_buf) as usize;
        // 10MB sanity limit - typical embedding responses are well under 1MB
        const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;
        if len > MAX_RESPONSE_SIZE {
            *stream_guard = None;
            warn!(
                response_size = len,
                max_size = MAX_RESPONSE_SIZE,
                "Rejecting oversized daemon response"
            );
            return Err(DaemonError::Failed(format!(
                "response too large: {} bytes (max {})",
                len, MAX_RESPONSE_SIZE
            )));
        }

        // Read response payload
        let mut payload = vec![0u8; len];
        if let Err(e) = stream.read_exact(&mut payload) {
            *stream_guard = None;
            self.available.store(false, Ordering::SeqCst);
            if e.kind() == std::io::ErrorKind::TimedOut {
                return Err(DaemonError::Timeout("response timeout".to_string()));
            } else {
                return Err(DaemonError::Unavailable(format!(
                    "failed to read response: {}",
                    e
                )));
            }
        }

        // Release connection lock before decoding
        drop(stream_guard);

        // Decode response
        let response: FramedMessage<Response> = decode_message(&payload)
            .map_err(|e| DaemonError::Failed(format!("failed to decode response: {}", e)))?;

        // Check version compatibility
        if response.version != PROTOCOL_VERSION {
            return Err(DaemonError::Failed(format!(
                "protocol version mismatch: expected {}, got {}",
                PROTOCOL_VERSION, response.version
            )));
        }

        // Handle error responses
        match response.payload {
            Response::Error(err) => {
                let daemon_err = match err.code {
                    ErrorCode::Overloaded => DaemonError::Overloaded {
                        retry_after: err.retry_after_ms.map(Duration::from_millis),
                        message: err.message,
                    },
                    ErrorCode::Timeout => DaemonError::Timeout(err.message),
                    ErrorCode::InvalidInput => DaemonError::InvalidInput(err.message),
                    _ => DaemonError::Failed(err.message),
                };
                Err(daemon_err)
            }
            other => Ok(other),
        }
    }

    /// Check daemon health.
    pub fn health(&self) -> Result<HealthStatus, DaemonError> {
        match self.send_request(Request::Health)? {
            Response::Health(status) => {
                *self.last_health_check.lock() = Some(Instant::now());
                Ok(status)
            }
            other => Err(unexpected_response(other)),
        }
    }

    /// Request daemon shutdown.
    pub fn shutdown(&self) -> Result<(), DaemonError> {
        match self.send_request(Request::Shutdown)? {
            Response::Shutdown { .. } => {
                self.available.store(false, Ordering::SeqCst);
                *self.connection.lock() = None;
                Ok(())
            }
            other => Err(unexpected_response(other)),
        }
    }

    /// Submit a background embedding job to the daemon.
    pub fn submit_embedding_job(&self, config: EmbeddingJobConfig) -> Result<String, DaemonError> {
        let response = self.send_request(Request::SubmitEmbeddingJob {
            db_path: config.db_path,
            index_path: config.index_path,
            two_tier: config.two_tier,
            fast_model: config.fast_model,
            quality_model: config.quality_model,
        })?;
        match response {
            Response::JobSubmitted { job_id, .. } => Ok(job_id),
            other => Err(unexpected_response(other)),
        }
    }

    /// Query the status of embedding jobs for a database.
    pub fn embedding_job_status(&self, db_path: &str) -> Result<EmbeddingJobInfo, DaemonError> {
        let response = self.send_request(Request::EmbeddingJobStatus {
            db_path: db_path.to_string(),
        })?;
        match response {
            Response::JobStatus(info) => Ok(info),
            other => Err(unexpected_response(other)),
        }
    }

    /// Cancel embedding jobs for a database.
    pub fn cancel_embedding_job(
        &self,
        db_path: &str,
        model_id: Option<&str>,
    ) -> Result<usize, DaemonError> {
        let response = self.send_request(Request::CancelEmbeddingJob {
            db_path: db_path.to_string(),
            model_id: model_id.map(|s| s.to_string()),
        })?;
        match response {
            Response::JobCancelled { cancelled, .. } => Ok(cancelled),
            other => Err(unexpected_response(other)),
        }
    }
}

impl DaemonClient for UdsDaemonClient {
    fn id(&self) -> &str {
        "uds-daemon"
    }

    fn is_available(&self) -> bool {
        // Quick check without reconnect
        if !self.available.load(Ordering::SeqCst) {
            return false;
        }

        // Check if health was recently verified (5 second cache for faster failure detection)
        if let Some(last) = *self.last_health_check.lock()
            && last.elapsed() < Duration::from_secs(5)
        {
            return true;
        }

        // Verify with health check
        match self.health() {
            Ok(status) => status.ready,
            Err(_) => {
                self.available.store(false, Ordering::SeqCst);
                false
            }
        }
    }

    fn embed(&self, text: &str, request_id: &str) -> Result<Vec<f32>, DaemonError> {
        debug!(
            request_id = request_id,
            text_len = text.len(),
            "Daemon embed request"
        );

        let response = self.send_request(Request::Embed {
            texts: vec![text.to_string()],
            model: "default".to_string(),
            dims: None,
        })?;

        match response {
            Response::Embed(embed) => {
                self.validate_embedder_id(&embed.model)?;
                if embed.embeddings.is_empty() {
                    return Err(DaemonError::Failed("no embeddings returned".to_string()));
                }
                debug!(
                    request_id = request_id,
                    elapsed_ms = embed.elapsed_ms,
                    dimension = embed.embeddings[0].len(),
                    "Daemon embed completed"
                );
                // Safety: We've verified embeddings is not empty above
                embed
                    .embeddings
                    .into_iter()
                    .next()
                    .ok_or_else(|| DaemonError::Failed("embedding unexpectedly empty".to_string()))
            }
            other => Err(unexpected_response(other)),
        }
    }

    fn embed_batch(&self, texts: &[&str], request_id: &str) -> Result<Vec<Vec<f32>>, DaemonError> {
        debug!(
            request_id = request_id,
            batch_size = texts.len(),
            "Daemon embed batch request"
        );

        let response = self.send_request(Request::Embed {
            texts: texts.iter().map(|s| s.to_string()).collect(),
            model: "default".to_string(),
            dims: None,
        })?;

        match response {
            Response::Embed(embed) => {
                self.validate_embedder_id(&embed.model)?;
                if embed.embeddings.len() != texts.len() {
                    return Err(DaemonError::Failed(format!(
                        "embedding count mismatch: expected {}, got {}",
                        texts.len(),
                        embed.embeddings.len()
                    )));
                }
                debug!(
                    request_id = request_id,
                    elapsed_ms = embed.elapsed_ms,
                    batch_size = texts.len(),
                    "Daemon embed batch completed"
                );
                Ok(embed.embeddings)
            }
            other => Err(unexpected_response(other)),
        }
    }

    fn rerank(
        &self,
        query: &str,
        documents: &[&str],
        request_id: &str,
    ) -> Result<Vec<f32>, DaemonError> {
        debug!(
            request_id = request_id,
            query_len = query.len(),
            doc_count = documents.len(),
            "Daemon rerank request"
        );

        let response = self.send_request(Request::Rerank {
            query: query.to_string(),
            documents: documents.iter().map(|s| s.to_string()).collect(),
            model: "default".to_string(),
        })?;

        match response {
            Response::Rerank(rerank) => {
                if rerank.scores.len() != documents.len() {
                    return Err(DaemonError::Failed(format!(
                        "score count mismatch: expected {}, got {}",
                        documents.len(),
                        rerank.scores.len()
                    )));
                }
                debug!(
                    request_id = request_id,
                    elapsed_ms = rerank.elapsed_ms,
                    doc_count = documents.len(),
                    "Daemon rerank completed"
                );
                Ok(rerank.scores)
            }
            other => Err(unexpected_response(other)),
        }
    }
}

/// Gather a read-only, deadline-bounded daemon observation for status/doctor.
/// No auto-spawn and no stale-artifact cleanup occurs on this path. A slow
/// connect or liveness request yields a truthful `unresponsive` diagnostic with
/// the already-collected lock/socket metadata still present.
pub fn probe_daemon_runtime(
    data_dir: &Path,
    timeout: Duration,
) -> crate::daemon_runtime_state::DaemonRuntimeDiagnostic {
    let mut config = DaemonClientConfig::from_env();
    config.auto_spawn = false;
    config.connect_timeout = timeout;
    // The caller's receive deadline is the authoritative hard bound. Keep the
    // worker's socket timeout longer so the two clocks cannot race and turn a
    // deadline breach nondeterministically into a ghost-process response.
    config.request_timeout = timeout.saturating_mul(2);
    probe_daemon_runtime_with_config(data_dir, config, timeout)
}

fn probe_daemon_runtime_with_config(
    data_dir: &Path,
    config: DaemonClientConfig,
    timeout: Duration,
) -> crate::daemon_runtime_state::DaemonRuntimeDiagnostic {
    use crate::daemon_runtime_state::{DaemonRuntimeDiagnostic, DaemonRuntimeObservation};

    let socket_path = config.socket_path.clone();
    let run_lock_path = daemon_run_lock_path(&socket_path);
    let socket_present = std::fs::symlink_metadata(&socket_path).is_ok();
    let run_lock_present = std::fs::symlink_metadata(&run_lock_path).is_ok();
    let lock_metadata = read_daemon_run_lock_metadata(&run_lock_path);
    let run_lock_acquirable = probe_run_lock_acquirable(&run_lock_path);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    let mut observation = DaemonRuntimeObservation {
        socket_path: Some(socket_path.display().to_string()),
        data_dir: Some(data_dir.display().to_string()),
        run_lock_present,
        run_lock_acquirable,
        socket_present,
        daemon_generation: lock_metadata.and_then(|metadata| metadata.generation),
        published_generation: published_lexical_generation(data_dir),
        owner_pid: lock_metadata.map(|metadata| metadata.pid),
        last_heartbeat_unix_ms: lock_metadata.map(|metadata| metadata.heartbeat_unix_ms),
        last_heartbeat_age_ms: lock_metadata
            .map(|metadata| now_ms.saturating_sub(metadata.heartbeat_unix_ms)),
        ..Default::default()
    };

    if !socket_present {
        observation.socket_connectable = Some(false);
        return DaemonRuntimeDiagnostic::from_observation(observation);
    }

    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawn = std::thread::Builder::new()
        .name("cass-daemon-runtime-probe".to_string())
        .spawn(move || {
            let client = UdsDaemonClient::new(config);
            let result = match client.connect() {
                Ok(()) => match client.health() {
                    Ok(_) => RuntimeProbeSocketResult {
                        socket_connectable: true,
                        responded_to_ping: true,
                        connect_error: None,
                    },
                    Err(error) => RuntimeProbeSocketResult {
                        socket_connectable: true,
                        responded_to_ping: false,
                        connect_error: Some(error.to_string()),
                    },
                },
                Err(error) => RuntimeProbeSocketResult {
                    socket_connectable: false,
                    responded_to_ping: false,
                    connect_error: Some(error.to_string()),
                },
            };
            let _ = tx.send(result);
        });

    if let Err(error) = spawn {
        observation.probe_incomplete = true;
        observation.connect_error = Some(format!("failed to start bounded daemon probe: {error}"));
        return DaemonRuntimeDiagnostic::from_observation(observation);
    }

    match rx.recv_timeout(timeout) {
        Ok(result) => {
            observation.socket_connectable = Some(result.socket_connectable);
            observation.responded_to_ping = result
                .socket_connectable
                .then_some(result.responded_to_ping);
            observation.connect_error = result.connect_error;
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            observation.socket_connectable = None;
            observation.responded_to_ping = None;
            observation.connect_timed_out = true;
            observation.connect_error = Some(format!(
                "daemon liveness probe exceeded {} ms",
                timeout.as_millis()
            ));
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            observation.probe_incomplete = true;
            observation.connect_error =
                Some("daemon liveness probe worker disconnected".to_string());
        }
    }

    DaemonRuntimeDiagnostic::from_observation(observation)
}

fn read_daemon_run_lock_metadata(path: &Path) -> Option<DaemonRunLockMetadata> {
    const MAX_RUN_LOCK_METADATA_BYTES: u64 = 4 * 1024;

    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_RUN_LOCK_METADATA_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(path)
        .ok()?
        .take(MAX_RUN_LOCK_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_RUN_LOCK_METADATA_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn probe_run_lock_acquirable(path: &Path) -> Option<bool> {
    if std::fs::symlink_metadata(path)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    let file = std::fs::OpenOptions::new().read(true).open(path).ok()?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            Some(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Some(false),
        Err(_) => None,
    }
}

fn remove_stale_daemon_socket(socket_path: &std::path::Path) -> Result<(), DaemonError> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() || metadata.file_type().is_symlink() => {
            std::fs::remove_file(socket_path).map_err(|error| {
                DaemonError::Unavailable(format!(
                    "failed to remove stale daemon socket {}: {}",
                    socket_path.display(),
                    error
                ))
            })
        }
        Ok(metadata) => Err(DaemonError::Unavailable(format!(
            "refusing to remove non-socket daemon path {} (file type: {:?})",
            socket_path.display(),
            metadata.file_type()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DaemonError::Unavailable(format!(
            "failed to inspect daemon socket path {}: {}",
            socket_path.display(),
            error
        ))),
    }
}

/// Connect to an existing daemon or spawn a new one.
pub fn connect_or_spawn() -> Result<Arc<UdsDaemonClient>, DaemonError> {
    let client = UdsDaemonClient::with_defaults();
    client.connect()?;
    Ok(Arc::new(client))
}

/// Connect to an existing daemon or spawn one, requiring embeddings from the
/// model that owns the caller's vector index.
pub fn connect_or_spawn_for_embedder(
    expected_embedder_id: &str,
) -> Result<Arc<UdsDaemonClient>, DaemonError> {
    let mut config = DaemonClientConfig::from_env();
    config.expected_embedder_id = Some(expected_embedder_id.to_string());
    let client = UdsDaemonClient::new(config);
    client.connect()?;
    Ok(Arc::new(client))
}

/// Try to connect to an existing daemon without spawning.
pub fn try_connect() -> Option<Arc<UdsDaemonClient>> {
    let mut config = DaemonClientConfig::from_env();
    config.auto_spawn = false;
    let client = UdsDaemonClient::new(config);
    match client.connect() {
        Ok(()) => Some(Arc::new(client)),
        Err(_) => None,
    }
}

/// Try an existing daemon without spawning and require embeddings from the
/// model that owns the caller's vector index.
pub fn try_connect_for_embedder(expected_embedder_id: &str) -> Option<Arc<UdsDaemonClient>> {
    let mut config = DaemonClientConfig::from_env();
    config.auto_spawn = false;
    config.expected_embedder_id = Some(expected_embedder_id.to_string());
    let client = UdsDaemonClient::new(config);
    match client.connect() {
        Ok(()) => Some(Arc::new(client)),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_lock_metadata(path: &Path, generation: Option<u64>) {
        let metadata = DaemonRunLockMetadata {
            pid: std::process::id(),
            heartbeat_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_millis() as u64,
            generation,
        };
        std::fs::write(
            path,
            serde_json::to_vec(&metadata).expect("serialize lock metadata"),
        )
        .expect("write lock metadata");
    }

    fn serve_one_health(listener: std::os::unix::net::UnixListener) {
        let (mut stream, _) = listener.accept().expect("accept probe");
        let mut len = [0_u8; 4];
        stream.read_exact(&mut len).expect("read request length");
        let mut body = vec![0_u8; u32::from_be_bytes(len) as usize];
        stream.read_exact(&mut body).expect("read request body");
        let request = decode_message::<Request>(&body).expect("decode request");
        assert!(matches!(request.payload, Request::Health));
        let response = FramedMessage::new(
            request.request_id,
            Response::Health(HealthStatus {
                uptime_secs: 1,
                version: PROTOCOL_VERSION,
                ready: true,
                memory_bytes: 0,
            }),
        );
        stream
            .write_all(&encode_message(&response).expect("encode response"))
            .expect("write response");
    }

    #[test]
    fn b7tb0_runtime_probe_classifies_stale_socket_then_live_restart_without_archive_mutation() {
        use crate::daemon_runtime_state::DaemonRuntimeState;

        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("semantic.sock");
        let lock_path = daemon_run_lock_path(&socket_path);
        let stale_listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("bind stale socket fixture");
        drop(stale_listener);
        write_test_lock_metadata(&lock_path, Some(7));

        let config = DaemonClientConfig {
            socket_path: socket_path.clone(),
            auto_spawn: false,
            request_timeout: Duration::from_millis(100),
            ..Default::default()
        };
        let stale = probe_daemon_runtime_with_config(
            dir.path(),
            config.clone(),
            Duration::from_millis(100),
        );
        assert_eq!(stale.state, DaemonRuntimeState::StaleSocket);
        assert!(stale.recovery.disposable_runtime_artifact);
        assert_eq!(stale.observation.owner_pid, Some(std::process::id()));
        assert!(stale.observation.last_heartbeat_unix_ms.is_some());

        remove_stale_daemon_socket(&socket_path).expect("reclaim stale runtime socket");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("bind restarted daemon fixture");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open daemon run lock");
        lock.try_lock_exclusive()
            .expect("hold restarted daemon lock");
        let server = std::thread::spawn(move || serve_one_health(listener));

        let restarted =
            probe_daemon_runtime_with_config(dir.path(), config, Duration::from_millis(250));
        server.join().expect("health responder");
        assert_eq!(restarted.state, DaemonRuntimeState::Ok);
        assert_eq!(restarted.observation.responded_to_ping, Some(true));
        assert!(!restarted.recovery.action_needed);
    }

    #[test]
    fn b7tb0_runtime_probe_returns_partial_unresponsive_diagnostic_at_deadline() {
        use crate::daemon_runtime_state::DaemonRuntimeState;

        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("slow.sock");
        let lock_path = daemon_run_lock_path(&socket_path);
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind slow daemon fixture");
        write_test_lock_metadata(&lock_path, None);
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open slow daemon lock");
        lock.try_lock_exclusive().expect("hold slow daemon lock");
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let accepted = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("accept slow probe");
            release_rx.recv().expect("release slow responder");
        });
        let timeout = Duration::from_millis(20);
        let started = Instant::now();
        let diagnostic = probe_daemon_runtime_with_config(
            dir.path(),
            DaemonClientConfig {
                socket_path,
                auto_spawn: false,
                request_timeout: Duration::from_millis(250),
                ..Default::default()
            },
            timeout,
        );
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(diagnostic.state, DaemonRuntimeState::Unresponsive);
        assert!(diagnostic.observation.connect_timed_out);
        assert!(diagnostic.observation.owner_pid.is_some());
        release_tx.send(()).expect("release slow responder");
        accepted.join().expect("slow responder");
    }

    #[test]
    fn test_config_defaults() {
        let config = DaemonClientConfig::default();
        assert!(config.auto_spawn);
        assert_eq!(config.connect_timeout, Duration::from_secs(2));
        assert_eq!(config.request_timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_default_socket_path() {
        let config = DaemonClientConfig::default();
        assert_eq!(
            config.socket_path.parent(),
            Some(std::env::temp_dir().as_path())
        );
        let file_name = config
            .socket_path
            .file_name()
            .expect("default socket path has a filename")
            .to_string_lossy();
        assert!(file_name.starts_with("semantic-daemon-"));
        assert!(file_name.ends_with(".sock"));
    }

    #[test]
    fn test_client_not_available_initially() {
        let config = DaemonClientConfig {
            auto_spawn: false,
            socket_path: PathBuf::from("/tmp/nonexistent-test-socket.sock"),
            ..Default::default()
        };

        let client = UdsDaemonClient::new(config);
        assert!(!client.is_available());
    }

    #[test]
    fn issue_347_embedding_response_model_must_match_the_callers_vector_index() {
        let client = UdsDaemonClient::new(DaemonClientConfig {
            expected_embedder_id: Some("minilm-384".to_string()),
            ..Default::default()
        });

        assert!(client.validate_embedder_id("minilm-384").is_ok());
        let error = client
            .validate_embedder_id("hash-384")
            .expect_err("a hash vector must never enter the MiniLM index");
        assert!(matches!(error, DaemonError::InvalidInput(_)));
        assert!(error.to_string().contains("expected minilm-384"));
        assert!(error.to_string().contains("received hash-384"));
    }

    #[test]
    fn test_request_counter_increments() {
        let client = UdsDaemonClient::with_defaults();
        let first = client.request_counter.fetch_add(1, Ordering::Relaxed);
        let second = client.request_counter.fetch_add(1, Ordering::Relaxed);
        assert_eq!(second, first + 1);
    }

    #[test]
    fn connection_not_established_error_text_is_stable() {
        assert_eq!(
            connection_not_established().to_string(),
            "daemon unavailable: connection not established"
        );
    }

    #[test]
    fn unexpected_response_error_text_is_stable() {
        assert_eq!(
            unexpected_response(Response::Shutdown {
                message: "bye".to_string()
            })
            .to_string(),
            "daemon failed: unexpected response: Shutdown { message: \"bye\" }"
        );
    }

    #[test]
    fn test_spawn_guard_lock_path_is_distinct_from_run_lock() {
        let socket = PathBuf::from("/tmp/cass-semantic.sock");
        assert_ne!(
            crate::daemon::daemon_spawn_guard_lock_path(&socket),
            crate::daemon::daemon_run_lock_path(&socket)
        );
        assert_eq!(
            crate::daemon::daemon_spawn_guard_lock_path(&socket),
            PathBuf::from("/tmp/cass-semantic.spawn-guard.lock")
        );
    }

    #[test]
    fn stale_socket_cleanup_refuses_to_remove_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("cass-daemon.sock");
        std::fs::write(&socket_path, b"not a socket").expect("write regular file");

        let err = remove_stale_daemon_socket(&socket_path)
            .expect_err("regular files must not be removed as stale sockets");

        assert!(
            socket_path.exists(),
            "regular file at daemon socket path must be preserved"
        );
        let message = err.to_string();
        assert!(
            message.contains("refusing to remove non-socket daemon path"),
            "error should explain the protected path type; got {message:?}"
        );
    }

    #[test]
    fn stale_socket_cleanup_removes_public_socket_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("cass-daemon.sock");
        let stale_private_socket = dir.path().join(".cass-daemon.sock.runtime/daemon.sock");
        std::os::unix::fs::symlink(&stale_private_socket, &socket_path)
            .expect("create stale daemon public symlink");

        remove_stale_daemon_socket(&socket_path).expect("stale public symlink is removable");

        assert!(
            !socket_path.exists(),
            "stale daemon public symlink should be removed before auto-spawn"
        );
    }
}
