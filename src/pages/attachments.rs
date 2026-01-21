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
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
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
        self.allowed_mime_types
            .iter()
            .any(|allowed| mime_type.starts_with(allowed.as_str()))
    }
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
        fs::create_dir_all(&blobs_dir).context("Failed to create blobs directory")?;

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
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: data.as_slice(),
                        aad: &aad,
                    },
                )
                .map_err(|e| anyhow::anyhow!("Blob encryption failed: {}", e))?;

            // Write to file
            let mut file = BufWriter::new(
                File::create(&blob_path).context("Failed to create blob file")?,
            );
            file.write_all(&ciphertext)?;
            file.flush()?;

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
                Nonce::from_slice(&manifest_nonce),
                Payload {
                    msg: &manifest_json,
                    aad: export_id,
                },
            )
            .map_err(|e| anyhow::anyhow!("Manifest encryption failed: {}", e))?;

        let manifest_path = blobs_dir.join("manifest.enc");
        fs::write(&manifest_path, manifest_ciphertext)
            .context("Failed to write encrypted manifest")?;

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
    use hkdf::Hkdf;
    use sha2::Sha256;

    let hkdf = Hkdf::<Sha256>::new(Some(BLOB_NONCE_DOMAIN), identifier.as_bytes());
    let mut nonce = [0u8; 12];
    hkdf.expand(b"nonce", &mut nonce)
        .expect("HKDF expansion should never fail for 12 bytes");
    nonce
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
            Nonce::from_slice(&nonce),
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
            Nonce::from_slice(&nonce),
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
        assert!(config.is_mime_allowed("image/jpeg"));
        assert!(config.is_mime_allowed("application/pdf"));
        assert!(config.is_mime_allowed("text/plain"));
        assert!(!config.is_mime_allowed("application/octet-stream"));
        assert!(!config.is_mime_allowed("video/mp4"));
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
                Nonce::from_slice(&nonce),
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
                Nonce::from_slice(&nonce),
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
}
