//! Update checker for release notifications.
//!
//! Provides non-blocking release checking with:
//! - GitHub releases API integration
//! - Persistent state (last check time, skipped versions)
//! - Offline-friendly behavior (silent failure)
//! - Hourly check cadence (configurable)

use anyhow::{Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// How often to check for updates (1 hour default)
const CHECK_INTERVAL_SECS: u64 = 3600;

/// Timeout for HTTP requests (short to avoid blocking startup)
const HTTP_TIMEOUT_SECS: u64 = 5;

/// GitHub repo for release checks
const GITHUB_REPO: &str = "Dicklesworthstone/coding_agent_session_search";
#[cfg(any(test, target_os = "macos", target_os = "linux"))]
const UNIX_INSTALL_ASSET: &str = "install.sh";
#[cfg(any(test, target_os = "windows"))]
const WINDOWS_INSTALL_ASSET: &str = "install.ps1";
const CHECKSUMS_ASSET: &str = "SHA256SUMS.txt";
const CHECKSUMS_ASSET_ALT: &str = "SHA256SUMS";
/// Standalone per-file checksum asset for the unix installer. Always published
/// alongside the release as a single line `<hash>  install.sh`, so it is a
/// last-resort fallback when both combined manifests omit the install.sh row
/// (defense-in-depth for the v0.6.10 self-update regression, issue #274).
#[cfg(any(test, target_os = "macos", target_os = "linux"))]
const UNIX_INSTALL_CHECKSUM_ASSET: &str = "install.sh.sha256";
/// Standalone per-file checksum asset for the Windows installer. Single line
/// `<hash>  install.ps1`. Same last-resort fallback role as the unix variant.
#[cfg(any(test, target_os = "windows"))]
const WINDOWS_INSTALL_CHECKSUM_ASSET: &str = "install.ps1.sha256";

fn updates_disabled() -> bool {
    dotenvy::var("CASS_SKIP_UPDATE").is_ok()
        || dotenvy::var("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT").is_ok()
        || dotenvy::var("TUI_HEADLESS").is_ok()
        || dotenvy::var("CI").is_ok()
}

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
            Err(_) => {
                let legacy = legacy_state_path();
                if legacy != path
                    && let Ok(content) = std::fs::read_to_string(&legacy)
                {
                    return serde_json::from_str(&content).unwrap_or_default();
                }
                Self::default()
            }
        }
    }

    /// Load state from disk (asynchronous)
    pub async fn load_async() -> Self {
        let path = state_path();
        match asupersync::fs::read_to_string(&path).await {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => {
                let legacy = legacy_state_path();
                if legacy != path
                    && let Ok(content) = asupersync::fs::read_to_string(&legacy).await
                {
                    return serde_json::from_str(&content).unwrap_or_default();
                }
                Self::default()
            }
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
        let temp_path = write_update_state_temp_file(&path, json.as_bytes())
            .with_context(|| format!("writing temporary update state for {}", path.display()))?;
        replace_update_state_file_from_temp(&temp_path, &path)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Save state to disk (asynchronous)
    pub async fn save_async(&self) -> Result<()> {
        let path = state_path();
        if let Some(parent) = path.parent() {
            asupersync::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating update state directory {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("serializing update state")?;
        let temp_path = write_update_state_temp_file_async(&path, json.as_bytes())
            .await
            .with_context(|| format!("writing temporary update state for {}", path.display()))?;
        replace_update_state_file_from_temp(&temp_path, &path)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Check if enough time has passed since last check
    pub fn should_check(&self) -> bool {
        let now = now_unix();
        if self.last_check_ts <= 0 || self.last_check_ts > now {
            return true;
        }
        now.saturating_sub(self.last_check_ts) >= CHECK_INTERVAL_SECS as i64
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
    /// Git tag name for the release
    pub tag_name: String,
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
/// - Not enough time since last successful check
/// - Network error (offline-friendly)
/// - Parse error
pub async fn check_for_updates(current_version: &str) -> Option<UpdateInfo> {
    check_for_updates_async_impl(current_version, false).await
}

async fn check_for_updates_async_impl(current_version: &str, force: bool) -> Option<UpdateInfo> {
    // Escape hatch for CI/CD or restricted environments
    if updates_disabled() {
        return None;
    }

    let mut state = UpdateState::load_async().await;

    // Respect check interval
    if !force && !state.should_check() {
        debug!("update check: skipping, checked recently");
        return None;
    }

    let release = match fetch_latest_release().await {
        Ok(r) => r,
        Err(e) => {
            debug!("update check: fetch failed (offline?): {e}");
            return None;
        }
    };

    let info = build_update_info(current_version, release, &state);

    // Persist cadence after any *successful fetch* — including when the
    // release metadata is unusable (non-semver tag, untrusted URL) — so a
    // bad upstream release cannot bypass the hourly throttle and turn every
    // startup into a fresh network request. Transient network errors above
    // still skip persistence so they do not suppress future checks.
    state.mark_checked();
    if let Err(e) = state.save_async().await {
        warn!("update check: failed to save state: {e}");
    }

    info
}

/// Force a check regardless of interval (for manual refresh)
pub async fn force_check(current_version: &str) -> Option<UpdateInfo> {
    check_for_updates_async_impl(current_version, true).await
}

/// Skip the specified version
pub fn skip_version(version: &str) -> Result<()> {
    let mut state = UpdateState::load();
    state.skip_version(version);
    state.save()
}

/// Open a URL in the system's default browser
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    validate_browser_url(url)?;

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
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

fn validate_browser_url(url: &str) -> std::io::Result<()> {
    if is_browser_url(url) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "release notes URL must be an absolute http(s) URL",
        ))
    }
}

fn is_browser_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if url_has_userinfo(&parsed) {
        return false;
    }
    matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some()
}

fn is_trusted_release_notes_url(url: &str, tag_name: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || url_has_userinfo(&parsed)
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return false;
    }

    let Some((expected_owner, expected_repo)) = GITHUB_REPO.split_once('/') else {
        return false;
    };
    let Some(mut path_segments) = parsed.path_segments() else {
        return false;
    };
    let Some(owner) = path_segments.next() else {
        return false;
    };
    let Some(repo) = path_segments.next() else {
        return false;
    };
    let Some(section) = path_segments.next() else {
        return false;
    };
    let Some(kind) = path_segments.next() else {
        return false;
    };
    let tag_path = path_segments.collect::<Vec<_>>().join("/");
    if tag_path.is_empty() {
        return false;
    }

    let tag_matches = release_tag_path_matches(&tag_path, tag_name);

    owner.eq_ignore_ascii_case(expected_owner)
        && repo.eq_ignore_ascii_case(expected_repo)
        && section == "releases"
        && kind == "tag"
        && tag_matches
}

fn url_has_userinfo(url: &url::Url) -> bool {
    !url.username().is_empty() || url.password().is_some()
}

fn release_tag_path_matches(tag_path: &str, tag_name: &str) -> bool {
    if tag_path == tag_name {
        return true;
    }
    urlencoding::decode(tag_path)
        .map(|decoded| decoded.as_ref() == tag_name)
        .unwrap_or(false)
}

fn release_asset_url(version: &str, asset: &str) -> String {
    format!("https://github.com/{GITHUB_REPO}/releases/download/{version}/{asset}")
}

fn parse_update_tag(tag: &str) -> Option<(&str, Version)> {
    if tag.trim() != tag {
        return None;
    }

    let version = tag.strip_prefix('v').unwrap_or(tag);
    let parsed = Version::parse(version).ok()?;
    Some((version, parsed))
}

fn is_valid_update_tag(tag: &str) -> bool {
    parse_update_tag(tag).is_some()
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
fn unix_self_update_script() -> &'static str {
    r#"
set -euo pipefail

tmp="$(mktemp -d "${TMPDIR:-/tmp}/cass-self-update.XXXXXX")"
cleanup() {
    rm -r "$tmp" 2>/dev/null || true
}
trap cleanup EXIT

script="$tmp/install.sh"
sums="$tmp/SHA256SUMS.txt"
curl -fsSL "$1" -o "$script"
expected=""
for checksums_url in "$2" "$4" "$5"; do
    [ -n "$checksums_url" ] || continue
    if ! curl -fsSL "$checksums_url" -o "$sums"; then
        continue
    fi
    candidate="$(awk '$2 == "install.sh" { print $1; exit }' "$sums")"
    if printf '%s' "$candidate" | grep -Eq '^[0-9a-fA-F]{64}$'; then
        expected="$candidate"
        break
    fi
done
if ! printf '%s' "$expected" | grep -Eq '^[0-9a-fA-F]{64}$'; then
    echo "install.sh checksum missing from release checksum manifests" >&2
    exit 1
fi
expected_lc="$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')"

if command -v sha256sum >/dev/null 2>&1; then
    printf '%s  %s\n' "$expected_lc" "$script" | sha256sum -c -
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$script" | awk '{ print $1 }' | tr '[:upper:]' '[:lower:]')"
    if [ "$actual" != "$expected_lc" ]; then
        echo "install.sh checksum mismatch" >&2
        exit 1
    fi
elif command -v openssl >/dev/null 2>&1; then
    actual="$(openssl dgst -sha256 "$script" | awk '{ print $NF }' | tr '[:upper:]' '[:lower:]')"
    if [ "$actual" != "$expected_lc" ]; then
        echo "install.sh checksum mismatch" >&2
        exit 1
    fi
else
    echo "No SHA-256 verification tool found" >&2
    exit 1
fi

exec bash "$script" --easy-mode --verify --version "$3"
"#
}

#[cfg(any(test, target_os = "windows"))]
fn windows_self_update_script() -> &'static str {
    r#"
$InstallUrl = $args[0]
$ChecksumsUrl = $args[1]
$Version = $args[2]
$Temp = Join-Path ([IO.Path]::GetTempPath()) ("cass-self-update-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $Temp -Force | Out-Null
try {
    $Script = Join-Path $Temp "install.ps1"
    $Sums = Join-Path $Temp "SHA256SUMS.txt"
    Invoke-WebRequest -Uri $InstallUrl -OutFile $Script -UseBasicParsing

    $Expected = $null
    foreach ($ChecksumsCandidateUrl in @($ChecksumsUrl, $args[3], $args[4])) {
        if (-not $ChecksumsCandidateUrl) {
            continue
        }
        try {
            Invoke-WebRequest -Uri $ChecksumsCandidateUrl -OutFile $Sums -UseBasicParsing
        } catch {
            continue
        }

        foreach ($Line in Get-Content -LiteralPath $Sums) {
            $Parts = $Line.Trim() -split '\s+', 2
            if ($Parts.Count -ge 2 -and $Parts[1] -eq "install.ps1" -and $Parts[0] -match '^[0-9a-fA-F]{64}$') {
                $Expected = $Parts[0].ToLowerInvariant()
                break
            }
        }
        if ($Expected) {
            break
        }
    }
    if (-not $Expected) {
        Write-Error "install.ps1 checksum missing from release checksum manifests"
        exit 1
    }

    $Actual = (Get-FileHash -LiteralPath $Script -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        Write-Error "install.ps1 checksum mismatch"
        exit 1
    }

    & $Script -EasyMode -Verify -Version $Version
    exit $LASTEXITCODE
} finally {
    Remove-Item -LiteralPath $Temp -Recurse -Force -ErrorAction SilentlyContinue
}
"#
}

/// Run the self-update installer script interactively.
/// This function does NOT return - it replaces the current process with the installer.
/// The caller should ensure the terminal is in a clean state before calling.
pub fn run_self_update(version: &str) -> ! {
    // Defense-in-depth: require the same release tag shape accepted from
    // GitHub metadata before interpolating the tag into release asset URLs.
    if !is_valid_update_tag(version) {
        eprintln!("Invalid version string: {}", version);
        std::process::exit(1);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::process::CommandExt;
        let install_url = release_asset_url(version, UNIX_INSTALL_ASSET);
        let checksums_url = release_asset_url(version, CHECKSUMS_ASSET);
        let checksums_alt_url = release_asset_url(version, CHECKSUMS_ASSET_ALT);
        let install_checksum_url = release_asset_url(version, UNIX_INSTALL_CHECKSUM_ASSET);
        // Use positional args instead of string interpolation to prevent injection.
        let err = std::process::Command::new("bash")
            .args([
                "-c",
                unix_self_update_script(),
                "cass-updater",
                &install_url,
                &checksums_url,
                version,
                &checksums_alt_url,
                &install_checksum_url,
            ])
            .exec();
        // If we get here, exec failed
        eprintln!("Failed to run installer: {}", err);
        std::process::exit(1);
    }

    #[cfg(target_os = "windows")]
    {
        let install_url = release_asset_url(version, WINDOWS_INSTALL_ASSET);
        let checksums_url = release_asset_url(version, CHECKSUMS_ASSET);
        let checksums_alt_url = release_asset_url(version, CHECKSUMS_ASSET_ALT);
        let install_checksum_url = release_asset_url(version, WINDOWS_INSTALL_CHECKSUM_ASSET);
        // Windows doesn't have exec(), so we spawn and wait.
        let status = std::process::Command::new("powershell")
            .args([
                "-ExecutionPolicy",
                "Bypass",
                "-NoProfile",
                "-Command",
                windows_self_update_script(),
                &install_url,
                &checksums_url,
                version,
                &checksums_alt_url,
                &install_checksum_url,
            ])
            .status();
        match status {
            Ok(s) => std::process::exit(s.code().unwrap_or(0)),
            Err(e) => {
                eprintln!("Failed to run installer: {}", e);
                std::process::exit(1);
            }
        }
    }
}

/// Get the base URL for release API. Overridable for testing via the
/// `CASS_UPDATE_API_BASE_URL` env var, but the override is validated
/// against an allow-list of schemes + hosts so a malicious `.env` or
/// shell environment can't redirect the release-metadata fetch to an
/// attacker-controlled server (beads
/// `coding_agent_session_search-87sqx`,
/// `coding_agent_session_search-6bvx8`). Allowed forms:
///   - `https://api.github.com/...`
///   - `https://github.com/...`
///   - `http://127.0.0.1:<port>...` (local integration tests)
///   - `http://localhost:<port>...` (local integration tests)
///
/// Any other value falls back to the default GitHub URL with a
/// one-shot stderr warning.
fn release_api_base_url() -> String {
    let default = || format!("https://api.github.com/repos/{GITHUB_REPO}");
    let Ok(override_url) = dotenvy::var("CASS_UPDATE_API_BASE_URL") else {
        return default();
    };
    if is_allowed_update_api_url(&override_url) {
        override_url
    } else {
        eprintln!(
            "warning: CASS_UPDATE_API_BASE_URL={override_url:?} ignored \
             (only GitHub HTTPS URLs or http://localhost/127.0.0.1 test endpoints allowed). \
             Falling back to the default GitHub release API."
        );
        default()
    }
}

/// Scheme + host allow-list check for `CASS_UPDATE_API_BASE_URL`
/// overrides. Kept as a small pure helper so the unit tests at the
/// bottom of this module can pin every accept/reject case
/// independently of the env-var plumbing.
fn is_allowed_update_api_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    if url_has_userinfo(&parsed) {
        return false;
    }

    match parsed.scheme() {
        "https" => matches!(host, "api.github.com" | "github.com"),
        "http" => matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]"),
        _ => false,
    }
}

/// Get path to update state file
fn state_path() -> PathBuf {
    crate::default_data_dir().join("update_state.json")
}

fn legacy_state_path() -> PathBuf {
    directories::ProjectDirs::from("com", "coding-agent-search", "coding-agent-search").map_or_else(
        || PathBuf::from("update_state.json"),
        |dirs| dirs.data_dir().join("update_state.json"),
    )
}

fn write_update_state_temp_file(path: &Path, contents: &[u8]) -> std::io::Result<PathBuf> {
    for _ in 0..100 {
        let temp_path = unique_update_state_temp_path(path);
        match write_update_state_temp_file_at(&temp_path, contents) {
            Ok(()) => return Ok(temp_path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "failed to allocate unique update state temp path for {}",
            path.display()
        ),
    ))
}

fn write_update_state_temp_file_at(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

async fn write_update_state_temp_file_async(
    path: &Path,
    contents: &[u8],
) -> std::io::Result<PathBuf> {
    for _ in 0..100 {
        let temp_path = unique_update_state_temp_path(path);
        match write_update_state_temp_file_at_async(&temp_path, contents).await {
            Ok(()) => return Ok(temp_path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "failed to allocate unique update state temp path for {}",
            path.display()
        ),
    ))
}

async fn write_update_state_temp_file_at_async(
    path: &Path,
    contents: &[u8],
) -> std::io::Result<()> {
    use asupersync::io::AsyncWriteExt;

    let mut file = asupersync::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(contents).await?;
    file.sync_all().await
}

fn replace_update_state_file_from_temp(temp_path: &Path, final_path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        match std::fs::rename(temp_path, final_path) {
            Ok(()) => sync_parent_directory(final_path),
            Err(first_err)
                if update_state_path_entry_exists(final_path)?
                    && matches!(
                        first_err.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                    ) =>
            {
                let backup_path = unique_update_state_backup_path(final_path);
                std::fs::rename(final_path, &backup_path).map_err(|backup_err| {
                    std::io::Error::other(format!(
                        "failed preparing backup {} before replacing {}: first error: {}; backup error: {}",
                        backup_path.display(),
                        final_path.display(),
                        first_err,
                        backup_err
                    ))
                })?;
                match std::fs::rename(temp_path, final_path) {
                    Ok(()) => sync_parent_directory(final_path),
                    Err(second_err) => match std::fs::rename(&backup_path, final_path) {
                        Ok(()) => {
                            sync_parent_directory(final_path)?;
                            Err(std::io::Error::other(format!(
                                "failed replacing {} with {}: first error: {}; second error: {}; restored original file; temp file retained at {}",
                                final_path.display(),
                                temp_path.display(),
                                first_err,
                                second_err,
                                temp_path.display()
                            )))
                        }
                        Err(restore_err) => Err(std::io::Error::other(format!(
                            "failed replacing {} with {}: first error: {}; second error: {}; restore error: {}; temp file retained at {}",
                            final_path.display(),
                            temp_path.display(),
                            first_err,
                            second_err,
                            restore_err,
                            temp_path.display()
                        ))),
                    },
                }
            }
            Err(err) => Err(err),
        }
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(temp_path, final_path)?;
        sync_parent_directory(final_path)
    }
}

#[cfg(any(windows, test))]
fn update_state_path_entry_exists(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => Ok(false),
        Err(err) => Err(std::io::Error::new(
            err.kind(),
            format!(
                "failed inspecting update state replacement target {}: {err}",
                path.display()
            ),
        )),
    }
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn unique_update_state_temp_path(path: &Path) -> PathBuf {
    unique_update_state_sidecar_path(path, "tmp")
}

#[cfg(windows)]
fn unique_update_state_backup_path(path: &Path) -> PathBuf {
    unique_update_state_sidecar_path(path, "bak")
}

fn unique_update_state_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("update_state.json");

    path.with_file_name(format!(
        ".{file_name}.{suffix}.{}.{}.{}",
        std::process::id(),
        timestamp,
        nonce
    ))
}

/// Current unix timestamp
fn now_unix() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}

// ============================================================================
// Synchronous API for TUI (blocking HTTP)
// ============================================================================

/// Synchronous version of `check_for_updates` for use in sync TUI code.
/// Uses a short-lived asupersync runtime and native HTTP client.
pub fn check_for_updates_sync(current_version: &str) -> Option<UpdateInfo> {
    if updates_disabled() {
        return None;
    }

    let mut state = UpdateState::load();

    // Respect check interval
    if !state.should_check() {
        debug!("update check: skipping, checked recently");
        return None;
    }

    // Fetch latest release (blocking)
    let release = match fetch_latest_release_blocking() {
        Ok(r) => r,
        Err(e) => {
            debug!("update check: fetch failed (offline?): {e}");
            return None;
        }
    };

    let info = build_update_info(current_version, release, &state);

    // Persist cadence after any *successful fetch* — including when the
    // release metadata is unusable (non-semver tag, untrusted URL) — so a
    // bad upstream release cannot bypass the hourly throttle and turn every
    // startup into a fresh network request. Transient network errors above
    // still skip persistence so they do not suppress future checks.
    state.mark_checked();
    if let Err(e) = state.save() {
        warn!("update check: failed to save state: {e}");
    }

    info
}

fn build_update_info(
    current_version: &str,
    release: GitHubRelease,
    state: &UpdateState,
) -> Option<UpdateInfo> {
    let GitHubRelease { tag_name, html_url } = release;
    let (latest_version, latest) = match parse_update_tag(&tag_name) {
        Some((version, parsed)) => (version.to_string(), parsed),
        None => {
            debug!("update check: invalid version tag '{}'", tag_name);
            return None;
        }
    };
    if !is_trusted_release_notes_url(&html_url, &tag_name) {
        debug!("update check: untrusted release notes URL '{}'", html_url);
        return None;
    }

    let current = match Version::parse(current_version) {
        Ok(v) => v,
        Err(e) => {
            debug!("update check: invalid current version '{current_version}': {e}");
            return None;
        }
    };
    let is_skipped = state.is_skipped(&latest_version);

    Some(UpdateInfo {
        latest_version,
        tag_name,
        current_version: current_version.to_string(),
        release_url: html_url,
        is_newer: latest > current,
        is_skipped,
    })
}

/// Fetch latest release without letting HTTP transport stalls block the async executor.
async fn fetch_latest_release() -> Result<GitHubRelease> {
    asupersync::runtime::spawn_blocking(fetch_latest_release_blocking).await
}

/// Fetch latest release using a short-timeout blocking HTTP client.
fn fetch_latest_release_blocking() -> Result<GitHubRelease> {
    let url = format!("{}/releases/latest", release_api_base_url());
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("cass/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .context("building update-check HTTP client")?;

    let response = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github.v3+json")
        .send()
        .with_context(|| format!("fetching release metadata from {url}"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("GitHub API returned {}", status.as_u16());
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
    if updates_disabled() {
        let _ = tx.send(None);
        return rx;
    }
    std::thread::spawn(move || {
        let result = check_for_updates_sync(&current_version);
        let _ = tx.send(result);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_release_asset_url_uses_immutable_release_downloads() {
        assert_eq!(
            release_asset_url("v1.2.3", UNIX_INSTALL_ASSET),
            format!(
                "https://github.com/{GITHUB_REPO}/releases/download/v1.2.3/{UNIX_INSTALL_ASSET}"
            )
        );
        assert_eq!(
            release_asset_url("v1.2.3", CHECKSUMS_ASSET),
            format!("https://github.com/{GITHUB_REPO}/releases/download/v1.2.3/{CHECKSUMS_ASSET}")
        );
        assert_eq!(
            release_asset_url("v1.2.3", CHECKSUMS_ASSET_ALT),
            format!(
                "https://github.com/{GITHUB_REPO}/releases/download/v1.2.3/{CHECKSUMS_ASSET_ALT}"
            )
        );
    }

    #[test]
    fn test_update_tag_validation_accepts_semver_release_tags() {
        for tag in [
            "1.2.3",
            "v1.2.3",
            "1.2.3-alpha.1",
            "v1.2.3-alpha.1",
            "1.2.3+build.5",
            "v1.2.3-alpha.1+build.5",
        ] {
            assert!(
                is_valid_update_tag(tag),
                "expected update tag {tag:?} to be accepted"
            );
        }
    }

    #[test]
    fn test_update_tag_validation_rejects_non_semver_or_pathlike_tags() {
        for tag in [
            "",
            "v",
            "..",
            "v..",
            "latest",
            "vlatest",
            "vv1.2.3",
            "1.2",
            "1",
            "1.2.3/",
            "1.2.3/../../main",
            " v1.2.3",
            "v1.2.3 ",
        ] {
            assert!(
                !is_valid_update_tag(tag),
                "expected update tag {tag:?} to be rejected"
            );
        }
    }

    #[test]
    fn test_unix_self_update_verifies_installer_script_before_running() {
        let script = unix_self_update_script();
        assert!(script.contains(CHECKSUMS_ASSET));
        assert!(
            script.contains(r#"for checksums_url in "$2" "$4" "$5"; do"#),
            "Unix self-update should try both combined manifests then the standalone per-file checksum"
        );
        assert!(script.contains(r#"expected="$candidate""#));
        assert!(script.contains(&format!(r#"$2 == "{UNIX_INSTALL_ASSET}""#)));
        assert!(script.contains("sha256sum -c -"));
        assert!(script.contains("shasum -a 256"));
        assert!(script.contains("openssl dgst -sha256"));
        assert!(script.contains(r#"exec bash "$script" --easy-mode --verify --version "$3""#));
    }

    #[test]
    fn test_unix_self_update_threads_standalone_checksum_asset_url() {
        // The standalone `install.sh.sha256` asset must be wired in as the
        // final positional arg ($5) so the loop can consult it when both
        // combined manifests omit the install.sh row (issue #274).
        let standalone_url = release_asset_url("v1.2.3", UNIX_INSTALL_CHECKSUM_ASSET);
        assert_eq!(
            standalone_url,
            format!(
                "https://github.com/{GITHUB_REPO}/releases/download/v1.2.3/{UNIX_INSTALL_CHECKSUM_ASSET}"
            )
        );
        assert_eq!(UNIX_INSTALL_CHECKSUM_ASSET, "install.sh.sha256");
        // The standalone manifest's second field is exactly `install.sh`, so
        // the existing awk parse works on it unchanged.
        let script = unix_self_update_script();
        assert!(script.contains(&format!(r#"$2 == "{UNIX_INSTALL_ASSET}""#)));
        assert!(script.contains(r#""$2" "$4" "$5""#));
    }

    #[test]
    fn test_windows_self_update_verifies_installer_script_before_running() {
        let script = windows_self_update_script();
        assert!(script.contains(CHECKSUMS_ASSET));
        assert!(
            script.contains(
                "foreach ($ChecksumsCandidateUrl in @($ChecksumsUrl, $args[3], $args[4]))"
            ),
            "Windows self-update should try both combined manifests then the standalone per-file checksum"
        );
        assert!(script.contains("Invoke-WebRequest -Uri $ChecksumsCandidateUrl -OutFile $Sums"));
        assert!(script.contains("if ($Expected)"));
        assert!(script.contains(&format!(r#"$Parts[1] -eq "{WINDOWS_INSTALL_ASSET}""#)));
        assert!(script.contains("Get-FileHash"));
        assert!(script.contains("-EasyMode -Verify -Version $Version"));
        assert!(script.contains("Remove-Item -LiteralPath $Temp"));
    }

    #[test]
    fn test_windows_self_update_threads_standalone_checksum_asset_url() {
        // The standalone `install.ps1.sha256` asset must be wired in as the
        // final positional arg ($args[4]) so the loop can consult it when both
        // combined manifests omit the install.ps1 row (issue #274).
        let standalone_url = release_asset_url("v1.2.3", WINDOWS_INSTALL_CHECKSUM_ASSET);
        assert_eq!(
            standalone_url,
            format!(
                "https://github.com/{GITHUB_REPO}/releases/download/v1.2.3/{WINDOWS_INSTALL_CHECKSUM_ASSET}"
            )
        );
        assert_eq!(WINDOWS_INSTALL_CHECKSUM_ASSET, "install.ps1.sha256");
        let script = windows_self_update_script();
        assert!(script.contains(&format!(r#"$Parts[1] -eq "{WINDOWS_INSTALL_ASSET}""#)));
        assert!(script.contains("@($ChecksumsUrl, $args[3], $args[4])"));
    }

    #[test]
    fn test_browser_url_validation_allows_absolute_web_urls() {
        assert!(is_browser_url(
            "https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v1.2.3"
        ));
        assert!(is_browser_url("http://localhost:8080/releases/v1.2.3"));
        assert!(is_browser_url(
            "https://github.com/releases/tag/v1.2.3?asset=install.sh&download=1"
        ));
    }

    #[test]
    fn test_browser_url_validation_rejects_non_web_or_relative_urls() {
        assert!(!is_browser_url(""));
        assert!(!is_browser_url("github.com/releases/tag/v1.2.3"));
        assert!(!is_browser_url("file:///etc/passwd"));
        assert!(!is_browser_url("javascript:alert(1)"));
        assert!(!is_browser_url("data:text/html,<script>alert(1)</script>"));
    }

    #[test]
    fn test_url_validation_rejects_userinfo_credentials() -> Result<(), &'static str> {
        for url in [
            "https://user:pass@github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v1.2.3",
            "http://user@localhost:8080/releases/v1.2.3",
        ] {
            if is_browser_url(url) {
                return Err("browser URL validation accepted embedded credentials");
            }
        }

        let state = UpdateState::default();
        let release = GitHubRelease {
            tag_name: "v9.9.9".to_string(),
            html_url: format!("https://token@github.com/{GITHUB_REPO}/releases/tag/v9.9.9"),
        };
        if build_update_info("1.0.0", release, &state).is_some() {
            return Err("release metadata accepted embedded credentials");
        }

        for url in [
            "https://token@api.github.com/repos/foo/bar",
            "https://token:secret@github.com/Dicklesworthstone/coding_agent_session_search/releases",
            "http://user@localhost:8080/api",
            "http://user:pass@[::1]:8080/api",
        ] {
            if is_allowed_update_api_url(url) {
                return Err("update API override accepted embedded credentials");
            }
        }

        Ok(())
    }

    #[test]
    fn test_release_info_rejects_untrusted_release_notes_urls() {
        let state = UpdateState::default();
        let release = GitHubRelease {
            tag_name: "v9.9.9".to_string(),
            html_url: "https://attacker.example/releases/tag/v9.9.9".to_string(),
        };
        assert!(
            build_update_info("1.0.0", release, &state).is_none(),
            "release metadata should not surface non-GitHub release notes URLs"
        );

        let release = GitHubRelease {
            tag_name: "v9.9.9".to_string(),
            html_url: "file:///tmp/release-notes.html".to_string(),
        };
        assert!(
            build_update_info("1.0.0", release, &state).is_none(),
            "release metadata should not surface non-web URLs"
        );

        let release = GitHubRelease {
            tag_name: "v9.9.9".to_string(),
            html_url: "https://github.com/other/project/releases/tag/v9.9.9".to_string(),
        };
        assert!(
            build_update_info("1.0.0", release, &state).is_none(),
            "release metadata should not surface unrelated GitHub release notes URLs"
        );

        let release = GitHubRelease {
            tag_name: "v9.9.9".to_string(),
            html_url: format!(
                "https://github.com/{GITHUB_REPO}/releases/download/v9.9.9/install.sh"
            ),
        };
        assert!(
            build_update_info("1.0.0", release, &state).is_none(),
            "release metadata should not accept release asset download URLs as release notes"
        );

        let release = GitHubRelease {
            tag_name: "v9.9.9".to_string(),
            html_url: format!("https://github.com/{GITHUB_REPO}/releases/tag/v9.9.8"),
        };
        assert!(
            build_update_info("1.0.0", release, &state).is_none(),
            "release metadata should not surface a release notes URL for a different tag"
        );

        let release = GitHubRelease {
            tag_name: "v9.9.9".to_string(),
            html_url: format!("https://github.com/{GITHUB_REPO}/releases/tag/v9.9.9?download=1"),
        };
        assert!(
            build_update_info("1.0.0", release, &state).is_none(),
            "release metadata should not accept release notes URLs with query strings"
        );

        let release = GitHubRelease {
            tag_name: "v9.9.9".to_string(),
            html_url: format!("https://github.com/{GITHUB_REPO}/releases/tag/v9.9.9#assets"),
        };
        assert!(
            build_update_info("1.0.0", release, &state).is_none(),
            "release metadata should not accept release notes URLs with fragments"
        );
    }

    #[test]
    fn test_release_info_accepts_exact_release_notes_url_for_tag() {
        let state = UpdateState::default();
        let release = GitHubRelease {
            tag_name: "v9.9.9+build.5".to_string(),
            html_url: format!("https://github.com/{GITHUB_REPO}/releases/tag/v9.9.9%2Bbuild.5"),
        };
        let info = build_update_info("1.0.0", release, &state)
            .expect("valid GitHub release notes URL should be accepted");

        assert_eq!(info.latest_version, "9.9.9+build.5");
        assert_eq!(info.tag_name, "v9.9.9+build.5");
        assert!(info.is_newer);
    }

    #[test]
    fn test_release_info_accepts_case_insensitive_encoded_plus_in_tag_url() {
        let state = UpdateState::default();
        let release = GitHubRelease {
            tag_name: "v9.9.9+build.5".to_string(),
            html_url: format!("https://github.com/{GITHUB_REPO}/releases/tag/v9.9.9%2bbuild.5"),
        };
        let info = build_update_info("1.0.0", release, &state)
            .expect("percent-encoded plus in a path segment is case-insensitive");

        assert_eq!(info.latest_version, "9.9.9+build.5");
        assert_eq!(info.tag_name, "v9.9.9+build.5");
        assert!(info.is_newer);
    }

    #[test]
    fn test_release_info_rejects_case_changed_tag_url() {
        let state = UpdateState::default();
        let release = GitHubRelease {
            tag_name: "v9.9.9+build.5".to_string(),
            html_url: format!("https://github.com/{GITHUB_REPO}/releases/tag/v9.9.9%2BBUILD.5"),
        };

        assert!(
            build_update_info("1.0.0", release, &state).is_none(),
            "tag names are case-sensitive; only percent escape hex case should be normalized"
        );
    }

    #[test]
    fn test_trusted_release_notes_url_accepts_full_tag_path_tail() {
        assert!(is_trusted_release_notes_url(
            &format!("https://github.com/{GITHUB_REPO}/releases/tag/channel/v9.9.9"),
            "channel/v9.9.9",
        ));
        assert!(is_trusted_release_notes_url(
            &format!("https://github.com/{GITHUB_REPO}/releases/tag/channel%2Fv9.9.9"),
            "channel/v9.9.9",
        ));
        assert!(!is_trusted_release_notes_url(
            &format!("https://github.com/{GITHUB_REPO}/releases/tag/v9.9.9/assets"),
            "v9.9.9",
        ));
    }

    #[test]
    fn update_state_sidecar_paths_use_pid_timestamp_and_nonce_namespace() -> anyhow::Result<()> {
        let sidecar = unique_update_state_temp_path(Path::new("/tmp/update_state.json"));
        let next_sidecar = unique_update_state_temp_path(Path::new("/tmp/update_state.json"));
        let file_name = sidecar
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("sidecar path has no UTF-8 file name"))?;
        let suffix = file_name
            .strip_prefix(".update_state.json.tmp.")
            .ok_or_else(|| anyhow::anyhow!("sidecar path lacks expected hidden temp prefix"))?;
        let mut parts = suffix.split('.');
        let pid = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("sidecar suffix lacks process id"))?;
        let timestamp = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("sidecar suffix lacks timestamp"))?;
        let nonce = parts
            .next()
            .ok_or_else(|| anyhow::anyhow!("sidecar suffix lacks nonce"))?;

        anyhow::ensure!(
            parts.next().is_none(),
            "unexpected sidecar suffix shape: {file_name:?}"
        );
        anyhow::ensure!(
            pid.parse::<u32>()? == std::process::id(),
            "sidecar process id should match this process"
        );
        let _timestamp = timestamp.parse::<u128>()?;
        let _nonce = nonce.parse::<u64>()?;
        anyhow::ensure!(
            sidecar != next_sidecar,
            "successive sidecar names should differ",
        );
        Ok(())
    }

    #[test]
    fn test_release_info_rejects_non_semver_release_tags() {
        let state = UpdateState::default();
        for tag in ["latest", "..", "vv9.9.9"] {
            let release = GitHubRelease {
                tag_name: tag.to_string(),
                html_url: format!("https://github.com/{GITHUB_REPO}/releases/tag/{tag}"),
            };
            assert!(
                build_update_info("1.0.0", release, &state).is_none(),
                "release metadata should not surface non-SemVer tag {tag:?}"
            );
        }
    }

    /// `coding_agent_session_search-87sqx` / `coding_agent_session_search-6bvx8`: the allow-list on
    /// `CASS_UPDATE_API_BASE_URL` must reject non-https overrides
    /// against non-loopback hosts and non-GitHub HTTPS hosts (malicious .env / shell pollution)
    /// while still permitting the `http://127.0.0.1:<port>` form the
    /// integration tests below use.
    #[test]
    fn test_is_allowed_update_api_url_allows_trusted_https_hosts() {
        assert!(is_allowed_update_api_url(
            "https://api.github.com/repos/foo"
        ));
        assert!(is_allowed_update_api_url(
            "https://api.github.com/repos/bar/baz"
        ));
        assert!(is_allowed_update_api_url(
            "https://github.com/Dicklesworthstone/coding_agent_session_search/releases"
        ));
    }

    #[test]
    fn test_is_allowed_update_api_url_rejects_untrusted_https_hosts() {
        assert!(!is_allowed_update_api_url("https://attacker.example.com"));
        assert!(!is_allowed_update_api_url("https://example.internal"));
        assert!(!is_allowed_update_api_url(
            "https://api.github.com.attacker.example/repos/foo"
        ));
        assert!(!is_allowed_update_api_url(
            "https://github.com.attacker.example/releases"
        ));
    }

    #[test]
    fn test_is_allowed_update_api_url_allows_http_loopback_only() {
        assert!(is_allowed_update_api_url("http://127.0.0.1:8080"));
        assert!(is_allowed_update_api_url("http://127.0.0.1:45123/api"));
        assert!(is_allowed_update_api_url("http://localhost:1234"));
        assert!(is_allowed_update_api_url("http://[::1]:8080"));
    }

    #[test]
    fn test_is_allowed_update_api_url_rejects_non_loopback_http() {
        assert!(!is_allowed_update_api_url("http://attacker.com"));
        assert!(!is_allowed_update_api_url("http://example.com/api"));
        // Prefix attack: host must match exactly, not be a prefix
        // of a longer attacker-controlled hostname.
        assert!(!is_allowed_update_api_url("http://127.0.0.1.attacker.com"));
        assert!(!is_allowed_update_api_url("http://localhost.attacker.com"));
    }

    #[test]
    fn test_is_allowed_update_api_url_rejects_other_schemes() {
        assert!(!is_allowed_update_api_url("ftp://api.github.com"));
        assert!(!is_allowed_update_api_url("file:///etc/passwd"));
        assert!(!is_allowed_update_api_url("gopher://example.com"));
        assert!(!is_allowed_update_api_url(""));
        assert!(!is_allowed_update_api_url("api.github.com"));
        // Empty-host https:// — reject so the URL parser doesn't see a
        // malformed-but-parseable URL.
        assert!(!is_allowed_update_api_url("https://"));
        assert!(!is_allowed_update_api_url("https:///path"));
    }

    #[test]
    #[serial]
    fn test_state_should_check() {
        let mut state = UpdateState::default();
        assert!(state.should_check()); // Fresh state should check

        state.mark_checked();
        assert!(!state.should_check()); // Just checked, should not check again

        // Simulate time passing
        state.last_check_ts = now_unix() - CHECK_INTERVAL_SECS as i64 - 1;
        assert!(state.should_check()); // Enough time passed

        // Future timestamps should not suppress checks indefinitely after
        // clock skew or state-file corruption.
        state.last_check_ts = now_unix() + CHECK_INTERVAL_SECS as i64;
        assert!(state.should_check());
    }

    #[test]
    #[serial]
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
    #[serial]
    fn update_check_state_remains_functional_without_session_dismiss_stub() {
        let state = UpdateState::default();
        assert!(
            state.should_check(),
            "fresh state should still trigger checks"
        );
        assert!(
            !state.is_skipped("9.9.9"),
            "default state should not invent skipped versions"
        );
    }

    #[test]
    #[serial]
    fn test_update_info_should_show() {
        let info = UpdateInfo {
            latest_version: "1.0.0".into(),
            tag_name: "v1.0.0".into(),
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

    // =========================================================================
    // Upgrade Process Tests
    // =========================================================================

    #[test]
    #[serial]
    fn test_version_comparison_upgrade_scenarios() {
        // Test various upgrade scenarios with semver comparison
        let test_cases = vec![
            ("0.1.50", "0.1.52", true, "patch upgrade"),
            ("0.1.52", "0.2.0", true, "minor upgrade"),
            ("0.1.52", "1.0.0", true, "major upgrade"),
            ("0.1.52", "0.1.52", false, "same version"),
            ("0.1.52", "0.1.51", false, "downgrade"),
            ("0.1.52", "0.1.52-alpha", false, "prerelease is older"),
            (
                "0.1.52-alpha",
                "0.1.52",
                true,
                "stable is newer than prerelease",
            ),
        ];

        for (current, latest, expected_newer, scenario) in test_cases {
            let current_ver = Version::parse(current).expect("valid current version");
            let latest_ver = Version::parse(latest).expect("valid latest version");
            let is_newer = latest_ver > current_ver;
            assert_eq!(
                is_newer, expected_newer,
                "scenario '{}': {} -> {} should be is_newer={}",
                scenario, current, latest, expected_newer
            );
        }
    }

    #[test]
    #[serial]
    fn test_update_state_persistence_round_trip() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let state_file = temp_dir.path().join("update_state.json");

        // Create state with specific values
        let mut state = UpdateState {
            last_check_ts: 1234567890,
            skipped_version: Some("0.1.50".to_string()),
        };

        // Write to temp location
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&state_file, &json).unwrap();

        // Read back
        let loaded: UpdateState =
            serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();

        assert_eq!(loaded.last_check_ts, 1234567890);
        assert_eq!(loaded.skipped_version, Some("0.1.50".to_string()));
        assert!(loaded.is_skipped("0.1.50"));
        assert!(!loaded.is_skipped("0.1.51"));

        // Modify and save again
        state.skip_version("0.1.51");
        state.mark_checked();
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&state_file, &json).unwrap();

        let loaded: UpdateState =
            serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
        assert!(loaded.is_skipped("0.1.51"));
        assert!(!loaded.is_skipped("0.1.50")); // Only latest skip is stored
    }

    #[cfg(unix)]
    #[test]
    fn update_state_replacement_path_entry_exists_detects_dangling_symlink() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp_dir = tempfile::TempDir::new()?;
        let state_file = temp_dir.path().join("update_state.json");
        let missing_target = temp_dir.path().join("missing-update-state.json");
        symlink(&missing_target, &state_file)?;

        match std::fs::metadata(&state_file) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => return Err(anyhow::anyhow!("dangling update state symlink resolved")),
            Err(err) => return Err(err.into()),
        }
        if !update_state_path_entry_exists(&state_file)? {
            return Err(anyhow::anyhow!(
                "update state replacement entry check missed dangling symlink {}",
                state_file.display()
            ));
        }

        Ok(())
    }

    #[cfg(unix)]
    fn install_update_state_symlink(data_dir: &std::path::Path) -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::symlink;

        let outside_dir = tempfile::TempDir::new().unwrap();
        let target_file = outside_dir.path().join("target-update-state.json");
        std::fs::write(&target_file, "untouched").unwrap();
        symlink(&target_file, data_dir.join("update_state.json")).unwrap();
        (outside_dir, target_file)
    }

    #[cfg(unix)]
    fn assert_update_state_symlink_was_replaced(
        data_dir: &std::path::Path,
        target_file: &std::path::Path,
        expected_ts: i64,
    ) {
        let state_file = data_dir.join("update_state.json");
        assert_eq!(
            std::fs::read_to_string(target_file).unwrap(),
            "untouched",
            "update state persistence must not follow an existing symlink"
        );
        assert!(
            !std::fs::symlink_metadata(&state_file)
                .unwrap()
                .file_type()
                .is_symlink(),
            "state path should be replaced with a regular JSON file"
        );

        let loaded: UpdateState =
            serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
        assert_eq!(loaded.last_check_ts, expected_ts);
        assert_eq!(loaded.skipped_version, Some("0.2.0".to_string()));
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn test_update_state_save_replaces_existing_symlink() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let (_outside_dir, target_file) = install_update_state_symlink(temp_dir.path());
        unsafe {
            std::env::set_var("CASS_DATA_DIR", temp_dir.path());
        }

        let state = UpdateState {
            last_check_ts: 42,
            skipped_version: Some("0.2.0".to_string()),
        };
        state.save().unwrap();

        unsafe {
            std::env::remove_var("CASS_DATA_DIR");
        }
        assert_update_state_symlink_was_replaced(temp_dir.path(), &target_file, 42);
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn test_update_state_save_async_replaces_existing_symlink() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let (_outside_dir, target_file) = install_update_state_symlink(temp_dir.path());
        unsafe {
            std::env::set_var("CASS_DATA_DIR", temp_dir.path());
        }

        let state = UpdateState {
            last_check_ts: 43,
            skipped_version: Some("0.2.0".to_string()),
        };
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        runtime.block_on(state.save_async()).unwrap();

        unsafe {
            std::env::remove_var("CASS_DATA_DIR");
        }
        assert_update_state_symlink_was_replaced(temp_dir.path(), &target_file, 43);
    }

    #[test]
    #[serial]
    fn test_update_info_upgrade_workflow() {
        // Simulate the full upgrade decision workflow

        // Case 1: New version available, not skipped -> should show
        let info = UpdateInfo {
            latest_version: "0.2.0".into(),
            tag_name: "v0.2.0".into(),
            current_version: "0.1.52".into(),
            release_url: "https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.0".into(),
            is_newer: true,
            is_skipped: false,
        };
        assert!(info.should_show(), "should show upgrade banner");
        assert!(info.is_newer, "should detect newer version");

        // Case 2: User skips this version
        let mut state = UpdateState::default();
        state.skip_version(&info.latest_version);
        assert!(state.is_skipped(&info.latest_version));

        // Now the info should not show (simulating re-check)
        let info_after_skip = UpdateInfo {
            is_skipped: state.is_skipped(&info.latest_version),
            ..info.clone()
        };
        assert!(
            !info_after_skip.should_show(),
            "should not show banner for skipped version"
        );

        // Case 3: New version beyond skipped -> should show again
        state.clear_skip();
        let newer_info = UpdateInfo {
            latest_version: "0.3.0".into(),
            tag_name: "v0.3.0".into(),
            current_version: "0.1.52".into(),
            release_url: "https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.3.0".into(),
            is_newer: true,
            is_skipped: false,
        };
        assert!(
            newer_info.should_show(),
            "should show banner for version newer than skipped"
        );
    }

    #[test]
    #[serial]
    fn test_check_interval_respects_cadence() {
        let mut state = UpdateState::default();

        // Fresh state should check
        assert!(state.should_check());

        // After checking, should not check again immediately
        state.mark_checked();
        assert!(!state.should_check());

        // After half the interval, still should not check
        state.last_check_ts = now_unix() - (CHECK_INTERVAL_SECS as i64 / 2);
        assert!(!state.should_check());

        // After full interval, should check again
        state.last_check_ts = now_unix() - CHECK_INTERVAL_SECS as i64 - 1;
        assert!(state.should_check());
    }

    #[test]
    #[serial]
    fn test_github_repo_constant_is_valid() {
        // Verify the repo constant is properly formatted
        assert!(GITHUB_REPO.contains('/'));
        let parts: Vec<&str> = GITHUB_REPO.split('/').collect();
        assert_eq!(parts.len(), 2, "should be owner/repo format");
        assert!(!parts[0].is_empty(), "owner should not be empty");
        assert!(!parts[1].is_empty(), "repo should not be empty");
        assert_eq!(parts[0], "Dicklesworthstone");
        assert_eq!(parts[1], "coding_agent_session_search");
    }

    // =========================================================================
    // Integration Tests with Local HTTP Server (br-e3ze)
    // Tests real HTTP client behavior against ephemeral local servers
    // =========================================================================

    /// Helper to create a simple HTTP response
    fn http_response(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {} {}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            status,
            match status {
                200 => "OK",
                404 => "Not Found",
                500 => "Internal Server Error",
                _ => "Unknown",
            },
            body.len(),
            body
        )
    }

    /// Start a simple HTTP server on an ephemeral port that serves a single response
    fn start_test_server(
        response_body: &str,
        status: u16,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
        use std::io::{ErrorKind, Read, Write};
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind to ephemeral port");
        let addr = listener.local_addr().expect("get local addr");
        let _ = listener.set_nonblocking(true);

        let response = http_response(status, response_body);
        let (ready_tx, ready_rx) = mpsc::channel();

        let handle = std::thread::spawn(move || {
            let _ = ready_tx.send(());
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(err)
                        if err.kind() == ErrorKind::WouldBlock && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            };

            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let mut buf = [0u8; 4096];
            match stream.read(&mut buf) {
                Ok(_) => {}
                Err(err)
                    if matches!(
                        err.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::UnexpectedEof
                    ) => {}
                Err(_) => return,
            }

            if stream.write_all(response.as_bytes()).is_ok() {
                let _ = stream.flush();
                std::thread::sleep(Duration::from_millis(25));
            }
        });

        let _ = ready_rx.recv_timeout(std::time::Duration::from_secs(1));

        (addr, handle)
    }

    #[test]
    #[serial]
    fn integration_fetch_release_success() {
        // Start local server with valid release JSON
        let release_json = r#"{
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.0"
        }"#;

        let (addr, handle) = start_test_server(release_json, 200);

        // Set env var to point to our local server
        // Safety: Tests run sequentially in same process, but this is still racy
        // We use a unique port each time so it's safe for our purposes
        unsafe {
            std::env::set_var("CASS_UPDATE_API_BASE_URL", format!("http://{}", addr));
        }

        // Make the request using blocking client
        let result = fetch_latest_release_blocking();

        // Clean up env var
        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        handle.join().expect("server thread");

        let release = result.expect("fetch should succeed");
        assert_eq!(release.tag_name, "v0.2.0");
        assert!(release.html_url.contains("v0.2.0"));
    }

    #[test]
    #[serial]
    fn integration_fetch_release_404_error() {
        let (addr, handle) = start_test_server(r#"{"message": "Not Found"}"#, 404);

        unsafe {
            std::env::set_var("CASS_UPDATE_API_BASE_URL", format!("http://{}", addr));
        }

        let result = fetch_latest_release_blocking();

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        handle.join().expect("server thread");

        assert!(result.is_err(), "should return error for 404");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("404") || err.to_string().contains("Not Found"),
            "error should mention 404: {}",
            err
        );
    }

    #[test]
    #[serial]
    fn integration_fetch_release_malformed_json() {
        let (addr, handle) = start_test_server("this is not json", 200);

        unsafe {
            std::env::set_var("CASS_UPDATE_API_BASE_URL", format!("http://{}", addr));
        }

        let result = fetch_latest_release_blocking();

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        handle.join().expect("server thread");

        assert!(result.is_err(), "should return error for malformed JSON");
    }

    #[test]
    #[serial]
    fn integration_fetch_release_missing_fields() {
        // JSON that doesn't have required fields
        let incomplete_json = r#"{"some_other_field": "value"}"#;

        let (addr, handle) = start_test_server(incomplete_json, 200);

        unsafe {
            std::env::set_var("CASS_UPDATE_API_BASE_URL", format!("http://{}", addr));
        }

        let result = fetch_latest_release_blocking();

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        handle.join().expect("server thread");

        // Should fail to parse because tag_name is missing
        assert!(result.is_err(), "should error on missing required fields");
    }

    #[test]
    #[serial]
    fn integration_fetch_release_server_error() {
        let (addr, handle) = start_test_server(r#"{"error": "Internal error"}"#, 500);

        unsafe {
            std::env::set_var("CASS_UPDATE_API_BASE_URL", format!("http://{}", addr));
        }

        let result = fetch_latest_release_blocking();

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        handle.join().expect("server thread");

        assert!(result.is_err(), "should return error for 500");
    }

    #[test]
    #[serial]
    fn integration_version_comparison_with_real_fetch() {
        // Test the full flow: fetch -> parse -> compare
        let release_json = r#"{
            "tag_name": "v0.3.0",
            "html_url": "https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.3.0"
        }"#;

        let (addr, handle) = start_test_server(release_json, 200);

        unsafe {
            std::env::set_var("CASS_UPDATE_API_BASE_URL", format!("http://{}", addr));
        }

        let result = fetch_latest_release_blocking();

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        handle.join().expect("server thread");

        let release = result.expect("fetch should succeed");

        // Parse and compare versions like the real code does
        let latest_str = release.tag_name.trim_start_matches('v');
        let latest = Version::parse(latest_str).expect("parse latest version");
        let current = Version::parse("0.1.50").expect("parse current version");

        assert!(latest > current, "0.3.0 should be newer than 0.1.50");
    }

    #[test]
    #[serial]
    fn integration_prerelease_version_handling() {
        // Test handling of pre-release versions from server
        let release_json = r#"{
            "tag_name": "v0.2.0-beta.1",
            "html_url": "https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v0.2.0-beta.1"
        }"#;

        let (addr, handle) = start_test_server(release_json, 200);

        unsafe {
            std::env::set_var("CASS_UPDATE_API_BASE_URL", format!("http://{}", addr));
        }

        let result = fetch_latest_release_blocking();

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        handle.join().expect("server thread");

        let release = result.expect("fetch should succeed");
        let latest_str = release.tag_name.trim_start_matches('v');
        let latest = Version::parse(latest_str).expect("parse prerelease version");

        // Prerelease 0.2.0-beta.1 should be less than 0.2.0
        let stable = Version::parse("0.2.0").expect("parse stable version");
        assert!(
            latest < stable,
            "prerelease 0.2.0-beta.1 should be older than stable 0.2.0"
        );

        // But newer than 0.1.50
        let older = Version::parse("0.1.50").expect("parse older version");
        assert!(
            latest > older,
            "prerelease 0.2.0-beta.1 should be newer than 0.1.50"
        );
    }

    #[test]
    #[serial]
    fn integration_connection_refused_is_offline_friendly() {
        // Point to a port that's not listening
        unsafe {
            std::env::set_var("CASS_UPDATE_API_BASE_URL", "http://127.0.0.1:1");
        }

        let result = fetch_latest_release_blocking();

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        // Should fail gracefully, not panic
        assert!(
            result.is_err(),
            "should return error when server unreachable"
        );
        // The error is wrapped in context, so check the full chain
        let err = result.unwrap_err();
        let err_chain = format!("{:?}", err).to_lowercase();
        assert!(
            err_chain.contains("connection")
                || err_chain.contains("connect")
                || err_chain.contains("refused")
                || err_chain.contains("fetch")
                || err_chain.contains("os error"),
            "should be a network/fetch error: {}",
            err_chain
        );
    }

    #[test]
    #[serial]
    fn integration_failed_sync_check_does_not_throttle_future_checks() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let state_file = temp_dir.path().join("update_state.json");
        unsafe {
            std::env::set_var("CASS_DATA_DIR", temp_dir.path());
            std::env::set_var("CASS_UPDATE_API_BASE_URL", "http://127.0.0.1:1");
            std::env::remove_var("CASS_SKIP_UPDATE");
            std::env::remove_var("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT");
            std::env::remove_var("TUI_HEADLESS");
            std::env::remove_var("CI");
        }

        let result = check_for_updates_sync("0.1.0");
        assert!(result.is_none(), "offline sync check should fail quietly");

        assert!(
            !state_file.exists(),
            "failed sync checks must not persist cadence state"
        );

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
            std::env::remove_var("CASS_DATA_DIR");
        }
    }

    #[test]
    #[serial]
    fn integration_failed_async_check_does_not_throttle_future_checks() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let state_file = temp_dir.path().join("update_state.json");
        unsafe {
            std::env::set_var("CASS_DATA_DIR", temp_dir.path());
            std::env::set_var("CASS_UPDATE_API_BASE_URL", "http://127.0.0.1:1");
            std::env::remove_var("CASS_SKIP_UPDATE");
            std::env::remove_var("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT");
            std::env::remove_var("TUI_HEADLESS");
            std::env::remove_var("CI");
        }

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let result = runtime.block_on(check_for_updates("0.1.0"));
        assert!(result.is_none(), "offline async check should fail quietly");

        assert!(
            !state_file.exists(),
            "failed async checks must not persist cadence state"
        );

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
            std::env::remove_var("CASS_DATA_DIR");
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn integration_force_check_bypasses_cadence_even_when_state_save_fails() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let state_file = temp_dir.path().join("update_state.json");
        let state = UpdateState {
            last_check_ts: now_unix(),
            skipped_version: None,
        };
        std::fs::write(&state_file, serde_json::to_string_pretty(&state).unwrap()).unwrap();

        let release_json = r#"{
            "tag_name": "v9.9.9",
            "html_url": "https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v9.9.9"
        }"#;
        let (addr, handle) = start_test_server(release_json, 200);

        let dir_metadata = std::fs::metadata(temp_dir.path()).unwrap();
        let file_metadata = std::fs::metadata(&state_file).unwrap();
        let dir_mode = dir_metadata.permissions().mode();
        let file_mode = file_metadata.permissions().mode();

        let mut readonly_dir = dir_metadata.permissions();
        readonly_dir.set_mode(0o555);
        std::fs::set_permissions(temp_dir.path(), readonly_dir).unwrap();

        let mut readonly_file = file_metadata.permissions();
        readonly_file.set_mode(0o444);
        std::fs::set_permissions(&state_file, readonly_file).unwrap();

        unsafe {
            std::env::set_var("CASS_DATA_DIR", temp_dir.path());
            std::env::set_var("CASS_UPDATE_API_BASE_URL", format!("http://{}", addr));
            std::env::remove_var("CASS_SKIP_UPDATE");
            std::env::remove_var("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT");
            std::env::remove_var("TUI_HEADLESS");
            std::env::remove_var("CI");
        }

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let result = runtime.block_on(force_check("0.1.0"));

        let mut restore_file = std::fs::metadata(&state_file).unwrap().permissions();
        restore_file.set_mode(file_mode);
        std::fs::set_permissions(&state_file, restore_file).unwrap();

        let mut restore_dir = std::fs::metadata(temp_dir.path()).unwrap().permissions();
        restore_dir.set_mode(dir_mode);
        std::fs::set_permissions(temp_dir.path(), restore_dir).unwrap();

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
            std::env::remove_var("CASS_DATA_DIR");
        }

        handle.join().expect("server thread");

        let info = result.expect("force check should bypass cadence and succeed");
        assert_eq!(info.latest_version, "9.9.9");
        assert!(info.is_newer);
    }

    #[test]
    #[serial]
    fn integration_blocking_fetch_release_success_v1() {
        // Validates the synchronous wrapper over the native HTTP client.
        let release_json = r#"{
            "tag_name": "v1.0.0",
            "html_url": "https://github.com/Dicklesworthstone/coding_agent_session_search/releases/tag/v1.0.0"
        }"#;

        let (addr, handle) = start_test_server(release_json, 200);

        unsafe {
            std::env::set_var("CASS_UPDATE_API_BASE_URL", format!("http://{}", addr));
        }

        let result = fetch_latest_release_blocking();

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        handle.join().expect("server thread");

        let release = result.expect("blocking fetch should succeed");
        assert_eq!(release.tag_name, "v1.0.0");
    }

    #[test]
    #[serial]
    fn integration_blocking_fetch_release_403_error() {
        let (addr, handle) = start_test_server(r#"{"error": "forbidden"}"#, 403);

        unsafe {
            std::env::set_var("CASS_UPDATE_API_BASE_URL", format!("http://{}", addr));
        }

        let result = fetch_latest_release_blocking();

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        handle.join().expect("server thread");

        assert!(result.is_err(), "should error on 403");
    }

    #[test]
    #[serial]
    fn integration_release_api_base_url_default() {
        // When env var is not set, should use GitHub API
        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        let url = release_api_base_url();
        assert!(
            url.contains("api.github.com"),
            "default should use GitHub API"
        );
        assert!(
            url.contains(GITHUB_REPO),
            "default should include repo path"
        );
    }

    #[test]
    #[serial]
    fn integration_release_api_base_url_override() {
        let custom_url = "http://localhost:8080/api";
        unsafe {
            std::env::set_var("CASS_UPDATE_API_BASE_URL", custom_url);
        }

        let url = release_api_base_url();

        unsafe {
            std::env::remove_var("CASS_UPDATE_API_BASE_URL");
        }

        assert_eq!(url, custom_url, "should use custom URL from env var");
    }

    #[test]
    #[serial]
    fn integration_http_timeout_is_reasonable() {
        const _: () = {
            // Verify the timeout constant is short enough for startup
            assert!(
                HTTP_TIMEOUT_SECS <= 10,
                "HTTP timeout should be short to avoid blocking startup"
            );
            assert!(
                HTTP_TIMEOUT_SECS >= 3,
                "HTTP timeout should be long enough for slow networks"
            );
        };
    }

    #[test]
    #[serial]
    fn integration_check_interval_is_reasonable() {
        const _: () = {
            // Verify check interval is reasonable (not too frequent, not too rare)
            assert!(
                CHECK_INTERVAL_SECS >= 3600,
                "should not check more than once per hour"
            );
            assert!(
                CHECK_INTERVAL_SECS <= 86400,
                "should check at least once per day"
            );
        };
    }
}
