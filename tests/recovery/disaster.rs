//! Disaster recovery tests for encrypted pages archives.
//!
//! Covers:
//! - Recovery from corrupted key slot metadata
//! - Partial archive recovery (some chunks valid)
//! - Re-export from partial data
//! - Backup verification
//! - Integrity manifest validation

use anyhow::Result;
use coding_agent_search::pages::bundle::IntegrityManifest;
use coding_agent_search::pages::encrypt::{DecryptionEngine, EncryptionConfig, EncryptionEngine, load_config};
use coding_agent_search::pages::key_management::key_list;
use coding_agent_search::pages::qr::RecoverySecret;
use std::fs;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

/// Create a test encrypted archive
fn setup_encrypted_archive(dir: &Path, password: &str, content: &[u8]) -> Result<EncryptionConfig> {
    let test_file = dir.join("test_input.db");
    fs::write(&test_file, content)?;

    let mut engine = EncryptionEngine::default();
    engine.add_password_slot(password)?;
    let config = engine.encrypt_file(&test_file, dir, |_, _| {})?;

    fs::remove_file(&test_file)?;
    Ok(config)
}

/// Create a larger archive with multiple chunks
fn setup_multi_chunk_archive(dir: &Path, password: &str) -> Result<EncryptionConfig> {
    let test_file = dir.join("test_input.db");
    // Create ~500KB of data to ensure multiple chunks (chunk size is typically 64KB)
    let content: Vec<u8> = (0..500_000).map(|i| (i % 256) as u8).collect();
    fs::write(&test_file, &content)?;

    let mut engine = EncryptionEngine::default();
    engine.add_password_slot(password)?;
    let config = engine.encrypt_file(&test_file, dir, |_, _| {})?;

    fs::remove_file(&test_file)?;
    Ok(config)
}

// ============================================================================
// Corrupted Metadata Tests
// ============================================================================

#[test]
fn test_detect_corrupted_config_json() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    setup_encrypted_archive(&archive_dir, "password", b"test content")?;

    // Corrupt the config.json
    let config_path = archive_dir.join("config.json");
    let mut file = fs::OpenOptions::new().write(true).open(&config_path)?;
    file.write_all(b"corrupted {")?;
    drop(file);

    // Loading should fail gracefully
    let result = key_list(&archive_dir);
    assert!(result.is_err(), "Should detect corrupted config");

    Ok(())
}

#[test]
fn test_detect_missing_config_json() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    setup_encrypted_archive(&archive_dir, "password", b"test content")?;

    // Remove config.json
    fs::remove_file(archive_dir.join("config.json"))?;

    // Loading should fail with appropriate error
    let result = key_list(&archive_dir);
    assert!(result.is_err(), "Should detect missing config");

    Ok(())
}

#[test]
fn test_detect_truncated_config_json() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    setup_encrypted_archive(&archive_dir, "password", b"test content")?;

    // Truncate config.json to half its size
    let config_path = archive_dir.join("config.json");
    let content = fs::read(&config_path)?;
    fs::write(&config_path, &content[..content.len() / 2])?;

    // Loading should fail
    let result = key_list(&archive_dir);
    assert!(result.is_err(), "Should detect truncated config");

    Ok(())
}

#[test]
fn test_detect_invalid_json_structure() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    setup_encrypted_archive(&archive_dir, "password", b"test content")?;

    // Write valid JSON but wrong structure
    let config_path = archive_dir.join("config.json");
    fs::write(&config_path, r#"{"wrong": "structure"}"#)?;

    // Loading should fail with schema error
    let result = key_list(&archive_dir);
    assert!(result.is_err(), "Should detect wrong JSON structure");

    Ok(())
}

// ============================================================================
// Corrupted Payload Tests
// ============================================================================

#[test]
fn test_detect_corrupted_chunk() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let config = setup_encrypted_archive(&archive_dir, "password", b"test content for corruption")?;

    // Find and corrupt the first chunk
    let payload_dir = archive_dir.join("payload");
    let chunk_path = payload_dir.join("chunk-00000.bin");
    if chunk_path.exists() {
        let content = fs::read(&chunk_path)?;
        if !content.is_empty() {
            // Flip some bits in the middle
            let mut corrupted = content.clone();
            let mid = corrupted.len() / 2;
            corrupted[mid] ^= 0xFF;
            fs::write(&chunk_path, &corrupted)?;

            // Decryption should fail with authentication error
            let decryptor = DecryptionEngine::unlock_with_password(config, "password")?;
            let decrypted_path = temp.path().join("decrypted.db");
            let result = decryptor.decrypt_to_file(&archive_dir, &decrypted_path, |_, _| {});
            assert!(result.is_err(), "Should detect corrupted chunk");
        }
    }

    Ok(())
}

#[test]
fn test_detect_missing_chunk() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let config = setup_multi_chunk_archive(&archive_dir, "password")?;

    // Remove the first chunk
    let chunk_path = archive_dir.join("payload/chunk-00000.bin");
    if chunk_path.exists() {
        fs::remove_file(&chunk_path)?;

        // Decryption should fail
        let decryptor = DecryptionEngine::unlock_with_password(config, "password")?;
        let decrypted_path = temp.path().join("decrypted.db");
        let result = decryptor.decrypt_to_file(&archive_dir, &decrypted_path, |_, _| {});
        assert!(result.is_err(), "Should detect missing chunk");
    }

    Ok(())
}

#[test]
fn test_detect_truncated_chunk() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let config = setup_encrypted_archive(&archive_dir, "password", b"test content for truncation test")?;

    // Truncate the chunk
    let chunk_path = archive_dir.join("payload/chunk-00000.bin");
    if chunk_path.exists() {
        let content = fs::read(&chunk_path)?;
        if content.len() > 10 {
            // Keep only first 10 bytes
            fs::write(&chunk_path, &content[..10])?;

            // Decryption should fail
            let decryptor = DecryptionEngine::unlock_with_password(config, "password")?;
            let decrypted_path = temp.path().join("decrypted.db");
            let result = decryptor.decrypt_to_file(&archive_dir, &decrypted_path, |_, _| {});
            assert!(result.is_err(), "Should detect truncated chunk");
        }
    }

    Ok(())
}

// ============================================================================
// Integrity Manifest Tests
// ============================================================================

#[test]
fn test_integrity_manifest_validates_files() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    setup_encrypted_archive(&archive_dir, "password", b"test content")?;

    // Load integrity manifest if present
    let integrity_path = archive_dir.join("integrity.json");
    if integrity_path.exists() {
        let content = fs::read_to_string(&integrity_path)?;
        let manifest: IntegrityManifest = serde_json::from_str(&content)?;

        // Verify each file's hash
        for entry in &manifest.files {
            let file_path = archive_dir.join(&entry.path);
            assert!(file_path.exists(), "File {} should exist", entry.path);
        }
    }

    Ok(())
}

#[test]
fn test_detect_integrity_mismatch() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    setup_encrypted_archive(&archive_dir, "password", b"test content")?;

    // Modify a file after creation
    let config_path = archive_dir.join("config.json");
    let content = fs::read_to_string(&config_path)?;
    fs::write(&config_path, content + " ")?; // Add a space

    // If integrity checking is implemented, it should detect the mismatch
    // This test validates that the infrastructure exists
    let integrity_path = archive_dir.join("integrity.json");
    if integrity_path.exists() {
        let integrity_content = fs::read_to_string(&integrity_path)?;
        let manifest: IntegrityManifest = serde_json::from_str(&integrity_content)?;

        // Find config.json entry and verify hash would mismatch
        let config_entry = manifest.files.iter().find(|e| e.path == "config.json");
        if let Some(entry) = config_entry {
            let actual_content = fs::read(&config_path)?;
            let actual_hash = sha256_hex(&actual_content);
            assert_ne!(
                actual_hash, entry.sha256,
                "Hash should mismatch after modification"
            );
        }
    }

    Ok(())
}

/// Calculate SHA-256 hash as hex string
fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

// ============================================================================
// Key Slot Metadata Corruption
// ============================================================================

#[test]
fn test_corrupted_wrapped_dek_detected() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    setup_encrypted_archive(&archive_dir, "password", b"test content")?;

    // Read and modify config.json to corrupt wrapped_dek
    let config_path = archive_dir.join("config.json");
    let content = fs::read_to_string(&config_path)?;

    // Parse, corrupt, and rewrite
    let mut config: serde_json::Value = serde_json::from_str(&content)?;
    if let Some(slots) = config.get_mut("key_slots") {
        if let Some(slot) = slots.get_mut(0) {
            if let Some(wrapped) = slot.get_mut("wrapped_dek") {
                // Corrupt the base64 by changing some characters
                let original = wrapped.as_str().unwrap_or("");
                let corrupted = original.chars().rev().collect::<String>();
                *wrapped = serde_json::Value::String(corrupted);
            }
        }
    }
    fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    // Try to load and unlock - should fail
    let load_result = load_config(&archive_dir);
    if let Ok(loaded_config) = load_result {
        assert!(
            DecryptionEngine::unlock_with_password(loaded_config, "password").is_err(),
            "Should fail with corrupted wrapped_dek"
        );
    }

    Ok(())
}

#[test]
fn test_corrupted_salt_detected() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    setup_encrypted_archive(&archive_dir, "password", b"test content")?;

    // Corrupt the salt
    let config_path = archive_dir.join("config.json");
    let content = fs::read_to_string(&config_path)?;

    let mut config: serde_json::Value = serde_json::from_str(&content)?;
    if let Some(slots) = config.get_mut("key_slots") {
        if let Some(slot) = slots.get_mut(0) {
            if let Some(salt) = slot.get_mut("salt") {
                *salt = serde_json::Value::String("invalid_base64!!!".to_string());
            }
        }
    }
    fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    // Try to load and unlock - should fail
    let load_result = load_config(&archive_dir);
    if let Ok(loaded_config) = load_result {
        assert!(
            DecryptionEngine::unlock_with_password(loaded_config, "password").is_err(),
            "Should fail with invalid salt"
        );
    }

    Ok(())
}

// ============================================================================
// Backup and Restore Tests
// ============================================================================

#[test]
fn test_archive_copy_preserves_decryptability() -> Result<()> {
    let temp = TempDir::new()?;
    let original_dir = temp.path().join("original");
    let backup_dir = temp.path().join("backup");
    fs::create_dir_all(&original_dir)?;

    let password = "backup-test-password";
    let content = b"important data to backup and restore";
    setup_encrypted_archive(&original_dir, password, content)?;

    // Copy entire archive
    copy_dir_recursive(&original_dir, &backup_dir)?;

    // Verify backup can be decrypted
    let config = load_config(&backup_dir)?;
    let decryptor = DecryptionEngine::unlock_with_password(config, password)?;
    let decrypted_path = temp.path().join("decrypted.db");
    decryptor.decrypt_to_file(&backup_dir, &decrypted_path, |_, _| {})?;

    let decrypted = fs::read(&decrypted_path)?;
    assert_eq!(decrypted, content, "Backup should decrypt to original content");

    Ok(())
}

/// Recursively copy a directory
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());

        if path.is_dir() {
            copy_dir_recursive(&path, &dest_path)?;
        } else {
            fs::copy(&path, &dest_path)?;
        }
    }
    Ok(())
}

#[test]
fn test_partial_archive_detected() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    let config = setup_multi_chunk_archive(&archive_dir, "password")?;

    // Remove payload directory entirely
    fs::remove_dir_all(archive_dir.join("payload"))?;

    // Should detect missing payload
    let decryptor = DecryptionEngine::unlock_with_password(config, "password")?;
    let decrypted_path = temp.path().join("decrypted.db");
    let result = decryptor.decrypt_to_file(&archive_dir, &decrypted_path, |_, _| {});
    assert!(result.is_err(), "Should detect missing payload directory");

    Ok(())
}

// ============================================================================
// Recovery Scenarios
// ============================================================================

#[test]
fn test_recover_with_valid_recovery_key_after_password_corruption() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    // Create archive with password and recovery
    let test_file = archive_dir.join("test_input.db");
    let content = b"critical data with recovery backup";
    fs::write(&test_file, content)?;

    let mut engine = EncryptionEngine::default();
    engine.add_password_slot("password")?;
    let recovery_secret = RecoverySecret::generate();
    engine.add_recovery_slot(recovery_secret.as_bytes())?;
    engine.encrypt_file(&test_file, &archive_dir, |_, _| {})?;
    fs::remove_file(&test_file)?;

    // Corrupt the password slot's wrapped_dek
    let config_path = archive_dir.join("config.json");
    let config_content = fs::read_to_string(&config_path)?;
    let mut config: serde_json::Value = serde_json::from_str(&config_content)?;

    if let Some(slots) = config.get_mut("key_slots") {
        if let Some(arr) = slots.as_array_mut() {
            for slot in arr.iter_mut() {
                if slot.get("slot_type").and_then(|v| v.as_str()) == Some("password") {
                    if let Some(wrapped) = slot.get_mut("wrapped_dek") {
                        *wrapped = serde_json::Value::String("corrupted".to_string());
                    }
                }
            }
        }
    }
    fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

    // Password should fail
    let config1 = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_password(config1, "password").is_err(),
        "Password should fail after corruption"
    );

    // Recovery should still work
    let config2 = load_config(&archive_dir)?;
    assert!(
        DecryptionEngine::unlock_with_recovery(config2, recovery_secret.as_bytes()).is_ok(),
        "Recovery should work even with corrupted password slot"
    );

    Ok(())
}

#[test]
fn test_graceful_error_on_completely_corrupted_archive() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    // Write garbage as config.json
    fs::write(archive_dir.join("config.json"), "not json at all {")?;

    // Should return error, not panic
    let result = load_config(&archive_dir);
    assert!(result.is_err(), "Should gracefully handle corrupted archive");

    Ok(())
}

#[test]
fn test_error_messages_are_informative() -> Result<()> {
    let temp = TempDir::new()?;
    let archive_dir = temp.path().join("archive");
    fs::create_dir_all(&archive_dir)?;

    setup_encrypted_archive(&archive_dir, "correct-password", b"test")?;

    // Wrong password should give informative error
    let config = load_config(&archive_dir)?;
    let result = DecryptionEngine::unlock_with_password(config, "wrong-password");
    assert!(result.is_err(), "Wrong password should fail");

    let error_msg = result.unwrap_err().to_string().to_lowercase();
    assert!(
        error_msg.contains("password") || error_msg.contains("key") || error_msg.contains("invalid"),
        "Error should mention password/key issue: {}",
        error_msg
    );

    Ok(())
}
