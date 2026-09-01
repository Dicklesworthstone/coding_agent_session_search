//! Daemon server core for the semantic model daemon.
//!
//! This module provides the server that listens on a Unix Domain Socket
//! and handles embedding/reranking requests using loaded models.

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use frankensearch::{
    AttestedDaemonEmbeddingResponseV1, DAEMON_CONNECTION_IDENTITY_SCHEMA_V1, DaemonChallengeV1,
    DaemonConnectionIdentityV1, DaemonEmbeddingAttestationV1, DaemonError, DaemonOperationV1,
};
use fs2::FileExt;
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
use super::{
    DAEMON_ATTESTATION_PROTOCOL_REVISION, DaemonAttestationKeyV1,
    current_daemon_executable_fingerprint, daemon_socket_endpoint_fingerprint,
    daemon_socket_path_for_data_dir, initialize_daemon_attestation_authority,
};
use super::{DaemonRunLockMetadata, daemon_run_lock_path};

struct BoundDaemonSocket {
    listener: UnixListener,
    public_path: PathBuf,
    bind_path: PathBuf,
}

struct DaemonAttestationState {
    connection: DaemonConnectionIdentityV1,
    authority: DaemonAttestationKeyV1,
}

impl DaemonAttestationState {
    fn validate_challenge_for_inputs(
        &self,
        challenge: &DaemonChallengeV1,
        operation: DaemonOperationV1,
        inputs: &[&str],
    ) -> Result<(), DaemonError> {
        let expected = DaemonChallengeV1::for_inputs(
            challenge.request_nonce.clone(),
            operation,
            inputs,
            &self.connection,
        )?;
        if &expected == challenge {
            Ok(())
        } else {
            Err(DaemonError::UnverifiableRemoteSpace)
        }
    }

    fn sign_control(
        &self,
        challenge: &DaemonChallengeV1,
        operation: DaemonOperationV1,
    ) -> Result<DaemonEmbeddingAttestationV1, DaemonError> {
        self.validate_challenge_for_inputs(challenge, operation, &[])?;
        let mut attestation = DaemonEmbeddingAttestationV1::unsigned(
            challenge.clone(),
            self.connection.clone(),
            &[],
        )?;
        attestation.sign_hmac_sha256(self.authority.secret())?;
        Ok(attestation)
    }

    fn sign_vectors(
        &self,
        challenge: &DaemonChallengeV1,
        operation: DaemonOperationV1,
        inputs: &[&str],
        vectors: Vec<Vec<f32>>,
    ) -> Result<AttestedDaemonEmbeddingResponseV1, DaemonError> {
        self.validate_challenge_for_inputs(challenge, operation, inputs)?;
        AttestedDaemonEmbeddingResponseV1::signed(
            challenge.clone(),
            self.connection.clone(),
            vectors,
            self.authority.secret(),
        )
    }
}

fn attestation_unavailable_response() -> Response {
    Response::Error(ErrorResponse {
        code: ErrorCode::ModelLoadFailed,
        message: "producer-attested daemon channel is unavailable".to_string(),
        retryable: false,
        retry_after_ms: None,
    })
}

fn rejected_attestation_response() -> Response {
    Response::Error(ErrorResponse {
        code: ErrorCode::InvalidInput,
        message: "daemon attestation request failed verification".to_string(),
        retryable: false,
        retry_after_ms: None,
    })
}

fn protocol_version_mismatch_message(actual_version: u32) -> String {
    format!(
        "protocol version mismatch: expected {}, got {}",
        PROTOCOL_VERSION, actual_version
    )
}

fn create_owner_only_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path)?;

    // MUST verify the path is a real directory and not a symlink.
    // This prevents symlink attacks in shared parents (e.g. /tmp) where
    // an attacker creates a symlink that DirBuilder happily traverses.
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "path exists but is not a regular directory: {}",
                path.display()
            ),
        ));
    }

    // Only apply chmod if permissions are too loose. This minimizes the TOCTOU window
    // since newly created directories will already have correct permissions.
    if meta.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn parent_dir_is_owner_only(path: &Path) -> std::io::Result<bool> {
    let Some(parent) = path.parent() else {
        return Ok(false);
    };

    // Follow a symlinked parent (notably macOS `/tmp` -> `/private/tmp`) when
    // classifying the directory. The socket itself is still handled with
    // `symlink_metadata`, and public parents still route through a freshly
    // created 0700 private runtime directory below.
    let metadata = fs::metadata(parent)?;
    if !metadata.is_dir() {
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
    /// Lexical generation visible when this daemon started. Advisory runtime
    /// metadata only, used to diagnose stale searchers after atomic publish.
    pub served_generation: Option<u64>,
    /// While resident, spawn a detached low-priority incremental
    /// `cass index --background` every this often (0 = never). The spawn goes
    /// through `indexer::background_refresh`, so the normal index lock,
    /// cooldown, and `CASS_AUTO_REFRESH=0` opt-out all apply, and it is
    /// skipped while the machine is under severe load.
    pub index_interval: Duration,
    /// Data dir the periodic index targets (`None` = platform default).
    pub data_dir: Option<PathBuf>,
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
            served_generation: None,
            index_interval: Duration::from_secs(0), // Off unless configured
            data_dir: None,
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

        if let Ok(val) = dotenvy::var("CASS_DAEMON_INDEX_INTERVAL_SECS")
            && let Ok(secs) = val.trim().parse::<u64>()
        {
            cfg.index_interval = Duration::from_secs(secs);
        }

        cfg
    }
}

/// Decide whether the resident daemon should kick a periodic index refresh on
/// this tick. Pure so it is testable without a socket.
pub(crate) fn periodic_index_due(
    interval: Duration,
    last_spawn: Option<Instant>,
    now: Instant,
) -> bool {
    if interval.is_zero() {
        return false;
    }
    match last_spawn {
        None => true,
        Some(last) => now.saturating_duration_since(last) >= interval,
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
    attestation: RwLock<Option<DaemonAttestationState>>,
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
            attestation: RwLock::new(None),
            worker_handle: parking_lot::Mutex::new(None),
        }
    }

    /// Spawn (or skip, with a reason) one detached incremental index pass.
    fn spawn_periodic_index(&self) {
        use crate::indexer::{background_refresh, responsiveness};

        let pressure = responsiveness::machine_pressure_now();
        if pressure.severe {
            info!(
                load_per_core = ?pressure.load_per_core,
                psi = ?pressure.psi_cpu_some_avg10,
                "daemon periodic index skipped: machine under severe load"
            );
            return;
        }
        let data_dir = self
            .config
            .data_dir
            .clone()
            .unwrap_or_else(crate::default_data_dir);
        let db_path = data_dir.join("agent_search.db");
        let outcome = background_refresh::maybe_spawn_background_index_refresh(
            &data_dir,
            &db_path,
            "daemon-periodic",
        );
        info!(?outcome, "daemon periodic index evaluated");
    }

    /// Create daemon with default config and models from data directory.
    pub fn with_defaults(data_dir: &Path) -> Self {
        let mut config = DaemonConfig::from_env();
        if dotenvy::var("CASS_DAEMON_SOCKET").is_err() {
            config.socket_path = daemon_socket_path_for_data_dir(data_dir);
        }
        config.data_dir = Some(data_dir.to_path_buf());
        let models = ModelManager::new(data_dir);
        Self::new(config, models)
    }

    fn initialize_attestation(&self) -> std::io::Result<()> {
        let data_dir = self.config.data_dir.as_deref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "daemon data directory is required for producer attestation",
            )
        })?;
        let (authority, generation) = initialize_daemon_attestation_authority(data_dir)?;
        let (embedding_identity, model_category) = self
            .models
            .embedder_attestation_identity()
            .map_err(std::io::Error::other)?;
        let connection = DaemonConnectionIdentityV1 {
            schema_version: DAEMON_CONNECTION_IDENTITY_SCHEMA_V1,
            endpoint_fingerprint: daemon_socket_endpoint_fingerprint(&self.config.socket_path),
            executable_fingerprint: current_daemon_executable_fingerprint()?,
            protocol_revision: DAEMON_ATTESTATION_PROTOCOL_REVISION.to_string(),
            key_id: authority.key_id().to_string(),
            generation,
            embedding_identity,
            model_category,
        };
        connection.validate().map_err(std::io::Error::other)?;
        *self.attestation.write() = Some(DaemonAttestationState {
            connection,
            authority,
        });
        Ok(())
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

        let mut create_options = std::fs::OpenOptions::new();
        create_options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let mut lock_file = match create_options.open(&lock_path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut existing_options = std::fs::OpenOptions::new();
                existing_options
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW);
                existing_options.open(&lock_path)?
            }
            Err(e) => return Err(e),
        };
        let lock_metadata = lock_file.metadata()?;
        if !lock_metadata.file_type().is_file() || lock_metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refusing to use a non-regular or multiply-linked daemon run lock",
            ));
        }

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

        write_daemon_run_lock_metadata(&mut lock_file, self.config.served_generation)?;
        let mut last_lock_heartbeat = Instant::now();
        // Periodic index refresh: `None` means "never spawned yet", so the
        // first tick after startup fires (the daemon was auto-spawned by a
        // search, which is exactly when a stale index hurts).
        let mut last_index_spawn: Option<Instant> = None;
        let mut last_index_check = Instant::now();

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
        if let Err(error) = self.initialize_attestation() {
            warn!(error = %error, "Producer-attested daemon channel is unavailable");
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

                // Refresh independently of socket idleness. A busy daemon may
                // accept continuously, but its heartbeat must still reflect a
                // live owner rather than looking stale under load.
                if last_lock_heartbeat.elapsed() >= Duration::from_secs(1) {
                    if let Err(error) = write_daemon_run_lock_metadata(
                        &mut lock_file,
                        self.config.served_generation,
                    ) {
                        warn!(error = %error, "Failed to refresh daemon run-lock heartbeat");
                    } else {
                        last_lock_heartbeat = Instant::now();
                    }
                }

                // Periodic background index (Layer 3 of "keep the index
                // fresh"): checked at most once a second; the actual spawn is
                // a detached `cass index --background` child so the daemon's
                // own memory/latency profile is untouched.
                if last_index_check.elapsed() >= Duration::from_secs(1) {
                    last_index_check = Instant::now();
                    if periodic_index_due(
                        self.config.index_interval,
                        last_index_spawn,
                        Instant::now(),
                    ) {
                        // Record the attempt regardless of outcome so a
                        // cooldown/lock skip does not turn into a 1 Hz retry.
                        last_index_spawn = Some(Instant::now());
                        self.spawn_periodic_index();
                    }
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
        let worker_handle = self.worker_handle.lock().take();
        if let Some(handle) = worker_handle
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
                    if msg.version != PROTOCOL_VERSION {
                        warn!(
                            request_version = msg.version,
                            expected_version = PROTOCOL_VERSION,
                            request_id = %msg.request_id,
                            "Rejected daemon request with incompatible protocol version"
                        );
                        FramedMessage::new(
                            msg.request_id,
                            Response::Error(ErrorResponse {
                                code: ErrorCode::VersionMismatch,
                                message: protocol_version_mismatch_message(msg.version),
                                retryable: false,
                                retry_after_ms: None,
                            }),
                        )
                    } else {
                        self.total_requests.fetch_add(1, Ordering::Relaxed);
                        self.touch_activity();
                        let response = self.handle_request(msg.request_id.clone(), msg.payload);
                        FramedMessage::new(msg.request_id, response)
                    }
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

            Request::ConnectionIdentity => self
                .attestation
                .read()
                .as_ref()
                .map(|state| Response::ConnectionIdentity(state.connection.clone()))
                .unwrap_or_else(attestation_unavailable_response),

            Request::HandshakeAttested { challenge } => {
                let state = self.attestation.read();
                match state.as_ref() {
                    Some(state) => state
                        .sign_control(&challenge, DaemonOperationV1::Handshake)
                        .map(Response::Attestation)
                        .unwrap_or_else(|_| rejected_attestation_response()),
                    None => attestation_unavailable_response(),
                }
            }

            Request::HealthAttested { challenge } => {
                let state = self.attestation.read();
                if !self.models.is_ready() {
                    return attestation_unavailable_response();
                }
                match state.as_ref() {
                    Some(state) => state
                        .sign_control(&challenge, DaemonOperationV1::Health)
                        .map(Response::Attestation)
                        .unwrap_or_else(|_| rejected_attestation_response()),
                    None => attestation_unavailable_response(),
                }
            }

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

            Request::EmbedAttested {
                texts,
                model,
                dims: _,
                challenge,
            } => {
                let operation = if texts.len() == 1 {
                    DaemonOperationV1::Embed
                } else {
                    DaemonOperationV1::EmbedBatch
                };
                let inputs: Vec<&str> = texts.iter().map(String::as_str).collect();
                let state = self.attestation.read();
                let Some(state) = state.as_ref() else {
                    return attestation_unavailable_response();
                };
                if state
                    .validate_challenge_for_inputs(&challenge, operation, &inputs)
                    .is_err()
                {
                    return rejected_attestation_response();
                }
                debug!(
                    request_id = %request_id,
                    batch_size = texts.len(),
                    model = %model,
                    "Processing attested embed request"
                );
                match self.models.embed_batch(&texts) {
                    Ok(embeddings) => state
                        .sign_vectors(&challenge, operation, &inputs, embeddings)
                        .map(Response::AttestedEmbedding)
                        .unwrap_or_else(|_| rejected_attestation_response()),
                    Err(error) => Response::Error(ErrorResponse {
                        code: ErrorCode::ModelLoadFailed,
                        message: error.to_string(),
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

            Request::RerankAttested {
                query,
                documents,
                model,
                challenge,
            } => {
                let mut inputs = Vec::with_capacity(documents.len() + 1);
                inputs.push(query.as_str());
                inputs.extend(documents.iter().map(String::as_str));
                let state = self.attestation.read();
                let Some(state) = state.as_ref() else {
                    return attestation_unavailable_response();
                };
                if state
                    .validate_challenge_for_inputs(&challenge, DaemonOperationV1::Rerank, &inputs)
                    .is_err()
                {
                    return rejected_attestation_response();
                }
                debug!(
                    request_id = %request_id,
                    doc_count = documents.len(),
                    model = %model,
                    "Processing attested rerank request"
                );
                match self.models.rerank(&query, &documents) {
                    Ok(scores) => state
                        .sign_vectors(&challenge, DaemonOperationV1::Rerank, &inputs, vec![scores])
                        .map(Response::AttestedEmbedding)
                        .unwrap_or_else(|_| rejected_attestation_response()),
                    Err(error) => Response::Error(ErrorResponse {
                        code: ErrorCode::ModelLoadFailed,
                        message: error.to_string(),
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
                let worker_handle = self.worker_handle.lock().clone();
                match worker_handle {
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
                let worker_handle = self.worker_handle.lock().clone();
                if let Some(handle) = worker_handle
                    && let Err(e) = handle.cancel(db_path.clone(), model_id.clone())
                {
                    warn!(error = %e, "Failed to send cancel to embedding worker");
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

fn write_daemon_run_lock_metadata(
    lock_file: &mut std::fs::File,
    generation: Option<u64>,
) -> std::io::Result<()> {
    let file_metadata = lock_file.metadata()?;
    if !file_metadata.file_type().is_file() || file_metadata.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to write daemon metadata through a non-regular or multiply-linked run lock",
        ));
    }
    let heartbeat_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    let metadata = DaemonRunLockMetadata {
        pid: std::process::id(),
        heartbeat_unix_ms,
        generation,
    };
    let encoded = serde_json::to_vec(&metadata).map_err(std::io::Error::other)?;
    lock_file.seek(SeekFrom::Start(0))?;
    lock_file.write_all(&encoded)?;
    lock_file.set_len(encoded.len() as u64)?;
    lock_file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankensearch::{Embedder as _, HashAlgorithm, HashEmbedder, ModelCategory};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn test_attestation_state(data_dir: &Path) -> DaemonAttestationState {
        let (authority, generation) =
            initialize_daemon_attestation_authority(data_dir).expect("test authority");
        let embedder = HashEmbedder::new(3, HashAlgorithm::FnvModular);
        let connection = DaemonConnectionIdentityV1 {
            schema_version: DAEMON_CONNECTION_IDENTITY_SCHEMA_V1,
            endpoint_fingerprint: "11".repeat(32),
            executable_fingerprint: "22".repeat(32),
            protocol_revision: DAEMON_ATTESTATION_PROTOCOL_REVISION.to_string(),
            key_id: authority.key_id().to_string(),
            generation,
            embedding_identity: embedder.identity().expect("hash identity").clone(),
            model_category: ModelCategory::HashEmbedder,
        };
        DaemonAttestationState {
            connection,
            authority,
        }
    }

    #[test]
    fn attestation_signer_binds_nonce_operation_inputs_identity_and_vectors() {
        let temp = TempDir::new().expect("tempdir");
        let state = test_attestation_state(temp.path());
        let challenge = DaemonChallengeV1::for_inputs(
            "aa".repeat(32),
            DaemonOperationV1::Embed,
            &["original input"],
            &state.connection,
        )
        .expect("challenge");

        assert!(
            state
                .validate_challenge_for_inputs(
                    &challenge,
                    DaemonOperationV1::Embed,
                    &["original input"],
                )
                .is_ok()
        );
        assert!(
            state
                .validate_challenge_for_inputs(
                    &challenge,
                    DaemonOperationV1::Embed,
                    &["substituted input"],
                )
                .is_err(),
            "the daemon must not sign a challenge for different input bytes"
        );

        let signed = state
            .sign_vectors(
                &challenge,
                DaemonOperationV1::Embed,
                &["original input"],
                vec![vec![0.25, 0.5, 0.75]],
            )
            .expect("signed response");
        signed
            .attestation
            .validate_against(&challenge, &state.connection, &signed.vectors)
            .expect("bound response");
        signed
            .attestation
            .authenticate_hmac_sha256(state.authority.secret())
            .expect("authenticated response");
    }

    #[test]
    fn periodic_index_is_off_at_zero_and_fires_on_first_tick_then_per_interval() {
        let now = Instant::now();
        assert!(!periodic_index_due(Duration::ZERO, None, now));
        assert!(periodic_index_due(Duration::from_secs(900), None, now));
        assert!(!periodic_index_due(
            Duration::from_secs(900),
            Some(now),
            now + Duration::from_secs(899)
        ));
        assert!(periodic_index_due(
            Duration::from_secs(900),
            Some(now),
            now + Duration::from_secs(900)
        ));
    }

    #[test]
    fn daemon_config_defaults_leave_periodic_index_off() {
        let cfg = DaemonConfig::default();
        assert!(cfg.index_interval.is_zero());
        assert!(cfg.data_dir.is_none());
    }

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

    /// Regression for #346: on macOS `/tmp` is a symlink to `/private/tmp`,
    /// and the daemon refused to start with "socket parent is not a
    /// directory: /tmp" because the parent check used symlink (lstat)
    /// semantics. The classifier must follow a symlinked parent and the full
    /// bind flow must succeed for a socket whose parent is a symlink to a
    /// world-writable directory (routing through the private runtime dir).
    #[test]
    fn test_bind_follows_symlinked_socket_parent() {
        let tmp = TempDir::new().expect("tempdir");
        // real_tmp plays /private/tmp: a real, world-writable directory.
        let real_tmp = tmp.path().join("private").join("tmp");
        fs::create_dir_all(&real_tmp).expect("create real tmp");
        fs::set_permissions(&real_tmp, fs::Permissions::from_mode(0o777)).expect("chmod real tmp");
        // link_tmp plays /tmp: a symlink to the real directory.
        let link_tmp = tmp.path().join("tmp");
        std::os::unix::fs::symlink(&real_tmp, &link_tmp).expect("symlink tmp");

        let socket_path = link_tmp.join("cass-semantic.sock");

        // The parent classifier must follow the symlink instead of erroring
        // with InvalidInput ("socket parent is not a directory").
        let owner_only = parent_dir_is_owner_only(&socket_path)
            .expect("symlinked parent must classify, not error (#346)");
        // A 0o777 parent is shared, so the daemon must route through the
        // private runtime directory rather than bind directly.
        assert!(!owner_only, "world-writable parent must not be owner-only");

        let bound = bind_owner_only_unix_listener(&socket_path)
            .expect("daemon bind must succeed through a symlinked /tmp (#346)");
        assert_eq!(bound.public_path, socket_path);
        assert_ne!(
            bound.bind_path, socket_path,
            "shared parent must route to the private runtime dir"
        );
        // The public path is a symlink to the private-runtime socket.
        let public_meta = fs::symlink_metadata(&socket_path).expect("public socket path exists");
        assert!(public_meta.file_type().is_symlink());
        cleanup_bound_socket(&bound.public_path, &bound.bind_path);
    }

    /// Complement to the #346 regression: an owner-only (0o700) symlinked
    /// parent binds the socket directly at the requested path.
    #[test]
    fn test_bind_symlinked_owner_only_parent_binds_directly() {
        let tmp = TempDir::new().expect("tempdir");
        let real_dir = tmp.path().join("real-private");
        fs::create_dir_all(&real_dir).expect("create dir");
        fs::set_permissions(&real_dir, fs::Permissions::from_mode(0o700)).expect("chmod");
        let link_dir = tmp.path().join("linked-private");
        std::os::unix::fs::symlink(&real_dir, &link_dir).expect("symlink");

        let socket_path = link_dir.join("daemon.sock");
        assert!(
            parent_dir_is_owner_only(&socket_path).expect("owner-only symlinked parent classifies")
        );
        let bound = bind_owner_only_unix_listener(&socket_path)
            .expect("bind through owner-only symlinked parent");
        assert_eq!(bound.bind_path, socket_path);
        cleanup_bound_socket(&bound.public_path, &bound.bind_path);
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
    fn incompatible_protocol_shutdown_is_rejected_without_side_effects()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = DaemonConfig {
            request_timeout: Duration::from_secs(1),
            ..Default::default()
        };
        let models = ModelManager::new(&test_data_dir());
        let daemon = Arc::new(ModelDaemon::new(config, models));
        let (server_stream, mut client_stream) = UnixStream::pair()?;

        let handler_daemon = Arc::clone(&daemon);
        let handler = std::thread::spawn(move || handler_daemon.handle_connection(server_stream));

        let request = FramedMessage {
            version: PROTOCOL_VERSION + 1,
            request_id: "future-shutdown".to_string(),
            payload: Request::Shutdown,
        };
        let encoded = encode_message(&request)?;
        client_stream.write_all(&encoded)?;

        let mut len = [0_u8; 4];
        client_stream.read_exact(&mut len)?;
        let mut payload = vec![0_u8; u32::from_be_bytes(len) as usize];
        client_stream.read_exact(&mut payload)?;
        let response = decode_message::<Response>(&payload)?;

        drop(client_stream);
        handler
            .join()
            .map_err(|_| std::io::Error::other("handler thread panicked"))??;

        if response.version != PROTOCOL_VERSION {
            return Err(std::io::Error::other("response used the wrong protocol version").into());
        }
        if response.request_id != "future-shutdown" {
            return Err(std::io::Error::other("response used the wrong request ID").into());
        }
        let error = match response.payload {
            Response::Error(error) => error,
            _ => {
                return Err(
                    std::io::Error::other("version mismatch did not return an error").into(),
                );
            }
        };
        if error.code != ErrorCode::VersionMismatch || error.retryable {
            return Err(std::io::Error::other("version mismatch error metadata was wrong").into());
        }
        let expected_message = format!(
            "expected {}, got {}",
            PROTOCOL_VERSION,
            PROTOCOL_VERSION + 1
        );
        if !error.message.contains(&expected_message) {
            return Err(std::io::Error::other("version mismatch error lacked context").into());
        }
        if daemon.shutdown.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("incompatible shutdown changed daemon state").into());
        }
        if daemon.total_requests.load(Ordering::Relaxed) != 0 {
            return Err(
                std::io::Error::other("incompatible request changed the request count").into(),
            );
        }
        Ok(())
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

    #[test]
    fn test_owner_only_bind_accepts_symlinked_public_parent() {
        let temp_dir = TempDir::new().unwrap();
        // Keep names short: RCH's worker-side target path is already long and
        // Unix-domain socket paths have a platform SUN_LEN ceiling.
        let real_public_dir = temp_dir.path().join("r");
        fs::create_dir(&real_public_dir).unwrap();
        fs::set_permissions(&real_public_dir, fs::Permissions::from_mode(0o777)).unwrap();
        let linked_public_dir = temp_dir.path().join("l");
        std::os::unix::fs::symlink(&real_public_dir, &linked_public_dir).unwrap();
        let public_socket = linked_public_dir.join("s");

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
                .is_symlink(),
            "a socket under a symlinked public temp directory should point at its private runtime socket"
        );
        assert_eq!(
            fs::symlink_metadata(bind_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
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
