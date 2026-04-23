//! Fuzz target for chunked encryption/decryption roundtrips.
//!
//! Tests the full encrypt -> decrypt cycle with arbitrary data
//! to ensure consistency and detect any edge cases.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use tempfile::TempDir;

use coding_agent_search::pages::encrypt::{load_config, DecryptionEngine, EncryptionEngine};

/// Fuzzer input for chunked encryption.
#[derive(Arbitrary, Debug)]
struct ChunkedInput {
    /// Plaintext data to encrypt
    plaintext: Vec<u8>,
    /// Password for encryption
    password: String,
    /// Chunk size (will be clamped to reasonable range)
    chunk_size: usize,
    /// Whether to add recovery slot
    add_recovery: bool,
    /// Recovery secret bytes
    recovery_secret: Vec<u8>,
}

fuzz_target!(|input: ChunkedInput| {
    // Skip empty plaintexts and passwords
    if input.plaintext.is_empty() || input.password.is_empty() {
        return;
    }

    // Clamp chunk size to reasonable range (1KB to 1MB)
    let chunk_size = input.chunk_size.clamp(1024, 1024 * 1024);

    // Limit plaintext size to avoid OOM (max 1MB for fuzzing)
    let plaintext = if input.plaintext.len() > 1024 * 1024 {
        &input.plaintext[..1024 * 1024]
    } else {
        &input.plaintext
    };

    // Create temp directory for encryption artifacts
    let temp_dir = match TempDir::new() {
        Ok(dir) => dir,
        Err(_) => return,
    };

    let input_path = temp_dir.path().join("input.bin");
    let encrypt_dir = temp_dir.path().join("encrypted");
    let decrypt_path = temp_dir.path().join("decrypted.bin");

    // Write input file
    if std::fs::write(&input_path, plaintext).is_err() {
        return;
    }

    // Create encryption engine
    let Ok(mut engine) = EncryptionEngine::new(chunk_size) else {
        return;
    };

    // Add password slot
    if engine.add_password_slot(&input.password).is_err() {
        return;
    }

    // Optionally add recovery slot
    if input.add_recovery && !input.recovery_secret.is_empty() {
        let _ = engine.add_recovery_slot(&input.recovery_secret);
    }

    // Encrypt
    let config = match engine.encrypt_file(&input_path, &encrypt_dir, |_, _| {}) {
        Ok(c) => c,
        Err(_) => return,
    };

    // Load config and decrypt with password
    let decryptor = match DecryptionEngine::unlock_with_password(config.clone(), &input.password) {
        Ok(d) => d,
        Err(_) => return,
    };

    // Decrypt
    assert!(
        decryptor
            .decrypt_to_file(&encrypt_dir, &decrypt_path, |_, _| {})
            .is_ok(),
        "Decryption failed after successful encryption"
    );

    // Verify roundtrip
    let decrypted = match std::fs::read(&decrypt_path) {
        Ok(d) => d,
        Err(_) => return,
    };

    assert_eq!(
        decrypted,
        plaintext,
        "Roundtrip mismatch! Original len: {}, Decrypted len: {}",
        plaintext.len(),
        decrypted.len()
    );

    // If recovery slot was added, test recovery decryption too
    if input.add_recovery && !input.recovery_secret.is_empty() {
        let config2 = match load_config(&encrypt_dir) {
            Ok(c) => c,
            Err(_) => return,
        };

        if let Ok(recovery_decryptor) =
            DecryptionEngine::unlock_with_recovery(config2, &input.recovery_secret)
        {
            let recovery_decrypt_path = temp_dir.path().join("recovery_decrypted.bin");
            if recovery_decryptor
                .decrypt_to_file(&encrypt_dir, &recovery_decrypt_path, |_, _| {})
                .is_ok()
            {
                let recovery_decrypted = std::fs::read(&recovery_decrypt_path).unwrap_or_default();
                assert_eq!(
                    recovery_decrypted, plaintext,
                    "Recovery roundtrip mismatch!"
                );
            }
        }
    }
});
