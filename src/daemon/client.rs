//! Daemon client for connecting to the semantic model daemon.
//!
//! This client connects via Unix Domain Socket and provides methods for
//! embedding and reranking. It implements the `DaemonClient` trait from
//! `search::daemon_client` for integration with the fallback wrappers.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::{debug, info};

use super::protocol::{
    ErrorCode, FramedMessage, HealthStatus, PROTOCOL_VERSION, Request, Response, decode_message,
    default_socket_path, encode_message,
};
use crate::search::daemon_client::{DaemonClient, DaemonError};

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
}

impl Default for DaemonClientConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(30),
            auto_spawn: true,
            daemon_binary: None, // Will use current executable with --daemon flag
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

        // Remove existing socket if present
        if self.config.socket_path.exists() {
            let _ = std::fs::remove_file(&self.config.socket_path);
        }

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
            Ok(child) => {
                info!(
                    pid = child.id(),
                    binary = %binary.display(),
                    socket = %self.config.socket_path.display(),
                    "Spawned daemon process"
                );
                Ok(())
            }
            Err(e) => Err(DaemonError::Unavailable(format!(
                "failed to spawn daemon: {}",
                e
            ))),
        }
    }

    /// Get a fresh connection, reconnecting if needed.
    fn get_connection(&self) -> Result<UnixStream, DaemonError> {
        // Try to use existing connection
        {
            let mut conn = self.connection.lock();
            if let Some(stream) = conn.take() {
                // Check if connection is still valid
                if stream.peer_addr().is_ok() {
                    // Clone once: put clone back in cache, return original
                    let clone = stream.try_clone().map_err(|e| {
                        DaemonError::Unavailable(format!("connection clone failed: {}", e))
                    })?;
                    *conn = Some(clone);
                    return Ok(stream);
                }
                // Connection is stale, drop it and reconnect
            }
        }

        // Reconnect
        self.available.store(false, Ordering::SeqCst);
        self.connect()?;

        let conn = self.connection.lock();
        conn.as_ref()
            .ok_or_else(|| DaemonError::Unavailable("connection not established".to_string()))?
            .try_clone()
            .map_err(|e| DaemonError::Unavailable(format!("connection clone failed: {}", e)))
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

        let mut stream = self.get_connection()?;

        // Send request
        stream.write_all(&encoded).map_err(|e| {
            self.available.store(false, Ordering::SeqCst);
            DaemonError::Unavailable(format!("failed to send request: {}", e))
        })?;

        // Read length prefix
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).map_err(|e| {
            self.available.store(false, Ordering::SeqCst);
            if e.kind() == std::io::ErrorKind::TimedOut {
                DaemonError::Timeout("response timeout".to_string())
            } else {
                DaemonError::Unavailable(format!("failed to read response length: {}", e))
            }
        })?;

        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 100 * 1024 * 1024 {
            // 100MB sanity limit
            return Err(DaemonError::Failed(format!(
                "response too large: {} bytes",
                len
            )));
        }

        // Read response payload
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).map_err(|e| {
            self.available.store(false, Ordering::SeqCst);
            if e.kind() == std::io::ErrorKind::TimedOut {
                DaemonError::Timeout("response timeout".to_string())
            } else {
                DaemonError::Unavailable(format!("failed to read response: {}", e))
            }
        })?;

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
            other => Err(DaemonError::Failed(format!(
                "unexpected response: {:?}",
                other
            ))),
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
            other => Err(DaemonError::Failed(format!(
                "unexpected response: {:?}",
                other
            ))),
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

        // Check if health was recently verified
        if let Some(last) = *self.last_health_check.lock()
            && last.elapsed() < Duration::from_secs(30)
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
            other => Err(DaemonError::Failed(format!(
                "unexpected response: {:?}",
                other
            ))),
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
            other => Err(DaemonError::Failed(format!(
                "unexpected response: {:?}",
                other
            ))),
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
            other => Err(DaemonError::Failed(format!(
                "unexpected response: {:?}",
                other
            ))),
        }
    }
}

/// Connect to an existing daemon or spawn a new one.
pub fn connect_or_spawn() -> Result<Arc<UdsDaemonClient>, DaemonError> {
    let client = UdsDaemonClient::with_defaults();
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let path_str = config.socket_path.to_string_lossy();
        assert!(path_str.starts_with("/tmp/semantic-daemon-"));
        assert!(path_str.ends_with(".sock"));
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
    fn test_request_counter_increments() {
        let client = UdsDaemonClient::with_defaults();
        let first = client.request_counter.fetch_add(1, Ordering::Relaxed);
        let second = client.request_counter.fetch_add(1, Ordering::Relaxed);
        assert_eq!(second, first + 1);
    }
}
