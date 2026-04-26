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
//!     "hide_metadata": false
//!   },
//!   "deployment": {
//!     "target": "local",
//!     "output_dir": "./dist",
//!     "repo": "my-archive",
//!     "branch": "gh-pages",
//!     "account_id": "env:CLOUDFLARE_ACCOUNT_ID",
//!     "api_token": "env:CLOUDFLARE_API_TOKEN"
//!   }
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;
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
    pub hide_metadata: bool,
}

/// Resolved deployment configuration.
#[derive(Debug, Serialize)]
pub struct ResolvedDeployment {
    pub target: String,
    pub output_dir: PathBuf,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub account_id: Option<String>,
    pub api_token_set: bool,
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

    /// Hide workspace/agent metadata in UI.
    #[serde(default)]
    pub hide_metadata: bool,
}

impl Default for BundleConfig {
    fn default() -> Self {
        Self {
            title: default_title(),
            description: default_description(),
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

    /// Repository/project name for GitHub or Cloudflare Pages deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,

    /// Branch for GitHub Pages deployment (default: gh-pages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    /// Cloudflare account ID (for API-token auth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,

    /// Cloudflare API token (for API-token auth).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token: Option<String>,
}

impl Default for DeploymentConfig {
    fn default() -> Self {
        Self {
            target: default_target(),
            output_dir: default_output_dir(),
            repo: None,
            branch: None,
            account_id: None,
            api_token: None,
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

const DEFAULT_PATH_MODE: &str = "relative";
const DEFAULT_COMPRESSION: &str = "deflate";
const DEFAULT_CHUNK_SIZE: u64 = 8 * 1024 * 1024;

fn resolve_env_var(env_var: &str) -> Result<String, ConfigError> {
    dotenvy::var(env_var).map_err(|_| ConfigError::EnvVarNotFound(env_var.to_string()))
}

impl PagesConfig {
    fn normalized_path_mode(&self) -> Option<String> {
        self.filters
            .path_mode
            .as_deref()
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
            .map(str::to_ascii_lowercase)
    }

    fn normalized_target(&self) -> String {
        self.deployment.target.trim().to_ascii_lowercase()
    }

    fn resolved_path_mode(&self) -> String {
        self.normalized_path_mode()
            .unwrap_or_else(|| DEFAULT_PATH_MODE.to_string())
    }

    fn resolved_compression(&self) -> String {
        self.encryption
            .compression
            .clone()
            .unwrap_or_else(|| DEFAULT_COMPRESSION.to_string())
    }

    fn resolved_chunk_size(&self) -> u64 {
        self.encryption.chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE)
    }

    fn resolved_time_range(&self) -> Option<String> {
        match (&self.filters.since, &self.filters.until) {
            (Some(since), Some(until)) => Some(format!("{} to {}", since, until)),
            (Some(since), None) => Some(format!("since {}", since)),
            (None, Some(until)) => Some(format!("until {}", until)),
            (None, None) => None,
        }
    }

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
        if let Some(ref password) = self.encryption.password
            && let Some(env_var) = password.strip_prefix("env:")
        {
            self.encryption.password = Some(resolve_env_var(env_var)?);
        }

        // Resolve env vars in output_dir if prefixed
        if let Some(env_var) = self.deployment.output_dir.strip_prefix("env:") {
            self.deployment.output_dir = resolve_env_var(env_var)?;
        }

        if let Some(ref account_id) = self.deployment.account_id
            && let Some(env_var) = account_id.strip_prefix("env:")
        {
            self.deployment.account_id = Some(resolve_env_var(env_var)?);
        }

        if let Some(ref api_token) = self.deployment.api_token
            && let Some(env_var) = api_token.strip_prefix("env:")
        {
            self.deployment.api_token = Some(resolve_env_var(env_var)?);
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
        if let Some(mode) = self.normalized_path_mode() {
            match mode.as_str() {
                "relative" | "basename" | "full" | "hash" => {}
                _ => {
                    errors.push(format!(
                        "Invalid filters.path_mode: '{}'. Must be one of: relative, basename, full, hash",
                        self.filters.path_mode.as_deref().unwrap_or_default()
                    ));
                }
            }
        }

        // Validate deployment target
        let target = self.normalized_target();
        match target.as_str() {
            "local" | "github" | "cloudflare" => {}
            _ => {
                errors.push(format!(
                    "Invalid deployment.target: '{}'. Must be one of: local, github, cloudflare",
                    self.deployment.target
                ));
            }
        }

        // Validate GitHub deployment config
        if target == "github" && self.deployment.repo.is_none() {
            errors.push(
                "deployment.repo is required when target is 'github'. \
                 Specify the repository name for GitHub Pages deployment."
                    .to_string(),
            );
        }

        if target == "cloudflare" {
            let account_id_set = self.deployment.account_id.is_some();
            let api_token_set = self.deployment.api_token.is_some();
            if account_id_set ^ api_token_set {
                errors.push(
                    "deployment.account_id and deployment.api_token must both be set for Cloudflare API-token auth (use env:VAR syntax if needed)."
                        .to_string(),
                );
            }
        } else if self.deployment.account_id.is_some() || self.deployment.api_token.is_some() {
            warnings.push(
                "deployment.account_id/api_token are set but deployment.target is not cloudflare; these values will be ignored."
                    .to_string(),
            );
        }

        // Validate time formats
        if let Some(ref since) = self.filters.since
            && parse_time_input(since).is_none()
        {
            errors.push(format!(
                "Invalid filters.since time format: '{}'. \
                 Use ISO 8601 (2025-01-06), relative (30 days ago), or keywords (today, yesterday).",
                since
            ));
        }

        if let Some(ref until) = self.filters.until
            && parse_time_input(until).is_none()
        {
            errors.push(format!(
                "Invalid filters.until time format: '{}'. \
                 Use ISO 8601 (2025-01-06), relative (30 days ago), or keywords (today, yesterday).",
                until
            ));
        }

        match self.encryption.chunk_size {
            Some(0) => errors.push("encryption.chunk_size must be greater than 0 bytes.".into()),
            Some(chunk_size) if chunk_size > crate::pages::encrypt::MAX_CHUNK_SIZE as u64 => errors.push(format!(
                    "encryption.chunk_size ({chunk_size}) exceeds the maximum supported size of {} bytes.",
                    crate::pages::encrypt::MAX_CHUNK_SIZE
                )),
            _ => {}
        }

        // Warnings
        if self
            .encryption
            .password
            .as_ref()
            .is_some_and(|p| p.chars().count() < 12)
        {
            warnings.push(
                "Password is less than 12 characters. Consider using a stronger password."
                    .to_string(),
            );
        }

        if self.encryption.no_encryption {
            warnings.push(
                "no_encryption is enabled. Content will be publicly readable without a password."
                    .to_string(),
            );
        }

        if self.encryption.generate_qr && !self.encryption.generate_recovery {
            warnings.push(
                "generate_qr is enabled but generate_recovery is false. QR codes are generated for recovery secrets only."
                    .to_string(),
            );
        }

        if target == "github" && self.deployment.branch.is_some() {
            warnings.push(
                "deployment.branch is set for GitHub Pages, but cass always deploys to gh-pages. The value will be ignored."
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
                workspaces: self.filters.workspaces.iter().map(PathBuf::from).collect(),
                since_ts: self.filters.since.as_deref().and_then(parse_time_input),
                until_ts: self.filters.until.as_deref().and_then(parse_time_input),
                path_mode: self.resolved_path_mode(),
            },
            encryption: ResolvedEncryption {
                enabled: !self.encryption.no_encryption,
                password_set: self.encryption.password.is_some(),
                generate_recovery: self.encryption.generate_recovery,
                generate_qr: self.encryption.generate_qr,
                compression: self.resolved_compression(),
                chunk_size: self.resolved_chunk_size(),
            },
            bundle: ResolvedBundle {
                title: self.bundle.title.clone(),
                description: self.bundle.description.clone(),
                hide_metadata: self.bundle.hide_metadata,
            },
            deployment: ResolvedDeployment {
                target: self.normalized_target(),
                output_dir: PathBuf::from(&self.deployment.output_dir),
                repo: self.deployment.repo.clone(),
                branch: self.deployment.branch.clone(),
                account_id: self.deployment.account_id.clone(),
                api_token_set: self.deployment.api_token.is_some(),
            },
        }
    }

    /// Convert to WizardState for execution.
    pub fn to_wizard_state(&self, db_path: PathBuf) -> Result<WizardState, ConfigError> {
        // Parse deploy target
        let target = match self.normalized_target().as_str() {
            "github" => DeployTarget::GitHubPages,
            "cloudflare" => DeployTarget::CloudflarePages,
            _ => DeployTarget::Local,
        };

        // Convert workspaces
        let workspaces = if self.filters.workspaces.is_empty() {
            None
        } else {
            Some(self.filters.workspaces.iter().map(PathBuf::from).collect())
        };

        Ok(WizardState {
            agents: self.filters.agents.clone(),
            time_range: self.resolved_time_range(),
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
            cloudflare_branch: self.deployment.branch.clone(),
            cloudflare_account_id: self.deployment.account_id.clone(),
            cloudflare_api_token: self.deployment.api_token.clone(),
            final_site_dir: None,
        })
    }

    /// Parse path mode from config.
    pub fn path_mode(&self) -> PathMode {
        match self.normalized_path_mode().as_deref() {
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
    "hide_metadata": false
  },
  "deployment": {
    "target": "local",
    "output_dir": "./cass-export",
    "repo": null,
    "branch": null,
    "account_id": null,
    "api_token": null
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

    // Tests for `include_attachments` config field removed: the flag was
    // accepted but unimplemented and has been removed from the pages
    // config surface (bead adyyt). Any future attachment-bundling work
    // will re-add the field with end-to-end implementation + fresh tests.

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
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("i_understand_risks"))
        );
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
    fn test_validate_zero_chunk_size() {
        let mut config = PagesConfig::default();
        config.encryption.password = Some("test123".to_string());
        config.encryption.chunk_size = Some(0);

        let result = config.validate();
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("chunk_size")));
    }

    #[test]
    fn test_validate_oversized_chunk_size() {
        let mut config = PagesConfig::default();
        config.encryption.password = Some("test123".to_string());
        config.encryption.chunk_size = Some(crate::pages::encrypt::MAX_CHUNK_SIZE as u64 + 1);

        let result = config.validate();
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("chunk_size")));
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
    fn test_env_var_resolution_deployment_credentials() {
        // SAFETY: This test runs in isolation and the env vars are cleaned up after use
        unsafe {
            std::env::set_var("TEST_CF_ACCOUNT_ID", "acc123");
            std::env::set_var("TEST_CF_API_TOKEN", "token456");
        }

        let mut config = PagesConfig::default();
        config.deployment.account_id = Some("env:TEST_CF_ACCOUNT_ID".to_string());
        config.deployment.api_token = Some("env:TEST_CF_API_TOKEN".to_string());
        config.resolve_env_vars().unwrap();

        assert_eq!(config.deployment.account_id, Some("acc123".to_string()));
        assert_eq!(config.deployment.api_token, Some("token456".to_string()));

        // SAFETY: Cleanup of test env vars
        unsafe {
            std::env::remove_var("TEST_CF_ACCOUNT_ID");
            std::env::remove_var("TEST_CF_API_TOKEN");
        }
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
    fn test_validate_partial_cloudflare_credentials() {
        let mut config = PagesConfig::default();
        config.encryption.password = Some("test123".to_string());
        config.deployment.target = "cloudflare".to_string();
        config.deployment.account_id = Some("acc-only".to_string());

        let result = config.validate();
        assert!(!result.valid);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.contains("account_id") && e.contains("api_token"))
        );
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

        // Parsing should be case-insensitive and whitespace-tolerant, matching validation.
        config.filters.path_mode = Some("Basename".to_string());
        assert!(matches!(config.path_mode(), PathMode::Basename));

        config.filters.path_mode = Some(" FULL ".to_string());
        assert!(matches!(config.path_mode(), PathMode::Full));

        config.filters.path_mode = Some("   ".to_string());
        assert!(matches!(config.path_mode(), PathMode::Relative));
    }

    #[test]
    fn test_validate_path_mode_trims_whitespace() {
        let mut config = PagesConfig::default();
        config.encryption.password = Some("test123".to_string());
        config.filters.path_mode = Some(" FULL ".to_string());

        let result = config.validate();
        assert!(result.valid, "{:?}", result.errors);

        let resolved = result.resolved.expect("resolved config should exist");
        assert_eq!(resolved.filters.path_mode, "full");
    }

    #[test]
    fn test_resolved_config_applies_export_defaults() {
        let mut config = PagesConfig::default();
        config.encryption.password = Some("test123".to_string());

        let result = config.validate();
        assert!(result.valid, "{:?}", result.errors);

        let resolved = result.resolved.expect("resolved config should exist");
        assert_eq!(resolved.filters.path_mode, DEFAULT_PATH_MODE);
        assert_eq!(resolved.encryption.compression, DEFAULT_COMPRESSION);
        assert_eq!(resolved.encryption.chunk_size, DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn test_validate_target_trims_whitespace() {
        let mut config = PagesConfig::default();
        config.encryption.password = Some("test123".to_string());
        config.deployment.target = " GitHub ".to_string();
        config.deployment.repo = Some("example-repo".to_string());

        let result = config.validate();
        assert!(result.valid, "{:?}", result.errors);

        let resolved = result.resolved.expect("resolved config should exist");
        assert_eq!(resolved.deployment.target, "github");
    }

    #[test]
    fn test_to_wizard_state_target_trims_whitespace() {
        let mut config = PagesConfig::default();
        config.encryption.password = Some("test123".to_string());
        config.deployment.target = " cloudflare ".to_string();

        let state = config
            .to_wizard_state(PathBuf::from("/tmp/test.db"))
            .expect("wizard state should parse");

        assert!(matches!(state.target, DeployTarget::CloudflarePages));
    }

    #[test]
    fn test_resolved_time_range_priority() {
        let mut config = PagesConfig::default();

        assert_eq!(config.resolved_time_range(), None);

        config.filters.since = Some("30 days ago".to_string());
        assert_eq!(
            config.resolved_time_range(),
            Some("since 30 days ago".to_string())
        );

        config.filters.until = Some("today".to_string());
        assert_eq!(
            config.resolved_time_range(),
            Some("30 days ago to today".to_string())
        );

        config.filters.since = None;
        assert_eq!(
            config.resolved_time_range(),
            Some("until today".to_string())
        );
    }
}
