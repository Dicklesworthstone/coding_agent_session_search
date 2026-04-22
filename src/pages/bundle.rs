//! Bundle builder for pages export.
//!
//! Creates the deployable static site bundle (site/) and private offline artifacts (private/)
//! from an export. Output is safe for public hosting (GitHub Pages / Cloudflare Pages).

use anyhow::{Context, Result, bail};
use base64::prelude::*;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read};
use std::path::{Path, PathBuf};

use super::archive_config::{ArchiveConfig, UnencryptedConfig};
use super::docs::{DocLocation, GeneratedDoc};
use super::encrypt::EncryptionConfig;

/// Files embedded from pages_assets at compile time
const PAGES_ASSETS: &[(&str, &[u8])] = &[
    ("index.html", include_bytes!("../pages_assets/index.html")),
    ("styles.css", include_bytes!("../pages_assets/styles.css")),
    ("auth.js", include_bytes!("../pages_assets/auth.js")),
    ("viewer.js", include_bytes!("../pages_assets/viewer.js")),
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
        }
    }
}

/// Bundle builder for creating static site exports
pub struct BundleBuilder {
    config: BundleConfig,
}

impl Default for BundleBuilder {
    fn default() -> Self {
        Self::new()
    }
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
    pub fn recovery_secret(mut self, secret: Option<Vec<u8>>) -> Self {
        self.config.recovery_secret = secret;
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

        if output_dir.exists() && !output_dir.is_dir() {
            bail!(
                "bundle output path points to a file, expected a directory: {}",
                output_dir.display()
            );
        }

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

        let temp_output_dir = unique_bundle_dir(output_dir, "tmp");
        let final_site_dir = output_dir.join("site");
        let final_private_dir = output_dir.join("private");
        let mut replace_attempted = false;
        let result = (|| -> Result<BundleResult> {
            progress("setup", "Creating directory structure...");

            // Stage the bundle under a unique temp root so reruns do not retain stale files.
            let site_dir = temp_output_dir.join("site");
            let private_dir = temp_output_dir.join("private");

            fs::create_dir_all(&site_dir).context("Failed to create site/ directory")?;
            fs::create_dir_all(&private_dir).context("Failed to create private/ directory")?;

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

            // Copy payload into site/payload/
            let (chunk_count, is_encrypted) = match archive_config.as_encrypted() {
                Some(_enc_config) => {
                    progress("payload", "Copying encrypted payload...");
                    let count = copy_payload_chunks(&payload_dir, &site_payload_dir)?;
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
                copy_blobs_directory(&blobs_dir, &site_blobs_dir)?
            } else {
                0
            };

            progress("config", "Writing configuration files...");

            // Write config.json to site/ (already has public params only)
            let site_config_path = site_dir.join("config.json");
            let config_file = File::create(&site_config_path)?;
            serde_json::to_writer_pretty(BufWriter::new(config_file), &archive_config)?;

            // Write site metadata
            let site_metadata = SiteMetadata {
                title: self.config.title.clone(),
                description: self.config.description.clone(),
                generated_at: Utc::now().to_rfc3339(),
                generator: "cass".to_string(),
                generator_version: env!("CARGO_PKG_VERSION").to_string(),
            };
            let site_json_path = site_dir.join("site.json");
            let site_json_file = File::create(&site_json_path)?;
            serde_json::to_writer_pretty(BufWriter::new(site_json_file), &site_metadata)?;

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
                    let dest_path = match doc.location {
                        DocLocation::RepoRoot => site_dir.join(&doc.filename),
                        DocLocation::WebRoot => site_dir.join(&doc.filename),
                    };
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

            progress("integrity", "Generating integrity manifest...");

            // Generate integrity.json for all files in site/
            let integrity_manifest = generate_integrity_manifest(&site_dir)?;
            let integrity_path = site_dir.join("integrity.json");
            let integrity_file = File::create(&integrity_path)?;
            serde_json::to_writer_pretty(BufWriter::new(integrity_file), &integrity_manifest)?;

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

            sync_tree(&temp_output_dir)?;
            replace_attempted = true;
            replace_dir_from_temp(&temp_output_dir, output_dir)
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

        if result.is_err() && !replace_attempted {
            let _ = fs::remove_dir_all(&temp_output_dir);
        }

        result
    }
}

fn unique_bundle_dir(path: &Path, suffix: &str) -> PathBuf {
    unique_bundle_sidecar_path(path, suffix, "pages_bundle")
}

fn unique_bundle_backup_dir(path: &Path) -> PathBuf {
    unique_bundle_sidecar_path(path, "bak", "pages_bundle")
}

fn unique_bundle_sidecar_path(path: &Path, suffix: &str, fallback_name: &str) -> PathBuf {
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

fn replace_dir_from_temp(temp_dir: &Path, final_dir: &Path) -> Result<()> {
    if !final_dir.exists() {
        fs::rename(temp_dir, final_dir).with_context(|| {
            format!(
                "failed renaming completed bundle {} into place at {}",
                temp_dir.display(),
                final_dir.display()
            )
        })?;
        sync_parent_directory(final_dir)?;
        return Ok(());
    }

    let backup_dir = unique_bundle_backup_dir(final_dir);
    fs::rename(final_dir, &backup_dir).with_context(|| {
        format!(
            "failed preparing backup {} before replacing {}",
            backup_dir.display(),
            final_dir.display()
        )
    })?;

    match fs::rename(temp_dir, final_dir) {
        Ok(()) => {
            sync_parent_directory(final_dir)?;
            let _ = fs::remove_dir_all(&backup_dir);
            sync_parent_directory(final_dir)?;
            Ok(())
        }
        Err(second_err) => match fs::rename(&backup_dir, final_dir) {
            Ok(()) => {
                let _ = fs::remove_dir_all(temp_dir);
                sync_parent_directory(final_dir)?;
                bail!(
                    "failed replacing {} with {}: {}; restored original bundle",
                    final_dir.display(),
                    temp_dir.display(),
                    second_err
                );
            }
            Err(restore_err) => {
                bail!(
                    "failed replacing {} with {}: {}; restore error: {}; temp bundle retained at {}",
                    final_dir.display(),
                    temp_dir.display(),
                    second_err,
                    restore_err,
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

/// Copy payload chunks from source to destination
fn copy_payload_chunks(src_dir: &Path, dest_dir: &Path) -> Result<usize> {
    let mut count = 0;

    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();

        if file_type.is_file() && path.extension().map(|e| e == "bin").unwrap_or(false) {
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
    if !src_path.is_file() {
        bail!("Unencrypted payload file not found: {}", src_path.display());
    }

    let dest_path = site_dir.join(rel_path);
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::copy(&src_path, &dest_path)?;
    Ok(1)
}

/// Copy encrypted attachment blobs from source to destination
fn copy_blobs_directory(src_dir: &Path, dest_dir: &Path) -> Result<usize> {
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
    fs::write(
        private_dir.join("integrity-fingerprint.txt"),
        fingerprint_content,
    )?;
    Ok(())
}

pub(crate) fn write_private_artifacts_encrypted(
    private_dir: &Path,
    enc_config: &EncryptionConfig,
    recovery_secret: Option<&[u8]>,
    generate_qr: bool,
    cleanup_missing_recovery: bool,
) -> Result<()> {
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
        fs::write(&recovery_secret_path, recovery_content)?;

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
    let master_key_backup = serde_json::json!({
        "export_id": enc_config.export_id,
        "key_slots": enc_config.key_slots,
        "note": "This file contains the wrapped DEK. Keep it with your recovery secret.",
        "generated_at": Utc::now().to_rfc3339(),
    });
    let master_key_path = private_dir.join("master-key.json");
    let master_key_file = File::create(&master_key_path)?;
    serde_json::to_writer_pretty(BufWriter::new(master_key_file), &master_key_backup)?;

    Ok(())
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
    fs::write(private_dir.join("unencrypted-warning.txt"), content)?;
    Ok(())
}

/// Generate QR code images for recovery secret
fn generate_qr_codes(private_dir: &Path, recovery_b64: &str) -> Result<()> {
    // Use the qr module from pages if available
    if let Ok(qr_png) = super::qr::generate_qr_png(recovery_b64) {
        fs::write(private_dir.join("qr-code.png"), qr_png)?;
    }

    if let Ok(qr_svg) = super::qr::generate_qr_svg(recovery_b64) {
        fs::write(private_dir.join("qr-code.svg"), qr_svg)?;
    }

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

- `index.html` - Entry point
- `config.json` - Public encryption parameters (no secrets)
- `integrity.json` - SHA256 hashes for all files
- `payload/` - Encrypted database chunks
- `*.js` - Application code
- `styles.css` - Styling

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
    fn test_generate_public_readme() {
        let readme = generate_public_readme("Test Archive", "A test archive", true);
        assert!(readme.contains("Test Archive"));
        assert!(readme.contains("A test archive"));
        assert!(readme.contains("AES-256-GCM"));
        assert!(readme.contains("Argon2id"));

        let unencrypted = generate_public_readme("Test Archive", "A test archive", false);
        assert!(unencrypted.contains("NOT encrypted"));
        assert!(unencrypted.contains("no password required"));
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
    #[cfg(unix)]
    fn test_copy_payload_chunks_skips_symlinked_bin_files() {
        use std::os::unix::fs::symlink;

        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        fs::write(src.path().join("chunk-0.bin"), "chunk").unwrap();
        fs::write(outside.path().join("secret.bin"), "secret").unwrap();
        symlink(
            outside.path().join("secret.bin"),
            src.path().join("chunk-linked.bin"),
        )
        .unwrap();

        let copied = copy_payload_chunks(src.path(), dst.path()).unwrap();
        assert_eq!(copied, 1);
        assert!(dst.path().join("chunk-0.bin").exists());
        assert!(!dst.path().join("chunk-linked.bin").exists());
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

        let copied = copy_blobs_directory(src.path(), dst.path()).unwrap();
        assert_eq!(copied, 1);
        assert!(dst.path().join("blob.bin").exists());
        assert!(!dst.path().join("linked-blob.bin").exists());
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
    fn test_replace_dir_from_temp_overwrites_existing_bundle() {
        let temp = TempDir::new().unwrap();
        let final_dir = temp.path().join("bundle");
        let staged_dir = temp.path().join("bundle.staged");

        fs::create_dir_all(final_dir.join("site")).unwrap();
        fs::write(final_dir.join("site/old.txt"), "old").unwrap();

        fs::create_dir_all(staged_dir.join("site")).unwrap();
        fs::write(staged_dir.join("site/new.txt"), "new").unwrap();

        replace_dir_from_temp(&staged_dir, &final_dir).unwrap();

        assert!(!staged_dir.exists());
        assert!(final_dir.join("site/new.txt").exists());
        assert!(!final_dir.join("site/old.txt").exists());
        let sidecars = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            !sidecars.iter().any(|name| name.contains(".bundle.bak.")),
            "backup sidecar should be cleaned up, found: {sidecars:?}"
        );
    }
}
