//! Share Profiles & Privacy Presets.
//!
//! This module provides pre-configured privacy profiles that simplify the redaction
//! process for common sharing scenarios. Users can select a profile instead of
//! manually configuring every option.
//!
//! ## Available Profiles
//!
//! - **Public**: Maximum redaction for public internet sharing
//! - **Team**: Moderate redaction for internal team sharing
//! - **Personal**: Minimal redaction for personal backups
//! - **Custom**: Manual configuration of all options

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::pages::patterns::{patterns_for_personal, patterns_for_public, patterns_for_team};
use crate::pages::redact::RedactionConfig;

/// Pre-configured privacy profile for sharing sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShareProfile {
    /// Maximum privacy - safe for public internet.
    /// Redacts usernames, paths, project names, emails, hostnames, and all detected secrets.
    Public,
    /// Team/organization sharing - internal refs OK.
    /// Keeps project context but redacts external credentials and personal info.
    #[default]
    Team,
    /// Personal backup - minimal redaction.
    /// Only removes critical secrets like private keys and cloud provider credentials.
    Personal,
    /// Manual configuration of all options.
    Custom,
}

impl ShareProfile {
    /// Human-readable name of the profile.
    pub fn name(self) -> &'static str {
        match self {
            Self::Public => "Public",
            Self::Team => "Team",
            Self::Personal => "Personal",
            Self::Custom => "Custom",
        }
    }

    /// Detailed description of what this profile does.
    pub fn description(self) -> &'static str {
        match self {
            Self::Public => {
                "Maximum privacy for public sharing. Redacts usernames, paths, project names, emails, hostnames, and all detected secrets."
            }
            Self::Team => {
                "For internal team sharing. Keeps project context but redacts external credentials and personal information."
            }
            Self::Personal => {
                "Personal backup with minimal redaction. Only removes critical secrets like private keys and API keys."
            }
            Self::Custom => "Configure each redaction option manually for fine-grained control.",
        }
    }

    /// Icon/emoji representing the profile.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Public => "🌐",
            Self::Team => "👥",
            Self::Personal => "🔒",
            Self::Custom => "⚙️",
        }
    }

    /// Short label for UI chips/tags.
    pub fn label(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Team => "team",
            Self::Personal => "personal",
            Self::Custom => "custom",
        }
    }

    /// Convert profile to a RedactionConfig with appropriate settings.
    pub fn to_redaction_config(self) -> RedactionConfig {
        match self {
            Self::Public => RedactionConfig {
                redact_home_paths: true,
                redact_usernames: true,
                anonymize_project_names: true,
                redact_hostnames: true,
                redact_emails: true,
                block_on_critical_secrets: true,
                custom_patterns: patterns_for_public(),
                ..Default::default()
            },
            Self::Team => RedactionConfig {
                redact_home_paths: true,
                redact_usernames: false,        // Team knows usernames
                anonymize_project_names: false, // Project context useful
                redact_hostnames: false,        // Internal hostnames OK
                redact_emails: true,            // External emails redacted
                block_on_critical_secrets: true,
                custom_patterns: patterns_for_team(),
                ..Default::default()
            },
            Self::Personal => RedactionConfig {
                redact_home_paths: false,
                redact_usernames: false,
                anonymize_project_names: false,
                redact_hostnames: false,
                redact_emails: false,
                block_on_critical_secrets: true, // Always block critical
                custom_patterns: patterns_for_personal(),
                ..Default::default()
            },
            Self::Custom => RedactionConfig::default(),
        }
    }

    /// Get all available profiles.
    pub fn all() -> &'static [Self] {
        &[Self::Public, Self::Team, Self::Personal, Self::Custom]
    }
}

impl std::str::FromStr for ShareProfile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "public" => Ok(Self::Public),
            "team" => Ok(Self::Team),
            "personal" => Ok(Self::Personal),
            "custom" => Ok(Self::Custom),
            _ => Err(format!("Unknown profile: {}", s)),
        }
    }
}

impl std::fmt::Display for ShareProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.icon(), self.name())
    }
}

/// User's profile preferences, persisted across sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilePreferences {
    /// Default profile to use when starting export.
    #[serde(default)]
    pub default_profile: ShareProfile,

    /// Custom overrides when using Custom profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_config: Option<SerializableRedactionConfig>,

    /// Last profile used (for UI convenience).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<ShareProfile>,
}

impl Default for ProfilePreferences {
    fn default() -> Self {
        Self {
            default_profile: ShareProfile::Team,
            custom_config: None,
            last_used: None,
        }
    }
}

impl ProfilePreferences {
    /// Load preferences from the default location.
    pub fn load() -> Result<Self> {
        let path = Self::default_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let prefs: Self = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(prefs)
    }

    /// Save preferences to the default location.
    pub fn save(&self) -> Result<()> {
        let path = Self::default_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }

        let content = toml::to_string_pretty(self).context("Failed to serialize preferences")?;

        // Atomic write: write to a unique temp file in the same directory, then replace.
        let temp_path = unique_atomic_temp_path(&path);
        std::fs::write(&temp_path, &content)
            .with_context(|| format!("Failed to write {}", temp_path.display()))?;
        sync_file_path(&temp_path)?;
        replace_file_from_temp(&temp_path, &path)?;

        Ok(())
    }

    /// Get the default path for profile preferences.
    fn default_path() -> Result<PathBuf> {
        let data_dir = crate::default_data_dir();
        Ok(data_dir.join("profile_prefs.toml"))
    }

    /// Update last used profile.
    pub fn set_last_used(&mut self, profile: ShareProfile) {
        self.last_used = Some(profile);
    }

    /// Get the effective profile (last used or default).
    pub fn effective_profile(&self) -> ShareProfile {
        self.last_used.unwrap_or(self.default_profile)
    }
}

fn replace_file_from_temp(temp_path: &std::path::Path, final_path: &std::path::Path) -> Result<()> {
    if cfg!(windows) {
        match std::fs::rename(temp_path, final_path) {
            Ok(()) => {
                sync_parent_directory(final_path)?;
                Ok(())
            }
            Err(first_err) if final_path.exists() => {
                let backup_path = unique_atomic_backup_path(final_path);
                std::fs::rename(final_path, &backup_path).with_context(|| {
                    let _ = std::fs::remove_file(temp_path);
                    format!(
                        "Failed preparing backup {} before replacing {} after {}",
                        backup_path.display(),
                        final_path.display(),
                        first_err
                    )
                })?;

                match std::fs::rename(temp_path, final_path) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&backup_path);
                        sync_parent_directory(final_path)?;
                        Ok(())
                    }
                    Err(second_err) => match std::fs::rename(&backup_path, final_path) {
                        Ok(()) => {
                            let _ = std::fs::remove_file(temp_path);
                            sync_parent_directory(final_path)?;
                            anyhow::bail!(
                                "Failed replacing {} with {}: {}; restored original preferences",
                                final_path.display(),
                                temp_path.display(),
                                second_err
                            );
                        }
                        Err(restore_err) => {
                            anyhow::bail!(
                                "Failed replacing {} with {}: {}; restore error: {}; temp file retained at {}",
                                final_path.display(),
                                temp_path.display(),
                                second_err,
                                restore_err,
                                temp_path.display()
                            );
                        }
                    },
                }
            }
            Err(err) => Err(err).with_context(|| {
                format!(
                    "Failed to rename {} to {}",
                    temp_path.display(),
                    final_path.display()
                )
            }),
        }
    } else {
        std::fs::rename(temp_path, final_path).with_context(|| {
            format!(
                "Failed to rename {} to {}",
                temp_path.display(),
                final_path.display()
            )
        })?;
        sync_parent_directory(final_path)
    }
}

fn sync_file_path(path: &std::path::Path) -> Result<()> {
    std::fs::File::open(path)
        .with_context(|| format!("Failed to reopen {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("Failed to sync {}", path.display()))
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &std::path::Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)
        .with_context(|| format!("Failed to open {} for sync", parent.display()))?
        .sync_all()
        .with_context(|| format!("Failed to sync {}", parent.display()))
}

#[cfg(windows)]
fn sync_parent_directory(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

fn unique_atomic_temp_path(path: &std::path::Path) -> PathBuf {
    unique_atomic_sidecar_path(path, "tmp", "profile_prefs.toml")
}

fn unique_atomic_backup_path(path: &std::path::Path) -> PathBuf {
    unique_atomic_sidecar_path(path, "bak", "profile_prefs.toml")
}

fn unique_atomic_sidecar_path(
    path: &std::path::Path,
    suffix: &str,
    fallback_name: &str,
) -> PathBuf {
    static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback_name);

    path.with_file_name(format!(
        ".{file_name}.{suffix}.{}.{}.{}",
        std::process::id(),
        timestamp,
        nonce
    ))
}

/// Serializable version of RedactionConfig for persistence.
///
/// This excludes compiled regex patterns since they can't be serialized.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableRedactionConfig {
    pub redact_home_paths: bool,
    pub redact_usernames: bool,
    pub anonymize_project_names: bool,
    pub redact_hostnames: bool,
    pub redact_emails: bool,
    pub block_on_critical_secrets: bool,
    #[serde(default)]
    pub custom_pattern_names: Vec<String>,
}

impl Default for SerializableRedactionConfig {
    fn default() -> Self {
        Self {
            redact_home_paths: true,
            redact_usernames: true,
            anonymize_project_names: false,
            redact_hostnames: false,
            redact_emails: true,
            block_on_critical_secrets: true,
            custom_pattern_names: Vec::new(),
        }
    }
}

/// Render a comparison table of all profiles for display.
pub fn render_profile_comparison() -> String {
    let mut output = String::new();

    output.push_str("┌──────────────────────┬─────────┬─────────┬──────────┐\n");
    output.push_str("│ Setting              │ Public  │ Team    │ Personal │\n");
    output.push_str("├──────────────────────┼─────────┼─────────┼──────────┤\n");
    output.push_str("│ Redact home paths    │    ✓    │    ✓    │    ✗     │\n");
    output.push_str("│ Redact usernames     │    ✓    │    ✗    │    ✗     │\n");
    output.push_str("│ Anonymize projects   │    ✓    │    ✗    │    ✗     │\n");
    output.push_str("│ Redact hostnames     │    ✓    │    ✗    │    ✗     │\n");
    output.push_str("│ Redact emails        │    ✓    │    ✓    │    ✗     │\n");
    output.push_str("│ Block critical       │    ✓    │    ✓    │    ✓     │\n");
    output.push_str("│ Pattern categories   │   All   │ External│ Critical │\n");
    output.push_str("└──────────────────────┴─────────┴─────────┴──────────┘\n");

    output
}

/// Render profile comparison for terminal with ANSI colors.
pub fn render_profile_comparison_colored() -> String {
    use console::style;

    let mut output = String::new();

    let check = style("✓").green().to_string();
    let cross = style("✗").red().to_string();

    output.push_str(&format!(
        "{}",
        style("Profile Comparison").bold().underlined()
    ));
    output.push('\n');
    output.push('\n');

    let headers = ["Setting", "🌐 Public", "👥 Team", "🔒 Personal"];
    let rows = [
        ("Redact home paths", true, true, false),
        ("Redact usernames", true, false, false),
        ("Anonymize projects", true, false, false),
        ("Redact hostnames", true, false, false),
        ("Redact emails", true, true, false),
        ("Block critical secrets", true, true, true),
    ];

    // Header
    output.push_str(&format!(
        "  {:<22} {:^10} {:^10} {:^10}\n",
        headers[0], headers[1], headers[2], headers[3]
    ));
    output.push_str(&format!("  {}\n", "─".repeat(54)));

    // Rows
    for (setting, public, team, personal) in rows {
        let p = if public { &check } else { &cross };
        let t = if team { &check } else { &cross };
        let pe = if personal { &check } else { &cross };
        output.push_str(&format!(
            "  {:<22} {:^10} {:^10} {:^10}\n",
            setting, p, t, pe
        ));
    }

    output
}

/// Information about a profile for display in selection UI.
#[derive(Debug, Clone)]
pub struct ProfileInfo {
    pub profile: ShareProfile,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub pattern_count: usize,
}

impl ProfileInfo {
    pub fn from_profile(profile: ShareProfile) -> Self {
        let config = profile.to_redaction_config();
        Self {
            profile,
            name: profile.name().to_string(),
            description: profile.description().to_string(),
            icon: profile.icon().to_string(),
            pattern_count: config.custom_patterns.len(),
        }
    }

    pub fn all() -> Vec<Self> {
        ShareProfile::all()
            .iter()
            .map(|&p| Self::from_profile(p))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_configs_differ() {
        let public = ShareProfile::Public.to_redaction_config();
        let team = ShareProfile::Team.to_redaction_config();
        let personal = ShareProfile::Personal.to_redaction_config();

        // Public is most restrictive
        assert!(public.redact_usernames);
        assert!(public.anonymize_project_names);
        assert!(public.redact_hostnames);
        assert!(public.redact_emails);

        // Team keeps some context
        assert!(!team.redact_usernames);
        assert!(!team.anonymize_project_names);
        assert!(!team.redact_hostnames);
        assert!(team.redact_emails);

        // Personal is least restrictive
        assert!(!personal.redact_home_paths);
        assert!(!personal.redact_emails);
        assert!(!personal.redact_hostnames);

        // All block critical secrets
        assert!(public.block_on_critical_secrets);
        assert!(team.block_on_critical_secrets);
        assert!(personal.block_on_critical_secrets);
    }

    #[test]
    fn test_profile_descriptions_not_empty() {
        for profile in ShareProfile::all() {
            assert!(!profile.name().is_empty());
            assert!(!profile.description().is_empty());
            assert!(!profile.icon().is_empty());
        }
    }

    #[test]
    fn test_public_has_most_patterns() {
        let public = ShareProfile::Public.to_redaction_config();
        let team = ShareProfile::Team.to_redaction_config();
        let personal = ShareProfile::Personal.to_redaction_config();

        // Public should have the most patterns
        assert!(public.custom_patterns.len() >= 10);

        // Team should have fewer than public
        assert!(team.custom_patterns.len() < public.custom_patterns.len());

        // Personal should have the fewest
        assert!(personal.custom_patterns.len() <= 6);
    }

    #[test]
    fn test_profile_from_str() {
        use std::str::FromStr;
        assert_eq!(ShareProfile::from_str("public"), Ok(ShareProfile::Public));
        assert_eq!(ShareProfile::from_str("PUBLIC"), Ok(ShareProfile::Public));
        assert_eq!(ShareProfile::from_str("Team"), Ok(ShareProfile::Team));
        assert_eq!(
            ShareProfile::from_str("personal"),
            Ok(ShareProfile::Personal)
        );
        assert_eq!(ShareProfile::from_str("custom"), Ok(ShareProfile::Custom));
        assert!(ShareProfile::from_str("invalid").is_err());
    }

    #[test]
    fn test_profile_display() {
        assert_eq!(format!("{}", ShareProfile::Public), "🌐 Public");
        assert_eq!(format!("{}", ShareProfile::Team), "👥 Team");
    }

    #[test]
    fn test_default_profile() {
        let prefs = ProfilePreferences::default();
        assert_eq!(prefs.default_profile, ShareProfile::Team);
        assert!(prefs.last_used.is_none());
    }

    #[test]
    fn test_effective_profile() {
        let mut prefs = ProfilePreferences::default();
        assert_eq!(prefs.effective_profile(), ShareProfile::Team);

        prefs.set_last_used(ShareProfile::Public);
        assert_eq!(prefs.effective_profile(), ShareProfile::Public);
    }

    #[test]
    fn test_comparison_table_renders() {
        let table = render_profile_comparison();
        assert!(table.contains("Public"));
        assert!(table.contains("Team"));
        assert!(table.contains("Personal"));
        assert!(table.contains("✓"));
        assert!(table.contains("✗"));
    }

    #[test]
    fn test_profile_info_all() {
        let infos = ProfileInfo::all();
        assert_eq!(infos.len(), 4);
        assert!(infos.iter().any(|i| i.profile == ShareProfile::Public));
        assert!(infos.iter().any(|i| i.profile == ShareProfile::Custom));
    }

    #[test]
    fn test_serializable_config_default() {
        let config = SerializableRedactionConfig::default();
        assert!(config.redact_home_paths);
        assert!(config.block_on_critical_secrets);
    }

    #[test]
    fn test_profile_serialization() {
        let prefs = ProfilePreferences {
            default_profile: ShareProfile::Public,
            custom_config: None,
            last_used: Some(ShareProfile::Team),
        };

        let serialized = toml::to_string(&prefs).unwrap();
        let deserialized: ProfilePreferences = toml::from_str(&serialized).unwrap();

        assert_eq!(deserialized.default_profile, ShareProfile::Public);
        assert_eq!(deserialized.last_used, Some(ShareProfile::Team));
    }

    #[test]
    fn test_preferences_path_uses_default_data_dir() {
        let path = ProfilePreferences::default_path().expect("default path");
        assert_eq!(path, crate::default_data_dir().join("profile_prefs.toml"));
    }

    #[test]
    fn test_unique_atomic_temp_path_changes_each_call() {
        let final_path = std::path::Path::new("/tmp/profile_prefs.toml");
        let first = unique_atomic_temp_path(final_path);
        let second = unique_atomic_temp_path(final_path);
        assert_ne!(first, second);
    }

    #[test]
    fn test_replace_file_from_temp_overwrites_existing_file() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let final_path = temp_dir.path().join("profile_prefs.toml");
        let first_tmp = temp_dir.path().join("first.tmp");
        let second_tmp = temp_dir.path().join("second.tmp");

        std::fs::write(&first_tmp, "default_profile = \"team\"\n").unwrap();
        replace_file_from_temp(&first_tmp, &final_path).unwrap();
        assert!(final_path.exists());
        assert!(!first_tmp.exists());

        std::fs::write(&second_tmp, "default_profile = \"public\"\n").unwrap();
        replace_file_from_temp(&second_tmp, &final_path).unwrap();

        let content = std::fs::read_to_string(&final_path).unwrap();
        assert!(content.contains("public"));
    }
}
