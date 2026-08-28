//! Semantic model daemon for warm embedding and reranking.
//!
//! This module provides a daemon server that keeps ML models resident in memory
//! for fast inference. The daemon:
//! - Listens on a Unix Domain Socket for requests
//! - Uses a CASS-owned socket and versioned protocol
//! - Allows CASS clients to connect to one shared warm process
//! - Supports graceful fallback to direct inference
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                       CASS MODEL DAEMON                         │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  cass clients ──▶ $TMPDIR/cass-semantic-daemon-<hash>.sock    │
//! │                         │                                      │
//! │                         ▼                                      │
//! │               ┌────────────────────┐                           │
//! │               │ warm model process │                           │
//! │               └────────────────────┘                           │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Usage
//!
//! ```ignore
//! use cass::daemon::{client::UdsDaemonClient, core::ModelDaemon};
//!
//! // Client usage (auto-spawns daemon if not running)
//! let client = UdsDaemonClient::with_defaults();
//! client.connect()?;
//! let embeddings = client.embed(&["hello world"])?;
//!
//! // Server usage (for daemon subprocess)
//! let daemon = ModelDaemon::with_defaults(&data_dir);
//! daemon.run()?;
//! ```

pub mod client;
pub mod core;
pub mod models;
pub mod protocol;
pub mod resource;
pub mod worker;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Wire identity revision for the producer-attested semantic daemon channel.
///
/// This label is signed as part of `DaemonConnectionIdentityV1`; changing any
/// request/response semantics requires a new label as well as a wire protocol
/// version bump.
#[cfg(unix)]
pub(crate) const DAEMON_ATTESTATION_PROTOCOL_REVISION: &str =
    "cass-semantic-msgpack-attested-v1";

#[cfg(unix)]
const DAEMON_ATTESTATION_KEY_BYTES: usize = 32;
#[cfg(unix)]
const DAEMON_ATTESTATION_GENERATION_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Owner-private HMAC authority shared by one CASS data directory's daemon
/// and clients. The secret is never serialized onto the daemon wire.
#[cfg(unix)]
pub(crate) struct DaemonAttestationKeyV1 {
    key_id: String,
    secret: Vec<u8>,
}

#[cfg(unix)]
impl DaemonAttestationKeyV1 {
    pub(crate) fn key_id(&self) -> &str {
        &self.key_id
    }

    pub(crate) fn secret(&self) -> &[u8] {
        &self.secret
    }

    pub(crate) fn pinned_verifier(
        &self,
    ) -> frankensearch::SearchResult<frankensearch::PinnedDaemonVerifierV1> {
        frankensearch::PinnedDaemonVerifierV1::new(self.key_id.clone(), self.secret.clone())
    }
}

#[cfg(unix)]
impl std::fmt::Debug for DaemonAttestationKeyV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DaemonAttestationKeyV1")
            .field("key_id", &self.key_id)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[cfg(unix)]
impl Drop for DaemonAttestationKeyV1 {
    fn drop(&mut self) {
        self.secret.fill(0);
    }
}

#[cfg(unix)]
fn daemon_attestation_state_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("daemon")
}

#[cfg(unix)]
fn daemon_attestation_key_path(data_dir: &Path) -> PathBuf {
    daemon_attestation_state_dir(data_dir).join("attestation-key-v1.bin")
}

#[cfg(unix)]
fn daemon_attestation_generation_path(data_dir: &Path) -> PathBuf {
    daemon_attestation_state_dir(data_dir).join("attestation-generations-v1.log")
}

#[cfg(unix)]
fn ensure_owner_private_daemon_state_dir(data_dir: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    let state_dir = daemon_attestation_state_dir(data_dir);
    std::fs::create_dir_all(&state_dir)?;
    let metadata = std::fs::symlink_metadata(&state_dir)?;
    if !metadata.file_type().is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "daemon attestation state path is not a directory: {}",
                state_dir.display()
            ),
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    validate_owner_private_daemon_state_dir(data_dir)?;
    Ok(state_dir)
}

#[cfg(unix)]
fn validate_owner_private_daemon_state_dir(data_dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let state_dir = daemon_attestation_state_dir(data_dir);
    let metadata = std::fs::symlink_metadata(&state_dir)?;
    if !metadata.file_type().is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "daemon attestation state directory is not owner-private: {}",
                state_dir.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_existing_owner_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "daemon attestation state must be an owner-private, singly-linked regular file: {}",
                path.display()
            ),
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn daemon_attestation_key_id(secret: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    let digest = Sha256::digest(secret);
    format!("cass-daemon-key-{}", hex::encode(&digest[..12]))
}

#[cfg(unix)]
fn decode_daemon_attestation_key(
    path: &Path,
    mut file: std::fs::File,
) -> std::io::Result<DaemonAttestationKeyV1> {
    use std::io::Read as _;

    let metadata = file.metadata()?;
    if metadata.len() != DAEMON_ATTESTATION_KEY_BYTES as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "daemon attestation key has invalid length at {}",
                path.display()
            ),
        ));
    }
    let mut secret = vec![0_u8; DAEMON_ATTESTATION_KEY_BYTES];
    file.read_exact(&mut secret)?;
    let key_id = daemon_attestation_key_id(&secret);
    Ok(DaemonAttestationKeyV1 { key_id, secret })
}

/// Load the already-pinned authority without creating or rotating it. Clients
/// use this read-only path and fail closed when the daemon has not provisioned
/// its data directory yet.
#[cfg(unix)]
pub(crate) fn load_daemon_attestation_key(
    data_dir: &Path,
) -> std::io::Result<DaemonAttestationKeyV1> {
    validate_owner_private_daemon_state_dir(data_dir)?;
    let path = daemon_attestation_key_path(data_dir);
    let file = open_existing_owner_private_file(&path)?;
    decode_daemon_attestation_key(&path, file)
}

#[cfg(unix)]
fn load_or_create_daemon_attestation_key(
    data_dir: &Path,
) -> std::io::Result<DaemonAttestationKeyV1> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    ensure_owner_private_daemon_state_dir(data_dir)?;
    let path = daemon_attestation_key_path(data_dir);
    use ring::rand::{SecureRandom as _, SystemRandom};

    let mut generated_secret = vec![0_u8; DAEMON_ATTESTATION_KEY_BYTES];
    SystemRandom::new()
        .fill(&mut generated_secret)
        .map_err(|_| {
            std::io::Error::other("secure random generation failed for daemon attestation key")
        })?;
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(&generated_secret)?;
            file.sync_all()?;
            std::fs::File::open(
                path.parent()
                    .ok_or_else(|| std::io::Error::other("attestation key has no parent"))?,
            )?
            .sync_all()?;
            let key_id = daemon_attestation_key_id(&generated_secret);
            Ok(DaemonAttestationKeyV1 {
                key_id,
                secret: generated_secret,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            load_daemon_attestation_key(data_dir)
        }
        Err(error) => Err(error),
    }
}

/// Provision the pinned authority and durably allocate the next per-data-dir
/// daemon generation. The append-only generation log avoids in-place counter
/// corruption and is independently locked for alternate socket paths.
#[cfg(unix)]
pub(crate) fn initialize_daemon_attestation_authority(
    data_dir: &Path,
) -> std::io::Result<(DaemonAttestationKeyV1, u64)> {
    use fs2::FileExt as _;
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    ensure_owner_private_daemon_state_dir(data_dir)?;
    let generation_path = daemon_attestation_generation_path(data_dir);
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    let mut generations = options.open(&generation_path)?;
    generations.lock_exclusive()?;

    let result = (|| {
        let metadata = generations.metadata()?;
        if !metadata.file_type().is_file()
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.len() > DAEMON_ATTESTATION_GENERATION_LOG_MAX_BYTES
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "daemon attestation generation log is unsafe or over its size bound: {}",
                    generation_path.display()
                ),
            ));
        }

        let mut encoded = String::with_capacity(metadata.len() as usize);
        generations.read_to_string(&mut encoded)?;
        let previous = encoded
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                line.trim().parse::<u64>().map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "daemon attestation generation log contains an invalid entry",
                    )
                })
            })
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        let generation = previous.checked_add(1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "daemon attestation generation counter exhausted",
            )
        })?;
        if !encoded.is_empty() && !encoded.ends_with('\n') {
            generations.write_all(b"\n")?;
        }
        writeln!(generations, "{generation}")?;
        generations.sync_all()?;
        let key = load_or_create_daemon_attestation_key(data_dir)?;
        Ok((key, generation))
    })();

    let unlock_result = fs2::FileExt::unlock(&generations);
    match (result, unlock_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

/// Fingerprint the exact logical UDS endpoint without lossy path conversion.
#[cfg(unix)]
pub(crate) fn daemon_socket_endpoint_fingerprint(socket_path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;

    let normalized = crate::normalize_path_identity(socket_path);
    let encoded = hex::encode(normalized.as_os_str().as_bytes());
    frankensearch::daemon_endpoint_fingerprint(&format!("uds-unix-hex:{encoded}"))
}

/// Short, data-directory-specific default socket path. Binding the model and
/// pinned authority to one socket prevents a warm daemon for another CASS
/// archive from being mistaken for the requested channel.
#[cfg(unix)]
pub(crate) fn daemon_socket_path_for_data_dir(data_dir: &Path) -> PathBuf {
    use sha2::{Digest as _, Sha256};
    use std::os::unix::ffi::OsStrExt as _;

    let normalized = crate::normalize_path_identity(data_dir);
    let mut hasher = Sha256::new();
    hasher.update(b"cass.semantic-daemon.socket.v1");
    if let Ok(user) = dotenvy::var("USER") {
        hasher.update((user.len() as u64).to_be_bytes());
        hasher.update(user.as_bytes());
    } else {
        hasher.update(0_u64.to_be_bytes());
    }
    let path_bytes = normalized.as_os_str().as_bytes();
    hasher.update((path_bytes.len() as u64).to_be_bytes());
    hasher.update(path_bytes);
    let digest = hasher.finalize();
    std::env::temp_dir().join(format!(
        "cass-semantic-daemon-{}.sock",
        hex::encode(&digest[..12])
    ))
}

/// Hash the current executable into a bounded deployment identity, then bind
/// that digest into Frankensearch's domain-separated executable fingerprint.
#[cfg(unix)]
pub(crate) fn current_daemon_executable_fingerprint() -> std::io::Result<String> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let executable = std::env::current_exe()?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(executable)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "daemon executable identity source is not a regular file",
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let deployment_digest = hasher.finalize();
    Ok(frankensearch::daemon_executable_fingerprint(
        &deployment_digest[..],
    ))
}

/// Advisory metadata stored inside the daemon's existing run-lock. The OS lock
/// remains the ownership authority; this content only makes the disposable
/// runtime artifact observable to read-only diagnostics.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct DaemonRunLockMetadata {
    pub pid: u32,
    pub heartbeat_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

/// Stable numeric identity for the currently published lexical artifact.
/// Tantivy metadata is atomically replaced at publish; the first 64 hash bits
/// are sufficient for runtime skew detection (this is not a security token).
pub(crate) fn published_lexical_generation(data_dir: &Path) -> Option<u64> {
    let index_path = crate::search::tantivy::expected_index_dir(data_dir);
    let fingerprint = crate::search::tantivy::searchable_index_fingerprint(&index_path)
        .ok()
        .flatten()?;
    u64::from_str_radix(fingerprint.get(..16)?, 16).ok()
}

// Used by daemon client/server paths in some target combinations, but not all
// library-only builds that we verify during placeholder cleanup.
#[allow(dead_code)]
pub(crate) fn daemon_run_lock_path(socket_path: &Path) -> PathBuf {
    socket_path.with_extension("spawnlock")
}

pub(crate) fn daemon_spawn_guard_lock_path(socket_path: &Path) -> PathBuf {
    socket_path.with_extension("spawn-guard.lock")
}

// Re-export key types for convenience
pub use client::{DaemonClientConfig, UdsDaemonClient};
pub use core::{DaemonConfig, ModelDaemon};
pub use models::ModelManager;
pub use protocol::{PROTOCOL_VERSION, Request, Response, default_socket_path};
pub use resource::ResourceMonitor;
pub use worker::{EmbeddingJobConfig, EmbeddingWorkerHandle};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn attestation_authority_is_private_stable_and_monotonically_generated() {
        use std::os::unix::fs::PermissionsExt as _;

        let data_dir = tempfile::tempdir().expect("temp data dir");
        let (first_key, first_generation) =
            initialize_daemon_attestation_authority(data_dir.path()).expect("first authority");
        let first_key_id = first_key.key_id().to_string();
        drop(first_key);
        let (second_key, second_generation) =
            initialize_daemon_attestation_authority(data_dir.path()).expect("second authority");

        assert_eq!(first_generation, 1);
        assert_eq!(second_generation, 2);
        assert_eq!(second_key.key_id(), first_key_id);
        assert_eq!(second_key.secret().len(), DAEMON_ATTESTATION_KEY_BYTES);
        assert_eq!(
            std::fs::symlink_metadata(daemon_attestation_state_dir(data_dir.path()))
                .expect("state directory")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for path in [
            daemon_attestation_key_path(data_dir.path()),
            daemon_attestation_generation_path(data_dir.path()),
        ] {
            let metadata = std::fs::symlink_metadata(path).expect("private state file");
            assert!(metadata.file_type().is_file());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn client_key_lookup_is_read_only_and_refuses_symlinks() {
        use std::os::unix::fs::PermissionsExt as _;

        let missing = tempfile::tempdir().expect("missing-key data dir");
        assert!(load_daemon_attestation_key(missing.path()).is_err());
        assert!(
            !daemon_attestation_state_dir(missing.path()).exists(),
            "client key lookup must not provision daemon state"
        );

        let data_dir = tempfile::tempdir().expect("symlink data dir");
        let state_dir = daemon_attestation_state_dir(data_dir.path());
        std::fs::create_dir(&state_dir).expect("state dir");
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))
            .expect("private state dir");
        let external = data_dir.path().join("external-key.bin");
        std::fs::write(&external, [9_u8; DAEMON_ATTESTATION_KEY_BYTES])
            .expect("external key fixture");
        std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o600))
            .expect("private external key");
        std::os::unix::fs::symlink(&external, daemon_attestation_key_path(data_dir.path()))
            .expect("key symlink fixture");

        assert!(
            load_daemon_attestation_key(data_dir.path()).is_err(),
            "a matching-length symlink must never become the pinned authority"
        );
    }

    #[cfg(unix)]
    #[test]
    fn data_directory_socket_names_are_stable_distinct_and_short() {
        let first = daemon_socket_path_for_data_dir(Path::new("/tmp/cass-one"));
        let same = daemon_socket_path_for_data_dir(Path::new("/tmp/cass-one/./"));
        let second = daemon_socket_path_for_data_dir(Path::new("/tmp/cass-two"));
        assert_eq!(first, same);
        assert_ne!(first, second);
        assert!(
            first.as_os_str().len() < 100,
            "default UDS path should leave headroom under platform limits"
        );
    }

    #[test]
    fn b7tb0_published_generation_observes_atomic_metadata_replacement() {
        let data_dir = tempfile::tempdir().expect("temp data dir");
        let index_dir = crate::search::tantivy::expected_index_dir(data_dir.path());
        std::fs::create_dir_all(&index_dir).expect("create index fixture");
        let live_meta = index_dir.join("meta.json");
        std::fs::write(&live_meta, br#"{"segments":["old"]}"#).expect("write old metadata");
        let old_generation = published_lexical_generation(data_dir.path()).expect("old generation");

        let staged_meta = index_dir.join("meta.staged.json");
        std::fs::write(&staged_meta, br#"{"segments":["new"]}"#).expect("write staged metadata");
        std::fs::rename(&staged_meta, &live_meta).expect("atomically publish metadata");

        let new_generation = published_lexical_generation(data_dir.path()).expect("new generation");
        assert_ne!(old_generation, new_generation);
    }
}
