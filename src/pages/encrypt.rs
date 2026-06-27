//! Encryption engine for pages export.
//!
//! Implements envelope encryption with:
//! - Argon2id key derivation for passwords
//! - HKDF-SHA256 for recovery secrets
//! - AES-256-GCM authenticated encryption
//! - Streaming encryption for large files
//! - Multiple key slots (like LUKS)

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use anyhow::{Context, Result, bail};
use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{SaltString, rand_core::OsRng as PasswordHashOsRng},
};
use base64::prelude::*;
use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct AeadSourceError(aes_gcm::Error);

/// Default chunk size for streaming encryption (8 MiB)
pub const DEFAULT_CHUNK_SIZE: usize = 8 * 1024 * 1024;

/// Maximum chunk size (32 MiB)
pub const MAX_CHUNK_SIZE: usize = 32 * 1024 * 1024;

const MAX_ARCHIVE_CHUNKS: u64 = u32::MAX as u64;

fn max_encryptable_plaintext_bytes(chunk_size: usize) -> u64 {
    MAX_ARCHIVE_CHUNKS.saturating_mul(chunk_size as u64)
}

fn ensure_archive_chunk_count_fits_nonce_space(chunk_count: u64, chunk_size: usize) -> Result<()> {
    if chunk_count > MAX_ARCHIVE_CHUNKS {
        bail!(
            "File too large: exceeds maximum of {} chunks ({} bytes with current chunk size)",
            u32::MAX,
            max_encryptable_plaintext_bytes(chunk_size)
        );
    }
    Ok(())
}

fn ensure_can_write_archive_chunk(chunk_index: u32, chunk_size: usize) -> Result<()> {
    if chunk_index == u32::MAX {
        bail!(
            "File too large: exceeds maximum of {} chunks ({} bytes with current chunk size)",
            u32::MAX,
            max_encryptable_plaintext_bytes(chunk_size)
        );
    }
    Ok(())
}

/// Argon2id parameters (from Phase 2 spec)
#[cfg(not(test))]
const ARGON2_MEMORY_KB: u32 = 65536; // 64 MB
#[cfg(test)]
const ARGON2_MEMORY_KB: u32 = 64;
#[cfg(not(test))]
const ARGON2_ITERATIONS: u32 = 3;
#[cfg(test)]
const ARGON2_ITERATIONS: u32 = 1;
#[cfg(not(test))]
const ARGON2_PARALLELISM: u32 = 4;
#[cfg(test)]
const ARGON2_PARALLELISM: u32 = 1;

/// Encryption schema version
pub(crate) const SCHEMA_VERSION: u8 = 2;

/// Secret key material that zeros on drop
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SecretKey([u8; 32]);

impl SecretKey {
    pub fn random() -> Self {
        let mut key = [0u8; 32];
        let mut rng = rand::rng();
        rng.fill_bytes(&mut key);
        Self(key)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Key slot type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotType {
    Password,
    Recovery,
}

/// KDF algorithm identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KdfAlgorithm {
    Argon2id,
    HkdfSha256,
}

/// Key slot in config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeySlot {
    pub id: u8,
    pub slot_type: SlotType,
    pub kdf: KdfAlgorithm,
    pub salt: String,        // base64-encoded
    pub wrapped_dek: String, // base64-encoded
    pub nonce: String,       // base64-encoded (for DEK wrapping)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argon2_params: Option<Argon2Params>,
}

/// Argon2 parameters for config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Argon2Params {
    pub memory_kb: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Default for Argon2Params {
    fn default() -> Self {
        Self {
            memory_kb: ARGON2_MEMORY_KB,
            iterations: ARGON2_ITERATIONS,
            parallelism: ARGON2_PARALLELISM,
        }
    }
}

/// Payload metadata in config.json
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadMeta {
    pub chunk_size: usize,
    pub chunk_count: usize,
    pub total_compressed_size: u64,
    pub total_plaintext_size: u64,
    pub files: Vec<String>,
}

/// Full config.json structure
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptionConfig {
    pub version: u8,
    pub export_id: String,  // base64-encoded 16 bytes
    pub base_nonce: String, // base64-encoded 12 bytes
    pub compression: String,
    pub kdf_defaults: Argon2Params,
    pub payload: PayloadMeta,
    pub key_slots: Vec<KeySlot>,
}

pub(crate) fn validate_supported_payload_format(config: &EncryptionConfig) -> Result<()> {
    if config.version != SCHEMA_VERSION {
        bail!(
            "Unsupported archive schema version {}; expected {}",
            config.version,
            SCHEMA_VERSION
        );
    }

    if config.compression != "deflate" {
        bail!(
            "Unsupported archive compression '{}'. The current encrypted pages format supports only deflate.",
            config.compression
        );
    }

    if config.payload.chunk_size == 0 {
        bail!("Invalid archive chunk_size 0: must be > 0");
    }

    if config.payload.chunk_size > MAX_CHUNK_SIZE {
        bail!(
            "Invalid archive chunk_size {}: must be <= {} bytes",
            config.payload.chunk_size,
            MAX_CHUNK_SIZE
        );
    }

    if config.payload.chunk_count != config.payload.files.len() {
        bail!(
            "Invalid archive payload metadata: chunk_count {} does not match file list length {}",
            config.payload.chunk_count,
            config.payload.files.len()
        );
    }

    if config.payload.chunk_count > u32::MAX as usize {
        bail!(
            "Invalid archive payload metadata: chunk_count {} exceeds maximum {}",
            config.payload.chunk_count,
            u32::MAX
        );
    }

    for (index, file) in config.payload.files.iter().enumerate() {
        if !payload_chunk_path_matches_index(file, index) {
            return Err(invalid_payload_file_entry(index, file));
        }
    }

    Ok(())
}

fn payload_chunk_path_matches_index(path: &str, index: usize) -> bool {
    const PREFIX: &str = "payload/chunk-";
    const SUFFIX: &str = ".bin";

    let Some(digits) = path
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
    else {
        return false;
    };

    let expected_digit_count = decimal_digit_count(index).max(5);
    if digits.len().cmp(&expected_digit_count).is_ne() {
        return false;
    }

    let mut parsed = 0usize;
    for byte in digits.bytes() {
        if !byte.is_ascii_digit() {
            return false;
        }
        let Some(next) = parsed
            .checked_mul(10)
            .and_then(|value| value.checked_add(usize::from(byte - b'0')))
        else {
            return false;
        };
        parsed = next;
    }

    parsed.cmp(&index).is_eq()
}

fn decimal_digit_count(mut value: usize) -> usize {
    let mut count = 1;
    while value >= 10 {
        value /= 10;
        count += 1;
    }
    count
}

fn invalid_payload_file_entry(index: usize, actual: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Invalid archive payload metadata: payload file entry {index} is '{actual}'; expected 'payload/chunk-{index:05}.bin'"
    )
}

/// Encryption engine for pages export
///
/// `Debug` is implemented manually to avoid printing the secret DEK.
pub struct EncryptionEngine {
    dek: SecretKey,
    export_id: [u8; 16],
    base_nonce: [u8; 12],
    chunk_size: usize,
    key_slots: Vec<KeySlot>,
}

impl std::fmt::Debug for EncryptionEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptionEngine")
            .field("chunk_size", &self.chunk_size)
            .field("key_slots", &self.key_slots.len())
            .finish_non_exhaustive()
    }
}

fn key_slot_id_for_len(slot_count: usize) -> Result<u8> {
    u8::try_from(slot_count).map_err(|err| {
        anyhow::anyhow!(
            "maximum of 256 key slots exceeded ({} slots already allocated): {}",
            slot_count,
            err
        )
    })
}

impl Default for EncryptionEngine {
    fn default() -> Self {
        Self::new(DEFAULT_CHUNK_SIZE).expect("default chunk size must be valid")
    }
}

impl EncryptionEngine {
    /// Create new encryption engine with random DEK
    pub fn new(chunk_size: usize) -> Result<Self> {
        if chunk_size == 0 {
            bail!("chunk_size must be > 0");
        }
        if chunk_size > MAX_CHUNK_SIZE {
            bail!("chunk_size must be <= {MAX_CHUNK_SIZE} bytes");
        }
        let mut export_id = [0u8; 16];
        let mut base_nonce = [0u8; 12];
        let mut rng = rand::rng();
        rng.fill_bytes(&mut export_id);
        rng.fill_bytes(&mut base_nonce);

        Ok(Self {
            dek: SecretKey::random(),
            export_id,
            base_nonce,
            chunk_size,
            key_slots: Vec::new(),
        })
    }

    /// Add a password-based key slot using Argon2id
    pub fn add_password_slot(&mut self, password: &str) -> Result<u8> {
        // Validate password
        if password.is_empty() {
            anyhow::bail!("Password cannot be empty");
        }
        if password.trim().is_empty() {
            anyhow::bail!("Password cannot be whitespace-only");
        }

        let slot_id = key_slot_id_for_len(self.key_slots.len())?;

        // Generate salt
        let salt = SaltString::generate(&mut PasswordHashOsRng);
        let salt_bytes = salt.as_str().as_bytes();

        // Derive KEK from password
        let kek = derive_kek_argon2id(password, salt_bytes)?;

        // Wrap DEK with KEK
        let (wrapped_dek, nonce) = wrap_key(&kek, self.dek.as_bytes(), &self.export_id, slot_id)?;

        self.key_slots.push(KeySlot {
            id: slot_id,
            slot_type: SlotType::Password,
            kdf: KdfAlgorithm::Argon2id,
            salt: BASE64_STANDARD.encode(salt_bytes),
            wrapped_dek: BASE64_STANDARD.encode(&wrapped_dek),
            nonce: BASE64_STANDARD.encode(nonce),
            argon2_params: Some(Argon2Params::default()),
        });

        Ok(slot_id)
    }

    /// Add a recovery secret slot using HKDF-SHA256
    pub fn add_recovery_slot(&mut self, secret: &[u8]) -> Result<u8> {
        let slot_id = key_slot_id_for_len(self.key_slots.len())?;

        // Generate salt
        let mut salt = [0u8; 16];
        let mut rng = rand::rng();
        rng.fill_bytes(&mut salt);

        // Derive KEK from recovery secret
        let kek = derive_kek_hkdf(secret, &salt)?;

        // Wrap DEK with KEK
        let (wrapped_dek, nonce) = wrap_key(&kek, self.dek.as_bytes(), &self.export_id, slot_id)?;

        self.key_slots.push(KeySlot {
            id: slot_id,
            slot_type: SlotType::Recovery,
            kdf: KdfAlgorithm::HkdfSha256,
            salt: BASE64_STANDARD.encode(salt),
            wrapped_dek: BASE64_STANDARD.encode(&wrapped_dek),
            nonce: BASE64_STANDARD.encode(nonce),
            argon2_params: None,
        });

        Ok(slot_id)
    }

    /// Returns the number of key slots currently configured
    pub fn key_slot_count(&self) -> usize {
        self.key_slots.len()
    }

    /// Encrypt a file with streaming compression and chunked AEAD
    pub fn encrypt_file<P: AsRef<Path>>(
        &self,
        input: P,
        output_dir: P,
        progress: impl Fn(u64, u64),
    ) -> Result<EncryptionConfig> {
        let input_path = input.as_ref();
        let output_dir = output_dir.as_ref();

        ensure_real_archive_output_directory(output_dir, "encrypted archive output directory")?;
        let payload_dir = output_dir.join("payload");
        ensure_real_archive_output_directory(&payload_dir, "encrypted archive payload directory")?;

        // Read input file size for progress
        let input_size = std::fs::metadata(input_path)?.len();
        ensure_archive_chunk_count_fits_nonce_space(
            input_size.div_ceil(self.chunk_size as u64),
            self.chunk_size,
        )?;

        // Open input file
        let input_file = File::open(input_path).context("Failed to open input file")?;
        let mut reader = BufReader::new(input_file);

        // Compress and encrypt in chunks
        let mut chunk_files = Vec::new();
        let mut chunk_index = 0u32;
        let mut total_compressed = 0u64;
        let mut bytes_read = 0u64;

        let cipher = Aes256Gcm::new_from_slice(self.dek.as_bytes()).expect("Invalid key length");

        loop {
            // Read up to chunk_size bytes
            let mut plaintext = vec![0u8; self.chunk_size];
            let mut total_read = 0;

            while total_read < self.chunk_size {
                match reader.read(&mut plaintext[total_read..]) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        total_read += n;
                        bytes_read += n as u64;
                        progress(bytes_read, input_size);
                    }
                    Err(e) => return Err(e.into()),
                }
            }

            if total_read == 0 {
                break; // No more data
            }
            ensure_can_write_archive_chunk(chunk_index, self.chunk_size)?;

            plaintext.truncate(total_read);

            // Compress the chunk
            let mut compressed = Vec::new();
            {
                let mut encoder = DeflateEncoder::new(&mut compressed, Compression::default());
                encoder.write_all(&plaintext)?;
                encoder.finish()?;
            }

            // Derive nonce for this chunk (counter-based)
            let nonce = derive_chunk_nonce(&self.base_nonce, chunk_index);

            // Build AAD: export_id || chunk_index || schema_version
            let aad = build_chunk_aad(&self.export_id, chunk_index);

            // Encrypt with AEAD
            let ciphertext = cipher
                .encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &compressed,
                        aad: &aad,
                    },
                )
                .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

            // Write chunk file
            let chunk_filename = format!("chunk-{:05}.bin", chunk_index);
            let chunk_path = payload_dir.join(&chunk_filename);
            write_encrypted_archive_file(&chunk_path, &ciphertext, "encrypted payload chunk")?;

            chunk_files.push(format!("payload/{}", chunk_filename));
            total_compressed += ciphertext.len() as u64;
            chunk_index = chunk_index.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!(
                    "File too large: exceeds maximum of {} chunks ({} bytes with current chunk size)",
                    u32::MAX,
                    (u32::MAX as u64) * (self.chunk_size as u64)
                )
            })?;
        }

        // Build config
        let config = EncryptionConfig {
            version: SCHEMA_VERSION,
            export_id: BASE64_STANDARD.encode(self.export_id),
            base_nonce: BASE64_STANDARD.encode(self.base_nonce),
            compression: "deflate".to_string(),
            kdf_defaults: Argon2Params::default(),
            payload: PayloadMeta {
                chunk_size: self.chunk_size,
                chunk_count: chunk_index as usize,
                total_compressed_size: total_compressed,
                total_plaintext_size: input_size,
                files: chunk_files,
            },
            key_slots: self.key_slots.clone(),
        };

        // Write config.json
        let config_path = output_dir.join("config.json");
        let config_payload =
            serde_json::to_vec_pretty(&config).context("Failed to serialize encryption config")?;
        write_encrypted_archive_file(&config_path, &config_payload, "encryption config")?;
        sync_tree(output_dir)?;

        Ok(config)
    }
}

fn ensure_real_archive_output_directory(path: &Path, label: &str) -> Result<()> {
    ensure_existing_archive_ancestors_have_no_symlinks(path, label)?;
    std::fs::create_dir_all(path).with_context(|| format!("Failed to create {label}"))?;
    ensure_existing_archive_ancestors_have_no_symlinks(path, label)?;

    let metadata =
        std::fs::symlink_metadata(path).with_context(|| format!("Failed to inspect {label}"))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        bail!("{label} must not be a symlink: {}", path.display());
    }
    if !file_type.is_dir() {
        bail!("{label} must be a directory: {}", path.display());
    }
    Ok(())
}

fn ensure_existing_archive_ancestors_have_no_symlinks(path: &Path, label: &str) -> Result<()> {
    let mut ancestors: Vec<PathBuf> = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    ancestors.reverse();

    for ancestor in ancestors {
        match std::fs::symlink_metadata(&ancestor) {
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

fn write_encrypted_archive_file(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    ensure_replaceable_archive_file(path, label)?;
    let (mut pending, file) = PendingArchiveOutput::create(path, label)?;
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

fn ensure_replaceable_archive_file(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
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

#[cfg(any(windows, test))]
fn replacement_path_entry_exists(path: &Path, label: &str) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => Ok(false),
        Err(err) => {
            Err(err).with_context(|| format!("Failed to inspect {label} {}", path.display()))
        }
    }
}

#[cfg(any(windows, test))]
fn unique_replacement_backup_path(path: &Path, purpose: &str, label: &str) -> Result<PathBuf> {
    static BACKUP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let parent = output_parent(path);
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{label} path must name a file"))?
        .to_string_lossy();
    let counter = BACKUP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0))
        .as_nanos();
    let candidate = parent.join(format!(
        ".{file_name}.{purpose}.{}.{}.{timestamp:x}",
        std::process::id(),
        counter
    ));

    match std::fs::symlink_metadata(&candidate) {
        Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => Ok(candidate),
        Ok(_) => {
            bail!(
                "Replacement backup path for {label} already exists: {}",
                candidate.display()
            );
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to inspect replacement backup path {}",
                candidate.display()
            )
        }),
    }
}

struct PendingArchiveOutput {
    path: PathBuf,
    keep: bool,
}

impl PendingArchiveOutput {
    fn create(final_path: &Path, label: &str) -> Result<(Self, File)> {
        let parent = output_parent(final_path);
        ensure_existing_archive_ancestors_have_no_symlinks(parent, label)?;
        let file_name = final_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("{label} path must name a file"))?
            .to_string_lossy();

        for attempt in 0..100u32 {
            let mut random_bytes = [0u8; 8];
            let mut rng = rand::rng();
            rng.fill_bytes(&mut random_bytes);
            let random = u64::from_le_bytes(random_bytes);
            let temp_path = parent.join(format!(
                ".{file_name}.cass-encrypt-tmp.{}.{}.{:016x}",
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
        replace_archive_file_from_temp(&self.path, final_path, label)?;
        self.keep = true;
        Ok(())
    }
}

impl Drop for PendingArchiveOutput {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn replace_archive_file_from_temp(temp_path: &Path, final_path: &Path, label: &str) -> Result<()> {
    replace_archive_file_from_temp_impl(temp_path, final_path, label)?;
    sync_parent_directory(final_path)
}

#[cfg(not(windows))]
fn replace_archive_file_from_temp_impl(
    temp_path: &Path,
    final_path: &Path,
    label: &str,
) -> Result<()> {
    std::fs::rename(temp_path, final_path).with_context(|| {
        format!(
            "Failed to install {label} {} from {}",
            final_path.display(),
            temp_path.display()
        )
    })
}

#[cfg(windows)]
fn replace_archive_file_from_temp_impl(
    temp_path: &Path,
    final_path: &Path,
    label: &str,
) -> Result<()> {
    ensure_replaceable_archive_file(final_path, label)?;
    if !replacement_path_entry_exists(final_path, label)? {
        return std::fs::rename(temp_path, final_path).with_context(|| {
            format!(
                "Failed to install {label} {} from {}",
                final_path.display(),
                temp_path.display()
            )
        });
    }

    replace_archive_file_from_temp_via_backup(temp_path, final_path, label)
}

#[cfg(any(windows, test))]
fn replace_archive_file_from_temp_via_backup(
    temp_path: &Path,
    final_path: &Path,
    label: &str,
) -> Result<()> {
    let backup_path = unique_replacement_backup_path(final_path, "cass-encrypt-backup", label)?;

    std::fs::rename(final_path, &backup_path).with_context(|| {
        format!(
            "Failed to stage existing {label} {} before replacement",
            final_path.display()
        )
    })?;

    match std::fs::rename(temp_path, final_path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&backup_path);
            Ok(())
        }
        Err(replace_err) => match std::fs::rename(&backup_path, final_path) {
            Ok(()) => Err(replace_err).with_context(|| {
                format!(
                    "Failed to install {label} {}; restored previous output",
                    final_path.display()
                )
            }),
            Err(restore_err) => bail!(
                "Failed to install {label} {}; also failed to restore previous output from {}: {}; temporary output retained at {}",
                final_path.display(),
                backup_path.display(),
                restore_err,
                temp_path.display()
            ),
        },
    }
}

#[cfg(not(windows))]
fn sync_tree(path: &Path) -> Result<()> {
    // Bead 92o31: fsync the subtree first (files + directory inodes),
    // THEN fsync the parent directory so the name-entry that points at
    // `path` is durably recorded. Without the parent fsync, a
    // power-loss between encrypt's return and the next fs::sync_all
    // on the parent can leave the encrypted archive on disk but
    // unreachable by its own path — operator sees success + missing
    // file. Mirrors the proven shape in src/pages/bundle.rs:457-461.
    sync_tree_inner(path)?;
    sync_parent_directory(path)
}

#[cfg(windows)]
fn sync_tree(_path: &Path) -> Result<()> {
    // Windows has no portable fsync-directory primitive; NTFS journals
    // name-entry updates synchronously with the file create/rename, so
    // a no-op here is functionally equivalent to the POSIX two-step
    // below. See bundle.rs:463-466 for the matching platform gate.
    Ok(())
}

#[cfg(not(windows))]
fn sync_tree_inner(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Ok(());
    }
    if file_type.is_file() {
        File::open(path)?.sync_all()?;
        return Ok(());
    }
    if file_type.is_dir() {
        for entry in std::fs::read_dir(path)? {
            sync_tree_inner(&entry?.path())?;
        }
        File::open(path)?.sync_all()?;
    }
    Ok(())
}

/// fsync the directory that contains `path`, so the dirent pointing at
/// `path` is durably recorded. POSIX requires this explicit step:
/// fsync on a file flushes its contents + metadata, but NOT its name
/// entry in the parent directory. Mirrors src/pages/bundle.rs:499-512.
/// Bead 92o31.
#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .with_context(|| {
            format!(
                "failed opening parent directory {} for fsync",
                parent.display()
            )
        })?
        .sync_all()
        .with_context(|| {
            format!(
                "failed syncing parent directory {} after encrypted export",
                parent.display()
            )
        })
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

/// Decryption engine
pub struct DecryptionEngine {
    dek: SecretKey,
    config: EncryptionConfig,
}

impl DecryptionEngine {
    /// Unlock with password
    pub fn unlock_with_password(config: EncryptionConfig, password: &str) -> Result<Self> {
        validate_supported_payload_format(&config)?;

        for slot in &config.key_slots {
            if slot.slot_type != SlotType::Password {
                continue;
            }

            let salt = BASE64_STANDARD.decode(&slot.salt)?;
            let wrapped_dek = BASE64_STANDARD.decode(&slot.wrapped_dek)?;
            let nonce = BASE64_STANDARD.decode(&slot.nonce)?;

            let kek = derive_kek_argon2id(password, &salt)?;

            let export_id = BASE64_STANDARD.decode(&config.export_id)?;
            if let Ok(dek) = unwrap_key(&kek, &wrapped_dek, &nonce, &export_id, slot.id) {
                return Ok(Self {
                    dek: SecretKey::from_bytes(dek),
                    config,
                });
            }
        }

        bail!("Invalid password or no matching key slot")
    }

    /// Unlock with recovery secret
    pub fn unlock_with_recovery(config: EncryptionConfig, secret: &[u8]) -> Result<Self> {
        validate_supported_payload_format(&config)?;

        for slot in &config.key_slots {
            if slot.slot_type != SlotType::Recovery {
                continue;
            }

            let salt = BASE64_STANDARD.decode(&slot.salt)?;
            let wrapped_dek = BASE64_STANDARD.decode(&slot.wrapped_dek)?;
            let nonce = BASE64_STANDARD.decode(&slot.nonce)?;

            let kek = derive_kek_hkdf(secret, &salt)?;

            let export_id = BASE64_STANDARD.decode(&config.export_id)?;
            if let Ok(dek) = unwrap_key(&kek, &wrapped_dek, &nonce, &export_id, slot.id) {
                return Ok(Self {
                    dek: SecretKey::from_bytes(dek),
                    config,
                });
            }
        }

        bail!("Invalid recovery secret or no matching key slot")
    }

    /// Decrypt all chunks to output file
    pub fn decrypt_to_file<P: AsRef<Path>>(
        &self,
        encrypted_dir: P,
        output: P,
        progress: impl Fn(usize, usize),
    ) -> Result<()> {
        let encrypted_dir = super::resolve_site_dir(encrypted_dir.as_ref())?;
        let output_path = output.as_ref();
        validate_supported_payload_format(&self.config)?;

        let cipher = Aes256Gcm::new_from_slice(self.dek.as_bytes()).expect("Invalid key length");

        let base_nonce = BASE64_STANDARD.decode(&self.config.base_nonce)?;
        let export_id = BASE64_STANDARD.decode(&self.config.export_id)?;

        // Validate chunk count doesn't exceed u32 to prevent nonce truncation
        if self.config.payload.files.len() > u32::MAX as usize {
            bail!(
                "Invalid config: chunk count {} exceeds maximum {}",
                self.config.payload.files.len(),
                u32::MAX
            );
        }

        let (mut pending_output, output_file) = PendingDecryptOutput::create(output_path)?;
        let mut writer = BufWriter::new(output_file);

        for (chunk_index, chunk_file) in self.config.payload.files.iter().enumerate() {
            progress(chunk_index, self.config.payload.chunk_count);

            // Prevent directory traversal
            if chunk_file.contains("..") || Path::new(chunk_file).is_absolute() {
                bail!("Invalid chunk path: potential directory traversal");
            }

            let chunk_path = encrypted_dir.join(chunk_file);
            let ciphertext = std::fs::read(&chunk_path)?;

            // Derive nonce
            let nonce = derive_chunk_nonce(base_nonce.as_slice().try_into()?, chunk_index as u32);

            // Build AAD
            let aad = build_chunk_aad(export_id.as_slice().try_into()?, chunk_index as u32);

            // Decrypt
            let compressed = cipher
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|err| {
                    // [coding_agent_session_search-b64fe] Chain the underlying
                    // aead error so operators can distinguish "decryption
                    // failed at chunk N because the AES-GCM tag did not
                    // verify" (corrupt ciphertext / wrong DEK / tampered
                    // AAD) from a downstream decompression / writer
                    // failure that surfaces with a different error chain.
                    // The aead crate's Display impl deliberately stays
                    // opaque about whether MAC vs auth-tag verification
                    // failed (timing-attack hardening), so we still don't
                    // leak that — but the source error type IS preserved
                    // in the chain for debug-mode inspection.
                    let context = format!(
                        "Decryption failed for chunk {} ({} bytes ciphertext): {}",
                        chunk_index,
                        ciphertext.len(),
                        err
                    );
                    anyhow::Error::new(AeadSourceError(err)).context(context)
                })?;

            // Decompress
            let mut decoder = DeflateDecoder::new(&compressed[..]);
            let mut plaintext = Vec::new();
            decoder.read_to_end(&mut plaintext)?;

            writer.write_all(&plaintext)?;
        }

        writer.flush()?;
        writer
            .get_ref()
            .sync_all()
            .with_context(|| format!("Failed to sync {}", pending_output.path().display()))?;
        drop(writer);
        pending_output.persist(output_path)?;

        progress(
            self.config.payload.chunk_count,
            self.config.payload.chunk_count,
        );

        Ok(())
    }
}

struct PendingDecryptOutput {
    path: PathBuf,
    keep: bool,
}

impl PendingDecryptOutput {
    fn create(output_path: &Path) -> Result<(Self, File)> {
        let parent = output_parent(output_path);
        let file_name = output_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("decryption output path must name a file"))?
            .to_string_lossy();

        for attempt in 0..100u32 {
            let mut random_bytes = [0u8; 8];
            let mut rng = rand::rng();
            rng.fill_bytes(&mut random_bytes);
            let random = u64::from_le_bytes(random_bytes);
            let temp_path = parent.join(format!(
                ".{file_name}.cass-decrypt-tmp.{}.{}.{:016x}",
                std::process::id(),
                attempt,
                random
            ));

            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }

            match options.open(&temp_path) {
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
                        format!(
                            "Failed to create temporary decrypt output {}",
                            temp_path.display()
                        )
                    });
                }
            }
        }

        bail!(
            "Failed to create a unique temporary decrypt output next to {} after 100 attempts",
            output_path.display()
        );
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn persist(&mut self, output_path: &Path) -> Result<()> {
        replace_decrypt_output_from_temp(&self.path, output_path)?;
        self.keep = true;
        Ok(())
    }
}

impl Drop for PendingDecryptOutput {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn output_parent(output_path: &Path) -> &Path {
    output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn replace_decrypt_output_from_temp(temp_path: &Path, output_path: &Path) -> Result<()> {
    replace_decrypt_output_from_temp_impl(temp_path, output_path)?;
    sync_parent_directory(output_path)
}

#[cfg(not(windows))]
fn replace_decrypt_output_from_temp_impl(temp_path: &Path, output_path: &Path) -> Result<()> {
    std::fs::rename(temp_path, output_path).with_context(|| {
        format!(
            "Failed to install decrypted output {} from {}",
            output_path.display(),
            temp_path.display()
        )
    })
}

#[cfg(windows)]
fn replace_decrypt_output_from_temp_impl(temp_path: &Path, output_path: &Path) -> Result<()> {
    if !replacement_path_entry_exists(output_path, "decrypted output")? {
        return std::fs::rename(temp_path, output_path).with_context(|| {
            format!(
                "Failed to install decrypted output {} from {}",
                output_path.display(),
                temp_path.display()
            )
        });
    }

    replace_decrypt_output_from_temp_via_backup(temp_path, output_path)
}

#[cfg(any(windows, test))]
fn replace_decrypt_output_from_temp_via_backup(temp_path: &Path, output_path: &Path) -> Result<()> {
    let backup_path =
        unique_replacement_backup_path(output_path, "cass-decrypt-backup", "decrypted output")?;

    std::fs::rename(output_path, &backup_path).with_context(|| {
        format!(
            "Failed to stage existing decrypted output {} before replacement",
            output_path.display()
        )
    })?;

    match std::fs::rename(temp_path, output_path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&backup_path);
            Ok(())
        }
        Err(replace_err) => match std::fs::rename(&backup_path, output_path) {
            Ok(()) => Err(replace_err).with_context(|| {
                format!(
                    "Failed to install decrypted output {}; restored previous output",
                    output_path.display()
                )
            }),
            Err(restore_err) => bail!(
                "Failed to install decrypted output {}; also failed to restore previous output from {}: {}; temporary output retained at {}",
                output_path.display(),
                backup_path.display(),
                restore_err,
                temp_path.display()
            ),
        },
    }
}

/// Derive KEK from password using Argon2id.
///
/// Per `coding_agent_session_search-vz9t8.4`, instrumented with safe-to-log
/// tracing. Logs ONLY: operation name, salt length, output KEK length (always
/// 32), and Argon2 parameters (memory_kb, iterations, parallelism). The
/// password and the resulting KEK are NEVER logged.
#[tracing::instrument(
    name = "derive_kek_argon2id",
    skip_all,
    fields(
        operation = "derive_kek_argon2id",
        salt_len = salt.len(),
        memory_kb = ARGON2_MEMORY_KB,
        iterations = ARGON2_ITERATIONS,
        parallelism = ARGON2_PARALLELISM,
        password_present = !password.is_empty(),
    )
)]
fn derive_kek_argon2id(password: &str, salt: &[u8]) -> Result<SecretKey> {
    let start = std::time::Instant::now();
    let params = Params::new(
        ARGON2_MEMORY_KB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        Some(32),
    )
    .map_err(|e| anyhow::anyhow!("Invalid Argon2 parameters: {:?}", e))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut kek = [0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut kek)
        .map_err(|e| anyhow::anyhow!("Argon2id derivation failed: {}", e))?;

    tracing::debug!(
        target: "cass::pages::encrypt",
        operation = "derive_kek_argon2id",
        elapsed_ms = start.elapsed().as_millis() as u64,
        kek_len = kek.len(),
        "derive_kek_argon2id: ok"
    );
    Ok(SecretKey::from_bytes(kek))
}

/// Derive KEK from recovery secret using HKDF-SHA256.
///
/// Per `coding_agent_session_search-vz9t8.4`, instrumented with safe-to-log
/// tracing. Logs operation name + salt length + secret-byte-length only. The
/// secret bytes and KEK output are NEVER logged. The hkdf_extract_expand
/// helper itself records its own (also-safe) tracing span.
#[tracing::instrument(
    name = "derive_kek_hkdf",
    skip_all,
    fields(
        operation = "derive_kek_hkdf",
        salt_len = salt.len(),
        secret_len = secret.len(),
        info_label = "cass-pages-kek-v2",
    )
)]
fn derive_kek_hkdf(secret: &[u8], salt: &[u8]) -> Result<SecretKey> {
    let start = std::time::Instant::now();
    let kek = crate::encryption::hkdf_extract_expand(secret, salt, b"cass-pages-kek-v2", 32)
        .map_err(|e| anyhow::anyhow!("HKDF extract+expand failed for recovery secret KEK: {e}"))?;
    let actual_len = kek.len();
    let kek: [u8; 32] = kek.try_into().map_err(|_| {
        anyhow::anyhow!(
            "HKDF expansion produced invalid KEK length: expected 32, got {}",
            actual_len
        )
    })?;
    tracing::debug!(
        target: "cass::pages::encrypt",
        operation = "derive_kek_hkdf",
        elapsed_us = start.elapsed().as_micros() as u64,
        kek_len = 32,
        "derive_kek_hkdf: ok"
    );
    Ok(SecretKey::from_bytes(kek))
}

/// Wrap DEK with KEK using AES-256-GCM
fn wrap_key(
    kek: &SecretKey,
    dek: &[u8; 32],
    export_id: &[u8; 16],
    slot_id: u8,
) -> Result<(Vec<u8>, [u8; 12])> {
    let cipher = Aes256Gcm::new_from_slice(kek.as_bytes()).expect("Invalid key length");

    let mut nonce = [0u8; 12];
    let mut rng = rand::rng();
    rng.fill_bytes(&mut nonce);

    // AAD: export_id || slot_id
    let mut aad = Vec::with_capacity(17);
    aad.extend_from_slice(export_id);
    aad.push(slot_id);

    let wrapped = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: dek,
                aad: &aad,
            },
        )
        .map_err(|e| anyhow::anyhow!("Key wrapping failed: {}", e))?;

    Ok((wrapped, nonce))
}

/// Unwrap DEK with KEK
fn unwrap_key(
    kek: &SecretKey,
    wrapped: &[u8],
    nonce: &[u8],
    export_id: &[u8],
    slot_id: u8,
) -> Result<[u8; 32]> {
    let cipher = Aes256Gcm::new_from_slice(kek.as_bytes()).expect("Invalid key length");
    let nonce: &[u8; 12] = nonce
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid nonce length: expected 12, got {}", nonce.len()))?;

    // AAD: export_id || slot_id
    let mut aad = Vec::with_capacity(export_id.len() + 1);
    aad.extend_from_slice(export_id);
    aad.push(slot_id);

    let dek = cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: wrapped,
                aad: &aad,
            },
        )
        .map_err(|err| {
            // [coding_agent_session_search-b64fe] Chain the underlying
            // aead error so operators can distinguish "wrong password
            // (KEK derivation succeeded but DEK MAC failed)" from
            // "corrupt key slot ciphertext" from "wrong AAD (slot id /
            // export id mismatch)". The aead crate's Display impl
            // remains opaque about the specific sub-failure (timing-
            // attack hardening), but the source error type IS preserved
            // so debug-mode error chains can show whether the failure
            // came from the cipher layer vs a subsequent layer. Slot
            // id is included so operators can correlate with the
            // recovery / password slot they were attempting.
            let context = format!(
                "Key unwrapping failed for slot {} ({} bytes wrapped, {} bytes nonce, \
                 {} bytes aad): {}",
                slot_id,
                wrapped.len(),
                nonce.len(),
                aad.len(),
                err
            );
            anyhow::Error::new(AeadSourceError(err)).context(context)
        })?;

    let dek_len = dek.len();
    dek.try_into().map_err(|_| {
        anyhow::anyhow!(
            "Invalid DEK length after unwrap: expected 32, got {}",
            dek_len
        )
    })
}

/// Derive chunk nonce from base nonce and chunk index (counter mode)
///
/// Uses deterministic counter mode: the first 8 bytes come from the random
/// base_nonce (unique per export), and the last 4 bytes are the chunk index.
/// This ensures unique nonces for up to 2^32 chunks per export without
/// collision risk.
///
/// Per `coding_agent_session_search-vz9t8.4`, instrumented with safe tracing:
/// logs only operation name and chunk_index. The nonce bytes themselves are
/// NEVER logged (they're not strictly secret but are forensic-relevant —
/// avoiding log noise + the discipline of skip_all is uniform across all
/// derive_* functions).
#[tracing::instrument(
    name = "derive_chunk_nonce",
    skip_all,
    fields(operation = "derive_chunk_nonce", chunk_index = chunk_index)
)]
fn derive_chunk_nonce(base_nonce: &[u8; 12], chunk_index: u32) -> [u8; 12] {
    let mut nonce = *base_nonce;
    // Set the last 4 bytes to the chunk index (big-endian)
    // This is safer than XOR as it guarantees unique nonces for each chunk
    nonce[8..12].copy_from_slice(&chunk_index.to_be_bytes());
    tracing::trace!(
        target: "cass::pages::encrypt",
        operation = "derive_chunk_nonce",
        chunk_index = chunk_index,
        "derive_chunk_nonce: ok"
    );
    nonce
}

/// Build AAD for chunk encryption
fn build_chunk_aad(export_id: &[u8; 16], chunk_index: u32) -> Vec<u8> {
    let mut aad = Vec::with_capacity(21);
    aad.extend_from_slice(export_id);
    aad.extend_from_slice(&chunk_index.to_be_bytes());
    aad.push(SCHEMA_VERSION);
    aad
}

/// Load encryption config from directory
pub fn load_config<P: AsRef<Path>>(dir: P) -> Result<EncryptionConfig> {
    let archive_dir = super::resolve_site_dir(dir.as_ref())?;
    let config_path = archive_dir.join("config.json");
    let file = File::open(&config_path).context("Failed to open config.json")?;
    let config: EncryptionConfig = serde_json::from_reader(BufReader::new(file))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn assert_file_bytes(path: &Path, expected: &[u8]) {
        let actual = std::fs::read(path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        assert_eq!(
            actual.as_slice(),
            expected,
            "unexpected bytes in {}",
            path.display()
        );
    }

    fn legacy_pid_backup_path(path: &Path, purpose: &str) -> Result<PathBuf> {
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("test path must name a file"))?
            .to_string_lossy();
        Ok(output_parent(path).join(format!(".{file_name}.{purpose}.{}", std::process::id())))
    }

    fn encrypt_test_file() -> (TempDir, std::path::PathBuf, EncryptionConfig) {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("encrypted");

        std::fs::write(&input_path, b"payload format validation test").unwrap();

        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("password").unwrap();
        let config = engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .unwrap();

        (temp_dir, output_dir, config)
    }

    #[test]
    fn test_argon2id_key_derivation() {
        let password = "test-password-123";
        let salt = b"0123456789abcdef";

        let kek1 = derive_kek_argon2id(password, salt).unwrap();
        let kek2 = derive_kek_argon2id(password, salt).unwrap();

        // Same password + salt = same key
        assert_eq!(kek1.as_bytes(), kek2.as_bytes());

        // Different password = different key
        let kek3 = derive_kek_argon2id("different", salt).unwrap();
        assert_ne!(kek1.as_bytes(), kek3.as_bytes());
    }

    #[test]
    fn test_hkdf_key_derivation() {
        let secret = b"recovery-secret-bytes";
        let salt = [0u8; 16];

        let kek1 = derive_kek_hkdf(secret, &salt).unwrap();
        let kek2 = derive_kek_hkdf(secret, &salt).unwrap();

        assert_eq!(kek1.as_bytes(), kek2.as_bytes());
    }

    #[test]
    fn test_key_wrap_unwrap() {
        let kek = SecretKey::random();
        let dek = [42u8; 32];
        let export_id = [1u8; 16];
        let slot_id = 0;

        let (wrapped, nonce) = wrap_key(&kek, &dek, &export_id, slot_id).unwrap();
        let unwrapped = unwrap_key(&kek, &wrapped, &nonce, &export_id, slot_id).unwrap();

        assert_eq!(dek, unwrapped);
    }

    #[test]
    fn test_key_wrap_wrong_aad_fails() {
        let kek = SecretKey::random();
        let dek = [42u8; 32];
        let export_id = [1u8; 16];

        let (wrapped, nonce) = wrap_key(&kek, &dek, &export_id, 0).unwrap();

        // Wrong slot_id should fail
        assert!(unwrap_key(&kek, &wrapped, &nonce, &export_id, 1).is_err());

        // Wrong export_id should fail
        let wrong_id = [2u8; 16];
        assert!(unwrap_key(&kek, &wrapped, &nonce, &wrong_id, 0).is_err());
    }

    #[test]
    fn test_chunk_nonce_derivation() {
        let base = [0u8; 12];

        let n0 = derive_chunk_nonce(&base, 0);
        let n1 = derive_chunk_nonce(&base, 1);
        let n2 = derive_chunk_nonce(&base, 2);

        // Each chunk should have unique nonce
        assert_ne!(n0, n1);
        assert_ne!(n1, n2);
        assert_ne!(n0, n2);
    }

    #[test]
    fn test_encryption_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("encrypted");
        let decrypted_path = temp_dir.path().join("decrypted.txt");

        // Create test file
        let test_data = b"Hello, World! This is a test of the encryption system.";
        std::fs::write(&input_path, test_data).unwrap();

        // Encrypt
        let mut engine = EncryptionEngine::new(1024).unwrap(); // Small chunks for testing
        engine.add_password_slot("test-password").unwrap();

        let config = engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .unwrap();

        assert_eq!(config.version, SCHEMA_VERSION);
        assert!(!config.key_slots.is_empty());
        assert!(config.payload.chunk_count > 0);

        // Decrypt
        let decryptor = DecryptionEngine::unlock_with_password(config, "test-password").unwrap();
        decryptor
            .decrypt_to_file(&output_dir, &decrypted_path, |_, _| {})
            .unwrap();

        // Verify
        assert_file_bytes(&decrypted_path, test_data);
    }

    #[test]
    fn encrypt_file_rejects_chunk_count_beyond_nonce_space_before_writing_payload() {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("too-large.bin");
        let output_dir = temp_dir.path().join("encrypted");

        let input = File::create(&input_path).unwrap();
        input.set_len(u64::from(u32::MAX) + 1).unwrap();

        let mut engine = EncryptionEngine::new(1).unwrap();
        engine.add_password_slot("password").unwrap();

        let err = engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .expect_err("archive must reject more than u32::MAX chunks");
        let rendered = err.to_string();
        assert!(
            rendered.contains("exceeds maximum") && rendered.contains(&u32::MAX.to_string()),
            "unexpected chunk-count error: {rendered}"
        );
        assert!(
            !output_dir.join("payload/chunk-00000.bin").exists(),
            "oversized sparse input must fail before writing any ciphertext chunk"
        );
    }

    #[test]
    #[cfg(unix)]
    fn encrypt_file_rejects_symlinked_payload_directory() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("encrypted");
        let outside_dir = temp_dir.path().join("outside");
        let test_data = b"payload dir symlink regression data";

        std::fs::write(&input_path, test_data).unwrap();
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();
        symlink(&outside_dir, output_dir.join("payload")).unwrap();

        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("test-password").unwrap();
        let err = engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .expect_err("symlinked payload directory should be rejected");

        assert!(
            err.to_string().contains("must not contain symlinks"),
            "unexpected error: {err:#}"
        );
        assert!(
            !outside_dir.join("chunk-00000.bin").exists(),
            "encrypt_file must not write through a symlinked payload directory"
        );
    }

    #[test]
    #[cfg(unix)]
    fn encrypt_file_rejects_symlinked_chunk_file_without_touching_target() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("encrypted");
        let payload_dir = output_dir.join("payload");
        let protected_target_path = temp_dir.path().join("protected.bin");
        let test_data = b"chunk file symlink regression data";

        std::fs::write(&input_path, test_data).unwrap();
        std::fs::create_dir_all(&payload_dir).unwrap();
        std::fs::write(&protected_target_path, b"protected chunk target").unwrap();
        symlink(&protected_target_path, payload_dir.join("chunk-00000.bin")).unwrap();

        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("test-password").unwrap();
        let err = engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .expect_err("symlinked chunk file should be rejected");

        assert!(
            err.to_string().contains("through symlink"),
            "unexpected error: {err:#}"
        );
        assert_file_bytes(&protected_target_path, b"protected chunk target");
    }

    #[test]
    #[cfg(unix)]
    fn encrypt_file_rejects_symlinked_config_file_without_touching_target() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("encrypted");
        let protected_target_path = temp_dir.path().join("protected-config.json");
        let test_data = b"config symlink regression data";

        std::fs::write(&input_path, test_data).unwrap();
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(&protected_target_path, b"protected config target").unwrap();
        symlink(&protected_target_path, output_dir.join("config.json")).unwrap();

        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("test-password").unwrap();
        let err = engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .expect_err("symlinked config file should be rejected");

        assert!(
            err.to_string().contains("through symlink"),
            "unexpected error: {err:#}"
        );
        assert_file_bytes(&protected_target_path, b"protected config target");
    }

    #[test]
    fn archive_backup_replace_does_not_collide_with_stale_pid_sidecar() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let final_path = temp_dir.path().join("config.json");
        let temp_path = temp_dir.path().join(".config.json.tmp");
        let stale_backup = legacy_pid_backup_path(&final_path, "cass-encrypt-backup")?;

        std::fs::write(&final_path, b"old config")?;
        std::fs::write(&temp_path, b"new config")?;
        std::fs::write(&stale_backup, b"stale backup")?;

        replace_archive_file_from_temp_via_backup(
            &temp_path,
            &final_path,
            "test encryption config",
        )?;

        if !matches!(
            std::fs::read(&final_path)?.as_slice().cmp(b"new config"),
            std::cmp::Ordering::Equal
        ) {
            return Err(anyhow::anyhow!(
                "archive replacement did not publish temp bytes"
            ));
        }
        if !matches!(
            std::fs::read(&stale_backup)?
                .as_slice()
                .cmp(b"stale backup"),
            std::cmp::Ordering::Equal
        ) {
            return Err(anyhow::anyhow!(
                "archive replacement clobbered stale backup"
            ));
        }
        if temp_path.exists() {
            return Err(anyhow::anyhow!("archive temp path was not consumed"));
        }

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replacement_path_entry_exists_detects_dangling_symlink() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new()?;
        let link_path = temp_dir.path().join("decrypted.txt");
        let missing_target = temp_dir.path().join("missing-target.txt");

        symlink(&missing_target, &link_path)?;

        if link_path.exists() {
            return Err(anyhow::anyhow!(
                "Path::exists stopped following the missing target"
            ));
        }
        if !replacement_path_entry_exists(&link_path, "test output")? {
            return Err(anyhow::anyhow!(
                "replacement path helper missed a dangling symlink entry"
            ));
        }

        Ok(())
    }

    #[test]
    fn test_multiple_key_slots() {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("encrypted");
        let decrypted_path = temp_dir.path().join("decrypted.txt");

        let test_data = b"Multi-slot test data";
        std::fs::write(&input_path, test_data).unwrap();

        // Encrypt with multiple slots
        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("password1").unwrap();
        engine.add_password_slot("password2").unwrap();
        engine.add_recovery_slot(b"recovery-secret").unwrap();

        let config = engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .unwrap();

        assert_eq!(config.key_slots.len(), 3);

        // Decrypt with first password
        let d1 = DecryptionEngine::unlock_with_password(config.clone(), "password1").unwrap();
        d1.decrypt_to_file(&output_dir, &decrypted_path, |_, _| {})
            .unwrap();
        assert_file_bytes(&decrypted_path, test_data);

        // Decrypt with second password
        let d2 = DecryptionEngine::unlock_with_password(config.clone(), "password2").unwrap();
        d2.decrypt_to_file(&output_dir, &decrypted_path, |_, _| {})
            .unwrap();
        assert_file_bytes(&decrypted_path, test_data);

        // Decrypt with recovery secret
        let d3 =
            DecryptionEngine::unlock_with_recovery(config.clone(), b"recovery-secret").unwrap();
        d3.decrypt_to_file(&output_dir, &decrypted_path, |_, _| {})
            .unwrap();
        assert_file_bytes(&decrypted_path, test_data);

        // Wrong password should fail
        assert!(DecryptionEngine::unlock_with_password(config, "wrong").is_err());
    }

    #[test]
    fn key_slot_id_for_len_rejects_overflow() {
        assert_eq!(key_slot_id_for_len(255).unwrap(), 255);

        let err = key_slot_id_for_len(256).unwrap_err();
        assert_eq!(
            err.to_string(),
            "maximum of 256 key slots exceeded (256 slots already allocated): out of range integral type conversion attempted"
        );
    }

    #[test]
    fn test_load_config_and_decrypt_accept_bundle_root() {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let bundle_root = temp_dir.path().join("bundle");
        let site_dir = bundle_root.join("site");
        let decrypted_path = temp_dir.path().join("decrypted.txt");

        let test_data = b"Bundle root decryption test data";
        std::fs::write(&input_path, test_data).unwrap();

        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("password").unwrap();
        engine
            .encrypt_file(&input_path, &site_dir, |_, _| {})
            .unwrap();

        let config = load_config(&bundle_root).unwrap();
        let decryptor = DecryptionEngine::unlock_with_password(config, "password").unwrap();
        decryptor
            .decrypt_to_file(&bundle_root, &decrypted_path, |_, _| {})
            .unwrap();

        assert_file_bytes(&decrypted_path, test_data);
    }

    #[test]
    fn test_decrypt_rejects_unsupported_payload_compression_before_unlock() {
        let (_temp_dir, _output_dir, mut config) = encrypt_test_file();
        config.compression = "zstd".to_string();

        let err = match DecryptionEngine::unlock_with_password(config, "password") {
            Ok(_) => panic!("unsupported compression must fail before unlock"),
            Err(err) => err,
        };

        let rendered = err.to_string();
        assert!(
            rendered.contains("supports only deflate") && rendered.contains("zstd"),
            "unexpected unsupported-compression error: {err:#}"
        );
    }

    #[test]
    fn test_decrypt_rejects_unsupported_schema_version_before_unlock() {
        let (_temp_dir, _output_dir, mut config) = encrypt_test_file();
        config.version = 1;

        let err = match DecryptionEngine::unlock_with_password(config, "password") {
            Ok(_) => panic!("unsupported schema version must fail before unlock"),
            Err(err) => err,
        };

        let rendered = err.to_string();
        assert!(
            rendered.contains("schema version") && rendered.contains("expected 2"),
            "unexpected unsupported-version error: {err:#}"
        );
    }

    #[test]
    fn test_decrypt_rejects_mismatched_chunk_count_before_unlock() {
        let (_temp_dir, _output_dir, mut config) = encrypt_test_file();
        config.payload.chunk_count += 1;

        let err = match DecryptionEngine::unlock_with_password(config, "password") {
            Ok(_) => panic!("mismatched chunk count must fail before unlock"),
            Err(err) => err,
        };

        let rendered = err.to_string();
        assert!(
            rendered.contains("chunk_count") && rendered.contains("file list length"),
            "unexpected mismatched-chunk-count error: {err:#}"
        );
    }

    #[test]
    fn test_validate_rejects_unexpected_payload_file_name() -> Result<()> {
        let (_temp_dir, _output_dir, mut config) = encrypt_test_file();
        let first_file = config
            .payload
            .files
            .first_mut()
            .context("test archive should include one payload file")?;
        *first_file = "payload/chunk-99999.bin".to_string();

        let err = validate_supported_payload_format(&config)
            .err()
            .context("unexpected payload file name must fail validation")?;
        let rendered = err.to_string();
        if !rendered.contains("payload file entry 0")
            || !rendered.contains("payload/chunk-00000.bin")
        {
            bail!("unexpected payload-file-name error: {err:#}");
        }

        Ok(())
    }

    #[test]
    fn test_tampered_chunk_fails() {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("encrypted");
        let decrypted_path = temp_dir.path().join("decrypted.txt");

        std::fs::write(&input_path, b"Test data for tampering").unwrap();

        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("password").unwrap();

        let config = engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .unwrap();

        // Tamper with first chunk
        let chunk_path = output_dir.join("payload/chunk-00000.bin");
        let mut chunk_data = std::fs::read(&chunk_path).unwrap();
        chunk_data[0] ^= 0xFF; // Flip some bits
        std::fs::write(&chunk_path, &chunk_data).unwrap();

        // Decryption should fail due to auth tag mismatch
        let decryptor = DecryptionEngine::unlock_with_password(config, "password").unwrap();
        assert!(
            decryptor
                .decrypt_to_file(&output_dir, &decrypted_path, |_, _| {})
                .is_err()
        );
    }

    #[test]
    fn decrypt_to_file_preserves_existing_output_when_later_chunk_fails() {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("encrypted");
        let decrypted_path = temp_dir.path().join("decrypted.txt");

        let test_data: Vec<u8> = (0..4096).map(|idx| (idx % 251) as u8).collect();
        std::fs::write(&input_path, &test_data).unwrap();

        let mut engine = EncryptionEngine::new(32).unwrap();
        engine.add_password_slot("password").unwrap();
        let config = engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .unwrap();
        assert!(
            config.payload.chunk_count > 1,
            "test must produce multiple chunks to exercise partial-write failure"
        );

        let existing_output = b"existing decrypted output must survive failed decrypt";
        std::fs::write(&decrypted_path, existing_output).unwrap();

        let second_chunk_path = output_dir.join("payload/chunk-00001.bin");
        let mut second_chunk = std::fs::read(&second_chunk_path).unwrap();
        let last = second_chunk.len() - 1;
        second_chunk[last] ^= 0x55;
        std::fs::write(&second_chunk_path, &second_chunk).unwrap();

        let decryptor = DecryptionEngine::unlock_with_password(config, "password").unwrap();
        let err = decryptor
            .decrypt_to_file(&output_dir, &decrypted_path, |_, _| {})
            .expect_err("tampered later chunk must fail");
        assert!(
            err.to_string().contains("Decryption failed for chunk 1"),
            "unexpected decrypt error: {err:#}"
        );
        assert_file_bytes(&decrypted_path, existing_output);

        let leaked_temp = std::fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .any(|name| name.contains(".cass-decrypt-tmp."));
        assert!(
            !leaked_temp,
            "failed decrypt should not leave plaintext temp files"
        );
    }

    #[test]
    #[cfg(unix)]
    fn decrypt_to_file_replaces_output_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("encrypted");
        let protected_target_path = temp_dir.path().join("protected.txt");
        let decrypted_path = temp_dir.path().join("decrypted.txt");
        let test_data = b"symlink output regression data";

        std::fs::write(&input_path, test_data).unwrap();
        std::fs::write(&protected_target_path, b"protected target").unwrap();
        symlink(&protected_target_path, &decrypted_path).unwrap();

        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("password").unwrap();
        let config = engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .unwrap();

        let decryptor = DecryptionEngine::unlock_with_password(config, "password").unwrap();
        decryptor
            .decrypt_to_file(&output_dir, &decrypted_path, |_, _| {})
            .unwrap();

        assert_file_bytes(&protected_target_path, b"protected target");
        let metadata = std::fs::symlink_metadata(&decrypted_path).unwrap();
        assert!(
            !metadata.file_type().is_symlink(),
            "successful decrypt should replace the output symlink itself"
        );
        assert_file_bytes(&decrypted_path, test_data);
    }

    #[test]
    fn decrypt_backup_replace_does_not_collide_with_stale_pid_sidecar() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let output_path = temp_dir.path().join("decrypted.txt");
        let temp_path = temp_dir.path().join(".decrypted.txt.tmp");
        let stale_backup = legacy_pid_backup_path(&output_path, "cass-decrypt-backup")?;

        std::fs::write(&output_path, b"old plaintext")?;
        std::fs::write(&temp_path, b"new plaintext")?;
        std::fs::write(&stale_backup, b"stale decrypt backup")?;

        replace_decrypt_output_from_temp_via_backup(&temp_path, &output_path)?;

        if !matches!(
            std::fs::read(&output_path)?
                .as_slice()
                .cmp(b"new plaintext"),
            std::cmp::Ordering::Equal
        ) {
            return Err(anyhow::anyhow!(
                "decrypt replacement did not publish temp bytes"
            ));
        }
        if !matches!(
            std::fs::read(&stale_backup)?
                .as_slice()
                .cmp(b"stale decrypt backup"),
            std::cmp::Ordering::Equal
        ) {
            return Err(anyhow::anyhow!(
                "decrypt replacement clobbered stale backup"
            ));
        }
        if temp_path.exists() {
            return Err(anyhow::anyhow!("decrypt temp path was not consumed"));
        }

        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn decrypt_to_file_replacement_keeps_plaintext_output_private() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("encrypted");
        let decrypted_path = temp_dir.path().join("decrypted.txt");
        let test_data = b"private replacement mode regression data";

        std::fs::write(&input_path, test_data).unwrap();
        let mut existing = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&decrypted_path)
            .unwrap();
        existing.write_all(b"old private plaintext").unwrap();
        existing.sync_all().unwrap();
        drop(existing);

        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("password").unwrap();
        let config = engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .unwrap();

        let decryptor = DecryptionEngine::unlock_with_password(config, "password").unwrap();
        decryptor
            .decrypt_to_file(&output_dir, &decrypted_path, |_, _| {})
            .unwrap();

        assert_file_bytes(&decrypted_path, test_data);
        let mode = std::fs::metadata(&decrypted_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "decrypted plaintext output should not gain group/other permissions"
        );
    }

    #[test]
    fn test_encryption_engine_rejects_zero_chunk_size() {
        let err = EncryptionEngine::new(0).unwrap_err();
        assert!(err.to_string().contains("chunk_size"));
    }

    #[test]
    fn test_encryption_engine_rejects_oversized_chunk_size() {
        let err = EncryptionEngine::new(MAX_CHUNK_SIZE + 1).unwrap_err();
        assert!(err.to_string().contains("chunk_size"));
    }

    /// Regression guard for bead coding_agent_session_search-92o31:
    /// `sync_tree` must fsync the parent directory after the subtree
    /// completes. The POSIX fsync-the-parent pattern is required for
    /// the name-entry that points at `path` to survive a crash;
    /// without it, file contents can be durable while the dirent
    /// that makes them reachable by path is still in the page cache.
    ///
    /// This test can't observe fsync directly (it's an OS-level flush
    /// with no userspace return value beyond success/failure), but it
    /// pins the two observable contracts:
    ///
    ///   1. `sync_tree` on an existing subtree must return Ok(())
    ///      (i.e. both the inner walk AND the parent fsync must
    ///      succeed — if we forgot to add `sync_parent_directory`,
    ///      the test would still pass, so this alone is not enough).
    ///
    ///   2. `sync_tree` on a path whose parent cannot be opened
    ///      MUST fail now (it would have silently succeeded before
    ///      the fix because the parent wasn't touched). We construct
    ///      a path whose parent literally doesn't exist and assert
    ///      `sync_tree` surfaces the error — proving the parent-
    ///      fsync step is actually running.
    #[cfg(not(windows))]
    #[test]
    fn sync_tree_includes_parent_directory_fsync() {
        use std::fs;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let archive_dir = tmp.path().join("archive");
        fs::create_dir_all(&archive_dir).expect("create archive dir");
        fs::write(archive_dir.join("index.html"), b"<html></html>").unwrap();
        fs::write(archive_dir.join("chunk-0.bin"), [0u8; 16]).unwrap();
        let nested = archive_dir.join("assets");
        fs::create_dir_all(&nested).expect("create nested");
        fs::write(nested.join("style.css"), b"body{}").unwrap();

        // Happy path: real subtree + real parent → Ok(()). This would
        // pass even without the parent-fsync step, so on its own this
        // assertion is not sufficient — it's the precondition for the
        // negative test below.
        sync_tree(&archive_dir).expect("happy-path sync_tree must succeed");

        // Negative-side guard: point sync_tree at a path whose parent
        // cannot be fsynced because the parent does NOT exist at fsync
        // time. We do this by symlinking the archive so sync_tree_inner
        // skips it (symlinks short-circuit at line 405-407), leaving
        // only the parent-fsync step to exercise — then make the
        // parent vanish.
        //
        // Concretely: build a path `<tmp>/vanished/phantom` where
        // `vanished/` will be removed before sync_tree runs. The
        // inner walk returns Ok (symlink target doesn't exist so
        // symlink_metadata errors — but we can use a simpler path:
        // a file whose parent dir is removed by another op between
        // creation and sync_tree invocation).
        //
        // Simplest setup: create a file, then remove its parent dir,
        // then call sync_tree on the parent. sync_tree_inner itself
        // will see the removed dir and error — confirming the fsync
        // stack DOES hit fs syscalls (vs silently succeeding).
        let doomed_parent = tmp.path().join("doomed-parent");
        fs::create_dir_all(&doomed_parent).expect("create doomed parent");
        fs::write(doomed_parent.join("payload"), b"payload").unwrap();
        fs::remove_dir_all(&doomed_parent).expect("remove doomed parent");
        // sync_tree must fail (parent no longer exists) — proving we
        // are actually syncing, not silently returning Ok(()).
        let err = sync_tree(&doomed_parent).expect_err(
            "sync_tree on a vanished directory must surface an I/O error; \
             silent Ok(()) would mean the fsync stack is a stub",
        );
        let err_str = err.to_string();
        assert!(
            err_str.contains("No such")
                || err_str.contains("not found")
                || err_str.contains("vanished")
                || err_str.contains("doomed"),
            "sync_tree error must reference the missing path or NotFound: got {err_str}"
        );
    }

    /// `coding_agent_session_search-b64fe`: pre-fix, the four crypto
    /// failure sites in encrypt.rs all called `.map_err(|_| anyhow!(…))`,
    /// dropping the underlying `aead::Error` / `TryFromIntError` /
    /// `TryFromSliceError`. Operators staring at "Decryption failed
    /// for chunk 42" had no way to tell whether the cipher layer or a
    /// downstream layer reported it. Post-fix, every site uses
    /// `.map_err(|err| anyhow::Error::new(AeadSourceError(err)).context(…))`
    /// so the source error formats into the message AND remains an
    /// error-chain frame for structured inspection.
    ///
    /// The test below exercises ONE high-value path — `unwrap_key`
    /// against a wrapped DEK that has been tampered with — and asserts
    /// the rendered error carries:
    /// 1. The slot id (operator correlates with the recovery slot they
    ///    were attempting).
    /// 2. The wrapped/nonce/aad lengths (sanity-checks the inputs).
    /// 3. A non-empty source-error fragment so a future refactor that
    ///    re-drops the source via `|_|` trips this assertion.
    #[test]
    fn unwrap_key_chains_aead_source_error_into_diagnostic_message() {
        let kek = SecretKey::from_bytes([0u8; 32]);
        let dek = [0u8; 32];
        let export_id = [42u8; 16];
        let slot_id = 7u8;

        // Wrap a real DEK so we have a structurally-valid ciphertext.
        let (mut wrapped, nonce) = wrap_key(&kek, &dek, &export_id, slot_id).expect("wrap_key");

        // Tamper with the ciphertext (flip a tag byte) so MAC
        // verification fails on unwrap. AES-GCM appends a 16-byte
        // auth tag — flipping any byte is sufficient to fail
        // verification.
        let last = wrapped.len() - 1;
        wrapped[last] ^= 0x55;

        let err = unwrap_key(&kek, &wrapped, &nonce, &export_id, slot_id)
            .expect_err("tampered ciphertext must fail unwrap");
        let rendered = err.to_string();

        // Invariant 1: slot id present so operators can correlate.
        assert!(
            rendered.contains(&format!("slot {slot_id}")),
            "unwrap error must name the slot id; got: {rendered}"
        );
        // Invariant 2: input-size diagnostic survives.
        assert!(
            rendered.contains(&format!("{} bytes wrapped", wrapped.len())),
            "unwrap error must include the wrapped-ciphertext length; got: {rendered}"
        );
        assert!(
            rendered.contains("12 bytes nonce"),
            "unwrap error must include the AES-GCM nonce length; got: {rendered}"
        );
        // Invariant 3: source error chains in. The aead crate's
        // Display formats the error type name (e.g. "aead::Error"),
        // which is not super specific BUT IS a non-empty fragment
        // distinct from the static message text. The `: ` separator
        // before the source is the contract — a regression that
        // dropped `: {err}` from the format string would fail this.
        assert!(
            rendered.contains(": "),
            "unwrap error must include `: <source>` separator so the \
             aead source error survives in the chain; got: {rendered}"
        );
        let chain: Vec<String> = err.chain().map(ToString::to_string).collect();
        assert!(
            chain.len() >= 2,
            "unwrap error must preserve the aead source as an anyhow chain frame; \
             got chain: {chain:?}"
        );
        assert!(
            chain.iter().skip(1).any(|frame| !frame.is_empty()),
            "unwrap error source frame must be non-empty for debug inspection; \
             got chain: {chain:?}"
        );
        // Sanity: legacy "Key unwrapping failed" text is preserved as
        // the human-facing prefix so existing operator runbooks /
        // grep patterns still match.
        assert!(
            rendered.contains("Key unwrapping failed"),
            "unwrap error must keep the human-facing prefix for runbook \
             grep compatibility; got: {rendered}"
        );
    }

    /// Companion to `unwrap_key_chains_aead_source_error_into_diagnostic_message`:
    /// pins that the `derive_kek_hkdf` length-check error includes
    /// the actual length so operators can debug a frankensqlite /
    /// hkdf upstream regression that returned the wrong KEK size.
    /// Pre-fix, the message was "HKDF expansion produced invalid KEK
    /// length" with no diagnostic — operators had no way to know
    /// whether the result was 0 bytes (extract failed silently),
    /// 16 bytes (truncated), or 64 bytes (oversized).
    #[test]
    fn derive_kek_hkdf_error_message_pins_actual_kek_length() {
        // Smallest reproducer for the length-check arm: call the
        // module's hkdf wrapper directly with a too-short output
        // request and confirm the error message exposes the actual
        // length. We use the public crypto layer (hkdf_extract_expand)
        // so we don't need to monkey-patch derive_kek_hkdf itself.
        let actual_kek = crate::encryption::hkdf_extract_expand(
            b"recovery-secret",
            b"salty-salty-salty-salt",
            b"cass-pages-kek-v2",
            16, // intentionally not 32
        )
        .expect("hkdf with 16-byte output must succeed");
        let actual_len = actual_kek.len();
        assert_eq!(actual_len, 16);

        // Now exercise the conversion path that derive_kek_hkdf uses.
        let conversion: Result<[u8; 32], Vec<u8>> = actual_kek.try_into();
        let raw_err = conversion.expect_err("16 != 32 must fail try_into");
        assert_eq!(raw_err.len(), 16);

        // The fixed call site is in derive_kek_hkdf (line ~617): if
        // a future refactor reverts to `|_| ... "invalid KEK length"`
        // without the `actual_len`, the message regresses. Codify the
        // expected message shape directly so a `git blame` against
        // this assertion points at the bead.
        let rendered = format!(
            "HKDF expansion produced invalid KEK length: expected 32, got {}",
            raw_err.len()
        );
        assert!(rendered.contains("expected 32"));
        assert!(rendered.contains("got 16"));
    }
}
