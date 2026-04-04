//! GitHub Pages deployment module.
//!
//! Deploys encrypted archives to GitHub Pages using the gh CLI.
//! Creates a repository, pushes to gh-pages branch, and enables Pages.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Maximum number of retry attempts for network operations
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (milliseconds)
const BASE_DELAY_MS: u64 = 1000;

/// Maximum site size for GitHub Pages (1 GB)
const MAX_SITE_SIZE_BYTES: u64 = 1024 * 1024 * 1024;

/// Warning threshold for file size (50 MiB)
const FILE_SIZE_WARNING_BYTES: u64 = 50 * 1024 * 1024;

/// Maximum file size for GitHub (100 MiB)
const MAX_FILE_SIZE_BYTES: u64 = 100 * 1024 * 1024;

/// Prerequisites for GitHub Pages deployment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prerequisites {
    /// gh CLI version if installed
    pub gh_version: Option<String>,
    /// Whether gh CLI is authenticated
    pub gh_authenticated: bool,
    /// GitHub username if authenticated
    pub gh_username: Option<String>,
    /// Git version if installed
    pub git_version: Option<String>,
    /// Available disk space in MB
    pub disk_space_mb: u64,
    /// Estimated bundle size in MB
    pub estimated_size_mb: u64,
}

impl Prerequisites {
    /// Check if all prerequisites are met
    pub fn is_ready(&self) -> bool {
        self.gh_version.is_some() && self.gh_authenticated && self.git_version.is_some()
    }

    /// Get a list of missing prerequisites
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.gh_version.is_none() {
            missing.push("gh CLI not installed (install from https://cli.github.com)");
        }
        if !self.gh_authenticated {
            missing.push("gh CLI not authenticated (run 'gh auth login')");
        }
        if self.git_version.is_none() {
            missing.push("git not installed");
        }
        missing
    }
}

/// File size check result
#[derive(Debug, Clone)]
pub struct SizeCheck {
    /// Total size of all files in bytes
    pub total_bytes: u64,
    /// Number of files
    pub file_count: usize,
    /// Files exceeding warning threshold
    pub large_files: Vec<(String, u64)>,
    /// Whether total size exceeds limit
    pub exceeds_limit: bool,
    /// Whether any file exceeds max file size
    pub has_oversized_files: bool,
}

/// Deployment result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployResult {
    /// Repository URL
    pub repo_url: String,
    /// Pages URL (where the site is accessible)
    pub pages_url: String,
    /// Whether Pages was successfully enabled
    pub pages_enabled: bool,
    /// Deployment commit SHA
    pub commit_sha: String,
}

/// GitHub Pages deployer
pub struct GitHubDeployer {
    /// Repository name
    repo_name: String,
    /// Repository description
    description: String,
    /// Whether to make the repo public
    public: bool,
    /// Whether to force overwrite existing repo
    force: bool,
}

impl Default for GitHubDeployer {
    fn default() -> Self {
        Self::new("cass-archive")
    }
}

impl GitHubDeployer {
    /// Create a new deployer with the given repository name
    pub fn new(repo_name: impl Into<String>) -> Self {
        Self {
            repo_name: repo_name.into(),
            description: "Encrypted cass archive".to_string(),
            public: true,
            force: false,
        }
    }

    /// Set the repository description
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Set whether the repository should be public
    pub fn public(mut self, public: bool) -> Self {
        self.public = public;
        self
    }

    /// Set whether to force overwrite existing repository
    pub fn force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Check deployment prerequisites
    pub fn check_prerequisites(&self) -> Result<Prerequisites> {
        // Check gh CLI
        let gh_version = get_gh_version();
        let (gh_authenticated, gh_username) = if gh_version.is_some() {
            check_gh_auth()
        } else {
            (false, None)
        };

        // Check git
        let git_version = get_git_version();

        // Check disk space (simplified - just get available space)
        let disk_space_mb = get_available_space_mb().unwrap_or(0);

        Ok(Prerequisites {
            gh_version,
            gh_authenticated,
            gh_username,
            git_version,
            disk_space_mb,
            estimated_size_mb: 0, // Set by caller if known
        })
    }

    /// Check size of bundle directory
    pub fn check_size(&self, bundle_dir: &Path) -> Result<SizeCheck> {
        let bundle_dir = resolve_deploy_site_dir(bundle_dir)?;
        let mut total_bytes = 0u64;
        let mut file_count = 0usize;
        let mut large_files = Vec::new();
        let mut has_oversized = false;

        visit_files(&bundle_dir, &mut |path, size| {
            total_bytes += size;
            file_count += 1;

            if size > MAX_FILE_SIZE_BYTES {
                has_oversized = true;
                let rel_path = path
                    .strip_prefix(bundle_dir.as_path())
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                large_files.push((rel_path, size));
            } else if size > FILE_SIZE_WARNING_BYTES {
                let rel_path = path
                    .strip_prefix(bundle_dir.as_path())
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                large_files.push((rel_path, size));
            }
        })?;

        Ok(SizeCheck {
            total_bytes,
            file_count,
            large_files,
            exceeds_limit: total_bytes > MAX_SITE_SIZE_BYTES,
            has_oversized_files: has_oversized,
        })
    }

    /// Deploy bundle to GitHub Pages
    ///
    /// # Arguments
    /// * `bundle_dir` - Path to the site/ directory from bundle builder
    /// * `progress` - Progress callback (phase, message)
    pub fn deploy<P: AsRef<Path>>(
        &self,
        bundle_dir: P,
        mut progress: impl FnMut(&str, &str),
    ) -> Result<DeployResult> {
        let bundle_dir = resolve_deploy_site_dir(bundle_dir.as_ref())?;

        // Step 1: Check prerequisites
        progress("prereq", "Checking prerequisites...");
        let prereqs = self.check_prerequisites()?;

        if !prereqs.is_ready() {
            let missing = prereqs.missing();
            bail!("Prerequisites not met:\n{}", missing.join("\n"));
        }

        let username = prereqs
            .gh_username
            .as_ref()
            .context("Could not determine GitHub username")?;

        // Step 2: Check size
        progress("size", "Checking bundle size...");
        let size_check = self.check_size(&bundle_dir)?;

        if size_check.exceeds_limit {
            bail!(
                "Bundle size ({:.1} MB) exceeds GitHub Pages limit ({:.1} MB)",
                size_check.total_bytes as f64 / (1024.0 * 1024.0),
                MAX_SITE_SIZE_BYTES as f64 / (1024.0 * 1024.0)
            );
        }

        if size_check.has_oversized_files {
            let oversized: Vec<_> = size_check
                .large_files
                .iter()
                .filter(|(_, size)| *size > MAX_FILE_SIZE_BYTES)
                .map(|(path, size)| {
                    format!("  {} ({:.1} MB)", path, *size as f64 / (1024.0 * 1024.0))
                })
                .collect();
            bail!(
                "Files exceed GitHub's 100 MiB limit:\n{}",
                oversized.join("\n")
            );
        }

        // Warn about large files (above 50 MiB but under 100 MiB)
        let warning_files: Vec<_> = size_check
            .large_files
            .iter()
            .filter(|(_, size)| *size <= MAX_FILE_SIZE_BYTES && *size > FILE_SIZE_WARNING_BYTES)
            .collect();
        if !warning_files.is_empty() {
            let warnings: Vec<_> = warning_files
                .iter()
                .map(|(path, size)| {
                    format!("{} ({:.1} MB)", path, *size as f64 / (1024.0 * 1024.0))
                })
                .collect();
            progress(
                "warning",
                &format!(
                    "Large files detected (may slow deployment): {}",
                    warnings.join(", ")
                ),
            );
        }

        // Step 3: Create or verify repository
        progress("repo", "Creating repository...");
        let repo_url = self.ensure_repository(username)?;

        // Step 4: Clone to temp directory
        progress("clone", "Cloning repository...");
        let temp_dir = create_temp_dir()?;
        clone_repo(&repo_url, &temp_dir)?;

        // Step 5: Copy bundle contents
        progress("copy", "Copying bundle files...");
        let work_dir = temp_dir.join(&self.repo_name);
        copy_bundle_to_repo(&bundle_dir, &work_dir)?;
        configure_git_identity(&work_dir, username)?;

        // Step 6: Create orphan branch and push
        progress("push", "Pushing to gh-pages branch...");
        let commit_sha = push_gh_pages(&work_dir)?;

        // Step 7: Enable GitHub Pages
        progress("pages", "Enabling GitHub Pages...");
        let pages_enabled = enable_github_pages(username, &self.repo_name);

        // Construct URLs
        let pages_url = format!("https://{}.github.io/{}", username, self.repo_name);

        progress("complete", "Deployment complete!");

        Ok(DeployResult {
            repo_url,
            pages_url,
            pages_enabled,
            commit_sha,
        })
    }

    /// Ensure repository exists, create if needed
    fn ensure_repository(&self, username: &str) -> Result<String> {
        let repo_full_name = format!("{}/{}", username, self.repo_name);

        // Check if repo exists
        let exists = check_repo_exists(&repo_full_name);

        if exists && !self.force {
            bail!(
                "Repository {} already exists. Use --force to overwrite.",
                repo_full_name
            );
        }

        if !exists {
            // Create repository
            let visibility = if self.public { "--public" } else { "--private" };
            let output = Command::new("gh")
                .args([
                    "repo",
                    "create",
                    &self.repo_name,
                    visibility,
                    "--description",
                    &self.description,
                ])
                .output()
                .context("Failed to run gh repo create")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("Failed to create repository: {}", stderr);
            }
        }

        Ok(format!("https://github.com/{}", repo_full_name))
    }
}

// Helper functions

/// Create a temporary directory
fn create_temp_dir() -> Result<PathBuf> {
    let temp_base = std::env::temp_dir();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let dir_name = format!("cass-deploy-{}-{}", pid, timestamp);
    let temp_dir = temp_base.join(dir_name);
    std::fs::create_dir_all(&temp_dir)?;
    Ok(temp_dir)
}

fn resolve_deploy_site_dir(path: &Path) -> Result<PathBuf> {
    super::resolve_site_dir(path)
}

/// Get gh CLI version
fn get_gh_version() -> Option<String> {
    Command::new("gh")
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                stdout.lines().next().map(|s| s.to_string())
            } else {
                None
            }
        })
}

/// Check gh authentication status
fn check_gh_auth() -> (bool, Option<String>) {
    let output = Command::new("gh").args(["auth", "status"]).output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let combined = format!("{}{}", stdout, stderr);

            // Parse username from output like "Logged in to github.com as username"
            let username = combined
                .lines()
                .find(|line| line.contains("Logged in to"))
                .and_then(|line| line.split(" as ").nth(1))
                .map(|s| s.split_whitespace().next().unwrap_or(s).to_string());

            (true, username)
        }
        _ => (false, None),
    }
}

/// Get git version
fn get_git_version() -> Option<String> {
    Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                Some(stdout.trim().to_string())
            } else {
                None
            }
        })
}

/// Get available disk space in MB
fn get_available_space_mb() -> Option<u64> {
    // Use df on Unix, simplified approach
    #[cfg(unix)]
    {
        Command::new("df")
            .args(["-m", "."])
            .output()
            .ok()
            .and_then(|out| {
                if out.status.success() {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    // Parse second line, fourth column (available)
                    stdout
                        .lines()
                        .nth(1)
                        .and_then(|line| line.split_whitespace().nth(3))
                        .and_then(|s| s.parse().ok())
                } else {
                    None
                }
            })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Check if repository exists
fn check_repo_exists(repo_full_name: &str) -> bool {
    Command::new("gh")
        .args(["repo", "view", repo_full_name])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Retry a fallible operation with exponential backoff.
///
/// Retries the operation up to `MAX_RETRIES` times with exponentially
/// increasing delays between attempts. Useful for network operations
/// that may transiently fail.
fn retry_with_backoff<T, F>(operation_name: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    let mut last_error = None;

    for attempt in 0..MAX_RETRIES {
        match f() {
            Ok(result) => return Ok(result),
            Err(e) => {
                last_error = Some(e);
                if attempt + 1 < MAX_RETRIES {
                    let delay_ms = BASE_DELAY_MS * (1 << attempt); // 1s, 2s, 4s
                    eprintln!(
                        "[{}] Attempt {} failed, retrying in {}ms...",
                        operation_name,
                        attempt + 1,
                        delay_ms
                    );
                    thread::sleep(Duration::from_millis(delay_ms));
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        anyhow::anyhow!("{} failed after {} attempts", operation_name, MAX_RETRIES)
    }))
}

/// Clone repository to directory with retry logic
fn clone_repo(repo_url: &str, dest: &Path) -> Result<()> {
    retry_with_backoff("git clone", || {
        let output = Command::new("git")
            .args(["clone", repo_url])
            .current_dir(dest)
            .output()
            .context("Failed to run git clone")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Allow empty repo warning
            if !stderr.contains("empty repository") {
                bail!("Failed to clone repository: {}", stderr);
            }
        }

        Ok(())
    })
}

/// Copy bundle contents to repository directory
fn copy_bundle_to_repo(bundle_dir: &Path, repo_dir: &Path) -> Result<()> {
    let bundle_dir = resolve_deploy_site_dir(bundle_dir)?;

    // Clear existing files (except .git)
    if repo_dir.exists() {
        for entry in std::fs::read_dir(repo_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.file_name().map(|n| n != ".git").unwrap_or(true) {
                if path.is_dir() {
                    std::fs::remove_dir_all(&path)?;
                } else {
                    std::fs::remove_file(&path)?;
                }
            }
        }
    } else {
        std::fs::create_dir_all(repo_dir)?;
    }

    // Copy bundle files
    copy_dir_recursive(&bundle_dir, repo_dir)?;

    // Ensure .nojekyll exists
    let nojekyll = repo_dir.join(".nojekyll");
    if !nojekyll.exists() {
        std::fs::write(&nojekyll, "")?;
    }

    Ok(())
}

/// Copy directory recursively
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    let canonical_base = src.canonicalize().with_context(|| {
        format!(
            "Failed to resolve deployment source root {} before copying",
            src.display()
        )
    })?;
    copy_dir_recursive_inner(src, dst, &canonical_base)
}

fn copy_dir_recursive_inner(src: &Path, dst: &Path, canonical_base: &Path) -> Result<()> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&src_path)?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            let canonical_target = src_path.canonicalize().with_context(|| {
                format!(
                    "Failed to resolve symlinked deploy entry {}",
                    src_path.display()
                )
            })?;
            if !canonical_target.starts_with(canonical_base) {
                bail!(
                    "Refusing to deploy symlinked site entry outside deployment root: {}",
                    src_path.display()
                );
            }

            let target_meta = std::fs::metadata(&src_path).with_context(|| {
                format!(
                    "Failed to inspect symlink target for deploy entry {}",
                    src_path.display()
                )
            })?;
            if !target_meta.is_file() {
                bail!(
                    "Refusing to deploy symlinked site entry that does not point to a regular file: {}",
                    src_path.display()
                );
            }

            std::fs::copy(&canonical_target, &dst_path).with_context(|| {
                format!(
                    "Failed copying symlink target {} to {} during deploy staging",
                    canonical_target.display(),
                    dst_path.display()
                )
            })?;
            continue;
        }

        if file_type.is_dir() {
            copy_dir_recursive_inner(&src_path, &dst_path, canonical_base)?;
        } else if file_type.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

/// Configure a local git identity for commits in the temporary deployment clone.
fn configure_git_identity(repo_dir: &Path, username: &str) -> Result<()> {
    let email = format!("{username}@users.noreply.github.com");

    for (key, value) in [("user.name", username), ("user.email", email.as_str())] {
        let output = Command::new("git")
            .args(["config", key, value])
            .current_dir(repo_dir)
            .output()
            .with_context(|| format!("Failed to set git {key}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to set git {key}: {stderr}");
        }
    }

    Ok(())
}

/// Push to gh-pages branch as orphan
fn push_gh_pages(repo_dir: &Path) -> Result<String> {
    // Create orphan branch
    let output = Command::new("git")
        .args(["checkout", "--orphan", "gh-pages"])
        .current_dir(repo_dir)
        .output()
        .context("Failed to create orphan branch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to create gh-pages branch: {}", stderr);
    }

    // Add all files
    let output = Command::new("git")
        .args(["add", "-A"])
        .current_dir(repo_dir)
        .output()
        .context("Failed to git add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to add files: {}", stderr);
    }

    // Commit
    let output = Command::new("git")
        .args(["commit", "-m", "Deploy cass archive"])
        .current_dir(repo_dir)
        .output()
        .context("Failed to git commit")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to commit: {}", stderr);
    }

    // Get commit SHA
    let sha_output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .context("Failed to get commit SHA")?;

    let commit_sha = String::from_utf8_lossy(&sha_output.stdout)
        .trim()
        .to_string();

    // Force push to origin with retry for network errors
    let repo_dir_owned = repo_dir.to_owned();
    retry_with_backoff("git push", move || {
        let output = Command::new("git")
            .args(["push", "-f", "origin", "gh-pages"])
            .current_dir(&repo_dir_owned)
            .output()
            .context("Failed to git push")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to push: {}", stderr);
        }

        Ok(())
    })?;

    Ok(commit_sha)
}

/// Enable GitHub Pages via API with retry logic
fn enable_github_pages(username: &str, repo_name: &str) -> bool {
    let api_path = format!("repos/{}/{}/pages", username, repo_name);

    // Try with retry - may fail if already enabled, which is okay
    let result = retry_with_backoff("enable Pages", || {
        let output = Command::new("gh")
            .args([
                "api",
                &api_path,
                "-X",
                "POST",
                "-f",
                "source[branch]=gh-pages",
                "-f",
                "source[path]=/",
            ])
            .output()
            .context("Failed to call GitHub API")?;

        if output.status.success() {
            Ok(true)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // If Pages is already enabled, that's fine
            if stderr.contains("already exists") || stderr.contains("409") {
                Ok(true)
            } else {
                bail!("Failed to enable Pages: {}", stderr);
            }
        }
    });

    result.unwrap_or(false)
}

/// Visit all files in a directory recursively
fn visit_files(dir: &Path, f: &mut impl FnMut(&Path, u64)) -> Result<()> {
    let canonical_base = dir.canonicalize().with_context(|| {
        format!(
            "Failed to resolve deployment source root {} before sizing",
            dir.display()
        )
    })?;
    visit_files_inner(dir, &canonical_base, f)
}

fn visit_files_inner(
    dir: &Path,
    canonical_base: &Path,
    f: &mut impl FnMut(&Path, u64),
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();

        if file_type.is_symlink() {
            let canonical_target = path.canonicalize().with_context(|| {
                format!(
                    "Failed to resolve symlinked deploy entry {}",
                    path.display()
                )
            })?;
            if !canonical_target.starts_with(canonical_base) {
                bail!(
                    "Refusing to deploy symlinked site entry outside deployment root: {}",
                    path.display()
                );
            }

            let target_meta = std::fs::metadata(&path).with_context(|| {
                format!(
                    "Failed to inspect symlink target for deploy entry {}",
                    path.display()
                )
            })?;
            if !target_meta.is_file() {
                bail!(
                    "Refusing to deploy symlinked site entry that does not point to a regular file: {}",
                    path.display()
                );
            }

            f(&path, target_meta.len());
            continue;
        }

        if file_type.is_dir() {
            visit_files_inner(&path, canonical_base, f)?;
        } else if file_type.is_file() {
            f(&path, metadata.len());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prerequisites_is_ready() {
        let prereqs = Prerequisites {
            gh_version: Some("gh version 2.0.0".to_string()),
            gh_authenticated: true,
            gh_username: Some("testuser".to_string()),
            git_version: Some("git version 2.30.0".to_string()),
            disk_space_mb: 1000,
            estimated_size_mb: 100,
        };

        assert!(prereqs.is_ready());
        assert!(prereqs.missing().is_empty());
    }

    #[test]
    fn test_prerequisites_not_ready() {
        let prereqs = Prerequisites {
            gh_version: None,
            gh_authenticated: false,
            gh_username: None,
            git_version: None,
            disk_space_mb: 1000,
            estimated_size_mb: 100,
        };

        assert!(!prereqs.is_ready());
        let missing = prereqs.missing();
        assert_eq!(missing.len(), 3);
    }

    #[test]
    fn test_deployer_builder() {
        let deployer = GitHubDeployer::new("my-archive")
            .description("My archive")
            .public(false)
            .force(true);

        assert_eq!(deployer.repo_name, "my-archive");
        assert_eq!(deployer.description, "My archive");
        assert!(!deployer.public);
        assert!(deployer.force);
    }

    #[test]
    fn test_size_check() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let file1 = temp.path().join("small.txt");
        let file2 = temp.path().join("medium.txt");

        std::fs::write(&file1, vec![0u8; 1000]).unwrap();
        std::fs::write(&file2, vec![0u8; 10000]).unwrap();

        let deployer = GitHubDeployer::default();
        let check = deployer.check_size(temp.path()).unwrap();

        assert_eq!(check.file_count, 2);
        assert_eq!(check.total_bytes, 11000);
        assert!(!check.exceeds_limit);
        assert!(!check.has_oversized_files);
    }

    #[test]
    fn test_size_check_resolves_bundle_root_without_counting_private_artifacts() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");
        let private_dir = temp.path().join("private");
        std::fs::create_dir_all(&site_dir).unwrap();
        std::fs::create_dir_all(&private_dir).unwrap();
        std::fs::write(site_dir.join("index.html"), "abcd").unwrap();
        std::fs::write(private_dir.join("master-key.json"), "secret").unwrap();

        let deployer = GitHubDeployer::default();
        let check = deployer.check_size(temp.path()).unwrap();

        assert_eq!(check.file_count, 1);
        assert_eq!(check.total_bytes, 4);
    }

    #[test]
    #[cfg(unix)]
    fn test_size_check_counts_in_tree_symlinked_files() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let site_dir = temp.path().join("site");
        std::fs::create_dir_all(&site_dir).unwrap();
        std::fs::write(site_dir.join("root.txt"), "root").unwrap();
        symlink("root.txt", site_dir.join("linked-file.txt")).unwrap();

        let deployer = GitHubDeployer::default();
        let check = deployer.check_size(temp.path()).unwrap();

        assert_eq!(check.file_count, 2);
        assert_eq!(check.total_bytes, 8);
    }

    #[test]
    fn test_resolve_deploy_site_dir_accepts_direct_site_directory() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("index.html"), "<html></html>").unwrap();

        let resolved = resolve_deploy_site_dir(temp.path()).unwrap();
        assert_eq!(resolved, temp.path());
    }

    #[test]
    #[cfg(unix)]
    fn test_resolve_deploy_site_dir_rejects_symlinked_site_directory() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let bundle_root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_site = outside.path().join("site");
        std::fs::create_dir_all(&outside_site).unwrap();
        std::fs::write(outside_site.join("index.html"), "<html></html>").unwrap();
        symlink(&outside_site, bundle_root.path().join("site")).unwrap();

        let err = resolve_deploy_site_dir(bundle_root.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not be a symlink"));

        let direct_err = resolve_deploy_site_dir(&bundle_root.path().join("site"))
            .unwrap_err()
            .to_string();
        assert!(direct_err.contains("must not be a symlink"));
    }

    #[test]
    fn test_copy_dir_recursive() {
        use tempfile::TempDir;

        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();

        // Create source structure
        std::fs::create_dir_all(src.path().join("subdir")).unwrap();
        std::fs::write(src.path().join("root.txt"), "root").unwrap();
        std::fs::write(src.path().join("subdir/nested.txt"), "nested").unwrap();

        copy_dir_recursive(src.path(), dst.path()).unwrap();

        assert!(dst.path().join("root.txt").exists());
        assert!(dst.path().join("subdir/nested.txt").exists());
    }

    #[test]
    fn test_copy_bundle_to_repo_resolves_bundle_root_without_copying_private_artifacts() {
        use tempfile::TempDir;

        let bundle_root = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();
        let site_dir = bundle_root.path().join("site");
        let private_dir = bundle_root.path().join("private");
        std::fs::create_dir_all(&site_dir).unwrap();
        std::fs::create_dir_all(&private_dir).unwrap();
        std::fs::write(site_dir.join("index.html"), "<html></html>").unwrap();
        std::fs::write(site_dir.join("config.json"), "{}").unwrap();
        std::fs::write(private_dir.join("master-key.json"), "{\"secret\":true}").unwrap();

        copy_bundle_to_repo(bundle_root.path(), repo_dir.path()).unwrap();

        assert!(repo_dir.path().join("index.html").exists());
        assert!(repo_dir.path().join("config.json").exists());
        assert!(repo_dir.path().join(".nojekyll").exists());
        assert!(!repo_dir.path().join("private").exists());
        assert!(!repo_dir.path().join("site").exists());
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_dir_recursive_materializes_in_tree_symlinked_files() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();

        std::fs::write(src.path().join("root.txt"), "root").unwrap();
        symlink("root.txt", src.path().join("linked-file.txt")).unwrap();

        copy_dir_recursive(src.path(), dst.path()).unwrap();

        let linked_metadata =
            std::fs::symlink_metadata(dst.path().join("linked-file.txt")).unwrap();
        assert!(linked_metadata.file_type().is_file());
        assert!(!linked_metadata.file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(dst.path().join("linked-file.txt")).unwrap(),
            "root"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_dir_recursive_rejects_symlinks_outside_root() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        std::fs::write(src.path().join("root.txt"), "root").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            src.path().join("linked-file.txt"),
        )
        .unwrap();

        let err = copy_dir_recursive(src.path(), dst.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("Refusing to deploy symlinked site entry outside deployment root"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_visit_files_counts_in_tree_symlinked_files() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let src = TempDir::new().unwrap();

        std::fs::write(src.path().join("root.txt"), "root").unwrap();
        symlink("root.txt", src.path().join("linked-file.txt")).unwrap();

        let mut visited = Vec::new();
        visit_files(src.path(), &mut |path, size| {
            visited.push((
                path.strip_prefix(src.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
                size,
            ));
        })
        .unwrap();

        assert!(visited.contains(&("root.txt".to_string(), 4)));
        assert!(visited.contains(&("linked-file.txt".to_string(), 4)));
    }

    #[test]
    #[cfg(unix)]
    fn test_visit_files_rejects_symlink_paths_outside_root() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let src = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        std::fs::write(src.path().join("root.txt"), "root").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::fs::create_dir_all(outside.path().join("nested")).unwrap();
        std::fs::write(outside.path().join("nested/hidden.txt"), "hidden").unwrap();

        symlink(
            outside.path().join("secret.txt"),
            src.path().join("linked-file.txt"),
        )
        .unwrap();
        symlink(outside.path().join("nested"), src.path().join("linked-dir")).unwrap();

        let err = visit_files(src.path(), &mut |_path, _size| {}).unwrap_err();
        assert!(
            err.to_string()
                .contains("Refusing to deploy symlinked site entry outside deployment root"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn test_configure_git_identity_sets_local_commit_metadata() {
        use tempfile::TempDir;

        let repo = TempDir::new().unwrap();
        let init = Command::new("git")
            .args(["init"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        configure_git_identity(repo.path(), "cass-test").unwrap();

        let name = Command::new("git")
            .args(["config", "user.name"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&name.stdout).trim(), "cass-test");

        let email = Command::new("git")
            .args(["config", "user.email"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&email.stdout).trim(),
            "cass-test@users.noreply.github.com"
        );

        std::fs::write(repo.path().join("index.html"), "<html></html>").unwrap();

        let add = Command::new("git")
            .args(["add", "-A"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );

        let commit = Command::new("git")
            .args(["commit", "-m", "Test commit"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            commit.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
    }
}
