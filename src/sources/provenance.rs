//! Provenance types for tracking conversation origins.
//!
//! This module defines the data model for tracking where conversations come from.
//! These types are used throughout cass: storage, indexing, search, CLI, TUI.
//!
//! # Key Types
//!
//! - [`SourceKind`]: The type of source (local, SSH, etc.)
//! - [`Source`]: A registered source in the system (stored in SQLite)
//! - [`Origin`]: Per-conversation provenance metadata
//!
//! # Example
//!
//! ```rust
//! use coding_agent_search::sources::provenance::{Origin, SourceKind, LOCAL_SOURCE_ID};
//!
//! // Create origin for a local conversation
//! let local_origin = Origin::local();
//! assert_eq!(local_origin.source_id, LOCAL_SOURCE_ID);
//!
//! // Create origin for a remote conversation
//! let remote_origin = Origin::remote("work-laptop");
//! assert!(remote_origin.is_remote());
//! ```

use serde::{Deserialize, Serialize};

/// The default source ID for local conversations.
pub const LOCAL_SOURCE_ID: &str = "local";

/// The kind/type of a source.
///
/// This enum represents how cass connects to a source to retrieve sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// Local machine (default).
    #[default]
    Local,
    /// Remote machine via SSH.
    Ssh,
    // Future extensions:
    // S3,
    // Git,
    // Http,
}

impl SourceKind {
    /// Returns true if this is a remote source kind.
    pub fn is_remote(&self) -> bool {
        !matches!(self, Self::Local)
    }

    /// Get the string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Ssh => "ssh",
        }
    }

    /// Parse from string.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "local" => Some(Self::Local),
            "ssh" => Some(Self::Ssh),
            _ => None,
        }
    }
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A registered source in the system.
///
/// This struct represents a source record as stored in SQLite.
/// It's different from [`super::config::SourceDefinition`] which is
/// the user-facing configuration for how to connect to a source.
///
/// # Fields
///
/// - `id`: Stable, user-friendly identifier (e.g., "local", "work-laptop")
/// - `kind`: The type of source (local, ssh, etc.)
/// - `host_label`: Display label for UI (often SSH alias or hostname)
/// - `machine_id`: Optional stable machine identifier
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// Stable, user-friendly identifier.
    /// Examples: "local", "work-laptop", "home-server"
    pub id: String,

    /// What type of source this is.
    pub kind: SourceKind,

    /// Display label for UI (often SSH alias or hostname).
    /// May be None for local source.
    pub host_label: Option<String>,

    /// Optional stable machine identifier (can be hashed for privacy).
    pub machine_id: Option<String>,

    /// Platform hint (macos, linux, windows).
    pub platform: Option<String>,

    /// Extra configuration as JSON (SSH params, path rewrites, etc.).
    pub config_json: Option<serde_json::Value>,

    /// When this source was first registered.
    pub created_at: Option<i64>,

    /// When this source was last updated.
    pub updated_at: Option<i64>,
}

impl Source {
    /// Create a new local source.
    pub fn local() -> Self {
        Self {
            id: LOCAL_SOURCE_ID.to_string(),
            kind: SourceKind::Local,
            host_label: None,
            machine_id: None,
            platform: None,
            config_json: None,
            created_at: None,
            updated_at: None,
        }
    }

    /// Create a new remote source.
    pub fn remote(id: impl Into<String>, host_label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: SourceKind::Ssh,
            host_label: Some(host_label.into()),
            machine_id: None,
            platform: None,
            config_json: None,
            created_at: None,
            updated_at: None,
        }
    }

    /// Check if this is a remote source.
    pub fn is_remote(&self) -> bool {
        self.kind.is_remote()
    }

    /// Check if this is the local source.
    pub fn is_local(&self) -> bool {
        self.id == LOCAL_SOURCE_ID && self.kind == SourceKind::Local
    }

    /// Get a display label for this source.
    pub fn display_label(&self) -> &str {
        self.host_label.as_deref().unwrap_or(&self.id)
    }
}

impl Default for Source {
    fn default() -> Self {
        Self::local()
    }
}

/// Per-conversation provenance metadata.
///
/// This struct is attached to each conversation to track where it came from.
/// It contains denormalized data from the parent [`Source`] for convenience.
///
/// # Example
///
/// ```rust
/// use coding_agent_search::sources::provenance::Origin;
///
/// // Local conversation
/// let local = Origin::local();
/// assert!(!local.is_remote());
///
/// // Remote conversation from laptop
/// let remote = Origin::remote("laptop");
/// assert!(remote.is_remote());
/// assert_eq!(remote.source_id, "laptop");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
    /// References Source.id.
    /// Examples: "local", "work-laptop"
    pub source_id: String,

    /// Denormalized source kind for convenience.
    pub kind: SourceKind,

    /// Display host label (may differ from source's host_label).
    /// Useful when the same source has multiple display representations.
    pub host: Option<String>,
}

impl Origin {
    /// Create an origin for local conversations.
    pub fn local() -> Self {
        Self {
            source_id: LOCAL_SOURCE_ID.to_string(),
            kind: SourceKind::Local,
            host: None,
        }
    }

    /// Create an origin for remote conversations.
    pub fn remote(source_id: impl Into<String>) -> Self {
        let id = source_id.into();
        Self {
            source_id: id.clone(),
            kind: SourceKind::Ssh,
            host: Some(id),
        }
    }

    /// Create an origin for remote conversations with explicit host label.
    pub fn remote_with_host(source_id: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            source_id: source_id.into(),
            kind: SourceKind::Ssh,
            host: Some(host.into()),
        }
    }

    /// Check if this origin is from a remote source.
    pub fn is_remote(&self) -> bool {
        self.kind.is_remote()
    }

    /// Check if this origin is local.
    pub fn is_local(&self) -> bool {
        self.source_id == LOCAL_SOURCE_ID && self.kind == SourceKind::Local
    }

    /// Get a display label for this origin.
    ///
    /// Returns format like "local" or "laptop (remote)".
    pub fn display_label(&self) -> String {
        match (&self.host, &self.kind) {
            (Some(host), SourceKind::Ssh) => format!("{} (remote)", host),
            (Some(host), SourceKind::Local) => host.clone(),
            (None, SourceKind::Local) => "local".to_string(),
            (None, SourceKind::Ssh) => format!("{} (remote)", self.source_id),
        }
    }

    /// Get a short display label (just the identifier, no suffix).
    pub fn short_label(&self) -> &str {
        self.host.as_deref().unwrap_or(&self.source_id)
    }
}

impl Default for Origin {
    fn default() -> Self {
        Self::local()
    }
}

/// Filter for searching by source.
///
/// Used in search queries to filter results by their origin.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SourceFilter {
    /// Match all sources (no filtering).
    #[default]
    All,
    /// Match only local sources.
    Local,
    /// Match only remote sources (any SSH source).
    Remote,
    /// Match a specific source by ID.
    SourceId(String),
}

impl SourceFilter {
    /// Parse a source filter from a string.
    ///
    /// - "all" or "*" → All
    /// - "local" → Local
    /// - "remote" → Remote
    /// - anything else → SourceId
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "all" | "*" => Self::All,
            "local" => Self::Local,
            "remote" => Self::Remote,
            _ => Self::SourceId(s.to_string()),
        }
    }

    /// Check if an origin matches this filter.
    pub fn matches(&self, origin: &Origin) -> bool {
        match self {
            Self::All => true,
            Self::Local => origin.is_local(),
            Self::Remote => origin.is_remote(),
            Self::SourceId(id) => origin.source_id == *id,
        }
    }

    /// Check if this filter allows any source.
    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }
}

impl std::fmt::Display for SourceFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::All => write!(f, "all"),
            Self::Local => write!(f, "local"),
            Self::Remote => write!(f, "remote"),
            Self::SourceId(id) => write!(f, "{}", id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_kind_default() {
        assert_eq!(SourceKind::default(), SourceKind::Local);
    }

    #[test]
    fn test_source_kind_is_remote() {
        assert!(!SourceKind::Local.is_remote());
        assert!(SourceKind::Ssh.is_remote());
    }

    #[test]
    fn test_source_kind_display() {
        assert_eq!(SourceKind::Local.to_string(), "local");
        assert_eq!(SourceKind::Ssh.to_string(), "ssh");
    }

    #[test]
    fn test_source_kind_parse() {
        assert_eq!(SourceKind::parse("local"), Some(SourceKind::Local));
        assert_eq!(SourceKind::parse("LOCAL"), Some(SourceKind::Local));
        assert_eq!(SourceKind::parse("ssh"), Some(SourceKind::Ssh));
        assert_eq!(SourceKind::parse("SSH"), Some(SourceKind::Ssh));
        assert_eq!(SourceKind::parse("unknown"), None);
    }

    #[test]
    fn test_source_kind_serialization() {
        assert_eq!(
            serde_json::to_string(&SourceKind::Local).unwrap(),
            "\"local\""
        );
        assert_eq!(serde_json::to_string(&SourceKind::Ssh).unwrap(), "\"ssh\"");
    }

    #[test]
    fn test_source_kind_deserialization() {
        assert_eq!(
            serde_json::from_str::<SourceKind>("\"local\"").unwrap(),
            SourceKind::Local
        );
        assert_eq!(
            serde_json::from_str::<SourceKind>("\"ssh\"").unwrap(),
            SourceKind::Ssh
        );
    }

    #[test]
    fn test_source_local() {
        let source = Source::local();
        assert_eq!(source.id, LOCAL_SOURCE_ID);
        assert_eq!(source.kind, SourceKind::Local);
        assert!(source.is_local());
        assert!(!source.is_remote());
    }

    #[test]
    fn test_source_remote() {
        let source = Source::remote("laptop", "user@laptop.local");
        assert_eq!(source.id, "laptop");
        assert_eq!(source.kind, SourceKind::Ssh);
        assert_eq!(source.host_label, Some("user@laptop.local".to_string()));
        assert!(source.is_remote());
        assert!(!source.is_local());
    }

    #[test]
    fn test_source_display_label() {
        let local = Source::local();
        assert_eq!(local.display_label(), "local");

        let remote = Source::remote("laptop", "user@laptop.local");
        assert_eq!(remote.display_label(), "user@laptop.local");
    }

    #[test]
    fn test_source_default() {
        let source = Source::default();
        assert!(source.is_local());
    }

    #[test]
    fn test_origin_local() {
        let origin = Origin::local();
        assert_eq!(origin.source_id, LOCAL_SOURCE_ID);
        assert_eq!(origin.kind, SourceKind::Local);
        assert!(origin.is_local());
        assert!(!origin.is_remote());
    }

    #[test]
    fn test_origin_remote() {
        let origin = Origin::remote("laptop");
        assert_eq!(origin.source_id, "laptop");
        assert_eq!(origin.kind, SourceKind::Ssh);
        assert!(origin.is_remote());
        assert!(!origin.is_local());
    }

    #[test]
    fn test_origin_remote_with_host() {
        let origin = Origin::remote_with_host("laptop", "user@laptop.local");
        assert_eq!(origin.source_id, "laptop");
        assert_eq!(origin.host, Some("user@laptop.local".to_string()));
    }

    #[test]
    fn test_origin_display_label() {
        let local = Origin::local();
        assert_eq!(local.display_label(), "local");

        let remote = Origin::remote("laptop");
        assert_eq!(remote.display_label(), "laptop (remote)");

        let remote_with_host = Origin::remote_with_host("laptop", "user@laptop.local");
        assert_eq!(
            remote_with_host.display_label(),
            "user@laptop.local (remote)"
        );
    }

    #[test]
    fn test_origin_short_label() {
        let local = Origin::local();
        assert_eq!(local.short_label(), "local");

        let remote = Origin::remote_with_host("laptop", "user@laptop.local");
        assert_eq!(remote.short_label(), "user@laptop.local");
    }

    #[test]
    fn test_origin_default() {
        let origin = Origin::default();
        assert!(origin.is_local());
    }

    #[test]
    fn test_origin_equality() {
        let a = Origin::local();
        let b = Origin::local();
        assert_eq!(a, b);

        let c = Origin::remote("laptop");
        let d = Origin::remote("laptop");
        assert_eq!(c, d);

        assert_ne!(a, c);
    }

    #[test]
    fn test_origin_serialization_roundtrip() {
        let original = Origin::remote_with_host("laptop", "user@host");
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: Origin = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_source_filter_parse() {
        assert_eq!(SourceFilter::parse("all"), SourceFilter::All);
        assert_eq!(SourceFilter::parse("ALL"), SourceFilter::All);
        assert_eq!(SourceFilter::parse("*"), SourceFilter::All);
        assert_eq!(SourceFilter::parse("local"), SourceFilter::Local);
        assert_eq!(SourceFilter::parse("LOCAL"), SourceFilter::Local);
        assert_eq!(SourceFilter::parse("remote"), SourceFilter::Remote);
        assert_eq!(SourceFilter::parse("REMOTE"), SourceFilter::Remote);
        assert_eq!(
            SourceFilter::parse("laptop"),
            SourceFilter::SourceId("laptop".to_string())
        );
    }

    #[test]
    fn test_source_filter_matches() {
        let local = Origin::local();
        let remote = Origin::remote("laptop");

        assert!(SourceFilter::All.matches(&local));
        assert!(SourceFilter::All.matches(&remote));

        assert!(SourceFilter::Local.matches(&local));
        assert!(!SourceFilter::Local.matches(&remote));

        assert!(!SourceFilter::Remote.matches(&local));
        assert!(SourceFilter::Remote.matches(&remote));

        assert!(SourceFilter::SourceId("laptop".to_string()).matches(&remote));
        assert!(!SourceFilter::SourceId("laptop".to_string()).matches(&local));
        assert!(!SourceFilter::SourceId("other".to_string()).matches(&remote));
    }

    #[test]
    fn test_source_filter_display() {
        assert_eq!(SourceFilter::All.to_string(), "all");
        assert_eq!(SourceFilter::Local.to_string(), "local");
        assert_eq!(SourceFilter::Remote.to_string(), "remote");
        assert_eq!(
            SourceFilter::SourceId("laptop".to_string()).to_string(),
            "laptop"
        );
    }

    #[test]
    fn test_source_filter_default() {
        assert_eq!(SourceFilter::default(), SourceFilter::All);
    }
}
