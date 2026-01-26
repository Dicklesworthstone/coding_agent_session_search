//! Tests for key slot operations in encrypted pages archives.
//!
//! Covers:
//! - Recovery key generation and unlock
//! - Multi-key-slot operations (add/remove)
//! - All active slots work independently
//! - Maximum slot limit behavior
//! - Edge cases (empty password, unicode, case sensitivity)

use anyhow::Result;
use coding_agent_search::pages::encrypt::{DecryptionEngine, EncryptionConfig, EncryptionEngine, load_config};
use coding_agent_search::pages::key_management::{
    key_add_password, key_add_recovery, key_list, key_revoke, key_rotate,
};
use coding_agent_search::pages::qr::RecoverySecret;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Create a test encrypted archive with a password slot
fn setup_encrypted_archive(dir: &Path, password: &str) -> Result<EncryptionConfig> {
    let test_file = dir.join("test_input.db");
    fs::write(&test_file, b"test database content for recovery testing")?;

    let mut engine = EncryptionEngine::default();
    engine.add_password_slot(password)?;
    let config = engine.encrypt_file(&test_file, dir, |_, _| {})?;

    fs::remove_file(&test_file)?;
    Ok(config)
}

/// Create a test encrypted archive with both password and recovery slots
fn setup_archive_with_recovery(dir: &Path, password: &str) -> Result<(EncryptionConfig, RecoverySecret)> {
    let test_file = dir.join("test_input.db");
    fs::write(&test_file, b"test database content for recovery testing")?;

    let mut engine = EncryptionEngine::default();
    engine.add_password_slot(password)?;
    let secret = RecoverySecret::generate();
    engine.add_recovery_slot(secret.as_bytes())?;
    let config = engine.encrypt_file(&test_file, dir, |_, _| {})?;

    fs::remove_file(&test_file)?;
    Ok((config, secret))
}

// ============================================================================
// Recovery Key Generation and Unlock Tests
// ============================================================================

#[test]
fn test_recovery_secret_generation() {
    // Recovery secrets should be 256 bits (32 bytes)
    let secret = RecoverySecret::generate();
    assert_eq!(secret.as_bytes().len(), 32, "Recovery secret should be 32 bytes");

    // Each generation should produce a unique secret
    let secret2 = RecoverySecret::generate();
    assert_ne!(
        secret.as_bytes(),
        secret2.as_bytes(),
        "Each generation should produce unique secrets"
    );
}

#[test]
fn test_recovery_secret_encoding_roundtrip() {
    let secret = RecoverySecret::generate();
    let encoded = secret.encoded();

    // Should be base64url encoded without padding
    assert!(
        !encoded.contains('='),
        "Base64url encoding should not have padding"
    );
    assert!(
        !encoded.contains('+') && !encoded.contains('/'),
        "Should be base64url, not base64"
    );

    // Roundtrip through encoding
    let decoded = RecoverySecret::from_encoded(encoded).expect("Should decode successfully");
    assert_eq!(secret.as_bytes(), decoded.as_bytes(), "Roundtrip should preserve bytes");
}

#[test]
fn test_recovery_key_unlocks_archive() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let password = "test-password-123";
    let (config, recovery_secret) = setup_archive_with_recovery(&archive_dir, password)?;

    // Unlock with recovery secret
    let result = DecryptionEngine::unlock_with_recovery(config, recovery_secret.as_bytes());
    assert!(result.is_ok(), "Should unlock with recovery secret");

    Ok(())
}

#[test]
fn test_recovery_key_works_after_password_change() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let password = "original-password";
    let (_config, recovery_secret) = setup_archive_with_recovery(&archive_dir, password)?;

    // Add a new password slot
    let new_password = "new-password-456";
    key_add_password(&archive_dir, password, new_password)?;

    // Revoke the original password slot (slot 0)
    key_revoke(&archive_dir, new_password, 0)?;

    // Reload config and try recovery key
    let updated_config = load_config(&archive_dir)?;
    let result = DecryptionEngine::unlock_with_recovery(updated_config, recovery_secret.as_bytes());
    assert!(result.is_ok(), "Recovery key should work after password change");

    Ok(())
}

#[test]
fn test_invalid_recovery_secret_rejected() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let (config, _recovery_secret) = setup_archive_with_recovery(&archive_dir, "password")?;

    // Try with a wrong recovery secret
    let wrong_secret = RecoverySecret::generate();
    let result = DecryptionEngine::unlock_with_recovery(config, wrong_secret.as_bytes());
    assert!(result.is_err(), "Should reject wrong recovery secret");

    Ok(())
}

// ============================================================================
// Multi-Key-Slot Tests
// ============================================================================

#[test]
fn test_add_password_slot_to_existing_archive() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let password1 = "first-password";
    setup_encrypted_archive(&archive_dir, password1)?;

    // Initially should have 1 slot
    let list1 = key_list(&archive_dir)?;
    assert_eq!(list1.active_slots, 1, "Should start with 1 slot");

    // Add second password
    let password2 = "second-password";
    let slot_id = key_add_password(&archive_dir, password1, password2)?;
    assert_eq!(slot_id, 1, "New slot should have ID 1");

    // Should now have 2 slots
    let list2 = key_list(&archive_dir)?;
    assert_eq!(list2.active_slots, 2, "Should have 2 slots after add");

    // Both passwords should work
    let config1 = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_password(config1, password1).is_ok(),
        "First password should work"
    );

    let config2 = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_password(config2, password2).is_ok(),
        "Second password should work"
    );

    Ok(())
}

#[test]
fn test_add_recovery_slot_to_existing_archive() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let password = "test-password";
    setup_encrypted_archive(&archive_dir, password)?;

    // Add recovery slot
    let (slot_id, secret) = key_add_recovery(&archive_dir, password)?;
    assert_eq!(slot_id, 1, "Recovery slot should have ID 1");

    // Should now have 2 slots
    let list = key_list(&archive_dir)?;
    assert_eq!(list.active_slots, 2, "Should have 2 slots");

    // Recovery secret should work
    let config = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_recovery(config, secret.as_bytes()).is_ok(),
        "Recovery secret should work"
    );

    Ok(())
}

#[test]
fn test_revoke_slot_from_archive() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let password1 = "first-password";
    setup_encrypted_archive(&archive_dir, password1)?;

    // Add second password
    let password2 = "second-password";
    key_add_password(&archive_dir, password1, password2)?;

    // Revoke first slot using second password
    let result = key_revoke(&archive_dir, password2, 0)?;
    assert_eq!(result.revoked_slot_id, 0, "Should revoke slot 0");
    assert_eq!(result.remaining_slots, 1, "Should have 1 remaining slot");

    // First password should no longer work
    let config1 = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_password(config1, password1).is_err(),
        "Revoked password should not work"
    );

    // Second password should still work
    let config2 = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_password(config2, password2).is_ok(),
        "Active password should work"
    );

    Ok(())
}

#[test]
fn test_cannot_revoke_last_slot() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let password = "only-password";
    setup_encrypted_archive(&archive_dir, password)?;

    // Cannot revoke the only slot
    let result = key_revoke(&archive_dir, password, 0);
    assert!(result.is_err(), "Should not allow revoking last slot");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("last remaining key slot"),
        "Error should mention last slot"
    );

    Ok(())
}

#[test]
fn test_cannot_revoke_authenticating_slot() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let password1 = "first-password";
    setup_encrypted_archive(&archive_dir, password1)?;

    // Add second password
    let password2 = "second-password";
    key_add_password(&archive_dir, password1, password2)?;

    // Cannot revoke slot 0 when authenticating with slot 0's password
    let result = key_revoke(&archive_dir, password1, 0);
    assert!(result.is_err(), "Should not allow revoking authenticating slot");
    assert!(
        result.unwrap_err().to_string().contains("authentication"),
        "Error should mention authentication"
    );

    Ok(())
}

#[test]
fn test_all_active_slots_work_independently() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let password1 = "password-one";
    setup_encrypted_archive(&archive_dir, password1)?;

    // Add multiple slots
    let password2 = "password-two";
    let password3 = "password-three";
    key_add_password(&archive_dir, password1, password2)?;
    key_add_password(&archive_dir, password1, password3)?;
    let (_, recovery) = key_add_recovery(&archive_dir, password1)?;

    // Verify all passwords work independently
    let passwords = vec![password1, password2, password3];
    for (i, pw) in passwords.iter().enumerate() {
        let config = load_config(&archive_dir)?;
        assert!(
            DecryptionEngine::unlock_with_password(config, pw).is_ok(),
            "Password {} (slot {}) should work",
            pw,
            i
        );
    }

    // Recovery should also work
    let config = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_recovery(config, recovery.as_bytes()).is_ok(),
        "Recovery secret should work"
    );

    Ok(())
}

#[test]
fn test_slot_ids_remain_stable_after_revocation() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let password1 = "password-one";
    setup_encrypted_archive(&archive_dir, password1)?;

    let password2 = "password-two";
    let password3 = "password-three";
    key_add_password(&archive_dir, password1, password2)?; // slot 1
    key_add_password(&archive_dir, password1, password3)?; // slot 2

    // Revoke slot 1
    key_revoke(&archive_dir, password3, 1)?;

    // Add another password - should get slot 3 (not reuse slot 1)
    let password4 = "password-four";
    let new_slot_id = key_add_password(&archive_dir, password3, password4)?;
    assert_eq!(new_slot_id, 3, "New slot should be ID 3, not reuse revoked ID");

    // Verify slot structure
    let list = key_list(&archive_dir)?;
    let slot_ids: Vec<u8> = list.slots.iter().map(|s| s.id).collect();
    assert!(slot_ids.contains(&0), "Slot 0 should exist");
    assert!(!slot_ids.contains(&1), "Slot 1 should be revoked");
    assert!(slot_ids.contains(&2), "Slot 2 should exist");
    assert!(slot_ids.contains(&3), "Slot 3 should exist");

    Ok(())
}

// ============================================================================
// Key Rotation Tests
// ============================================================================

#[test]
fn test_key_rotation_basic() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let old_password = "old-password";
    setup_encrypted_archive(&archive_dir, old_password)?;

    // Rotate to new password
    let new_password = "new-password";
    let result = key_rotate(&archive_dir, old_password, new_password, false, |_| {})?;
    assert_eq!(result.slot_count, 1, "Should have 1 slot after rotation");
    assert!(result.recovery_secret.is_none(), "Should not have recovery when keep_recovery=false");

    // Old password should not work
    let config1 = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_password(config1, old_password).is_err(),
        "Old password should not work after rotation"
    );

    // New password should work
    let config2 = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_password(config2, new_password).is_ok(),
        "New password should work after rotation"
    );

    Ok(())
}

#[test]
fn test_key_rotation_with_recovery() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let old_password = "old-password";
    setup_encrypted_archive(&archive_dir, old_password)?;

    // Rotate with recovery
    let new_password = "new-password";
    let result = key_rotate(&archive_dir, old_password, new_password, true, |_| {})?;
    assert_eq!(result.slot_count, 2, "Should have 2 slots with recovery");
    assert!(result.recovery_secret.is_some(), "Should have recovery secret");

    // Verify new recovery works
    let recovery_encoded = result.recovery_secret.unwrap();
    let recovery = RecoverySecret::from_encoded(&recovery_encoded)?;

    let config = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_recovery(config, recovery.as_bytes()).is_ok(),
        "New recovery should work after rotation"
    );

    Ok(())
}

// ============================================================================
// Edge Cases
// ============================================================================

#[test]
fn test_empty_password_rejected() {
    let mut engine = EncryptionEngine::default();
    let result = engine.add_password_slot("");
    assert!(result.is_err(), "Empty password should be rejected");
}

#[test]
fn test_unicode_password_support() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    // Unicode password with various scripts
    let unicode_password = "пароль密码🔐мир";
    let config = setup_encrypted_archive(&archive_dir, unicode_password)?;

    // Should unlock with exact same password
    assert!(
        DecryptionEngine::unlock_with_password(config, unicode_password).is_ok(),
        "Unicode password should work"
    );

    Ok(())
}

#[test]
fn test_password_case_sensitivity() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let password = "CaseSensitivePassword";
    setup_encrypted_archive(&archive_dir, password)?;

    // Exact case should work
    let config1 = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_password(config1, password).is_ok(),
        "Exact case should work"
    );

    // Different case should fail
    let config2 = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_password(config2, "casesensitivepassword").is_err(),
        "Lowercase should fail"
    );

    let config3 = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_password(config3, "CASESENSITIVEPASSWORD").is_err(),
        "Uppercase should fail"
    );

    Ok(())
}

#[test]
fn test_long_password_support() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    // Very long password (1000 chars)
    let long_password: String = (0..1000).map(|i| ((i % 26) as u8 + b'a') as char).collect();
    let config = setup_encrypted_archive(&archive_dir, &long_password)?;

    assert!(
        DecryptionEngine::unlock_with_password(config, &long_password).is_ok(),
        "Long password should work"
    );

    Ok(())
}

#[test]
fn test_whitespace_only_password_rejected() {
    let mut engine = EncryptionEngine::default();

    // Whitespace-only passwords should be rejected
    let result = engine.add_password_slot("   ");
    assert!(result.is_err(), "Whitespace-only password should be rejected");

    let result2 = engine.add_password_slot("\t\n");
    assert!(result2.is_err(), "Tab/newline only password should be rejected");
}

#[test]
fn test_password_with_special_characters() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    // Password with special characters, quotes, backslashes, null-like
    let special_password = r#"p@ss\w0rd'"<>&;|`$(){}[]!#%^*~"#;
    let config = setup_encrypted_archive(&archive_dir, special_password)?;

    assert!(
        DecryptionEngine::unlock_with_password(config, special_password).is_ok(),
        "Special character password should work"
    );

    Ok(())
}
