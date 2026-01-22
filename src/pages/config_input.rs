//! Non-interactive configuration input for `cass pages` command.
//!
//! This module provides a JSON-based configuration schema for running the pages
//! export workflow in robot/CI mode without interactive wizard prompts.
//!
//! # Example Configuration
//!
//! ```json
//! {
//!   "filters": {
//!     "agents": ["claude-code", "codex"],
//!     "since": "30 days ago",
//!     "until": "2025-01-06",
//!     "workspaces": ["/path/one", "/path/two"],
//!     "path_mode": "relative"
//!   },
//!   "encryption": {
//!     "password": "env:EXPORT_PASSWORD",
//!     "generate_recovery": true,
//!     "generate_qr": true,
//!     "compression": "deflate",
//!     "chunk_size": 8388608
//!   },
//!   "bundle": {
//!     "title": "Team Archive",
//!     "description": "Encrypted cass export",
//!     "include_pwa": false,
//!     "include_attachments": false,
//!     "hide_metadata": false
//!   },
//!   "deployment": {
//!     "target": "local",
//!     "output_dir": "./dist",
//!     "repo": "my-archive",
//!     "branch": "gh-pages"
//!   }
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use thiserror::Error;

use super::export::PathMode;
use super::wizard::{DeployTarget, WizardState};
use crate::ui::time_parser::parse_time_input;

/// Errors that can occur when loading or validating pages configuration.
#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    ReadFile(#[from] std::io::Error),

    #[error("Failed to parse config JSON: {0}")]
    ParseJson(#[from] serde_json::Error),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Environment variable not found: {0}")]
    EnvVarNotFound(String),

    #[error("Invalid time format: {0}")]
    InvalidTime(String),
}

/// Configuration result for JSON output.
#[derive(Debug, Serialize)]
pub struct ConfigValidationResult {
    /// Whether the configuration is valid.
    pub valid: bool,
    /// Validation errors, if any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    /// Warnings that don't prevent export but should be reviewed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Resolved configuration (with env vars expanded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<ResolvedConfig>,
}

/// Resolved configuration with env vars expanded and defaults applied.
#[derive(Debug, Serialize)]
pub struct ResolvedConfig {
    pub filters: ResolvedFilters,
    pub encryption: ResolvedEncryption,
    pub bundle: ResolvedBundle,
    pub deployment: ResolvedDeployment,
}

/// Resolved filter configuration.
#[derive(Debug, Serialize)]
pub struct ResolvedFilters {
    pub agents: Vec<String>,
    pub workspaces: Vec<PathBuf>,
    pub since_ts: Option<i64>,
    pub until_ts: Option<i64>,
    pub path_mode: String,
}

/// Resolved encryption configuration.
#[derive(Debug, Serialize)]
pub struct ResolvedEncryption {
    pub enabled: bool,
    pub password_set: bool,
    pub generate_recovery: bool,
    pub generate_qr: bool,
    pub compression: String,
    pub chunk_size: u64,
}

/// Resolved bundle configuration.
#[derive(Debug, Serialize)]
pub struct ResolvedBundle {
    pub title: String,
    pub description: String,
    pub include_pwa: bool,
    pub include_attachments: bool,
    pub hide_metadata: bool,
}

/// Resolved deployment configuration.
#[derive(Debug, Serialize)]
pub struct ResolvedDeployment {
    pub target: String,
    pub output_dir: PathBuf,
    pub repo: Option<String>,
    pub branch: Option<String>,
}

/// Root pages configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PagesConfig {
    /// Filter configuration for content selection.
    #[serde(default)]
    pub filters: FilterConfig,

    /// Encryption and security configuration.
    #[serde(default)]
    pub encryption: EncryptionConfig,

    /// Bundle/site configuration.
    #[serde(default)]
    pub bundle: BundleConfig,

    /// Deployment configuration.
    #[serde(default)]
    pub deployment: DeploymentConfig,
}

/// Filter configuration for content selection.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FilterConfig {
    /// Filter by agent slugs (e.g., "claude-code", "codex").
    #[serde(default)]
    pub agents: Vec<String>,

    /// Filter entries since this time (ISO date or relative like "30 days ago").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,

    /// Filter entries until this time (ISO date or relative).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,

    /// Filter by workspace paths.
    #[serde(default)]
    pub workspaces: Vec<String>,

    /// Path mode: relative (default), basename, full, hash.
    #[serde(default)]
    pub path_mode: Option<String>,
}

/// Encryption and security configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionConfig {
    /// Password for encryption. Supports "env:VAR_NAME" syntax for env var resolution.
    /// If None and no_encryption is false, will error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,

    /// Disable encryption entirely (DANGEROUS).
    /// Requires explicit acknowledgment via `i_understand_risks: true`.
    #[serde(default)]
    pub no_encryption: bool,

    /// Required acknowledgment for no_encryption mode.
    #[serde(default)]
    pub i_understand_risks: bool,

    /// Generate recovery secret for password recovery.
    #[serde(default = "default_true")]
    pub generate_recovery: bool,

    /// Generate QR code for password.
    #[serde(default)]
    pub generate_qr: bool,

    /// Compression algorithm: deflate (default), gzip, none.
    #[serde(default)]
    pub compression: Option<String>,

    /// Chunk size for encryption in bytes. Default: 8MB.
    #[serde(default)]
    pub chunk_size: Option<u64>,
}

impl Default for EncryptionConfig {
    fn default() -> Self {
        Self {
            password: None,
            no_encryption: false,
            i_understand_risks: false,
            generate_recovery: true,
            generate_qr: false,
            compression: None,
            chunk_size: None,
        }
    }
}

/// Bundle/site configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleConfig {
    /// Site title.
    #[serde(default = "default_title")]
    pub title: String,

    /// Site description.
    #[serde(default = "default_description")]
    pub description: String,

    /// Include PWA support (service worker, offline mode).
    #[serde(default)]
    pub include_pwa: bool,

    /// Include message attachments (images, PDFs, etc.).
    #[serde(default)]
    pub include_attachments: bool,

    /// Hide workspace/agent metadata in UI.
    #[serde(default)]
    pub hide_metadata: bool,
}

impl Default for BundleConfig {
    fn default() -> Self {
        Self {
            title: default_title(),
            description: default_description(),
            include_pwa: false,
            include_attachments: false,
            hide_metadata: false,
        }
    }
}

/// Deployment configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentConfig {
    /// Deployment target: local (default), github, cloudflare.
    #[serde(default = "default_target")]
    pub target: String,

    /// Output directory for local exports.
    #[serde(default = "default_output_dir")]
    pub output_dir: String,

    /// Repository name for GitHub Pages deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,

    /// Branch for GitHub Pages deployment (default: gh-pages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl Default for DeploymentConfig {
    fn default() -> Self {
        Self {
            target: default_target(),
            output_dir: default_output_dir(),
            repo: None,
            branch: None,
        }
    }
}

// Default value functions
fn default_true() -> bool {
    true
}
fn default_title() -> String {
    "cass Archive".to_string()
}
fn default_description() -> String {
    "Encrypted archive of AI coding agent conversations".to_string()
}
fn default_target() -> String {
    "local".to_string()
}
fn default_output_dir() -> String {
    "cass-export".to_string()
}

impl PagesConfig {
    /// Load configuration from a file path.
    ///
    /// If path is "-", reads from stdin.
    pub fn load(path: &str) -> Result<Self, ConfigError> {
        let content = if path == "-" {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        } else {
            std::fs::read_to_string(path)?
        };

        let config: PagesConfig = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Load configuration from a reader.
    pub fn from_reader<R: Read>(reader: R) -> Result<Self, ConfigError> {
        let config: PagesConfig = serde_json::from_reader(reader)?;
        Ok(config)
    }

    /// Resolve environment variables in configuration values.
    ///
    /// Values starting with "env:" are resolved to the corresponding
    /// environment variable value.
    pub fn resolve_env_vars(&mut self) -> Result<(), ConfigError> {
        if let Some(ref password) = self.encryption.password {
            if let Some(env_var) = password.strip_prefix("env:") {
                self.encryption.password = Some(
                    std::env::var(env_var)
                        .map_err(|_| ConfigError::EnvVarNotFound(env_var.to_string()))?,
                );
            }
        }

        // Resolve env vars in output_dir if prefixed
        if let Some(env_var) = self.deployment.output_dir.strip_prefix("env:") {
            self.deployment.output_dir = std::env::var(env_var)
                .map_err(|_| ConfigError::EnvVarNotFound(env_var.to_string()))?;
        }

        Ok(())
    }

    /// Validate the configuration and return any errors/warnings.
    pub fn validate(&self) -> ConfigValidationResult {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        // Validate encryption config
        if !self.encryption.no_encryption && self.encryption.password.is_none() {
            errors.push(
                "encryption.password is required when encryption is enabled. \
                 Use \"env:VAR_NAME\" syntax to read from environment variable, \
                 or set encryption.no_encryption: true (requires i_understand_risks: true)."
                    .to_string(),
            );
        }

        if self.encryption.no_encryption && !self.encryption.i_understand_risks {
            errors.push(
                "encryption.i_understand_risks must be true when no_encryption is enabled. \
                 This confirms you understand the security implications of unencrypted exports."
                    .to_string(),
            );
        }

        // Validate path_mode if specified
        if let Some(ref mode) = self.filters.path_mode {
            match mode.to_lowercase().as_str() {
                "relative" | "basename" | "full" | "hash" => {}
                _ => {
                    errors.push(format!(
                        "Invalid filters.path_mode: '{}'. Must be one of: relative, basename, full, hash",
                        mode
                    ));
                }
            }
        }

        // Validate deployment target
        match self.deployment.target.to_lowercase().as_str() {
            "local" | "github" | "cloudflare" => {}
            _ => {
                errors.push(format!(
                    "Invalid deployment.target: '{}'. Must be one of: local, github, cloudflare",
                    self.deployment.target
                ));
            }
        }

        // Validate GitHub deployment config
        if self.deployment.target.to_lowercase() == "github" && self.deployment.repo.is_none() {
            errors.push(
                "deployment.repo is required when target is 'github'. \
                 Specify the repository name for GitHub Pages deployment."
                    .to_string(),
            );
        }

        // Validate time formats
        if let Some(ref since) = self.filters.since {
            if parse_time_input(since).is_none() {
                errors.push(format!(
                    "Invalid filters.since time format: '{}'. \
                     Use ISO 8601 (2025-01-06), relative (30 days ago), or keywords (today, yesterday).",
                    since
                ));
            }
        }

        if let Some(ref until) = self.filters.until {
            if parse_time_input(until).is_none() {
                errors.push(format!(
                    "Invalid filters.until time format: '{}'. \
                     Use ISO 8601 (2025-01-06), relative (30 days ago), or keywords (today, yesterday).",
                    until
                ));
            }
        }

        // Warnings
        if self.encryption.password.as_ref().is_some_and(|p| p.len() < 12) {
            warnings.push(
                "Password is less than 12 characters. Consider using a stronger password."
                    .to_string(),
            );
        }

        if self.bundle.include_attachments {
            warnings.push(
                "include_attachments is enabled. This may significantly increase export size."
                    .to_string(),
            );
        }

        let valid = errors.is_empty();
        let resolved = if valid {
            Some(self.to_resolved())
        } else {
            None
        };

        ConfigValidationResult {
            valid,
            errors,
            warnings,
            resolved,
        }
    }

    /// Convert to resolved config (with defaults applied).
    fn to_resolved(&self) -> ResolvedConfig {
        ResolvedConfig {
            filters: ResolvedFilters {
                agents: self.filters.agents.clone(),
                workspaces: self
                    .filters
                    .workspaces
                    .iter()
                    .map(PathBuf::from)
                    .collect(),
                since_ts: self.filters.since.as_deref().and_then(parse_time_input),
                until_ts: self.filters.until.as_deref().and_then(parse_time_input),
                path_mode: self
                    .filters
                    .path_mode
                    .clone()
                    .unwrap_or_else(|| "relative".to_string()),
            },
            encryption: ResolvedEncryption {
                enabled: !self.encryption.no_encryption,
                password_set: self.encryption.password.is_some(),
                generate_recovery: self.encryption.generate_recovery,
                generate_qr: self.encryption.generate_qr,
                compression: self
                    .encryption
                    .compression
                    .clone()
                    .unwrap_or_else(|| "deflate".to_string()),
                chunk_size: self.encryption.chunk_size.unwrap_or(8 * 1024 * 1024),
            },
            bundle: ResolvedBundle {
                title: self.bundle.title.clone(),
                description: self.bundle.description.clone(),
                include_pwa: self.bundle.include_pwa,
                include_attachments: self.bundle.include_attachments,
                hide_metadata: self.bundle.hide_metadata,
            },
            deployment: ResolvedDeployment {
                target: self.deployment.target.clone(),
                output_dir: PathBuf::from(&self.deployment.output_dir),
                repo: self.deployment.repo.clone(),
                branch: self.deployment.branch.clone(),
            },
        }
    }

    /// Convert to WizardState for execution.
    pub fn to_wizard_state(&self, db_path: PathBuf) -> Result<WizardState, ConfigError> {
        // Parse time filters
        let time_range = match (&self.filters.since, &self.filters.until) {
            (Some(since), Some(until)) => Some(format!("{} to {}", since, until)),
            (Some(since), None) => Some(format!("since {}", since)),
            (None, Some(until)) => Some(format!("until {}", until)),
            (None, None) => None,
        };

        // Parse deploy target
        let target = match self.deployment.target.to_lowercase().as_str() {
            "github" => DeployTarget::GitHubPages,
            "cloudflare" => DeployTarget::CloudflarePages,
            _ => DeployTarget::Local,
        };

        // Convert workspaces
        let workspaces = if self.filters.workspaces.is_empty() {
            None
        } else {
            Some(
                self.filters
                    .workspaces
                    .iter()
                    .map(PathBuf::from)
                    .collect(),
            )
        };

        Ok(WizardState {
            agents: self.filters.agents.clone(),
            time_range,
            workspaces,
            password: self.encryption.password.clone(),
            recovery_secret: None,
            generate_recovery: self.encryption.generate_recovery,
            generate_qr: self.encryption.generate_qr,
            title: self.bundle.title.clone(),
            description: self.bundle.description.clone(),
            hide_metadata: self.bundle.hide_metadata,
            target,
            output_dir: PathBuf::from(&self.deployment.output_dir),
            repo_name: self.deployment.repo.clone(),
            db_path,
            exclusions: Default::default(),
            last_summary: None,
            secret_scan_has_findings: false,
            secret_scan_has_critical: false,
            secret_scan_count: 0,
            password_entropy_bits: 0.0,
            no_encryption: self.encryption.no_encryption,
            unencrypted_confirmed: self.encryption.i_understand_risks,
            include_attachments: self.bundle.include_attachments,
        })
    }

    /// Parse path mode from config.
    pub fn path_mode(&self) -> PathMode {
        match self.filters.path_mode.as_deref() {
            Some("basename") => PathMode::Basename,
            Some("full") => PathMode::Full,
            Some("hash") => PathMode::Hash,
            _ => PathMode::Relative,
        }
    }

    /// Get since timestamp.
    pub fn since_ts(&self) -> Option<i64> {
        self.filters.since.as_deref().and_then(parse_time_input)
    }

    /// Get until timestamp.
    pub fn until_ts(&self) -> Option<i64> {
        self.filters.until.as_deref().and_then(parse_time_input)
    }
}

/// Generate example configuration JSON.
pub fn example_config() -> &'static str {
    r#"{
  "filters": {
    "agents": ["claude-code", "codex"],
    "since": "30 days ago",
    "until": null,
    "workspaces": [],
    "path_mode": "relative"
  },
  "encryption": {
    "password": "env:CASS_EXPORT_PASSWORD",
    "no_encryption": false,
    "i_understand_risks": false,
    "generate_recovery": true,
    "generate_qr": false,
    "compression": "deflate",
    "chunk_size": 8388608
  },
  "bundle": {
    "title": "My Archive",
    "description": "Encrypted cass export",
    "include_pwa": false,
    "include_attachments": false,
    "hide_metadata": false
  },
  "deployment": {
    "target": "local",
    "output_dir": "./cass-export",
    "repo": null,
    "branch": null
  }
}"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let json = r#"{"encryption": {"password": "test123"}}"#;
        let config: PagesConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.encryption.password, Some("test123".to_string()));
        assert!(!config.encryption.no_encryption);
    }

    #[test]
    fn test_parse_full_config() {
        let json = example_config();
        let config: PagesConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.filters.agents, vec!["claude-code", "codex"]);
        assert_eq!(config.bundle.title, "My Archive");
        assert_eq!(config.deployment.target, "local");
    }

    #[test]
    fn test_validate_missing_password() {
        let config = PagesConfig::default();
        let result = config.validate();
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("password")));
    }

    #[test]
    fn test_validate_no_encryption_without_ack() {
        let mut config = PagesConfig::default();
        config.encryption.no_encryption = true;
        config.encryption.i_understand_risks = false;
        let result = config.validate();
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("i_understand_risks")));
    }

    #[test]
    fn test_validate_no_encryption_with_ack() {
        let mut config = PagesConfig::default();
        config.encryption.no_encryption = true;
        config.encryption.i_understand_risks = true;
        let result = config.validate();
        assert!(result.valid);
    }

    #[test]
    fn test_validate_github_without_repo() {
        let mut config = PagesConfig::default();
        config.encryption.password = Some("test123".to_string());
        config.deployment.target = "github".to_string();
        let result = config.validate();
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("repo")));
    }

    #[test]
    fn test_env_var_resolution() {
        // SAFETY: This test runs in isolation and the env var is cleaned up after use
        unsafe { std::env::set_var("TEST_PASSWORD_VAR", "secret123") };
        let mut config = PagesConfig::default();
        config.encryption.password = Some("env:TEST_PASSWORD_VAR".to_string());
        config.resolve_env_vars().unwrap();
        assert_eq!(config.encryption.password, Some("secret123".to_string()));
        // SAFETY: Cleanup of test env var
        unsafe { std::env::remove_var("TEST_PASSWORD_VAR") };
    }

    #[test]
    fn test_env_var_not_found() {
        let mut config = PagesConfig::default();
        config.encryption.password = Some("env:NONEXISTENT_VAR_12345".to_string());
        let result = config.resolve_env_vars();
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_path_mode() {
        let mut config = PagesConfig::default();
        config.encryption.password = Some("test123".to_string());
        config.filters.path_mode = Some("invalid".to_string());
        let result = config.validate();
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("path_mode")));
    }

    #[test]
    fn test_invalid_deploy_target() {
        let mut config = PagesConfig::default();
        config.encryption.password = Some("test123".to_string());
        config.deployment.target = "invalid".to_string();
        let result = config.validate();
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("target")));
    }

    #[test]
    fn test_path_mode_parsing() {
        let mut config = PagesConfig::default();

        config.filters.path_mode = None;
        assert!(matches!(config.path_mode(), PathMode::Relative));

        config.filters.path_mode = Some("basename".to_string());
        assert!(matches!(config.path_mode(), PathMode::Basename));

        config.filters.path_mode = Some("full".to_string());
        assert!(matches!(config.path_mode(), PathMode::Full));

        config.filters.path_mode = Some("hash".to_string());
        assert!(matches!(config.path_mode(), PathMode::Hash));
    }
}
