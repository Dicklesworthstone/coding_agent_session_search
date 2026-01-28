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
//! │         │ Same socket path: /tmp/semantic-daemon-$USER.sock    │
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

// Re-export key types for convenience
pub use client::{DaemonClientConfig, UdsDaemonClient};
pub use core::{DaemonConfig, ModelDaemon};
pub use models::ModelManager;
pub use protocol::{Request, Response, PROTOCOL_VERSION, default_socket_path};
pub use resource::ResourceMonitor;
