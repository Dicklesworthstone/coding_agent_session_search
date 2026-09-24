//! Fuzz target for decryption code paths.
//!
//! Tests that the decryption engine handles malformed inputs gracefully
//! without panicking or causing memory safety issues.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use coding_agent_search::pages::encrypt::{
    Argon2Params, DecryptionEngine, EncryptionConfig, KdfAlgorithm, KeySlot, PayloadMeta,
    SlotType,
};

/// Fuzzer input representing arbitrary decryption parameters.
#[derive(Arbitrary, Debug)]
struct DecryptInput {
    /// Password to try
    password: String,
    /// Salt bytes (will be base64 encoded)
    salt: Vec<u8>,
    /// Wrapped DEK bytes (will be base64 encoded)
    wrapped_dek: Vec<u8>,
    /// Nonce bytes (will be base64 encoded)
    nonce: Vec<u8>,
    /// Export ID (will be base64 encoded)
    export_id: Vec<u8>,
    /// Base nonce (will be base64 encoded)
    base_nonce: Vec<u8>,
    /// KDF memory parameter (clamped to reasonable range)
    argon2_memory: u32,
    /// KDF iterations (clamped)
    argon2_iterations: u32,
    /// KDF parallelism (clamped)
    argon2_parallelism: u32,
    /// Schema version
    version: u8,
}

fn encode_base64(data: &[u8]) -> String {
    use base64::prelude::*;
    BASE64_STANDARD.encode(data)
}

fuzz_target!(|input: DecryptInput| {
    // Clamp KDF params to avoid OOM while still exercising parameter handling
    let memory_kb = (input.argon2_memory % 8192).max(1024); // 1-8 MB
    let iterations = (input.argon2_iterations % 3).max(1); // 1-3
    let parallelism = (input.argon2_parallelism % 4).max(1); // 1-4

    // Build a synthetic EncryptionConfig with fuzzed parameters
    let key_slot = KeySlot {
        id: 0,
        slot_type: SlotType::Password,
        kdf: KdfAlgorithm::Argon2id,
        salt: encode_base64(&input.salt),
        wrapped_dek: encode_base64(&input.wrapped_dek),
        nonce: encode_base64(&input.nonce),
        argon2_params: Some(Argon2Params {
            memory_kb,
            iterations,
            parallelism,
        }),
    };

    let config = EncryptionConfig {
        version: input.version,
        export_id: encode_base64(&input.export_id),
        base_nonce: encode_base64(&input.base_nonce),
        compression: "deflate".to_string(),
        kdf_defaults: Argon2Params {
            memory_kb,
            iterations,
            parallelism,
        },
        payload: PayloadMeta {
            chunk_size: 1024,
            chunk_count: 0,
            total_compressed_size: 0,
            total_plaintext_size: 0,
            files: vec![],
        },
        key_slots: vec![key_slot],
        next_slot_id: None,
    };

    // This should never panic - only return errors for invalid inputs
    let _ = DecryptionEngine::unlock_with_password(config, &input.password);
});
