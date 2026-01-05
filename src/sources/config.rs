//! Configuration types for remote sources.
//!
//! This module defines the data structures for configuring remote sources
//! that cass can sync agent sessions from. Configuration is stored in TOML
//! format at `~/.config/cass/sources.toml` (or XDG equivalent).
//!
//! # Example Configuration
//!
//! ```toml
//! [[sources]]
//! name = "laptop"
//! type = "ssh"
//! host = "user@laptop.local"
//! paths = ["~/.claude/projects", "~/.cursor"]
//! sync_schedule = "manual"
//!
//! [[sources]]
//! name = "workstation"
//! type = "ssh"
//! host = "user@work.example.com"
//! paths = ["~/.claude/projects"]
//! sync_schedule = "daily"
//!
//! # Path mappings rewrite remote paths to local equivalents
//! [[sources.path_mappings]]
//! from = "/home/user/projects"
//! to = "/Users/me/projects"
//!
//! # Agent-specific mappings only apply when viewing specific agent sessions
//! [[sources.path_mappings]]
//! from = "/opt/work"
//! to = "/Volumes/Work"
//! agents = ["claude-code"]
//! ```

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

use super::provenance::SourceKind;

/// Errors that can occur when loading or saving source configuration.
#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    Read(#[from] std::io::Error),

    #[error("Failed to parse config file: {0}")]
    Parse(#[from] toml::de::Error),

    #[error("Failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),

    #[error("Could not determine config directory")]
    NoConfigDir,

    #[error("Validation error: {0}")]
    Validation(String),
}

/// Root configuration containing all source definitions.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SourcesConfig {
    /// List of configured sources.
    #[serde(default)]
    pub sources: Vec<SourceDefinition>,
}

/// A single path mapping rule for rewriting paths.
///
/// Path mappings transform paths from one location to another,
/// useful for mapping remote paths to local equivalents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathMapping {
    /// Remote path prefix to match.
    pub from: String,
    /// Local path prefix to replace with.
    pub to: String,
    /// Optional: only apply this mapping for specific agents.
    /// If None, applies to all agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<String>>,
}

impl PathMapping {
    /// Create a new path mapping.
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            agents: None,
        }
    }

    /// Create a new path mapping with agent filter.
    pub fn with_agents(
        from: impl Into<String>,
        to: impl Into<String>,
        agents: Vec<String>,
    ) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            agents: Some(agents),
        }
    }

    /// Apply this mapping to a path if it matches.
    ///
    /// Returns `Some(rewritten_path)` if the path starts with `from` prefix,
    /// `None` otherwise.
    pub fn apply(&self, path: &str) -> Option<String> {
        if path.starts_with(&self.from) {
            Some(path.replacen(&self.from, &self.to, 1))
        } else {
            None
        }
    }

    /// Check if this mapping applies to a given agent.
    pub fn applies_to_agent(&self, agent: Option<&str>) -> bool {
        match (&self.agents, agent) {
            (None, _) => true,       // No filter means applies to all
            (Some(_), None) => true, // No agent specified means match all mappings
            (Some(agents), Some(a)) => agents.iter().any(|allowed| allowed == a),
        }
    }
}

/// Definition of a single source (local or remote).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceDefinition {
    /// Friendly name for this source (e.g., "laptop", "workstation").
    /// This becomes the `source_id` used throughout the system.
    pub name: String,

    /// Connection type (local, ssh, etc.).
    #[serde(rename = "type", default)]
    pub source_type: SourceKind,

    /// Remote host for SSH connections (e.g., "user@laptop.local").
    #[serde(default)]
    pub host: Option<String>,

    /// Paths to sync from this source.
    /// For SSH sources, these are remote paths.
    /// Supports ~ expansion.
    #[serde(default)]
    pub paths: Vec<String>,

    /// When to automatically sync this source.
    #[serde(default)]
    pub sync_schedule: SyncSchedule,

    /// Path mappings for workspace rewriting.
    /// Maps remote paths to local equivalents.
    /// Example: "/home/user/projects" -> "/Users/me/projects"
    #[serde(default)]
    pub path_mappings: Vec<PathMapping>,

    /// Platform hint for default paths (macos, linux).
    #[serde(default)]
    pub platform: Option<Platform>,

    /// Custom rsync path on remote machine.
    /// If specified, uses --rsync-path flag to use a custom rsync binary.
    /// Example: "/opt/homebrew/bin/rsync"
    #[serde(default)]
    pub rsync_path: Option<String>,
}

impl SourceDefinition {
    /// Create a new local source definition.
    pub fn local(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source_type: SourceKind::Local,
            ..Default::default()
        }
    }

    /// Create a new SSH source definition.
    pub fn ssh(name: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            source_type: SourceKind::Ssh,
            host: Some(host.into()),
            ..Default::default()
        }
    }

    /// Check if this source requires SSH connectivity.
    pub fn is_remote(&self) -> bool {
        matches!(self.source_type, SourceKind::Ssh)
    }

    /// Validate the source definition.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::Validation(
                "Source name cannot be empty".into(),
            ));
        }

        if self.name.contains('/') || self.name.contains('\\') {
            return Err(ConfigError::Validation(
                "Source name cannot contain path separators".into(),
            ));
        }

        if has_dot_components(Path::new(&self.name)) {
            return Err(ConfigError::Validation(
                "Source name cannot be '.' or '..'".into(),
            ));
        }

        if self.is_remote() && self.host.is_none() {
            return Err(ConfigError::Validation("SSH sources require a host".into()));
        }

        if self.is_remote()
            && let Some(host) = self.host.as_deref()
        {
            validate_ssh_host(host)?;
        }

        Ok(())
    }

    /// Apply path mapping to rewrite a workspace path.
    ///
    /// Uses longest-prefix matching. If an agent is specified,
    /// only mappings that apply to that agent are considered.
    pub fn rewrite_path(&self, path: &str) -> String {
        self.rewrite_path_for_agent(path, None)
    }

    /// Apply path mapping for a specific agent.
    ///
    /// Uses longest-prefix matching, filtering by agent.
    pub fn rewrite_path_for_agent(&self, path: &str, agent: Option<&str>) -> String {
        // Sort by prefix length descending for longest-prefix match
        let mut mappings: Vec<_> = self
            .path_mappings
            .iter()
            .filter(|m| m.applies_to_agent(agent))
            .collect();
        mappings.sort_by(|a, b| b.from.len().cmp(&a.from.len()));

        for mapping in mappings {
            if let Some(rewritten) = mapping.apply(path) {
                return rewritten;
            }
        }

        path.to_string()
    }
}

fn has_dot_components(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
}

fn validate_ssh_host(host: &str) -> Result<(), ConfigError> {
    let host = host.trim();

    if host.is_empty() {
        return Err(ConfigError::Validation("SSH host cannot be empty".into()));
    }

    if host.starts_with('-') {
        return Err(ConfigError::Validation(
            "SSH host cannot start with '-' (would be parsed as an ssh option)".into(),
        ));
    }

    if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ConfigError::Validation(
            "SSH host cannot contain whitespace or control characters".into(),
        ));
    }

    Ok(())
}

/// Sync schedule for remote sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyncSchedule {
    /// Only sync when explicitly requested.
    #[default]
    Manual,
    /// Sync every hour.
    Hourly,
    /// Sync once per day.
    Daily,
}

impl std::fmt::Display for SyncSchedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manual => write!(f, "manual"),
            Self::Hourly => write!(f, "hourly"),
            Self::Daily => write!(f, "daily"),
        }
    }
}

/// Platform hint for choosing default paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Macos,
    Linux,
    Windows,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Macos => write!(f, "macos"),
            Self::Linux => write!(f, "linux"),
            Self::Windows => write!(f, "windows"),
        }
    }
}

impl SourcesConfig {
    /// Load configuration from the default location.
    ///
    /// Returns an empty config if the file doesn't exist.
    pub fn load() -> Result<Self, ConfigError> {
        let config_path = Self::config_path()?;

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&config_path)?;
        let config: Self = toml::from_str(&content)?;

        // Validate all sources
        config.validate()?;

        Ok(config)
    }

    /// Load configuration from a specific path.
    pub fn load_from(path: &PathBuf) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        config.validate()?;

        Ok(config)
    }

    /// Save configuration to the default location.
    pub fn save(&self) -> Result<(), ConfigError> {
        let config_path = Self::config_path()?;

        // Create parent directories if needed
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;
        std::fs::write(&config_path, content)?;

        Ok(())
    }

    /// Save configuration to a specific path.
    pub fn save_to(&self, path: &PathBuf) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;

        Ok(())
    }

    /// Get the default configuration file path.
    ///
    /// Uses XDG conventions:
    /// - Primary: `$XDG_CONFIG_HOME/cass/sources.toml`
    /// - Fallback: platform-specific config dir (e.g., `~/.config/cass/sources.toml` on Linux)
    pub fn config_path() -> Result<PathBuf, ConfigError> {
        // Respect XDG_CONFIG_HOME first (important for testing and Linux users)
        if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(xdg_config).join("cass").join("sources.toml"));
        }

        dirs::config_dir()
            .map(|p| p.join("cass").join("sources.toml"))
            .ok_or(ConfigError::NoConfigDir)
    }

    /// Validate all sources in the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Check for duplicate names
        let mut seen_names = std::collections::HashSet::new();
        for source in &self.sources {
            source.validate()?;

            if !seen_names.insert(&source.name) {
                return Err(ConfigError::Validation(format!(
                    "Duplicate source name: {}",
                    source.name
                )));
            }
        }

        Ok(())
    }

    /// Find a source by name.
    pub fn find_source(&self, name: &str) -> Option<&SourceDefinition> {
        self.sources.iter().find(|s| s.name == name)
    }

    /// Find a source by name (mutable).
    pub fn find_source_mut(&mut self, name: &str) -> Option<&mut SourceDefinition> {
        self.sources.iter_mut().find(|s| s.name == name)
    }

    /// Add a new source. Returns error if name already exists.
    pub fn add_source(&mut self, source: SourceDefinition) -> Result<(), ConfigError> {
        source.validate()?;

        if self.sources.iter().any(|s| s.name == source.name) {
            return Err(ConfigError::Validation(format!(
                "Source '{}' already exists",
                source.name
            )));
        }

        self.sources.push(source);
        Ok(())
    }

    /// Remove a source by name. Returns true if found and removed.
    pub fn remove_source(&mut self, name: &str) -> bool {
        let initial_len = self.sources.len();
        self.sources.retain(|s| s.name != name);
        self.sources.len() < initial_len
    }

    /// Get all remote sources (SSH type).
    pub fn remote_sources(&self) -> impl Iterator<Item = &SourceDefinition> {
        self.sources.iter().filter(|s| s.is_remote())
    }
}

/// Get preset paths for a given platform.
///
/// These are the default agent session directories for each platform.
pub fn get_preset_paths(preset: &str) -> Result<Vec<String>, ConfigError> {
    match preset {
        "macos-defaults" | "macos" => Ok(vec![
            "~/.claude/projects".into(),
            "~/.codex/sessions".into(),
            "~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev".into(),
            "~/Library/Application Support/Code/User/globalStorage/rooveterinaryinc.roo-cline"
                .into(),
            "~/Library/Application Support/Cursor/User/globalStorage/saoudrizwan.claude-dev".into(),
            "~/Library/Application Support/Cursor/User/globalStorage/rooveterinaryinc.roo-cline"
                .into(),
            "~/Library/Application Support/com.openai.chat".into(),
            "~/.gemini/tmp".into(),
            "~/.pi/agent/sessions".into(),
            "~/.local/share/opencode".into(),
            "~/.continue/sessions".into(),
            "~/.aider.chat.history.md".into(),
            "~/.goose/sessions".into(),
        ]),
        "linux-defaults" | "linux" => Ok(vec![
            "~/.claude/projects".into(),
            "~/.codex/sessions".into(),
            "~/.config/Code/User/globalStorage/saoudrizwan.claude-dev".into(),
            "~/.config/Code/User/globalStorage/rooveterinaryinc.roo-cline".into(),
            "~/.config/Cursor/User/globalStorage/saoudrizwan.claude-dev".into(),
            "~/.config/Cursor/User/globalStorage/rooveterinaryinc.roo-cline".into(),
            "~/.gemini/tmp".into(),
            "~/.pi/agent/sessions".into(),
            "~/.local/share/opencode".into(),
            "~/.continue/sessions".into(),
            "~/.aider.chat.history.md".into(),
            "~/.goose/sessions".into(),
        ]),
        _ => Err(ConfigError::Validation(format!(
            "Unknown preset: '{}'. Valid presets: macos-defaults, linux-defaults",
            preset
        ))),
    }
}

// =============================================================================
// SSH Config Discovery
// =============================================================================

/// Discovered SSH host from ~/.ssh/config
#[derive(Debug, Clone)]
pub struct DiscoveredHost {
    /// Host alias from SSH config
    pub name: String,
    /// Hostname or IP address
    pub hostname: Option<String>,
    /// Username
    pub user: Option<String>,
    /// Port (defaults to 22)
    pub port: Option<u16>,
    /// Identity file path
    pub identity_file: Option<String>,
}

impl DiscoveredHost {
    /// Get the SSH connection string (user@host or just host)
    pub fn connection_string(&self) -> String {
        if let Some(user) = &self.user {
            format!("{}@{}", user, self.name)
        } else {
            self.name.clone()
        }
    }
}

/// Discover SSH hosts from ~/.ssh/config.
///
/// Parses the SSH config file and returns a list of discovered hosts
/// that could be used as remote sources.
pub fn discover_ssh_hosts() -> Vec<DiscoveredHost> {
    let ssh_config_path = dirs::home_dir()
        .map(|h| h.join(".ssh").join("config"))
        .unwrap_or_default();

    if !ssh_config_path.exists() {
        return Vec::new();
    }

    let content = match std::fs::read_to_string(&ssh_config_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    parse_ssh_config(&content)
}

/// Parse SSH config file content into discovered hosts.
fn parse_ssh_config(content: &str) -> Vec<DiscoveredHost> {
    let mut hosts = Vec::new();
    let mut current_host: Option<DiscoveredHost> = None;

    for line in content.lines() {
        let line = line.trim();

        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Parse key-value pairs
        let parts: Vec<&str> = line.splitn(2, |c: char| c.is_whitespace()).collect();
        if parts.len() != 2 {
            continue;
        }

        let key = parts[0].to_lowercase();
        let value = parts[1].trim();

        match key.as_str() {
            "host" => {
                // Save previous host if exists
                if let Some(host) = current_host.take() {
                    // Skip wildcard patterns and generic hosts
                    if !host.name.contains('*') && !host.name.contains('?') {
                        hosts.push(host);
                    }
                }

                // Start new host (skip wildcards)
                if !value.contains('*') && !value.contains('?') {
                    current_host = Some(DiscoveredHost {
                        name: value.to_string(),
                        hostname: None,
                        user: None,
                        port: None,
                        identity_file: None,
                    });
                }
            }
            "hostname" => {
                if let Some(ref mut host) = current_host {
                    host.hostname = Some(value.to_string());
                }
            }
            "user" => {
                if let Some(ref mut host) = current_host {
                    host.user = Some(value.to_string());
                }
            }
            "port" => {
                if let Some(ref mut host) = current_host {
                    host.port = value.parse().ok();
                }
            }
            "identityfile" => {
                if let Some(ref mut host) = current_host {
                    host.identity_file = Some(value.to_string());
                }
            }
            _ => {}
        }
    }

    // Don't forget the last host
    if let Some(host) = current_host
        && !host.name.contains('*')
        && !host.name.contains('?')
    {
        hosts.push(host);
    }

    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_config_default() {
        let config = SourcesConfig::default();
        assert!(config.sources.is_empty());
    }

    #[test]
    fn test_source_definition_local() {
        let source = SourceDefinition::local("test");
        assert_eq!(source.name, "test");
        assert_eq!(source.source_type, SourceKind::Local);
        assert!(!source.is_remote());
    }

    #[test]
    fn test_source_definition_ssh() {
        let source = SourceDefinition::ssh("laptop", "user@laptop.local");
        assert_eq!(source.name, "laptop");
        assert_eq!(source.source_type, SourceKind::Ssh);
        assert_eq!(source.host, Some("user@laptop.local".into()));
        assert!(source.is_remote());
    }

    #[test]
    fn test_source_validation_empty_name() {
        let source = SourceDefinition::default();
        assert!(source.validate().is_err());
    }

    #[test]
    fn test_source_validation_dot_names() {
        let source = SourceDefinition::local(".");
        assert!(source.validate().is_err());

        let source = SourceDefinition::local("..");
        assert!(source.validate().is_err());
    }

    #[test]
    fn test_source_validation_ssh_without_host() {
        let mut source = SourceDefinition::ssh("test", "host");
        source.host = None;
        assert!(source.validate().is_err());
    }

    #[test]
    fn test_source_validation_ssh_host_hardening() {
        let source = SourceDefinition::ssh("test", "-oProxyCommand=evil");
        assert!(source.validate().is_err());

        let source = SourceDefinition::ssh("test", "user@host withspace");
        assert!(source.validate().is_err());
    }

    #[test]
    fn test_path_mapping_new() {
        let mapping = PathMapping::new("/home/user", "/Users/me");
        assert_eq!(mapping.from, "/home/user");
        assert_eq!(mapping.to, "/Users/me");
        assert!(mapping.agents.is_none());
    }

    #[test]
    fn test_path_mapping_with_agents() {
        let mapping = PathMapping::with_agents(
            "/home/user",
            "/Users/me",
            vec!["claude-code".into(), "cursor".into()],
        );
        assert_eq!(mapping.from, "/home/user");
        assert_eq!(mapping.to, "/Users/me");
        assert_eq!(
            mapping.agents,
            Some(vec!["claude-code".into(), "cursor".into()])
        );
    }

    #[test]
    fn test_path_mapping_apply() {
        let mapping = PathMapping::new("/home/user/projects", "/Users/me/projects");

        // Matching prefix
        assert_eq!(
            mapping.apply("/home/user/projects/myapp"),
            Some("/Users/me/projects/myapp".into())
        );

        // Non-matching prefix
        assert_eq!(mapping.apply("/opt/data"), None);

        // Partial match (not at start)
        assert_eq!(mapping.apply("/data/home/user/projects"), None);
    }

    #[test]
    fn test_path_mapping_applies_to_agent() {
        // Mapping with no agent filter
        let global = PathMapping::new("/home", "/Users");
        assert!(global.applies_to_agent(None));
        assert!(global.applies_to_agent(Some("claude-code")));
        assert!(global.applies_to_agent(Some("any-agent")));

        // Mapping with agent filter
        let filtered = PathMapping::with_agents("/home", "/Users", vec!["claude-code".into()]);
        assert!(filtered.applies_to_agent(None)); // No agent specified = match all
        assert!(filtered.applies_to_agent(Some("claude-code")));
        assert!(!filtered.applies_to_agent(Some("cursor"))); // Not in list
    }

    #[test]
    fn test_path_rewriting() {
        let mut source = SourceDefinition::local("test");
        source.path_mappings.push(PathMapping::new(
            "/home/user/projects",
            "/Users/me/projects",
        ));
        source
            .path_mappings
            .push(PathMapping::new("/home/user", "/Users/me"));

        // Longest prefix should match
        assert_eq!(
            source.rewrite_path("/home/user/projects/myapp"),
            "/Users/me/projects/myapp"
        );

        // Shorter prefix
        assert_eq!(source.rewrite_path("/home/user/other"), "/Users/me/other");

        // No match
        assert_eq!(source.rewrite_path("/opt/data"), "/opt/data");
    }

    #[test]
    fn test_path_rewriting_with_agent_filter() {
        let mut source = SourceDefinition::local("test");
        // Global mapping
        source
            .path_mappings
            .push(PathMapping::new("/home/user", "/Users/me"));
        // Agent-specific mapping
        source.path_mappings.push(PathMapping::with_agents(
            "/home/user/projects",
            "/Volumes/Work/projects",
            vec!["claude-code".into()],
        ));

        // Without agent filter, both mappings apply (longest match wins)
        assert_eq!(
            source.rewrite_path_for_agent("/home/user/projects/app", None),
            "/Volumes/Work/projects/app"
        );

        // With claude-code agent, use specific mapping
        assert_eq!(
            source.rewrite_path_for_agent("/home/user/projects/app", Some("claude-code")),
            "/Volumes/Work/projects/app"
        );

        // With cursor agent, falls back to global mapping
        assert_eq!(
            source.rewrite_path_for_agent("/home/user/projects/app", Some("cursor")),
            "/Users/me/projects/app"
        );

        // Non-matching path
        assert_eq!(
            source.rewrite_path_for_agent("/opt/data", Some("claude-code")),
            "/opt/data"
        );
    }

    #[test]
    fn test_config_duplicate_names() {
        let mut config = SourcesConfig::default();
        config.sources.push(SourceDefinition::local("test"));
        config.sources.push(SourceDefinition::local("test"));

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_add_source() {
        let mut config = SourcesConfig::default();
        config.add_source(SourceDefinition::local("test")).unwrap();

        assert_eq!(config.sources.len(), 1);

        // Adding duplicate should fail
        assert!(config.add_source(SourceDefinition::local("test")).is_err());
    }

    #[test]
    fn test_config_remove_source() {
        let mut config = SourcesConfig::default();
        config.sources.push(SourceDefinition::local("test"));

        assert!(config.remove_source("test"));
        assert!(!config.remove_source("nonexistent"));
        assert!(config.sources.is_empty());
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let mut config = SourcesConfig::default();
        config.sources.push(SourceDefinition {
            name: "laptop".into(),
            source_type: SourceKind::Ssh,
            host: Some("user@laptop.local".into()),
            paths: vec!["~/.claude/projects".into()],
            sync_schedule: SyncSchedule::Daily,
            path_mappings: vec![PathMapping::new("/home/user", "/Users/me")],
            platform: Some(Platform::Linux),
        });

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: SourcesConfig = toml::from_str(&serialized).unwrap();

        assert_eq!(deserialized.sources.len(), 1);
        assert_eq!(deserialized.sources[0].name, "laptop");
        assert_eq!(deserialized.sources[0].sync_schedule, SyncSchedule::Daily);
        assert_eq!(deserialized.sources[0].path_mappings.len(), 1);
        assert_eq!(deserialized.sources[0].path_mappings[0].from, "/home/user");
        assert_eq!(deserialized.sources[0].path_mappings[0].to, "/Users/me");
    }

    #[test]
    fn test_path_mapping_serialization_with_agents() {
        let mut config = SourcesConfig::default();
        config.sources.push(SourceDefinition {
            name: "remote".into(),
            source_type: SourceKind::Ssh,
            host: Some("user@server".into()),
            paths: vec![],
            sync_schedule: SyncSchedule::Manual,
            path_mappings: vec![
                PathMapping::new("/home/user", "/Users/me"),
                PathMapping::with_agents("/opt/work", "/Volumes/Work", vec!["claude-code".into()]),
            ],
            platform: None,
        });

        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: SourcesConfig = toml::from_str(&serialized).unwrap();

        assert_eq!(deserialized.sources[0].path_mappings.len(), 2);
        // First mapping has no agents filter
        assert!(deserialized.sources[0].path_mappings[0].agents.is_none());
        // Second mapping has agents filter
        assert_eq!(
            deserialized.sources[0].path_mappings[1].agents,
            Some(vec!["claude-code".into()])
        );
    }

    #[test]
    fn test_preset_paths() {
        let macos = get_preset_paths("macos-defaults").unwrap();
        assert!(!macos.is_empty());
        assert!(macos.iter().any(|p| p.contains(".claude")));

        let linux = get_preset_paths("linux-defaults").unwrap();
        assert!(!linux.is_empty());

        assert!(get_preset_paths("unknown").is_err());
    }

    #[test]
    fn test_sync_schedule_display() {
        assert_eq!(SyncSchedule::Manual.to_string(), "manual");
        assert_eq!(SyncSchedule::Hourly.to_string(), "hourly");
        assert_eq!(SyncSchedule::Daily.to_string(), "daily");
    }

    #[test]
    fn test_discover_ssh_hosts() {
        // Just test that the function doesn't panic
        let hosts = super::discover_ssh_hosts();
        // Could be empty if no ~/.ssh/config exists
        for host in hosts {
            assert!(!host.name.is_empty());
        }
    }
}
