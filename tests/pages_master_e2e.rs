//! Master E2E Test Suite for Pages Export Pipeline (P6.14)
//!
//! This comprehensive test suite validates the entire export-to-view workflow
//! with detailed logging for rapid debugging.
//!
//! # Test Categories
//!
//! - **Workflow Tests**: Full export → encrypt → bundle → verify pipeline
//! - **Authentication Tests**: Password, recovery key, multi-key-slot
//! - **Search Tests**: FTS functionality in exported archives
//! - **Edge Cases**: Large archives, secrets, corruption detection
//! - **Performance Assertions**: Timing guarantees
//!
//! # Running
//!
//! ```bash
//! # Run all master E2E tests
//! cargo test --test pages_master_e2e
//!
//! # Run with detailed logging
//! RUST_LOG=debug cargo test --test pages_master_e2e -- --nocapture
//!
//! # Run specific test
//! cargo test --test pages_master_e2e test_full_export_workflow
//! ```

use assert_cmd::cargo::cargo_bin_cmd;
use coding_agent_search::model::types::{Agent, AgentKind};
use coding_agent_search::pages::bundle::{BundleBuilder, BundleResult};
use coding_agent_search::pages::encrypt::{DecryptionEngine, EncryptionEngine, load_config};
use coding_agent_search::pages::export::{ExportEngine, ExportFilter, PathMode};
use coding_agent_search::pages::fts::escape_fts5_query;
use coding_agent_search::pages::key_management::{key_add_password, key_list, key_revoke};
use coding_agent_search::pages::verify::verify_bundle;
use coding_agent_search::storage::sqlite::SqliteStorage;
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tracing::{Level, debug, info, instrument};

#[path = "util/mod.rs"]
mod util;

use util::{ConversationFixtureBuilder, PerfMeasurement};

// =============================================================================
// Test Configuration
// =============================================================================

const TEST_PASSWORD: &str = "master-e2e-test-password";
const TEST_PASSWORD_2: &str = "secondary-password-for-multi-slot";
const TEST_RECOVERY_SECRET: &[u8] = b"master-e2e-recovery-secret-32bytes!";

/// Test configuration for the E2E suite.
#[derive(Debug, Clone)]
struct E2EConfig {
    /// Number of test conversations to generate.
    conversation_count: usize,
    /// Number of messages per conversation.
    messages_per_conversation: usize,
    /// Timeout for operations in milliseconds.
    timeout_ms: u64,
    /// Whether to capture screenshots on failure.
    capture_screenshots: bool,
    /// Enable verbose logging.
    verbose: bool,
}

impl Default for E2EConfig {
    fn default() -> Self {
        Self {
            conversation_count: 5,
            messages_per_conversation: 10,
            timeout_ms: 30000,
            capture_screenshots: true,
            verbose: std::env::var("RUST_LOG").is_ok(),
        }
    }
}

// =============================================================================
// Pipeline Artifacts
// =============================================================================

/// Artifacts from a complete pipeline run.
struct PipelineArtifacts {
    export_db_path: std::path::PathBuf,
    bundle: BundleResult,
    temp_dir: TempDir,
}

/// Build the complete pages export pipeline.
#[instrument(skip_all)]
fn build_pipeline(config: &E2EConfig) -> PipelineArtifacts {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    info!("Created temp directory: {}", temp_dir.path().display());
    debug!(
        "E2E config: timeout_ms={} capture_screenshots={} verbose={}",
        config.timeout_ms, config.capture_screenshots, config.verbose
    );

    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("Failed to create data directory");

    // Step 1: Setup database with fixtures
    debug!(
        "Step 1: Setting up database with {} conversations",
        config.conversation_count
    );
    let source_db_path = setup_test_db(&data_dir, config);
    info!("Database created at: {}", source_db_path.display());

    // Step 2: Export
    debug!("Step 2: Exporting conversations");
    let export_staging = temp_dir.path().join("export_staging");
    fs::create_dir_all(&export_staging).expect("Failed to create export staging directory");
    let export_db_path = export_staging.join("export.db");

    let filter = ExportFilter {
        agents: None,
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Relative,
    };

    let export_engine = ExportEngine::new(&source_db_path, &export_db_path, filter);
    let stats = export_engine
        .execute(
            |current, total| {
                if total > 0 {
                    debug!("Export progress: {}/{}", current, total);
                }
            },
            None,
        )
        .expect("Export failed");

    info!(
        "Export complete: {} conversations, {} messages",
        stats.conversations_processed, stats.messages_processed
    );

    // Step 3: Encrypt
    debug!("Step 3: Encrypting archive");
    let encrypt_dir = temp_dir.path().join("encrypt_staging");
    let mut enc_engine = EncryptionEngine::new(1024 * 1024).expect("valid chunk size"); // 1MB chunks

    enc_engine
        .add_password_slot(TEST_PASSWORD)
        .expect("Failed to add password slot");
    enc_engine
        .add_recovery_slot(TEST_RECOVERY_SECRET)
        .expect("Failed to add recovery slot");

    let _enc_config = enc_engine
        .encrypt_file(&export_db_path, &encrypt_dir, |phase, msg| {
            debug!("Encrypt phase {}: {}", phase, msg);
        })
        .expect("Encryption failed");

    assert!(
        encrypt_dir.join("config.json").exists(),
        "config.json should exist"
    );
    assert!(
        encrypt_dir.join("payload").exists(),
        "payload directory should exist"
    );
    info!("Encryption complete");

    // Step 4: Bundle
    debug!("Step 4: Building static site bundle");
    let bundle_dir = temp_dir.path().join("bundle");
    let builder = BundleBuilder::new()
        .title("Master E2E Test Archive")
        .description("Comprehensive test archive for E2E pipeline validation")
        .generate_qr(false)
        .recovery_secret(Some(TEST_RECOVERY_SECRET.to_vec()));

    let bundle = builder
        .build(&encrypt_dir, &bundle_dir, |phase, msg| {
            debug!("Bundle phase {}: {}", phase, msg);
        })
        .expect("Bundle failed");

    assert!(
        bundle.site_dir.join("index.html").exists(),
        "index.html should exist"
    );
    assert!(
        bundle.private_dir.join("recovery-secret.txt").exists(),
        "recovery-secret.txt should exist"
    );
    info!(
        "Bundle complete: site={}, private={}",
        bundle.site_dir.display(),
        bundle.private_dir.display()
    );

    PipelineArtifacts {
        export_db_path,
        bundle,
        temp_dir,
    }
}

/// Setup test database with conversation fixtures.
fn setup_test_db(data_dir: &Path, config: &E2EConfig) -> std::path::PathBuf {
    let db_path = data_dir.join("agent_search.db");
    let mut storage = SqliteStorage::open(&db_path).expect("Failed to open storage");

    // Create agent
    let agent = Agent {
        id: None,
        slug: "claude_code".to_string(),
        name: "Claude Code".to_string(),
        version: Some("1.0".to_string()),
        kind: AgentKind::Cli,
    };
    let agent_id = storage
        .ensure_agent(&agent)
        .expect("Failed to ensure agent");

    // Create workspace
    let workspace_path = Path::new("/home/user/projects/e2e-test");
    let workspace_id = Some(
        storage
            .ensure_workspace(workspace_path, None)
            .expect("Failed to ensure workspace"),
    );

    // Create conversations
    for i in 0..config.conversation_count {
        let conversation = ConversationFixtureBuilder::new("claude_code")
            .title(format!("E2E Test Conversation {}", i + 1))
            .workspace(workspace_path)
            .source_path(format!("/home/user/.claude/projects/test/session-{}.jsonl", i))
            .messages(config.messages_per_conversation)
            .with_content(0, format!("User query for test conversation {}", i + 1))
            .with_content(1, format!("Assistant response for conversation {}. This contains searchable content like function, debug, and optimize.", i + 1))
            .build_conversation();

        storage
            .insert_conversation_tree(agent_id, workspace_id, &conversation)
            .expect("Failed to insert conversation");
    }

    db_path
}

// =============================================================================
// Workflow Tests
// =============================================================================

#[test]
#[instrument]
fn test_full_export_workflow() {
    let _tracing = setup_test_tracing("test_full_export_workflow");
    info!("=== Full Export Workflow Test ===");

    let start = Instant::now();
    let config = E2EConfig::default();

    // Build complete pipeline
    let artifacts = build_pipeline(&config);

    // Verify bundle integrity
    let result = verify_bundle(&artifacts.bundle.site_dir, false).expect("Verification failed");
    assert_eq!(result.status, "valid", "Bundle should be valid");

    // Verify CLI verification
    let mut cmd = cargo_bin_cmd!("cass");
    cmd.arg("pages")
        .arg("--verify")
        .arg(&artifacts.bundle.site_dir)
        .arg("--json")
        .assert()
        .success();

    let duration = start.elapsed();
    info!("=== Full Export Workflow Test PASSED in {:?} ===", duration);
}

#[test]
#[instrument]
fn test_password_authentication_flow() {
    let _tracing = setup_test_tracing("test_password_authentication_flow");
    info!("=== Password Authentication Test ===");

    let config = E2EConfig::default();
    let artifacts = build_pipeline(&config);

    // Test valid password
    let enc_config = load_config(&artifacts.bundle.site_dir).expect("Failed to load config");
    let decryptor = DecryptionEngine::unlock_with_password(enc_config, TEST_PASSWORD)
        .expect("Should unlock with correct password");

    let decrypted_path = artifacts.temp_dir.path().join("decrypted.db");
    decryptor
        .decrypt_to_file(&artifacts.bundle.site_dir, &decrypted_path, |_, _| {})
        .expect("Decryption should succeed");

    // Verify decrypted content matches original
    assert_eq!(
        fs::read(&artifacts.export_db_path).unwrap(),
        fs::read(&decrypted_path).unwrap(),
        "Decrypted content should match original"
    );

    // Verify FTS search works on decrypted database
    let conn = Connection::open(&decrypted_path).expect("open decrypted db");
    let query = escape_fts5_query("optimize");
    let hit_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?",
            [query],
            |r| r.get(0),
        )
        .expect("fts query");
    assert!(hit_count > 0, "FTS should return matches after decrypt");

    // Test invalid password
    let enc_config = load_config(&artifacts.bundle.site_dir).expect("Failed to load config");
    let result = DecryptionEngine::unlock_with_password(enc_config, "wrong-password");
    assert!(result.is_err(), "Should fail with wrong password");

    info!("=== Password Authentication Test PASSED ===");
}

#[test]
#[instrument]
fn test_recovery_key_authentication() {
    let _tracing = setup_test_tracing("test_recovery_key_authentication");
    info!("=== Recovery Key Authentication Test ===");

    let config = E2EConfig::default();
    let artifacts = build_pipeline(&config);

    // Test valid recovery key
    let enc_config = load_config(&artifacts.bundle.site_dir).expect("Failed to load config");
    let decryptor = DecryptionEngine::unlock_with_recovery(enc_config, TEST_RECOVERY_SECRET)
        .expect("Should unlock with recovery key");

    let decrypted_path = artifacts.temp_dir.path().join("decrypted_recovery.db");
    decryptor
        .decrypt_to_file(&artifacts.bundle.site_dir, &decrypted_path, |_, _| {})
        .expect("Decryption with recovery key should succeed");

    // Verify content matches
    assert_eq!(
        fs::read(&artifacts.export_db_path).unwrap(),
        fs::read(&decrypted_path).unwrap(),
        "Recovery-decrypted content should match original"
    );

    // Test invalid recovery key
    let enc_config = load_config(&artifacts.bundle.site_dir).expect("Failed to load config");
    let result = DecryptionEngine::unlock_with_recovery(enc_config, b"wrong-recovery-key");
    assert!(result.is_err(), "Should fail with wrong recovery key");

    info!("=== Recovery Key Authentication Test PASSED ===");
}

#[test]
#[instrument]
fn test_recovery_secret_faster_than_password() {
    let _tracing = setup_test_tracing("test_recovery_secret_faster_than_password");
    info!("=== Recovery Secret Speed Comparison Test ===");

    let config = E2EConfig::default();
    let artifacts = build_pipeline(&config);

    // Measure password unlock time (uses Argon2id)
    let password_start = Instant::now();
    let enc_config = load_config(&artifacts.bundle.site_dir).expect("Failed to load config");
    let _decryptor_password = DecryptionEngine::unlock_with_password(enc_config, TEST_PASSWORD)
        .expect("Should unlock with password");
    let password_duration = password_start.elapsed();
    info!("Password unlock took: {:?}", password_duration);

    // Measure recovery secret unlock time (uses HKDF, should be faster)
    let recovery_start = Instant::now();
    let enc_config = load_config(&artifacts.bundle.site_dir).expect("Failed to load config");
    let _decryptor_recovery =
        DecryptionEngine::unlock_with_recovery(enc_config, TEST_RECOVERY_SECRET)
            .expect("Should unlock with recovery key");
    let recovery_duration = recovery_start.elapsed();
    info!("Recovery secret unlock took: {:?}", recovery_duration);

    // Recovery should be significantly faster (HKDF vs Argon2id)
    // Recovery uses HKDF which is nearly instant, while Argon2id takes 1-3 seconds
    assert!(
        recovery_duration < password_duration,
        "Recovery unlock ({:?}) should be faster than password unlock ({:?})",
        recovery_duration,
        password_duration
    );

    // Recovery should be at least 2x faster than password (typically 10-100x faster)
    let speedup = password_duration.as_secs_f64() / recovery_duration.as_secs_f64().max(0.001);
    info!("Recovery speedup factor: {:.1}x", speedup);
    assert!(
        speedup > 2.0,
        "Recovery should be at least 2x faster than password (got {:.1}x)",
        speedup
    );

    info!("=== Recovery Secret Speed Comparison Test PASSED ===");
}

#[test]
#[instrument]
fn test_multi_key_slot_management() {
    let _tracing = setup_test_tracing("test_multi_key_slot_management");
    info!("=== Multi-Key-Slot Management Test ===");

    let config = E2EConfig::default();
    let artifacts = build_pipeline(&config);
    let site_dir = &artifacts.bundle.site_dir;

    // Initial state: 2 slots (password + recovery)
    let list = key_list(site_dir).expect("Failed to list keys");
    assert_eq!(list.active_slots, 2, "Should start with 2 slots");
    info!("Initial slots: {}", list.active_slots);

    // Add second password slot
    let slot_id = key_add_password(site_dir, TEST_PASSWORD, TEST_PASSWORD_2)
        .expect("Failed to add second password");
    assert_eq!(slot_id, 2, "New slot should be ID 2");
    info!("Added password slot: {}", slot_id);

    // Verify 3 slots now
    let list = key_list(site_dir).expect("Failed to list keys");
    assert_eq!(list.active_slots, 3, "Should have 3 slots now");

    // Both passwords should work
    let config1 = load_config(site_dir).unwrap();
    assert!(
        DecryptionEngine::unlock_with_password(config1, TEST_PASSWORD).is_ok(),
        "Original password should work"
    );

    let config2 = load_config(site_dir).unwrap();
    assert!(
        DecryptionEngine::unlock_with_password(config2, TEST_PASSWORD_2).is_ok(),
        "Second password should work"
    );

    // Revoke original password
    let revoke = key_revoke(site_dir, TEST_PASSWORD_2, 0).expect("Failed to revoke password");
    assert_eq!(revoke.revoked_slot_id, 0);
    assert_eq!(revoke.remaining_slots, 2);
    info!("Revoked slot 0, remaining: {}", revoke.remaining_slots);

    // Original password should no longer work
    let config3 = load_config(site_dir).unwrap();
    assert!(
        DecryptionEngine::unlock_with_password(config3, TEST_PASSWORD).is_err(),
        "Original password should no longer work after revocation"
    );

    // Second password should still work
    let config4 = load_config(site_dir).unwrap();
    assert!(
        DecryptionEngine::unlock_with_password(config4, TEST_PASSWORD_2).is_ok(),
        "Second password should still work"
    );

    info!("=== Multi-Key-Slot Management Test PASSED ===");
}

#[test]
#[instrument]
fn test_corruption_detection() {
    let _tracing = setup_test_tracing("test_corruption_detection");
    info!("=== Corruption Detection Test ===");

    let config = E2EConfig::default();
    let artifacts = build_pipeline(&config);
    let site_dir = &artifacts.bundle.site_dir;

    // Baseline: bundle is valid
    let baseline = verify_bundle(site_dir, false).expect("Baseline verification failed");
    assert_eq!(baseline.status, "valid", "Baseline should be valid");
    info!("Baseline verification: {}", baseline.status);

    // Corrupt a payload chunk
    let payload_dir = site_dir.join("payload");
    let chunk = fs::read_dir(&payload_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().map(|e| e == "bin").unwrap_or(false))
        .expect("Should find payload chunk");

    info!("Corrupting chunk: {}", chunk.display());
    fs::write(&chunk, b"CORRUPTED DATA").expect("Failed to corrupt chunk");

    // Verification should now fail
    let result = verify_bundle(site_dir, false).expect("Verification should complete");
    assert_eq!(
        result.status, "invalid",
        "Corrupted bundle should be invalid"
    );
    info!("Corrupted verification: {}", result.status);

    info!("=== Corruption Detection Test PASSED ===");
}

#[test]
#[instrument]
fn test_large_archive_handling() {
    let _tracing = setup_test_tracing("test_large_archive_handling");
    info!("=== Large Archive Handling Test ===");

    // Configure for larger dataset
    let config = E2EConfig {
        conversation_count: 50,
        messages_per_conversation: 20,
        ..Default::default()
    };

    let start = Instant::now();
    let artifacts = build_pipeline(&config);
    let build_duration = start.elapsed();
    info!("Built large archive in {:?}", build_duration);

    // Verify it's still valid
    let result = verify_bundle(&artifacts.bundle.site_dir, false).expect("Verification failed");
    assert_eq!(result.status, "valid", "Large bundle should be valid");

    // Test decryption performance
    let decrypt_start = Instant::now();
    let enc_config = load_config(&artifacts.bundle.site_dir).expect("Failed to load config");
    let decryptor =
        DecryptionEngine::unlock_with_password(enc_config, TEST_PASSWORD).expect("Should unlock");

    let decrypted_path = artifacts.temp_dir.path().join("large_decrypted.db");
    decryptor
        .decrypt_to_file(&artifacts.bundle.site_dir, &decrypted_path, |_, _| {})
        .expect("Decryption should succeed");
    let decrypt_duration = decrypt_start.elapsed();

    info!("Decrypted large archive in {:?}", decrypt_duration);

    // Performance assertion: decryption should complete within timeout
    assert!(
        decrypt_duration < Duration::from_secs(30),
        "Decryption should complete within 30 seconds"
    );

    info!("=== Large Archive Handling Test PASSED ===");
}

/// Test with 100K messages (P6.5 exit criteria)
/// This test is marked as ignored because it takes significant time/resources.
/// Run with: cargo test --test pages_master_e2e test_xlarge_archive_100k -- --ignored
#[test]
#[ignore]
#[instrument]
fn test_xlarge_archive_100k() {
    let _tracing = setup_test_tracing("test_xlarge_archive_100k");
    info!("=== XLarge Archive (100K Messages) Test ===");

    // 1000 conversations * 100 messages = 100K messages
    let config = E2EConfig {
        conversation_count: 1000,
        messages_per_conversation: 100,
        timeout_ms: 300000, // 5 minute timeout
        ..Default::default()
    };

    let start = Instant::now();
    let artifacts = build_pipeline(&config);
    let build_duration = start.elapsed();
    info!("Built 100K message archive in {:?}", build_duration);

    // Verify bundle is valid
    let result = verify_bundle(&artifacts.bundle.site_dir, false).expect("Verification failed");
    assert_eq!(
        result.status, "valid",
        "100K message bundle should be valid"
    );

    // Verify bundle size is under GitHub Pages limit (1GB, but target <500MB)
    let site_size = dir_size(&artifacts.bundle.site_dir);
    info!("Bundle size: {} MB", site_size / (1024 * 1024));
    assert!(
        site_size < 1024 * 1024 * 1024,
        "Bundle should be under 1GB (GitHub Pages limit)"
    );

    // Test decryption completes within reasonable time
    let decrypt_start = Instant::now();
    let enc_config = load_config(&artifacts.bundle.site_dir).expect("Failed to load config");
    let decryptor =
        DecryptionEngine::unlock_with_password(enc_config, TEST_PASSWORD).expect("Should unlock");

    let decrypted_path = artifacts.temp_dir.path().join("xlarge_decrypted.db");
    decryptor
        .decrypt_to_file(&artifacts.bundle.site_dir, &decrypted_path, |_, _| {})
        .expect("Decryption should succeed");
    let decrypt_duration = decrypt_start.elapsed();
    info!("Decrypted 100K message archive in {:?}", decrypt_duration);

    // Decryption should complete within 2 minutes
    assert!(
        decrypt_duration < Duration::from_secs(120),
        "100K message decryption should complete within 2 minutes"
    );

    // Verify FTS search still works and is fast
    let conn = Connection::open(&decrypted_path).expect("open decrypted db");
    let search_start = Instant::now();
    let query = escape_fts5_query("optimize");
    let hit_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?",
            [query],
            |r| r.get(0),
        )
        .expect("fts query");
    let search_duration = search_start.elapsed();

    info!(
        "FTS search returned {} results in {:?}",
        hit_count, search_duration
    );
    assert!(
        search_duration < Duration::from_millis(500),
        "Search should complete within 500ms even on 100K messages"
    );

    info!("=== XLarge Archive (100K Messages) Test PASSED ===");
}

/// Calculate directory size recursively
fn dir_size(path: &Path) -> u64 {
    let mut size = 0;
    if path.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.filter_map(|e| e.ok()) {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                size += dir_size(&entry_path);
            } else if let Ok(metadata) = entry_path.metadata() {
                size += metadata.len();
            }
        }
    }
    size
}

#[test]
#[instrument]
fn test_empty_archive_handling() {
    let _tracing = setup_test_tracing("test_empty_archive_handling");
    info!("=== Empty Archive Handling Test ===");

    // Configure for minimal dataset
    let config = E2EConfig {
        conversation_count: 1,
        messages_per_conversation: 1,
        ..Default::default()
    };

    let artifacts = build_pipeline(&config);

    // Verify it's still valid
    let result = verify_bundle(&artifacts.bundle.site_dir, false).expect("Verification failed");
    assert_eq!(result.status, "valid", "Minimal bundle should be valid");

    info!("=== Empty Archive Handling Test PASSED ===");
}

#[test]
#[instrument]
fn test_export_with_filters() {
    let _tracing = setup_test_tracing("test_export_with_filters");
    info!("=== Export with Filters Test ===");

    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("Failed to create data directory");

    // Create DB with multiple agents
    let db_path = data_dir.join("agent_search.db");
    let mut storage = SqliteStorage::open(&db_path).expect("Failed to open storage");

    // Create two agents
    let claude_agent = Agent {
        id: None,
        slug: "claude_code".to_string(),
        name: "Claude Code".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };
    let claude_id = storage
        .ensure_agent(&claude_agent)
        .expect("ensure claude agent");

    let codex_agent = Agent {
        id: None,
        slug: "codex".to_string(),
        name: "Codex".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };
    let codex_id = storage
        .ensure_agent(&codex_agent)
        .expect("ensure codex agent");

    // Create workspace
    let workspace_path = Path::new("/home/user/projects/test");
    let workspace_id = Some(
        storage
            .ensure_workspace(workspace_path, None)
            .expect("ensure workspace"),
    );

    // Create conversations for each agent
    for agent_id in [claude_id, codex_id] {
        let agent_slug = if agent_id == claude_id {
            "claude_code"
        } else {
            "codex"
        };
        let conversation = ConversationFixtureBuilder::new(agent_slug)
            .title(format!("Conversation from {}", agent_slug))
            .workspace(workspace_path)
            .source_path(format!("/tmp/{}/session.jsonl", agent_slug))
            .messages(3)
            .build_conversation();

        storage
            .insert_conversation_tree(agent_id, workspace_id, &conversation)
            .expect("insert conversation");
    }

    // Export with filter for claude_code only
    let export_dir = temp_dir.path().join("filtered_export");
    fs::create_dir_all(&export_dir).expect("create export dir");
    let export_db_path = export_dir.join("export.db");

    let filter = ExportFilter {
        agents: Some(vec!["claude_code".to_string()]),
        workspaces: None,
        since: None,
        until: None,
        path_mode: PathMode::Relative,
    };

    let engine = ExportEngine::new(&db_path, &export_db_path, filter);
    let stats = engine.execute(|_, _| {}, None).expect("export");

    // Should only export 1 conversation (claude_code)
    assert_eq!(
        stats.conversations_processed, 1,
        "Should export only 1 conversation with agent filter"
    );
    info!(
        "Filtered export: {} conversations",
        stats.conversations_processed
    );

    info!("=== Export with Filters Test PASSED ===");
}

// =============================================================================
// Performance Tests
// =============================================================================

#[test]
#[instrument]
fn test_performance_benchmarks() {
    let _tracing = setup_test_tracing("test_performance_benchmarks");
    info!("=== Performance Benchmarks Test ===");

    let config = E2EConfig {
        conversation_count: 10,
        messages_per_conversation: 10,
        ..Default::default()
    };

    // Measure pipeline build time
    let perf = PerfMeasurement::measure(1, 3, || {
        let _artifacts = build_pipeline(&config);
    });

    perf.print_summary("Pipeline Build");

    // Performance assertions
    assert!(
        perf.mean() < Duration::from_secs(60),
        "Pipeline build should complete within 60 seconds on average"
    );

    assert!(
        perf.percentile(95.0) < Duration::from_secs(90),
        "Pipeline build p95 should be under 90 seconds"
    );

    info!("=== Performance Benchmarks Test PASSED ===");
}

// =============================================================================
// Test Utilities
// =============================================================================

fn setup_test_tracing(_test_name: &str) -> tracing::subscriber::DefaultGuard {
    let subscriber = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(Level::DEBUG)
        .with_target(false)
        .compact()
        .finish();

    tracing::subscriber::set_default(subscriber)
}

// =============================================================================
// E2E Logger Integration
// =============================================================================

use util::e2e_log::{E2eError, E2eLogger, E2ePhase, E2eRunSummary, E2eTestInfo};

/// Test result for collecting outcomes.
#[derive(Debug, Clone)]
pub struct TestOutcome {
    pub name: String,
    pub suite: String,
    pub status: TestStatus,
    pub duration: Duration,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestStatus {
    Passed,
    Failed,
    Skipped,
}

impl TestStatus {
    #[allow(dead_code)]
    fn as_str(&self) -> &'static str {
        match self {
            TestStatus::Passed => "pass",
            TestStatus::Failed => "fail",
            TestStatus::Skipped => "skip",
        }
    }
}

/// Run a test with E2eLogger instrumentation.
pub fn run_with_logging<F>(
    logger: &E2eLogger,
    name: &str,
    suite: &str,
    file: &str,
    line: u32,
    test_fn: F,
) -> TestOutcome
where
    F: FnOnce() -> Result<(), Box<dyn std::error::Error>>,
{
    let test_info = E2eTestInfo::new(name, suite, file, line);

    // Emit test_start event
    let _ = logger.test_start(&test_info);

    let start = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test_fn));
    let duration = start.elapsed();

    let (status, error) = match result {
        Ok(Ok(())) => (TestStatus::Passed, None),
        Ok(Err(e)) => (TestStatus::Failed, Some(e.to_string())),
        Err(panic) => {
            let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic.downcast_ref::<String>() {
                s.clone()
            } else {
                "Unknown panic".to_string()
            };
            (TestStatus::Failed, Some(msg))
        }
    };

    // Emit test_end event
    match status {
        TestStatus::Passed => {
            let _ = logger.test_pass(&test_info, duration.as_millis() as u64, None);
        }
        TestStatus::Failed => {
            let _ = logger.test_fail(
                &test_info,
                duration.as_millis() as u64,
                None,
                E2eError {
                    message: error.clone().unwrap_or_default(),
                    error_type: Some("TestFailure".to_string()),
                    stack: None,
                    context: None,
                },
            );
        }
        TestStatus::Skipped => {
            let _ = logger.test_skip(&test_info);
        }
    }

    TestOutcome {
        name: name.to_string(),
        suite: suite.to_string(),
        status,
        duration,
        error,
    }
}

// =============================================================================
// HTML Report Generation
// =============================================================================

/// Generate an HTML test report from collected outcomes.
pub fn generate_html_report(outcomes: &[TestOutcome], total_duration: Duration) -> String {
    let passed = outcomes
        .iter()
        .filter(|o| o.status == TestStatus::Passed)
        .count();
    let failed = outcomes
        .iter()
        .filter(|o| o.status == TestStatus::Failed)
        .count();
    let skipped = outcomes
        .iter()
        .filter(|o| o.status == TestStatus::Skipped)
        .count();
    let total = outcomes.len();

    let test_rows: String = outcomes
        .iter()
        .map(|o| {
            let status_class = match o.status {
                TestStatus::Passed => "passed",
                TestStatus::Failed => "failed",
                TestStatus::Skipped => "skipped",
            };
            let status_icon = match o.status {
                TestStatus::Passed => "✓",
                TestStatus::Failed => "✗",
                TestStatus::Skipped => "⊘",
            };
            let error_row = if let Some(ref err) = o.error {
                format!(
                    r#"<tr class="error-row"><td colspan="4"><pre>{}</pre></td></tr>"#,
                    html_escape(err)
                )
            } else {
                String::new()
            };
            format!(
                r#"<tr class="test-row {status_class}">
                    <td class="status">{status_icon}</td>
                    <td class="name">{name}</td>
                    <td class="suite">{suite}</td>
                    <td class="duration">{duration:.2}ms</td>
                </tr>{error_row}"#,
                status_class = status_class,
                status_icon = status_icon,
                name = html_escape(&o.name),
                suite = html_escape(&o.suite),
                duration = o.duration.as_secs_f64() * 1000.0,
                error_row = error_row,
            )
        })
        .collect();

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>E2E Test Report - Master Suite</title>
    <style>
        :root {{
            --bg-primary: #1a1b26;
            --bg-secondary: #24283b;
            --text-primary: #c0caf5;
            --text-secondary: #565f89;
            --green: #9ece6a;
            --red: #f7768e;
            --yellow: #e0af68;
            --blue: #7aa2f7;
        }}
        * {{ box-sizing: border-box; margin: 0; padding: 0; }}
        body {{
            font-family: 'JetBrains Mono', 'Fira Code', monospace;
            background: var(--bg-primary);
            color: var(--text-primary);
            line-height: 1.6;
            padding: 2rem;
        }}
        .header {{
            text-align: center;
            margin-bottom: 2rem;
            padding-bottom: 1rem;
            border-bottom: 1px solid var(--bg-secondary);
        }}
        h1 {{
            color: var(--blue);
            font-size: 1.8rem;
            margin-bottom: 0.5rem;
        }}
        .timestamp {{
            color: var(--text-secondary);
            font-size: 0.9rem;
        }}
        .summary {{
            display: flex;
            justify-content: center;
            gap: 2rem;
            margin-bottom: 2rem;
            flex-wrap: wrap;
        }}
        .stat {{
            background: var(--bg-secondary);
            padding: 1rem 2rem;
            border-radius: 8px;
            text-align: center;
        }}
        .stat-value {{
            font-size: 2rem;
            font-weight: bold;
        }}
        .stat-label {{
            color: var(--text-secondary);
            font-size: 0.8rem;
            text-transform: uppercase;
        }}
        .stat.passed .stat-value {{ color: var(--green); }}
        .stat.failed .stat-value {{ color: var(--red); }}
        .stat.skipped .stat-value {{ color: var(--yellow); }}
        .stat.total .stat-value {{ color: var(--blue); }}
        table {{
            width: 100%;
            border-collapse: collapse;
            margin-top: 1rem;
        }}
        th {{
            background: var(--bg-secondary);
            padding: 0.75rem;
            text-align: left;
            color: var(--text-secondary);
            text-transform: uppercase;
            font-size: 0.8rem;
        }}
        td {{
            padding: 0.75rem;
            border-bottom: 1px solid var(--bg-secondary);
        }}
        .test-row.passed {{ background: rgba(158, 206, 106, 0.1); }}
        .test-row.failed {{ background: rgba(247, 118, 142, 0.1); }}
        .test-row.skipped {{ background: rgba(224, 175, 104, 0.1); }}
        .status {{ width: 30px; text-align: center; font-size: 1.2rem; }}
        .passed .status {{ color: var(--green); }}
        .failed .status {{ color: var(--red); }}
        .skipped .status {{ color: var(--yellow); }}
        .name {{ font-weight: 500; }}
        .suite {{ color: var(--text-secondary); }}
        .duration {{ text-align: right; font-variant-numeric: tabular-nums; }}
        .error-row td {{
            background: rgba(247, 118, 142, 0.05);
            border-left: 3px solid var(--red);
        }}
        .error-row pre {{
            color: var(--red);
            font-size: 0.85rem;
            white-space: pre-wrap;
            word-break: break-word;
            padding: 0.5rem;
            background: var(--bg-secondary);
            border-radius: 4px;
            max-height: 200px;
            overflow: auto;
        }}
        .footer {{
            margin-top: 2rem;
            padding-top: 1rem;
            border-top: 1px solid var(--bg-secondary);
            text-align: center;
            color: var(--text-secondary);
            font-size: 0.8rem;
        }}
    </style>
</head>
<body>
    <div class="header">
        <h1>🧪 E2E Test Report</h1>
        <p class="timestamp">Generated: {timestamp}</p>
    </div>

    <div class="summary">
        <div class="stat passed">
            <div class="stat-value">{passed}</div>
            <div class="stat-label">Passed</div>
        </div>
        <div class="stat failed">
            <div class="stat-value">{failed}</div>
            <div class="stat-label">Failed</div>
        </div>
        <div class="stat skipped">
            <div class="stat-value">{skipped}</div>
            <div class="stat-label">Skipped</div>
        </div>
        <div class="stat total">
            <div class="stat-value">{total}</div>
            <div class="stat-label">Total</div>
        </div>
        <div class="stat">
            <div class="stat-value">{duration:.2}s</div>
            <div class="stat-label">Duration</div>
        </div>
    </div>

    <table>
        <thead>
            <tr>
                <th></th>
                <th>Test Name</th>
                <th>Suite</th>
                <th style="text-align: right">Duration</th>
            </tr>
        </thead>
        <tbody>
            {test_rows}
        </tbody>
    </table>

    <div class="footer">
        <p>cass E2E Test Suite • Master Pipeline Tests</p>
    </div>
</body>
</html>"#,
        timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
        passed = passed,
        failed = failed,
        skipped = skipped,
        total = total,
        duration = total_duration.as_secs_f64(),
        test_rows = test_rows,
    )
}

/// Escape HTML entities.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

// =============================================================================
// Master Test Runner (Programmatic Execution)
// =============================================================================

/// Macro to run a workflow test with correct line number capture.
///
/// Using a macro instead of a function ensures `line!()` captures the call site.
macro_rules! run_workflow_test {
    ($logger:expr, $name:expr, $f:expr) => {
        run_with_logging($logger, $name, "pages_master_e2e", file!(), line!(), $f)
    };
}

/// Run all master E2E tests programmatically with comprehensive logging.
///
/// This function is designed to be called from a binary or integration test
/// to run all tests with E2eLogger instrumentation and generate reports.
///
/// # Example
///
/// ```ignore
/// let report = run_master_suite()?;
/// fs::write("test-results/e2e/report.html", report.html)?;
/// ```
#[allow(dead_code)]
pub fn run_master_suite() -> std::io::Result<MasterSuiteReport> {
    let logger = E2eLogger::new("rust")?;
    logger.run_start(None)?;

    let suite_start = Instant::now();
    let mut outcomes = Vec::new();

    // Phase 1: Workflow Tests
    let phase = E2ePhase {
        name: "Workflow Tests".to_string(),
        description: Some("Full export pipeline validation".to_string()),
    };
    logger.phase_start(&phase)?;
    let phase_start = Instant::now();

    outcomes.push(run_workflow_test!(
        &logger,
        "test_full_export_workflow",
        || {
            let config = E2EConfig::default();
            let artifacts = build_pipeline(&config);
            let result = verify_bundle(&artifacts.bundle.site_dir, false)?;
            if result.status != "valid" {
                return Err(format!("Bundle validation failed: {}", result.status).into());
            }
            Ok(())
        }
    ));

    outcomes.push(run_workflow_test!(
        &logger,
        "test_empty_archive_handling",
        || {
            let config = E2EConfig {
                conversation_count: 1,
                messages_per_conversation: 1,
                ..Default::default()
            };
            let artifacts = build_pipeline(&config);
            let result = verify_bundle(&artifacts.bundle.site_dir, false)?;
            if result.status != "valid" {
                return Err(format!("Minimal bundle validation failed: {}", result.status).into());
            }
            Ok(())
        }
    ));

    logger.phase_end(&phase, phase_start.elapsed().as_millis() as u64)?;

    // Phase 2: Authentication Tests
    let phase = E2ePhase {
        name: "Authentication Tests".to_string(),
        description: Some("Password and recovery key validation".to_string()),
    };
    logger.phase_start(&phase)?;
    let phase_start = Instant::now();

    outcomes.push(run_workflow_test!(
        &logger,
        "test_password_authentication",
        || {
            let config = E2EConfig::default();
            let artifacts = build_pipeline(&config);
            let enc_config = load_config(&artifacts.bundle.site_dir)?;
            let _decryptor = DecryptionEngine::unlock_with_password(enc_config, TEST_PASSWORD)
                .map_err(|e| format!("Password unlock failed: {:?}", e))?;
            Ok(())
        }
    ));

    outcomes.push(run_workflow_test!(
        &logger,
        "test_recovery_key_authentication",
        || {
            let config = E2EConfig::default();
            let artifacts = build_pipeline(&config);
            let enc_config = load_config(&artifacts.bundle.site_dir)?;
            let _decryptor =
                DecryptionEngine::unlock_with_recovery(enc_config, TEST_RECOVERY_SECRET)
                    .map_err(|e| format!("Recovery unlock failed: {:?}", e))?;
            Ok(())
        }
    ));

    logger.phase_end(&phase, phase_start.elapsed().as_millis() as u64)?;

    // Phase 3: Security Tests
    let phase = E2ePhase {
        name: "Security Tests".to_string(),
        description: Some("Key management and corruption detection".to_string()),
    };
    logger.phase_start(&phase)?;
    let phase_start = Instant::now();

    outcomes.push(run_workflow_test!(
        &logger,
        "test_invalid_password_rejected",
        || {
            let config = E2EConfig::default();
            let artifacts = build_pipeline(&config);
            let enc_config = load_config(&artifacts.bundle.site_dir)?;
            let result = DecryptionEngine::unlock_with_password(enc_config, "wrong-password");
            if result.is_ok() {
                return Err("Should have rejected invalid password".into());
            }
            Ok(())
        }
    ));

    logger.phase_end(&phase, phase_start.elapsed().as_millis() as u64)?;

    let total_duration = suite_start.elapsed();

    // Generate summary
    let passed = outcomes
        .iter()
        .filter(|o| o.status == TestStatus::Passed)
        .count() as u32;
    let failed = outcomes
        .iter()
        .filter(|o| o.status == TestStatus::Failed)
        .count() as u32;
    let skipped = outcomes
        .iter()
        .filter(|o| o.status == TestStatus::Skipped)
        .count() as u32;

    let summary = E2eRunSummary {
        total: outcomes.len() as u32,
        passed,
        failed,
        skipped,
        flaky: None,
        duration_ms: total_duration.as_millis() as u64,
    };

    let exit_code = if failed > 0 { 1 } else { 0 };
    logger.run_end(summary, exit_code)?;

    // Generate HTML report
    let html = generate_html_report(&outcomes, total_duration);

    Ok(MasterSuiteReport {
        outcomes,
        total_duration,
        jsonl_path: logger.output_path().clone(),
        html,
        exit_code,
    })
}

/// Report from running the master test suite.
#[derive(Debug)]
pub struct MasterSuiteReport {
    pub outcomes: Vec<TestOutcome>,
    pub total_duration: Duration,
    pub jsonl_path: std::path::PathBuf,
    pub html: String,
    pub exit_code: i32,
}

impl MasterSuiteReport {
    /// Write the HTML report to a file.
    pub fn write_html(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        fs::write(path, &self.html)
    }

    /// Returns true if all tests passed.
    pub fn all_passed(&self) -> bool {
        self.exit_code == 0
    }

    /// Get count of tests by status.
    pub fn count_by_status(&self, status: TestStatus) -> usize {
        self.outcomes.iter().filter(|o| o.status == status).count()
    }
}

// =============================================================================
// Test for the Test Runner
// =============================================================================

#[test]
fn test_master_suite_runner() {
    // Run the master suite programmatically
    let report = run_master_suite().expect("Failed to run master suite");

    // Verify we got results
    assert!(!report.outcomes.is_empty(), "Should have test outcomes");

    // Verify JSONL was created
    assert!(
        report.jsonl_path.exists(),
        "JSONL log file should exist at {:?}",
        report.jsonl_path
    );

    // Verify HTML was generated
    assert!(
        report.html.contains("E2E Test Report"),
        "HTML should contain report title"
    );
    assert!(
        report.html.contains("Passed"),
        "HTML should contain pass count"
    );

    // Write HTML report to test-results
    let report_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-results")
        .join("e2e");
    fs::create_dir_all(&report_dir).ok();

    let html_path = report_dir.join("master_e2e_report.html");
    report
        .write_html(&html_path)
        .expect("Failed to write HTML report");

    println!("📊 HTML Report: {}", html_path.display());
    println!("📄 JSONL Log: {}", report.jsonl_path.display());
    println!(
        "✅ Passed: {} | ❌ Failed: {} | ⊘ Skipped: {}",
        report.count_by_status(TestStatus::Passed),
        report.count_by_status(TestStatus::Failed),
        report.count_by_status(TestStatus::Skipped)
    );

    // Don't fail the test if some tests failed - we want to see the report
    // In CI, we'd assert all_passed() instead
}
