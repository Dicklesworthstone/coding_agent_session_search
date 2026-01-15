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
use argon2::{Algorithm, Argon2, Params, Version, password_hash::SaltString};
use base64::prelude::*;
use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};
use hkdf::Hkdf;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Default chunk size for streaming encryption (8 MiB)
pub const DEFAULT_CHUNK_SIZE: usize = 8 * 1024 * 1024;

/// Maximum chunk size (32 MiB)
pub const MAX_CHUNK_SIZE: usize = 32 * 1024 * 1024;

/// Argon2id parameters (from Phase 2 spec)
const ARGON2_MEMORY_KB: u32 = 65536; // 64 MB
const ARGON2_ITERATIONS: u32 = 3;
const ARGON2_PARALLELISM: u32 = 4;

/// Encryption schema version
const SCHEMA_VERSION: u8 = 2;

/// Secret key material that zeros on drop
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SecretKey([u8; 32]);

impl SecretKey {
    pub fn random() -> Self {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
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
pub struct PayloadMeta {
    pub chunk_size: usize,
    pub chunk_count: usize,
    pub total_compressed_size: u64,
    pub total_plaintext_size: u64,
    pub files: Vec<String>,
}

/// Full config.json structure
#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct EncryptionEngine {
    dek: SecretKey,
    export_id: [u8; 16],
    base_nonce: [u8; 12],
    chunk_size: usize,
    key_slots: Vec<KeySlot>,
}

impl Default for EncryptionEngine {
    fn default() -> Self {
        Self::new(DEFAULT_CHUNK_SIZE)
    }
}

impl EncryptionEngine {
    /// Create new encryption engine with random DEK
    pub fn new(chunk_size: usize) -> Self {
        let chunk_size = chunk_size.min(MAX_CHUNK_SIZE);
        let mut export_id = [0u8; 16];
        let mut base_nonce = [0u8; 12];
        OsRng.fill_bytes(&mut export_id);
        OsRng.fill_bytes(&mut base_nonce);

        Self {
            dek: SecretKey::random(),
            export_id,
            base_nonce,
            chunk_size,
            key_slots: Vec::new(),
        }
    }

    /// Add a password-based key slot using Argon2id
    pub fn add_password_slot(&mut self, password: &str) -> Result<u8> {
        let slot_id = self.key_slots.len() as u8;

        // Generate salt
        let salt = SaltString::generate(&mut OsRng);
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
        let slot_id = self.key_slots.len() as u8;

        // Generate salt
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);

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
        serde_json::to_writer_pretty(BufWriter::new(config_file), &config)?;

        Ok(config)
    }
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
        let encrypted_dir = encrypted_dir.as_ref();
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
                .map_err(|_| anyhow::anyhow!("Decryption failed for chunk {}", chunk_index))?;

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
    let hkdf = Hkdf::<Sha256>::new(Some(salt), secret);
    let mut kek = [0u8; 32];
    hkdf.expand(b"cass-pages-kek-v2", &mut kek)
        .map_err(|_| anyhow::anyhow!("HKDF expansion failed"))?;
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
    OsRng.fill_bytes(&mut nonce);

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
    let config_path = dir.as_ref().join("config.json");
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
        let mut engine = EncryptionEngine::new(1024); // Small chunks for testing
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
        let mut engine = EncryptionEngine::new(1024);
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
    fn test_tampered_chunk_fails() {
        let temp_dir = TempDir::new().unwrap();
        let input_path = temp_dir.path().join("input.txt");
        let output_dir = temp_dir.path().join("encrypted");
        let decrypted_path = temp_dir.path().join("decrypted.txt");

        std::fs::write(&input_path, b"Test data for tampering").unwrap();

        let mut engine = EncryptionEngine::new(1024);
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
}
