//! Regression coverage for bead coding_agent_session_search-dyoj4.
//!
//! A tampered encrypted pages config with a malformed key-slot nonce must
//! return a structured unlock error instead of panicking inside AES-GCM.

use base64::prelude::*;
use coding_agent_search::pages::encrypt::{DecryptionEngine, EncryptionEngine, load_config};
use std::panic::{AssertUnwindSafe, catch_unwind};
use tempfile::TempDir;

#[test]
fn malformed_key_slot_nonce_returns_error_without_panic() {
    let temp_dir = TempDir::new().unwrap();
    let input_path = temp_dir.path().join("session.html");
    let encrypted_dir = temp_dir.path().join("encrypted");

    std::fs::write(
        &input_path,
        b"<html><body>real encrypted pages fixture</body></html>",
    )
    .unwrap();

    let mut engine = EncryptionEngine::new(1024).unwrap();
    engine.add_recovery_slot(b"real-recovery-secret").unwrap();
    engine
        .encrypt_file(&input_path, &encrypted_dir, |_, _| {})
        .unwrap();

    let mut config = load_config(&encrypted_dir).unwrap();
    assert_eq!(config.key_slots.len(), 1);
    config.key_slots[0].nonce = BASE64_STANDARD.encode([0x42_u8; 8]);

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        DecryptionEngine::unlock_with_recovery(config, b"real-recovery-secret")
    }));
    let result = outcome.expect("malformed key-slot nonce must not panic");
    match result {
        Ok(_) => panic!("malformed key-slot nonce must reject unlock"),
        Err(err) => assert!(
            err.to_string().contains("Invalid recovery secret")
                || err.to_string().contains("invalid nonce length"),
            "unexpected error: {err:#}"
        ),
    }
}
