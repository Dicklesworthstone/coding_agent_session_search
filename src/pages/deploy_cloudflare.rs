//! Cloudflare Pages deployment module.
//!
//! Deploys encrypted archives to Cloudflare Pages using wrangler or direct API calls.
//! Supports native COOP/COEP headers, no file size limits, and private repos.

use anyhow::{Context, Result, bail};
use base64::prelude::*;
use blake3::Hasher;
use mime_guess::MimeGuess;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::TryRecvError;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

/// Maximum number of retry attempts for network operations
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (milliseconds)
const BASE_DELAY_MS: u64 = 1000;

/// Timeout for direct Cloudflare API calls.
const API_TIMEOUT_SECS: u64 = 30;

const ENV_CLOUDFLARE_ACCOUNT_ID: &str = "CLOUDFLARE_ACCOUNT_ID";
const ENV_CLOUDFLARE_API_TOKEN: &str = "CLOUDFLARE_API_TOKEN";
const ENV_CLOUDFLARE_API_BASE_URL: &str = "CLOUDFLARE_API_BASE_URL";
const ENV_CF_API_BASE_URL: &str = "CF_API_BASE_URL";

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
    /// Check if all prerequisites are met.
    ///
    /// Either wrangler must be installed and authenticated, or direct API
    /// credentials must be present.
    pub fn is_ready(&self) -> bool {
        self.api_credentials_present
            || (self.wrangler_version.is_some() && self.wrangler_authenticated)
    }

    /// Get a list of missing prerequisites
    pub fn missing(&self) -> Vec<&'static str> {
        if self.is_ready() {
            return Vec::new();
        }

        let mut missing = Vec::new();
        if self.wrangler_version.is_none() && !self.api_credentials_present {
            missing.push(
                "wrangler CLI not installed — run `npm install -g wrangler` or set CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_API_TOKEN for direct API deploys",
            );
        }
        if !self.wrangler_authenticated && !self.api_credentials_present {
            missing.push(
                "not authenticated — set CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_API_TOKEN or run `wrangler login`",
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

        let account_id = self
            .config
            .account_id
            .clone()
            .or_else(|| dotenvy::var(ENV_CLOUDFLARE_ACCOUNT_ID).ok());
        let api_token = self
            .config
            .api_token
            .clone()
            .or_else(|| dotenvy::var(ENV_CLOUDFLARE_API_TOKEN).ok());
        let api_credentials_present = account_id.is_some() && api_token.is_some();

        let disk_space_mb = get_available_space_mb().unwrap_or(0);

        Ok(Prerequisites {
            wrangler_version,
            wrangler_authenticated,
            account_email,
            api_credentials_present,
            account_id,
            disk_space_mb,
        })
    }

    /// Generate _headers file for Cloudflare Pages
    pub fn generate_headers_file(&self, site_dir: &Path) -> Result<()> {
        let headers_content = r#"/*
  Cross-Origin-Opener-Policy: same-origin
  Cross-Origin-Embedder-Policy: require-corp
  Content-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; connect-src 'self'; worker-src 'self' blob:; object-src 'none'; frame-ancestors 'none'; form-action 'none'; base-uri 'none';
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
        mut progress: impl FnMut(&str, &str),
    ) -> Result<DeployResult> {
        let branch = self.config.branch.clone();
        let account_id = self
            .config
            .account_id
            .clone()
            .or_else(|| dotenvy::var(ENV_CLOUDFLARE_ACCOUNT_ID).ok());
        let api_token = self
            .config
            .api_token
            .clone()
            .or_else(|| dotenvy::var(ENV_CLOUDFLARE_API_TOKEN).ok());
        let account_id_ref = account_id.as_deref();
        let api_token_ref = api_token.as_deref();

        // Step 1: Check prerequisites
        progress("prereq", "Checking prerequisites...");
        let prereqs = self.check_prerequisites()?;

        if !prereqs.is_ready() {
            let missing = prereqs.missing();
            bail!("Prerequisites not met:\n{}", missing.join("\n"));
        }
        let can_use_wrangler = prereqs.wrangler_version.is_some()
            && (prereqs.wrangler_authenticated || prereqs.api_credentials_present);

        // Step 2: Copy bundle to temp directory and add Cloudflare files
        progress("prepare", "Preparing deployment...");
        let temp_dir = stage_deploy_dir(bundle_dir.as_ref())?;
        let deploy_dir = temp_dir.path().join("site");

        // Step 3: Generate Cloudflare-specific files
        progress("headers", "Generating COOP/COEP headers...");
        self.generate_headers_file(&deploy_dir)?;
        self.generate_redirects_file(&deploy_dir)?;

        // Step 4: Create project if needed
        progress("project", "Checking Cloudflare Pages project...");
        if self.config.create_if_missing {
            let exists = if can_use_wrangler {
                check_project_exists(&self.config.project_name, account_id_ref, api_token_ref)
            } else if let (Some(account_id), Some(api_token)) = (account_id_ref, api_token_ref) {
                check_project_exists_api(&self.config.project_name, account_id, api_token)?
            } else {
                false
            };
            if !exists {
                progress("create", "Creating new Pages project...");
                if can_use_wrangler {
                    create_project(
                        &self.config.project_name,
                        &branch,
                        account_id_ref,
                        api_token_ref,
                    )?;
                } else if let (Some(account_id), Some(api_token)) = (account_id_ref, api_token_ref)
                {
                    create_project_api(&self.config.project_name, &branch, account_id, api_token)?;
                } else {
                    bail!("Cloudflare API credentials required to create project");
                }
            }
        }

        // Step 5: Deploy using wrangler
        progress("deploy", "Deploying to Cloudflare Pages...");
        let (pages_url, deployment_id) = if can_use_wrangler {
            deploy_with_wrangler(
                &deploy_dir,
                &self.config.project_name,
                &branch,
                account_id_ref,
                api_token_ref,
            )?
        } else if let (Some(account_id), Some(api_token)) = (account_id_ref, api_token_ref) {
            deploy_with_api(
                &deploy_dir,
                &self.config.project_name,
                &branch,
                account_id,
                api_token,
                &mut progress,
            )?
        } else {
            bail!("Cloudflare API credentials required for direct API deployment");
        };

        // Step 6: Configure custom domain if specified
        if let Some(ref domain) = self.config.custom_domain {
            progress(
                "domain",
                &format!("Configuring custom domain: {}...", domain),
            );
            configure_custom_domain(
                &self.config.project_name,
                domain,
                account_id_ref,
                api_token_ref,
            )?;
        }

        progress("complete", "Deployment complete!");

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

struct TempDeployDir {
    path: PathBuf,
}

impl TempDeployDir {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDeployDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Create a temporary directory
fn create_temp_dir() -> Result<TempDeployDir> {
    let temp_base = std::env::temp_dir();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let dir_name = format!("cass-cf-deploy-{}-{}", pid, timestamp);
    let temp_dir = temp_base.join(dir_name);
    std::fs::create_dir_all(&temp_dir)?;
    Ok(TempDeployDir { path: temp_dir })
}

fn stage_deploy_dir(source_path: &Path) -> Result<TempDeployDir> {
    let source_site_dir = resolve_deploy_site_dir(source_path)?;
    let temp_dir = create_temp_dir()?;
    let deploy_dir = temp_dir.path().join("site");
    copy_dir_recursive(&source_site_dir, &deploy_dir)?;
    Ok(temp_dir)
}

fn resolve_deploy_site_dir(path: &Path) -> Result<PathBuf> {
    if path.file_name().map(|name| name == "site").unwrap_or(false) {
        return super::resolve_site_dir(path);
    }

    let site_subdir = path.join("site");
    match std::fs::symlink_metadata(&site_subdir) {
        Ok(_) => return super::resolve_site_dir(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to inspect deployment site directory {}",
                    site_subdir.display()
                )
            });
        }
    }

    bail!(
        "expected a bundle root containing site/ or a site/ directory, got {}",
        path.display()
    );
}

fn apply_api_credentials(cmd: &mut Command, account_id: Option<&str>, api_token: Option<&str>) {
    if let Some(id) = account_id {
        cmd.env(ENV_CLOUDFLARE_ACCOUNT_ID, id);
    }
    if let Some(token) = api_token {
        cmd.env(ENV_CLOUDFLARE_API_TOKEN, token);
    }
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
fn check_project_exists(
    project_name: &str,
    account_id: Option<&str>,
    api_token: Option<&str>,
) -> bool {
    let mut cmd = Command::new("wrangler");
    cmd.args(["pages", "project", "list"]);
    apply_api_credentials(&mut cmd, account_id, api_token);

    cmd.output()
        .map(|out| {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                output_contains_project(&stdout, project_name)
            } else {
                false
            }
        })
        .unwrap_or(false)
}

fn output_contains_project(stdout: &str, project_name: &str) -> bool {
    stdout.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('┌')
            || trimmed.starts_with('├')
            || trimmed.starts_with('└')
        {
            return false;
        }

        // Wrangler table output usually places the project name in the first column.
        let trimmed_edges = trimmed.trim_matches(|c| matches!(c, '│' | '|'));
        let first_cell = trimmed_edges
            .split(['│', '|'])
            .next()
            .unwrap_or(trimmed_edges)
            .trim();
        if first_cell == project_name {
            return true;
        }

        // Fallback for non-table output.
        trimmed_edges
            .split_whitespace()
            .any(|token| token == project_name)
    })
}

/// Create a new Cloudflare Pages project
fn create_project(
    project_name: &str,
    branch: &str,
    account_id: Option<&str>,
    api_token: Option<&str>,
) -> Result<()> {
    let mut cmd = Command::new("wrangler");
    cmd.args([
        "pages",
        "project",
        "create",
        project_name,
        "--production-branch",
        branch,
    ]);
    apply_api_credentials(&mut cmd, account_id, api_token);

    let output = cmd
        .output()
        .context("Failed to run wrangler pages project create")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore if project already exists
        if !stderr.contains("already exists")
            && !stderr.contains("A project with this name already exists")
        {
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
fn deploy_with_wrangler(
    deploy_dir: &Path,
    project_name: &str,
    branch: &str,
    account_id: Option<&str>,
    api_token: Option<&str>,
) -> Result<(String, String)> {
    let deploy_dir_str = deploy_dir
        .to_str()
        .context("Invalid deploy directory path")?;

    retry_with_backoff("wrangler deploy", || {
        let mut cmd = Command::new("wrangler");
        cmd.args([
            "pages",
            "deploy",
            deploy_dir_str,
            "--project-name",
            project_name,
            "--branch",
            branch,
        ]);
        apply_api_credentials(&mut cmd, account_id, api_token);

        let output = cmd
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
                        .map(|url| {
                            url.trim_matches(|c: char| {
                                !c.is_alphanumeric() && c != '.' && c != ':' && c != '/'
                            })
                        })
                } else {
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
                    line.split_whitespace().last().map(|s| s.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string());

        Ok((pages_url, deployment_id))
    })
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    result: Option<T>,
}

#[derive(Debug, Deserialize)]
struct UploadTokenResult {
    jwt: String,
}

#[derive(Debug, Deserialize)]
struct DeploymentResult {
    id: String,
    url: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

#[derive(Debug, Clone)]
struct AssetFile {
    path: PathBuf,
    content_type: String,
    size_bytes: u64,
    hash: String,
}

const MAX_ASSET_COUNT_DEFAULT: usize = 20_000;
const MAX_ASSET_SIZE_BYTES: u64 = 25 * 1024 * 1024;
const MAX_BUCKET_SIZE_BYTES: u64 = 40 * 1024 * 1024;
const MAX_BUCKET_FILE_COUNT: usize = if cfg!(windows) { 1000 } else { 2000 };

fn api_base_url() -> String {
    dotenvy::var(ENV_CLOUDFLARE_API_BASE_URL)
        .or_else(|_| dotenvy::var(ENV_CF_API_BASE_URL))
        .unwrap_or_else(|_| "https://api.cloudflare.com/client/v4".to_string())
}

fn run_cloudflare_with_cx<T, F, Fut>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(asupersync::Cx) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
{
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .context("building Cloudflare API runtime")?;

    runtime.block_on(async move {
        let handle = asupersync::runtime::Runtime::current_handle()
            .ok_or_else(|| anyhow::anyhow!("Cloudflare API runtime handle unavailable"))?;
        let (tx, rx) = std::sync::mpsc::channel();
        handle
            .try_spawn_with_cx(move |cx| async move {
                let _ = tx.send(f(cx).await);
            })
            .map_err(|e| anyhow::anyhow!("spawning Cloudflare API task: {e}"))?;

        loop {
            match rx.try_recv() {
                Ok(result) => return result,
                Err(TryRecvError::Empty) => asupersync::runtime::yield_now().await,
                Err(TryRecvError::Disconnected) => {
                    bail!("Cloudflare API task exited before returning a result");
                }
            }
        }
    })
}

fn cloudflare_api_headers(
    bearer_token: String,
    mut extra_headers: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        (
            "Authorization".to_string(),
            format!("Bearer {bearer_token}"),
        ),
        ("Accept".to_string(), "application/json".to_string()),
    ];
    headers.append(&mut extra_headers);
    headers
}

fn execute_cloudflare_request(
    method: asupersync::http::h1::Method,
    url: String,
    bearer_token: String,
    extra_headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<asupersync::http::h1::Response> {
    run_cloudflare_with_cx(move |cx| async move {
        let client = asupersync::http::h1::HttpClient::builder()
            .user_agent(concat!(
                "cass/",
                env!("CARGO_PKG_VERSION"),
                " (cloudflare-pages)"
            ))
            .build();
        asupersync::time::timeout(
            cx.now(),
            Duration::from_secs(API_TIMEOUT_SECS),
            client.request(
                &cx,
                method,
                &url,
                cloudflare_api_headers(bearer_token, extra_headers),
                body,
            ),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Cloudflare API request timed out: {e}"))?
        .context("Failed to contact Cloudflare API")
    })
}

fn execute_cloudflare_multipart_request(
    url: String,
    bearer_token: String,
    extra_headers: Vec<(String, String)>,
    form: asupersync::http::h1::MultipartForm,
) -> Result<asupersync::http::h1::Response> {
    run_cloudflare_with_cx(move |cx| async move {
        let client = asupersync::http::h1::HttpClient::builder()
            .user_agent(concat!(
                "cass/",
                env!("CARGO_PKG_VERSION"),
                " (cloudflare-pages)"
            ))
            .build();
        asupersync::time::timeout(
            cx.now(),
            Duration::from_secs(API_TIMEOUT_SECS),
            client.request_multipart(
                &cx,
                asupersync::http::h1::Method::Post,
                &url,
                cloudflare_api_headers(bearer_token, extra_headers),
                &form,
            ),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Cloudflare multipart request timed out: {e}"))?
        .context("Failed to contact Cloudflare API")
    })
}

fn parse_api_response<T: DeserializeOwned>(
    response: asupersync::http::h1::Response,
    context_label: &str,
) -> Result<T> {
    let status = response.status;
    let body = response.text().map_or_else(
        |_| String::from_utf8_lossy(response.bytes()).into_owned(),
        str::to_owned,
    );
    let envelope: ApiEnvelope<T> = serde_json::from_str(&body).with_context(|| {
        format!(
            "Failed to parse Cloudflare API response for {} (status {})",
            context_label, status
        )
    })?;
    if !envelope.success {
        let detail = if envelope.errors.is_empty() {
            body
        } else {
            envelope
                .errors
                .iter()
                .map(|err| format!("{} ({})", err.message, err.code))
                .collect::<Vec<_>>()
                .join("; ")
        };
        bail!(
            "Cloudflare API error for {} (status {}): {}",
            context_label,
            status,
            detail
        );
    }
    envelope.result.ok_or_else(|| {
        anyhow::anyhow!("Cloudflare API response missing result for {context_label}")
    })
}

fn check_project_exists_api(project_name: &str, account_id: &str, api_token: &str) -> Result<bool> {
    let url = format!(
        "{}/accounts/{}/pages/projects/{}",
        api_base_url(),
        account_id,
        project_name
    );
    let response = execute_cloudflare_request(
        asupersync::http::h1::Method::Get,
        url,
        api_token.to_string(),
        Vec::new(),
        Vec::new(),
    )?;
    if response.status == 404 {
        return Ok(false);
    }
    parse_api_response::<serde_json::Value>(response, "project lookup")?;
    Ok(true)
}

fn create_project_api(
    project_name: &str,
    branch: &str,
    account_id: &str,
    api_token: &str,
) -> Result<()> {
    let url = format!("{}/accounts/{}/pages/projects", api_base_url(), account_id);
    let body = project_create_body(project_name, branch);
    let response = execute_cloudflare_request(
        asupersync::http::h1::Method::Post,
        url,
        api_token.to_string(),
        vec![("Content-Type".to_string(), "application/json".to_string())],
        serde_json::to_vec(&body).context("Failed to serialize project create body")?,
    )?;
    parse_api_response::<serde_json::Value>(response, "project create")?;
    Ok(())
}

fn project_create_body(project_name: &str, branch: &str) -> serde_json::Value {
    json!({
        "name": project_name,
        "production_branch": branch,
        "deployment_configs": {
            "production": {},
            "preview": {}
        }
    })
}

fn deploy_with_api(
    deploy_dir: &Path,
    project_name: &str,
    branch: &str,
    account_id: &str,
    api_token: &str,
    progress: &mut impl FnMut(&str, &str),
) -> Result<(String, String)> {
    let base_url = api_base_url();

    progress("api-token", "Requesting Pages upload token...");
    let upload_jwt = fetch_upload_token(&base_url, account_id, project_name, api_token)?;
    let max_file_count = jwt_max_file_count(&upload_jwt).unwrap_or(MAX_ASSET_COUNT_DEFAULT);

    progress("scan", "Scanning static assets...");
    let file_map = collect_asset_files(deploy_dir, max_file_count)?;

    progress("upload", "Uploading Pages assets via API...");
    upload_assets(&base_url, &upload_jwt, &file_map, false)?;

    progress("deploy", "Creating Pages deployment via API...");
    let manifest = build_manifest(&file_map);
    let manifest_json =
        serde_json::to_string(&manifest).context("Failed to serialize Pages asset manifest")?;

    let mut form = asupersync::http::h1::MultipartForm::new().text("manifest", manifest_json);
    if !branch.is_empty() {
        form = form.text("branch", branch.to_string());
    }
    let headers_path = deploy_dir.join("_headers");
    if headers_path.exists() {
        let bytes = std::fs::read(&headers_path).context("Failed to read _headers")?;
        form = form.file("_headers", "_headers", "text/plain; charset=utf-8", bytes);
    }
    let redirects_path = deploy_dir.join("_redirects");
    if redirects_path.exists() {
        let bytes = std::fs::read(&redirects_path).context("Failed to read _redirects")?;
        form = form.file(
            "_redirects",
            "_redirects",
            "text/plain; charset=utf-8",
            bytes,
        );
    }

    let deploy_url = format!(
        "{}/accounts/{}/pages/projects/{}/deployments",
        base_url, account_id, project_name
    );
    let response =
        execute_cloudflare_multipart_request(deploy_url, api_token.to_string(), Vec::new(), form)?;
    let deployment = parse_api_response::<DeploymentResult>(response, "deployment create")?;

    let pages_url = deployment
        .url
        .or_else(|| deployment.aliases.first().cloned())
        .unwrap_or_else(|| format!("https://{}.pages.dev", project_name));

    Ok((pages_url, deployment.id))
}

fn fetch_upload_token(
    base_url: &str,
    account_id: &str,
    project_name: &str,
    api_token: &str,
) -> Result<String> {
    let url = format!(
        "{}/accounts/{}/pages/projects/{}/upload-token",
        base_url, account_id, project_name
    );
    let response = execute_cloudflare_request(
        asupersync::http::h1::Method::Get,
        url,
        api_token.to_string(),
        Vec::new(),
        Vec::new(),
    )?;
    let result = parse_api_response::<UploadTokenResult>(response, "upload token")?;
    Ok(result.jwt)
}

fn jwt_max_file_count(jwt: &str) -> Option<usize> {
    let claims_b64 = jwt.split('.').nth(1)?;
    let decoded = BASE64_URL_SAFE_NO_PAD.decode(claims_b64).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    value
        .get("max_file_count_allowed")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
}

fn collect_asset_files(root: &Path, max_files: usize) -> Result<HashMap<String, AssetFile>> {
    let mut files = HashMap::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.context("Failed to read Pages asset entry")?;
        let metadata = entry.metadata().context("Failed to read asset metadata")?;
        if metadata.is_dir() {
            continue;
        }
        if entry.file_type().is_symlink() {
            continue;
        }
        let rel_path = entry
            .path()
            .strip_prefix(root)
            .context("Failed to compute asset relative path")?;
        if should_ignore_path(rel_path) {
            continue;
        }
        let rel_string = normalize_rel_path(rel_path)?;
        let size_bytes = metadata.len();
        if size_bytes > MAX_ASSET_SIZE_BYTES {
            bail!(
                "Cloudflare Pages supports files up to {} bytes; '{}' is {} bytes",
                MAX_ASSET_SIZE_BYTES,
                rel_string,
                size_bytes
            );
        }
        let content_type = MimeGuess::from_path(entry.path())
            .first_or_octet_stream()
            .essence_str()
            .to_string();
        let hash = hash_asset_file(entry.path())?;
        files.insert(
            rel_string.clone(),
            AssetFile {
                path: entry.path().to_path_buf(),
                content_type,
                size_bytes,
                hash,
            },
        );
        if files.len() > max_files {
            bail!(
                "Cloudflare Pages supports up to {} files for this deployment",
                max_files
            );
        }
    }
    Ok(files)
}

fn should_ignore_path(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|s| s.to_str())
        && matches!(
            name,
            "_worker.js" | "_redirects" | "_headers" | "_routes.json" | ".DS_Store"
        )
    {
        return true;
    }
    for component in path.components() {
        if let std::path::Component::Normal(os) = component
            && let Some(part) = os.to_str()
            && matches!(part, "node_modules" | ".git" | "functions")
        {
            return true;
        }
    }
    false
}

fn normalize_rel_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => {
                parts.push(
                    part.to_str()
                        .ok_or_else(|| anyhow::anyhow!("Invalid UTF-8 path segment"))?
                        .to_string(),
                );
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                bail!("Parent directory segments are not allowed in Pages asset paths");
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {}
        }
    }
    Ok(parts.join("/"))
}

fn hash_asset_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).context("Failed to read asset for hashing")?;
    let base64_contents = BASE64_STANDARD.encode(&bytes);
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    let mut hasher = Hasher::new();
    hasher.update(base64_contents.as_bytes());
    hasher.update(extension.as_bytes());
    let hash = hasher.finalize().to_hex().to_string();
    Ok(hash[..32].to_string())
}

fn build_manifest(file_map: &HashMap<String, AssetFile>) -> HashMap<String, String> {
    file_map
        .iter()
        .map(|(name, file)| (format!("/{}", name), file.hash.clone()))
        .collect()
}

fn upload_assets(
    base_url: &str,
    jwt: &str,
    file_map: &HashMap<String, AssetFile>,
    skip_caching: bool,
) -> Result<()> {
    let mut hashes: Vec<String> = file_map.values().map(|file| file.hash.clone()).collect();
    hashes.sort();
    hashes.dedup();
    let missing_hashes = if skip_caching {
        hashes.clone()
    } else {
        check_missing_hashes(base_url, jwt, &hashes)?
    };
    let mut missing_files = select_missing_files(file_map, &missing_hashes);
    missing_files.sort_by_key(|file| std::cmp::Reverse(file.size_bytes));

    let buckets = build_upload_buckets(&missing_files);
    for bucket in buckets {
        upload_bucket(base_url, jwt, &bucket)?;
    }

    upsert_hashes(base_url, jwt, &hashes)?;
    Ok(())
}

fn check_missing_hashes(base_url: &str, jwt: &str, hashes: &[String]) -> Result<Vec<String>> {
    let url = format!("{}/pages/assets/check-missing", base_url);
    let response = execute_cloudflare_request(
        asupersync::http::h1::Method::Post,
        url,
        jwt.to_string(),
        vec![("Content-Type".to_string(), "application/json".to_string())],
        serde_json::to_vec(&json!({ "hashes": hashes }))
            .context("Failed to serialize missing-hashes request")?,
    )?;
    parse_api_response::<Vec<String>>(response, "asset check-missing")
}

fn build_upload_buckets<'a>(files: &[&'a AssetFile]) -> Vec<Vec<&'a AssetFile>> {
    #[derive(Default)]
    struct Bucket<'a> {
        files: Vec<&'a AssetFile>,
        remaining: u64,
    }

    let mut buckets: Vec<Bucket<'a>> = (0..3)
        .map(|_| Bucket {
            files: Vec::new(),
            remaining: MAX_BUCKET_SIZE_BYTES,
        })
        .collect();
    let mut offset = 0usize;

    for file in files {
        let mut inserted = false;
        for i in 0..buckets.len() {
            let idx = (i + offset) % buckets.len();
            let bucket = &mut buckets[idx];
            if bucket.remaining >= file.size_bytes && bucket.files.len() < MAX_BUCKET_FILE_COUNT {
                bucket.remaining -= file.size_bytes;
                bucket.files.push(*file);
                inserted = true;
                break;
            }
        }
        if !inserted {
            buckets.push(Bucket {
                files: vec![*file],
                remaining: MAX_BUCKET_SIZE_BYTES.saturating_sub(file.size_bytes),
            });
        }
        offset = offset.saturating_add(1);
    }

    buckets
        .into_iter()
        .filter(|bucket| !bucket.files.is_empty())
        .map(|bucket| bucket.files)
        .collect()
}

fn select_missing_files<'a>(
    file_map: &'a HashMap<String, AssetFile>,
    missing_hashes: &[String],
) -> Vec<&'a AssetFile> {
    let missing_set: std::collections::HashSet<&str> =
        missing_hashes.iter().map(String::as_str).collect();
    let mut by_hash: HashMap<String, &'a AssetFile> = HashMap::new();

    for file in file_map.values() {
        if missing_set.contains(file.hash.as_str()) {
            // Only one upload per content hash is needed.
            by_hash.entry(file.hash.clone()).or_insert(file);
        }
    }

    by_hash.into_values().collect()
}

fn upload_bucket(base_url: &str, jwt: &str, bucket: &[&AssetFile]) -> Result<()> {
    if bucket.is_empty() {
        return Ok(());
    }
    let payload: Vec<serde_json::Value> = bucket
        .iter()
        .map(|file| {
            let bytes = std::fs::read(&file.path)?;
            Ok(json!({
                "key": file.hash,
                "value": BASE64_STANDARD.encode(&bytes),
                "metadata": { "contentType": file.content_type },
                "base64": true
            }))
        })
        .collect::<Result<Vec<_>>>()?;

    let url = format!("{}/pages/assets/upload", base_url);
    let response = execute_cloudflare_request(
        asupersync::http::h1::Method::Post,
        url,
        jwt.to_string(),
        vec![("Content-Type".to_string(), "application/json".to_string())],
        serde_json::to_vec(&payload).context("Failed to serialize asset upload bucket")?,
    )?;
    parse_api_response::<serde_json::Value>(response, "asset upload")?;
    Ok(())
}

fn upsert_hashes(base_url: &str, jwt: &str, hashes: &[String]) -> Result<()> {
    let url = format!("{}/pages/assets/upsert-hashes", base_url);
    let response = execute_cloudflare_request(
        asupersync::http::h1::Method::Post,
        url,
        jwt.to_string(),
        vec![("Content-Type".to_string(), "application/json".to_string())],
        serde_json::to_vec(&json!({ "hashes": hashes }))
            .context("Failed to serialize asset hash upsert body")?,
    )?;
    parse_api_response::<serde_json::Value>(response, "asset upsert-hashes")?;
    Ok(())
}

/// Configure custom domain for project
fn configure_custom_domain(
    project_name: &str,
    domain: &str,
    account_id: Option<&str>,
    api_token: Option<&str>,
) -> Result<()> {
    // Note: Custom domain configuration typically requires manual setup
    // in the Cloudflare dashboard due to DNS verification requirements.
    // This is a best-effort attempt using wrangler.

    let mut cmd = Command::new("wrangler");
    cmd.args([
        "pages",
        "project",
        "edit",
        project_name,
        "--custom-domain",
        domain,
    ]);
    apply_api_credentials(&mut cmd, account_id, api_token);

    let output = cmd.output();

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
        // When wrangler is not installed and no API creds, there are 2 missing items
        assert_eq!(missing.len(), 2);
        assert!(missing[0].contains("wrangler CLI not installed"));
        assert!(missing[1].contains("not authenticated"));
    }

    #[test]
    fn test_prerequisites_ready_with_api_only() {
        let prereqs = Prerequisites {
            wrangler_version: None,
            wrangler_authenticated: false,
            account_email: None,
            api_credentials_present: true,
            account_id: Some("abc123".to_string()),
            disk_space_mb: 1000,
        };

        assert!(prereqs.is_ready());
        assert!(prereqs.missing().is_empty());
    }

    #[test]
    fn test_config_default() {
        let config = CloudflareConfig::default();
        assert_eq!(config.project_name, "cass-archive");
        assert!(config.custom_domain.is_none());
        assert!(config.create_if_missing);
    }

    #[test]
    fn test_project_create_body_shape() {
        let body = project_create_body("archive-prod", "main");

        assert_eq!(body["name"], json!("archive-prod"));
        assert_eq!(body["production_branch"], json!("main"));
        assert_eq!(body["deployment_configs"]["production"], json!({}));
        assert_eq!(body["deployment_configs"]["preview"], json!({}));
        assert_eq!(body.as_object().expect("object").len(), 3);
        assert_eq!(
            body["deployment_configs"]
                .as_object()
                .expect("configs")
                .len(),
            2
        );
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
    fn test_temp_deploy_dir_cleans_up_on_drop() {
        let temp_path = {
            let temp = create_temp_dir().unwrap();
            let marker = temp.path().join("marker.txt");
            std::fs::write(&marker, "cleanup").unwrap();
            assert!(marker.exists());
            temp.path().to_path_buf()
        };

        assert!(!temp_path.exists());
    }

    #[test]
    fn test_stage_deploy_dir_resolves_bundle_root_without_copying_private_artifacts() {
        use tempfile::TempDir;

        let bundle_root = TempDir::new().unwrap();
        let site_dir = bundle_root.path().join("site");
        let private_dir = bundle_root.path().join("private");
        std::fs::create_dir_all(&site_dir).unwrap();
        std::fs::create_dir_all(&private_dir).unwrap();
        std::fs::write(site_dir.join("index.html"), "<html></html>").unwrap();
        std::fs::write(site_dir.join("config.json"), "{}").unwrap();
        std::fs::write(private_dir.join("master-key.json"), "{\"secret\":true}").unwrap();

        let staged = stage_deploy_dir(bundle_root.path()).unwrap();
        let staged_site_dir = staged.path().join("site");

        assert!(staged_site_dir.join("index.html").exists());
        assert!(staged_site_dir.join("config.json").exists());
        assert!(!staged_site_dir.join("private").exists());
        assert!(!staged.path().join("private").exists());
    }

    #[test]
    fn test_resolve_deploy_site_dir_rejects_non_bundle_directory() {
        use tempfile::TempDir;

        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("index.html"), "<html></html>").unwrap();

        let err = resolve_deploy_site_dir(temp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected a bundle root containing site/ or a site/ directory"));
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
    fn test_output_contains_project_exact_match() {
        let list_output = "\
┌──────────────┬────────────┐
│ Name         │ Production │
├──────────────┼────────────┤
│ cass-archive │ main       │
│ cass-prod    │ main       │
└──────────────┴────────────┘";

        assert!(output_contains_project(list_output, "cass-archive"));
        assert!(!output_contains_project(list_output, "cass"));
    }

    #[test]
    fn test_select_missing_files_dedupes_by_hash() {
        let mut file_map = HashMap::new();
        file_map.insert(
            "a.txt".to_string(),
            AssetFile {
                path: PathBuf::from("/tmp/a.txt"),
                content_type: "text/plain".to_string(),
                size_bytes: 10,
                hash: "hash-shared".to_string(),
            },
        );
        file_map.insert(
            "b.txt".to_string(),
            AssetFile {
                path: PathBuf::from("/tmp/b.txt"),
                content_type: "text/plain".to_string(),
                size_bytes: 10,
                hash: "hash-shared".to_string(),
            },
        );
        file_map.insert(
            "c.txt".to_string(),
            AssetFile {
                path: PathBuf::from("/tmp/c.txt"),
                content_type: "text/plain".to_string(),
                size_bytes: 8,
                hash: "hash-unique".to_string(),
            },
        );

        let missing = vec!["hash-shared".to_string(), "hash-unique".to_string()];
        let selected = select_missing_files(&file_map, &missing);

        // Two unique hashes should produce two uploads, not three files.
        assert_eq!(selected.len(), 2);
        let hashes: std::collections::HashSet<_> =
            selected.iter().map(|f| f.hash.as_str()).collect();
        assert!(hashes.contains("hash-shared"));
        assert!(hashes.contains("hash-unique"));
    }
}
