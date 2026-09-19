//! Connection ownership and one-shot exchanges under a single request budget.
//! Never replay an exchange after any attempted write: submissions and controls
//! may have executed even when their response was interrupted.

use std::io;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use fs2::FileExt;
use parking_lot::{Mutex, MutexGuard};

use super::transport::{self, Deadline, MAX_FRAME_BYTES};
use super::{UdsDaemonClient, connection_not_established, remove_stale_daemon_socket};
use crate::daemon::daemon_spawn_guard_lock_path;
use crate::daemon::protocol::{
    ErrorCode, FramedMessage, PROTOCOL_VERSION, Request, Response, decode_message,
};
use crate::search::daemon_client::DaemonError;

fn io_error(error: io::Error, phase: &str) -> DaemonError {
    let message = format!("daemon {phase}: {error}");
    match error.kind() {
        io::ErrorKind::TimedOut => DaemonError::Timeout(message),
        io::ErrorKind::InvalidInput => DaemonError::InvalidInput(message),
        io::ErrorKind::InvalidData | io::ErrorKind::OutOfMemory => DaemonError::Failed(message),
        _ => DaemonError::Unavailable(message),
    }
}

fn check(deadline: &Deadline) -> Result<(), DaemonError> {
    deadline
        .check()
        .map_err(|error| io_error(error, "deadline"))
}

fn absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

impl UdsDaemonClient {
    /// Connect under connect_timeout, reusing an existing owned connection.
    /// Filesystem lookup and process launch are checked cooperatively; neither
    /// can be forcibly preempted. Socket backlog and lock waits are bounded.
    pub fn connect(&self) -> Result<(), DaemonError> {
        let deadline = Deadline::new(self.config.connect_timeout);
        let _connection = self.connection_until(&deadline)?;
        Ok(())
    }

    fn connection_until(
        &self,
        deadline: &Deadline,
    ) -> Result<MutexGuard<'_, Option<UnixStream>>, DaemonError> {
        // Do not alter another caller's stream/availability if admission expires.
        let mut connection = loop {
            check(deadline)?;
            if let Some(connection) = self.connection.try_lock() {
                break connection;
            }
            deadline
                .pause()
                .map_err(|error| io_error(error, "connection admission"))?;
        };
        check(deadline)?;
        if connection
            .as_ref()
            .is_some_and(|stream| stream.peer_addr().is_ok())
        {
            return Ok(connection);
        }
        *connection = None;
        self.mark_unavailable();
        let connecting = deadline.capped(self.config.connect_timeout);
        let stream = self.open_until(&connecting)?;
        check(deadline)?;
        *connection = Some(stream);
        *self.last_health_check.lock() = None;
        self.available.store(true, Ordering::SeqCst);
        Ok(connection)
    }

    fn open_until(&self, deadline: &Deadline) -> Result<UnixStream, DaemonError> {
        match transport::connect(&self.config.socket_path, deadline) {
            Ok(stream) => return Ok(stream),
            Err(error) if self.config.auto_spawn && absent(&error) => {}
            Err(error) => return Err(io_error(error, "connect")),
        }
        check(deadline)?;
        let lock_path = daemon_spawn_guard_lock_path(&self.config.socket_path);
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let lock = match options.open(&lock_path) {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&lock_path)
                    .map_err(|error| io_error(error, "spawn lock open"))?
            }
            Err(error) => return Err(io_error(error, "spawn lock create")),
        };
        check(deadline)?;
        let metadata = lock
            .metadata()
            .map_err(|error| io_error(error, "spawn lock inspection"))?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(DaemonError::Unavailable(
                "refusing a non-regular or multiply-linked daemon spawn lock".into(),
            ));
        }
        loop {
            check(deadline)?;
            match lock.try_lock_exclusive() {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    deadline
                        .pause()
                        .map_err(|error| io_error(error, "spawn lock admission"))?;
                }
                Err(error) => return Err(io_error(error, "spawn lock admission")),
            }
        }
        // Re-check under the cross-process spawn lock. A busy live backlog or
        // inaccessible endpoint never licenses deletion or another daemon.
        match transport::connect(&self.config.socket_path, deadline) {
            Ok(stream) => return Ok(stream),
            Err(error) if absent(&error) => {}
            Err(error) => return Err(io_error(error, "connect under spawn lock")),
        }
        check(deadline)?;
        let binary = self
            .config
            .daemon_binary
            .clone()
            .or_else(|| std::env::current_exe().ok())
            .ok_or_else(|| {
                DaemonError::Unavailable("cannot determine daemon binary path".into())
            })?;
        remove_stale_daemon_socket(&self.config.socket_path)?;
        check(deadline)?;
        let mut command = Command::new(binary);
        command
            .arg("daemon")
            .arg("--socket")
            .arg(&self.config.socket_path);
        if let Some(data_dir) = &self.config.data_dir {
            command.arg("--data-dir").arg(data_dir);
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| io_error(error, "spawn"))?;
        let result = self.wait_until(&mut child, deadline);
        // Also hand off after timeout. The intentionally long-lived daemon may
        // become ready later; the failed request itself is never queued/sent.
        reap_daemon(child)?;
        result
    }

    fn wait_until(
        &self,
        child: &mut Child,
        deadline: &Deadline,
    ) -> Result<UnixStream, DaemonError> {
        loop {
            check(deadline)?;
            if let Some(status) = child
                .try_wait()
                .map_err(|error| io_error(error, "startup child poll"))?
            {
                return Err(DaemonError::Unavailable(format!(
                    "spawned daemon exited before becoming ready: {status}"
                )));
            }
            match transport::connect(&self.config.socket_path, deadline) {
                Ok(stream) => return Ok(stream),
                Err(error) if absent(&error) => {}
                Err(error) => return Err(io_error(error, "startup connect")),
            }
            deadline
                .pause()
                .map_err(|error| io_error(error, "startup wait"))?;
        }
    }

    pub(super) fn send_request(&self, request: Request) -> Result<Response, DaemonError> {
        let deadline = Deadline::new(self.config.request_timeout);
        let request_id = format!(
            "cass-{}",
            self.request_counter.fetch_add(1, Ordering::Relaxed)
        );
        let message = FramedMessage::new(&request_id, request);
        let encoded = transport::encode(&message, &deadline)
            .map_err(|error| io_error(error, "request encoding"))?;
        let mut guard = self.connection_until(&deadline)?;
        let result = (|| -> Result<Response, DaemonError> {
            let stream = guard.as_mut().ok_or_else(connection_not_established)?;
            transport::write_all(stream, &encoded, &deadline)
                .map_err(|error| io_error(error, "request write"))?;
            let mut prefix = [0; 4];
            transport::read_exact(stream, &mut prefix, &deadline)
                .map_err(|error| io_error(error, "response prefix"))?;
            let length = u32::from_be_bytes(prefix) as usize;
            if length > MAX_FRAME_BYTES {
                return Err(DaemonError::Failed(format!(
                    "response too large: {length} bytes (max {MAX_FRAME_BYTES})"
                )));
            }
            if length == 0 {
                return Err(DaemonError::Failed("empty daemon response frame".into()));
            }
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(length)
                .map_err(|_| DaemonError::Failed("cannot allocate daemon response frame".into()))?;
            bytes.resize(length, 0);
            transport::read_exact(stream, &mut bytes, &deadline)
                .map_err(|error| io_error(error, "response payload"))?;
            let response = decode_message::<Response>(&bytes)
                .map_err(|_| DaemonError::Failed("failed to decode daemon response".into()))?;
            check(&deadline)?;
            if response.version != PROTOCOL_VERSION {
                return Err(DaemonError::Failed(format!(
                    "protocol version mismatch: expected {PROTOCOL_VERSION}, got {}",
                    response.version
                )));
            }
            if response.request_id != request_id {
                let observed = response.request_id.chars().take(128).collect::<String>();
                return Err(DaemonError::Failed(format!(
                    "response request ID mismatch: expected {request_id}, got {observed}"
                )));
            }
            validate_response_kind(&message.payload, &response.payload)?;
            validate_inference_response(self, &message.payload, &response.payload)?;
            if let Response::Health(health) = &response.payload {
                if health.version != PROTOCOL_VERSION {
                    return Err(DaemonError::Failed(
                        "health payload protocol version mismatch".into(),
                    ));
                }
                *self.last_health_check.lock() = health.ready.then(Instant::now);
            }
            check(&deadline)?;
            Ok(response.payload)
        })();
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                // We own this exchange. Closing prevents late/trailing bytes
                // from becoming another caller's response. Never replay it.
                *guard = None;
                self.mark_unavailable();
                return Err(error);
            }
        };
        if matches!(&response, Response::Shutdown { .. }) {
            *guard = None;
            self.mark_unavailable();
        }
        drop(guard);
        match response {
            Response::Error(error) => {
                let mut characters = error.message.chars();
                let mut message = characters.by_ref().take(1024).collect::<String>();
                if characters.next().is_some() {
                    message.push_str(" [truncated]");
                }
                Err(match error.code {
                    ErrorCode::Overloaded => DaemonError::Overloaded {
                        retry_after: error.retry_after_ms.map(Duration::from_millis),
                        message,
                    },
                    ErrorCode::Timeout => DaemonError::Timeout(message),
                    ErrorCode::InvalidInput => DaemonError::InvalidInput(message),
                    _ => DaemonError::Failed(message),
                })
            }
            response => Ok(response),
        }
    }
}

fn validate_response_kind(request: &Request, response: &Response) -> Result<(), DaemonError> {
    let valid = matches!(
        (request, response),
        (_, Response::Error(_))
            | (Request::Health, Response::Health(_))
            | (Request::ConnectionIdentity, Response::ConnectionIdentity(_))
            | (
                Request::HandshakeAttested { .. } | Request::HealthAttested { .. },
                Response::Attestation(_)
            )
            | (
                Request::EmbedAttested { .. } | Request::RerankAttested { .. },
                Response::AttestedEmbedding(_)
            )
            | (Request::Embed { .. }, Response::Embed(_))
            | (Request::Rerank { .. }, Response::Rerank(_))
            | (Request::Status, Response::Status(_))
            | (
                Request::SubmitEmbeddingJob { .. },
                Response::JobSubmitted { .. }
            )
            | (Request::EmbeddingJobStatus { .. }, Response::JobStatus(_))
            | (
                Request::CancelEmbeddingJob { .. },
                Response::JobCancelled { .. }
            )
            | (Request::Shutdown, Response::Shutdown { .. })
    );
    if valid {
        Ok(())
    } else {
        Err(DaemonError::Failed(
            "unexpected response type for daemon request".into(),
        ))
    }
}

/// Check raw model outputs while the exchange still owns its socket. Signed
/// responses get structural binding checks here; authentication with the pinned
/// local key remains mandatory in the verified fallback wrapper.
fn validate_inference_response(
    client: &UdsDaemonClient,
    request: &Request,
    response: &Response,
) -> Result<(), DaemonError> {
    let invalid = || {
        DaemonError::Failed("daemon inference response has invalid identity, count, dimensions or non-finite values".into())
    };
    match (request, response) {
        (Request::Embed { texts, dims, .. }, Response::Embed(output)) => {
            if output.model.is_empty() || output.model.len() > 256 {
                return Err(invalid());
            }
            client.validate_embedder_id(&output.model)?;
            let registered =
                crate::search::fastembed_embedder::FastEmbedder::config_for(&output.model)
                    .map(|config| config.dimension);
            if dims
                .zip(registered)
                .is_some_and(|(requested, actual)| requested != actual)
            {
                return Err(invalid());
            }
            let dimension = registered
                .or(*dims)
                .or_else(|| output.embeddings.first().map(Vec::len));
            if output.embeddings.len() != texts.len()
                || dimension == Some(0)
                || output.embeddings.iter().any(|vector| {
                    Some(vector.len()) != dimension || vector.iter().any(|value| !value.is_finite())
                })
            {
                return Err(invalid());
            }
        }
        (Request::Rerank { documents, .. }, Response::Rerank(output)) => {
            if output.model.is_empty()
                || output.model.len() > 256
                || output.scores.len() != documents.len()
                || output.scores.iter().any(|score| !score.is_finite())
            {
                return Err(invalid());
            }
        }
        (
            Request::EmbedAttested { challenge, .. } | Request::RerankAttested { challenge, .. },
            Response::AttestedEmbedding(output),
        ) => {
            output
                .attestation
                .validate_against(challenge, &output.attestation.connection, &output.vectors)
                .map_err(|_| DaemonError::UnverifiableRemoteSpace)?;
        }
        (
            Request::HandshakeAttested { challenge } | Request::HealthAttested { challenge },
            Response::Attestation(output),
        ) => {
            output
                .validate_against(challenge, &output.connection, &[])
                .map_err(|_| DaemonError::UnverifiableRemoteSpace)?;
        }
        _ => {}
    }
    Ok(())
}

fn reap_daemon(child: Child) -> Result<(), DaemonError> {
    // Preserve ownership if thread creation fails, unlike dropping Child in a
    // failed spawn closure. Cleanup only ever targets the child we just made.
    let owned = Arc::new(Mutex::new(Some(child)));
    let reaper = Arc::clone(&owned);
    match std::thread::Builder::new()
        .name("cass-daemon-reaper".into())
        .spawn(move || {
            if let Some(mut child) = reaper.lock().take() {
                let _ = child.wait();
            }
        }) {
        Ok(_) => Ok(()),
        Err(error) => {
            if let Some(mut child) = owned.lock().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            Err(io_error(error, "child reaper creation"))
        }
    }
}

#[cfg(test)]
mod tests;
