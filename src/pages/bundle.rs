//! Bundle builder for pages export.
//!
//! Creates the deployable static site bundle (site/) and private offline artifacts (private/)
//! from an export. Output is safe for public hosting (GitHub Pages / Cloudflare Pages).

use anyhow::{Context, Result, anyhow, bail};
use base64::prelude::*;
use chrono::Utc;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::archive_config::{ArchiveConfig, UnencryptedConfig};
use super::docs::{DocLocation, GeneratedDoc};
use super::encrypt::{EncryptionConfig, validate_supported_payload_format};

/// Files embedded from pages_assets at compile time
const PAGES_ASSETS: &[(&str, &[u8])] = &[
    ("index.html", include_bytes!("../pages_assets/index.html")),
    ("styles.css", include_bytes!("../pages_assets/styles.css")),
    ("auth.js", include_bytes!("../pages_assets/auth.js")),
    (
        "password-strength.js",
        include_bytes!("../pages_assets/password-strength.js"),
    ),
    ("viewer.js", include_bytes!("../pages_assets/viewer.js")),
    ("router.js", include_bytes!("../pages_assets/router.js")),
    ("share.js", include_bytes!("../pages_assets/share.js")),
    ("stats.js", include_bytes!("../pages_assets/stats.js")),
    ("storage.js", include_bytes!("../pages_assets/storage.js")),
    ("search.js", include_bytes!("../pages_assets/search.js")),
    (
        "conversation.js",
        include_bytes!("../pages_assets/conversation.js"),
    ),
    ("database.js", include_bytes!("../pages_assets/database.js")),
    ("session.js", include_bytes!("../pages_assets/session.js")),
    ("sw.js", include_bytes!("../pages_assets/sw.js")),
    (
        "sw-register.js",
        include_bytes!("../pages_assets/sw-register.js"),
    ),
    (
        "crypto_worker.js",
        include_bytes!("../pages_assets/crypto_worker.js"),
    ),
    (
        "virtual-list.js",
        include_bytes!("../pages_assets/virtual-list.js"),
    ),
    (
        "coi-detector.js",
        include_bytes!("../pages_assets/coi-detector.js"),
    ),
    (
        "attachments.js",
        include_bytes!("../pages_assets/attachments.js"),
    ),
    ("settings.js", include_bytes!("../pages_assets/settings.js")),
];

/// Immutable third-party runtime files embedded in every Pages bundle.
///
/// These are exact bytes from the upstream releases recorded in
/// `pages_assets/vendor/manifest.json`. Keep the byte include, size, SHA-256,
/// manifest record, and license in lockstep when updating a dependency.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PinnedVendorAsset {
    pub path: &'static str,
    pub bytes: &'static [u8],
    pub size: u64,
    pub sha256: &'static str,
}

macro_rules! pinned_vendor_asset {
    ($path:literal, $size:literal, $sha256:literal) => {
        PinnedVendorAsset {
            path: concat!("vendor/", $path),
            bytes: include_bytes!(concat!("../pages_assets/vendor/", $path)),
            size: $size,
            sha256: $sha256,
        }
    };
}

pub(crate) const PAGES_VENDOR_ASSETS: &[PinnedVendorAsset] = &[
    pinned_vendor_asset!(
        "sqlite3.mjs",
        578_559,
        "f80870f0fa03a39a3338d17ed3fbea04808d344c88e724d90d5f37b9b7b83154"
    ),
    pinned_vendor_asset!(
        "sqlite3.wasm",
        864_752,
        "02d7e48164395fa68f81c6ec33e9da5461be397dc57602ac0cd89b4bbba1d312"
    ),
    pinned_vendor_asset!(
        "sqlite3-opfs-async-proxy.js",
        32_289,
        "502762db7bd24130f345df6a42883166ec0a91815207940328c9739bb0f5ec2f"
    ),
    pinned_vendor_asset!(
        "argon2.js",
        11_835,
        "ecfe330a8f3c6d491b92197391088f117ab13ad13c784cb01235a1afb5757a3b"
    ),
    pinned_vendor_asset!(
        "argon2-wasm.js",
        14_215,
        "cdb6ef704dbc287ffa0056376d9af297c8691ddeb985f9137b96fc9b0ddea8b0"
    ),
    pinned_vendor_asset!(
        "argon2.wasm",
        25_725,
        "0c2149886c13e4eae4a6ca25ee71d47423c5c8740a874cf04ff816d1b2c901d7"
    ),
    pinned_vendor_asset!(
        "fflate.js",
        33_044,
        "462ef8041fc970e3615a20a9dd2b2e3047a073b2da729ef4f02b634bba8b7b83"
    ),
    pinned_vendor_asset!(
        "html5-qrcode.min.js",
        375_364,
        "660b12437b1d747e3e68b8be0685c08cb728140110ad213f167b14b66f8b1d8e"
    ),
    pinned_vendor_asset!(
        "LICENSE-sqlite-wasm.txt",
        11_358,
        "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30"
    ),
    pinned_vendor_asset!(
        "LICENSE-argon2-browser.txt",
        1_058,
        "524a1d77701975a063a590424637abbd7fa519042a113f2bbe2eaf4cde76296d"
    ),
    pinned_vendor_asset!(
        "LICENSE-fflate.txt",
        1_069,
        "0a1df3a083d0c010560aa342e87959c8c1070e6fd54545741f083f22d0c8b551"
    ),
    pinned_vendor_asset!(
        "LICENSE-html5-qrcode.txt",
        11_361,
        "306bc450da41244fefb92fec6fba30f27afb9d9b0eaa72b96372c23e2338c612"
    ),
    pinned_vendor_asset!(
        "manifest.json",
        4_802,
        "bd7571df7a9ddd1553c398474b4f69f2ab64ea8f2f887fe85f14c8518c1c44bb"
    ),
];

const MASTER_KEY_BACKUP_NOTE: &str =
    "This file contains the wrapped DEK. Keep it with your recovery secret.";
const BUNDLE_PUBLISH_MARKER_NAME: &str = ".cass-pages-publish-in-progress-v1";
const BUNDLE_PUBLISH_MARKER_CONTENT: &[u8] = b"cass-pages-publish-in-progress-v1\n";

/// Integrity entry for a single file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityEntry {
    /// SHA256 hash as hex string
    pub sha256: String,
    /// File size in bytes
    pub size: u64,
}

/// Full integrity manifest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityManifest {
    /// Schema version for integrity format
    pub version: u8,
    /// Generated timestamp
    pub generated_at: String,
    /// Map of relative path -> integrity entry
    pub files: BTreeMap<String, IntegrityEntry>,
}

/// Site metadata for public config
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteMetadata {
    pub title: String,
    pub description: String,
    pub generated_at: String,
    pub generator: String,
    pub generator_version: String,
}

/// Bundle configuration
#[derive(Debug, Clone)]
pub struct BundleConfig {
    /// Archive title
    pub title: String,
    /// Archive description
    pub description: String,
    /// Whether to obfuscate metadata (workspace paths etc)
    pub hide_metadata: bool,
    /// Recovery secret bytes (if generated)
    pub recovery_secret: Option<Vec<u8>>,
    /// Whether to generate QR codes for recovery
    pub generate_qr: bool,
    /// Additional generated documentation files to include
    pub generated_docs: Vec<GeneratedDoc>,
    /// Exact `cass analytics status --json` data projection for the source
    /// archive. Pages consumes this instead of inventing separate readiness.
    pub analytics_status: Option<serde_json::Value>,
}

impl Default for BundleConfig {
    fn default() -> Self {
        Self {
            title: "cass Archive".to_string(),
            description: "Encrypted archive of AI coding agent conversations".to_string(),
            hide_metadata: false,
            recovery_secret: None,
            generate_qr: false,
            generated_docs: Vec::new(),
            analytics_status: None,
        }
    }
}

/// Bundle builder for creating static site exports
#[derive(Default)]
pub struct BundleBuilder {
    config: BundleConfig,
}

impl BundleBuilder {
    /// Create a new bundle builder with default config
    pub fn new() -> Self {
        Self {
            config: BundleConfig::default(),
        }
    }

    /// Create a bundle builder with specific config
    pub fn with_config(config: BundleConfig) -> Self {
        Self { config }
    }

    /// Set the archive title
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.config.title = title.into();
        self
    }

    /// Set the archive description
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.config.description = description.into();
        self
    }

    /// Set metadata hiding option
    pub fn hide_metadata(mut self, hide: bool) -> Self {
        self.config.hide_metadata = hide;
        self
    }

    /// Set the recovery secret
    pub fn recovery_secret(mut self, recovery_material: Option<Vec<u8>>) -> Self {
        // ubs:ignore — this stores caller-provided recovery bytes; no secret literal is embedded.
        let recovery_slot = &mut self.config.recovery_secret;
        *recovery_slot = recovery_material;
        self
    }

    /// Set QR code generation option
    pub fn generate_qr(mut self, generate: bool) -> Self {
        self.config.generate_qr = generate;
        self
    }

    /// Add generated documentation files to include in the bundle
    pub fn with_docs(mut self, docs: Vec<GeneratedDoc>) -> Self {
        self.config.generated_docs = docs;
        self
    }

    /// Build the bundle from encrypted output
    ///
    /// # Arguments
    /// * `encrypted_dir` - Directory containing encryption output (config.json, payload/)
    /// * `output_dir` - Directory to write the bundle (will create site/ and private/ subdirs)
    /// * `progress` - Progress callback (phase, message)
    pub fn build<P: AsRef<Path>>(
        &self,
        encrypted_dir: P,
        output_dir: P,
        progress: impl Fn(&str, &str),
    ) -> Result<BundleResult> {
        let encrypted_dir = encrypted_dir.as_ref();
        let output_dir = output_dir.as_ref();

        // Heal an interrupted fallback publish before doing potentially long
        // archive validation/copying work, so a parked prior generation is
        // returned to the live handle as soon as the next build starts.
        recover_interrupted_bundle_publish(output_dir)?;
        ensure_replaceable_bundle_output_dir(output_dir)?;

        // Validate encrypted_dir has required files
        let config_path = encrypted_dir.join("config.json");
        let payload_dir = encrypted_dir.join("payload");

        if !config_path.exists() {
            bail!("Missing config.json in encrypted directory");
        }
        if !payload_dir.exists() {
            bail!("Missing payload/ directory in encrypted directory");
        }

        // Load archive config (encrypted or unencrypted)
        let archive_config: ArchiveConfig = {
            let file = File::open(&config_path).context("Failed to open config.json")?;
            serde_json::from_reader(BufReader::new(file))?
        };
        if self.config.generate_qr {
            if self.config.recovery_secret.is_none() {
                bail!(
                    "QR recovery artifacts were requested, but no recovery secret is available; enable recovery-secret generation or disable QR generation"
                );
            }
            if archive_config.as_encrypted().is_none() {
                bail!(
                    "QR recovery artifacts were requested for an unencrypted archive, which has no recovery-key slot; disable QR generation"
                );
            }
        }

        let temp_output_dir = unique_bundle_dir(output_dir, "tmp")?;
        let final_site_dir = output_dir.join("site");
        let final_private_dir = output_dir.join("private");
        let mut retain_temp_on_replace_error = false;
        let result = (|| -> Result<BundleResult> {
            progress("setup", "Creating directory structure...");

            // Stage the bundle under a unique temp root so reruns do not retain stale files.
            create_bundle_staging_root(&temp_output_dir)?;
            let site_dir = temp_output_dir.join("site");
            let private_dir = temp_output_dir.join("private");

            fs::create_dir_all(&site_dir).context("Failed to create site/ directory")?;
            ensure_private_artifact_dir(&private_dir)?;

            // Create site subdirectories
            let site_payload_dir = site_dir.join("payload");
            fs::create_dir_all(&site_payload_dir).context("Failed to create site/payload/")?;

            progress("assets", "Copying web assets...");

            // Copy embedded assets to site/
            for (name, content) in PAGES_ASSETS {
                let dest_path = site_dir.join(name);
                fs::write(&dest_path, content)
                    .with_context(|| format!("Failed to write {}", name))?;
            }
            write_pinned_vendor_assets(&site_dir)?;

            if let Some(status) = &self.config.analytics_status {
                let status_json = serde_json::to_vec_pretty(status)
                    .context("Failed to serialize analytics_status.json")?;
                crate::pages::write_file_durably(
                    &site_dir.join("analytics_status.json"),
                    &status_json,
                )
                .context("Failed to write analytics_status.json")?;
            }

            // Copy payload into site/payload/
            let (chunk_count, is_encrypted) = match archive_config.as_encrypted() {
                Some(enc_config) => {
                    progress("payload", "Copying encrypted payload...");
                    let count = copy_payload_chunks(
                        encrypted_dir,
                        &payload_dir,
                        &site_payload_dir,
                        enc_config,
                    )?;
                    (count, true)
                }
                None => {
                    progress("payload", "Copying unencrypted payload...");
                    let unenc_config = archive_config
                        .as_unencrypted()
                        .context("Unencrypted config missing")?;
                    let count = copy_payload_file(encrypted_dir, &site_dir, unenc_config)?;
                    (count, false)
                }
            };

            // Copy attachment blobs if present
            let blobs_dir = encrypted_dir.join("blobs");
            let attachment_count = if blobs_dir.exists() && blobs_dir.is_dir() {
                progress("attachments", "Copying encrypted attachments...");
                let site_blobs_dir = site_dir.join("blobs");
                copy_blobs_directory(encrypted_dir, &blobs_dir, &site_blobs_dir)?
            } else {
                0
            };

            progress("config", "Writing configuration files...");

            // Write config.json to site/ (already has public params only)
            let site_config_path = site_dir.join("config.json");
            let config_json = serde_json::to_vec_pretty(&archive_config)
                .context("Failed to serialize public Pages config")?;
            crate::pages::write_file_durably(&site_config_path, &config_json)
                .context("Failed to write public Pages config")?;

            // Write site metadata
            let site_metadata = SiteMetadata {
                title: self.config.title.clone(),
                description: self.config.description.clone(),
                generated_at: Utc::now().to_rfc3339(),
                generator: "cass".to_string(),
                generator_version: env!("CARGO_PKG_VERSION").to_string(),
            };
            let site_json_path = site_dir.join("site.json");
            let site_json = serde_json::to_vec_pretty(&site_metadata)
                .context("Failed to serialize Pages site metadata")?;
            crate::pages::write_file_durably(&site_json_path, &site_json)
                .context("Failed to write Pages site metadata")?;

            progress("static", "Writing static files...");

            // Write robots.txt
            let robots_content = "User-agent: *\nDisallow: /\n";
            fs::write(site_dir.join("robots.txt"), robots_content)?;

            // Write .nojekyll (empty file to disable Jekyll processing)
            fs::write(site_dir.join(".nojekyll"), "")?;

            // Write generated documentation if provided, otherwise fallback to basic readme
            if !self.config.generated_docs.is_empty() {
                progress("docs", "Writing generated documentation...");
                for doc in &self.config.generated_docs {
                    let dest_path = resolve_generated_doc_path(&site_dir, doc)?;
                    fs::write(&dest_path, &doc.content)
                        .with_context(|| format!("Failed to write {}", doc.filename))?;
                }
            } else {
                // Fallback to basic README.md
                let public_readme = generate_public_readme(
                    &self.config.title,
                    &self.config.description,
                    is_encrypted,
                );
                fs::write(site_dir.join("README.md"), public_readme)?;
            }

            validate_pinned_vendor_assets(&site_dir)
                .context("Materialized Pages vendor runtime failed exact-byte validation")?;

            progress("integrity", "Generating integrity manifest...");

            // Generate integrity.json for all files in site/
            let integrity_manifest = generate_integrity_manifest(&site_dir)?;
            let integrity_path = site_dir.join("integrity.json");
            let integrity_json = serde_json::to_vec_pretty(&integrity_manifest)
                .context("Failed to serialize Pages integrity manifest")?;
            crate::pages::write_file_durably(&integrity_path, &integrity_json)
                .context("Failed to write Pages integrity manifest")?;

            // Compute integrity fingerprint (short hash for visual verification)
            let fingerprint = compute_fingerprint(&integrity_manifest);

            progress("private", "Writing private artifacts...");

            // Write private artifacts
            write_private_fingerprint(&private_dir, &fingerprint)?;
            if is_encrypted {
                let enc_config = archive_config
                    .as_encrypted()
                    .context("Encrypted config missing")?;
                write_private_artifacts_encrypted(
                    &private_dir,
                    enc_config,
                    self.config.recovery_secret.as_deref(),
                    self.config.generate_qr,
                    true,
                )?;
            } else {
                write_private_unencrypted_notice(&private_dir)?;
            }

            prepare_bundle_root_for_publish(&temp_output_dir)?;
            sync_tree(&temp_output_dir)?;
            replace_dir_from_temp(
                &temp_output_dir,
                output_dir,
                &mut retain_temp_on_replace_error,
            )
            .context("Failed to install completed bundle")?;

            progress("complete", "Bundle complete!");

            Ok(BundleResult {
                site_dir: final_site_dir,
                private_dir: final_private_dir,
                chunk_count,
                attachment_count,
                fingerprint,
                total_files: integrity_manifest.files.len(),
            })
        })();

        match result {
            Err(build_error) if !retain_temp_on_replace_error => {
                match cleanup_rejected_bundle_temp(&temp_output_dir) {
                    Ok(()) => Err(build_error),
                    Err(cleanup_error) => Err(build_error.context(format!(
                        "failed to remove rejected staged bundle: {cleanup_error:#}"
                    ))),
                }
            }
            other => other,
        }
    }
}

fn create_bundle_staging_root(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed creating bundle staging parent {}", parent.display())
        })?;
    }

    #[cfg(unix)]
    {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path).with_context(|| {
            format!(
                "failed creating private bundle staging root {}",
                path.display()
            )
        })?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed securing bundle staging root {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
            .with_context(|| format!("failed creating bundle staging root {}", path.display()))?;
    }
    Ok(())
}

fn prepare_bundle_root_for_publish(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).with_context(|| {
        format!(
            "failed setting bundle root permissions on {}",
            path.display()
        )
    })?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn cleanup_rejected_bundle_temp(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed removing staged bundle {}", path.display()))
        }
    }
}

fn unique_bundle_dir(path: &Path, suffix: &str) -> Result<PathBuf> {
    unique_bundle_sidecar_path(path, suffix, "pages_bundle")
}

fn bundle_publish_in_progress_backup_path(path: &Path) -> PathBuf {
    let mut backup_name = std::ffi::OsString::from(".");
    backup_name.push(
        path.file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("pages_bundle")),
    );
    backup_name.push(".publish-in-progress.bak");
    path.with_file_name(backup_name)
}

fn bundle_publish_marker_path(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join(BUNDLE_PUBLISH_MARKER_NAME)
}

fn bundle_publish_marker_exists(bundle_dir: &Path) -> Result<bool> {
    let marker_path = bundle_publish_marker_path(bundle_dir);
    match fs::symlink_metadata(&marker_path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!(
                    "bundle publish marker must not be a symlink: {}",
                    marker_path.display()
                );
            }
            if !file_type.is_file() {
                bail!(
                    "bundle publish marker must be a regular file: {}",
                    marker_path.display()
                );
            }
            let maximum_marker_bytes =
                u64::try_from(BUNDLE_PUBLISH_MARKER_CONTENT.len().saturating_add(1))
                    .unwrap_or(u64::MAX);
            let mut contents = Vec::with_capacity(BUNDLE_PUBLISH_MARKER_CONTENT.len());
            File::open(&marker_path)
                .with_context(|| {
                    format!(
                        "failed opening bundle publish marker {}",
                        marker_path.display()
                    )
                })?
                .take(maximum_marker_bytes)
                .read_to_end(&mut contents)
                .with_context(|| {
                    format!(
                        "failed reading bundle publish marker {}",
                        marker_path.display()
                    )
                })?;
            if contents != BUNDLE_PUBLISH_MARKER_CONTENT {
                bail!(
                    "bundle publish marker has unrecognized contents and will not be trusted: {}",
                    marker_path.display()
                );
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed inspecting bundle publish marker {}",
                marker_path.display()
            )
        }),
    }
}

fn write_bundle_publish_marker(bundle_dir: &Path) -> Result<()> {
    let marker_path = bundle_publish_marker_path(bundle_dir);
    let mut marker = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker_path)
        .with_context(|| {
            format!(
                "failed creating bundle publish marker {}",
                marker_path.display()
            )
        })?;
    marker
        .write_all(BUNDLE_PUBLISH_MARKER_CONTENT)
        .with_context(|| {
            format!(
                "failed writing bundle publish marker {}",
                marker_path.display()
            )
        })?;
    marker.sync_all().with_context(|| {
        format!(
            "failed syncing bundle publish marker {}",
            marker_path.display()
        )
    })?;
    sync_parent_directory(&marker_path)
}

fn remove_bundle_publish_marker(bundle_dir: &Path) -> Result<()> {
    if !bundle_publish_marker_exists(bundle_dir)? {
        return Ok(());
    }
    let marker_path = bundle_publish_marker_path(bundle_dir);
    fs::remove_file(&marker_path).with_context(|| {
        format!(
            "failed removing completed bundle publish marker {}",
            marker_path.display()
        )
    })?;
    sync_parent_directory(&marker_path)
}

fn unique_bundle_sidecar_path(path: &Path, suffix: &str, fallback_name: &str) -> Result<PathBuf> {
    static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let random_nonce = bundle_sidecar_random_nonce()?;
    let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback_name);

    Ok(path.with_file_name(format!(".{file_name}.{suffix}.{random_nonce:032x}.{nonce}")))
}

fn bundle_sidecar_random_nonce() -> Result<u128> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("failed to generate random bundle sidecar nonce"))?;
    Ok(u128::from_le_bytes(bytes))
}

fn ensure_replaceable_bundle_output_dir(path: &Path) -> Result<bool> {
    ensure_bundle_directory_entry(path, "bundle output path")
}

fn ensure_bundle_directory_entry(path: &Path, label: &str) -> Result<bool> {
    ensure_existing_parent_ancestors_are_real_dirs(path, label)?;

    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!("{label} must not be a symlink: {}", path.display());
            }
            if !file_type.is_dir() {
                bail!(
                    "{label} points to a file, expected a directory: {}",
                    path.display()
                );
            }
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => {
            Err(err).with_context(|| format!("failed inspecting {label} {}", path.display()))
        }
    }
}

fn ensure_existing_parent_ancestors_are_real_dirs(path: &Path, label: &str) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    let mut ancestors = Vec::new();
    let mut current = Some(parent);
    while let Some(ancestor) = current {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        ancestors.push(ancestor.to_path_buf());
        current = ancestor.parent();
    }
    ancestors.reverse();

    for ancestor in ancestors {
        match fs::symlink_metadata(&ancestor) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    if is_allowed_system_symlink_ancestor(&ancestor) {
                        continue;
                    }
                    bail!(
                        "{label} parent must not contain symlinks: {}",
                        ancestor.display()
                    );
                }
                if !file_type.is_dir() {
                    bail!("{label} parent must be a directory: {}", ancestor.display());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed inspecting {label} parent {}", ancestor.display())
                });
            }
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn is_allowed_system_symlink_ancestor(path: &Path) -> bool {
    path == Path::new("/var") || path == Path::new("/tmp")
}

#[cfg(not(target_os = "macos"))]
fn is_allowed_system_symlink_ancestor(_path: &Path) -> bool {
    false
}

fn replace_dir_from_temp(
    temp_dir: &Path,
    final_dir: &Path,
    retain_temp_on_error: &mut bool,
) -> Result<()> {
    *retain_temp_on_error = false;
    if !ensure_bundle_directory_entry(temp_dir, "staged bundle path")? {
        bail!("staged bundle path does not exist: {}", temp_dir.display());
    }

    recover_interrupted_bundle_publish(final_dir)?;
    if !ensure_replaceable_bundle_output_dir(final_dir)? {
        fs::rename(temp_dir, final_dir).with_context(|| {
            format!(
                "failed renaming completed bundle {} into place at {}",
                temp_dir.display(),
                final_dir.display()
            )
        })?;
        sync_parent_directory(final_dir).with_context(|| {
            format!(
                "completed bundle is live at {}, but its first publication could not be durably synced",
                final_dir.display()
            )
        })?;
        return Ok(());
    }

    let backup_dir = bundle_publish_in_progress_backup_path(final_dir);
    // Every replacement candidate carries the same durable ownership marker,
    // including the non-Linux rename-pair path. Recovery may only remove one
    // of two simultaneously present trees when this marker identifies which
    // tree is NEW; an unmarked deterministic sidecar could predate cass and
    // must never be guessed away.
    write_bundle_publish_marker(temp_dir)?;

    #[cfg(target_os = "linux")]
    {
        if try_publish_linux_bundle_via_atomic_exchange(
            temp_dir,
            final_dir,
            &backup_dir,
            retain_temp_on_error,
        )? {
            return Ok(());
        }
    }

    replace_dir_from_temp_via_recoverable_rename_pair(
        temp_dir,
        final_dir,
        &backup_dir,
        retain_temp_on_error,
    )
}

fn recover_interrupted_bundle_publish(final_dir: &Path) -> Result<()> {
    let backup_dir = bundle_publish_in_progress_backup_path(final_dir);
    let final_exists = ensure_replaceable_bundle_output_dir(final_dir)?;
    let final_has_marker = if final_exists {
        bundle_publish_marker_exists(final_dir)?
    } else {
        false
    };
    let backup_exists =
        ensure_bundle_directory_entry(&backup_dir, "bundle publish recovery backup")?;
    if !backup_exists {
        if final_has_marker {
            remove_bundle_publish_marker(final_dir).with_context(|| {
                format!(
                    "the published bundle is live at {}, but its completed publish marker could not be cleaned",
                    final_dir.display()
                )
            })?;
        }
        return Ok(());
    }
    let backup_has_marker = bundle_publish_marker_exists(&backup_dir)?;

    if !final_exists {
        if backup_has_marker {
            bail!(
                "interrupted atomic bundle publish is missing its prior live handle {}; staged candidate containing private artifacts retained at {}",
                final_dir.display(),
                backup_dir.display()
            );
        }

        fs::rename(&backup_dir, final_dir).with_context(|| {
            format!(
                "failed restoring the only prior live bundle from interrupted-publish backup {} to {}",
                backup_dir.display(),
                final_dir.display()
            )
        })?;
        sync_parent_directory(final_dir).with_context(|| {
            format!(
                "restored the prior live bundle at {} from {}, but could not durably sync the recovery",
                final_dir.display(),
                backup_dir.display()
            )
        })?;
        return Ok(());
    }

    match (final_has_marker, backup_has_marker) {
        (true, false) => {
            cleanup_prior_bundle_after_publish(&backup_dir, final_dir)?;
            remove_bundle_publish_marker(final_dir).with_context(|| {
                format!(
                    "new bundle is live at {}, but its completed atomic-publish marker could not be cleaned",
                    final_dir.display()
                )
            })
        }
        (false, true) => cleanup_rejected_atomic_staged_bundle(&backup_dir, final_dir),
        (false, false) => bail!(
            "ambiguous interrupted bundle publish: live bundle {} and recovery sidecar {} are both unmarked; neither tree was removed",
            final_dir.display(),
            backup_dir.display()
        ),
        (true, true) => bail!(
            "ambiguous interrupted bundle publish: both live bundle {} and recovery sidecar {} carry the in-progress marker; neither tree was removed",
            final_dir.display(),
            backup_dir.display()
        ),
    }
}

fn cleanup_rejected_atomic_staged_bundle(staged_dir: &Path, final_dir: &Path) -> Result<()> {
    if !ensure_bundle_directory_entry(staged_dir, "rejected atomic staged bundle path")? {
        bail!(
            "prior live bundle remains at {}, but rejected staged bundle path disappeared before cleanup: {}",
            final_dir.display(),
            staged_dir.display()
        );
    }
    fs::remove_dir_all(staged_dir).with_context(|| {
        format!(
            "prior live bundle remains at {}, but failed to remove rejected staged bundle containing private artifacts retained at {}",
            final_dir.display(),
            staged_dir.display()
        )
    })?;
    sync_parent_directory(final_dir)
}

fn cleanup_prior_bundle_after_publish(backup_dir: &Path, final_dir: &Path) -> Result<()> {
    // A closure rather than the bare `fs::remove_dir_all` fn item: the
    // generic fn monomorphizes with one concrete lifetime and cannot satisfy
    // the higher-ranked `for<'a> FnOnce(&'a Path)` bound.
    cleanup_prior_bundle_after_publish_with(backup_dir, final_dir, |path| fs::remove_dir_all(path))
}

fn cleanup_prior_bundle_after_publish_with<F>(
    backup_dir: &Path,
    final_dir: &Path,
    remove_dir: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
{
    if !ensure_bundle_directory_entry(backup_dir, "prior bundle cleanup path")? {
        bail!(
            "new bundle is live at {}, but the prior bundle cleanup path disappeared before removal: {}",
            final_dir.display(),
            backup_dir.display()
        );
    }
    remove_dir(backup_dir).with_context(|| {
        format!(
            "new bundle is live at {}, but failed to remove the prior bundle containing private artifacts retained at {}",
            final_dir.display(),
            backup_dir.display()
        )
    })?;
    sync_parent_directory(final_dir).with_context(|| {
        format!(
            "removed the prior bundle after publishing {}, but could not durably sync its parent directory",
            final_dir.display()
        )
    })
}

#[cfg(target_os = "linux")]
fn try_publish_linux_bundle_via_atomic_exchange(
    temp_dir: &Path,
    final_dir: &Path,
    backup_dir: &Path,
    retain_temp_on_error: &mut bool,
) -> Result<bool> {
    fs::rename(temp_dir, backup_dir).with_context(|| {
        format!(
            "failed moving staged bundle {} to deterministic atomic-exchange path {}",
            temp_dir.display(),
            backup_dir.display()
        )
    })?;
    if let Err(sync_error) = sync_parent_directory(backup_dir) {
        return match restore_linux_atomic_staged_candidate(
            backup_dir,
            temp_dir,
            retain_temp_on_error,
        ) {
            Ok(()) => Err(sync_error).with_context(|| {
                format!(
                    "failed durably staging bundle at deterministic atomic-exchange path {}; restored candidate at {}",
                    backup_dir.display(),
                    temp_dir.display()
                )
            }),
            Err(restore_error) => Err(anyhow!(
                "failed durably staging bundle at deterministic atomic-exchange path {}: {sync_error:#}; failed restoring staged candidate to {}: {restore_error:#}",
                backup_dir.display(),
                temp_dir.display()
            )),
        };
    }

    match crate::indexer::atomic_exchange_paths(final_dir, backup_dir) {
        Ok(()) => {
            if let Err(sync_error) = sync_parent_directory(final_dir) {
                *retain_temp_on_error = true;
                bail!(
                    "new bundle is live at {} after atomic exchange, but the parent directory sync failed: {}; prior bundle containing private artifacts retained at {}",
                    final_dir.display(),
                    sync_error,
                    backup_dir.display()
                );
            }
            if let Err(cleanup_error) = cleanup_prior_bundle_after_publish(backup_dir, final_dir) {
                *retain_temp_on_error = true;
                return Err(cleanup_error);
            }
            remove_bundle_publish_marker(final_dir).with_context(|| {
                format!(
                    "new bundle is live at {}, but its completed atomic-publish marker could not be cleaned",
                    final_dir.display()
                )
            })?;
            Ok(true)
        }
        Err(exchange_error)
            if crate::indexer::linux_atomic_exchange_is_unsupported(&exchange_error) =>
        {
            restore_linux_atomic_staged_candidate(
                backup_dir,
                temp_dir,
                retain_temp_on_error,
            )
            .context(
                "atomic bundle exchange is unsupported and the staged candidate could not be restored for rename-pair publication",
            )?;
            tracing::info!(
                live_bundle_path = %final_dir.display(),
                staged_bundle_path = %temp_dir.display(),
                "renameat2(RENAME_EXCHANGE) is unsupported for the Pages bundle output; using recoverable rename-pair publication"
            );
            Ok(false)
        }
        Err(exchange_error) => {
            match restore_linux_atomic_staged_candidate(backup_dir, temp_dir, retain_temp_on_error)
            {
                Ok(()) => Err(exchange_error).with_context(|| {
                    format!(
                        "failed atomically exchanging staged bundle {} with live bundle {}",
                        temp_dir.display(),
                        final_dir.display()
                    )
                }),
                Err(restore_error) => Err(anyhow!(
                    "failed atomically exchanging staged bundle {} with live bundle {}: {exchange_error:#}; failed restoring staged candidate from {}: {restore_error:#}",
                    temp_dir.display(),
                    final_dir.display(),
                    backup_dir.display()
                )),
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn restore_linux_atomic_staged_candidate(
    backup_dir: &Path,
    temp_dir: &Path,
    retain_temp_on_error: &mut bool,
) -> Result<()> {
    if let Err(restore_error) = fs::rename(backup_dir, temp_dir) {
        *retain_temp_on_error = true;
        bail!(
            "failed restoring staged bundle from deterministic atomic-exchange path {} to {}; staged bundle containing private artifacts retained at {}: {}",
            backup_dir.display(),
            temp_dir.display(),
            backup_dir.display(),
            restore_error
        );
    }
    sync_parent_directory(temp_dir).with_context(|| {
        format!(
            "restored staged bundle to {}, but could not durably sync the rename from {}",
            temp_dir.display(),
            backup_dir.display()
        )
    })
}

fn replace_dir_from_temp_via_recoverable_rename_pair(
    temp_dir: &Path,
    final_dir: &Path,
    backup_dir: &Path,
    retain_temp_on_error: &mut bool,
) -> Result<()> {
    fs::rename(final_dir, backup_dir).with_context(|| {
        format!(
            "failed parking live bundle {} at deterministic recovery path {}",
            final_dir.display(),
            backup_dir.display()
        )
    })?;

    if let Err(park_sync_error) = sync_parent_directory(final_dir) {
        return match fs::rename(backup_dir, final_dir) {
            Ok(()) => {
                sync_parent_directory(final_dir).with_context(|| {
                    format!(
                        "failed syncing recovery after restoring prior live bundle at {}",
                        final_dir.display()
                    )
                })?;
                Err(park_sync_error).with_context(|| {
                    format!(
                        "failed durably parking the prior live bundle at {}; restored it at {}",
                        backup_dir.display(),
                        final_dir.display()
                    )
                })
            }
            Err(restore_error) => {
                *retain_temp_on_error = true;
                bail!(
                    "failed durably parking the prior live bundle at {}: {}; restore to {} also failed: {}; prior bundle retained at {} and staged bundle retained at {}",
                    backup_dir.display(),
                    park_sync_error,
                    final_dir.display(),
                    restore_error,
                    backup_dir.display(),
                    temp_dir.display()
                )
            }
        };
    }

    match fs::rename(temp_dir, final_dir) {
        Ok(()) => {
            if let Err(sync_error) = sync_parent_directory(final_dir) {
                *retain_temp_on_error = true;
                bail!(
                    "new bundle was moved into place at {}, but the publish could not be durably synced: {}; prior bundle containing private artifacts retained at {}",
                    final_dir.display(),
                    sync_error,
                    backup_dir.display()
                );
            }
            if let Err(cleanup_error) = cleanup_prior_bundle_after_publish(backup_dir, final_dir) {
                *retain_temp_on_error = true;
                return Err(cleanup_error);
            }
            remove_bundle_publish_marker(final_dir).with_context(|| {
                format!(
                    "new bundle is live at {}, but its completed rename-pair publish marker could not be cleaned",
                    final_dir.display()
                )
            })
        }
        Err(publish_error) => match fs::rename(backup_dir, final_dir) {
            Ok(()) => {
                sync_parent_directory(final_dir)?;
                Err(publish_error).with_context(|| {
                    format!(
                        "failed publishing staged bundle {} at {}; restored the prior live bundle",
                        temp_dir.display(),
                        final_dir.display()
                    )
                })
            }
            Err(restore_error) => {
                *retain_temp_on_error = true;
                bail!(
                    "failed publishing staged bundle {} at {}: {}; restore also failed: {}; prior bundle retained at {} and staged bundle retained at {}",
                    temp_dir.display(),
                    final_dir.display(),
                    publish_error,
                    restore_error,
                    backup_dir.display(),
                    temp_dir.display()
                );
            }
        },
    }
}

#[cfg(not(windows))]
fn sync_tree(path: &Path) -> Result<()> {
    sync_tree_inner(path)?;
    sync_parent_directory(path)
}

#[cfg(windows)]
fn sync_tree(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn sync_tree_inner(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed reading metadata for {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Ok(());
    }
    if file_type.is_file() {
        File::open(path)
            .with_context(|| format!("failed opening {} for sync", path.display()))?
            .sync_all()
            .with_context(|| format!("failed syncing {}", path.display()))?;
        return Ok(());
    }
    if file_type.is_dir() {
        for entry in
            fs::read_dir(path).with_context(|| format!("failed reading {}", path.display()))?
        {
            let entry = entry.with_context(|| format!("failed walking {}", path.display()))?;
            sync_tree_inner(&entry.path())?;
        }
        File::open(path)
            .with_context(|| format!("failed opening directory {} for sync", path.display()))?
            .sync_all()
            .with_context(|| format!("failed syncing directory {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .with_context(|| format!("failed opening parent directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("failed syncing parent directory {}", parent.display()))
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

/// Result from bundle building
#[derive(Debug, Clone)]
pub struct BundleResult {
    /// Path to site/ directory (deploy this)
    pub site_dir: PathBuf,
    /// Path to private/ directory (never deploy)
    pub private_dir: PathBuf,
    /// Number of encrypted payload chunks
    pub chunk_count: usize,
    /// Number of encrypted attachment blobs
    pub attachment_count: usize,
    /// Integrity fingerprint (for visual verification)
    pub fingerprint: String,
    /// Total number of files in site/
    pub total_files: usize,
}

/// Copy encrypted payload chunks from source to destination.
///
/// The archive config is the authority: copying by directory scan can publish
/// stale chunks left behind by an earlier export.
fn copy_payload_chunks(
    src_root: &Path,
    src_dir: &Path,
    dest_dir: &Path,
    config: &EncryptionConfig,
) -> Result<usize> {
    ensure_regular_copy_directory_under_root(src_root, src_dir, "Encrypted payload directory")?;
    validate_supported_payload_format(config)?;

    let mut count = 0;

    for (idx, expected_file) in config.payload.files.iter().enumerate() {
        let expected_path = format!("payload/chunk-{idx:05}.bin");
        if expected_file != &expected_path {
            bail!(
                "Encrypted payload file entry {idx} must be {expected_path}, got {expected_file}"
            );
        }

        let rel_path = Path::new(expected_file);
        let src_path = src_root.join(rel_path);
        let label = format!("Encrypted payload chunk {expected_file}");
        ensure_regular_copy_source_under_root(src_root, &src_path, &label)?;

        let Some(filename) = rel_path.file_name() else {
            bail!("Encrypted payload chunk path has no file name: {expected_file}");
        };
        let dest_path = dest_dir.join(filename);
        fs::copy(&src_path, &dest_path)?;
        count += 1;
    }

    Ok(count)
}

/// Copy a single unencrypted payload file into the site directory.
fn copy_payload_file(
    src_root: &Path,
    site_dir: &Path,
    config: &UnencryptedConfig,
) -> Result<usize> {
    let rel_path = Path::new(&config.payload.path);
    if rel_path.is_absolute() {
        bail!("Unencrypted payload path must be relative");
    }
    if rel_path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("Unencrypted payload path must not contain '..'");
    }
    if !rel_path.starts_with("payload") {
        bail!("Unencrypted payload path must reside under payload/");
    }

    let src_path = src_root.join(rel_path);
    ensure_regular_copy_source_under_root(src_root, &src_path, "Unencrypted payload file")?;

    let dest_path = site_dir.join(rel_path);
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::copy(&src_path, &dest_path)?;
    Ok(1)
}

fn resolve_generated_doc_path(site_dir: &Path, doc: &GeneratedDoc) -> Result<PathBuf> {
    if doc.filename.contains(['/', '\\']) {
        bail!(
            "Generated documentation filename must not contain path separators: {}",
            doc.filename
        );
    }

    let rel_path = Path::new(&doc.filename);
    let mut components = rel_path.components();
    let Some(std::path::Component::Normal(file_name)) = components.next() else {
        bail!(
            "Generated documentation filename must be a plain relative file name: {}",
            doc.filename
        );
    };
    if components.next().is_some() {
        bail!(
            "Generated documentation filename must not contain path separators: {}",
            doc.filename
        );
    }

    Ok(match doc.location {
        DocLocation::RepoRoot | DocLocation::WebRoot => site_dir.join(file_name),
    })
}

fn ensure_regular_copy_source_under_root(
    src_root: &Path,
    src_path: &Path,
    label: &str,
) -> Result<()> {
    let metadata = fs::symlink_metadata(src_path)
        .with_context(|| format!("{label} not found: {}", src_path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("{label} must not be a symlink: {}", src_path.display());
    }
    if !file_type.is_file() {
        bail!("{label} must be a regular file: {}", src_path.display());
    }

    let canonical_root = src_root.canonicalize().with_context(|| {
        format!(
            "Failed to resolve bundle source directory {}",
            src_root.display()
        )
    })?;
    let canonical_source = src_path.canonicalize().with_context(|| {
        format!(
            "Failed to resolve {label} source path {}",
            src_path.display()
        )
    })?;
    if !canonical_source.starts_with(&canonical_root) {
        bail!(
            "{label} resolves outside bundle source directory: {}",
            src_path.display()
        );
    }

    Ok(())
}

fn ensure_regular_copy_directory_under_root(
    src_root: &Path,
    src_dir: &Path,
    label: &str,
) -> Result<()> {
    let metadata = fs::symlink_metadata(src_dir)
        .with_context(|| format!("{label} not found: {}", src_dir.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("{label} must not be a symlink: {}", src_dir.display());
    }
    if !file_type.is_dir() {
        bail!("{label} must be a directory: {}", src_dir.display());
    }

    let canonical_root = src_root.canonicalize().with_context(|| {
        format!(
            "Failed to resolve bundle source directory {}",
            src_root.display()
        )
    })?;
    let canonical_source = src_dir.canonicalize().with_context(|| {
        format!(
            "Failed to resolve {label} source directory {}",
            src_dir.display()
        )
    })?;
    if !canonical_source.starts_with(&canonical_root) {
        bail!(
            "{label} resolves outside bundle source directory: {}",
            src_dir.display()
        );
    }

    Ok(())
}

/// Copy encrypted attachment blobs from source to destination
fn copy_blobs_directory(src_root: &Path, src_dir: &Path, dest_dir: &Path) -> Result<usize> {
    ensure_regular_copy_directory_under_root(src_root, src_dir, "Attachment blobs directory")?;
    fs::create_dir_all(dest_dir).context("Failed to create blobs directory")?;

    let mut count = 0;

    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();

        if file_type.is_file() {
            let Some(filename) = path.file_name() else {
                continue; // Skip entries without valid filenames
            };
            let dest_path = dest_dir.join(filename);
            fs::copy(&path, &dest_path)?;
            count += 1;
        }
    }

    Ok(count)
}

fn validate_embedded_vendor_asset(asset: &PinnedVendorAsset) -> Result<()> {
    let actual_size = u64::try_from(asset.bytes.len()).context("Vendor asset size overflow")?;
    if actual_size != asset.size {
        bail!(
            "Embedded Pages vendor asset {} has size {}, expected {}",
            asset.path,
            actual_size,
            asset.size
        );
    }

    let actual_sha256 = hex::encode(Sha256::digest(asset.bytes));
    if actual_sha256 != asset.sha256 {
        bail!(
            "Embedded Pages vendor asset {} has SHA-256 {}, expected {}",
            asset.path,
            actual_sha256,
            asset.sha256
        );
    }

    Ok(())
}

/// Materialize the exact self-hosted runtime dependency set into `site/vendor`.
///
/// The embedded bytes are checked against independent hard-coded pins before
/// any file is written. This turns accidental vendor replacement into a build
/// failure instead of silently publishing unreviewed executable code.
pub(crate) fn write_pinned_vendor_assets(site_dir: &Path) -> Result<()> {
    for asset in PAGES_VENDOR_ASSETS {
        validate_embedded_vendor_asset(asset)?;
    }

    fs::create_dir_all(site_dir.join("vendor"))
        .context("Failed to create site/vendor directory")?;
    for asset in PAGES_VENDOR_ASSETS {
        fs::write(site_dir.join(asset.path), asset.bytes)
            .with_context(|| format!("Failed to write Pages vendor asset {}", asset.path))?;
    }

    Ok(())
}

/// Verify that a materialized bundle contains every pinned vendor file as an
/// exact regular-file byte match.
pub(crate) fn validate_pinned_vendor_assets(site_dir: &Path) -> Result<()> {
    for asset in PAGES_VENDOR_ASSETS {
        let path = site_dir.join(asset.path);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("Missing Pages vendor asset {}", asset.path))?;
        if !metadata.file_type().is_file() {
            bail!("Pages vendor asset {} is not a regular file", asset.path);
        }
        if metadata.len() != asset.size {
            bail!(
                "Pages vendor asset {} has size {}, expected {}",
                asset.path,
                metadata.len(),
                asset.size
            );
        }

        let entry = build_integrity_entry(&path)
            .with_context(|| format!("Failed to hash Pages vendor asset {}", asset.path))?;
        if entry.sha256 != asset.sha256 {
            bail!(
                "Pages vendor asset {} has SHA-256 {}, expected {}",
                asset.path,
                entry.sha256,
                asset.sha256
            );
        }
    }

    Ok(())
}

/// Generate integrity manifest for all files in a directory
pub(crate) fn generate_integrity_manifest(dir: &Path) -> Result<IntegrityManifest> {
    let mut files = BTreeMap::new();

    collect_file_hashes(dir, dir, &mut files)?;

    Ok(IntegrityManifest {
        version: 1,
        generated_at: Utc::now().to_rfc3339(),
        files,
    })
}

/// Recursively collect SHA256 hashes of all files
fn collect_file_hashes(
    base_dir: &Path,
    current_dir: &Path,
    files: &mut BTreeMap<String, IntegrityEntry>,
) -> Result<()> {
    let canonical_base_dir = base_dir.canonicalize().with_context(|| {
        format!(
            "Failed to resolve site directory {} while generating integrity manifest",
            base_dir.display()
        )
    })?;
    collect_file_hashes_recursive(base_dir, current_dir, &canonical_base_dir, files)
}

fn collect_file_hashes_recursive(
    base_dir: &Path,
    current_dir: &Path,
    canonical_base_dir: &Path,
    files: &mut BTreeMap<String, IntegrityEntry>,
) -> Result<()> {
    for entry in fs::read_dir(current_dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();
        let rel_path = path.strip_prefix(base_dir)?;
        let rel_str = rel_path.to_string_lossy().replace('\\', "/");

        // Skip integrity.json itself (chicken/egg)
        if rel_str == "integrity.json" {
            continue;
        }

        if file_type.is_dir() {
            collect_file_hashes_recursive(base_dir, &path, canonical_base_dir, files)?;
        } else if file_type.is_symlink() {
            let canonical_target = path.canonicalize().with_context(|| {
                format!(
                    "Failed to resolve symlink {} while generating integrity manifest",
                    rel_str
                )
            })?;
            if !canonical_target.starts_with(canonical_base_dir) {
                bail!(
                    "Refusing to include symlink outside site directory in integrity manifest: {}",
                    rel_str
                );
            }

            let target_meta = fs::metadata(&path).with_context(|| {
                format!(
                    "Failed to read symlink target metadata for {} while generating integrity manifest",
                    rel_str
                )
            })?;
            if !target_meta.is_file() {
                bail!(
                    "Refusing to include symlink that does not point to a regular file in integrity manifest: {}",
                    rel_str
                );
            }

            files.insert(rel_str, build_integrity_entry(&path)?);
        } else if file_type.is_file() {
            files.insert(rel_str, build_integrity_entry(&path)?);
        }
    }

    Ok(())
}

fn build_integrity_entry(path: &Path) -> Result<IntegrityEntry> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let size = metadata.len();

    let mut hasher = Sha256::new();
    let mut reader = BufReader::new(file);
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(IntegrityEntry {
        // sha2 ≥ 0.11 dropped `LowerHex` for the `Output` GenericArray;
        // route through `hex::encode` for the same lowercase-hex wire
        // format.
        sha256: hex::encode(hasher.finalize()),
        size,
    })
}

/// Compute a short fingerprint from the integrity manifest
pub(crate) fn compute_fingerprint(manifest: &IntegrityManifest) -> String {
    // Compute a fingerprint by hashing the sorted list of file hashes
    let mut hasher = Sha256::new();

    for (path, entry) in &manifest.files {
        hasher.update(path.as_bytes());
        hasher.update(entry.sha256.as_bytes());
    }

    let hash = hasher.finalize();

    // Return first 16 hex chars as fingerprint. `hex::encode` replaces the
    // pre-sha2-0.11 `format!("{:x}", hash)` path (Output no longer
    // implements `LowerHex`).
    hex::encode(hash)[..16].to_string()
}

/// Write private artifacts that should never be deployed
pub(crate) fn write_private_fingerprint(private_dir: &Path, fingerprint: &str) -> Result<()> {
    let fingerprint_content = format!(
        "Integrity Fingerprint: {}\n\n\
        Generated: {}\n\n\
        Verify this fingerprint matches the one displayed in the web viewer\n\
        before proceeding. If it doesn't match, the archive may have been\n\
        tampered with.\n",
        fingerprint,
        Utc::now().to_rfc3339()
    );
    write_private_artifact_file(
        private_dir,
        "integrity-fingerprint.txt",
        fingerprint_content.as_bytes(),
    )?;
    Ok(())
}

fn ensure_private_artifact_dir(private_dir: &Path) -> Result<()> {
    ensure_existing_parent_ancestors_are_real_dirs(private_dir, "private artifact directory")?;

    match fs::symlink_metadata(private_dir) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!(
                    "private artifact directory must not be a symlink: {}",
                    private_dir.display()
                );
            }
            if !file_type.is_dir() {
                bail!(
                    "private artifact path must be a directory: {}",
                    private_dir.display()
                );
            }
            secure_private_artifact_dir(private_dir)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            create_private_artifact_dir(private_dir)?;
            ensure_private_artifact_dir(private_dir)
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to inspect private artifact directory {}",
                private_dir.display()
            )
        }),
    }
}

fn create_private_artifact_dir(private_dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(private_dir).with_context(|| {
            format!(
                "Failed to create owner-only private artifact directory {}",
                private_dir.display()
            )
        })?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(private_dir).with_context(|| {
        format!(
            "Failed to create private artifact directory {}",
            private_dir.display()
        )
    })?;
    Ok(())
}

fn secure_private_artifact_dir(private_dir: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(private_dir, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Failed to set owner-only permissions on private artifact directory {}",
            private_dir.display()
        )
    })?;
    #[cfg(not(unix))]
    let _ = private_dir;
    Ok(())
}

fn reject_symlinked_private_artifact(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!(
                    "private artifact file must not be a symlink: {}",
                    path.display()
                );
            }
            if file_type.is_dir() {
                bail!(
                    "private artifact path must be a regular file, not a directory: {}",
                    path.display()
                );
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err)
            .with_context(|| format!("Failed to inspect private artifact {}", path.display())),
    }
}

fn write_private_artifact_file(private_dir: &Path, filename: &str, contents: &[u8]) -> Result<()> {
    if filename.contains(['/', '\\']) {
        bail!("private artifact filename must not contain path separators: {filename}");
    }

    ensure_private_artifact_dir(private_dir)?;
    let final_path = private_dir.join(filename);
    reject_symlinked_private_artifact(&final_path)?;
    let temp_path = unique_bundle_sidecar_path(&final_path, "tmp", "private_artifact")?;

    let write_result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp_path).with_context(|| {
            format!(
                "Failed to create temporary private artifact {}",
                temp_path.display()
            )
        })?;
        file.write_all(contents).with_context(|| {
            format!(
                "Failed to write temporary private artifact {}",
                temp_path.display()
            )
        })?;
        file.sync_all().with_context(|| {
            format!(
                "Failed to sync temporary private artifact {}",
                temp_path.display()
            )
        })?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    let mut retain_temp_on_replace_error = false;
    if let Err(err) = replace_private_artifact_from_temp(
        &temp_path,
        &final_path,
        &mut retain_temp_on_replace_error,
    ) {
        if !retain_temp_on_replace_error {
            let _ = fs::remove_file(&temp_path);
        }
        return Err(err);
    }
    Ok(())
}

#[cfg(windows)]
fn private_artifact_path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err)
            .with_context(|| format!("Failed to inspect private artifact {}", path.display())),
    }
}

#[cfg(any(windows, test))]
fn replace_private_artifact_from_temp_via_backup(
    temp_path: &Path,
    final_path: &Path,
    first_error: &std::io::Error,
    retain_temp_on_error: &mut bool,
) -> Result<()> {
    *retain_temp_on_error = false;
    reject_symlinked_private_artifact(final_path)?;
    let backup_path = unique_bundle_sidecar_path(final_path, "bak", "private_artifact")?;
    fs::rename(final_path, &backup_path).with_context(|| {
        format!(
            "Failed to preserve existing private artifact {} at {} after initial replacement error: {}",
            final_path.display(),
            backup_path.display(),
            first_error
        )
    })?;

    match fs::rename(temp_path, final_path) {
        Ok(()) => {
            fs::remove_file(&backup_path).with_context(|| {
                format!(
                    "Installed private artifact {} but failed to remove prior generation {}",
                    final_path.display(),
                    backup_path.display()
                )
            })?;
            sync_parent_directory(final_path)
        }
        Err(replace_error) => match fs::rename(&backup_path, final_path) {
            Ok(()) => {
                sync_parent_directory(final_path)?;
                Err(replace_error).with_context(|| {
                    format!(
                        "Failed to replace private artifact {} after initial error {}; restored its prior generation",
                        final_path.display(),
                        first_error
                    )
                })
            }
            Err(restore_error) => {
                *retain_temp_on_error = true;
                bail!(
                    "Failed to replace private artifact {} after initial error {}; replacement error: {}; restore error: {}; prior generation retained at {} and new generation retained at {}",
                    final_path.display(),
                    first_error,
                    replace_error,
                    restore_error,
                    backup_path.display(),
                    temp_path.display()
                )
            }
        },
    }
}

fn replace_private_artifact_from_temp(
    temp_path: &Path,
    final_path: &Path,
    retain_temp_on_error: &mut bool,
) -> Result<()> {
    *retain_temp_on_error = false;
    #[cfg(windows)]
    {
        match fs::rename(temp_path, final_path) {
            Ok(()) => sync_parent_directory(final_path),
            Err(first_error)
                if matches!(
                    first_error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) && private_artifact_path_entry_exists(final_path)? =>
            {
                replace_private_artifact_from_temp_via_backup(
                    temp_path,
                    final_path,
                    &first_error,
                    retain_temp_on_error,
                )
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "Failed to install private artifact {} from {}",
                    final_path.display(),
                    temp_path.display()
                )
            }),
        }
    }

    #[cfg(not(windows))]
    {
        fs::rename(temp_path, final_path).with_context(|| {
            format!(
                "Failed to install private artifact {} from {}",
                final_path.display(),
                temp_path.display()
            )
        })?;
        sync_parent_directory(final_path)
    }
}

pub(crate) fn write_private_artifacts_encrypted(
    private_dir: &Path,
    enc_config: &EncryptionConfig,
    recovery_secret: Option<&[u8]>,
    generate_qr: bool,
    cleanup_missing_recovery: bool,
) -> Result<()> {
    if generate_qr && recovery_secret.is_none() {
        bail!(
            "QR recovery artifacts were requested, but no recovery secret is available; enable recovery-secret generation or disable QR generation"
        );
    }
    ensure_private_artifact_dir(private_dir)?;

    let recovery_secret_path = private_dir.join("recovery-secret.txt");
    let qr_png_path = private_dir.join("qr-code.png");
    let qr_svg_path = private_dir.join("qr-code.svg");

    // Write recovery secret if provided
    if let Some(secret) = recovery_secret {
        let recovery_b64 = BASE64_URL_SAFE_NO_PAD.encode(secret);
        let recovery_content = format!(
            "Recovery Secret\n\
            ================\n\n\
            This secret can unlock your archive if you forget your password.\n\
            Store it securely and NEVER share it.\n\n\
            Secret (base64url):\n\
            {}\n\n\
            To use: Click \"Scan Recovery QR Code\" in the web viewer, or\n\
            use this base64 value with the recovery function.\n\n\
            Archive Export ID: {}\n\
            Generated: {}\n",
            recovery_b64,
            enc_config.export_id,
            Utc::now().to_rfc3339()
        );
        write_private_artifact_file(
            private_dir,
            "recovery-secret.txt",
            recovery_content.as_bytes(),
        )?;

        // Generate QR code if requested
        if generate_qr {
            generate_qr_codes(private_dir, &recovery_b64)?;
        } else {
            remove_file_if_exists(&qr_png_path)?;
            remove_file_if_exists(&qr_svg_path)?;
        }
    } else if cleanup_missing_recovery {
        remove_file_if_exists(&recovery_secret_path)?;
        remove_file_if_exists(&qr_png_path)?;
        remove_file_if_exists(&qr_svg_path)?;
    }

    // Write master key backup (encrypted DEK wrapped with KEK)
    let master_key_backup = master_key_backup_json(enc_config, Utc::now().to_rfc3339());
    let master_key_json = serde_json::to_vec_pretty(&master_key_backup)?;
    write_private_artifact_file(private_dir, "master-key.json", &master_key_json)?;

    Ok(())
}

fn master_key_backup_json(
    enc_config: &EncryptionConfig,
    generated_at: String,
) -> serde_json::Value {
    serde_json::json!({
        "export_id": &enc_config.export_id,
        "key_slots": &enc_config.key_slots,
        "note": MASTER_KEY_BACKUP_NOTE,
        "generated_at": generated_at,
    })
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn write_private_unencrypted_notice(private_dir: &Path) -> Result<()> {
    let content = format!(
        "UNENCRYPTED ARCHIVE WARNING\n\
        ============================\n\n\
        This bundle was generated WITHOUT encryption.\n\
        Anyone with access to the site can read its contents.\n\n\
        Generated: {}\n",
        Utc::now().to_rfc3339()
    );
    write_private_artifact_file(private_dir, "unencrypted-warning.txt", content.as_bytes())?;
    Ok(())
}

/// Generate QR code images for recovery secret
fn generate_qr_codes(private_dir: &Path, recovery_b64: &str) -> Result<()> {
    generate_qr_codes_with(
        private_dir,
        recovery_b64,
        super::qr::generate_qr_png,
        super::qr::generate_qr_svg,
    )
}

fn generate_qr_codes_with<P, S>(
    private_dir: &Path,
    recovery_b64: &str,
    generate_png: P,
    generate_svg: S,
) -> Result<()>
where
    P: FnOnce(&str) -> Result<Vec<u8>>,
    S: FnOnce(&str) -> Result<String>,
{
    // Generate both representations before writing either one. If a requested
    // encoder is unavailable or rejects the payload, callers receive an error
    // and no partial QR artifact set is published.
    let qr_png = generate_png(recovery_b64)
        .context("Failed to generate requested recovery QR PNG artifact")?;
    let qr_svg = generate_svg(recovery_b64)
        .context("Failed to generate requested recovery QR SVG artifact")?;

    write_private_artifact_file(private_dir, "qr-code.png", &qr_png)
        .context("Failed to write requested recovery QR PNG artifact")?;
    write_private_artifact_file(private_dir, "qr-code.svg", qr_svg.as_bytes())
        .context("Failed to write requested recovery QR SVG artifact")?;

    Ok(())
}

/// Generate public README for the site directory
fn generate_public_readme(title: &str, description: &str, is_encrypted: bool) -> String {
    let about_line = if is_encrypted {
        "This is an encrypted, searchable archive of AI coding agent conversations"
    } else {
        "This is a searchable archive of AI coding agent conversations (not encrypted)"
    };

    let security_section = if is_encrypted {
        r#"## Security

- All data is encrypted with AES-256-GCM
- Password-based key derivation uses Argon2id
- The archive can be safely hosted on public servers
- No data is accessible without the correct password"#
    } else {
        r#"## Security

⚠️ This archive is **NOT encrypted**.
Anyone with access to the site can read its contents.
Host it only on a trusted, private location."#
    };

    let open_section = if is_encrypted {
        r#"## How to Open

1. Host these files on any static web server
2. Open index.html in a modern browser
3. Verify the fingerprint matches your records
4. Enter your password to decrypt"#
    } else {
        r#"## How to Open

1. Host these files on any static web server
2. Open index.html in a modern browser
3. Verify the fingerprint matches your records
4. The archive loads immediately (no password required)"#
    };

    let technical_section = if is_encrypted {
        r#"## Technical Details

- Encryption: AES-256-GCM with chunked streaming
- KDF: Argon2id (64MB memory, 3 iterations)
- Search: SQLite with FTS5 (runs in browser via sql.js)
- Requires: SharedArrayBuffer (COOP/COEP headers)"#
    } else {
        r#"## Technical Details

- Encryption: none (unencrypted archive)
- Search: SQLite with FTS5 (runs in browser via sql.js)
- Requires: SharedArrayBuffer (COOP/COEP headers)"#
    };

    let files_section = if is_encrypted {
        r#"- `index.html` - Entry point
- `config.json` - Public encryption parameters (no secrets)
- `integrity.json` - SHA256 hashes for all files
- `payload/` - Encrypted database chunks
- `*.js` - Application code
- `styles.css` - Styling"#
    } else {
        r#"- `index.html` - Entry point
- `config.json` - Plaintext archive metadata and payload location
- `integrity.json` - SHA256 hashes for all files
- `payload/data.db` - Unencrypted SQLite database (publicly readable)
- `*.js` - Application code
- `styles.css` - Styling"#
    };

    format!(
        r#"# {}

{}

## About This Archive

{}
generated by [cass](https://github.com/Dicklesworthstone/coding_agent_session_search).

{}

{}

{}

## Files

{}

## Hosting Requirements

For the viewer to function correctly, your web server must set:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

The included service worker (sw.js) handles this automatically for
most static hosts (GitHub Pages, Cloudflare Pages, etc.).

---

Generated by cass v{}
"#,
        title,
        description,
        about_line,
        security_section,
        open_section,
        technical_section,
        files_section,
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::archive_config::{ArchiveConfig, UnencryptedPayload};
    use tempfile::TempDir;

    fn write_unencrypted_source(root: &Path, payload_name: &str, body: &str) {
        let payload_dir = root.join("payload");
        fs::create_dir_all(&payload_dir).unwrap();
        let payload_path = payload_dir.join(payload_name);
        fs::write(&payload_path, body).unwrap();

        let config = ArchiveConfig::Unencrypted(UnencryptedConfig {
            encrypted: false,
            version: "1.0.0".to_string(),
            payload: UnencryptedPayload {
                path: format!("payload/{payload_name}"),
                format: "sqlite".to_string(),
                size_bytes: Some(body.len() as u64),
            },
            warning: Some("UNENCRYPTED".to_string()),
        });

        let file = File::create(root.join("config.json")).unwrap();
        serde_json::to_writer_pretty(BufWriter::new(file), &config).unwrap();
    }

    fn encrypted_config_for_files(files: Vec<&str>) -> EncryptionConfig {
        use base64::prelude::*;
        let chunk_count = files.len();
        let total_plaintext_size = if chunk_count == 0 {
            0
        } else {
            ((chunk_count - 1) * 1024 + 1) as u64
        };
        EncryptionConfig {
            version: crate::pages::encrypt::SCHEMA_VERSION,
            // The shared payload-format validation decodes these as base64
            // and requires exactly 16 / 12 bytes.
            export_id: BASE64_STANDARD.encode([0u8; 16]),
            base_nonce: BASE64_STANDARD.encode([0u8; 12]),
            compression: "deflate".to_string(),
            kdf_defaults: crate::pages::encrypt::Argon2Params::default(),
            payload: crate::pages::encrypt::PayloadMeta {
                chunk_size: 1024,
                chunk_count,
                total_compressed_size: chunk_count as u64,
                total_plaintext_size,
                files: files.into_iter().map(str::to_string).collect(),
            },
            key_slots: vec![crate::pages::encrypt::KeySlot {
                id: 0,
                slot_type: crate::pages::encrypt::SlotType::Password,
                kdf: crate::pages::encrypt::KdfAlgorithm::Argon2id,
                salt: BASE64_STANDARD.encode([0u8; 16]),
                wrapped_dek: BASE64_STANDARD.encode([0u8; 48]),
                nonce: BASE64_STANDARD.encode([0u8; 12]),
                argon2_params: Some(crate::pages::encrypt::Argon2Params::default()),
            }],
        }
    }

    #[test]
    fn test_bundle_builder_default() {
        let builder = BundleBuilder::new();
        assert_eq!(builder.config.title, "cass Archive");
        assert!(!builder.config.hide_metadata);
        assert!(!builder.config.generate_qr);
    }

    #[test]
    fn test_bundle_builder_fluent() {
        let builder = BundleBuilder::new()
            .title("My Archive")
            .description("Test description")
            .hide_metadata(true)
            .generate_qr(true);

        assert_eq!(builder.config.title, "My Archive");
        assert_eq!(builder.config.description, "Test description");
        assert!(builder.config.hide_metadata);
        assert!(builder.config.generate_qr);
    }

    #[test]
    fn test_compute_fingerprint() {
        let mut files = BTreeMap::new();
        files.insert(
            "test.txt".to_string(),
            IntegrityEntry {
                sha256: "abc123".to_string(),
                size: 100,
            },
        );

        let manifest = IntegrityManifest {
            version: 1,
            generated_at: "2024-01-01T00:00:00Z".to_string(),
            files,
        };

        let fingerprint = compute_fingerprint(&manifest);
        assert_eq!(fingerprint.len(), 16);

        // Same manifest should produce same fingerprint
        let fingerprint2 = compute_fingerprint(&manifest);
        assert_eq!(fingerprint, fingerprint2);
    }

    #[test]
    fn test_master_key_backup_json_shape() {
        let config = EncryptionConfig {
            version: 2,
            export_id: "export-123".to_string(),
            base_nonce: "nonce".to_string(),
            compression: "deflate".to_string(),
            kdf_defaults: crate::pages::encrypt::Argon2Params::default(),
            payload: crate::pages::encrypt::PayloadMeta {
                chunk_size: 1024,
                chunk_count: 0,
                total_compressed_size: 0,
                total_plaintext_size: 0,
                files: Vec::new(),
            },
            key_slots: Vec::new(),
        };

        let backup = master_key_backup_json(&config, "2026-04-25T19:08:00Z".to_string());

        assert_eq!(backup["export_id"], "export-123");
        assert_eq!(backup["key_slots"], serde_json::json!([]));
        assert_eq!(backup["note"], MASTER_KEY_BACKUP_NOTE);
        assert_eq!(backup["generated_at"], "2026-04-25T19:08:00Z");
    }

    #[test]
    #[cfg(unix)]
    fn private_artifacts_are_owner_only_even_under_permissive_ambient_modes() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new()?;
        let private_dir = temp.path().join("private");
        fs::create_dir_all(&private_dir)?;
        fs::set_permissions(&private_dir, fs::Permissions::from_mode(0o777))?;

        write_private_artifact_file(&private_dir, "recovery-secret.txt", b"unlock material")?;

        let dir_mode = fs::metadata(&private_dir)?.permissions().mode();
        let file_mode = fs::metadata(private_dir.join("recovery-secret.txt"))?
            .permissions()
            .mode();
        if dir_mode & 0o077 != 0 {
            return Err(anyhow!("private directory mode was {dir_mode:o}"));
        }
        if file_mode & 0o077 != 0 {
            return Err(anyhow!("private file mode was {file_mode:o}"));
        }
        Ok(())
    }

    #[test]
    fn private_artifact_writer_replaces_an_existing_generation() -> Result<()> {
        let temp = TempDir::new()?;
        let private_dir = temp.path().join("private");

        write_private_artifact_file(&private_dir, "master-key.json", b"old generation")?;
        write_private_artifact_file(&private_dir, "master-key.json", b"new generation")?;

        let installed = fs::read(private_dir.join("master-key.json"))?;
        if installed != b"new generation" {
            return Err(anyhow!(
                "private artifact replacement published stale bytes"
            ));
        }
        Ok(())
    }

    #[test]
    fn requested_qr_requires_a_recovery_secret() -> Result<()> {
        let temp = TempDir::new()?;
        let private_dir = temp.path().join("private");
        let config = encrypted_config_for_files(Vec::new());

        let error = write_private_artifacts_encrypted(&private_dir, &config, None, true, false)
            .expect_err("a requested QR without a recovery secret must fail closed");

        if !error
            .to_string()
            .contains("no recovery secret is available")
        {
            return Err(anyhow!("unexpected missing-recovery error: {error:#}"));
        }
        if private_dir.exists() {
            return Err(anyhow!(
                "rejected QR request unexpectedly created a private artifact directory"
            ));
        }
        Ok(())
    }

    #[test]
    fn requested_qr_generation_failure_writes_no_partial_artifacts() -> Result<()> {
        let temp = TempDir::new()?;
        let private_dir = temp.path().join("private");

        let error = generate_qr_codes_with(
            &private_dir,
            "recovery-secret",
            |_| Ok(vec![1, 2, 3]),
            |_| Err(anyhow!("SVG encoder unavailable")),
        )
        .expect_err("a requested QR encoder failure must propagate");

        let message = format!("{error:#}");
        if !message.contains("requested recovery QR SVG artifact")
            || !message.contains("SVG encoder unavailable")
        {
            return Err(anyhow!("QR generation error lost its cause: {message}"));
        }
        if private_dir.join("qr-code.png").exists() || private_dir.join("qr-code.svg").exists() {
            return Err(anyhow!(
                "failed QR generation published a partial artifact set"
            ));
        }
        Ok(())
    }

    #[test]
    fn requested_qr_png_generation_failure_writes_no_artifacts() -> Result<()> {
        let temp = TempDir::new()?;
        let private_dir = temp.path().join("private");

        let error = generate_qr_codes_with(
            &private_dir,
            "recovery-secret",
            |_| Err(anyhow!("PNG encoder unavailable")),
            |_| Ok("<svg>recovery</svg>".to_string()),
        )
        .expect_err("a requested QR PNG encoder failure must propagate");

        let message = format!("{error:#}");
        if !message.contains("requested recovery QR PNG artifact")
            || !message.contains("PNG encoder unavailable")
        {
            return Err(anyhow!("QR PNG generation error lost its cause: {message}"));
        }
        if private_dir.join("qr-code.png").exists() || private_dir.join("qr-code.svg").exists() {
            return Err(anyhow!("failed QR PNG generation published an artifact"));
        }
        Ok(())
    }

    #[test]
    fn requested_qr_generation_writes_both_artifacts() -> Result<()> {
        let temp = TempDir::new()?;
        let private_dir = temp.path().join("private");

        generate_qr_codes_with(
            &private_dir,
            "recovery-secret",
            |_| Ok(vec![1, 2, 3]),
            |_| Ok("<svg>recovery</svg>".to_string()),
        )?;

        if fs::read(private_dir.join("qr-code.png"))? != [1, 2, 3] {
            return Err(anyhow!("requested QR PNG bytes were not published"));
        }
        if fs::read_to_string(private_dir.join("qr-code.svg"))? != "<svg>recovery</svg>" {
            return Err(anyhow!("requested QR SVG bytes were not published"));
        }
        Ok(())
    }

    #[test]
    fn private_artifact_backup_fallback_preserves_then_replaces() -> Result<()> {
        let temp = TempDir::new()?;
        let final_path = temp.path().join("master-key.json");
        let staged_path = temp.path().join("master-key.tmp.json");
        fs::write(&final_path, b"old generation")?;
        fs::write(&staged_path, b"new generation")?;
        let first_error = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
        let mut retain_temp_on_error = false;

        replace_private_artifact_from_temp_via_backup(
            &staged_path,
            &final_path,
            &first_error,
            &mut retain_temp_on_error,
        )?;

        if retain_temp_on_error {
            return Err(anyhow!(
                "successful private artifact fallback requested temp retention"
            ));
        }
        if fs::read(&final_path)? != b"new generation" {
            return Err(anyhow!(
                "private artifact fallback did not publish staged bytes"
            ));
        }
        if staged_path.exists() {
            return Err(anyhow!(
                "private artifact fallback left the staged path behind"
            ));
        }
        let remaining_entries = fs::read_dir(temp.path())?.collect::<std::io::Result<Vec<_>>>()?;
        if remaining_entries.len() != 1 {
            return Err(anyhow!(
                "private artifact fallback left an unexpected backup sidecar"
            ));
        }
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn bundle_staging_root_is_private_until_publish() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new()?;
        let staging_root = temp.path().join("bundle.staged");
        create_bundle_staging_root(&staging_root)?;

        let staged_mode = fs::metadata(&staging_root)?.permissions().mode();
        if staged_mode & 0o077 != 0 {
            return Err(anyhow!("bundle staging root mode was {staged_mode:o}"));
        }

        prepare_bundle_root_for_publish(&staging_root)?;
        let published_mode = fs::metadata(&staging_root)?.permissions().mode();
        if published_mode & 0o777 != 0o755 {
            return Err(anyhow!("published bundle root mode was {published_mode:o}"));
        }
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_private_artifacts_reject_symlinked_secret_file() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let private_dir = temp.path().join("private");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&private_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        let protected_secret = outside_dir.join("protected-secret.txt");
        fs::write(&protected_secret, "do not overwrite").unwrap();
        symlink(&protected_secret, private_dir.join("recovery-secret.txt")).unwrap();

        let config = encrypted_config_for_files(Vec::new());
        let err = write_private_artifacts_encrypted(
            &private_dir,
            &config,
            Some(&[7u8; 32]),
            false,
            false,
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("must not be a symlink"),
            "unexpected error: {err:#}"
        );
        assert_eq!(
            fs::read_to_string(&protected_secret).unwrap(),
            "do not overwrite"
        );
        assert!(
            fs::symlink_metadata(private_dir.join("recovery-secret.txt"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "rejected private artifact symlink should be left intact"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_private_artifacts_cleanup_rejects_symlinked_private_dir_before_removal() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let outside_dir = temp.path().join("outside");
        let private_dir = temp.path().join("private");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(outside_dir.join("recovery-secret.txt"), "keep recovery").unwrap();
        fs::write(outside_dir.join("qr-code.png"), "keep png").unwrap();
        fs::write(outside_dir.join("qr-code.svg"), "keep svg").unwrap();
        symlink(&outside_dir, &private_dir).unwrap();

        let config = encrypted_config_for_files(Vec::new());
        let err = write_private_artifacts_encrypted(&private_dir, &config, None, false, true)
            .unwrap_err();

        assert!(
            err.to_string().contains("must not be a symlink"),
            "unexpected error: {err:#}"
        );
        assert_eq!(
            fs::read_to_string(outside_dir.join("recovery-secret.txt")).unwrap(),
            "keep recovery"
        );
        assert_eq!(
            fs::read_to_string(outside_dir.join("qr-code.png")).unwrap(),
            "keep png"
        );
        assert_eq!(
            fs::read_to_string(outside_dir.join("qr-code.svg")).unwrap(),
            "keep svg"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_private_artifacts_reject_symlinked_parent_before_writing() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let linked_parent = temp.path().join("linked-parent");
        let private_dir = linked_parent.join("private");
        symlink(outside_dir.path(), &linked_parent).unwrap();

        let err = write_private_fingerprint(&private_dir, "fingerprint").unwrap_err();

        assert!(
            err.to_string().contains("parent must not contain symlinks"),
            "unexpected error: {err:#}"
        );
        assert!(
            fs::read_dir(outside_dir.path()).unwrap().next().is_none(),
            "private artifact writer must not create files through a symlinked parent"
        );
    }

    #[test]
    fn test_generate_public_readme() {
        let readme = generate_public_readme("Test Archive", "A test archive", true);
        assert!(readme.contains("Test Archive"));
        assert!(readme.contains("A test archive"));
        assert!(readme.contains("AES-256-GCM"));
        assert!(readme.contains("Argon2id"));
        assert!(readme.contains("Public encryption parameters"));
        assert!(readme.contains("Encrypted database chunks"));

        let unencrypted = generate_public_readme("Test Archive", "A test archive", false);
        assert!(unencrypted.contains("NOT encrypted"));
        assert!(unencrypted.contains("no password required"));
        assert!(unencrypted.contains("Plaintext archive metadata"));
        assert!(unencrypted.contains("Unencrypted SQLite database"));
        assert!(!unencrypted.contains("Public encryption parameters"));
        assert!(!unencrypted.contains("Encrypted database chunks"));
    }

    #[test]
    fn test_integrity_manifest_excludes_itself() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path();

        // Create some test files
        fs::write(temp_path.join("test.txt"), "hello").unwrap();
        fs::write(temp_path.join("integrity.json"), "{}").unwrap();

        let manifest = generate_integrity_manifest(temp_path).unwrap();

        // Should include test.txt but not integrity.json
        assert!(manifest.files.contains_key("test.txt"));
        assert!(!manifest.files.contains_key("integrity.json"));
    }

    #[test]
    fn pinned_vendor_contract_matches_every_embedded_byte() {
        for asset in PAGES_VENDOR_ASSETS {
            validate_embedded_vendor_asset(asset)
                .unwrap_or_else(|error| panic!("invalid vendor pin for {}: {error:#}", asset.path));
        }

        let manifest: serde_json::Value = serde_json::from_slice(
            PAGES_VENDOR_ASSETS
                .iter()
                .find(|asset| asset.path == "vendor/manifest.json")
                .expect("vendor manifest pin")
                .bytes,
        )
        .expect("valid vendor manifest JSON");
        let package_versions = manifest["packages"]
            .as_array()
            .expect("vendor package list")
            .iter()
            .map(|package| {
                (
                    package["name"].as_str().expect("package name"),
                    package["version"].as_str().expect("package version"),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            package_versions.get("@sqlite.org/sqlite-wasm"),
            Some(&"3.53.0-build1")
        );
        assert_eq!(package_versions.get("argon2-browser"), Some(&"1.18.0"));
        assert_eq!(package_versions.get("fflate"), Some(&"0.8.3"));
        assert_eq!(package_versions.get("html5-qrcode"), Some(&"2.3.8"));
    }

    #[test]
    fn pinned_vendor_contract_rejects_hash_mutations() {
        let mutated = PinnedVendorAsset {
            path: "vendor/mutated.js",
            bytes: b"mutated",
            size: 7,
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
        };
        let error = validate_embedded_vendor_asset(&mutated).unwrap_err();
        assert!(error.to_string().contains("SHA-256"));
    }

    #[test]
    fn bundle_build_validates_materialized_vendor_before_integrity_publish() {
        let source = include_str!("bundle.rs");
        let validation = source
            .find("validate_pinned_vendor_assets(&site_dir)")
            .expect("materialized vendor validation in build path");
        let integrity = source
            .find("progress(\"integrity\", \"Generating integrity manifest...\")")
            .expect("integrity generation step");
        let publish = source
            .find("replace_dir_from_temp(")
            .expect("bundle publication step");
        assert!(validation < integrity && integrity < publish);
    }

    #[test]
    fn materialized_vendor_validation_rejects_same_size_tampering() {
        let temp = TempDir::new().unwrap();
        write_pinned_vendor_assets(temp.path()).unwrap();
        validate_pinned_vendor_assets(temp.path()).unwrap();

        let fflate_path = temp.path().join("vendor/fflate.js");
        let mut mutated = fs::read(&fflate_path).unwrap();
        mutated[0] ^= 0x01;
        fs::write(fflate_path, mutated).unwrap();

        let error = validate_pinned_vendor_assets(temp.path()).unwrap_err();
        assert!(error.to_string().contains("SHA-256"));
    }

    #[test]
    fn test_collect_file_hashes() {
        let temp = TempDir::new().unwrap();
        let temp_path = temp.path();

        // Create nested structure
        fs::create_dir_all(temp_path.join("subdir")).unwrap();
        fs::write(temp_path.join("root.txt"), "root").unwrap();
        fs::write(temp_path.join("subdir/nested.txt"), "nested").unwrap();

        let mut files = BTreeMap::new();
        collect_file_hashes(temp_path, temp_path, &mut files).unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.contains_key("root.txt"));
        assert!(files.contains_key("subdir/nested.txt"));

        // Verify hash is SHA256 hex (64 chars)
        for entry in files.values() {
            assert_eq!(entry.sha256.len(), 64);
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_collect_file_hashes_includes_symlinked_files_within_site() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let temp_path = temp.path();

        fs::write(temp_path.join("real.txt"), "real").unwrap();
        symlink("real.txt", temp_path.join("linked-file.txt")).unwrap();

        let mut files = BTreeMap::new();
        collect_file_hashes(temp_path, temp_path, &mut files).unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.contains_key("real.txt"));
        assert!(files.contains_key("linked-file.txt"));
        assert_eq!(files["real.txt"].sha256, files["linked-file.txt"].sha256);
        assert_eq!(files["real.txt"].size, files["linked-file.txt"].size);
    }

    #[test]
    #[cfg(unix)]
    fn test_collect_file_hashes_rejects_symlinks_outside_site() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let temp_path = temp.path();
        let outside = TempDir::new().unwrap();

        fs::write(temp_path.join("root.txt"), "root").unwrap();
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        fs::create_dir_all(outside.path().join("nested")).unwrap();
        fs::write(outside.path().join("nested/hidden.txt"), "hidden").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            temp_path.join("linked-file.txt"),
        )
        .unwrap();
        symlink(outside.path().join("nested"), temp_path.join("linked-dir")).unwrap();

        let mut files = BTreeMap::new();
        let err = collect_file_hashes(temp_path, temp_path, &mut files).unwrap_err();
        assert!(
            err.to_string().contains("outside site directory"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn test_copy_payload_chunks_copies_only_manifest_files() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let payload_dir = src.path().join("payload");
        fs::create_dir_all(&payload_dir).unwrap();

        fs::write(payload_dir.join("chunk-00000.bin"), "chunk").unwrap();
        fs::write(payload_dir.join("chunk-99999.bin"), "stale chunk").unwrap();
        fs::write(payload_dir.join("secret.bin"), "unlisted payload").unwrap();

        let config = encrypted_config_for_files(vec!["payload/chunk-00000.bin"]);
        let copied = copy_payload_chunks(src.path(), &payload_dir, dst.path(), &config).unwrap();
        assert_eq!(copied, 1);
        assert!(dst.path().join("chunk-00000.bin").exists());
        assert!(!dst.path().join("chunk-99999.bin").exists());
        assert!(!dst.path().join("secret.bin").exists());
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_payload_chunks_rejects_manifest_symlinked_chunk() {
        use std::os::unix::fs::symlink;

        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let payload_dir = src.path().join("payload");
        fs::create_dir_all(&payload_dir).unwrap();

        fs::write(outside.path().join("secret.bin"), "secret").unwrap();
        symlink(
            outside.path().join("secret.bin"),
            payload_dir.join("chunk-00000.bin"),
        )
        .unwrap();

        let config = encrypted_config_for_files(vec!["payload/chunk-00000.bin"]);
        let err = copy_payload_chunks(src.path(), &payload_dir, dst.path(), &config).unwrap_err();
        assert!(
            err.to_string().contains("must not be a symlink"),
            "unexpected error: {err:#}"
        );
        assert!(!dst.path().join("chunk-00000.bin").exists());
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_payload_chunks_rejects_symlinked_source_directory() {
        use std::os::unix::fs::symlink;

        let source = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        fs::write(outside.path().join("chunk-0.bin"), "outside chunk").unwrap();
        symlink(outside.path(), source.path().join("payload")).unwrap();

        let config = encrypted_config_for_files(vec!["payload/chunk-00000.bin"]);
        let err = copy_payload_chunks(
            source.path(),
            &source.path().join("payload"),
            dst.path(),
            &config,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("must not be a symlink"),
            "unexpected error: {err:#}"
        );
        assert!(!dst.path().join("chunk-0.bin").exists());
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_unencrypted_payload_rejects_final_symlink() {
        use std::os::unix::fs::symlink;

        let source = TempDir::new().unwrap();
        let site = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        fs::create_dir_all(source.path().join("payload")).unwrap();
        fs::write(outside.path().join("secret.db"), "outside secret").unwrap();
        symlink(
            outside.path().join("secret.db"),
            source.path().join("payload/data.db"),
        )
        .unwrap();

        let config = UnencryptedConfig {
            encrypted: false,
            version: "1.0.0".to_string(),
            payload: UnencryptedPayload {
                path: "payload/data.db".to_string(),
                format: "sqlite".to_string(),
                size_bytes: None,
            },
            warning: None,
        };

        let err = copy_payload_file(source.path(), site.path(), &config).unwrap_err();
        assert!(
            err.to_string().contains("must not be a symlink"),
            "unexpected error: {err:#}"
        );
        assert!(!site.path().join("payload/data.db").exists());
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_unencrypted_payload_rejects_symlinked_parent_escape() {
        use std::os::unix::fs::symlink;

        let source = TempDir::new().unwrap();
        let site = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        fs::write(outside.path().join("data.db"), "outside secret").unwrap();
        symlink(outside.path(), source.path().join("payload")).unwrap();

        let config = UnencryptedConfig {
            encrypted: false,
            version: "1.0.0".to_string(),
            payload: UnencryptedPayload {
                path: "payload/data.db".to_string(),
                format: "sqlite".to_string(),
                size_bytes: None,
            },
            warning: None,
        };

        let err = copy_payload_file(source.path(), site.path(), &config).unwrap_err();
        assert!(
            err.to_string().contains("outside bundle source directory"),
            "unexpected error: {err:#}"
        );
        assert!(!site.path().join("payload/data.db").exists());
    }

    #[test]
    fn test_generated_docs_reject_path_traversal_filename() {
        let source = TempDir::new().unwrap();
        let output_parent = TempDir::new().unwrap();
        let output_dir = output_parent.path().join("bundle");

        write_unencrypted_source(source.path(), "data.db", "payload");

        let config = BundleConfig {
            generated_docs: vec![GeneratedDoc {
                filename: "../escaped.md".to_string(),
                content: "escaped".to_string(),
                location: DocLocation::WebRoot,
            }],
            ..BundleConfig::default()
        };

        let err = BundleBuilder::with_config(config)
            .build(source.path(), output_dir.as_path(), |_, _| {})
            .unwrap_err();
        assert!(
            err.to_string().contains("must not contain path separators"),
            "unexpected error: {err:#}"
        );
        assert!(!output_parent.path().join("escaped.md").exists());
        let leftovers = fs::read_dir(output_parent.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "rejected bundle staging directory leaked: {leftovers:?}"
        );
    }

    #[test]
    fn test_generated_docs_reject_backslash_separator_filename() {
        let doc = GeneratedDoc {
            filename: r"nested\escaped.md".to_string(),
            content: "escaped".to_string(),
            location: DocLocation::WebRoot,
        };

        let err = resolve_generated_doc_path(Path::new("site"), &doc).unwrap_err();
        assert!(
            err.to_string().contains("must not contain path separators"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_blobs_directory_skips_symlinked_files() {
        use std::os::unix::fs::symlink;

        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        fs::write(src.path().join("blob.bin"), "blob").unwrap();
        fs::write(outside.path().join("secret.bin"), "secret").unwrap();
        symlink(
            outside.path().join("secret.bin"),
            src.path().join("linked-blob.bin"),
        )
        .unwrap();

        let copied = copy_blobs_directory(src.path(), src.path(), dst.path()).unwrap();
        assert_eq!(copied, 1);
        assert!(dst.path().join("blob.bin").exists());
        assert!(!dst.path().join("linked-blob.bin").exists());
    }

    #[test]
    #[cfg(unix)]
    fn test_copy_blobs_directory_rejects_symlinked_source_directory() {
        use std::os::unix::fs::symlink;

        let source = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        fs::write(outside.path().join("blob.bin"), "outside blob").unwrap();
        symlink(outside.path(), source.path().join("blobs")).unwrap();

        let err = copy_blobs_directory(source.path(), &source.path().join("blobs"), dst.path())
            .unwrap_err();
        assert!(
            err.to_string().contains("must not be a symlink"),
            "unexpected error: {err:#}"
        );
        assert!(!dst.path().join("blob.bin").exists());
    }

    #[test]
    fn test_build_replaces_existing_bundle_without_stale_files() {
        let source = TempDir::new().unwrap();
        let output_parent = TempDir::new().unwrap();
        let output_dir = output_parent.path().join("bundle");

        write_unencrypted_source(source.path(), "data.db", "fresh payload");

        let builder = BundleBuilder::new();
        builder
            .build(source.path(), output_dir.as_path(), |_, _| {})
            .expect("initial build");

        fs::write(output_dir.join("site/stale.txt"), "stale").unwrap();
        fs::write(output_dir.join("private/old-secret.txt"), "secret").unwrap();
        fs::write(output_dir.join("site/payload/old.bin"), "old").unwrap();

        builder
            .build(source.path(), output_dir.as_path(), |_, _| {})
            .expect("rebuild");

        assert!(output_dir.join("site/config.json").exists());
        assert!(
            output_dir
                .join("private/integrity-fingerprint.txt")
                .exists()
        );
        assert!(!output_dir.join("site/stale.txt").exists());
        assert!(!output_dir.join("private/old-secret.txt").exists());
        assert!(!output_dir.join("site/payload/old.bin").exists());
        assert!(output_dir.join("site/payload/data.db").exists());
    }

    #[test]
    fn test_build_failure_preserves_existing_bundle() {
        let source = TempDir::new().unwrap();
        let output_parent = TempDir::new().unwrap();
        let output_dir = output_parent.path().join("bundle");
        let broken_source = TempDir::new().unwrap();

        write_unencrypted_source(source.path(), "data.db", "fresh payload");

        let builder = BundleBuilder::new();
        builder
            .build(source.path(), output_dir.as_path(), |_, _| {})
            .expect("initial build");

        fs::write(output_dir.join("site/marker.txt"), "keep me").unwrap();

        let result = builder.build(broken_source.path(), output_dir.as_path(), |_, _| {});
        assert!(result.is_err(), "broken rebuild should fail");

        assert!(output_dir.join("site/marker.txt").exists());
        assert!(output_dir.join("site/config.json").exists());
        assert!(
            output_dir
                .join("private/integrity-fingerprint.txt")
                .exists()
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_build_rejects_symlinked_output_directory() {
        use std::os::unix::fs::symlink;

        let source = TempDir::new().unwrap();
        let output_parent = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let output_dir = output_parent.path().join("bundle-link");

        write_unencrypted_source(source.path(), "data.db", "payload");
        symlink(outside.path(), &output_dir).unwrap();

        let err = BundleBuilder::new()
            .build(source.path(), output_dir.as_path(), |_, _| {})
            .unwrap_err();

        assert!(
            err.to_string().contains("must not be a symlink"),
            "unexpected error: {err:#}"
        );
        assert!(
            fs::symlink_metadata(&output_dir)
                .unwrap()
                .file_type()
                .is_symlink(),
            "rejected symlink output path must be preserved for operator inspection"
        );
        assert!(
            !outside.path().join("site").exists(),
            "build must not write through a symlinked output directory"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_build_rejects_symlinked_output_parent_before_staging() {
        use std::os::unix::fs::symlink;

        let source = TempDir::new().unwrap();
        let output_parent = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let linked_parent = output_parent.path().join("linked-parent");
        let output_dir = linked_parent.join("bundle");

        write_unencrypted_source(source.path(), "data.db", "payload");
        symlink(outside.path(), &linked_parent).unwrap();

        let err = BundleBuilder::new()
            .build(source.path(), output_dir.as_path(), |_, _| {})
            .unwrap_err();

        assert!(
            err.to_string().contains("parent must not contain symlinks"),
            "unexpected error: {err:#}"
        );
        assert!(
            fs::read_dir(outside.path()).unwrap().next().is_none(),
            "bundle builder must not stage output through a symlinked parent"
        );
    }

    #[test]
    fn test_build_rejects_requested_qr_for_unencrypted_archive() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let output = temp.path().join("bundle");
        fs::create_dir_all(&source).unwrap();
        write_unencrypted_source(&source, "data.db", "plaintext archive");

        let builder = BundleBuilder::with_config(BundleConfig {
            recovery_secret: Some(vec![7; 32]),
            generate_qr: true,
            ..BundleConfig::default()
        });
        let error = builder
            .build(&source, &output, |_, _| {})
            .expect_err("an unencrypted archive cannot provide recovery QR artifacts");

        assert!(
            error
                .to_string()
                .contains("requested for an unencrypted archive"),
            "unexpected QR/archive-mode error: {error:#}"
        );
        assert!(
            !output.exists(),
            "rejected QR request unexpectedly published a bundle"
        );
    }

    #[test]
    fn test_replace_dir_from_temp_overwrites_existing_bundle() {
        let temp = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let staged_dir = temp.path().join("bundle.staged");

        fs::create_dir_all(final_dir.join("site")).unwrap();
        fs::write(final_dir.join("site/old.txt"), "old").unwrap();

        fs::create_dir_all(staged_dir.join("site")).unwrap();
        fs::write(staged_dir.join("site/new.txt"), "new").unwrap();

        let mut retain_temp_on_error = false;
        replace_dir_from_temp(&staged_dir, &final_dir, &mut retain_temp_on_error).unwrap();

        assert!(!staged_dir.exists());
        assert!(final_dir.join("site/new.txt").exists());
        assert!(!final_dir.join("site/old.txt").exists());
        assert!(
            !bundle_publish_in_progress_backup_path(&final_dir).exists(),
            "deterministic recovery sidecar should be cleaned up"
        );
        assert!(
            !bundle_publish_marker_path(&final_dir).exists(),
            "completed replacement marker should be cleaned up"
        );
    }

    #[test]
    fn test_replace_dir_from_temp_preserves_first_publish_behavior() {
        let temp = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let staged_dir = temp.path().join("bundle.staged");

        fs::create_dir_all(staged_dir.join("site")).unwrap();
        fs::write(staged_dir.join("site/new.txt"), "new").unwrap();

        let mut retain_temp_on_error = false;
        replace_dir_from_temp(&staged_dir, &final_dir, &mut retain_temp_on_error).unwrap();

        assert!(!retain_temp_on_error);
        assert!(!staged_dir.exists());
        assert_eq!(
            fs::read_to_string(final_dir.join("site/new.txt")).unwrap(),
            "new"
        );
        assert!(!bundle_publish_in_progress_backup_path(&final_dir).exists());
    }

    #[test]
    fn test_recover_interrupted_bundle_publish_restores_parked_only_live_bundle() {
        let temp = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let staged_dir = temp.path().join("bundle.staged");
        let backup_dir = bundle_publish_in_progress_backup_path(&final_dir);

        fs::create_dir_all(final_dir.join("private")).unwrap();
        fs::write(final_dir.join("private/old-private.txt"), "old").unwrap();
        fs::create_dir_all(staged_dir.join("private")).unwrap();
        fs::write(staged_dir.join("private/new-private.txt"), "new").unwrap();

        // Failpoint state: the fallback publisher durably parked OLD, then
        // the process died before installing NEW at the live handle.
        fs::rename(&final_dir, &backup_dir).unwrap();
        assert!(!final_dir.exists());

        recover_interrupted_bundle_publish(&final_dir).unwrap();

        assert_eq!(
            fs::read_to_string(final_dir.join("private/old-private.txt")).unwrap(),
            "old"
        );
        assert!(!backup_dir.exists());
        assert_eq!(
            fs::read_to_string(staged_dir.join("private/new-private.txt")).unwrap(),
            "new",
            "recovery must not consume the next staged candidate"
        );
    }

    #[test]
    fn test_recover_interrupted_fallback_publish_keeps_new_live_and_cleans_parked_old() {
        let temp = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let staged_dir = temp.path().join("bundle.staged");
        let backup_dir = bundle_publish_in_progress_backup_path(&final_dir);

        fs::create_dir_all(final_dir.join("private")).unwrap();
        fs::write(final_dir.join("private/old-private.txt"), "old").unwrap();
        fs::create_dir_all(staged_dir.join("private")).unwrap();
        fs::write(staged_dir.join("private/new-private.txt"), "new").unwrap();
        fs::write(
            bundle_publish_marker_path(&staged_dir),
            BUNDLE_PUBLISH_MARKER_CONTENT,
        )
        .unwrap();

        // Failpoint state: OLD was parked and NEW reached the live handle,
        // then the process died before removing OLD.
        fs::rename(&final_dir, &backup_dir).unwrap();
        fs::rename(&staged_dir, &final_dir).unwrap();

        recover_interrupted_bundle_publish(&final_dir).unwrap();

        assert_eq!(
            fs::read_to_string(final_dir.join("private/new-private.txt")).unwrap(),
            "new"
        );
        assert!(!final_dir.join("private/old-private.txt").exists());
        assert!(!backup_dir.exists());
        assert!(!bundle_publish_marker_path(&final_dir).exists());
    }

    #[test]
    fn test_recover_interrupted_publish_preserves_unmarked_ambiguous_sidecar() {
        let temp = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let backup_dir = bundle_publish_in_progress_backup_path(&final_dir);

        fs::create_dir_all(final_dir.join("site")).unwrap();
        fs::write(final_dir.join("site/live.txt"), "live").unwrap();
        fs::create_dir_all(backup_dir.join("unrelated")).unwrap();
        fs::write(backup_dir.join("unrelated/sentinel.txt"), "preserve").unwrap();

        let error = recover_interrupted_bundle_publish(&final_dir)
            .expect_err("two unmarked trees must be preserved as ambiguous");
        let message = format!("{error:#}");

        assert!(
            message.contains("both unmarked"),
            "unexpected error: {message}"
        );
        assert_eq!(
            fs::read_to_string(final_dir.join("site/live.txt")).unwrap(),
            "live"
        );
        assert_eq!(
            fs::read_to_string(backup_dir.join("unrelated/sentinel.txt")).unwrap(),
            "preserve"
        );
    }

    #[test]
    fn test_recover_interrupted_atomic_publish_before_exchange_keeps_prior_live() {
        let temp = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let staged_dir = temp.path().join("bundle.staged");
        let exchange_dir = bundle_publish_in_progress_backup_path(&final_dir);

        fs::create_dir_all(final_dir.join("private")).unwrap();
        fs::write(final_dir.join("private/old-private.txt"), "old").unwrap();
        fs::create_dir_all(staged_dir.join("private")).unwrap();
        fs::write(staged_dir.join("private/new-private.txt"), "new").unwrap();
        fs::write(
            bundle_publish_marker_path(&staged_dir),
            BUNDLE_PUBLISH_MARKER_CONTENT,
        )
        .unwrap();

        // Failpoint state: NEW reached the deterministic exchange handle,
        // but the process died before the atomic exchange committed.
        fs::rename(&staged_dir, &exchange_dir).unwrap();

        recover_interrupted_bundle_publish(&final_dir).unwrap();

        assert_eq!(
            fs::read_to_string(final_dir.join("private/old-private.txt")).unwrap(),
            "old"
        );
        assert!(!final_dir.join("private/new-private.txt").exists());
        assert!(!exchange_dir.exists());
        assert!(!bundle_publish_marker_path(&final_dir).exists());
    }

    #[test]
    fn test_recover_interrupted_atomic_publish_immediately_after_exchange_keeps_new_live() {
        let temp = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let exchange_dir = bundle_publish_in_progress_backup_path(&final_dir);

        // Failpoint state immediately after exchange: NEW is live and carries
        // the marker that moved with it; OLD is discoverable at the canonical
        // exchange handle and carries no marker.
        fs::create_dir_all(final_dir.join("private")).unwrap();
        fs::write(final_dir.join("private/new-private.txt"), "new").unwrap();
        fs::write(
            bundle_publish_marker_path(&final_dir),
            BUNDLE_PUBLISH_MARKER_CONTENT,
        )
        .unwrap();
        fs::create_dir_all(exchange_dir.join("private")).unwrap();
        fs::write(exchange_dir.join("private/old-private.txt"), "old").unwrap();

        recover_interrupted_bundle_publish(&final_dir).unwrap();

        assert_eq!(
            fs::read_to_string(final_dir.join("private/new-private.txt")).unwrap(),
            "new"
        );
        assert!(!final_dir.join("private/old-private.txt").exists());
        assert!(!exchange_dir.exists());
        assert!(!bundle_publish_marker_path(&final_dir).exists());
    }

    #[test]
    fn test_recoverable_rename_pair_replaces_and_cleans_prior_bundle() {
        let temp = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let staged_dir = temp.path().join("bundle.staged");
        let backup_dir = bundle_publish_in_progress_backup_path(&final_dir);

        fs::create_dir_all(final_dir.join("site")).unwrap();
        fs::write(final_dir.join("site/old.txt"), "old").unwrap();
        fs::create_dir_all(staged_dir.join("site")).unwrap();
        fs::write(staged_dir.join("site/new.txt"), "new").unwrap();
        fs::write(
            bundle_publish_marker_path(&staged_dir),
            BUNDLE_PUBLISH_MARKER_CONTENT,
        )
        .unwrap();

        let mut retain_temp_on_error = false;
        replace_dir_from_temp_via_recoverable_rename_pair(
            &staged_dir,
            &final_dir,
            &backup_dir,
            &mut retain_temp_on_error,
        )
        .unwrap();

        assert!(!retain_temp_on_error);
        assert_eq!(
            fs::read_to_string(final_dir.join("site/new.txt")).unwrap(),
            "new"
        );
        assert!(!backup_dir.exists());
        assert!(!bundle_publish_marker_path(&final_dir).exists());
    }

    #[test]
    fn test_prior_private_bundle_cleanup_failure_names_every_retained_path() {
        let temp = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let backup_dir = bundle_publish_in_progress_backup_path(&final_dir);
        fs::create_dir_all(final_dir.join("site")).unwrap();
        fs::create_dir_all(backup_dir.join("private")).unwrap();
        fs::write(backup_dir.join("private/recovery-material.txt"), "private").unwrap();

        let error = cleanup_prior_bundle_after_publish_with(&backup_dir, &final_dir, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected cleanup denial",
            ))
        })
        .expect_err("cleanup failure must fail the publication result");
        let message = format!("{error:#}");

        assert!(message.contains(&final_dir.display().to_string()));
        assert!(message.contains(&backup_dir.display().to_string()));
        assert!(message.contains("injected cleanup denial"));
        assert!(message.contains("private artifacts retained"));
        assert!(backup_dir.join("private/recovery-material.txt").exists());
    }

    #[test]
    #[cfg(unix)]
    fn test_recover_interrupted_bundle_publish_rejects_symlinked_backup() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let backup_dir = bundle_publish_in_progress_backup_path(&final_dir);
        fs::create_dir_all(outside.path().join("private")).unwrap();
        fs::write(outside.path().join("private/material.txt"), "private").unwrap();
        symlink(outside.path(), &backup_dir).unwrap();

        let error = recover_interrupted_bundle_publish(&final_dir)
            .expect_err("a symlinked recovery backup must fail closed");

        assert!(error.to_string().contains("must not be a symlink"));
        assert!(
            error
                .to_string()
                .contains(&backup_dir.display().to_string())
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("private/material.txt")).unwrap(),
            "private"
        );
        assert!(!final_dir.exists());
    }

    #[test]
    #[cfg(unix)]
    fn test_replace_dir_from_temp_rejects_symlinked_staging_tree() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let staged_dir = temp.path().join("bundle.staged");
        fs::create_dir_all(final_dir.join("site")).unwrap();
        fs::write(final_dir.join("site/old.txt"), "old").unwrap();
        fs::create_dir_all(outside.path().join("site")).unwrap();
        fs::write(outside.path().join("site/new.txt"), "new").unwrap();
        symlink(outside.path(), &staged_dir).unwrap();

        let mut retain_temp_on_error = false;
        let error = replace_dir_from_temp(&staged_dir, &final_dir, &mut retain_temp_on_error)
            .expect_err("a symlinked staged bundle must fail closed");

        assert!(error.to_string().contains("must not be a symlink"));
        assert!(
            error
                .to_string()
                .contains(&staged_dir.display().to_string())
        );
        assert_eq!(
            fs::read_to_string(final_dir.join("site/old.txt")).unwrap(),
            "old"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("site/new.txt")).unwrap(),
            "new"
        );
        assert!(!retain_temp_on_error);
    }

    #[test]
    #[cfg(unix)]
    fn test_replace_dir_from_temp_rejects_dangling_symlink_target() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let staged_dir = temp.path().join("bundle.staged");

        fs::create_dir_all(staged_dir.join("site")).unwrap();
        fs::write(staged_dir.join("site/new.txt"), "new").unwrap();
        symlink(temp.path().join("missing-target"), &final_dir).unwrap();

        let mut retain_temp_on_error = false;
        let err =
            replace_dir_from_temp(&staged_dir, &final_dir, &mut retain_temp_on_error).unwrap_err();
        assert!(
            err.to_string().contains("must not be a symlink"),
            "unexpected error: {err:#}"
        );
        assert!(staged_dir.join("site/new.txt").exists());
        assert!(
            !retain_temp_on_error,
            "ordinary validation failures do not require recovery retention"
        );
        assert!(
            fs::symlink_metadata(&final_dir)
                .unwrap()
                .file_type()
                .is_symlink(),
            "dangling symlink target must not be silently replaced"
        );
    }
}
