//! Semantic model daemon for warm embedding and reranking.
//!
//! This module provides a daemon server that keeps ML models resident in memory
//! for fast inference. The daemon:
//! - Listens on a Unix Domain Socket for requests
//! - Shares the socket with xf (wire-compatible protocol)
//! - First-come spawns, others connect
//! - Supports graceful fallback to direct inference
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                    WIRE-COMPATIBLE DAEMONS                      │
//! ├─────────────────────────────────────────────────────────────────┤
//! │  xf (standalone)           cass (standalone)                   │
//! │  ┌──────────────┐          ┌──────────────┐                    │
//! │  │ xf binary    │          │ cass binary  │                    │
//! │  │  └─ daemon   │          │  └─ daemon   │                    │
//! │  └──────────────┘          └──────────────┘                    │
//! │         │ Same socket path: $TMPDIR/semantic-daemon-$USER.sock │
//! │         ▼                         ▼                            │
//! │  ┌────────────────────────────────────────┐                    │
//! │  │  Shared UDS Socket (first-come wins)   │                    │
//! │  └────────────────────────────────────────┘                    │
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
