//! Cloudflare Pages deployment module.
//!
//! Deploys encrypted archives to Cloudflare Pages using the wrangler CLI.
//! Supports native COOP/COEP headers, no file size limits, and private repos.

use anyhow::{Context, Result, bail};
use base64::prelude::*;
use blake3::Hasher;
use mime_guess::MimeGuess;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::blocking::multipart::{Form, Part};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

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
        self.api_credentials_present
            || (self.wrangler_version.is_some() && self.wrangler_authenticated)
    }

    /// Get a list of missing prerequisites
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.wrangler_version.is_none() && !self.api_credentials_present {
            missing.push(
                "wrangler CLI not installed and no API token provided (install wrangler or pass --account-id/--api-token)",
            );
        }
        if self.wrangler_version.is_some()
            && !self.wrangler_authenticated
            && !self.api_credentials_present
        {
            missing.push(
                "wrangler CLI not authenticated and no API token provided (use --account-id/--api-token or set CLOUDFLARE_ACCOUNT_ID + CLOUDFLARE_API_TOKEN)",
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
            .or_else(|| dotenvy::var("CLOUDFLARE_ACCOUNT_ID").ok());
        let api_token = self
            .config
            .api_token
            .clone()
            .or_else(|| dotenvy::var("CLOUDFLARE_API_TOKEN").ok());
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
        let bundle_dir = bundle_dir.as_ref();
        let branch = self.config.branch.clone();
        let account_id = self
            .config
            .account_id
            .clone()
            .or_else(|| dotenvy::var("CLOUDFLARE_ACCOUNT_ID").ok());
        let api_token = self
            .config
            .api_token
            .clone()
            .or_else(|| dotenvy::var("CLOUDFLARE_API_TOKEN").ok());
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

fn apply_api_credentials(cmd: &mut Command, account_id: Option<&str>, api_token: Option<&str>) {
    if let Some(id) = account_id {
        cmd.env("CLOUDFLARE_ACCOUNT_ID", id);
    }
    if let Some(token) = api_token {
        cmd.env("CLOUDFLARE_API_TOKEN", token);
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
                stdout.lines().any(|line| line.contains(project_name))
            } else {
                false
            }
        })
        .unwrap_or(false)
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
    dotenvy::var("CLOUDFLARE_API_BASE_URL")
        .or_else(|_| dotenvy::var("CF_API_BASE_URL"))
        .unwrap_or_else(|_| "https://api.cloudflare.com/client/v4".to_string())
}

fn parse_api_response<T: DeserializeOwned>(
    response: reqwest::blocking::Response,
    context_label: &str,
) -> Result<T> {
    let status = response.status();
    let body = response
        .text()
        .context("Failed to read Cloudflare API response body")?;
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
    let client = Client::new();
    let url = format!(
        "{}/accounts/{}/pages/projects/{}",
        api_base_url(),
        account_id,
        project_name
    );
    let response = client
        .get(url)
        .bearer_auth(api_token)
        .send()
        .context("Failed to query Cloudflare Pages project")?;
    if response.status() == StatusCode::NOT_FOUND {
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
    let client = Client::new();
    let url = format!("{}/accounts/{}/pages/projects", api_base_url(), account_id);
    let body = json!({
        "name": project_name,
        "production_branch": branch,
        "deployment_configs": {
            "production": {},
            "preview": {}
        }
    });
    let response = client
        .post(url)
        .bearer_auth(api_token)
        .json(&body)
        .send()
        .context("Failed to create Cloudflare Pages project")?;
    parse_api_response::<serde_json::Value>(response, "project create")?;
    Ok(())
}

fn deploy_with_api(
    deploy_dir: &Path,
    project_name: &str,
    branch: &str,
    account_id: &str,
    api_token: &str,
    progress: &mut impl FnMut(&str, &str),
) -> Result<(String, String)> {
    let client = Client::new();
    let base_url = api_base_url();

    progress("api-token", "Requesting Pages upload token...");
    let upload_jwt = fetch_upload_token(&client, &base_url, account_id, project_name, api_token)?;
    let max_file_count = jwt_max_file_count(&upload_jwt).unwrap_or(MAX_ASSET_COUNT_DEFAULT);

    progress("scan", "Scanning static assets...");
    let file_map = collect_asset_files(deploy_dir, max_file_count)?;

    progress("upload", "Uploading Pages assets via API...");
    upload_assets(&client, &base_url, &upload_jwt, &file_map, false)?;

    progress("deploy", "Creating Pages deployment via API...");
    let manifest = build_manifest(&file_map);
    let manifest_json =
        serde_json::to_string(&manifest).context("Failed to serialize Pages asset manifest")?;

    let mut form = Form::new().text("manifest", manifest_json);
    if !branch.is_empty() {
        form = form.text("branch", branch.to_string());
    }
    let headers_path = deploy_dir.join("_headers");
    if headers_path.exists() {
        let bytes = std::fs::read(&headers_path).context("Failed to read _headers")?;
        form = form.part(
            "_headers",
            Part::bytes(bytes).file_name("_headers".to_string()),
        );
    }
    let redirects_path = deploy_dir.join("_redirects");
    if redirects_path.exists() {
        let bytes = std::fs::read(&redirects_path).context("Failed to read _redirects")?;
        form = form.part(
            "_redirects",
            Part::bytes(bytes).file_name("_redirects".to_string()),
        );
    }

    let deploy_url = format!(
        "{}/accounts/{}/pages/projects/{}/deployments",
        base_url, account_id, project_name
    );
    let response = client
        .post(deploy_url)
        .bearer_auth(api_token)
        .multipart(form)
        .send()
        .context("Failed to create Pages deployment via API")?;
    let deployment = parse_api_response::<DeploymentResult>(response, "deployment create")?;

    let pages_url = deployment
        .url
        .or_else(|| deployment.aliases.first().cloned())
        .unwrap_or_else(|| format!("https://{}.pages.dev", project_name));

    Ok((pages_url, deployment.id))
}

fn fetch_upload_token(
    client: &Client,
    base_url: &str,
    account_id: &str,
    project_name: &str,
    api_token: &str,
) -> Result<String> {
    let url = format!(
        "{}/accounts/{}/pages/projects/{}/upload-token",
        base_url, account_id, project_name
    );
    let response = client
        .get(url)
        .bearer_auth(api_token)
        .send()
        .context("Failed to request Pages upload token")?;
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
    client: &Client,
    base_url: &str,
    jwt: &str,
    file_map: &HashMap<String, AssetFile>,
    skip_caching: bool,
) -> Result<()> {
    let hashes: Vec<String> = file_map.values().map(|file| file.hash.clone()).collect();
    let missing_hashes = if skip_caching {
        hashes.clone()
    } else {
        check_missing_hashes(client, base_url, jwt, &hashes)?
    };
    let missing_set: std::collections::HashSet<String> = missing_hashes.into_iter().collect();
    let mut missing_files: Vec<&AssetFile> = file_map
        .values()
        .filter(|file| missing_set.contains(&file.hash))
        .collect();
    missing_files.sort_by_key(|file| std::cmp::Reverse(file.size_bytes));

    let buckets = build_upload_buckets(&missing_files);
    for bucket in buckets {
        upload_bucket(client, base_url, jwt, &bucket)?;
    }

    upsert_hashes(client, base_url, jwt, &hashes)?;
    Ok(())
}

fn check_missing_hashes(
    client: &Client,
    base_url: &str,
    jwt: &str,
    hashes: &[String],
) -> Result<Vec<String>> {
    let url = format!("{}/pages/assets/check-missing", base_url);
    let response = client
        .post(url)
        .bearer_auth(jwt)
        .json(&json!({ "hashes": hashes }))
        .send()
        .context("Failed to check missing Pages assets")?;
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

fn upload_bucket(client: &Client, base_url: &str, jwt: &str, bucket: &[&AssetFile]) -> Result<()> {
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
    let response = client
        .post(url)
        .bearer_auth(jwt)
        .json(&payload)
        .send()
        .context("Failed to upload Pages asset bucket")?;
    parse_api_response::<serde_json::Value>(response, "asset upload")?;
    Ok(())
}

fn upsert_hashes(client: &Client, base_url: &str, jwt: &str, hashes: &[String]) -> Result<()> {
    let url = format!("{}/pages/assets/upsert-hashes", base_url);
    let response = client
        .post(url)
        .bearer_auth(jwt)
        .json(&json!({ "hashes": hashes }))
        .send()
        .context("Failed to upsert Pages asset hashes")?;
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
