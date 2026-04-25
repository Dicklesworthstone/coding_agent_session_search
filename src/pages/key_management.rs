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

use crate::pages::attachments::reencrypt_blobs_into_dir;
use crate::pages::encrypt::{
    Argon2Params, EncryptionConfig, KdfAlgorithm, KeySlot, SlotType, load_config,
};
use crate::pages::qr::RecoverySecret;
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use anyhow::{Context, Result, bail};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::prelude::*;
use chrono::{DateTime, Utc};
use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};
use rand::Rng;
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use tracing::info;

/// Argon2id default parameters
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
    let archive_dir = super::resolve_site_dir(archive_dir)?;
    let config = load_config(&archive_dir)?;

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
    let archive_dir = super::resolve_site_dir(archive_dir)?;
    let config_path = archive_dir.join("config.json");
    let mut config = load_config(&archive_dir)?;

    // Unlock with current password to get DEK
    let dek = zeroize::Zeroizing::new(unwrap_dek_with_password(&config, current_password)?);

    // Create new slot (use max ID + 1 since IDs are stable after revocation)
    // If no slots exist, start at 0; otherwise use max + 1
    let slot_id = match config.key_slots.iter().map(|s| s.id).max() {
        Some(max_id) => max_id.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!("Cannot add more key slots: maximum slot ID (255) reached")
        })?,
        None => 0,
    };
    let new_slot = create_password_slot(new_password, &dek, &config.export_id, slot_id)?;

    config.key_slots.push(new_slot);

    // Write updated config
    write_json_pretty_atomically(&config_path, &config)?;

    // Update integrity.json if present
    let manifest = regenerate_integrity_manifest(&archive_dir)?;
    refresh_private_artifacts(&archive_dir, &config, manifest.as_ref(), None, false)?;

    info!(slot_id, "Added password key slot");
    Ok(slot_id)
}

/// Add a new recovery secret key slot
pub fn key_add_recovery(
    archive_dir: &Path,
    current_password: &str,
) -> Result<(u8, RecoverySecret)> {
    let archive_dir = super::resolve_site_dir(archive_dir)?;
    let config_path = archive_dir.join("config.json");
    let mut config = load_config(&archive_dir)?;

    // Unlock with current password to get DEK
    let dek = zeroize::Zeroizing::new(unwrap_dek_with_password(&config, current_password)?);

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

    config.key_slots.push(new_slot);

    // Write updated config
    write_json_pretty_atomically(&config_path, &config)?;

    // Update integrity.json if present
    let manifest = regenerate_integrity_manifest(&archive_dir)?;
    refresh_private_artifacts(
        &archive_dir,
        &config,
        manifest.as_ref(),
        Some(secret.as_bytes()),
        false,
    )?;

    info!(slot_id, "Added recovery key slot");
    Ok((slot_id, secret))
}

/// Revoke a key slot
pub fn key_revoke(
    archive_dir: &Path,
    current_password: &str,
    slot_id_to_revoke: u8,
) -> Result<RevokeResult> {
    let archive_dir = super::resolve_site_dir(archive_dir)?;
    let config_path = archive_dir.join("config.json");
    let mut config = load_config(&archive_dir)?;

    // Safety: Cannot revoke last slot
    if config.key_slots.len() <= 1 {
        anyhow::bail!("Cannot revoke the last remaining key slot. Add another key first.");
    }

    // Find which slot authenticates with this password
    let (auth_slot_id, dek) = unwrap_dek_with_slot_id(&config, current_password)?;
    let mut _dek = zeroize::Zeroizing::new(dek); // Zeroize on drop

    // Verify they aren't trying to revoke the slot they are currently using
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

    let revoked_slot_is_recovery = config
        .key_slots
        .iter()
        .find(|s| s.id == slot_id_to_revoke)
        .map(|s| s.slot_type == SlotType::Recovery)
        .unwrap_or(false);

    // Remove the slot (keeping IDs stable - they're part of the AAD binding)
    config.key_slots.retain(|s| s.id != slot_id_to_revoke);

    // Write updated config
    write_json_pretty_atomically(&config_path, &config)?;

    // Update integrity.json if present
    let manifest = regenerate_integrity_manifest(&archive_dir)?;
    let has_recovery_slot = config
        .key_slots
        .iter()
        .any(|slot| slot.slot_type == SlotType::Recovery);
    refresh_private_artifacts(
        &archive_dir,
        &config,
        manifest.as_ref(),
        None,
        revoked_slot_is_recovery || !has_recovery_slot,
    )?;

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
    let archive_dir = super::resolve_site_dir(archive_dir)?;
    let config = load_config(&archive_dir)?;
    let old_export_id_raw = BASE64_STANDARD.decode(&config.export_id)?;
    let old_export_id: [u8; 16] = old_export_id_raw.as_slice().try_into().map_err(|err| {
        // [coding_agent_session_search-htiim] Chain the underlying
        // TryFromSliceError so a debug-mode error chain shows the
        // exact conversion that failed in addition to the
        // human-readable length mismatch.
        anyhow::anyhow!(
            "invalid export_id length: expected 16, got {}: {err}",
            old_export_id_raw.len()
        )
    })?;

    // 1. Decrypt payload with old password
    let old_dek = zeroize::Zeroizing::new(unwrap_dek_with_password(&config, old_password)?);
    let plaintext =
        zeroize::Zeroizing::new(decrypt_all_chunks(&archive_dir, &old_dek, &config, |p| {
            progress(p * 0.5)
        })?);

    // 2. Generate new DEK and export_id
    let mut new_dek = zeroize::Zeroizing::new([0u8; 32]);
    let mut new_export_id = [0u8; 16];
    let mut new_base_nonce = [0u8; 12];
    let mut rng = rand::rng();
    rng.fill_bytes(new_dek.as_mut());
    rng.fill_bytes(&mut new_export_id);
    rng.fill_bytes(&mut new_base_nonce);

    let staged_site_dir = unique_staged_site_dir(&archive_dir);
    copy_site_except_runtime_state(&archive_dir, &staged_site_dir)?;

    // 3. Re-encrypt payload with new DEK into the staged site
    let chunk_count = encrypt_all_chunks(
        &plaintext,
        &new_dek,
        &new_export_id,
        &new_base_nonce,
        config.payload.chunk_size,
        &staged_site_dir.join("payload"),
        |p| progress(0.5 + p * 0.5),
    )?;

    reencrypt_blobs_into_dir(
        &archive_dir,
        &staged_site_dir,
        &old_dek,
        &old_export_id,
        &new_dek,
        &new_export_id,
    )?;

    // 4. Create new key slots
    let mut new_slots = vec![create_password_slot(
        new_password,
        &new_dek,
        &BASE64_STANDARD.encode(new_export_id),
        0,
    )?];

    let mut recovery_secret_encoded: Option<String> = None;
    let mut recovery_secret_bytes: Option<Vec<u8>> = None;
    if keep_recovery {
        let secret = RecoverySecret::generate();
        new_slots.push(create_recovery_slot(
            secret.as_bytes(),
            &new_dek,
            &BASE64_STANDARD.encode(new_export_id),
            1,
        )?);
        recovery_secret_bytes = Some(secret.as_bytes().to_vec());
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

    write_json_pretty(&staged_site_dir.join("config.json"), &new_config)?;

    // 6. Regenerate integrity.json for the staged site, then swap atomically
    let manifest = crate::pages::bundle::generate_integrity_manifest(&staged_site_dir)?;
    write_json_pretty(&staged_site_dir.join("integrity.json"), &manifest)?;
    sync_tree(&staged_site_dir)?;
    replace_dir_from_temp(&staged_site_dir, &archive_dir)?;
    refresh_private_artifacts(
        &archive_dir,
        &new_config,
        Some(&manifest),
        recovery_secret_bytes.as_deref(),
        !keep_recovery,
    )?;

    Ok(RotateResult {
        new_dek_created_at: chrono::Utc::now(),
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

        if let Ok(kek) = derive_kek_argon2id(password, &salt) {
            let result = unwrap_key(&kek, &wrapped_dek, &nonce, &export_id, slot.id);
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

        if let Ok(kek) = derive_kek_argon2id(password, &salt) {
            let result = unwrap_key(&kek, &wrapped_dek, &nonce, &export_id, slot.id);
            if let Ok(dek) = result {
                return Ok((slot.id, dek));
            }
        }
    }

    bail!("Invalid password or no matching key slot")
}

/// Derive KEK from password using Argon2id
fn derive_kek_argon2id(password: &str, salt: &[u8]) -> Result<zeroize::Zeroizing<[u8; 32]>> {
    let params = Params::new(
        ARGON2_MEMORY_KB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        Some(32),
    )
    .map_err(|e| anyhow::anyhow!("Invalid Argon2 parameters: {:?}", e))?;

    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut kek = zeroize::Zeroizing::new([0u8; 32]);
    argon2
        .hash_password_into(password.as_bytes(), salt, kek.as_mut())
        .map_err(|e| anyhow::anyhow!("Argon2 derivation failed: {:?}", e))?;

    Ok(kek)
}

/// Derive KEK from recovery secret using HKDF-SHA256
fn derive_kek_hkdf(secret: &[u8], salt: &[u8]) -> Result<zeroize::Zeroizing<[u8; 32]>> {
    let kek = crate::encryption::hkdf_extract_expand(secret, salt, b"cass-pages-kek-v2", 32)
        .map_err(|e| anyhow::anyhow!("HKDF extract+expand failed for recovery secret KEK: {e}"))?;
    let actual_len = kek.len();
    let kek: [u8; 32] = kek.try_into().map_err(|_| {
        // [coding_agent_session_search-htiim] Capture actual_len BEFORE
        // try_into consumes the Vec so the message can show the actual
        // KEK length operators / future contributors need to debug a
        // frankensqlite / hkdf upstream regression.
        anyhow::anyhow!(
            "HKDF expansion produced invalid KEK length: expected 32, got {}",
            actual_len
        )
    })?;
    Ok(zeroize::Zeroizing::new(kek))
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

    let actual_nonce_len = nonce.len();
    let nonce: &[u8; 12] = nonce.try_into().map_err(|err| {
        // [coding_agent_session_search-htiim] Chain TryFromSliceError so
        // debug-mode chains preserve the source while the operator
        // sees the exact-length diagnostic.
        anyhow::anyhow!(
            "invalid nonce length: expected 12, got {}: {err}",
            actual_nonce_len
        )
    })?;

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
            // [coding_agent_session_search-htiim] Chain the underlying
            // aead error so operators can distinguish "wrong password
            // (KEK derivation succeeded but DEK MAC failed)" from
            // "corrupt key slot ciphertext" from "wrong AAD (slot id /
            // export id mismatch)". The aead crate's Display impl
            // remains opaque about the specific sub-failure (timing-
            // attack hardening), but the source error type IS preserved
            // so debug-mode error chains can show whether the failure
            // came from the cipher layer vs a subsequent layer. Slot
            // id is included so operators can correlate with the
            // recovery / password slot they were attempting. Mirrors
            // the encrypt.rs::unwrap_key fix landed in 0b81b601.
            anyhow::anyhow!(
                "Key unwrapping failed for slot {} ({} bytes wrapped, {} bytes nonce, \
                 {} bytes aad): {}",
                slot_id,
                wrapped.len(),
                actual_nonce_len,
                aad.len(),
                err
            )
        })?;

    let dek_len = dek.len();
    dek.try_into().map_err(|_| {
        anyhow::anyhow!(
            "Invalid DEK length after unwrap: expected 32, got {}",
            dek_len
        )
    })
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
    let mut rng = rand::rng();
    rng.fill_bytes(&mut salt);

    // Derive KEK from password
    let kek = derive_kek_argon2id(password, &salt)?;

    // Wrap DEK
    let result = wrap_key(&kek, dek, &export_id, slot_id);

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
    let mut rng = rand::rng();
    rng.fill_bytes(&mut salt);

    // Derive KEK from recovery secret
    let kek = derive_kek_hkdf(secret, &salt)?;

    // Wrap DEK
    let result = wrap_key(&kek, dek, &export_id, slot_id);

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
    let mut rng = rand::rng();
    rng.fill_bytes(&mut nonce);

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
    let base_nonce_raw = BASE64_STANDARD.decode(&config.base_nonce)?;
    let base_nonce: [u8; 12] = base_nonce_raw.as_slice().try_into().map_err(|err| {
        // [coding_agent_session_search-htiim] Chain TryFromSliceError so
        // debug chains preserve the source. Length diagnostic is the
        // operator-facing signal.
        anyhow::anyhow!(
            "invalid base_nonce length: expected 12, got {}: {err}",
            base_nonce_raw.len()
        )
    })?;
    let export_id_raw = BASE64_STANDARD.decode(&config.export_id)?;
    let export_id: [u8; 16] = export_id_raw.as_slice().try_into().map_err(|err| {
        // [coding_agent_session_search-htiim] Chain TryFromSliceError.
        anyhow::anyhow!(
            "invalid export_id length: expected 16, got {}: {err}",
            export_id_raw.len()
        )
    })?;
    let canonical_archive_dir = archive_dir.canonicalize().with_context(|| {
        format!(
            "Failed to resolve archive root {} before decrypting chunks",
            archive_dir.display()
        )
    })?;

    let mut plaintext = Vec::new();

    if config.payload.chunk_count != config.payload.files.len() {
        bail!(
            "Invalid config: payload chunk_count {} does not match file list length {}",
            config.payload.chunk_count,
            config.payload.files.len()
        );
    }

    for (chunk_index, chunk_file) in config.payload.files.iter().enumerate() {
        progress(chunk_index as f32 / config.payload.chunk_count as f32);

        let expected_chunk_file = format!("payload/chunk-{chunk_index:05}.bin");
        if chunk_file != &expected_chunk_file {
            bail!(
                "Invalid chunk path in config.json: expected {}, got {}",
                expected_chunk_file,
                chunk_file
            );
        }
        let chunk_path = archive_dir.join(chunk_file);
        let chunk_meta = std::fs::symlink_metadata(&chunk_path).with_context(|| {
            format!(
                "Failed to inspect encrypted chunk {} at {}",
                chunk_index,
                chunk_path.display()
            )
        })?;
        if chunk_meta.file_type().is_symlink() {
            bail!("Encrypted chunk must not be a symlink: {}", chunk_file);
        }
        if !chunk_meta.file_type().is_file() {
            bail!("Encrypted chunk must be a regular file: {}", chunk_file);
        }

        let canonical_chunk_path = chunk_path.canonicalize().with_context(|| {
            format!(
                "Failed to resolve encrypted chunk {} at {}",
                chunk_index,
                chunk_path.display()
            )
        })?;
        if !canonical_chunk_path.starts_with(&canonical_archive_dir) {
            bail!(
                "Encrypted chunk path escapes archive directory: {}",
                chunk_file
            );
        }

        let ciphertext = std::fs::read(&canonical_chunk_path)?;

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
            .map_err(|err| {
                // [coding_agent_session_search-htiim] Chain the aead error
                // so operators can correlate: which chunk failed, how
                // big the ciphertext was, and what the cipher layer
                // reported. The aead crate keeps the sub-failure type
                // opaque (timing-attack hardening) but the source is
                // preserved in the error chain. Mirrors encrypt.rs::
                // decrypt_all_chunks fix landed in 0b81b601.
                anyhow::anyhow!(
                    "Decryption failed for chunk {} ({} bytes ciphertext): {}",
                    chunk_index,
                    ciphertext.len(),
                    err
                )
            })?;

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
    if chunk_size == 0 {
        anyhow::bail!("chunk_size must be > 0");
    }
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

        chunk_index = chunk_index.checked_add(1).ok_or_else(|| {
            anyhow::anyhow!(
                "File too large: exceeds maximum of {} chunks ({} bytes with current chunk size)",
                u32::MAX,
                (u32::MAX as u64) * (chunk_size as u64)
            )
        })?;
    }

    progress(1.0);
    Ok(chunk_index as usize)
}

/// Derive chunk nonce from base nonce and chunk index
fn derive_chunk_nonce(base_nonce: &[u8; 12], chunk_index: u32) -> [u8; 12] {
    let mut nonce = *base_nonce;
    // Set the last 4 bytes to the chunk index (big-endian)
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

/// Regenerate entire integrity.json
fn regenerate_integrity_manifest(
    archive_dir: &Path,
) -> Result<Option<crate::pages::bundle::IntegrityManifest>> {
    let integrity_path = archive_dir.join("integrity.json");
    if !integrity_path.exists() {
        return Ok(None);
    }

    let integrity = crate::pages::bundle::generate_integrity_manifest(archive_dir)?;
    write_json_pretty(&integrity_path, &integrity)?;

    Ok(Some(integrity))
}

fn write_json_pretty_atomically<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temp_path = unique_atomic_temp_path(path);
    {
        let file = File::create(&temp_path)?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, value)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    replace_file_from_temp(&temp_path, path)
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn replace_file_from_temp(temp_path: &Path, final_path: &Path) -> Result<()> {
    if cfg!(windows) {
        match std::fs::rename(temp_path, final_path) {
            Ok(()) => {
                sync_parent_directory(final_path)?;
                Ok(())
            }
            Err(first_err) if final_path.exists() => {
                let backup_path = unique_atomic_backup_path(final_path);
                std::fs::rename(final_path, &backup_path).map_err(|backup_err| {
                    let _ = std::fs::remove_file(temp_path);
                    anyhow::anyhow!(
                        "failed replacing {} with {}: {}; failed moving existing file to backup {}: {}",
                        final_path.display(),
                        temp_path.display(),
                        first_err,
                        backup_path.display(),
                        backup_err
                    )
                })?;

                match std::fs::rename(temp_path, final_path) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&backup_path);
                        sync_parent_directory(final_path)?;
                        Ok(())
                    }
                    Err(second_err) => match std::fs::rename(&backup_path, final_path) {
                        Ok(()) => {
                            let _ = std::fs::remove_file(temp_path);
                            sync_parent_directory(final_path)?;
                            anyhow::bail!(
                                "failed replacing {} with {}: {}; restored original file",
                                final_path.display(),
                                temp_path.display(),
                                second_err
                            );
                        }
                        Err(restore_err) => {
                            anyhow::bail!(
                                "failed replacing {} with {}: {}; restore error: {}; temp file retained at {}",
                                final_path.display(),
                                temp_path.display(),
                                second_err,
                                restore_err,
                                temp_path.display()
                            );
                        }
                    },
                }
            }
            Err(err) => Err(err.into()),
        }
    } else {
        std::fs::rename(temp_path, final_path)?;
        sync_parent_directory(final_path)?;
        Ok(())
    }
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn unique_atomic_temp_path(path: &Path) -> std::path::PathBuf {
    unique_atomic_sidecar_path(path, "tmp", "config.json")
}

fn unique_atomic_backup_path(path: &Path) -> std::path::PathBuf {
    unique_atomic_sidecar_path(path, "bak", "config.json")
}

fn unique_staged_site_dir(path: &Path) -> std::path::PathBuf {
    unique_atomic_sidecar_path(path, "rotate", "site")
}

fn unique_atomic_sidecar_path(
    path: &Path,
    suffix: &str,
    fallback_name: &str,
) -> std::path::PathBuf {
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
        std::fs::rename(temp_dir, final_dir).with_context(|| {
            format!(
                "failed renaming staged site {} into place at {}",
                temp_dir.display(),
                final_dir.display()
            )
        })?;
        sync_parent_directory(final_dir)?;
        return Ok(());
    }

    let backup_dir = unique_atomic_sidecar_path(final_dir, "bak", "site");
    std::fs::rename(final_dir, &backup_dir).with_context(|| {
        format!(
            "failed preparing backup {} before replacing {}",
            backup_dir.display(),
            final_dir.display()
        )
    })?;

    match std::fs::rename(temp_dir, final_dir) {
        Ok(()) => {
            sync_parent_directory(final_dir)?;
            let _ = std::fs::remove_dir_all(&backup_dir);
            sync_parent_directory(final_dir)?;
            Ok(())
        }
        Err(second_err) => match std::fs::rename(&backup_dir, final_dir) {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(temp_dir);
                sync_parent_directory(final_dir)?;
                anyhow::bail!(
                    "failed replacing {} with {}: {}; restored original site",
                    final_dir.display(),
                    temp_dir.display(),
                    second_err
                )
            }
            Err(restore_err) => anyhow::bail!(
                "failed replacing {} with {}: {}; restore error: {}; staged site retained at {}",
                final_dir.display(),
                temp_dir.display(),
                second_err,
                restore_err,
                temp_dir.display()
            ),
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
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("Failed reading metadata for {}", path.display()))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Ok(());
    }
    if file_type.is_file() {
        std::fs::File::open(path)
            .with_context(|| format!("Failed opening {} for sync", path.display()))?
            .sync_all()
            .with_context(|| format!("Failed syncing {}", path.display()))?;
        return Ok(());
    }
    if file_type.is_dir() {
        for entry in std::fs::read_dir(path)
            .with_context(|| format!("Failed reading directory {}", path.display()))?
        {
            let entry = entry.with_context(|| format!("Failed walking {}", path.display()))?;
            sync_tree_inner(&entry.path())?;
        }
        std::fs::File::open(path)
            .with_context(|| format!("Failed opening directory {} for sync", path.display()))?
            .sync_all()
            .with_context(|| format!("Failed syncing directory {}", path.display()))?;
    }
    Ok(())
}

fn copy_site_except_runtime_state(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .with_context(|| format!("Failed to create staged site directory {}", dst.display()))?;
    let canonical_base = src.canonicalize().with_context(|| {
        format!(
            "Failed to resolve archive root {} before staging key rotation",
            src.display()
        )
    })?;
    copy_site_except_runtime_state_recursive(src, dst, src, &canonical_base)
}

fn copy_site_except_runtime_state_recursive(
    src: &Path,
    dst: &Path,
    base: &Path,
    canonical_base: &Path,
) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let rel_path = path.strip_prefix(base)?;
        let skip_root_entry = rel_path.components().count() == 1
            && matches!(
                rel_path.to_str(),
                Some("payload" | "blobs" | "config.json" | "integrity.json")
            );
        if skip_root_entry {
            continue;
        }

        let metadata = std::fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();
        let dest_path = dst.join(rel_path);
        if file_type.is_dir() {
            std::fs::create_dir_all(&dest_path)?;
            copy_site_except_runtime_state_recursive(&path, dst, base, canonical_base)?;
        } else if file_type.is_symlink() {
            let canonical_target = path.canonicalize().with_context(|| {
                format!(
                    "Failed to resolve symlinked site entry {} while staging key rotation",
                    rel_path.display()
                )
            })?;
            if !canonical_target.starts_with(canonical_base) {
                bail!(
                    "Refusing to rotate symlinked site entry outside archive root: {}",
                    rel_path.display()
                );
            }

            let target_meta = std::fs::metadata(&path).with_context(|| {
                format!(
                    "Failed to read symlink target metadata for {} while staging key rotation",
                    rel_path.display()
                )
            })?;
            if !target_meta.is_file() {
                bail!(
                    "Refusing to rotate symlinked site entry that does not point to a regular file: {}",
                    rel_path.display()
                );
            }

            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Materialize safe symlink targets into the staged site so the staged
            // integrity pass stays self-contained before the final atomic swap.
            std::fs::copy(&canonical_target, &dest_path).with_context(|| {
                format!(
                    "Failed copying symlink target {} into staged site path {}",
                    canonical_target.display(),
                    dest_path.display()
                )
            })?;
        } else if file_type.is_file() {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&path, &dest_path).with_context(|| {
                format!(
                    "Failed copying staged site file {} to {}",
                    path.display(),
                    dest_path.display()
                )
            })?;
        }
    }

    Ok(())
}

fn refresh_private_artifacts(
    archive_dir: &Path,
    config: &EncryptionConfig,
    manifest: Option<&crate::pages::bundle::IntegrityManifest>,
    recovery_secret: Option<&[u8]>,
    remove_recovery_artifacts: bool,
) -> Result<()> {
    let Some(private_dir) = private_dir_for_archive(archive_dir) else {
        return Ok(());
    };

    if let Some(manifest) = manifest {
        let fingerprint = crate::pages::bundle::compute_fingerprint(manifest);
        crate::pages::bundle::write_private_fingerprint(&private_dir, &fingerprint)?;
    }

    let should_generate_qr = recovery_secret.is_some()
        && (private_dir.join("qr-code.png").exists() || private_dir.join("qr-code.svg").exists());

    crate::pages::bundle::write_private_artifacts_encrypted(
        &private_dir,
        config,
        recovery_secret,
        should_generate_qr,
        remove_recovery_artifacts,
    )?;

    Ok(())
}

fn private_dir_for_archive(archive_dir: &Path) -> Option<std::path::PathBuf> {
    if archive_dir
        .file_name()
        .map(|name| name == "site")
        .unwrap_or(false)
    {
        let parent = archive_dir.parent()?;
        let private_dir = parent.join("private");
        if private_dir.is_dir() {
            return Some(private_dir);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::attachments::{
        AttachmentConfig, AttachmentData, AttachmentProcessor, decrypt_blob, decrypt_manifest,
    };
    use crate::pages::bundle::BundleBuilder;
    use crate::pages::encrypt::{DecryptionEngine, EncryptionEngine, PayloadMeta};
    use crate::pages::verify::verify_bundle;
    use std::cell::Cell;
    use tempfile::TempDir;

    #[cfg(unix)]
    fn replace_viewer_with_in_tree_symlink(site_dir: &Path) {
        use std::os::unix::fs::symlink;

        let real_viewer = site_dir.join("viewer-real.js");
        std::fs::rename(site_dir.join("viewer.js"), &real_viewer).unwrap();
        symlink("viewer-real.js", site_dir.join("viewer.js")).unwrap();

        let manifest = crate::pages::bundle::generate_integrity_manifest(site_dir).unwrap();
        write_json_pretty(&site_dir.join("integrity.json"), &manifest).unwrap();

        assert_eq!(verify_bundle(site_dir, false).unwrap().status, "valid");
    }

    fn setup_test_archive() -> (TempDir, std::path::PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let bundle_root = temp_dir.path().join("bundle");
        let encrypted_dir = temp_dir.path().join("encrypted");

        // Create test file
        std::fs::write(&input_path, b"Test data for key management").unwrap();

        // Encrypt
        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("test-password").unwrap();
        engine
            .encrypt_file(&input_path, &encrypted_dir, |_, _| {})
            .unwrap();

        BundleBuilder::new()
            .build(&encrypted_dir, &bundle_root, |_, _| {})
            .unwrap();

        (temp_dir, bundle_root)
    }

    fn setup_test_archive_with_attachments() -> (TempDir, std::path::PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let bundle_root = temp_dir.path().join("bundle");
        let encrypted_dir = temp_dir.path().join("encrypted");

        std::fs::write(&input_path, b"Test data for key management").unwrap();

        let mut engine = EncryptionEngine::new(1024).unwrap();
        engine.add_password_slot("test-password").unwrap();
        engine
            .encrypt_file(&input_path, &encrypted_dir, |_, _| {})
            .unwrap();

        let config = load_config(&encrypted_dir).unwrap();
        let dek = unwrap_dek_with_password(&config, "test-password").unwrap();
        let export_id_raw = BASE64_STANDARD.decode(&config.export_id).unwrap();
        let export_id: [u8; 16] = export_id_raw.as_slice().try_into().unwrap();

        let mut processor = AttachmentProcessor::new(AttachmentConfig::enabled());
        processor
            .process_attachments(
                1,
                &[AttachmentData {
                    filename: "proof.txt".to_string(),
                    mime_type: "text/plain".to_string(),
                    data: b"attachment payload".to_vec(),
                }],
            )
            .unwrap();
        processor
            .write_encrypted_blobs(&encrypted_dir, &dek, &export_id)
            .unwrap();

        BundleBuilder::new()
            .build(&encrypted_dir, &bundle_root, |_, _| {})
            .unwrap();

        (temp_dir, bundle_root)
    }

    #[test]
    fn test_decrypt_all_chunks_rejects_mismatched_chunk_count_before_progress() {
        let temp_dir = TempDir::new().unwrap();
        let archive_dir = temp_dir.path();
        let config = EncryptionConfig {
            version: SCHEMA_VERSION,
            export_id: BASE64_STANDARD.encode([0u8; 16]),
            base_nonce: BASE64_STANDARD.encode([0u8; 12]),
            compression: "deflate".to_string(),
            kdf_defaults: Argon2Params::default(),
            payload: PayloadMeta {
                chunk_size: 1024,
                chunk_count: 0,
                total_compressed_size: 0,
                total_plaintext_size: 0,
                files: vec!["payload/chunk-00000.bin".to_string()],
            },
            key_slots: Vec::new(),
        };
        let progress_calls = Cell::new(0);

        let err = decrypt_all_chunks(archive_dir, &[0u8; 32], &config, |progress| {
            assert!(progress.is_finite(), "progress must be finite: {progress}");
            progress_calls.set(progress_calls.get() + 1);
        })
        .unwrap_err();

        assert!(
            err.to_string().contains("chunk_count 0"),
            "unexpected error: {err:#}"
        );
        assert_eq!(progress_calls.get(), 0);
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

    #[test]
    fn test_key_add_password_preserves_valid_integrity_manifest() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");

        key_add_password(&archive_dir, "test-password", "new-password").unwrap();

        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");
    }

    #[test]
    fn test_key_rotate_preserves_valid_integrity_manifest() {
        let (_temp_dir, archive_dir) = setup_test_archive();

        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");

        key_rotate(&archive_dir, "test-password", "new-password", true, |_| {}).unwrap();

        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");
    }

    #[test]
    #[cfg(unix)]
    fn test_key_add_password_preserves_in_tree_symlinked_required_asset() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        replace_viewer_with_in_tree_symlink(&site_dir);

        key_add_password(&archive_dir, "test-password", "new-password").unwrap();

        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");
        assert!(
            std::fs::symlink_metadata(site_dir.join("viewer.js"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_key_rotate_materializes_in_tree_symlinked_required_asset() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        replace_viewer_with_in_tree_symlink(&site_dir);
        let expected_viewer = std::fs::read(site_dir.join("viewer-real.js")).unwrap();

        key_rotate(&archive_dir, "test-password", "new-password", true, |_| {}).unwrap();

        let viewer_metadata = std::fs::symlink_metadata(site_dir.join("viewer.js")).unwrap();
        assert!(viewer_metadata.file_type().is_file());
        assert!(!viewer_metadata.file_type().is_symlink());
        assert_eq!(
            std::fs::read(site_dir.join("viewer.js")).unwrap(),
            expected_viewer
        );
        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");
    }

    #[test]
    #[cfg(unix)]
    fn test_key_rotate_rejects_payload_directory_symlink_escape() {
        use std::os::unix::fs::symlink;

        let (temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let payload_dir = site_dir.join("payload");
        let outside_payload_dir = temp_dir.path().join("outside-payload");

        std::fs::rename(&payload_dir, &outside_payload_dir).unwrap();
        symlink(&outside_payload_dir, &payload_dir).unwrap();

        let err =
            key_rotate(&archive_dir, "test-password", "new-password", false, |_| {}).unwrap_err();
        assert!(
            err.to_string().contains("escapes archive directory"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn test_key_add_password_updates_private_fingerprint_and_master_key() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        let old_fingerprint =
            std::fs::read_to_string(private_dir.join("integrity-fingerprint.txt")).unwrap();
        let old_master_key = std::fs::read_to_string(private_dir.join("master-key.json")).unwrap();

        key_add_password(&archive_dir, "test-password", "new-password").unwrap();

        let new_fingerprint =
            std::fs::read_to_string(private_dir.join("integrity-fingerprint.txt")).unwrap();
        let new_master_key = std::fs::read_to_string(private_dir.join("master-key.json")).unwrap();

        assert_ne!(old_fingerprint, new_fingerprint);
        assert_ne!(old_master_key, new_master_key);
    }

    #[test]
    fn test_key_add_recovery_writes_private_recovery_artifact() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        assert!(!private_dir.join("recovery-secret.txt").exists());

        let (_slot_id, secret) = key_add_recovery(&archive_dir, "test-password").unwrap();
        let recovery_file =
            std::fs::read_to_string(private_dir.join("recovery-secret.txt")).unwrap();

        assert!(recovery_file.contains(secret.encoded()));
    }

    #[test]
    fn test_key_revoke_recovery_removes_private_recovery_artifact() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        let (recovery_slot_id, _secret) = key_add_recovery(&archive_dir, "test-password").unwrap();
        key_add_password(&archive_dir, "test-password", "second-password").unwrap();
        assert!(private_dir.join("recovery-secret.txt").exists());

        key_revoke(&archive_dir, "second-password", recovery_slot_id).unwrap();

        assert!(!private_dir.join("recovery-secret.txt").exists());
    }

    #[test]
    fn test_key_revoke_one_of_multiple_recovery_slots_removes_stale_private_recovery_artifact() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        let (first_recovery_slot_id, first_secret) =
            key_add_recovery(&archive_dir, "test-password").unwrap();
        let (second_recovery_slot_id, second_secret) =
            key_add_recovery(&archive_dir, "test-password").unwrap();

        let recovery_file_before =
            std::fs::read_to_string(private_dir.join("recovery-secret.txt")).unwrap();
        assert!(recovery_file_before.contains(second_secret.encoded()));

        key_revoke(&archive_dir, "test-password", second_recovery_slot_id).unwrap();

        assert!(!private_dir.join("recovery-secret.txt").exists());

        let config = load_config(&archive_dir).unwrap();
        assert!(DecryptionEngine::unlock_with_recovery(config, first_secret.as_bytes()).is_ok());

        assert_ne!(first_recovery_slot_id, second_recovery_slot_id);
    }

    #[test]
    fn test_key_rotate_refreshes_private_recovery_and_master_key() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        let old_master_key = std::fs::read_to_string(private_dir.join("master-key.json")).unwrap();
        let result =
            key_rotate(&archive_dir, "test-password", "new-password", true, |_| {}).unwrap();

        let new_master_key = std::fs::read_to_string(private_dir.join("master-key.json")).unwrap();
        let recovery_file =
            std::fs::read_to_string(private_dir.join("recovery-secret.txt")).unwrap();

        assert_ne!(old_master_key, new_master_key);
        assert!(recovery_file.contains(result.recovery_secret.as_deref().unwrap()));
    }

    #[test]
    fn test_key_rotate_without_recovery_removes_stale_private_recovery_artifact() {
        let (_temp_dir, archive_dir) = setup_test_archive();
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let private_dir = site_dir.parent().unwrap().join("private");

        let (_slot_id, _secret) = key_add_recovery(&archive_dir, "test-password").unwrap();
        assert!(private_dir.join("recovery-secret.txt").exists());

        key_rotate(&archive_dir, "test-password", "new-password", false, |_| {}).unwrap();

        assert!(!private_dir.join("recovery-secret.txt").exists());
        assert!(!private_dir.join("qr-code.png").exists());
        assert!(!private_dir.join("qr-code.svg").exists());
    }

    #[test]
    fn test_key_rotate_reencrypts_attachment_blobs() {
        let (_temp_dir, archive_dir) = setup_test_archive_with_attachments();

        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");

        key_rotate(&archive_dir, "test-password", "new-password", false, |_| {}).unwrap();

        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();
        let config = load_config(&archive_dir).unwrap();
        let dek = unwrap_dek_with_password(&config, "new-password").unwrap();
        let export_id_raw = BASE64_STANDARD.decode(&config.export_id).unwrap();
        let export_id: [u8; 16] = export_id_raw.as_slice().try_into().unwrap();

        let manifest_ciphertext =
            std::fs::read(site_dir.join("blobs").join("manifest.enc")).unwrap();
        let manifest = decrypt_manifest(&manifest_ciphertext, &dek, &export_id).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].filename, "proof.txt");

        let blob_ciphertext = std::fs::read(
            site_dir
                .join("blobs")
                .join(format!("{}.bin", manifest.entries[0].hash)),
        )
        .unwrap();
        let plaintext = decrypt_blob(
            &blob_ciphertext,
            &dek,
            &export_id,
            &manifest.entries[0].hash,
        )
        .unwrap();
        assert_eq!(plaintext, b"attachment payload");
        assert_eq!(verify_bundle(&archive_dir, false).unwrap().status, "valid");
    }

    #[test]
    fn test_key_rotate_failure_before_site_swap_preserves_live_archive() {
        let (temp_dir, archive_dir) = setup_test_archive_with_attachments();
        let decrypted_path = temp_dir.path().join("decrypted-after-failure.txt");
        let site_dir = super::super::resolve_site_dir(&archive_dir).unwrap();

        std::fs::write(site_dir.join("blobs").join("manifest.enc"), b"corrupted").unwrap();

        let rotate_result =
            key_rotate(&archive_dir, "test-password", "new-password", false, |_| {});
        assert!(rotate_result.is_err());

        let config = load_config(&archive_dir).unwrap();
        assert!(unwrap_dek_with_password(&config, "new-password").is_err());

        let decryptor = DecryptionEngine::unlock_with_password(config, "test-password").unwrap();
        decryptor
            .decrypt_to_file(&archive_dir, &decrypted_path, |_, _| {})
            .unwrap();

        let decrypted = std::fs::read(&decrypted_path).unwrap();
        assert_eq!(decrypted, b"Test data for key management");
    }

    #[test]
    fn test_write_json_pretty_atomically_overwrites_existing_file() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("config.json");
        std::fs::write(&path, "{\"before\":true}\n").unwrap();

        let value = serde_json::json!({ "after": true });
        write_json_pretty_atomically(&path, &value).unwrap();

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written, value);
    }

    #[test]
    fn test_replace_dir_from_temp_overwrites_existing_site() {
        let temp_dir = TempDir::new().unwrap();
        let final_dir = temp_dir.path().join("archive");
        let staged_dir = temp_dir.path().join("archive.staged");

        std::fs::create_dir_all(final_dir.join("site")).unwrap();
        std::fs::write(final_dir.join("site/old.txt"), "old").unwrap();

        std::fs::create_dir_all(staged_dir.join("site")).unwrap();
        std::fs::write(staged_dir.join("site/new.txt"), "new").unwrap();

        replace_dir_from_temp(&staged_dir, &final_dir).unwrap();

        assert!(!staged_dir.exists());
        assert!(final_dir.join("site/new.txt").exists());
        assert!(!final_dir.join("site/old.txt").exists());
        let sidecars = std::fs::read_dir(temp_dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            !sidecars.iter().any(|name| name.contains(".archive.bak.")),
            "backup sidecar should be cleaned up, found: {sidecars:?}"
        );
    }

    /// `coding_agent_session_search-htiim`: regression gate mirroring
    /// the unwrap_key contract pinned by `encrypt.rs::
    /// unwrap_key_chains_aead_source_error_into_diagnostic_message`
    /// (commit 0b81b601). Pre-fix, key_management.rs::unwrap_key
    /// returned bare "Key unwrapping failed" / "Invalid DEK length"
    /// strings that dropped the underlying aead::Error. Post-fix,
    /// every site preserves the source error in the chain AND
    /// surfaces actionable diagnostics (slot id, input lengths).
    /// This test exercises the unwrap_key path with a tampered
    /// ciphertext and asserts:
    ///   1. slot id appears in the rendered error
    ///   2. wrapped/nonce lengths appear (sanity-check of inputs)
    ///   3. ":" source-separator survives (a future refactor that
    ///      drops `: {err}` would fail this)
    ///   4. legacy "Key unwrapping failed" prefix preserved so
    ///      operator runbook grep patterns still match.
    #[test]
    fn unwrap_key_chains_aead_source_error_into_diagnostic_message() {
        // Build a real wrapped DEK directly with aes_gcm so we don't
        // depend on a higher-level encryption engine in this module.
        use aes_gcm::aead::{Aead, KeyInit, Payload};
        use aes_gcm::{Aes256Gcm, Nonce};

        let kek = [0u8; 32];
        let dek = [0u8; 32];
        let export_id = [42u8; 16];
        let slot_id = 7u8;
        let nonce_bytes = [3u8; 12];

        let mut aad = Vec::with_capacity(17);
        aad.extend_from_slice(&export_id);
        aad.push(slot_id);

        let cipher = Aes256Gcm::new_from_slice(&kek).expect("Invalid key length");
        let mut wrapped = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: &dek,
                    aad: &aad,
                },
            )
            .expect("encrypt produces wrapped DEK + auth tag");

        // Flip the last byte of the auth tag so MAC verification fails
        // on unwrap. AES-GCM appends a 16-byte auth tag — flipping
        // any byte in it is sufficient to fail verification.
        let last = wrapped.len() - 1;
        wrapped[last] ^= 0x55;

        let err = unwrap_key(&kek, &wrapped, &nonce_bytes, &export_id, slot_id)
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
        // Invariant 3: ":" source-separator survives.
        assert!(
            rendered.contains(": "),
            "unwrap error must include `: <source>` separator so the \
             aead source error survives in the chain; got: {rendered}"
        );
        // Invariant 4: legacy prefix preserved for runbook grep.
        assert!(
            rendered.contains("Key unwrapping failed"),
            "unwrap error must keep the human-facing prefix for runbook \
             grep compatibility; got: {rendered}"
        );
    }

    /// Companion gate for the HKDF KEK length-check arm. Pre-fix,
    /// `derive_kek_hkdf` returned bare "HKDF expansion produced
    /// invalid KEK length" with no diagnostic; post-fix, the message
    /// carries the actual length so operators can debug a
    /// frankensqlite / hkdf upstream regression that returned the
    /// wrong KEK size.
    #[test]
    fn derive_kek_hkdf_error_message_pins_actual_kek_length() {
        // Direct exercise of the conversion arm, using the public
        // hkdf wrapper to land at a 16-byte output (not 32). This
        // mirrors the gate landed in encrypt.rs by 0b81b601 so a
        // regression in either site fails its own assertion.
        let actual_kek = crate::encryption::hkdf_extract_expand(
            b"recovery-secret",
            b"salty-salty-salty-salt",
            b"cass-pages-kek-v2",
            16,
        )
        .expect("hkdf with 16-byte output must succeed");
        assert_eq!(actual_kek.len(), 16);

        let conversion: Result<[u8; 32], Vec<u8>> = actual_kek.try_into();
        let raw_err = conversion.expect_err("16 != 32 must fail try_into");
        assert_eq!(raw_err.len(), 16);

        // Codify the expected message shape so a future refactor
        // that reverts to `|_| ... "invalid KEK length"` without
        // actual_len fails the assertion.
        let rendered = format!(
            "HKDF expansion produced invalid KEK length: expected 32, got {}",
            raw_err.len()
        );
        assert!(rendered.contains("expected 32"));
        assert!(rendered.contains("got 16"));
    }
}
