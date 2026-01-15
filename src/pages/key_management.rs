//! Key management operations for encrypted pages archives.
//!
//! Provides CLI operations to manage key slots in an encrypted archive:
//! - `list`: Show all key slots
//! - `add`: Add a new password or recovery key slot
//! - `revoke`: Remove a key slot
//! - `rotate`: Full key rotation (regenerate DEK, re-encrypt payload)
//!
//! # Security Model
//!
//! The archive uses envelope encryption with multiple key slots (like LUKS):
//! - A random Data Encryption Key (DEK) encrypts the payload
//! - Each key slot wraps the DEK with a Key Encryption Key (KEK)
//! - KEK is derived from password (Argon2id) or recovery secret (HKDF-SHA256)
//! - Add/revoke only modifies config.json; payload unchanged
//! - Rotate re-encrypts entire payload with new DEK

use crate::pages::encrypt::{
    Argon2Params, EncryptionConfig, KdfAlgorithm, KeySlot, SlotType, load_config,
};
use crate::pages::qr::RecoverySecret;
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use anyhow::{Result, bail};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::prelude::*;
use chrono::{DateTime, Utc};
use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};
use hkdf::Hkdf;
use rand::{RngCore, rngs::OsRng};
use serde::Serialize;
use sha2::Sha256;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use tracing::info;
use zeroize::Zeroize;

/// Argon2id default parameters
const ARGON2_MEMORY_KB: u32 = 65536; // 64 MB
const ARGON2_ITERATIONS: u32 = 3;
const ARGON2_PARALLELISM: u32 = 4;

/// Schema version for encryption
const SCHEMA_VERSION: u8 = 2;

/// Result of listing key slots
#[derive(Debug, Clone, Serialize)]
pub struct KeyListResult {
    pub slots: Vec<KeySlotInfo>,
    pub active_slots: usize,
    pub dek_created_at: Option<String>,
    pub export_id: String,
}

/// Information about a single key slot
#[derive(Debug, Clone, Serialize)]
pub struct KeySlotInfo {
    pub id: u8,
    pub slot_type: String,
    pub kdf: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kdf_params: Option<Argon2Params>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Result of adding a key slot
#[derive(Debug)]
pub enum AddKeyResult {
    Password { slot_id: u8 },
    Recovery { slot_id: u8, secret: RecoverySecret },
}

/// Result of revoking a key slot
#[derive(Debug, Serialize)]
pub struct RevokeResult {
    pub revoked_slot_id: u8,
    pub remaining_slots: usize,
}

/// Result of key rotation
#[derive(Debug, Serialize)]
pub struct RotateResult {
    pub new_dek_created_at: DateTime<Utc>,
    pub slot_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_secret: Option<String>,
}

/// List all key slots in an archive
pub fn key_list(archive_dir: &Path) -> Result<KeyListResult> {
    let config = load_config(archive_dir)?;

    let slots: Vec<KeySlotInfo> = config
        .key_slots
        .iter()
        .map(|slot| KeySlotInfo {
            id: slot.id,
            slot_type: match slot.slot_type {
                SlotType::Password => "password".to_string(),
                SlotType::Recovery => "recovery".to_string(),
            },
            kdf: match slot.kdf {
                KdfAlgorithm::Argon2id => "argon2id".to_string(),
                KdfAlgorithm::HkdfSha256 => "hkdf-sha256".to_string(),
            },
            kdf_params: slot.argon2_params.clone(),
            label: None, // Labels stored in encrypted metadata (future)
        })
        .collect();

    Ok(KeyListResult {
        active_slots: slots.len(),
        slots,
        dek_created_at: None, // Would need to store in config
        export_id: config.export_id,
    })
}

/// Add a new password-based key slot
pub fn key_add_password(
    archive_dir: &Path,
    current_password: &str,
    new_password: &str,
) -> Result<u8> {
    let config_path = archive_dir.join("config.json");
    let mut config = load_config(archive_dir)?;

    // Unlock with current password to get DEK
    let mut dek = unwrap_dek_with_password(&config, current_password)?;

    // Create new slot (use max ID + 1 since IDs are stable after revocation)
    // If no slots exist, start at 0; otherwise use max + 1
    let slot_id = match config.key_slots.iter().map(|s| s.id).max() {
        Some(max_id) => max_id.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("Cannot add more key slots: maximum slot ID (255) reached")
        })?,
        None => 0,
    };
    let new_slot = create_password_slot(new_password, &dek, &config.export_id, slot_id)?;

    // Zeroize DEK before any potential early return
    dek.zeroize();

    config.key_slots.push(new_slot);

    // Write updated config
    let file = File::create(&config_path)?;
    serde_json::to_writer_pretty(BufWriter::new(file), &config)?;

    // Update integrity.json if present
    update_integrity_hash(archive_dir, "config.json")?;

    info!(slot_id, "Added password key slot");
    Ok(slot_id)
}

/// Add a new recovery secret key slot
pub fn key_add_recovery(
    archive_dir: &Path,
    current_password: &str,
) -> Result<(u8, RecoverySecret)> {
    let config_path = archive_dir.join("config.json");
    let mut config = load_config(archive_dir)?;

    // Unlock with current password to get DEK
    let mut dek = unwrap_dek_with_password(&config, current_password)?;

    // Generate recovery secret
    let secret = RecoverySecret::generate();

    // Create new slot (use max ID + 1 since IDs are stable after revocation)
    // If no slots exist, start at 0; otherwise use max + 1
    let slot_id = match config.key_slots.iter().map(|s| s.id).max() {
        Some(max_id) => max_id.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("Cannot add more key slots: maximum slot ID (255) reached")
        })?,
        None => 0,
    };
    let new_slot = create_recovery_slot(secret.as_bytes(), &dek, &config.export_id, slot_id)?;

    // Zeroize DEK before any potential early return
    dek.zeroize();

    config.key_slots.push(new_slot);

    // Write updated config
    let file = File::create(&config_path)?;
    serde_json::to_writer_pretty(BufWriter::new(file), &config)?;

    // Update integrity.json if present
    update_integrity_hash(archive_dir, "config.json")?;

    info!(slot_id, "Added recovery key slot");
    Ok((slot_id, secret))
}

/// Revoke a key slot
pub fn key_revoke(
    archive_dir: &Path,
    current_password: &str,
    slot_id_to_revoke: u8,
) -> Result<RevokeResult> {
    let config_path = archive_dir.join("config.json");
    let mut config = load_config(archive_dir)?;

    // Safety: Cannot revoke last slot
    if config.key_slots.len() <= 1 {
        bail!("Cannot revoke the last remaining key slot");
    }

    // Find which slot authenticates with this password
    let (auth_slot_id, mut dek) = unwrap_dek_with_slot_id(&config, current_password)?;

    // Zeroize DEK immediately - we only needed to verify the password works
    dek.zeroize();

    // Safety: Cannot revoke slot used for authentication
    if auth_slot_id == slot_id_to_revoke {
        bail!(
            "Cannot revoke slot {} used for authentication. Use a different password.",
            slot_id_to_revoke
        );
    }

    // Verify slot exists
    if !config.key_slots.iter().any(|s| s.id == slot_id_to_revoke) {
        bail!("Slot {} not found", slot_id_to_revoke);
    }

    // Remove the slot (keeping IDs stable - they're part of the AAD binding)
    config.key_slots.retain(|s| s.id != slot_id_to_revoke);

    // Write updated config
    let file = File::create(&config_path)?;
    serde_json::to_writer_pretty(BufWriter::new(file), &config)?;

    // Update integrity.json if present
    update_integrity_hash(archive_dir, "config.json")?;

    info!(slot_id = slot_id_to_revoke, "Revoked key slot");
    Ok(RevokeResult {
        revoked_slot_id: slot_id_to_revoke,
        remaining_slots: config.key_slots.len(),
    })
}

/// Full key rotation - regenerate DEK and re-encrypt payload
pub fn key_rotate(
    archive_dir: &Path,
    old_password: &str,
    new_password: &str,
    keep_recovery: bool,
    progress: impl Fn(f32),
) -> Result<RotateResult> {
    let config_path = archive_dir.join("config.json");
    let config = load_config(archive_dir)?;

    // 1. Decrypt payload with old password
    let mut old_dek = unwrap_dek_with_password(&config, old_password)?;
    let plaintext = decrypt_all_chunks(archive_dir, &old_dek, &config, |p| progress(p * 0.5))?;

    // 2. Generate new DEK and export_id
    let mut new_dek = [0u8; 32];
    let mut new_export_id = [0u8; 16];
    let mut new_base_nonce = [0u8; 12];
    OsRng.fill_bytes(&mut new_dek);
    OsRng.fill_bytes(&mut new_export_id);
    OsRng.fill_bytes(&mut new_base_nonce);

    // 3. Re-encrypt payload with new DEK
    let chunk_count = encrypt_all_chunks(
        &plaintext,
        &new_dek,
        &new_export_id,
        &new_base_nonce,
        config.payload.chunk_size,
        &archive_dir.join("payload"),
        |p| progress(0.5 + p * 0.5),
    )?;

    // 4. Create new key slots
    let mut new_slots = vec![create_password_slot(
        new_password,
        &new_dek,
        &BASE64_STANDARD.encode(new_export_id),
        0,
    )?];

    let mut recovery_secret_encoded: Option<String> = None;
    if keep_recovery {
        let secret = RecoverySecret::generate();
        new_slots.push(create_recovery_slot(
            secret.as_bytes(),
            &new_dek,
            &BASE64_STANDARD.encode(new_export_id),
            1,
        )?);
        recovery_secret_encoded = Some(secret.encoded().to_string());
    }

    // 5. Write new config
    let new_config = EncryptionConfig {
        version: config.version,
        export_id: BASE64_STANDARD.encode(new_export_id),
        base_nonce: BASE64_STANDARD.encode(new_base_nonce),
        compression: config.compression,
        kdf_defaults: Argon2Params::default(),
        payload: crate::pages::encrypt::PayloadMeta {
            chunk_size: config.payload.chunk_size,
            chunk_count,
            total_compressed_size: 0, // Recalculated
            total_plaintext_size: plaintext.len() as u64,
            files: (0..chunk_count)
                .map(|i| format!("payload/chunk-{:05}.bin", i))
                .collect(),
        },
        key_slots: new_slots.clone(),
    };

    let file = File::create(&config_path)?;
    serde_json::to_writer_pretty(BufWriter::new(file), &new_config)?;

    // 6. Regenerate integrity.json
    regenerate_integrity_manifest(archive_dir)?;

    // 7. Zeroize sensitive key material
    old_dek.zeroize();
    new_dek.zeroize();

    info!("Key rotation complete");
    Ok(RotateResult {
        new_dek_created_at: Utc::now(),
        slot_count: new_slots.len(),
        recovery_secret: recovery_secret_encoded,
    })
}

// ============================================================================
// Helper functions
// ============================================================================

/// Unwrap DEK using password (tries all password slots)
fn unwrap_dek_with_password(config: &EncryptionConfig, password: &str) -> Result<[u8; 32]> {
    let export_id = BASE64_STANDARD.decode(&config.export_id)?;

    for slot in &config.key_slots {
        if slot.slot_type != SlotType::Password {
            continue;
        }

        let salt = BASE64_STANDARD.decode(&slot.salt)?;
        let wrapped_dek = BASE64_STANDARD.decode(&slot.wrapped_dek)?;
        let nonce = BASE64_STANDARD.decode(&slot.nonce)?;

        if let Ok(mut kek) = derive_kek_argon2id(password, &salt) {
            let result = unwrap_key(&kek, &wrapped_dek, &nonce, &export_id, slot.id);
            kek.zeroize(); // Always zeroize KEK after use
            if let Ok(dek) = result {
                return Ok(dek);
            }
        }
    }

    bail!("Invalid password or no matching key slot")
}

/// Unwrap DEK and return which slot was used
fn unwrap_dek_with_slot_id(config: &EncryptionConfig, password: &str) -> Result<(u8, [u8; 32])> {
    let export_id = BASE64_STANDARD.decode(&config.export_id)?;

    for slot in &config.key_slots {
        if slot.slot_type != SlotType::Password {
            continue;
        }

        let salt = BASE64_STANDARD.decode(&slot.salt)?;
        let wrapped_dek = BASE64_STANDARD.decode(&slot.wrapped_dek)?;
        let nonce = BASE64_STANDARD.decode(&slot.nonce)?;

        if let Ok(mut kek) = derive_kek_argon2id(password, &salt) {
            let result = unwrap_key(&kek, &wrapped_dek, &nonce, &export_id, slot.id);
            kek.zeroize(); // Always zeroize KEK after use
            if let Ok(dek) = result {
                return Ok((slot.id, dek));
            }
        }
    }

    bail!("Invalid password or no matching key slot")
}

/// Derive KEK from password using Argon2id
fn derive_kek_argon2id(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
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

    Ok(kek)
}

/// Derive KEK from recovery secret using HKDF-SHA256
fn derive_kek_hkdf(secret: &[u8], salt: &[u8]) -> Result<[u8; 32]> {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), secret);
    let mut kek = [0u8; 32];
    hkdf.expand(b"cass-pages-kek-v2", &mut kek)
        .map_err(|_| anyhow::anyhow!("HKDF expansion failed"))?;
    Ok(kek)
}

/// Unwrap DEK with KEK
fn unwrap_key(
    kek: &[u8; 32],
    wrapped: &[u8],
    nonce: &[u8],
    export_id: &[u8],
    slot_id: u8,
) -> Result<[u8; 32]> {
    let cipher = Aes256Gcm::new_from_slice(kek).expect("Invalid key length");

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
        .map_err(|_| anyhow::anyhow!("Key unwrapping failed"))?;

    dek.try_into()
        .map_err(|_| anyhow::anyhow!("Invalid DEK length"))
}

/// Create a password-based key slot
fn create_password_slot(
    password: &str,
    dek: &[u8; 32],
    export_id_b64: &str,
    slot_id: u8,
) -> Result<KeySlot> {
    let export_id = BASE64_STANDARD.decode(export_id_b64)?;

    // Generate salt
    let mut salt = [0u8; 32];
    OsRng.fill_bytes(&mut salt);

    // Derive KEK from password
    let mut kek = derive_kek_argon2id(password, &salt)?;

    // Wrap DEK
    let result = wrap_key(&kek, dek, &export_id, slot_id);

    // Zeroize KEK immediately after use
    kek.zeroize();

    let (wrapped_dek, nonce) = result?;

    Ok(KeySlot {
        id: slot_id,
        slot_type: SlotType::Password,
        kdf: KdfAlgorithm::Argon2id,
        salt: BASE64_STANDARD.encode(salt),
        wrapped_dek: BASE64_STANDARD.encode(&wrapped_dek),
        nonce: BASE64_STANDARD.encode(nonce),
        argon2_params: Some(Argon2Params::default()),
    })
}

/// Create a recovery secret key slot
fn create_recovery_slot(
    secret: &[u8],
    dek: &[u8; 32],
    export_id_b64: &str,
    slot_id: u8,
) -> Result<KeySlot> {
    let export_id = BASE64_STANDARD.decode(export_id_b64)?;

    // Generate salt
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);

    // Derive KEK from recovery secret
    let mut kek = derive_kek_hkdf(secret, &salt)?;

    // Wrap DEK
    let result = wrap_key(&kek, dek, &export_id, slot_id);

    // Zeroize KEK immediately after use
    kek.zeroize();

    let (wrapped_dek, nonce) = result?;

    Ok(KeySlot {
        id: slot_id,
        slot_type: SlotType::Recovery,
        kdf: KdfAlgorithm::HkdfSha256,
        salt: BASE64_STANDARD.encode(salt),
        wrapped_dek: BASE64_STANDARD.encode(&wrapped_dek),
        nonce: BASE64_STANDARD.encode(nonce),
        argon2_params: None,
    })
}

/// Wrap DEK with KEK using AES-256-GCM
fn wrap_key(
    kek: &[u8; 32],
    dek: &[u8; 32],
    export_id: &[u8],
    slot_id: u8,
) -> Result<(Vec<u8>, [u8; 12])> {
    let cipher = Aes256Gcm::new_from_slice(kek).expect("Invalid key length");

    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);

    // AAD: export_id || slot_id
    let mut aad = Vec::with_capacity(export_id.len() + 1);
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

/// Decrypt all chunks and return plaintext
fn decrypt_all_chunks(
    archive_dir: &Path,
    dek: &[u8; 32],
    config: &EncryptionConfig,
    progress: impl Fn(f32),
) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(dek).expect("Invalid key length");
    let base_nonce = BASE64_STANDARD.decode(&config.base_nonce)?;
    let export_id = BASE64_STANDARD.decode(&config.export_id)?;

    let mut plaintext = Vec::new();

    for (chunk_index, chunk_file) in config.payload.files.iter().enumerate() {
        progress(chunk_index as f32 / config.payload.chunk_count as f32);

        let chunk_path = archive_dir.join(chunk_file);
        let ciphertext = std::fs::read(&chunk_path)?;

        // Derive nonce
        let nonce = derive_chunk_nonce(&base_nonce, chunk_index as u32);

        // Build AAD
        let aad = build_chunk_aad(&export_id, chunk_index as u32);

        // Decrypt
        let compressed = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("Decryption failed for chunk {}", chunk_index))?;

        // Decompress
        let mut decoder = DeflateDecoder::new(&compressed[..]);
        let mut chunk_plaintext = Vec::new();
        decoder.read_to_end(&mut chunk_plaintext)?;

        plaintext.extend(chunk_plaintext);
    }

    progress(1.0);
    Ok(plaintext)
}

/// Encrypt plaintext into chunks
fn encrypt_all_chunks(
    plaintext: &[u8],
    dek: &[u8; 32],
    export_id: &[u8; 16],
    base_nonce: &[u8; 12],
    chunk_size: usize,
    payload_dir: &Path,
    progress: impl Fn(f32),
) -> Result<usize> {
    std::fs::create_dir_all(payload_dir)?;

    let cipher = Aes256Gcm::new_from_slice(dek).expect("Invalid key length");
    let total_chunks = plaintext.len().div_ceil(chunk_size);
    let mut chunk_index = 0u32;

    for (i, chunk) in plaintext.chunks(chunk_size).enumerate() {
        progress(i as f32 / total_chunks as f32);

        // Compress
        let mut compressed = Vec::new();
        {
            let mut encoder = DeflateEncoder::new(&mut compressed, Compression::default());
            encoder.write_all(chunk)?;
            encoder.finish()?;
        }

        // Derive nonce
        let nonce = derive_chunk_nonce(base_nonce, chunk_index);

        // Build AAD
        let aad = build_chunk_aad(export_id, chunk_index);

        // Encrypt
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

        chunk_index += 1;
    }

    progress(1.0);
    Ok(chunk_index as usize)
}

/// Derive chunk nonce from base nonce and chunk index
fn derive_chunk_nonce(base_nonce: &[u8], chunk_index: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..base_nonce.len().min(12)].copy_from_slice(&base_nonce[..base_nonce.len().min(12)]);
    // Set the last 4 bytes to the chunk index (big-endian)
    nonce[8..12].copy_from_slice(&chunk_index.to_be_bytes());
    nonce
}

/// Build AAD for chunk encryption
fn build_chunk_aad(export_id: &[u8], chunk_index: u32) -> Vec<u8> {
    let mut aad = Vec::with_capacity(21);
    aad.extend_from_slice(export_id);
    aad.extend_from_slice(&chunk_index.to_be_bytes());
    aad.push(SCHEMA_VERSION);
    aad
}

/// Update integrity.json for a single file
fn update_integrity_hash(archive_dir: &Path, filename: &str) -> Result<()> {
    let integrity_path = archive_dir.join("integrity.json");
    if !integrity_path.exists() {
        return Ok(());
    }

    // Read current integrity.json
    let file = File::open(&integrity_path)?;
    let mut integrity: serde_json::Value = serde_json::from_reader(BufReader::new(file))?;

    // Calculate new hash for file
    let file_path = archive_dir.join(filename);
    let content = std::fs::read(&file_path)?;
    let hash = sha256_hex(&content);

    // Update hash in integrity
    if let Some(files) = integrity.get_mut("files").and_then(|f| f.as_object_mut()) {
        files.insert(filename.to_string(), serde_json::json!(hash));
    }

    // Write updated integrity.json
    let file = File::create(&integrity_path)?;
    serde_json::to_writer_pretty(BufWriter::new(file), &integrity)?;

    Ok(())
}

/// Regenerate entire integrity.json
fn regenerate_integrity_manifest(archive_dir: &Path) -> Result<()> {
    let integrity_path = archive_dir.join("integrity.json");

    // Find all files to hash
    let mut files_map = serde_json::Map::new();

    // Hash config.json
    let config_path = archive_dir.join("config.json");
    if config_path.exists() {
        let content = std::fs::read(&config_path)?;
        files_map.insert(
            "config.json".to_string(),
            serde_json::json!(sha256_hex(&content)),
        );
    }

    // Hash all payload chunks
    let payload_dir = archive_dir.join("payload");
    if payload_dir.exists() {
        for entry in std::fs::read_dir(&payload_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let filename = entry.file_name().to_string_lossy().to_string();
                let content = std::fs::read(entry.path())?;
                files_map.insert(
                    format!("payload/{}", filename),
                    serde_json::json!(sha256_hex(&content)),
                );
            }
        }
    }

    let integrity = serde_json::json!({
        "version": 1,
        "algorithm": "sha256",
        "files": files_map,
        "generated_at": Utc::now().to_rfc3339(),
    });

    let file = File::create(&integrity_path)?;
    serde_json::to_writer_pretty(BufWriter::new(file), &integrity)?;

    Ok(())
}

/// Calculate SHA-256 hash as hex string
fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::encrypt::{DecryptionEngine, EncryptionEngine};
    use tempfile::TempDir;

    fn setup_test_archive() -> (TempDir, std::path::PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("site");

        // Create test file
        std::fs::write(&input_path, b"Test data for key management").unwrap();

        // Encrypt
        let mut engine = EncryptionEngine::new(1024);
        engine.add_password_slot("test-password").unwrap();
        engine
            .encrypt_file(&input_path, &output_dir, |_, _| {})
            .unwrap();

        (temp_dir, output_dir)
    }

    #[test]
    fn test_key_list() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        let result = key_list(&archive_dir).unwrap();
        assert_eq!(result.active_slots, 1);
        assert_eq!(result.slots.len(), 1);
        assert_eq!(result.slots[0].slot_type, "password");
        assert_eq!(result.slots[0].kdf, "argon2id");
    }

    #[test]
    fn test_key_add_password() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Add new password
        let slot_id = key_add_password(&archive_dir, "test-password", "new-password").unwrap();
        assert_eq!(slot_id, 1);

        // Verify it was added
        let result = key_list(&archive_dir).unwrap();
        assert_eq!(result.active_slots, 2);

        // Verify new password works
        let config = load_config(&archive_dir).unwrap();
        let dek = unwrap_dek_with_password(&config, "new-password").unwrap();
        assert!(!dek.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_key_add_recovery() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Add recovery slot
        let (slot_id, secret) = key_add_recovery(&archive_dir, "test-password").unwrap();
        assert_eq!(slot_id, 1);
        assert_eq!(secret.entropy_bits(), 256);

        // Verify it was added
        let result = key_list(&archive_dir).unwrap();
        assert_eq!(result.active_slots, 2);
        assert_eq!(result.slots[1].slot_type, "recovery");
        assert_eq!(result.slots[1].kdf, "hkdf-sha256");
    }

    #[test]
    fn test_key_add_wrong_password_fails() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        let result = key_add_password(&archive_dir, "wrong-password", "new-password");
        assert!(result.is_err());
    }

    #[test]
    fn test_key_revoke() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Add second slot
        key_add_password(&archive_dir, "test-password", "second-password").unwrap();

        // Revoke first slot using second password
        let result = key_revoke(&archive_dir, "second-password", 0).unwrap();
        assert_eq!(result.revoked_slot_id, 0);
        assert_eq!(result.remaining_slots, 1);

        // Old password should no longer work
        let config = load_config(&archive_dir).unwrap();
        assert!(unwrap_dek_with_password(&config, "test-password").is_err());

        // Second password should still work
        assert!(unwrap_dek_with_password(&config, "second-password").is_ok());
    }

    #[test]
    fn test_key_revoke_last_slot_fails() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        let result = key_revoke(&archive_dir, "test-password", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("last remaining"));
    }

    #[test]
    fn test_key_revoke_auth_slot_fails() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Add second slot
        key_add_password(&archive_dir, "test-password", "second-password").unwrap();

        // Try to revoke slot 0 using slot 0's password
        let result = key_revoke(&archive_dir, "test-password", 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("authentication"));
    }

    #[test]
    fn test_key_rotate() {
        let (temp_dir, archive_dir) = setup_test_archive();
        let decrypted_path = temp_dir.path().join("decrypted.txt");

        // Rotate keys
        let result =
            key_rotate(&archive_dir, "test-password", "new-password", false, |_| {}).unwrap();
        assert_eq!(result.slot_count, 1);
        assert!(result.recovery_secret.is_none());

        // Old password should fail
        let config = load_config(&archive_dir).unwrap();
        assert!(unwrap_dek_with_password(&config, "test-password").is_err());

        // New password should work and decrypt correctly
        let decryptor = DecryptionEngine::unlock_with_password(config, "new-password").unwrap();
        decryptor
            .decrypt_to_file(&archive_dir, &decrypted_path, |_, _| {})
            .unwrap();

        let decrypted = std::fs::read(&decrypted_path).unwrap();
        assert_eq!(decrypted, b"Test data for key management");
    }

    #[test]
    fn test_key_rotate_with_recovery() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Rotate keys with recovery
        let result =
            key_rotate(&archive_dir, "test-password", "new-password", true, |_| {}).unwrap();
        assert_eq!(result.slot_count, 2);
        assert!(result.recovery_secret.is_some());

        // Verify recovery slot
        let list_result = key_list(&archive_dir).unwrap();
        assert_eq!(list_result.slots.len(), 2);
        assert_eq!(list_result.slots[0].slot_type, "password");
        assert_eq!(list_result.slots[1].slot_type, "recovery");
    }

    #[test]
    fn test_key_add_after_revoke_no_id_collision() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        // Add slots 1 and 2
        key_add_password(&archive_dir, "test-password", "password-1").unwrap();
        key_add_password(&archive_dir, "test-password", "password-2").unwrap();

        // Now have slots [0, 1, 2]
        let list = key_list(&archive_dir).unwrap();
        assert_eq!(list.slots.len(), 3);

        // Revoke slot 1 using slot 2's password
        key_revoke(&archive_dir, "password-2", 1).unwrap();

        // Now have slots [0, 2] (gap at 1)
        let list = key_list(&archive_dir).unwrap();
        assert_eq!(list.slots.len(), 2);
        let ids: Vec<u8> = list.slots.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![0, 2]);

        // Add new slot - should get ID 3, not 2
        let new_id = key_add_password(&archive_dir, "test-password", "password-3").unwrap();
        assert_eq!(new_id, 3, "New slot should get max_id + 1, not len()");

        // Verify all passwords still work
        let config = load_config(&archive_dir).unwrap();
        assert!(unwrap_dek_with_password(&config, "test-password").is_ok());
        assert!(unwrap_dek_with_password(&config, "password-1").is_err()); // Revoked
        assert!(unwrap_dek_with_password(&config, "password-2").is_ok());
        assert!(unwrap_dek_with_password(&config, "password-3").is_ok());
    }
}
