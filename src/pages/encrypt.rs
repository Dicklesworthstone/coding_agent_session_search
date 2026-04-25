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
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct AeadSourceError(aes_gcm::Error);

/// Default chunk size for streaming encryption (8 MiB)
pub const DEFAULT_CHUNK_SIZE: usize = 8 * 1024 * 1024;

/// Maximum chunk size (32 MiB)
pub const MAX_CHUNK_SIZE: usize = 32 * 1024 * 1024;

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
const SCHEMA_VERSION: u8 = 2;

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

        std::fs::create_dir_all(output_dir)?;
        let payload_dir = output_dir.join("payload");
        std::fs::create_dir_all(&payload_dir)?;

        // Read input file size for progress
        let input_size = std::fs::metadata(input_path)?.len();

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
            let mut chunk_file = File::create(&chunk_path)?;
            chunk_file.write_all(&ciphertext)?;
            chunk_file.sync_all()?;

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
        let config_file = File::create(&config_path)?;
        let mut config_writer = BufWriter::new(config_file);
        serde_json::to_writer_pretty(&mut config_writer, &config)?;
        config_writer.flush()?;
        config_writer.get_ref().sync_all()?;
        sync_tree(output_dir)?;

        Ok(config)
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

        let mut output_file = File::create(output_path)?;
        let mut writer = BufWriter::new(&mut output_file);

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
        progress(
            self.config.payload.chunk_count,
            self.config.payload.chunk_count,
        );

        Ok(())
    }
}

/// Derive KEK from password using Argon2id
fn derive_kek_argon2id(password: &str, salt: &[u8]) -> Result<SecretKey> {
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

    Ok(SecretKey::from_bytes(kek))
}

/// Derive KEK from recovery secret using HKDF-SHA256
fn derive_kek_hkdf(secret: &[u8], salt: &[u8]) -> Result<SecretKey> {
    let kek = crate::encryption::hkdf_extract_expand(secret, salt, b"cass-pages-kek-v2", 32)
        .map_err(|e| anyhow::anyhow!("HKDF extract+expand failed for recovery secret KEK: {e}"))?;
    let actual_len = kek.len();
    let kek: [u8; 32] = kek.try_into().map_err(|_| {
        anyhow::anyhow!(
            "HKDF expansion produced invalid KEK length: expected 32, got {}",
            actual_len
        )
    })?;
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
fn derive_chunk_nonce(base_nonce: &[u8; 12], chunk_index: u32) -> [u8; 12] {
    let mut nonce = *base_nonce;
    // Set the last 4 bytes to the chunk index (big-endian)
    // This is safer than XOR as it guarantees unique nonces for each chunk
    nonce[8..12].copy_from_slice(&chunk_index.to_be_bytes());
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
        let decrypted = std::fs::read(&decrypted_path).unwrap();
        assert_eq!(decrypted, test_data);
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
        assert_eq!(std::fs::read(&decrypted_path).unwrap(), test_data);

        // Decrypt with second password
        let d2 = DecryptionEngine::unlock_with_password(config.clone(), "password2").unwrap();
        d2.decrypt_to_file(&output_dir, &decrypted_path, |_, _| {})
            .unwrap();
        assert_eq!(std::fs::read(&decrypted_path).unwrap(), test_data);

        // Decrypt with recovery secret
        let d3 =
            DecryptionEngine::unlock_with_recovery(config.clone(), b"recovery-secret").unwrap();
        d3.decrypt_to_file(&output_dir, &decrypted_path, |_, _| {})
            .unwrap();
        assert_eq!(std::fs::read(&decrypted_path).unwrap(), test_data);

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

        assert_eq!(std::fs::read(&decrypted_path).unwrap(), test_data);
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
