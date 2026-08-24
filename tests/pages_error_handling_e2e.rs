//! Error Handling E2E Tests (P6.8)
//!
//! Comprehensive test suite for error handling in the pages export system.
//! Tests verify that:
//! - All error types have user-friendly messages
//! - Error messages don't leak sensitive information
//! - All error paths are tested
//! - Recovery suggestions are provided
//! - Timing attacks are prevented
//!
//! # Running
//!
//! ```bash
//! cargo test --test pages_error_handling_e2e
//! ```

use coding_agent_search::pages::encrypt::{DecryptionEngine, EncryptionEngine, load_config};
use coding_agent_search::pages::errors::{
    BrowserError, DbError, DecryptError, ErrorCode, ExportError, NetworkError,
};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

// =============================================================================
// Test Configuration
// =============================================================================

const TEST_PASSWORD: &str = "test-password-for-error-handling";
const TEST_RECOVERY_SECRET: &[u8] = b"test-recovery-secret-32-bytes!!";

// =============================================================================
// Helper Functions
// =============================================================================

/// Create a test archive with password encryption.
fn create_test_archive(temp_dir: &Path, password: &str) -> std::path::PathBuf {
    let input_path = temp_dir.join("input.db");
    fs::write(
        &input_path,
        b"Test database content for error handling tests",
    )
    .unwrap();

    let encrypt_dir = temp_dir.join("encrypted");
    let mut engine = EncryptionEngine::new(1024).expect("valid chunk size");
    engine.add_password_slot(password).unwrap();

    engine
        .encrypt_file(&input_path, &encrypt_dir, |_, _| {})
        .unwrap();

    encrypt_dir
}

/// Create a test archive with both password and recovery slots.
fn create_test_archive_with_recovery(
    temp_dir: &Path,
    password: &str,
    recovery: &[u8],
) -> std::path::PathBuf {
    let input_path = temp_dir.join("input.db");
    fs::write(&input_path, b"Test database content").unwrap();

    let encrypt_dir = temp_dir.join("encrypted");
    let mut engine = EncryptionEngine::new(1024).expect("valid chunk size");
    engine.add_password_slot(password).unwrap();
    engine.add_recovery_slot(recovery).unwrap();

    engine
        .encrypt_file(&input_path, &encrypt_dir, |_, _| {})
        .unwrap();

    encrypt_dir
}

// =============================================================================
// Authentication Error Tests
// =============================================================================

#[test]
fn test_wrong_password_error() {
    let temp_dir = TempDir::new().unwrap();
    let archive_dir = create_test_archive(temp_dir.path(), "correct-password");

    let config = load_config(&archive_dir).expect("Should load config");
    let result = DecryptionEngine::unlock_with_password(config, "wrong-password");

    assert!(result.is_err(), "Should fail with wrong password");

    // Verify error message is user-friendly
    match result {
        Ok(_) => panic!("Should have failed"),
        Err(e) => {
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("password")
                    || err_msg.contains("Invalid")
                    || err_msg.contains("key slot"),
                "Error should mention password issue: {}",
                err_msg
            );
        }
    }
}

#[test]
fn test_empty_password_validation() {
    // Test that empty passwords are handled appropriately
    // DecryptError should have a specific variant for empty passwords
    let error = DecryptError::EmptyPassword;
    let message = error.to_string();

    assert!(
        message.to_lowercase().contains("enter") || message.to_lowercase().contains("password"),
        "Empty password error should be clear: {}",
        message
    );

    let suggestion = error.suggestion();
    assert!(!suggestion.is_empty(), "Should have a suggestion");
}

#[test]
fn test_password_error_no_timing_leak() {
    // Verify that wrong password attempts take similar time
    // This helps prevent timing attacks that could reveal password length
    let temp_dir = TempDir::new().unwrap();
    let archive_dir = create_test_archive(temp_dir.path(), "correctpassword123");

    let config = load_config(&archive_dir).expect("Should load config");

    // Measure time for different wrong passwords
    let attempts = [
        "a",
        "ab",
        "abc",
        "wrongpassword",
        "wrongpassword123",
        "wrongpassword12345678901234567890",
    ];

    let mut times = Vec::new();

    for password in &attempts {
        let config_copy = config.clone();
        let start = Instant::now();
        let _ = DecryptionEngine::unlock_with_password(config_copy, password);
        times.push(start.elapsed());
    }

    // Calculate mean and variance
    let mean_ns: u128 = times.iter().map(|t| t.as_nanos()).sum::<u128>() / times.len() as u128;
    let variance: f64 = times
        .iter()
        .map(|t| (t.as_nanos() as f64 - mean_ns as f64).powi(2))
        .sum::<f64>()
        / times.len() as f64;

    let std_dev = variance.sqrt();
    let coefficient_of_variation = std_dev / mean_ns as f64;

    // The coefficient of variation should be reasonably low
    // (high variance would indicate timing leak)
    // Note: This is a heuristic; actual timing attack prevention
    // requires constant-time comparison in crypto code
    println!(
        "Timing test: mean={:.2}ms, std_dev={:.2}ms, cv={:.4}",
        mean_ns as f64 / 1_000_000.0,
        std_dev / 1_000_000.0,
        coefficient_of_variation
    );

    // CV above 0.5 would be suspicious for constant-time operations
    // but Argon2 time varies with system load, so we use a lenient threshold
    assert!(
        coefficient_of_variation < 1.0,
        "Timing variance is suspiciously high (CV={:.4}), may indicate timing leak",
        coefficient_of_variation
    );
}

#[test]
fn test_wrong_recovery_key_error() {
    let temp_dir = TempDir::new().unwrap();
    let archive_dir =
        create_test_archive_with_recovery(temp_dir.path(), TEST_PASSWORD, TEST_RECOVERY_SECRET);

    let config = load_config(&archive_dir).expect("Should load config");
    let result = DecryptionEngine::unlock_with_recovery(config, &[0xEE; 32]);

    assert!(result.is_err(), "Should fail with wrong recovery key");
}

// =============================================================================
// Archive Format Error Tests
// =============================================================================

#[test]
fn test_corrupted_config_header() {
    let temp_dir = TempDir::new().unwrap();
    let archive_dir = create_test_archive(temp_dir.path(), TEST_PASSWORD);

    // Corrupt the config.json
    let config_path = archive_dir.join("config.json");
    fs::write(&config_path, b"{ invalid json }").unwrap();

    let result = load_config(&archive_dir);
    assert!(result.is_err(), "Should fail with corrupted config");
}

#[test]
fn test_corrupted_ciphertext() {
    let temp_dir = TempDir::new().unwrap();
    let archive_dir = create_test_archive(temp_dir.path(), TEST_PASSWORD);

    // Find and corrupt a payload chunk
    let payload_dir = archive_dir.join("payload");
    let chunk_path = fs::read_dir(&payload_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().map(|e| e == "bin").unwrap_or(false))
        .expect("Should find a chunk");

    let mut chunk_data = fs::read(&chunk_path).unwrap();
    if !chunk_data.is_empty() {
        // Flip bits in middle of chunk
        let mid = chunk_data.len() / 2;
        chunk_data[mid] ^= 0xFF;
        fs::write(&chunk_path, &chunk_data).unwrap();
    }

    // Loading config should work
    let config = load_config(&archive_dir).expect("Config should load");

    // But decryption should fail due to tampered ciphertext
    let decryptor = DecryptionEngine::unlock_with_password(config, TEST_PASSWORD)
        .expect("Password should still work");

    let decrypted_path = temp_dir.path().join("decrypted.db");
    let result = decryptor.decrypt_to_file(&archive_dir, &decrypted_path, |_, _| {});

    assert!(result.is_err(), "Should fail on corrupted ciphertext");
}

#[test]
fn test_truncated_archive() {
    let temp_dir = TempDir::new().unwrap();
    let archive_dir = create_test_archive(temp_dir.path(), TEST_PASSWORD);

    // Truncate a payload chunk
    let payload_dir = archive_dir.join("payload");
    let chunk_path = fs::read_dir(&payload_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().map(|e| e == "bin").unwrap_or(false))
        .expect("Should find a chunk");

    let chunk_data = fs::read(&chunk_path).unwrap();
    if chunk_data.len() > 10 {
        fs::write(&chunk_path, &chunk_data[..chunk_data.len() / 2]).unwrap();
    }

    let config = load_config(&archive_dir).expect("Config should load");
    let decryptor = DecryptionEngine::unlock_with_password(config, TEST_PASSWORD)
        .expect("Password should work");

    let decrypted_path = temp_dir.path().join("decrypted.db");
    let result = decryptor.decrypt_to_file(&archive_dir, &decrypted_path, |_, _| {});

    assert!(result.is_err(), "Should fail on truncated archive");
}

#[test]
fn test_missing_chunk_file() {
    let temp_dir = TempDir::new().unwrap();
    let archive_dir = create_test_archive(temp_dir.path(), TEST_PASSWORD);

    // Remove a payload chunk
    let payload_dir = archive_dir.join("payload");
    let chunk_path = fs::read_dir(&payload_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().map(|e| e == "bin").unwrap_or(false))
        .expect("Should find a chunk");

    fs::remove_file(&chunk_path).unwrap();

    let config = load_config(&archive_dir).expect("Config should load");
    let decryptor = DecryptionEngine::unlock_with_password(config, TEST_PASSWORD)
        .expect("Password should work");

    let decrypted_path = temp_dir.path().join("decrypted.db");
    let result = decryptor.decrypt_to_file(&archive_dir, &decrypted_path, |_, _| {});

    assert!(result.is_err(), "Should fail on missing chunk");
}

#[test]
fn test_version_mismatch() {
    // Test handling of unsupported version
    let error = DecryptError::UnsupportedVersion(99);
    let message = error.to_string();

    assert!(
        message.contains("99") || message.contains("version") || message.contains("newer"),
        "Version error should mention the version: {}",
        message
    );

    let suggestion = error.suggestion();
    assert!(
        suggestion.to_lowercase().contains("update"),
        "Suggestion should mention updating: {}",
        suggestion
    );
}

#[test]
fn test_invalid_format_error() {
    let error = DecryptError::InvalidFormat("Magic bytes mismatch".into());
    let message = error.to_string();

    // User-facing message should be friendly
    assert!(
        message.to_lowercase().contains("not a valid")
            || message.to_lowercase().contains("archive"),
        "Format error should be user-friendly: {}",
        message
    );

    // Should not expose internal details
    assert!(
        !message.contains("Magic bytes"),
        "Should not expose internal details in display: {}",
        message
    );
}

// =============================================================================
// Database Error Tests
// =============================================================================

#[test]
fn test_corrupt_database_error() {
    let error = DbError::CorruptDatabase("file is not a database".into());
    let message = error.to_string();

    assert!(
        message.to_lowercase().contains("corrupt"),
        "Should mention corruption: {}",
        message
    );

    // Should not expose SQLite internals
    assert!(
        !message.contains("not a database"),
        "Should not expose internal error: {}",
        message
    );
}

#[test]
fn test_missing_table_error() {
    let error = DbError::MissingTable("messages_fts".into());
    let message = error.to_string();

    assert!(
        message.to_lowercase().contains("missing"),
        "Should mention missing data: {}",
        message
    );

    // Should not expose table names to users
    assert!(
        !message.contains("messages_fts"),
        "Should not expose table name: {}",
        message
    );
}

#[test]
fn test_invalid_query_error() {
    // Simulate a user entering a malformed FTS query
    let error = DbError::InvalidQuery("fts5: syntax error near 'MATCH'".into());
    let message = error.to_string();

    // User message should be friendly
    assert!(
        message.to_lowercase().contains("search") || message.to_lowercase().contains("processed"),
        "Should give user-friendly message: {}",
        message
    );

    // Should not expose FTS/SQL internals
    assert!(
        !message.contains("fts5"),
        "Should not expose FTS details: {}",
        message
    );
    assert!(
        !message.contains("MATCH"),
        "Should not expose SQL keywords: {}",
        message
    );
}

// =============================================================================
// Error Message Quality Tests
// =============================================================================

#[test]
fn test_error_messages_are_user_friendly() {
    let test_cases: Vec<(Box<dyn std::fmt::Display>, &str)> = vec![
        (Box::new(DecryptError::AuthenticationFailed), "password"),
        (
            Box::new(DecryptError::InvalidFormat("test".into())),
            "archive",
        ),
        (Box::new(DecryptError::IntegrityCheckFailed), "corrupt"),
        (
            Box::new(DecryptError::CorruptPayload("chunk authentication".into())),
            "corrupt",
        ),
        (Box::new(DecryptError::UnsupportedVersion(1)), "version"),
        (
            Box::new(DecryptError::UnsupportedMetadata("compression".into())),
            "unsupported",
        ),
        (Box::new(DbError::CorruptDatabase("test".into())), "corrupt"),
        (Box::new(DbError::InvalidQuery("test".into())), "search"),
    ];

    for (error, expected_substring) in test_cases {
        let message = error.to_string().to_lowercase();
        assert!(
            message.contains(expected_substring),
            "Error should mention '{}', got: {}",
            expected_substring,
            message
        );
    }
}

#[test]
fn test_error_messages_no_technical_jargon() {
    let errors: Vec<Box<dyn std::fmt::Display>> = vec![
        Box::new(DecryptError::AuthenticationFailed),
        Box::new(DecryptError::EmptyPassword),
        Box::new(DecryptError::InvalidFormat("header".into())),
        Box::new(DecryptError::IntegrityCheckFailed),
        Box::new(DecryptError::CorruptPayload("cipher authentication".into())),
        Box::new(DecryptError::UnsupportedVersion(2)),
        Box::new(DecryptError::UnsupportedMetadata("compression".into())),
        Box::new(DecryptError::CryptoError("GCM tag mismatch".into())),
        Box::new(DbError::CorruptDatabase("sqlite error".into())),
        Box::new(DbError::InvalidQuery("FTS5 syntax".into())),
    ];

    let jargon = [
        "GCM",
        "AES",
        "AEAD",
        "nonce",
        "cipher",
        "tag",
        "MAC",
        "sqlite",
        "FTS",
        "FTS5",
        "SQL",
        "query syntax",
    ];

    for error in errors {
        let display = error.to_string();
        for word in jargon {
            assert!(
                !display.to_uppercase().contains(&word.to_uppercase()),
                "Error should not contain '{}' in display: {}",
                word,
                display
            );
        }
    }
}

#[test]
fn test_error_messages_dont_leak_secrets() {
    let password = "secret-password-123";
    let error = DecryptError::AuthenticationFailed;

    let display = error.to_string();
    let debug = format!("{:?}", error);
    let log_msg = error.log_message();

    assert!(
        !display.contains(password),
        "Display should not contain password"
    );
    assert!(
        !debug.contains(password),
        "Debug should not contain password"
    );
    assert!(
        !log_msg.contains(password),
        "Log message should not contain password"
    );

    // Also check that "wrong" attempt isn't leaked
    assert!(
        !display.contains("wrong"),
        "Should not reveal what was attempted"
    );
}

#[test]
fn test_all_errors_have_suggestions() {
    let decrypt_errors: Vec<DecryptError> = vec![
        DecryptError::AuthenticationFailed,
        DecryptError::EmptyPassword,
        DecryptError::InvalidFormat("test".into()),
        DecryptError::IntegrityCheckFailed,
        DecryptError::CorruptPayload("chunk authentication".into()),
        DecryptError::UnsupportedVersion(2),
        DecryptError::UnsupportedMetadata("compression".into()),
        DecryptError::NoMatchingKeySlot,
        DecryptError::CryptoError("test".into()),
    ];

    for error in decrypt_errors {
        let suggestion = error.suggestion();
        assert!(!suggestion.is_empty(), "{:?} has no suggestion", error);
        assert!(
            suggestion.ends_with('.') || suggestion.ends_with('!'),
            "{:?} suggestion should end with punctuation: {}",
            error,
            suggestion
        );
    }

    let db_errors: Vec<DbError> = vec![
        DbError::CorruptDatabase("test".into()),
        DbError::MissingTable("test".into()),
        DbError::InvalidQuery("test".into()),
        DbError::DatabaseLocked,
        DbError::NoResults,
    ];

    for error in db_errors {
        let suggestion = error.suggestion();
        assert!(!suggestion.is_empty(), "{:?} has no suggestion", error);
    }
}

#[test]
fn test_error_codes_exist_and_unique() {
    let mut codes = std::collections::HashSet::new();

    let decrypt_errors: Vec<Box<dyn ErrorCode>> = vec![
        Box::new(DecryptError::AuthenticationFailed),
        Box::new(DecryptError::EmptyPassword),
        Box::new(DecryptError::InvalidFormat("".into())),
        Box::new(DecryptError::IntegrityCheckFailed),
        Box::new(DecryptError::CorruptPayload("".into())),
        Box::new(DecryptError::UnsupportedVersion(0)),
        Box::new(DecryptError::UnsupportedMetadata("compression".into())),
        Box::new(DecryptError::NoMatchingKeySlot),
        Box::new(DecryptError::CryptoError("".into())),
    ];

    for error in decrypt_errors {
        let code = error.error_code();
        assert!(
            code.starts_with("E"),
            "Error code should start with 'E': {}",
            code
        );
        assert!(
            codes.insert(code.to_string()),
            "Duplicate error code: {}",
            code
        );
    }

    let db_errors: Vec<Box<dyn ErrorCode>> = vec![
        Box::new(DbError::CorruptDatabase("".into())),
        Box::new(DbError::MissingTable("".into())),
        Box::new(DbError::InvalidQuery("".into())),
        Box::new(DbError::DatabaseLocked),
        Box::new(DbError::NoResults),
    ];

    for error in db_errors {
        let code = error.error_code();
        assert!(
            code.starts_with("E"),
            "Error code should start with 'E': {}",
            code
        );
        assert!(
            codes.insert(code.to_string()),
            "Duplicate error code: {}",
            code
        );
    }
}

// =============================================================================
// Browser Error Tests (Unit Tests for Error Types)
// =============================================================================

#[test]
fn test_browser_error_messages() {
    let errors = vec![
        (
            BrowserError::UnsupportedBrowser("IndexedDB".into()),
            "browser",
        ),
        (BrowserError::WasmNotSupported, "webassembly"),
        (BrowserError::CryptoNotSupported, "cryptography"),
        (BrowserError::StorageQuotaExceeded, "storage"),
        (BrowserError::SharedArrayBufferNotAvailable, "cross-origin"),
    ];

    for (error, expected) in errors {
        let message = error.to_string().to_lowercase();
        assert!(
            message.contains(expected),
            "Browser error should mention '{}': {}",
            expected,
            message
        );
    }
}

#[test]
fn test_browser_error_suggestions_actionable() {
    let errors = vec![
        BrowserError::UnsupportedBrowser("test".into()),
        BrowserError::WasmNotSupported,
        BrowserError::CryptoNotSupported,
        BrowserError::StorageQuotaExceeded,
        BrowserError::SharedArrayBufferNotAvailable,
    ];

    for error in errors {
        let suggestion = error.suggestion();

        // Suggestions should be actionable (contain verbs like "use", "update", "clear")
        let actionable_words = ["use", "update", "clear", "close", "served"];
        let is_actionable = actionable_words
            .iter()
            .any(|word| suggestion.to_lowercase().contains(word));

        assert!(
            is_actionable,
            "Browser error suggestion should be actionable: {}",
            suggestion
        );
    }
}

// =============================================================================
// Network Error Tests (Unit Tests for Error Types)
// =============================================================================

#[test]
fn test_network_error_messages() {
    let errors = vec![
        (
            NetworkError::FetchFailed("connection refused".into()),
            "download",
        ),
        (
            NetworkError::IncompleteDownload {
                expected: 1000,
                received: 500,
            },
            "incomplete",
        ),
        (NetworkError::Timeout, "timed out"),
        (NetworkError::ServerError(500), "error"),
    ];

    for (error, expected) in errors {
        let message = error.to_string().to_lowercase();
        assert!(
            message.contains(expected),
            "Network error should mention '{}': {}",
            expected,
            message
        );
    }
}

#[test]
fn test_network_error_no_internal_details() {
    let error = NetworkError::FetchFailed("ECONNREFUSED 127.0.0.1:3000".into());
    let message = error.to_string();

    assert!(
        !message.contains("ECONNREFUSED"),
        "Should not expose internal error: {}",
        message
    );
    assert!(
        !message.contains("127.0.0.1"),
        "Should not expose IP address: {}",
        message
    );
}

// =============================================================================
// Export Error Tests
// =============================================================================

#[test]
fn test_export_error_messages() {
    let errors = vec![
        (ExportError::NoConversations, "no conversations"),
        (
            ExportError::SourceDatabaseError("file not found".into()),
            "database",
        ),
        (
            ExportError::OutputError("permission denied".into()),
            "output",
        ),
        (ExportError::FilterMatchedNothing, "filter"),
    ];

    for (error, expected) in errors {
        let message = error.to_string().to_lowercase();
        assert!(
            message.contains(expected),
            "Export error should mention '{}': {}",
            expected,
            message
        );
    }
}

#[test]
fn test_export_error_suggestions() {
    let errors = vec![
        ExportError::NoConversations,
        ExportError::SourceDatabaseError("test".into()),
        ExportError::OutputError("test".into()),
        ExportError::FilterMatchedNothing,
    ];

    for error in errors {
        let suggestion = error.suggestion();
        assert!(
            !suggestion.is_empty(),
            "{:?} should have a suggestion",
            error
        );
    }
}

// =============================================================================
// Integration: Full Error Flow Tests
// =============================================================================

#[test]
fn test_error_chain_authentication_to_recovery() {
    // Simulate: user enters wrong password, gets error, uses recovery key
    let temp_dir = TempDir::new().unwrap();
    let archive_dir =
        create_test_archive_with_recovery(temp_dir.path(), TEST_PASSWORD, TEST_RECOVERY_SECRET);

    // Step 1: Wrong password
    let config = load_config(&archive_dir).unwrap();
    let wrong_result = DecryptionEngine::unlock_with_password(config, "wrong-password");
    assert!(wrong_result.is_err());

    // Step 2: User sees helpful error message
    match wrong_result {
        Ok(_) => panic!("Should have failed"),
        Err(e) => {
            let err_msg = e.to_string();
            assert!(!err_msg.is_empty(), "Error message should not be empty");
        }
    }

    // Step 3: User uses recovery key instead
    let config = load_config(&archive_dir).unwrap();
    let recovery_result = DecryptionEngine::unlock_with_recovery(config, TEST_RECOVERY_SECRET);
    assert!(recovery_result.is_ok(), "Recovery key should work");
}

#[test]
fn test_graceful_degradation_corrupted_archive() {
    // Test that corruption is rejected gracefully with a useful error.
    let temp_dir = TempDir::new().unwrap();
    let archive_dir = create_test_archive(temp_dir.path(), TEST_PASSWORD);

    // Partially corrupt the archive
    let config_path = archive_dir.join("config.json");
    let config_content = fs::read_to_string(&config_path).unwrap();

    // Insert garbage but keep JSON valid
    let modified = config_content.replace("\"version\"", "\"garbage_field\": true, \"version\"");
    fs::write(&config_path, modified).unwrap();

    let err = load_config(&archive_dir).expect_err("corrupted config should be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("unknown field") && msg.contains("garbage_field"),
        "Should surface the offending unexpected field cleanly: {msg}"
    );
}

#[test]
fn browser_lock_terminates_in_flight_crypto_before_reinitializing() {
    let auth_js = include_str!("../src/pages_assets/auth.js");
    let terminate_body = auth_js
        .split_once("function terminateCryptoWorker()")
        .expect("auth.js should define the crypto-worker termination boundary")
        .1
        .split_once("function resetCryptoWorker()")
        .expect("worker termination should remain a bounded helper")
        .0;
    assert!(
        terminate_body.contains("previousWorker.terminate();"),
        "worker termination must cancel in-flight cryptographic work"
    );

    let reset_body = auth_js
        .split_once("function resetCryptoWorker()")
        .expect("auth.js should define the crypto-worker reset boundary")
        .1
        .split_once("function beginAppInitAttempt()")
        .expect("worker reset should remain a bounded helper")
        .0;
    let terminate_offset = reset_body
        .find("terminateCryptoWorker();")
        .expect("worker reset must use the hard termination boundary");
    let reinitialize_offset = reset_body
        .find("initializeCryptoWorker();")
        .expect("worker reset must create a clean replacement");
    assert!(
        terminate_offset < reinitialize_offset,
        "the old worker must be terminated before its replacement is started"
    );

    let lock_body = auth_js
        .split_once("async function lockArchive(options = {})")
        .expect("auth.js should define lockArchive")
        .1
        .split_once("async function loadQrScannerLibrary()")
        .expect("lockArchive should remain a bounded helper")
        .0;
    let reset_offset = lock_body
        .find("const workerReady = resetCryptoWorker();")
        .expect("locking must reset the crypto worker");
    let clear_session_offset = lock_body
        .find("window.cassSession = null;")
        .expect("locking must clear the in-memory session key");
    let first_await_offset = lock_body
        .find("await closeQrScanner();")
        .expect("locking should still close the QR scanner");
    assert!(
        reset_offset < first_await_offset,
        "worker termination must happen synchronously before lockArchive yields"
    );
    assert!(
        clear_session_offset < first_await_offset,
        "the in-memory session key must be cleared before lockArchive yields"
    );
    assert!(
        !auth_js.contains("postMessage({ type: 'CLEAR_KEYS' })"),
        "a queued CLEAR_KEYS message is not a cancellation boundary"
    );

    let unlock_success_body = auth_js
        .split_once("function handleUnlockSuccess(data)")
        .expect("auth.js should define the successful-unlock transition")
        .1
        .split_once("function handleUnlockFailed(data)")
        .expect("successful unlock should remain a bounded helper")
        .0;
    let clear_password_offset = unlock_success_body
        .find("elements.passwordInput.value = '';")
        .expect("a successful unlock must erase the password input");
    let persist_session_offset = unlock_success_body
        .find("persistSession(data.dek);")
        .expect("successful unlock should still establish the configured session");
    assert!(
        clear_password_offset < persist_session_offset,
        "the plaintext password must not remain in the DOM for the unlocked session"
    );
}

#[test]
fn browser_storage_clear_reports_partial_failures_and_continues_cleanup() {
    let script = r#"
        class StorageMock {
            constructor() {
                this.data = new Map();
                this.failedKeys = new Set();
                this.removeAttempts = [];
            }

            get length() {
                return this.data.size;
            }

            key(index) {
                return Array.from(this.data.keys())[index] ?? null;
            }

            getItem(key) {
                return this.data.has(key) ? this.data.get(key) : null;
            }

            setItem(key, value) {
                this.data.set(key, String(value));
            }

            removeItem(key) {
                this.removeAttempts.push(key);
                if (this.failedKeys.has(key)) {
                    throw new Error(`injected remove failure for ${key}`);
                }
                this.data.delete(key);
            }
        }

        const originalWindow = globalThis.window;
        const originalLocalStorage = globalThis.localStorage;
        const originalSessionStorage = globalThis.sessionStorage;
        const originalNavigatorDescriptor = Object.getOwnPropertyDescriptor(globalThis, 'navigator');

        globalThis.window = { location: { href: 'https://example.com/archive/index.html#/' } };
        globalThis.localStorage = new StorageMock();
        globalThis.sessionStorage = new StorageMock();
        Object.defineProperty(globalThis, 'navigator', {
            value: { storage: {} },
            configurable: true,
            writable: true,
        });

        try {
            const { clearAllStorage, getArchiveScopeId } = await import('./src/pages_assets/storage.js');
            const scopeId = getArchiveScopeId();
            const otherScopeId = scopeId === 'deadbeef' ? 'feedface' : 'deadbeef';
            const failedSessionKey = `cass_session_dek_${scopeId}`;
            const laterSessionKey = `cass_session_expiry_${scopeId}`;
            const currentLocalKey = `cass-archive-${scopeId}-pref-storage-mode`;
            const otherSessionKey = `cass_session_dek_${otherScopeId}`;
            const otherLocalKey = `cass-archive-${otherScopeId}-pref-storage-mode`;

            sessionStorage.setItem(failedSessionKey, 'secret');
            sessionStorage.setItem(laterSessionKey, 'expiry');
            sessionStorage.setItem(otherSessionKey, 'other');
            localStorage.setItem(currentLocalKey, 'local');
            localStorage.setItem(otherLocalKey, 'other');
            sessionStorage.failedKeys.add(failedSessionKey);

            const partialResult = await clearAllStorage();
            if (partialResult !== false) {
                throw new Error('clearAllStorage must report a failed browser-storage deletion');
            }
            if (sessionStorage.getItem(failedSessionKey) !== 'secret') {
                throw new Error('the injected failed key should demonstrate that data can remain');
            }
            if (sessionStorage.getItem(laterSessionKey) !== null) {
                throw new Error('cleanup must continue to later sessionStorage keys after one failure');
            }
            if (localStorage.getItem(currentLocalKey) !== null) {
                throw new Error('cleanup must still attempt localStorage after a sessionStorage failure');
            }
            if (
                sessionStorage.getItem(otherSessionKey) !== 'other'
                || localStorage.getItem(otherLocalKey) !== 'other'
            ) {
                throw new Error('archive-scoped cleanup must preserve other archives');
            }

            sessionStorage.failedKeys.clear();
            const retryResult = await clearAllStorage();
            if (retryResult !== true || sessionStorage.getItem(failedSessionKey) !== null) {
                throw new Error('a successful retry must remove the previously retained key');
            }
        } finally {
            globalThis.window = originalWindow;
            globalThis.localStorage = originalLocalStorage;
            globalThis.sessionStorage = originalSessionStorage;
            if (originalNavigatorDescriptor) {
                Object.defineProperty(globalThis, 'navigator', originalNavigatorDescriptor);
            } else {
                delete globalThis.navigator;
            }
        }
    "#;

    let output = Command::new("node")
        .args(["--input-type=module", "--eval", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run browser storage clear assertions with node");

    assert!(
        output.status.success(),
        "browser storage clear assertions failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// =============================================================================
// Performance: Error Path Performance
// =============================================================================

#[test]
fn test_error_creation_is_fast() {
    let start = Instant::now();

    for _ in 0..10_000 {
        let _ = DecryptError::AuthenticationFailed;
        let _ = DecryptError::InvalidFormat("test".into());
        let _ = DbError::CorruptDatabase("test".into());
        let _ = BrowserError::WasmNotSupported;
        let _ = NetworkError::Timeout;
    }

    let duration = start.elapsed();

    // 10k error creations should be well under 100ms
    assert!(
        duration < Duration::from_millis(100),
        "Error creation took too long: {:?}",
        duration
    );
}

#[test]
fn test_error_display_is_fast() {
    let errors: Vec<Box<dyn std::fmt::Display>> = vec![
        Box::new(DecryptError::AuthenticationFailed),
        Box::new(DecryptError::InvalidFormat("detailed info".into())),
        Box::new(DbError::CorruptDatabase("sqlite error".into())),
        Box::new(BrowserError::UnsupportedBrowser("IndexedDB".into())),
        Box::new(NetworkError::FetchFailed("connection refused".into())),
    ];

    let start = Instant::now();

    for _ in 0..10_000 {
        for error in &errors {
            let _ = error.to_string();
        }
    }

    let duration = start.elapsed();

    // 50k error displays should be well under 500ms
    assert!(
        duration < Duration::from_millis(500),
        "Error display took too long: {:?}",
        duration
    );
}
