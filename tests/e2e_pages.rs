//! End-to-end integration tests for the Pages export pipeline (P6.5).
//!
//! This module validates the complete workflow:
//! - Export → Encrypt → Bundle → Verify → Decrypt
//!
//! # Running
//!
//! ```bash
//! # Run all pages E2E tests
//! cargo test --test e2e_pages
//!
//! # Run with detailed logging
//! RUST_LOG=debug cargo test --test e2e_pages -- --nocapture
//!
//! # Run specific test
//! cargo test --test e2e_pages test_full_export_pipeline_password_only
//! ```

use coding_agent_search::model::types::{Agent, AgentKind};
use coding_agent_search::pages::bundle::{BundleBuilder, BundleResult};
use coding_agent_search::pages::encrypt::{DecryptionEngine, EncryptionEngine, load_config};
use coding_agent_search::pages::export::{ExportEngine, ExportFilter, PathMode};
use coding_agent_search::pages::verify::verify_bundle;
use coding_agent_search::storage::sqlite::SqliteStorage;
use frankensqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tempfile::TempDir;

#[path = "util/mod.rs"]
mod util;

use util::ConversationFixtureBuilder;
use util::e2e_log::PhaseTracker;

// =============================================================================
// Test Constants
// =============================================================================

const TEST_PASSWORD: &str = "test-password-123!";
const TEST_RECOVERY_SECRET: &[u8] = b"recovery-secret-32bytes-padding!";
const CHUNK_SIZE: usize = 1024 * 1024; // 1 MB chunks

// =============================================================================
// E2E Logger Support
// =============================================================================

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_pages", test_name)
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Setup a test database with conversations.
fn setup_test_db(data_dir: &Path, conversation_count: usize) -> std::path::PathBuf {
    let db_path = data_dir.join("agent_search.db");

    let mut storage = SqliteStorage::open(&db_path).expect("Failed to open storage");

    // Create agent
    let agent = Agent {
        id: None,
        slug: "claude_code".to_string(),
        name: "Claude Code".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };
    let agent_id = storage.ensure_agent(&agent).expect("ensure agent");

    // Create workspace
    let workspace_path = Path::new("/home/user/projects/test");
    let workspace_id = Some(
        storage
            .ensure_workspace(workspace_path, None)
            .expect("ensure workspace"),
    );

    // Create conversations
    for i in 0..conversation_count {
        let conversation = ConversationFixtureBuilder::new("claude_code")
            .title(format!("Test Conversation {}", i))
            .workspace(workspace_path)
            .source_path(format!(
                "/home/user/.claude/projects/test/session-{}.jsonl",
                i
            ))
            .messages(10)
            .with_content(0, format!("User message {} - requesting help with code", i))
            .with_content(1, format!("Assistant response {} - here's the solution", i))
            .build_conversation();

        storage
            .insert_conversation_tree(agent_id, workspace_id, &conversation)
            .expect("Failed to insert conversation");
    }

    db_path
}

/// Build the complete pipeline and return artifacts.
struct PipelineArtifacts {
    export_db_path: std::path::PathBuf,
    bundle: BundleResult,
    _temp_dir: TempDir, // Keep alive for duration of test
}

fn build_full_pipeline(
    conversation_count: usize,
    include_password: bool,
    include_recovery: bool,
) -> PipelineArtifacts {
    let tracker = tracker_for("build_full_pipeline");
    let _trace_guard = tracker.trace_env_guard();
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("Failed to create data directory");

    // Step 1: Setup database
    let start = tracker.start(
        "setup_database",
        Some("Create test database with conversations"),
    );
    let source_db_path = setup_test_db(&data_dir, conversation_count);
    tracker.end(
        "setup_database",
        Some("Create test database with conversations"),
        start,
    );

    // Step 2: Export
    let start = tracker.start("export", Some("Export conversations to staging database"));
    let export_staging = temp_dir.path().join("export_staging");
    fs::create_dir_all(&export_staging).expect("Failed to create export staging");
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
        .execute(|_, _| {}, None)
        .expect("Export failed");
    assert!(
        stats.conversations_processed > 0,
        "Should export at least one conversation"
    );
    tracker.end(
        "export",
        Some("Export conversations to staging database"),
        start,
    );

    // Step 3: Encrypt
    let start = tracker.start("encrypt", Some("Encrypt exported database with AES-GCM"));
    let encrypt_dir = temp_dir.path().join("encrypt_staging");
    let mut enc_engine = EncryptionEngine::new(CHUNK_SIZE);

    if include_password {
        enc_engine
            .add_password_slot(TEST_PASSWORD)
            .expect("Failed to add password slot");
    }

    if include_recovery {
        enc_engine
            .add_recovery_slot(TEST_RECOVERY_SECRET)
            .expect("Failed to add recovery slot");
    }

    let _enc_config = enc_engine
        .encrypt_file(&export_db_path, &encrypt_dir, |_, _| {})
        .expect("Encryption failed");
    tracker.end(
        "encrypt",
        Some("Encrypt exported database with AES-GCM"),
        start,
    );

    // Step 4: Bundle
    let start = tracker.start("bundle", Some("Create deployable web bundle"));
    let bundle_dir = temp_dir.path().join("bundle");
    let mut builder = BundleBuilder::new()
        .title("E2E Test Archive")
        .description("Test archive for integration tests")
        .generate_qr(false);

    if include_recovery {
        builder = builder.recovery_secret(Some(TEST_RECOVERY_SECRET.to_vec()));
    }

    let bundle = builder
        .build(&encrypt_dir, &bundle_dir, |_, _| {})
        .expect("Bundle failed");
    tracker.end("bundle", Some("Create deployable web bundle"), start);

    tracker.flush();

    PipelineArtifacts {
        export_db_path,
        bundle,
        _temp_dir: temp_dir,
    }
}

// =============================================================================
// Test: Full Export Pipeline (Password Only)
// =============================================================================

/// Test the complete export pipeline with password-only authentication.
#[test]
fn test_full_export_pipeline_password_only() {
    let tracker = tracker_for("test_full_export_pipeline_password_only");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_full_export_pipeline_password_only\",\"status\":\"START\"}}");

    let artifacts = build_full_pipeline(5, true, false);

    // Verify bundle structure
    let verify_start = tracker.start("verify_structure", Some("Validate bundle artifacts exist"));
    let site = &artifacts.bundle.site_dir;
    assert!(site.join("index.html").exists(), "index.html should exist");
    assert!(site.join("sw.js").exists(), "sw.js should exist");
    assert!(
        site.join("config.json").exists(),
        "config.json should exist"
    );
    assert!(
        site.join("payload").exists(),
        "payload directory should exist"
    );

    // Verify config.json has single key slot
    let config_str = fs::read_to_string(site.join("config.json")).expect("read config");
    let config: serde_json::Value = serde_json::from_str(&config_str).expect("parse config");
    let slots = config.get("key_slots").expect("key_slots field");
    assert_eq!(slots.as_array().unwrap().len(), 1, "Should have 1 key slot");
    assert_eq!(
        slots[0].get("kdf").unwrap().as_str().unwrap(),
        "argon2id",
        "Should use argon2id KDF"
    );

    tracker.end(
        "verify_structure",
        Some("Validate bundle artifacts exist"),
        verify_start,
    );
    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_full_export_pipeline_password_only\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

// =============================================================================
// Test: Full Export Pipeline (Password + Recovery)
// =============================================================================

/// Test the complete export pipeline with dual authentication (password + recovery).
#[test]
fn test_full_export_pipeline_dual_auth() {
    let start = Instant::now();
    eprintln!("{{\"test\":\"test_full_export_pipeline_dual_auth\",\"status\":\"START\"}}");

    let artifacts = build_full_pipeline(3, true, true);

    // Verify config.json has two key slots
    let site = &artifacts.bundle.site_dir;
    let config_str = fs::read_to_string(site.join("config.json")).expect("read config");
    let config: serde_json::Value = serde_json::from_str(&config_str).expect("parse config");
    let slots = config.get("key_slots").expect("key_slots field");
    let slots_arr = slots.as_array().unwrap();
    assert_eq!(slots_arr.len(), 2, "Should have 2 key slots");

    // Verify first slot is password (argon2id)
    assert_eq!(
        slots_arr[0].get("kdf").unwrap().as_str().unwrap(),
        "argon2id"
    );

    // Verify second slot is recovery (hkdf-sha256)
    assert_eq!(
        slots_arr[1].get("kdf").unwrap().as_str().unwrap(),
        "hkdf-sha256"
    );

    // Verify private directory has recovery secret
    assert!(
        artifacts
            .bundle
            .private_dir
            .join("recovery-secret.txt")
            .exists(),
        "recovery-secret.txt should exist"
    );

    eprintln!(
        "{{\"test\":\"test_full_export_pipeline_dual_auth\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        start.elapsed().as_millis()
    );
}

// =============================================================================
// Test: Integrity and Decrypt Roundtrip
// =============================================================================

/// Test that decrypted payload matches original export database.
#[test]
fn test_integrity_decrypt_roundtrip_password() {
    let tracker = tracker_for("test_integrity_decrypt_roundtrip_password");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_integrity_decrypt_roundtrip_password\",\"status\":\"START\"}}");

    let temp_dir = TempDir::new().unwrap();
    let artifacts = build_full_pipeline(2, true, true);

    // Decrypt with password
    let decrypt_start = tracker.start(
        "decrypt_password",
        Some("Decrypt payload using password-derived key"),
    );
    let config = load_config(&artifacts.bundle.site_dir).expect("load config");
    let decryptor =
        DecryptionEngine::unlock_with_password(config, TEST_PASSWORD).expect("unlock password");
    let decrypted_path = temp_dir.path().join("decrypted_password.db");
    decryptor
        .decrypt_to_file(&artifacts.bundle.site_dir, &decrypted_path, |_, _| {})
        .expect("decrypt with password");

    // Verify bytes match
    let original = fs::read(&artifacts.export_db_path).expect("read original");
    let decrypted = fs::read(&decrypted_path).expect("read decrypted");
    assert_eq!(
        original, decrypted,
        "Decrypted content should match original"
    );

    tracker.end(
        "decrypt_password",
        Some("Decrypt payload using password-derived key"),
        decrypt_start,
    );
    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_integrity_decrypt_roundtrip_password\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

/// Test that decrypted payload matches original using recovery key.
#[test]
fn test_integrity_decrypt_roundtrip_recovery() {
    let tracker = tracker_for("test_integrity_decrypt_roundtrip_recovery");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_integrity_decrypt_roundtrip_recovery\",\"status\":\"START\"}}");

    let temp_dir = TempDir::new().unwrap();
    let artifacts = build_full_pipeline(2, true, true);

    // Decrypt with recovery key
    let decrypt_start = tracker.start(
        "decrypt_recovery",
        Some("Decrypt payload using recovery secret"),
    );
    let config = load_config(&artifacts.bundle.site_dir).expect("load config");
    let decryptor = DecryptionEngine::unlock_with_recovery(config, TEST_RECOVERY_SECRET)
        .expect("unlock recovery");
    let decrypted_path = temp_dir.path().join("decrypted_recovery.db");
    decryptor
        .decrypt_to_file(&artifacts.bundle.site_dir, &decrypted_path, |_, _| {})
        .expect("decrypt with recovery");
    tracker.end(
        "decrypt_recovery",
        Some("Decrypt payload using recovery secret"),
        decrypt_start,
    );

    // Verify bytes match
    let verify_start = tracker.start(
        "verify_content",
        Some("Compare decrypted content with original"),
    );
    let original = fs::read(&artifacts.export_db_path).expect("read original");
    let decrypted = fs::read(&decrypted_path).expect("read decrypted");
    assert_eq!(
        original, decrypted,
        "Decrypted content should match original"
    );
    tracker.end(
        "verify_content",
        Some("Compare decrypted content with original"),
        verify_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_integrity_decrypt_roundtrip_recovery\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

// =============================================================================
// Test: Tampering Detection
// =============================================================================

/// Test that tampering with a chunk fails authentication.
#[test]
fn test_tampering_fails_authentication() {
    let tracker = tracker_for("test_tampering_fails_authentication");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_tampering_fails_authentication\",\"status\":\"START\"}}");

    let artifacts = build_full_pipeline(2, true, false);
    let site_dir = &artifacts.bundle.site_dir;

    // Baseline: verify passes
    let phase_start = tracker.start(
        "verify_baseline",
        Some("Verify bundle is valid before tampering"),
    );
    let baseline = verify_bundle(site_dir, false).expect("verify baseline");
    assert_eq!(baseline.status, "valid", "Baseline should be valid");
    tracker.end(
        "verify_baseline",
        Some("Verify bundle is valid before tampering"),
        phase_start,
    );

    // Find and corrupt a payload chunk
    let phase_start = tracker.start(
        "corrupt_chunk",
        Some("Modify payload chunk to simulate tampering"),
    );
    let payload_dir = site_dir.join("payload");
    let chunk = fs::read_dir(&payload_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().map(|e| e == "bin").unwrap_or(false))
        .expect("payload chunk");
    fs::write(&chunk, b"corrupted payload data").expect("corrupt chunk");
    tracker.end(
        "corrupt_chunk",
        Some("Modify payload chunk to simulate tampering"),
        phase_start,
    );

    // Verify should now detect corruption
    let phase_start = tracker.start(
        "verify_corruption_detected",
        Some("Confirm verification detects tampering"),
    );
    let result = verify_bundle(site_dir, false).expect("verify after corruption");
    assert_eq!(
        result.status, "invalid",
        "Corrupted bundle should be invalid"
    );
    tracker.end(
        "verify_corruption_detected",
        Some("Confirm verification detects tampering"),
        phase_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_tampering_fails_authentication\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

// =============================================================================
// Test: Bundle Verification
// =============================================================================

/// Test CLI verify command works correctly.
#[test]
fn test_cli_verify_command() {
    use assert_cmd::cargo::cargo_bin_cmd;

    let tracker = tracker_for("test_cli_verify_command");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_cli_verify_command\",\"status\":\"START\"}}");

    let artifacts = build_full_pipeline(1, true, false);

    // Run cass pages --verify
    let phase_start = tracker.start("cli_verify", Some("Execute cass pages --verify command"));
    let mut cmd = cargo_bin_cmd!("cass");
    let assert = cmd
        .arg("pages")
        .arg("--verify")
        .arg(&artifacts.bundle.site_dir)
        .arg("--json")
        .assert();

    assert.success();
    tracker.end(
        "cli_verify",
        Some("Execute cass pages --verify command"),
        phase_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_cli_verify_command\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

// =============================================================================
// Test: Search in Decrypted Archive
// =============================================================================

/// Test that we can query the decrypted export database.
#[test]
fn test_search_in_decrypted_archive() {
    let tracker = tracker_for("test_search_in_decrypted_archive");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_search_in_decrypted_archive\",\"status\":\"START\"}}");

    let temp_dir = TempDir::new().unwrap();
    let artifacts = build_full_pipeline(5, true, false);

    // Decrypt
    let phase_start = tracker.start("decrypt", Some("Decrypt payload to SQLite database"));
    let config = load_config(&artifacts.bundle.site_dir).expect("load config");
    let decryptor = DecryptionEngine::unlock_with_password(config, TEST_PASSWORD).expect("unlock");
    let decrypted_path = temp_dir.path().join("decrypted.db");
    decryptor
        .decrypt_to_file(&artifacts.bundle.site_dir, &decrypted_path, |_, _| {})
        .expect("decrypt");
    tracker.end(
        "decrypt",
        Some("Decrypt payload to SQLite database"),
        phase_start,
    );

    // Open the export database directly (it has a different schema than the main DB)
    let phase_start = tracker.start(
        "query_database",
        Some("Query decrypted database to verify schema"),
    );
    let conn =
        Connection::open(decrypted_path.to_string_lossy().as_ref()).expect("open decrypted db");

    use frankensqlite::compat::{ConnectionExt, RowExt};

    // Verify conversations table exists and has data
    let conv_count: i64 = conn
        .query_row_map(
            "SELECT COUNT(*) FROM conversations",
            &[],
            |row: &frankensqlite::Row| row.get_typed(0),
        )
        .expect("count conversations");
    assert_eq!(conv_count, 5, "Should have 5 conversations");

    // Verify messages table exists and has data
    let msg_count: i64 = conn
        .query_row_map(
            "SELECT COUNT(*) FROM messages",
            &[],
            |row: &frankensqlite::Row| row.get_typed(0),
        )
        .expect("count messages");
    assert!(msg_count > 0, "Should have messages");

    // Verify export_meta table has schema version
    let schema_version: String = conn
        .query_row_map(
            "SELECT value FROM export_meta WHERE key = 'schema_version'",
            &[],
            |row: &frankensqlite::Row| row.get_typed(0),
        )
        .expect("get schema version");
    assert_eq!(schema_version, "1", "Export schema version should be 1");
    tracker.end(
        "query_database",
        Some("Query decrypted database to verify schema"),
        phase_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_search_in_decrypted_archive\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

// =============================================================================
// Test: Wizard/Export Flows with Real Fixtures (br-1rvb)
// =============================================================================

/// Test summary generation with multi-agent fixtures.
/// Verifies that SummaryGenerator correctly aggregates data from multiple agents.
#[test]
fn test_summary_generation_multi_agent_fixtures() {
    use coding_agent_search::pages::summary::SummaryGenerator;

    let tracker = tracker_for("test_summary_generation_multi_agent_fixtures");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_summary_generation_multi_agent_fixtures\",\"status\":\"START\"}}");

    let temp_dir = TempDir::new().expect("temp dir");
    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    // Phase 1: Setup multi-agent database
    let phase_start = tracker.start(
        "setup_database",
        Some("Create database with multiple agent types"),
    );
    let db_path = data_dir.join("agent_search.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open storage");

    // Create multiple agents
    let agents = [
        ("claude_code", "Claude Code", AgentKind::Cli),
        ("codex", "Codex", AgentKind::Cli),
        ("gemini", "Gemini", AgentKind::Cli),
        ("cline", "Cline", AgentKind::VsCode),
    ];

    for (slug, name, kind) in agents {
        let agent = Agent {
            id: None,
            slug: slug.to_string(),
            name: name.to_string(),
            version: None,
            kind,
        };
        let agent_id = storage.ensure_agent(&agent).expect("ensure agent");

        // Create workspace for each agent
        let workspace_path = PathBuf::from(format!("/home/user/projects/{}", slug));
        let workspace_id = storage
            .ensure_workspace(&workspace_path, None)
            .expect("ensure workspace");

        // Create 3 conversations per agent
        for i in 0..3 {
            let conversation = ConversationFixtureBuilder::new(slug)
                .title(format!("{} Session {}", name, i))
                .workspace(&workspace_path)
                .source_path(format!("/home/user/.{}/session-{}.jsonl", slug, i))
                .messages(5)
                .with_content(0, format!("User message for {} session {}", name, i))
                .with_content(1, format!("Assistant response for {} session {}", name, i))
                .build_conversation();

            storage
                .insert_conversation_tree(agent_id, Some(workspace_id), &conversation)
                .expect("insert conversation");
        }
    }
    tracker.end(
        "setup_database",
        Some("Create database with multiple agent types"),
        phase_start,
    );

    // Phase 2: Generate summary
    let phase_start = tracker.start(
        "generate_summary",
        Some("Generate summary from multi-agent database"),
    );
    let conn = Connection::open(db_path.to_string_lossy().as_ref()).expect("open connection");
    let generator = SummaryGenerator::new(&conn);
    let summary = generator.generate(None).expect("generate summary");
    tracker.end(
        "generate_summary",
        Some("Generate summary from multi-agent database"),
        phase_start,
    );

    // Phase 3: Verify summary contents
    let phase_start = tracker.start(
        "verify_summary",
        Some("Validate summary contains all agents and workspaces"),
    );
    assert_eq!(
        summary.total_conversations, 12,
        "Should have 12 conversations (4 agents * 3 each)"
    );
    assert_eq!(summary.agents.len(), 4, "Should have 4 agents");
    assert_eq!(summary.workspaces.len(), 4, "Should have 4 workspaces");

    // Verify each agent has 25% of conversations
    for agent in &summary.agents {
        assert_eq!(
            agent.conversation_count, 3,
            "Each agent should have 3 conversations"
        );
        assert!(
            (agent.percentage - 25.0).abs() < 0.1,
            "Each agent should be ~25% of total"
        );
    }
    tracker.end(
        "verify_summary",
        Some("Validate summary contains all agents and workspaces"),
        phase_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_summary_generation_multi_agent_fixtures\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

/// Test summary with agent filter applied.
/// Verifies that filtering to specific agents works correctly.
#[test]
fn test_summary_with_agent_filter() {
    use coding_agent_search::pages::summary::{SummaryFilters, SummaryGenerator};

    let tracker = tracker_for("test_summary_with_agent_filter");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_summary_with_agent_filter\",\"status\":\"START\"}}");

    let temp_dir = TempDir::new().expect("temp dir");
    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    // Setup database with 2 agents
    let db_path = data_dir.join("agent_search.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open storage");

    let claude_agent = Agent {
        id: None,
        slug: "claude_code".to_string(),
        name: "Claude Code".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };
    let codex_agent = Agent {
        id: None,
        slug: "codex".to_string(),
        name: "Codex".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };

    let claude_id = storage.ensure_agent(&claude_agent).expect("ensure claude");
    let codex_id = storage.ensure_agent(&codex_agent).expect("ensure codex");

    let workspace_path = Path::new("/home/user/projects/shared");
    let workspace_id = storage
        .ensure_workspace(workspace_path, None)
        .expect("ensure workspace");

    // 5 Claude conversations
    for i in 0..5 {
        let conv = ConversationFixtureBuilder::new("claude_code")
            .title(format!("Claude Session {}", i))
            .workspace(workspace_path)
            .source_path(format!("/home/user/.claude/session-{}.jsonl", i))
            .messages(3)
            .build_conversation();
        storage
            .insert_conversation_tree(claude_id, Some(workspace_id), &conv)
            .expect("insert");
    }

    // 3 Codex conversations
    for i in 0..3 {
        let conv = ConversationFixtureBuilder::new("codex")
            .title(format!("Codex Session {}", i))
            .workspace(workspace_path)
            .source_path(format!("/home/user/.codex/session-{}.jsonl", i))
            .messages(3)
            .build_conversation();
        storage
            .insert_conversation_tree(codex_id, Some(workspace_id), &conv)
            .expect("insert");
    }

    // Test: Filter to Claude only
    let phase_start = tracker.start(
        "filter_claude",
        Some("Generate summary filtered to Claude Code only"),
    );
    let conn = Connection::open(db_path.to_string_lossy().as_ref()).expect("open connection");
    let generator = SummaryGenerator::new(&conn);

    let filters = SummaryFilters {
        agents: Some(vec!["claude_code".to_string()]),
        ..Default::default()
    };
    let summary = generator.generate(Some(&filters)).expect("generate");

    assert_eq!(
        summary.total_conversations, 5,
        "Should have only 5 Claude conversations"
    );
    assert_eq!(summary.agents.len(), 1, "Should have only 1 agent");
    assert_eq!(summary.agents[0].name, "claude_code");
    tracker.end(
        "filter_claude",
        Some("Generate summary filtered to Claude Code only"),
        phase_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_summary_with_agent_filter\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

/// Test summary with workspace exclusions.
/// Verifies that ExclusionSet correctly filters out workspaces.
#[test]
fn test_summary_with_workspace_exclusions() {
    use coding_agent_search::pages::summary::{ExclusionSet, SummaryGenerator};

    let tracker = tracker_for("test_summary_with_workspace_exclusions");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_summary_with_workspace_exclusions\",\"status\":\"START\"}}");

    let temp_dir = TempDir::new().expect("temp dir");
    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    // Setup database with 2 workspaces
    let phase_start = tracker.start("setup_database", Some("Create database with 2 workspaces"));
    let db_path = data_dir.join("agent_search.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open storage");

    let agent = Agent {
        id: None,
        slug: "claude_code".to_string(),
        name: "Claude Code".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };
    let agent_id = storage.ensure_agent(&agent).expect("ensure agent");

    let public_ws_path = Path::new("/home/user/projects/public-app");
    let private_ws_path = Path::new("/home/user/projects/private-secrets");

    let public_ws_id = storage
        .ensure_workspace(public_ws_path, None)
        .expect("ensure public ws");
    let private_ws_id = storage
        .ensure_workspace(private_ws_path, None)
        .expect("ensure private ws");

    // 3 public conversations
    for i in 0..3 {
        let conv = ConversationFixtureBuilder::new("claude_code")
            .title(format!("Public App Session {}", i))
            .workspace(public_ws_path)
            .source_path(format!("/home/user/.claude/public-{}.jsonl", i))
            .messages(4)
            .build_conversation();
        storage
            .insert_conversation_tree(agent_id, Some(public_ws_id), &conv)
            .expect("insert");
    }

    // 2 private conversations (should be excluded)
    for i in 0..2 {
        let conv = ConversationFixtureBuilder::new("claude_code")
            .title(format!("Private Secrets Session {}", i))
            .workspace(private_ws_path)
            .source_path(format!("/home/user/.claude/private-{}.jsonl", i))
            .messages(4)
            .build_conversation();
        storage
            .insert_conversation_tree(agent_id, Some(private_ws_id), &conv)
            .expect("insert");
    }
    tracker.end(
        "setup_database",
        Some("Create database with 2 workspaces"),
        phase_start,
    );

    // Test: Exclude private workspace
    let phase_start = tracker.start(
        "generate_with_exclusions",
        Some("Generate summary with private workspace excluded"),
    );
    let conn = Connection::open(db_path.to_string_lossy().as_ref()).expect("open connection");
    let generator = SummaryGenerator::new(&conn);

    let mut exclusions = ExclusionSet::new();
    exclusions.exclude_workspace("/home/user/projects/private-secrets");

    let summary = generator
        .generate_with_exclusions(None, &exclusions)
        .expect("generate with exclusions");

    // Total should show all 5, but private-secrets marked as not included
    let private_ws = summary
        .workspaces
        .iter()
        .find(|w| w.path.contains("private-secrets"));
    assert!(private_ws.is_some(), "Private workspace should appear");
    assert!(
        !private_ws.unwrap().included,
        "Private workspace should be marked excluded"
    );

    let public_ws = summary
        .workspaces
        .iter()
        .find(|w| w.path.contains("public-app"));
    assert!(public_ws.is_some(), "Public workspace should appear");
    assert!(
        public_ws.unwrap().included,
        "Public workspace should be included"
    );

    // Conversation count should reflect exclusions
    assert_eq!(
        summary.total_conversations, 3,
        "Should only count 3 public conversations"
    );
    tracker.end(
        "generate_with_exclusions",
        Some("Generate summary with private workspace excluded"),
        phase_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_summary_with_workspace_exclusions\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

/// Test export filter with date range boundaries.
/// Verifies that time-based filtering works correctly.
#[test]
fn test_export_filter_date_range() {
    use coding_agent_search::pages::summary::{SummaryFilters, SummaryGenerator};

    let tracker = tracker_for("test_export_filter_date_range");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_export_filter_date_range\",\"status\":\"START\"}}");

    let temp_dir = TempDir::new().expect("temp dir");
    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    // Setup database with conversations across different time ranges
    let phase_start = tracker.start(
        "setup_database",
        Some("Create database with conversations across time range"),
    );
    let db_path = data_dir.join("agent_search.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open storage");

    let agent = Agent {
        id: None,
        slug: "claude_code".to_string(),
        name: "Claude Code".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };
    let agent_id = storage.ensure_agent(&agent).expect("ensure agent");

    let workspace_path = Path::new("/home/user/projects/test");
    let workspace_id = storage
        .ensure_workspace(workspace_path, None)
        .expect("ensure workspace");

    // January 2024 conversations (2 total)
    let jan_base_ts = 1704067200000i64; // 2024-01-01
    for i in 0..2 {
        let conv = ConversationFixtureBuilder::new("claude_code")
            .title(format!("January Session {}", i))
            .workspace(workspace_path)
            .source_path(format!("/home/user/.claude/jan-{}.jsonl", i))
            .base_ts(jan_base_ts + (i as i64 * 86400000))
            .messages(3)
            .build_conversation();
        storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &conv)
            .expect("insert");
    }

    // March 2024 conversations (3 total)
    let mar_base_ts = 1709251200000i64; // 2024-03-01
    for i in 0..3 {
        let conv = ConversationFixtureBuilder::new("claude_code")
            .title(format!("March Session {}", i))
            .workspace(workspace_path)
            .source_path(format!("/home/user/.claude/mar-{}.jsonl", i))
            .base_ts(mar_base_ts + (i as i64 * 86400000))
            .messages(3)
            .build_conversation();
        storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &conv)
            .expect("insert");
    }

    // May 2024 conversations (4 total)
    let may_base_ts = 1714521600000i64; // 2024-05-01
    for i in 0..4 {
        let conv = ConversationFixtureBuilder::new("claude_code")
            .title(format!("May Session {}", i))
            .workspace(workspace_path)
            .source_path(format!("/home/user/.claude/may-{}.jsonl", i))
            .base_ts(may_base_ts + (i as i64 * 86400000))
            .messages(3)
            .build_conversation();
        storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &conv)
            .expect("insert");
    }
    tracker.end(
        "setup_database",
        Some("Create database with conversations across time range"),
        phase_start,
    );

    // Test: Filter to February-April range (should get only March conversations)
    let phase_start = tracker.start(
        "filter_date_range",
        Some("Filter to February-April date range"),
    );
    let conn = Connection::open(db_path.to_string_lossy().as_ref()).expect("open connection");
    let generator = SummaryGenerator::new(&conn);

    let feb_start = 1706745600000i64; // 2024-02-01
    let apr_end = 1714435200000i64; // 2024-04-30

    let filters = SummaryFilters {
        since_ts: Some(feb_start),
        until_ts: Some(apr_end),
        ..Default::default()
    };
    let summary = generator.generate(Some(&filters)).expect("generate");

    assert_eq!(
        summary.total_conversations, 3,
        "Should have only 3 March conversations in Feb-Apr range"
    );
    tracker.end(
        "filter_date_range",
        Some("Filter to February-April date range"),
        phase_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_export_filter_date_range\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

/// Test exclusion patterns match conversation titles.
/// Verifies that regex-based exclusion works correctly.
#[test]
fn test_exclusion_pattern_matching() {
    use coding_agent_search::pages::summary::ExclusionSet;

    let tracker = tracker_for("test_exclusion_pattern_matching");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_exclusion_pattern_matching\",\"status\":\"START\"}}");

    let temp_dir = TempDir::new().expect("temp dir");
    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    // Setup database with conversations having various titles
    let phase_start = tracker.start(
        "setup_database",
        Some("Create database with varied conversation titles"),
    );
    let db_path = data_dir.join("agent_search.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open storage");

    let agent = Agent {
        id: None,
        slug: "claude_code".to_string(),
        name: "Claude Code".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };
    let agent_id = storage.ensure_agent(&agent).expect("ensure agent");

    let workspace_path = Path::new("/home/user/projects/test");
    let workspace_id = storage
        .ensure_workspace(workspace_path, None)
        .expect("ensure workspace");

    let titles = [
        "Fix authentication bug",
        "SECRET: API key rotation",
        "Add user profile page",
        "PRIVATE: Personal notes",
        "Implement search feature",
        "SECRET: Database credentials",
    ];

    for (i, title) in titles.iter().enumerate() {
        let conv = ConversationFixtureBuilder::new("claude_code")
            .title(*title)
            .workspace(workspace_path)
            .source_path(format!("/home/user/.claude/session-{}.jsonl", i))
            .messages(3)
            .build_conversation();
        storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &conv)
            .expect("insert");
    }
    tracker.end(
        "setup_database",
        Some("Create database with varied conversation titles"),
        phase_start,
    );

    // Test: Pattern exclusion
    let phase_start = tracker.start("pattern_exclusion", Some("Test regex pattern exclusions"));

    let mut exclusions = ExclusionSet::new();
    // Exclude titles starting with "SECRET:" or "PRIVATE:"
    exclusions
        .add_pattern("^SECRET:")
        .expect("add SECRET pattern");
    exclusions
        .add_pattern("^PRIVATE:")
        .expect("add PRIVATE pattern");

    // Verify pattern matching works
    assert!(exclusions.matches_pattern("SECRET: API key rotation"));
    assert!(exclusions.matches_pattern("PRIVATE: Personal notes"));
    assert!(!exclusions.matches_pattern("Fix authentication bug"));
    assert!(!exclusions.matches_pattern("Implement search feature"));

    // Verify should_exclude integrates patterns
    assert!(exclusions.should_exclude(None, 1, "SECRET: Something"));
    assert!(exclusions.should_exclude(None, 1, "PRIVATE: Something"));
    assert!(!exclusions.should_exclude(None, 1, "Normal title"));
    tracker.end(
        "pattern_exclusion",
        Some("Test regex pattern exclusions"),
        phase_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_exclusion_pattern_matching\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

/// Test that prepublish summary rendering includes all expected sections.
/// Verifies the human-readable output format.
#[test]
fn test_prepublish_summary_render() {
    use coding_agent_search::pages::summary::SummaryGenerator;

    let tracker = tracker_for("test_prepublish_summary_render");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_prepublish_summary_render\",\"status\":\"START\"}}");

    let temp_dir = TempDir::new().expect("temp dir");
    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");

    // Setup database
    let phase_start = tracker.start("setup_database", Some("Create test database"));
    let db_path = data_dir.join("agent_search.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open storage");

    let agent = Agent {
        id: None,
        slug: "claude_code".to_string(),
        name: "Claude Code".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };
    let agent_id = storage.ensure_agent(&agent).expect("ensure agent");

    let workspace_path = Path::new("/home/user/projects/webapp");
    let workspace_id = storage
        .ensure_workspace(workspace_path, None)
        .expect("ensure workspace");

    for i in 0..5 {
        let conv = ConversationFixtureBuilder::new("claude_code")
            .title(format!("Web App Session {}", i))
            .workspace(workspace_path)
            .source_path(format!("/home/user/.claude/webapp-{}.jsonl", i))
            .messages(10)
            .with_content(
                0,
                format!("Working on feature {} for the web application", i),
            )
            .build_conversation();
        storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &conv)
            .expect("insert");
    }
    tracker.end("setup_database", Some("Create test database"), phase_start);

    // Generate summary and verify render output
    let phase_start = tracker.start(
        "verify_render",
        Some("Verify summary render contains all sections"),
    );
    let conn = Connection::open(db_path.to_string_lossy().as_ref()).expect("open connection");
    let generator = SummaryGenerator::new(&conn);
    let summary = generator.generate(None).expect("generate");
    let rendered = summary.render_overview();

    // Verify all expected sections are present
    assert!(
        rendered.contains("CONTENT OVERVIEW"),
        "Should have content overview section"
    );
    assert!(
        rendered.contains("Conversations: 5"),
        "Should show conversation count"
    );
    assert!(rendered.contains("Messages:"), "Should show message count");
    assert!(
        rendered.contains("Characters:"),
        "Should show character count"
    );
    assert!(
        rendered.contains("Archive Size:"),
        "Should show estimated size"
    );
    assert!(
        rendered.contains("DATE RANGE"),
        "Should have date range section"
    );
    assert!(
        rendered.contains("WORKSPACES"),
        "Should have workspaces section"
    );
    assert!(rendered.contains("webapp"), "Should show workspace name");
    assert!(rendered.contains("AGENTS"), "Should have agents section");
    assert!(rendered.contains("claude_code"), "Should show agent name");
    assert!(
        rendered.contains("SECURITY"),
        "Should have security section"
    );
    assert!(
        rendered.contains("AES-256-GCM"),
        "Should show encryption algorithm"
    );
    tracker.end(
        "verify_render",
        Some("Verify summary render contains all sections"),
        phase_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_prepublish_summary_render\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

/// Test size estimation accuracy.
/// Verifies that estimate_compressed_size produces reasonable values.
#[test]
fn test_size_estimation_accuracy() {
    use coding_agent_search::pages::summary::{estimate_compressed_size, format_size};

    let tracker = tracker_for("test_size_estimation_accuracy");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_size_estimation_accuracy\",\"status\":\"START\"}}");

    // Test various size ranges
    let phase_start = tracker.start(
        "size_estimation",
        Some("Test size estimation for various inputs"),
    );

    // Small content (~1KB)
    let small_estimate = estimate_compressed_size(1000);
    assert!(small_estimate > 400, "Small estimate should be > 400 bytes");
    assert!(small_estimate < 450, "Small estimate should be < 450 bytes");

    // Medium content (~100KB)
    let medium_estimate = estimate_compressed_size(100_000);
    assert!(medium_estimate > 40_000, "Medium estimate should be > 40KB");
    assert!(medium_estimate < 45_000, "Medium estimate should be < 45KB");

    // Large content (~10MB)
    let large_estimate = estimate_compressed_size(10_000_000);
    assert!(large_estimate > 4_000_000, "Large estimate should be > 4MB");
    assert!(
        large_estimate < 4_500_000,
        "Large estimate should be < 4.5MB"
    );

    tracker.end(
        "size_estimation",
        Some("Test size estimation for various inputs"),
        phase_start,
    );

    // Test format_size output
    let phase_start = tracker.start("format_size", Some("Test human-readable size formatting"));

    assert_eq!(format_size(512), "512 bytes");
    assert!(format_size(1536).contains("KB"));
    assert!(format_size(1_500_000).contains("MB"));
    assert!(format_size(2_000_000_000).contains("GB"));

    tracker.end(
        "format_size",
        Some("Test human-readable size formatting"),
        phase_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_size_estimation_accuracy\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}
