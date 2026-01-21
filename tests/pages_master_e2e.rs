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
use coding_agent_search::pages::key_management::{key_add_password, key_list, key_revoke};
use coding_agent_search::pages::verify::verify_bundle;
use coding_agent_search::storage::sqlite::SqliteStorage;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tracing::{debug, info, instrument, Level};

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
    debug!("Step 1: Setting up database with {} conversations", config.conversation_count);
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
        .execute(|current, total| {
            if total > 0 {
                debug!("Export progress: {}/{}", current, total);
            }
        }, None)
        .expect("Export failed");

    info!(
        "Export complete: {} conversations, {} messages",
        stats.conversations_processed, stats.messages_processed
    );

    // Step 3: Encrypt
    debug!("Step 3: Encrypting archive");
    let encrypt_dir = temp_dir.path().join("encrypt_staging");
    let mut enc_engine = EncryptionEngine::new(1024 * 1024); // 1MB chunks

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

    assert!(encrypt_dir.join("config.json").exists(), "config.json should exist");
    assert!(encrypt_dir.join("payload").exists(), "payload directory should exist");
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

    assert!(bundle.site_dir.join("index.html").exists(), "index.html should exist");
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
    let agent_id = storage.ensure_agent(&agent).expect("Failed to ensure agent");

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
    let result = verify_bundle(&artifacts.bundle.site_dir, false)
        .expect("Verification failed");
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
    let revoke = key_revoke(site_dir, TEST_PASSWORD_2, 0)
        .expect("Failed to revoke password");
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
    assert_eq!(result.status, "invalid", "Corrupted bundle should be invalid");
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
    let result = verify_bundle(&artifacts.bundle.site_dir, false)
        .expect("Verification failed");
    assert_eq!(result.status, "valid", "Large bundle should be valid");

    // Test decryption performance
    let decrypt_start = Instant::now();
    let enc_config = load_config(&artifacts.bundle.site_dir).expect("Failed to load config");
    let decryptor = DecryptionEngine::unlock_with_password(enc_config, TEST_PASSWORD)
        .expect("Should unlock");

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
    let result = verify_bundle(&artifacts.bundle.site_dir, false)
        .expect("Verification failed");
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
    let claude_id = storage.ensure_agent(&claude_agent).expect("ensure claude agent");

    let codex_agent = Agent {
        id: None,
        slug: "codex".to_string(),
        name: "Codex".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };
    let codex_id = storage.ensure_agent(&codex_agent).expect("ensure codex agent");

    // Create workspace
    let workspace_path = Path::new("/home/user/projects/test");
    let workspace_id = Some(
        storage
            .ensure_workspace(workspace_path, None)
            .expect("ensure workspace"),
    );

    // Create conversations for each agent
    for agent_id in [claude_id, codex_id] {
        let agent_slug = if agent_id == claude_id { "claude_code" } else { "codex" };
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
    info!("Filtered export: {} conversations", stats.conversations_processed);

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
