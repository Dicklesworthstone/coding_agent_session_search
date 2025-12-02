//! Update checker for release notifications.
//!
//! Provides non-blocking release checking with:
//! - GitHub releases API integration
//! - Persistent state (last check time, skipped versions)
//! - Offline-friendly behavior (silent failure)
//! - Hourly check cadence (configurable)

use anyhow::{Context, Result};
use reqwest::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// How often to check for updates (1 hour default)
const CHECK_INTERVAL_SECS: u64 = 3600;

/// Timeout for HTTP requests (short to avoid blocking startup)
const HTTP_TIMEOUT_SECS: u64 = 5;

/// GitHub repo for release checks
const GITHUB_REPO: &str = "Dicklesworthstone/coding_agent_session_search";

/// Persistent state for update checker
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateState {
    /// Unix timestamp of last successful check
    pub last_check_ts: i64,
    /// Version string that user chose to skip (e.g., "0.2.0")
    pub skipped_version: Option<String>,
}

impl UpdateState {
    /// Load state from disk (synchronous)
    pub fn load() -> Self {
        let path = state_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Load state from disk (asynchronous)
    pub async fn load_async() -> Self {
        let path = state_path();
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save state to disk (synchronous)
    pub fn save(&self) -> Result<()> {
        let path = state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating update state directory {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Save state to disk (asynchronous)
    pub async fn save_async(&self) -> Result<()> {
        let path = state_path();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating update state directory {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        tokio::fs::write(&path, json)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Check if enough time has passed since last check
    pub fn should_check(&self) -> bool {
        let now = now_unix();
        (now - self.last_check_ts) >= CHECK_INTERVAL_SECS as i64
    }

    /// Mark that we just checked
    pub fn mark_checked(&mut self) {
        self.last_check_ts = now_unix();
    }

    /// Skip a specific version
    pub fn skip_version(&mut self, version: &str) {
        self.skipped_version = Some(version.to_string());
    }

    /// Check if a version is skipped
    pub fn is_skipped(&self, version: &str) -> bool {
        self.skipped_version.as_deref() == Some(version)
    }

    /// Clear skip preference (on upgrade or manual clear)
    pub fn clear_skip(&mut self) {
        self.skipped_version = None;
    }
}

/// Information about an available update
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Latest version available
    pub latest_version: String,
    /// Current running version
    pub current_version: String,
    /// URL to release notes
    pub release_url: String,
    /// Whether latest is newer than current
    pub is_newer: bool,
    /// Whether user has skipped this version
    pub is_skipped: bool,
}

impl UpdateInfo {
    /// Check if we should show the update banner
    pub fn should_show(&self) -> bool {
        self.is_newer && !self.is_skipped
    }
}

/// GitHub release API response (minimal fields)
#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
}

/// Check for updates asynchronously
///
/// Returns None if:
/// - Not enough time since last check
/// - Network error (offline-friendly)
/// - Parse error
/// - Already on latest
pub async fn check_for_updates(current_version: &str) -> Option<UpdateInfo> {
    let mut state = UpdateState::load_async().await;

    // Respect check interval
    if !state.should_check() {
        debug!("update check: skipping, checked recently");
        return None;
    }

    // Mark that we're checking (even if it fails)
    state.mark_checked();
    if let Err(e) = state.save_async().await {
        warn!("update check: failed to save state: {e}");
    }

    // Fetch latest release
    let release = match fetch_latest_release().await {
        Ok(r) => r,
        Err(e) => {
            debug!("update check: fetch failed (offline?): {e}");
            return None;
        }
    };

    // Parse versions
    let latest_str = release.tag_name.trim_start_matches('v');
    let latest = match Version::parse(latest_str) {
        Ok(v) => v,
        Err(e) => {
            debug!("update check: invalid version '{}': {e}", release.tag_name);
            return None;
        }
    };

    let current = match Version::parse(current_version) {
        Ok(v) => v,
        Err(e) => {
            debug!("update check: invalid current version '{current_version}': {e}");
            return None;
        }
    };

    let is_newer = latest > current;
    let is_skipped = state.is_skipped(latest_str);

    Some(UpdateInfo {
        latest_version: latest_str.to_string(),
        current_version: current_version.to_string(),
        release_url: release.html_url,
        is_newer,
        is_skipped,
    })
}

/// Force a check regardless of interval (for manual refresh)
pub async fn force_check(current_version: &str) -> Option<UpdateInfo> {
    let mut state = UpdateState::load_async().await;
    state.last_check_ts = 0; // Reset to force check
    if let Err(e) = state.save_async().await {
        warn!("update check: failed to reset state: {e}");
    }
    check_for_updates(current_version).await
}

/// Skip the specified version
pub fn skip_version(version: &str) -> Result<()> {
    let mut state = UpdateState::load();
    state.skip_version(version);
    state.save()
}

/// Dismiss update banner for this session (doesn't persist skip)
/// Returns true if there was an update to dismiss
pub fn dismiss_update() -> bool {
    // This is handled in-memory by the TUI, not persisted
    true
}

/// Open a URL in the system's default browser
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

/// Fetch latest release from GitHub API
async fn fetch_latest_release() -> Result<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");

    let client = Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent(concat!("cass/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building http client")?;

    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await
        .context("fetching release")?;

    if !response.status().is_success() {
        anyhow::bail!("GitHub API returned {}", response.status());
    }

    response
        .json::<GitHubRelease>()
        .await
        .context("parsing release JSON")
}

/// Get path to update state file
fn state_path() -> PathBuf {
    directories::ProjectDirs::from("com", "coding-agent-search", "coding-agent-search")
        .map(|dirs| dirs.data_dir().join("update_state.json"))
        .unwrap_or_else(|| PathBuf::from("update_state.json"))
}

/// Current unix timestamp
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ============================================================================
// Synchronous API for TUI (blocking HTTP)
// ============================================================================

/// Synchronous version of check_for_updates for use in sync TUI code.
/// Uses reqwest blocking client with short timeout.
pub fn check_for_updates_sync(current_version: &str) -> Option<UpdateInfo> {
    let mut state = UpdateState::load();

    // Respect check interval
    if !state.should_check() {
        debug!("update check: skipping, checked recently");
        return None;
    }

    // Mark that we're checking (even if it fails)
    state.mark_checked();
    if let Err(e) = state.save() {
        warn!("update check: failed to save state: {e}");
    }

    // Fetch latest release (blocking)
    let release = match fetch_latest_release_blocking() {
        Ok(r) => r,
        Err(e) => {
            debug!("update check: fetch failed (offline?): {e}");
            return None;
        }
    };

    // Parse versions
    let latest_str = release.tag_name.trim_start_matches('v');
    let latest = match Version::parse(latest_str) {
        Ok(v) => v,
        Err(e) => {
            debug!("update check: invalid version '{}': {e}", release.tag_name);
            return None;
        }
    };

    let current = match Version::parse(current_version) {
        Ok(v) => v,
        Err(e) => {
            debug!("update check: invalid current version '{current_version}': {e}");
            return None;
        }
    };

    let is_newer = latest > current;
    let is_skipped = state.is_skipped(latest_str);

    Some(UpdateInfo {
        latest_version: latest_str.to_string(),
        current_version: current_version.to_string(),
        release_url: release.html_url,
        is_newer,
        is_skipped,
    })
}

/// Fetch latest release using blocking HTTP client
fn fetch_latest_release_blocking() -> Result<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent(concat!("cass/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building http client")?;

    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .context("fetching release")?;

    if !response.status().is_success() {
        anyhow::bail!("GitHub API returned {}", response.status());
    }

    response
        .json::<GitHubRelease>()
        .context("parsing release JSON")
}

/// Start a background thread to check for updates.
/// Returns a receiver that will contain the result when ready.
pub fn spawn_update_check(
    current_version: String,
) -> std::sync::mpsc::Receiver<Option<UpdateInfo>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = check_for_updates_sync(&current_version);
        let _ = tx.send(result);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_should_check() {
        let mut state = UpdateState::default();
        assert!(state.should_check()); // Fresh state should check

        state.mark_checked();
        assert!(!state.should_check()); // Just checked, should not check again

        // Simulate time passing
        state.last_check_ts = now_unix() - CHECK_INTERVAL_SECS as i64 - 1;
        assert!(state.should_check()); // Enough time passed
    }

    #[test]
    fn test_skip_version() {
        let mut state = UpdateState::default();
        assert!(!state.is_skipped("1.0.0"));

        state.skip_version("1.0.0");
        assert!(state.is_skipped("1.0.0"));
        assert!(!state.is_skipped("1.0.1"));

        state.clear_skip();
        assert!(!state.is_skipped("1.0.0"));
    }

    #[test]
    fn test_update_info_should_show() {
        let info = UpdateInfo {
            latest_version: "1.0.0".into(),
            current_version: "0.9.0".into(),
            release_url: "https://example.com".into(),
            is_newer: true,
            is_skipped: false,
        };
        assert!(info.should_show());

        let skipped = UpdateInfo {
            is_skipped: true,
            ..info.clone()
        };
        assert!(!skipped.should_show());

        let not_newer = UpdateInfo {
            is_newer: false,
            ..info
        };
        assert!(!not_newer.should_show());
    }
}
