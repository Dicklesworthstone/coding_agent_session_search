//! Attachment support for pages export.
//!
//! Implements opt-in attachment handling for images, PDFs, and code snapshots
//! with proper encryption, size limits, and lazy loading.
//!
//! # Overview
//!
//! Attachments are stored in a `blobs/` directory with:
//! - Each blob named by its SHA-256 hash
//! - Blobs individually encrypted with unique nonces
//! - A manifest file mapping hashes to metadata
//!
//! # Size Limits
//!
//! - Per-file maximum: 10 MB (default)
//! - Total maximum: 100 MB (default, configurable)

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Default maximum size per attachment (10 MB)
pub const DEFAULT_MAX_FILE_SIZE: usize = 10 * 1024 * 1024;

/// Default maximum total size for all attachments (100 MB)
pub const DEFAULT_MAX_TOTAL_SIZE: usize = 100 * 1024 * 1024;

/// Default allowed MIME types
pub const DEFAULT_ALLOWED_MIME_TYPES: &[&str] = &[
    // Images
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/svg+xml",
    // Documents
    "application/pdf",
    // Text
    "text/plain",
    "text/html",
    "text/css",
    "text/javascript",
    "application/json",
    "application/xml",
];

/// Nonce derivation domain separator for blob encryption
const BLOB_NONCE_DOMAIN: &[u8] = b"cass-blob-nonce-v1";

/// Configuration for attachment processing
#[derive(Debug, Clone)]
pub struct AttachmentConfig {
    /// Whether attachment processing is enabled
    pub enabled: bool,
    /// Maximum size per file in bytes
    pub max_file_size_bytes: usize,
    /// Maximum total size for all attachments in bytes
    pub max_total_size_bytes: usize,
    /// Allowed MIME types
    pub allowed_mime_types: Vec<String>,
}

impl Default for AttachmentConfig {
    fn default() -> Self {
        Self {
            enabled: false, // Disabled by default
            max_file_size_bytes: DEFAULT_MAX_FILE_SIZE,
            max_total_size_bytes: DEFAULT_MAX_TOTAL_SIZE,
            allowed_mime_types: DEFAULT_ALLOWED_MIME_TYPES
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl AttachmentConfig {
    /// Create a new config with attachments enabled
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Default::default()
        }
    }

    /// Set the maximum file size
    pub fn with_max_file_size(mut self, bytes: usize) -> Self {
        self.max_file_size_bytes = bytes;
        self
    }

    /// Set the maximum total size
    pub fn with_max_total_size(mut self, bytes: usize) -> Self {
        self.max_total_size_bytes = bytes;
        self
    }

    /// Check if a MIME type is allowed
    pub fn is_mime_allowed(&self, mime_type: &str) -> bool {
        let Some(mime_type) = mime_type_essence(mime_type) else {
            return false;
        };
        self.allowed_mime_types
            .iter()
            .filter_map(|allowed| mime_type_essence(allowed))
            .any(|allowed| mime_type == allowed)
    }
}

fn mime_type_essence(mime_type: &str) -> Option<String> {
    let essence = mime_type.split(';').next()?.trim();
    if essence.is_empty() {
        return None;
    }
    Some(essence.to_ascii_lowercase())
}

/// Raw attachment data from a connector
#[derive(Debug, Clone)]
pub struct AttachmentData {
    /// Original filename
    pub filename: String,
    /// MIME type
    pub mime_type: String,
    /// Raw data bytes
    pub data: Vec<u8>,
}

/// Metadata for a processed attachment entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentEntry {
    /// SHA-256 hash of plaintext (used as blob filename)
    pub hash: String,
    /// Original filename
    pub filename: String,
    /// MIME type
    pub mime_type: String,
    /// Size in bytes
    pub size_bytes: usize,
    /// Associated message ID
    pub message_id: i64,
}

/// Manifest containing all attachment metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentManifest {
    /// Version of the manifest format
    pub version: u8,
    /// List of all attachments
    pub entries: Vec<AttachmentEntry>,
    /// Total size of all attachments
    pub total_size_bytes: usize,
}

impl Default for AttachmentManifest {
    fn default() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
            total_size_bytes: 0,
        }
    }
}

/// Attachment processor that collects and encrypts attachments
pub struct AttachmentProcessor {
    config: AttachmentConfig,
    entries: Vec<AttachmentEntry>,
    /// Map of hash -> data for deduplication
    blobs: HashMap<String, Vec<u8>>,
    total_size: usize,
    /// Count of skipped attachments
    skipped_count: usize,
}

impl AttachmentProcessor {
    /// Create a new attachment processor with the given configuration
    pub fn new(config: AttachmentConfig) -> Self {
        Self {
            config,
            entries: Vec::new(),
            blobs: HashMap::new(),
            total_size: 0,
            skipped_count: 0,
        }
    }

    /// Check if attachment processing is enabled
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Get the current total size
    pub fn total_size(&self) -> usize {
        self.total_size
    }

    /// Get the number of processed attachments
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Get the number of skipped attachments
    pub fn skipped_count(&self) -> usize {
        self.skipped_count
    }

    /// Process attachments from a message
    ///
    /// Returns a list of blob hashes that were successfully processed.
    /// Attachments that exceed size limits or have disallowed MIME types
    /// are logged and skipped.
    pub fn process_attachments(
        &mut self,
        message_id: i64,
        attachments: &[AttachmentData],
    ) -> Result<Vec<String>> {
        if !self.config.enabled {
            return Ok(Vec::new());
        }

        let mut refs = Vec::new();

        for attachment in attachments {
            // Check MIME type
            if !self.config.is_mime_allowed(&attachment.mime_type) {
                warn!(
                    filename = %attachment.filename,
                    mime_type = %attachment.mime_type,
                    "Skipping attachment with disallowed MIME type"
                );
                self.skipped_count += 1;
                continue;
            }

            // Check per-file size limit
            if attachment.data.len() > self.config.max_file_size_bytes {
                warn!(
                    filename = %attachment.filename,
                    size = attachment.data.len(),
                    limit = self.config.max_file_size_bytes,
                    "Skipping oversized attachment"
                );
                self.skipped_count += 1;
                continue;
            }

            // Check total size limit
            if self.total_size + attachment.data.len() > self.config.max_total_size_bytes {
                warn!(
                    filename = %attachment.filename,
                    current_total = self.total_size,
                    attachment_size = attachment.data.len(),
                    limit = self.config.max_total_size_bytes,
                    "Total attachment limit reached, skipping"
                );
                self.skipped_count += 1;
                continue;
            }

            // Compute SHA-256 hash
            let hash = compute_sha256_hex(&attachment.data);

            // Check for deduplication
            if self.blobs.contains_key(&hash) {
                debug!(
                    filename = %attachment.filename,
                    hash = %hash,
                    "Attachment already processed (deduplicated)"
                );
                // Still add the entry for this message
                self.entries.push(AttachmentEntry {
                    hash: hash.clone(),
                    filename: attachment.filename.clone(),
                    mime_type: attachment.mime_type.clone(),
                    size_bytes: attachment.data.len(),
                    message_id,
                });
                refs.push(hash);
                continue;
            }

            // Store the blob
            self.blobs.insert(hash.clone(), attachment.data.clone());
            self.total_size += attachment.data.len();

            // Create entry
            self.entries.push(AttachmentEntry {
                hash: hash.clone(),
                filename: attachment.filename.clone(),
                mime_type: attachment.mime_type.clone(),
                size_bytes: attachment.data.len(),
                message_id,
            });

            debug!(
                filename = %attachment.filename,
                hash = %hash,
                size = attachment.data.len(),
                "Processed attachment"
            );

            refs.push(hash);
        }

        Ok(refs)
    }

    /// Write encrypted blobs to the output directory
    ///
    /// Each blob is encrypted with AES-256-GCM using:
    /// - DEK: Same data encryption key as main database
    /// - Nonce: Derived from blob hash using HKDF
    /// - AAD: export_id || hash bytes
    pub fn write_encrypted_blobs(
        &self,
        output_dir: &Path,
        dek: &[u8; 32],
        export_id: &[u8; 16],
    ) -> Result<AttachmentManifest> {
        if self.blobs.is_empty() {
            return Ok(AttachmentManifest::default());
        }

        let blobs_dir = output_dir.join("blobs");
        ensure_real_output_directory(output_dir, "Attachment output directory")?;
        ensure_real_output_directory(&blobs_dir, "Attachment blobs directory")?;

        let cipher = Aes256Gcm::new_from_slice(dek).expect("Invalid DEK length");

        for (hash, data) in &self.blobs {
            let blob_path = blobs_dir.join(format!("{}.bin", hash));

            // Derive nonce from hash
            let nonce = derive_blob_nonce(hash);

            // Build AAD: export_id || hash_bytes
            let hash_bytes = hex::decode(hash).context("Invalid hash hex")?;
            let mut aad = Vec::with_capacity(export_id.len() + hash_bytes.len());
            aad.extend_from_slice(export_id);
            aad.extend_from_slice(&hash_bytes);

            // Encrypt
            let ciphertext = cipher
                .encrypt(
                    &Nonce::from(nonce),
                    Payload {
                        msg: data.as_slice(),
                        aad: &aad,
                    },
                )
                .map_err(|e| anyhow::anyhow!("Blob encryption failed: {}", e))?;

            write_ciphertext_file(&blob_path, &ciphertext, "attachment blob")?;

            debug!(hash = %hash, path = %blob_path.display(), "Wrote encrypted blob");
        }

        // Write encrypted manifest
        let manifest = AttachmentManifest {
            version: 1,
            entries: self.entries.clone(),
            total_size_bytes: self.total_size,
        };

        let manifest_json =
            serde_json::to_vec(&manifest).context("Failed to serialize manifest")?;

        // Use a fixed nonce for the manifest (derived from "manifest" string)
        let manifest_nonce = derive_blob_nonce("manifest");

        // AAD for manifest: just export_id
        let manifest_ciphertext = cipher
            .encrypt(
                &Nonce::from(manifest_nonce),
                Payload {
                    msg: &manifest_json,
                    aad: export_id,
                },
            )
            .map_err(|e| anyhow::anyhow!("Manifest encryption failed: {}", e))?;

        let manifest_path = blobs_dir.join("manifest.enc");
        write_ciphertext_file(&manifest_path, &manifest_ciphertext, "attachment manifest")?;

        info!(
            count = self.entries.len(),
            unique_blobs = self.blobs.len(),
            total_size = self.total_size,
            skipped = self.skipped_count,
            "Wrote encrypted attachments"
        );

        Ok(manifest)
    }
}

/// Compute SHA-256 hash of data and return as lowercase hex string
fn compute_sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex::encode(result)
}

/// Derive a unique 12-byte nonce from a blob identifier using HKDF
fn derive_blob_nonce(identifier: &str) -> [u8; 12] {
    crate::encryption::hkdf_extract_expand(identifier.as_bytes(), BLOB_NONCE_DOMAIN, b"nonce", 12)
        .expect("HKDF expansion should never fail for 12 bytes")
        .try_into()
        .expect("HKDF expansion should return the requested nonce length")
}

/// Decrypt a blob given the DEK, export_id, and hash
pub fn decrypt_blob(
    ciphertext: &[u8],
    dek: &[u8; 32],
    export_id: &[u8; 16],
    hash: &str,
) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(dek).expect("Invalid DEK length");

    // Derive nonce from hash
    let nonce = derive_blob_nonce(hash);

    // Build AAD
    let hash_bytes = hex::decode(hash).context("Invalid hash hex")?;
    let mut aad = Vec::with_capacity(export_id.len() + hash_bytes.len());
    aad.extend_from_slice(export_id);
    aad.extend_from_slice(&hash_bytes);

    // Decrypt
    let plaintext = cipher
        .decrypt(
            &Nonce::from(nonce),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("Blob decryption failed"))?;

    Ok(plaintext)
}

/// Decrypt the attachment manifest
pub fn decrypt_manifest(
    ciphertext: &[u8],
    dek: &[u8; 32],
    export_id: &[u8; 16],
) -> Result<AttachmentManifest> {
    let cipher = Aes256Gcm::new_from_slice(dek).expect("Invalid DEK length");

    // Use fixed nonce for manifest
    let nonce = derive_blob_nonce("manifest");

    // Decrypt
    let plaintext = cipher
        .decrypt(
            &Nonce::from(nonce),
            Payload {
                msg: ciphertext,
                aad: export_id,
            },
        )
        .map_err(|_| anyhow::anyhow!("Manifest decryption failed"))?;

    let manifest: AttachmentManifest =
        serde_json::from_slice(&plaintext).context("Failed to deserialize manifest")?;

    Ok(manifest)
}

pub(crate) fn reencrypt_blobs_into_dir(
    source_archive_dir: &Path,
    output_archive_dir: &Path,
    old_dek: &[u8; 32],
    old_export_id: &[u8; 16],
    new_dek: &[u8; 32],
    new_export_id: &[u8; 16],
) -> Result<()> {
    let source_blobs_dir = source_archive_dir.join("blobs");
    ensure_existing_ancestors_have_no_symlinks(
        &source_blobs_dir,
        "Source attachment blobs directory",
    )?;
    match fs::symlink_metadata(&source_blobs_dir) {
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                bail!(
                    "Refusing to re-encrypt attachments from symlinked blobs directory: {}",
                    source_blobs_dir.display()
                );
            }
            if !file_type.is_dir() {
                bail!(
                    "Refusing to re-encrypt attachments from non-directory blobs path: {}",
                    source_blobs_dir.display()
                );
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "Failed to inspect attachment blobs directory {}",
                    source_blobs_dir.display()
                )
            });
        }
    }

    let output_blobs_dir = output_archive_dir.join("blobs");
    ensure_real_output_directory(&output_blobs_dir, "Destination attachment blobs directory")?;

    let manifest_path = source_blobs_dir.join("manifest.enc");
    ensure_regular_ciphertext_file(&manifest_path, "attachment manifest")?;
    let manifest_ciphertext =
        fs::read(&manifest_path).context("Failed to read attachment manifest for rekey")?;
    let manifest = decrypt_manifest(&manifest_ciphertext, old_dek, old_export_id)
        .context("Failed to decrypt attachment manifest during key rotation")?;

    let mut plaintext_blobs: HashMap<String, Vec<u8>> = HashMap::new();
    for entry in &manifest.entries {
        if plaintext_blobs.contains_key(&entry.hash) {
            continue;
        }

        let blob_path = source_blobs_dir.join(format!("{}.bin", entry.hash));
        ensure_regular_ciphertext_file(&blob_path, &format!("attachment blob {}", entry.hash))?;
        let ciphertext = fs::read(&blob_path)
            .with_context(|| format!("Failed to read attachment blob {}", entry.hash))?;
        let plaintext = decrypt_blob(&ciphertext, old_dek, old_export_id, &entry.hash)
            .with_context(|| format!("Failed to decrypt attachment blob {}", entry.hash))?;
        plaintext_blobs.insert(entry.hash.clone(), plaintext);
    }

    let cipher = Aes256Gcm::new_from_slice(new_dek).expect("Invalid DEK length");

    for (hash, data) in plaintext_blobs {
        let nonce = derive_blob_nonce(&hash);
        let hash_bytes = hex::decode(&hash).context("Invalid hash hex")?;
        let mut aad = Vec::with_capacity(new_export_id.len() + hash_bytes.len());
        aad.extend_from_slice(new_export_id);
        aad.extend_from_slice(&hash_bytes);

        let ciphertext = cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: data.as_slice(),
                    aad: &aad,
                },
            )
            .map_err(|e| anyhow::anyhow!("Blob encryption failed during key rotation: {}", e))?;

        write_ciphertext_file(
            &output_blobs_dir.join(format!("{}.bin", hash)),
            &ciphertext,
            "attachment blob",
        )
        .with_context(|| format!("Failed to rewrite attachment blob {}", hash))?;
    }

    let manifest_json =
        serde_json::to_vec(&manifest).context("Failed to serialize attachment manifest")?;
    let manifest_nonce = derive_blob_nonce("manifest");
    let reencrypted_manifest = cipher
        .encrypt(
            &Nonce::from(manifest_nonce),
            Payload {
                msg: &manifest_json,
                aad: new_export_id,
            },
        )
        .map_err(|e| anyhow::anyhow!("Manifest encryption failed during key rotation: {}", e))?;

    write_ciphertext_file(
        &output_blobs_dir.join("manifest.enc"),
        &reencrypted_manifest,
        "attachment manifest",
    )
    .context("Failed to rewrite attachment manifest during key rotation")?;

    Ok(())
}

fn ensure_real_output_directory(path: &Path, label: &str) -> Result<()> {
    ensure_existing_ancestors_have_no_symlinks(path, label)?;
    fs::create_dir_all(path).with_context(|| format!("Failed to create {label}"))?;
    ensure_existing_ancestors_have_no_symlinks(path, label)?;

    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("Failed to inspect {label}"))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("{label} must not be a symlink: {}", path.display());
    }
    if !file_type.is_dir() {
        bail!("{label} must be a directory: {}", path.display());
    }
    Ok(())
}

fn ensure_existing_ancestors_have_no_symlinks(path: &Path, label: &str) -> Result<()> {
    let mut ancestors: Vec<PathBuf> = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    ancestors.reverse();

    for ancestor in ancestors {
        match fs::symlink_metadata(&ancestor) {
            Ok(metadata) => {
                let file_type = metadata.file_type();
                if file_type.is_symlink() {
                    if is_allowed_system_symlink_ancestor(&ancestor) {
                        continue;
                    }
                    bail!("{label} must not contain symlinks: {}", ancestor.display());
                }
                if !file_type.is_dir() {
                    bail!(
                        "{label} parent path must be a directory: {}",
                        ancestor.display()
                    );
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to inspect {label} {}", ancestor.display()));
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

fn write_ciphertext_file(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    ensure_replaceable_regular_file(path, label)?;
    let (mut pending, file) = PendingCiphertextFile::create(path, label)?;
    let mut writer = BufWriter::new(file);
    writer
        .write_all(bytes)
        .with_context(|| format!("Failed to write {label} {}", pending.path().display()))?;
    writer
        .flush()
        .with_context(|| format!("Failed to flush {label} {}", pending.path().display()))?;
    writer
        .get_ref()
        .sync_all()
        .with_context(|| format!("Failed to sync {label} {}", pending.path().display()))?;
    drop(writer);
    pending.persist(path, label)
}

fn ensure_replaceable_regular_file(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                bail!(
                    "Refusing to write {label} through symlink: {}",
                    path.display()
                );
            }
            if !file_type.is_file() {
                bail!(
                    "Refusing to replace {label} at non-file path: {}",
                    path.display()
                );
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            Err(err).with_context(|| format!("Failed to inspect {label} {}", path.display()))
        }
    }
}

struct PendingCiphertextFile {
    path: PathBuf,
    keep: bool,
}

impl PendingCiphertextFile {
    fn create(final_path: &Path, label: &str) -> Result<(Self, File)> {
        let parent = output_parent(final_path);
        let file_name = final_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("{label} path must name a file"))?
            .to_string_lossy();

        for attempt in 0..100u32 {
            let random: u64 = rand::random();
            let temp_path = parent.join(format!(
                ".{file_name}.cass-attachment-tmp.{}.{}.{:016x}",
                std::process::id(),
                attempt,
                random
            ));

            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
            {
                Ok(file) => {
                    return Ok((
                        Self {
                            path: temp_path,
                            keep: false,
                        },
                        file,
                    ));
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("Failed to create temporary {label} {}", temp_path.display())
                    });
                }
            }
        }

        bail!(
            "Failed to create a unique temporary {label} next to {} after 100 attempts",
            final_path.display()
        );
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn persist(&mut self, final_path: &Path, label: &str) -> Result<()> {
        replace_ciphertext_file_from_temp(&self.path, final_path, label)?;
        self.keep = true;
        Ok(())
    }
}

impl Drop for PendingCiphertextFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn output_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn replace_ciphertext_file_from_temp(
    temp_path: &Path,
    final_path: &Path,
    label: &str,
) -> Result<()> {
    replace_ciphertext_file_from_temp_impl(temp_path, final_path, label)?;
    sync_parent_directory(final_path)
}

#[cfg(not(windows))]
fn replace_ciphertext_file_from_temp_impl(
    temp_path: &Path,
    final_path: &Path,
    label: &str,
) -> Result<()> {
    fs::rename(temp_path, final_path).with_context(|| {
        format!(
            "Failed to install {label} {} from {}",
            final_path.display(),
            temp_path.display()
        )
    })
}

#[cfg(windows)]
fn replace_ciphertext_file_from_temp_impl(
    temp_path: &Path,
    final_path: &Path,
    label: &str,
) -> Result<()> {
    ensure_replaceable_regular_file(final_path, label)?;
    match fs::rename(temp_path, final_path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::copy(temp_path, final_path).with_context(|| {
                format!(
                    "Failed to install {label} {} from {}",
                    final_path.display(),
                    temp_path.display()
                )
            })?;
            fs::remove_file(temp_path).with_context(|| {
                format!(
                    "Failed to remove temporary {label} {} after install",
                    temp_path.display()
                )
            })?;
            Ok(())
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to install {label} {} from {}",
                final_path.display(),
                temp_path.display()
            )
        }),
    }
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    File::open(parent)
        .with_context(|| format!("Failed to open parent directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("Failed to sync parent directory {}", parent.display()))
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn ensure_regular_ciphertext_file(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Failed to inspect {label} at {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("Refusing to read {label} from symlink: {}", path.display());
    }
    if !file_type.is_file() {
        bail!(
            "Refusing to read {label} from non-file path: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_disabled() {
        let config = AttachmentConfig::default();
        assert!(!config.enabled);
    }

    #[test]
    fn test_enabled_config() {
        let config = AttachmentConfig::enabled();
        assert!(config.enabled);
        assert_eq!(config.max_file_size_bytes, DEFAULT_MAX_FILE_SIZE);
        assert_eq!(config.max_total_size_bytes, DEFAULT_MAX_TOTAL_SIZE);
    }

    #[test]
    fn test_mime_type_check() {
        let config = AttachmentConfig::enabled();
        assert!(config.is_mime_allowed("image/png"));
        assert!(config.is_mime_allowed("IMAGE/PNG"));
        assert!(config.is_mime_allowed("text/plain; charset=utf-8"));
        assert!(config.is_mime_allowed("image/jpeg"));
        assert!(config.is_mime_allowed("application/pdf"));
        assert!(config.is_mime_allowed("text/plain"));
        assert!(!config.is_mime_allowed("application/octet-stream"));
        assert!(!config.is_mime_allowed("video/mp4"));
        assert!(!config.is_mime_allowed("image/png-malicious"));
        assert!(!config.is_mime_allowed("text/html+xml"));
        assert!(!config.is_mime_allowed(""));
    }

    #[test]
    fn test_size_limit_per_file() {
        let config = AttachmentConfig::enabled().with_max_file_size(1024);
        let mut processor = AttachmentProcessor::new(config);

        let large_attachment = AttachmentData {
            filename: "large.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: vec![0u8; 2048], // Over limit
        };

        let refs = processor
            .process_attachments(1, &[large_attachment])
            .unwrap();

        assert!(refs.is_empty()); // Should be skipped
        assert_eq!(processor.skipped_count(), 1);
    }

    #[test]
    fn test_total_size_limit() {
        let config = AttachmentConfig::enabled()
            .with_max_file_size(1024)
            .with_max_total_size(2048);
        let mut processor = AttachmentProcessor::new(config);

        // Add 3 attachments of ~800 bytes each - should only get 2
        for i in 0..3 {
            let attachment = AttachmentData {
                filename: format!("file{}.txt", i),
                mime_type: "text/plain".to_string(),
                data: vec![i as u8; 800],
            };
            processor.process_attachments(i as i64, &[attachment]).ok();
        }

        assert_eq!(processor.count(), 2);
        assert_eq!(processor.skipped_count(), 1);
    }

    #[test]
    fn test_deduplication() {
        let config = AttachmentConfig::enabled();
        let mut processor = AttachmentProcessor::new(config);

        let data = vec![1u8, 2, 3, 4, 5];

        // Same data in two attachments
        let attachment1 = AttachmentData {
            filename: "file1.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: data.clone(),
        };
        let attachment2 = AttachmentData {
            filename: "file2.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: data.clone(),
        };

        processor.process_attachments(1, &[attachment1]).unwrap();
        processor.process_attachments(2, &[attachment2]).unwrap();

        // Two entries but only one unique blob
        assert_eq!(processor.count(), 2);
        assert_eq!(processor.blobs.len(), 1);
        // Size should only count once
        assert_eq!(processor.total_size(), data.len());
    }

    #[test]
    fn test_sha256_hash() {
        let data = b"hello world";
        let hash = compute_sha256_hex(data);
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_blob_nonce_deterministic() {
        let nonce1 = derive_blob_nonce("test-hash");
        let nonce2 = derive_blob_nonce("test-hash");
        assert_eq!(nonce1, nonce2);

        let nonce3 = derive_blob_nonce("different-hash");
        assert_ne!(nonce1, nonce3);
    }

    #[test]
    fn test_blob_encryption_roundtrip() {
        let data = b"secret attachment data";
        let dek = [0x42u8; 32];
        let export_id = [0x01u8; 16];
        let hash = compute_sha256_hex(data);

        // Encrypt
        let cipher = Aes256Gcm::new_from_slice(&dek).unwrap();
        let nonce = derive_blob_nonce(&hash);
        let hash_bytes = hex::decode(&hash).unwrap();
        let mut aad = Vec::new();
        aad.extend_from_slice(&export_id);
        aad.extend_from_slice(&hash_bytes);

        let ciphertext = cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &data[..],
                    aad: &aad,
                },
            )
            .unwrap();

        // Decrypt
        let plaintext = decrypt_blob(&ciphertext, &dek, &export_id, &hash).unwrap();

        assert_eq!(plaintext, data);
    }

    #[test]
    fn test_write_encrypted_blobs() {
        use tempfile::TempDir;

        let config = AttachmentConfig::enabled();
        let mut processor = AttachmentProcessor::new(config);

        let attachment = AttachmentData {
            filename: "test.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: b"test content".to_vec(),
        };

        processor.process_attachments(1, &[attachment]).unwrap();

        let temp_dir = TempDir::new().unwrap();
        let dek = [0x42u8; 32];
        let export_id = [0x01u8; 16];

        let manifest = processor
            .write_encrypted_blobs(temp_dir.path(), &dek, &export_id)
            .unwrap();

        // Check blobs directory exists
        let blobs_dir = temp_dir.path().join("blobs");
        assert!(blobs_dir.exists());

        // Check manifest.enc exists
        assert!(blobs_dir.join("manifest.enc").exists());

        // Check manifest contents
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].filename, "test.txt");

        // Check blob file exists
        let blob_path = blobs_dir.join(format!("{}.bin", manifest.entries[0].hash));
        assert!(blob_path.exists());

        // Verify decryption
        let ciphertext = std::fs::read(&blob_path).unwrap();
        let plaintext =
            decrypt_blob(&ciphertext, &dek, &export_id, &manifest.entries[0].hash).unwrap();
        assert_eq!(plaintext, b"test content");
    }

    #[test]
    #[cfg(unix)]
    fn test_write_encrypted_blobs_rejects_symlinked_blobs_directory() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let config = AttachmentConfig::enabled();
        let mut processor = AttachmentProcessor::new(config);
        let attachment = AttachmentData {
            filename: "test.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: b"test content".to_vec(),
        };
        processor.process_attachments(1, &[attachment]).unwrap();

        let output_dir = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        symlink(outside_dir.path(), output_dir.path().join("blobs")).unwrap();

        let dek = [0x42u8; 32];
        let export_id = [0x01u8; 16];
        let err = processor
            .write_encrypted_blobs(output_dir.path(), &dek, &export_id)
            .unwrap_err();

        assert!(
            err.to_string().contains("must not contain symlinks")
                || err.to_string().contains("must not be a symlink"),
            "unexpected error: {err:#}"
        );
        assert!(
            !outside_dir.path().join("manifest.enc").exists(),
            "attachment writer must not write through a symlinked blobs directory"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_write_encrypted_blobs_rejects_symlinked_blob_file() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let config = AttachmentConfig::enabled();
        let mut processor = AttachmentProcessor::new(config);
        let data = b"test content".to_vec();
        let hash = compute_sha256_hex(&data);
        let attachment = AttachmentData {
            filename: "test.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data,
        };
        processor.process_attachments(1, &[attachment]).unwrap();

        let output_dir = TempDir::new().unwrap();
        let blobs_dir = output_dir.path().join("blobs");
        fs::create_dir_all(&blobs_dir).unwrap();
        let protected_target = output_dir.path().join("protected.bin");
        fs::write(&protected_target, b"do not overwrite").unwrap();
        symlink(&protected_target, blobs_dir.join(format!("{hash}.bin"))).unwrap();

        let dek = [0x42u8; 32];
        let export_id = [0x01u8; 16];
        let err = processor
            .write_encrypted_blobs(output_dir.path(), &dek, &export_id)
            .unwrap_err();

        assert!(
            err.to_string().contains("through symlink"),
            "unexpected error: {err:#}"
        );
        assert_eq!(
            fs::read(&protected_target).unwrap(),
            b"do not overwrite",
            "attachment writer must not clobber a symlink target"
        );
    }

    #[test]
    fn test_manifest_encryption_roundtrip() {
        let manifest = AttachmentManifest {
            version: 1,
            entries: vec![AttachmentEntry {
                hash: "abc123".to_string(),
                filename: "test.txt".to_string(),
                mime_type: "text/plain".to_string(),
                size_bytes: 100,
                message_id: 1,
            }],
            total_size_bytes: 100,
        };

        let dek = [0x42u8; 32];
        let export_id = [0x01u8; 16];

        // Encrypt
        let cipher = Aes256Gcm::new_from_slice(&dek).unwrap();
        let nonce = derive_blob_nonce("manifest");
        let manifest_json = serde_json::to_vec(&manifest).unwrap();

        let ciphertext = cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &manifest_json,
                    aad: &export_id,
                },
            )
            .unwrap();

        // Decrypt
        let decrypted = decrypt_manifest(&ciphertext, &dek, &export_id).unwrap();

        assert_eq!(decrypted.entries.len(), 1);
        assert_eq!(decrypted.entries[0].hash, "abc123");
    }

    #[test]
    fn test_reencrypt_existing_blobs_roundtrip() {
        use tempfile::TempDir;

        let config = AttachmentConfig::enabled();
        let mut processor = AttachmentProcessor::new(config);
        let attachment = AttachmentData {
            filename: "test.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: b"test content".to_vec(),
        };
        processor.process_attachments(1, &[attachment]).unwrap();

        let temp_dir = TempDir::new().unwrap();
        let old_dek = [0x42u8; 32];
        let old_export_id = [0x01u8; 16];
        let new_dek = [0x24u8; 32];
        let new_export_id = [0x02u8; 16];

        let manifest = processor
            .write_encrypted_blobs(temp_dir.path(), &old_dek, &old_export_id)
            .unwrap();

        reencrypt_blobs_into_dir(
            temp_dir.path(),
            temp_dir.path(),
            &old_dek,
            &old_export_id,
            &new_dek,
            &new_export_id,
        )
        .unwrap();

        let blobs_dir = temp_dir.path().join("blobs");
        let manifest_ciphertext = fs::read(blobs_dir.join("manifest.enc")).unwrap();
        let decrypted_manifest =
            decrypt_manifest(&manifest_ciphertext, &new_dek, &new_export_id).unwrap();
        assert_eq!(decrypted_manifest.entries.len(), 1);
        assert_eq!(decrypted_manifest.entries[0].hash, manifest.entries[0].hash);

        let blob_ciphertext =
            fs::read(blobs_dir.join(format!("{}.bin", manifest.entries[0].hash))).unwrap();
        let blob_plaintext = decrypt_blob(
            &blob_ciphertext,
            &new_dek,
            &new_export_id,
            &manifest.entries[0].hash,
        )
        .unwrap();
        assert_eq!(blob_plaintext, b"test content");
        assert!(decrypt_manifest(&manifest_ciphertext, &old_dek, &old_export_id).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn test_reencrypt_existing_blobs_rejects_symlinked_blobs_directory() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let config = AttachmentConfig::enabled();
        let mut processor = AttachmentProcessor::new(config);
        let attachment = AttachmentData {
            filename: "test.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: b"test content".to_vec(),
        };
        processor.process_attachments(1, &[attachment]).unwrap();

        let source_archive_dir = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let output_archive_dir = TempDir::new().unwrap();
        let old_dek = [0x42u8; 32];
        let old_export_id = [0x01u8; 16];
        let new_dek = [0x24u8; 32];
        let new_export_id = [0x02u8; 16];

        processor
            .write_encrypted_blobs(outside_dir.path(), &old_dek, &old_export_id)
            .unwrap();
        symlink(
            outside_dir.path().join("blobs"),
            source_archive_dir.path().join("blobs"),
        )
        .unwrap();

        let err = reencrypt_blobs_into_dir(
            source_archive_dir.path(),
            output_archive_dir.path(),
            &old_dek,
            &old_export_id,
            &new_dek,
            &new_export_id,
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("symlink"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_reencrypt_existing_blobs_rejects_symlinked_destination_directory() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let config = AttachmentConfig::enabled();
        let mut processor = AttachmentProcessor::new(config);
        let attachment = AttachmentData {
            filename: "test.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: b"test content".to_vec(),
        };
        processor.process_attachments(1, &[attachment]).unwrap();

        let source_archive_dir = TempDir::new().unwrap();
        let output_archive_dir = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let old_dek = [0x42u8; 32];
        let old_export_id = [0x01u8; 16];
        let new_dek = [0x24u8; 32];
        let new_export_id = [0x02u8; 16];

        processor
            .write_encrypted_blobs(source_archive_dir.path(), &old_dek, &old_export_id)
            .unwrap();
        fs::create_dir_all(outside_dir.path().join("elsewhere")).unwrap();
        symlink(
            outside_dir.path().join("elsewhere"),
            output_archive_dir.path().join("blobs"),
        )
        .unwrap();

        let err = reencrypt_blobs_into_dir(
            source_archive_dir.path(),
            output_archive_dir.path(),
            &old_dek,
            &old_export_id,
            &new_dek,
            &new_export_id,
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("symlink"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_reencrypt_existing_blobs_rejects_symlinked_destination_archive_dir() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let config = AttachmentConfig::enabled();
        let mut processor = AttachmentProcessor::new(config);
        let attachment = AttachmentData {
            filename: "test.txt".to_string(),
            mime_type: "text/plain".to_string(),
            data: b"test content".to_vec(),
        };
        processor.process_attachments(1, &[attachment]).unwrap();

        let source_archive_dir = TempDir::new().unwrap();
        let link_parent = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let output_archive_link = link_parent.path().join("archive-link");
        let old_dek = [0x42u8; 32];
        let old_export_id = [0x01u8; 16];
        let new_dek = [0x24u8; 32];
        let new_export_id = [0x02u8; 16];

        processor
            .write_encrypted_blobs(source_archive_dir.path(), &old_dek, &old_export_id)
            .unwrap();
        symlink(outside_dir.path(), &output_archive_link).unwrap();

        let err = reencrypt_blobs_into_dir(
            source_archive_dir.path(),
            &output_archive_link,
            &old_dek,
            &old_export_id,
            &new_dek,
            &new_export_id,
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("symlink"),
            "unexpected error: {err:#}"
        );
        assert!(
            !outside_dir.path().join("blobs/manifest.enc").exists(),
            "key rotation must not write attachments through a symlinked archive directory"
        );
    }
}
