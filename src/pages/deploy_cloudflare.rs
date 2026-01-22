//! Cloudflare Pages deployment module.
//!
//! Deploys encrypted archives to Cloudflare Pages using the wrangler CLI.
//! Supports native COOP/COEP headers, no file size limits, and private repos.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Maximum number of retry attempts for network operations
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (milliseconds)
const BASE_DELAY_MS: u64 = 1000;

/// Prerequisites for Cloudflare Pages deployment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prerequisites {
    /// wrangler CLI version if installed
    pub wrangler_version: Option<String>,
    /// Whether wrangler CLI is authenticated
    pub wrangler_authenticated: bool,
    /// Cloudflare account email if authenticated
    pub account_email: Option<String>,
    /// Whether API credentials (token + account ID) are available
    pub api_credentials_present: bool,
    /// Account ID if provided (safe to display)
    pub account_id: Option<String>,
    /// Available disk space in MB
    pub disk_space_mb: u64,
}

impl Prerequisites {
    /// Check if all prerequisites are met
    pub fn is_ready(&self) -> bool {
        self.wrangler_version.is_some()
            && (self.wrangler_authenticated || self.api_credentials_present)
    }

    /// Get a list of missing prerequisites
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.wrangler_version.is_none() {
            missing.push("wrangler CLI not installed (install with: npm install -g wrangler)");
        }
        if !self.wrangler_authenticated && !self.api_credentials_present {
            missing.push(
                "wrangler CLI not authenticated and no API token provided (set CLOUDFLARE_API_TOKEN + CLOUDFLARE_ACCOUNT_ID)",
            );
        }
        missing
    }
}

/// Deployment result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployResult {
    /// Project name
    pub project_name: String,
    /// Pages URL (where the site is accessible)
    pub pages_url: String,
    /// Whether deployment was successful
    pub deployed: bool,
    /// Deployment ID if available
    pub deployment_id: Option<String>,
    /// Custom domain if configured
    pub custom_domain: Option<String>,
}

/// Cloudflare Pages deployer configuration
#[derive(Debug, Clone)]
pub struct CloudflareConfig {
    /// Project name for Cloudflare Pages
    pub project_name: String,
    /// Optional custom domain
    pub custom_domain: Option<String>,
    /// Whether to create project if it doesn't exist
    pub create_if_missing: bool,
    /// Production branch for Pages deployments
    pub branch: String,
    /// Optional Cloudflare account ID (fallback auth for CI)
    pub account_id: Option<String>,
    /// Optional Cloudflare API token (fallback auth for CI)
    pub api_token: Option<String>,
}

impl Default for CloudflareConfig {
    fn default() -> Self {
        Self {
            project_name: "cass-archive".to_string(),
            custom_domain: None,
            create_if_missing: true,
            branch: "main".to_string(),
            account_id: None,
            api_token: None,
        }
    }
}

/// Cloudflare Pages deployer
pub struct CloudflareDeployer {
    config: CloudflareConfig,
}

impl Default for CloudflareDeployer {
    fn default() -> Self {
        Self::new(CloudflareConfig::default())
    }
}

impl CloudflareDeployer {
    /// Create a new deployer with the given configuration
    pub fn new(config: CloudflareConfig) -> Self {
        Self { config }
    }

    /// Create a deployer with just a project name
    pub fn with_project_name(project_name: impl Into<String>) -> Self {
        Self::new(CloudflareConfig {
            project_name: project_name.into(),
            ..Default::default()
        })
    }

    /// Set custom domain
    pub fn custom_domain(mut self, domain: impl Into<String>) -> Self {
        self.config.custom_domain = Some(domain.into());
        self
    }

    /// Set whether to create project if missing
    pub fn create_if_missing(mut self, create: bool) -> Self {
        self.config.create_if_missing = create;
        self
    }

    /// Set deployment branch (defaults to "main")
    pub fn branch(mut self, branch: impl Into<String>) -> Self {
        self.config.branch = branch.into();
        self
    }

    /// Set Cloudflare account ID (for API-token auth)
    pub fn account_id(mut self, account_id: impl Into<String>) -> Self {
        self.config.account_id = Some(account_id.into());
        self
    }

    /// Set Cloudflare API token (for API-token auth)
    pub fn api_token(mut self, api_token: impl Into<String>) -> Self {
        self.config.api_token = Some(api_token.into());
        self
    }

    /// Check deployment prerequisites
    pub fn check_prerequisites(&self) -> Result<Prerequisites> {
        let wrangler_version = get_wrangler_version();
        let (wrangler_authenticated, account_email) = if wrangler_version.is_some() {
            check_wrangler_auth()
        } else {
            (false, None)
        };

        let disk_space_mb = get_available_space_mb().unwrap_or(0);

        Ok(Prerequisites {
            wrangler_version,
            wrangler_authenticated,
            account_email,
            api_credentials_present: false,
            account_id: None,
            disk_space_mb,
        })
    }

    /// Generate _headers file for Cloudflare Pages
    pub fn generate_headers_file(&self, site_dir: &Path) -> Result<()> {
        let headers_content = r#"/*
  Cross-Origin-Opener-Policy: same-origin
  Cross-Origin-Embedder-Policy: require-corp
  X-Content-Type-Options: nosniff
  X-Frame-Options: DENY
  Referrer-Policy: no-referrer
  X-Robots-Tag: noindex, nofollow
  Cache-Control: public, max-age=31536000, immutable

/index.html
  Cache-Control: no-cache

/config.json
  Cache-Control: no-cache

/*.html
  Cache-Control: no-cache
"#;

        std::fs::write(site_dir.join("_headers"), headers_content)
            .context("Failed to write _headers file")?;
        Ok(())
    }

    /// Generate _redirects file for SPA support
    pub fn generate_redirects_file(&self, site_dir: &Path) -> Result<()> {
        // For hash-based routing, no redirects needed
        // But we can add a fallback for direct URL access
        let redirects_content = "/* /index.html 200\n";

        std::fs::write(site_dir.join("_redirects"), redirects_content)
            .context("Failed to write _redirects file")?;
        Ok(())
    }

    /// Deploy bundle to Cloudflare Pages
    ///
    /// # Arguments
    /// * `bundle_dir` - Path to the site/ directory from bundle builder
    /// * `progress` - Progress callback (phase, message)
    pub fn deploy<P: AsRef<Path>>(
        &self,
        bundle_dir: P,
        progress: impl Fn(&str, &str),
    ) -> Result<DeployResult> {
        let bundle_dir = bundle_dir.as_ref();

        // Step 1: Check prerequisites
        progress("prereq", "Checking prerequisites...");
        let prereqs = self.check_prerequisites()?;

        if !prereqs.is_ready() {
            let missing = prereqs.missing();
            bail!("Prerequisites not met:\n{}", missing.join("\n"));
        }

        // Step 2: Copy bundle to temp directory and add Cloudflare files
        progress("prepare", "Preparing deployment...");
        let temp_dir = create_temp_dir()?;
        let deploy_dir = temp_dir.join("site");
        copy_dir_recursive(bundle_dir, &deploy_dir)?;

        // Step 3: Generate Cloudflare-specific files
        progress("headers", "Generating COOP/COEP headers...");
        self.generate_headers_file(&deploy_dir)?;
        self.generate_redirects_file(&deploy_dir)?;

        // Step 4: Create project if needed
        progress("project", "Checking Cloudflare Pages project...");
        if self.config.create_if_missing {
            let exists = check_project_exists(&self.config.project_name);
            if !exists {
                progress("create", "Creating new Pages project...");
                create_project(&self.config.project_name)?;
            }
        }

        // Step 5: Deploy using wrangler
        progress("deploy", "Deploying to Cloudflare Pages...");
        let (pages_url, deployment_id) =
            deploy_with_wrangler(&deploy_dir, &self.config.project_name)?;

        // Step 6: Configure custom domain if specified
        if let Some(ref domain) = self.config.custom_domain {
            progress("domain", &format!("Configuring custom domain: {}...", domain));
            configure_custom_domain(&self.config.project_name, domain)?;
        }

        progress("complete", "Deployment complete!");

        // Clean up temp directory
        let _ = std::fs::remove_dir_all(&temp_dir);

        Ok(DeployResult {
            project_name: self.config.project_name.clone(),
            pages_url,
            deployed: true,
            deployment_id: Some(deployment_id),
            custom_domain: self.config.custom_domain.clone(),
        })
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
    let dir_name = format!("cass-cf-deploy-{}-{}", pid, timestamp);
    let temp_dir = temp_base.join(dir_name);
    std::fs::create_dir_all(&temp_dir)?;
    Ok(temp_dir)
}

/// Get wrangler CLI version
fn get_wrangler_version() -> Option<String> {
    Command::new("wrangler")
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

/// Check wrangler authentication status
fn check_wrangler_auth() -> (bool, Option<String>) {
    let output = Command::new("wrangler").args(["whoami"]).output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);

            // Parse email from output
            let email = stdout
                .lines()
                .find(|line| line.contains('@'))
                .map(|line| line.trim().to_string());

            (true, email)
        }
        _ => (false, None),
    }
}

/// Get available disk space in MB
fn get_available_space_mb() -> Option<u64> {
    #[cfg(unix)]
    {
        Command::new("df")
            .args(["-m", "."])
            .output()
            .ok()
            .and_then(|out| {
                if out.status.success() {
                    let stdout = String::from_utf8_lossy(&out.stdout);
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

/// Check if Cloudflare Pages project exists
fn check_project_exists(project_name: &str) -> bool {
    Command::new("wrangler")
        .args(["pages", "project", "list"])
        .output()
        .map(|out| {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                stdout.lines().any(|line| line.contains(project_name))
            } else {
                false
            }
        })
        .unwrap_or(false)
}

/// Create a new Cloudflare Pages project
fn create_project(project_name: &str) -> Result<()> {
    let output = Command::new("wrangler")
        .args(["pages", "project", "create", project_name, "--production-branch", "main"])
        .output()
        .context("Failed to run wrangler pages project create")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore if project already exists
        if !stderr.contains("already exists") && !stderr.contains("A project with this name already exists") {
            bail!("Failed to create project: {}", stderr);
        }
    }

    Ok(())
}

/// Retry a fallible operation with exponential backoff
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
                    let delay_ms = BASE_DELAY_MS * (1 << attempt);
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

/// Deploy using wrangler CLI with retry logic
fn deploy_with_wrangler(deploy_dir: &Path, project_name: &str) -> Result<(String, String)> {
    let deploy_dir_str = deploy_dir
        .to_str()
        .context("Invalid deploy directory path")?;

    retry_with_backoff("wrangler deploy", || {
        let output = Command::new("wrangler")
            .args([
                "pages",
                "deploy",
                deploy_dir_str,
                "--project-name",
                project_name,
            ])
            .output()
            .context("Failed to run wrangler pages deploy")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Deployment failed: {}", stderr);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Parse URL from output
        // Typical output: "Deployment complete! ... https://xxx.project.pages.dev"
        let pages_url = stdout
            .lines()
            .find_map(|line| {
                if line.contains(".pages.dev") {
                    line.split_whitespace()
                        .find(|word| word.contains(".pages.dev"))
                        .map(|url| url.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != ':' && c != '/'))
                }
                else {
                    None
                }
            })
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("https://{}.pages.dev", project_name));

        // Parse deployment ID if available
        let deployment_id = stdout
            .lines()
            .find_map(|line| {
                if line.contains("Deployment ID:") || line.contains("deployment_id") {
                    line.split_whitespace()
                        .last()
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string());

        Ok((pages_url, deployment_id))
    })
}

/// Configure custom domain for project
fn configure_custom_domain(project_name: &str, domain: &str) -> Result<()> {
    // Note: Custom domain configuration typically requires manual setup
    // in the Cloudflare dashboard due to DNS verification requirements.
    // This is a best-effort attempt using wrangler.

    let output = Command::new("wrangler")
        .args([
            "pages",
            "project",
            "edit",
            project_name,
            "--custom-domain",
            domain,
        ])
        .output();

    match output {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprintln!(
                "Warning: Could not automatically configure custom domain. \
                Please configure '{}' manually in the Cloudflare dashboard.\nError: {}",
                domain, stderr
            );
            Ok(()) // Don't fail deployment for domain config issues
        }
        Err(e) => {
            eprintln!(
                "Warning: Could not configure custom domain: {}. \
                Please configure '{}' manually in the Cloudflare dashboard.",
                e, domain
            );
            Ok(())
        }
    }
}

/// Copy directory recursively
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
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
            wrangler_version: Some("wrangler 3.0.0".to_string()),
            wrangler_authenticated: true,
            account_email: Some("test@example.com".to_string()),
            api_credentials_present: false,
            account_id: None,
            disk_space_mb: 1000,
        };

        assert!(prereqs.is_ready());
        assert!(prereqs.missing().is_empty());
    }

    #[test]
    fn test_prerequisites_not_ready() {
        let prereqs = Prerequisites {
            wrangler_version: None,
            wrangler_authenticated: false,
            account_email: None,
            api_credentials_present: false,
            account_id: None,
            disk_space_mb: 1000,
        };

        assert!(!prereqs.is_ready());
        let missing = prereqs.missing();
        assert_eq!(missing.len(), 2);
    }

    #[test]
    fn test_config_default() {
        let config = CloudflareConfig::default();
        assert_eq!(config.project_name, "cass-archive");
        assert!(config.custom_domain.is_none());
        assert!(config.create_if_missing);
    }

    #[test]
    fn test_deployer_builder() {
        let deployer = CloudflareDeployer::with_project_name("my-archive")
            .custom_domain("archive.example.com")
            .create_if_missing(false);

        assert_eq!(deployer.config.project_name, "my-archive");
        assert_eq!(
            deployer.config.custom_domain,
            Some("archive.example.com".to_string())
        );
        assert!(!deployer.config.create_if_missing);
    }

    #[test]
    fn test_generate_headers_file() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let deployer = CloudflareDeployer::default();

        deployer.generate_headers_file(temp.path()).unwrap();

        let headers_path = temp.path().join("_headers");
        assert!(headers_path.exists());

        let content = std::fs::read_to_string(&headers_path).unwrap();
        assert!(content.contains("Cross-Origin-Opener-Policy: same-origin"));
        assert!(content.contains("Cross-Origin-Embedder-Policy: require-corp"));
        assert!(content.contains("X-Frame-Options: DENY"));
    }

    #[test]
    fn test_generate_redirects_file() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        let deployer = CloudflareDeployer::default();

        deployer.generate_redirects_file(temp.path()).unwrap();

        let redirects_path = temp.path().join("_redirects");
        assert!(redirects_path.exists());

        let content = std::fs::read_to_string(&redirects_path).unwrap();
        assert!(content.contains("/* /index.html 200"));
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
}
