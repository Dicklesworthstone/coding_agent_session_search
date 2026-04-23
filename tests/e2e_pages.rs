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

use assert_cmd::cargo::cargo_bin_cmd;
use coding_agent_search::model::types::{Agent, AgentKind};
use coding_agent_search::pages::bundle::{BundleBuilder, BundleResult};
use coding_agent_search::pages::encrypt::{DecryptionEngine, EncryptionEngine, load_config};
use coding_agent_search::pages::export::{ExportEngine, ExportFilter, PathMode};
use coding_agent_search::pages::verify::verify_bundle;
use coding_agent_search::storage::sqlite::SqliteStorage;
use frankensqlite::Connection;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
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

static PAGES_WIZARD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn pages_wizard_guard() -> std::sync::MutexGuard<'static, ()> {
    match PAGES_WIZARD_LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn cass_bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_cass")
}

fn spawn_capture_thread<R: Read + Send + 'static>(
    mut reader: R,
) -> (Arc<Mutex<Vec<u8>>>, thread::JoinHandle<()>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);
    let handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => captured_clone
                    .lock()
                    .expect("capture lock")
                    .extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
    });
    (captured, handle)
}

fn wait_for_output_growth(
    captured: &Arc<Mutex<Vec<u8>>>,
    base_len: usize,
    min_delta: usize,
    timeout: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        {
            let data = captured.lock().expect("capture lock");
            if data.len() >= base_len.saturating_add(min_delta) {
                return true;
            }
        }
        if start.elapsed() >= timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn wait_for_output_contains(
    captured: &Arc<Mutex<Vec<u8>>>,
    needle: &str,
    timeout: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        {
            let data = captured.lock().expect("capture lock");
            let text = String::from_utf8_lossy(&data);
            if text.contains(needle) {
                return true;
            }
        }
        if start.elapsed() >= timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn wait_for_output_stable(captured: &Arc<Mutex<Vec<u8>>>, stable_for: Duration) {
    let poll = Duration::from_millis(40);
    let mut last_len = captured.lock().expect("capture lock").len();
    let mut stable_elapsed = Duration::ZERO;

    while stable_elapsed < stable_for {
        thread::sleep(poll);
        let next_len = captured.lock().expect("capture lock").len();
        if next_len == last_len {
            stable_elapsed += poll;
        } else {
            last_len = next_len;
            stable_elapsed = Duration::ZERO;
        }
    }
}

fn send_key_sequence(writer: &mut (dyn Write + Send), bytes: &[u8]) {
    writer.write_all(bytes).expect("write to PTY");
    writer.flush().expect("flush PTY");
}

fn send_line_and_wait(
    writer: &mut (dyn Write + Send),
    captured: &Arc<Mutex<Vec<u8>>>,
    line: &str,
    label: &str,
) {
    let before = captured.lock().expect("capture lock").len();
    let mut input = line.as_bytes().to_vec();
    input.push(b'\r');
    send_key_sequence(writer, &input);
    assert!(
        wait_for_output_growth(captured, before, 1, Duration::from_secs(3)),
        "did not observe PTY output growth after {label}"
    );
}

fn send_confirm_yes_and_wait(
    writer: &mut (dyn Write + Send),
    captured: &Arc<Mutex<Vec<u8>>>,
    label: &str,
) {
    let before_yes = captured.lock().expect("capture lock").len();
    send_key_sequence(writer, b"y");
    assert!(
        wait_for_output_growth(captured, before_yes, 1, Duration::from_secs(3)),
        "did not observe PTY output growth after {label} (yes key)"
    );
}

fn wait_for_prompt(captured: &Arc<Mutex<Vec<u8>>>, prompt: &str) {
    if wait_for_output_contains(captured, prompt, Duration::from_secs(8)) {
        wait_for_output_stable(captured, Duration::from_millis(120));
        return;
    }

    let captured_bytes = captured.lock().expect("capture lock").clone();
    let captured_output = String::from_utf8_lossy(&captured_bytes);
    let tail: String = captured_output
        .chars()
        .rev()
        .take(4000)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    panic!("did not observe PTY prompt containing {prompt:?}\nPTY tail:\n{tail}");
}

fn pick_unused_local_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn http_request(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect preview server");
    stream
        .write_all(request.as_bytes())
        .expect("write preview request");
    stream.flush().expect("flush preview request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read preview response");
    String::from_utf8_lossy(&response).into_owned()
}

fn write_pages_config(path: &Path, output_dir: &Path) {
    let config = serde_json::json!({
        "filters": {
            "path_mode": "relative"
        },
        "encryption": {
            "password": TEST_PASSWORD,
            "generate_recovery": true,
            "generate_qr": false,
            "chunk_size": CHUNK_SIZE
        },
        "bundle": {
            "title": "CLI E2E Archive",
            "description": "Full CLI pages export workflow",
            "hide_metadata": false
        },
        "deployment": {
            "target": "local",
            "output_dir": output_dir,
            "repo": null,
            "branch": null
        }
    });
    fs::write(
        path,
        serde_json::to_string_pretty(&config).expect("serialize pages config"),
    )
    .expect("write pages config");
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Setup a test database with conversations.
fn setup_test_db(data_dir: &Path, conversation_count: usize) -> std::path::PathBuf {
    let db_path = data_dir.join("agent_search.db");

    let storage = SqliteStorage::open(&db_path).expect("Failed to open storage");

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
    let mut enc_engine = EncryptionEngine::new(CHUNK_SIZE).expect("valid chunk size");

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

/// Test the full user-facing CLI pages flow:
/// config-driven export -> CLI verify -> CLI preview -> decrypt roundtrip.
#[test]
fn test_cli_pages_full_workflow_end_to_end() {
    let tracker = tracker_for("test_cli_pages_full_workflow_end_to_end");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!("{{\"test\":\"test_cli_pages_full_workflow_end_to_end\",\"status\":\"START\"}}");

    let temp_dir = TempDir::new().expect("temp dir");
    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).expect("create data dir");
    let db_path = setup_test_db(&data_dir, 4);
    let bundle_dir = temp_dir.path().join("cli-bundle");
    let config_path = temp_dir.path().join("pages-config.json");
    write_pages_config(&config_path, &bundle_dir);

    let phase_start = tracker.start(
        "cli_export",
        Some("Run cass pages --config end-to-end export"),
    );
    let export_output = Command::new(cass_bin_path())
        .arg("--db")
        .arg(&db_path)
        .arg("pages")
        .arg("--config")
        .arg(&config_path)
        .arg("--json")
        .output()
        .expect("spawn cass pages --config");
    tracker.end(
        "cli_export",
        Some("Run cass pages --config end-to-end export"),
        phase_start,
    );
    assert!(
        export_output.status.success(),
        "cass pages --config failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&export_output.stdout),
        String::from_utf8_lossy(&export_output.stderr)
    );

    let export_json: Value =
        serde_json::from_slice(&export_output.stdout).expect("parse pages export JSON");
    assert_eq!(export_json["status"].as_str(), Some("success"));
    assert_eq!(export_json["stats"]["conversations"].as_u64(), Some(4));
    assert_eq!(export_json["encryption"]["enabled"].as_bool(), Some(true));

    let site_dir = PathBuf::from(
        export_json["site_dir"]
            .as_str()
            .expect("site_dir path in export JSON"),
    );
    let private_dir = PathBuf::from(
        export_json["private_dir"]
            .as_str()
            .expect("private_dir path in export JSON"),
    );

    assert!(bundle_dir.is_dir(), "bundle root should exist");
    assert!(
        site_dir.join("index.html").exists(),
        "site bundle should exist"
    );
    assert!(
        private_dir.join("recovery-secret.txt").exists(),
        "private recovery secret should exist"
    );
    assert!(
        !bundle_dir.join("payload").exists(),
        "final bundle root must not leak staging payload directories"
    );

    let phase_start = tracker.start("cli_verify", Some("Run cass pages --verify on bundle"));
    let verify_output = Command::new(cass_bin_path())
        .arg("pages")
        .arg("--verify")
        .arg(&site_dir)
        .arg("--json")
        .output()
        .expect("spawn cass pages --verify");
    tracker.end(
        "cli_verify",
        Some("Run cass pages --verify on bundle"),
        phase_start,
    );
    assert!(
        verify_output.status.success(),
        "cass pages --verify failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&verify_output.stdout),
        String::from_utf8_lossy(&verify_output.stderr)
    );
    let verify_json: Value =
        serde_json::from_slice(&verify_output.stdout).expect("parse verify JSON");
    assert_eq!(verify_json["status"].as_str(), Some("valid"));

    let preview_port = pick_unused_local_port();
    let phase_start = tracker.start("cli_preview", Some("Run preview server and fetch bundle"));
    let mut preview_child = Command::new(cass_bin_path())
        .arg("pages")
        .arg("--preview")
        .arg(&site_dir)
        .arg("--port")
        .arg(preview_port.to_string())
        .arg("--no-open")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn preview server");
    let preview_stderr = preview_child.stderr.take().expect("preview stderr");
    let (captured_stderr, stderr_handle) = spawn_capture_thread(preview_stderr);

    let preview_ready = wait_for_port(preview_port, Duration::from_secs(8));
    let preview_stderr_bytes = captured_stderr.lock().expect("preview stderr lock").clone();
    assert!(
        preview_ready,
        "preview server never became ready\nstderr:\n{}",
        String::from_utf8_lossy(&preview_stderr_bytes)
    );

    let index_response = http_request(
        preview_port,
        "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        index_response.starts_with("HTTP/1.1 200"),
        "expected 200 from preview index, got:\n{}",
        index_response
    );
    assert!(
        index_response.contains("Cross-Origin-Opener-Policy: same-origin"),
        "preview response should include COOP header"
    );
    assert!(
        index_response.contains("Cross-Origin-Embedder-Policy: require-corp"),
        "preview response should include COEP header"
    );

    let config_response = http_request(
        preview_port,
        "HEAD /config.json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        config_response.starts_with("HTTP/1.1 200"),
        "expected 200 from preview config HEAD, got:\n{}",
        config_response
    );

    let _ = preview_child.kill();
    let preview_status = preview_child.wait().expect("wait preview child");
    let _ = stderr_handle.join();
    tracker.end(
        "cli_preview",
        Some("Run preview server and fetch bundle"),
        phase_start,
    );
    assert!(
        !preview_status.success() || preview_status.code().is_none(),
        "preview server should terminate due to explicit test shutdown"
    );

    let phase_start = tracker.start(
        "decrypt_roundtrip",
        Some("Decrypt CLI-generated bundle and compare contents"),
    );
    let config = load_config(&site_dir).expect("load generated config");
    let decryptor = DecryptionEngine::unlock_with_password(config, TEST_PASSWORD)
        .expect("unlock CLI-generated bundle");
    let decrypted_path = temp_dir.path().join("cli-decrypted.db");
    decryptor
        .decrypt_to_file(&site_dir, &decrypted_path, |_, _| {})
        .expect("decrypt CLI-generated bundle");
    let conn =
        Connection::open(decrypted_path.to_string_lossy().as_ref()).expect("open decrypted db");
    use frankensqlite::compat::{ConnectionExt, RowExt};
    let conversation_count: i64 = conn
        .query_row_map(
            "SELECT COUNT(*) FROM conversations",
            &[],
            |row: &frankensqlite::Row| row.get_typed(0),
        )
        .expect("count decrypted conversations");
    let message_count: i64 = conn
        .query_row_map(
            "SELECT COUNT(*) FROM messages",
            &[],
            |row: &frankensqlite::Row| row.get_typed(0),
        )
        .expect("count decrypted messages");
    assert_eq!(
        conversation_count, 4,
        "CLI-generated bundle should export all conversations"
    );
    assert!(
        message_count > 0,
        "CLI-generated bundle should export messages"
    );
    tracker.end(
        "decrypt_roundtrip",
        Some("Decrypt CLI-generated bundle and compare contents"),
        phase_start,
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_cli_pages_full_workflow_end_to_end\",\"duration_ms\":{},\"status\":\"PASS\"}}",
        test_start.elapsed().as_millis()
    );
}

/// Test the real interactive wizard in a PTY.
/// Verifies that `--db` is honored and that the wizard writes the final bundle
/// to the requested output directory instead of leaving users in a staging dir.
#[test]
fn test_pages_wizard_pty_respects_db_override_and_writes_bundle_root() {
    let _wizard_guard = pages_wizard_guard();
    let tracker = tracker_for("test_pages_wizard_pty_respects_db_override_and_writes_bundle_root");
    let _trace_guard = tracker.trace_env_guard();
    let test_start = Instant::now();
    eprintln!(
        "{{\"test\":\"test_pages_wizard_pty_respects_db_override_and_writes_bundle_root\",\"status\":\"START\"}}"
    );

    let temp_dir = TempDir::new().expect("temp dir");
    let home_dir = temp_dir.path().join("home");
    let xdg_dir = temp_dir.path().join("xdg");
    let cass_data_dir = temp_dir.path().join("cass-data");
    fs::create_dir_all(&home_dir).expect("create home dir");
    fs::create_dir_all(&xdg_dir).expect("create xdg dir");
    fs::create_dir_all(&cass_data_dir).expect("create cass data dir");

    let db_dir = temp_dir.path().join("db");
    fs::create_dir_all(&db_dir).expect("create db dir");
    let db_path = setup_test_db(&db_dir, 2);
    let wizard_output = temp_dir.path().join("wizard-output");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 48,
            cols: 140,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open PTY");
    let reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let (captured, reader_handle) = spawn_capture_thread(reader);
    let mut writer = pair.master.take_writer().expect("take PTY writer");

    let phase_start = tracker.start("wizard_launch", Some("Launch interactive pages wizard"));
    let mut cmd = CommandBuilder::new(cass_bin_path());
    cmd.arg("--db");
    cmd.arg(db_path.to_string_lossy().as_ref());
    cmd.arg("pages");
    cmd.cwd(home_dir.to_string_lossy().as_ref());
    cmd.env("HOME", home_dir.to_string_lossy().as_ref());
    cmd.env("XDG_DATA_HOME", xdg_dir.to_string_lossy().as_ref());
    cmd.env("CASS_DATA_DIR", cass_data_dir.to_string_lossy().as_ref());
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_LOG", "error");
    cmd.env("TERM", "xterm-256color");
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("spawn pages wizard in PTY");

    assert!(
        wait_for_output_growth(&captured, 0, 64, Duration::from_secs(8)),
        "did not observe pages wizard startup output"
    );

    let strong_password = "S3cure-Horse-Battery-Staple-2026!";
    wait_for_prompt(&captured, "Which agents would you like to include?");
    send_line_and_wait(&mut *writer, &captured, "", "accepting all agents");
    wait_for_prompt(&captured, "Include all workspaces?");
    send_line_and_wait(&mut *writer, &captured, "", "including all workspaces");
    wait_for_prompt(&captured, "Time range");
    send_line_and_wait(&mut *writer, &captured, "", "keeping all-time range");
    wait_for_prompt(&captured, "Archive password (min 8 characters)");
    send_line_and_wait(
        &mut *writer,
        &captured,
        strong_password,
        "entering password",
    );
    wait_for_prompt(&captured, "Confirm password");
    send_line_and_wait(
        &mut *writer,
        &captured,
        strong_password,
        "confirming password",
    );
    wait_for_prompt(&captured, "Generate recovery secret? (recommended)");
    send_line_and_wait(
        &mut *writer,
        &captured,
        "",
        "accepting recovery key generation",
    );
    wait_for_prompt(
        &captured,
        "Generate QR code for recovery? (for mobile access)",
    );
    send_line_and_wait(&mut *writer, &captured, "", "skipping QR generation");
    wait_for_prompt(&captured, "Archive title");
    send_line_and_wait(&mut *writer, &captured, "", "accepting default title");
    wait_for_prompt(&captured, "Description (shown on unlock page)");
    send_line_and_wait(&mut *writer, &captured, "", "accepting default description");
    wait_for_prompt(
        &captured,
        "Hide workspace paths and file names? (for privacy)",
    );
    send_line_and_wait(&mut *writer, &captured, "", "keeping metadata visible");
    wait_for_prompt(&captured, "Where would you like to deploy?");
    send_line_and_wait(
        &mut *writer,
        &captured,
        "",
        "keeping local deployment target",
    );
    wait_for_prompt(&captured, "Output directory");
    send_line_and_wait(
        &mut *writer,
        &captured,
        wizard_output.to_string_lossy().as_ref(),
        "setting bundle output directory",
    );
    wait_for_prompt(&captured, "What would you like to do?");
    send_line_and_wait(
        &mut *writer,
        &captured,
        "",
        "proceeding from pre-publish summary",
    );
    wait_for_prompt(&captured, "Have you reviewed the content summary?");
    send_confirm_yes_and_wait(&mut *writer, &captured, "confirming content review");
    wait_for_prompt(
        &captured,
        "I understand that I must save the recovery key securely",
    );
    send_confirm_yes_and_wait(&mut *writer, &captured, "confirming recovery key backup");
    wait_for_prompt(&captured, "[First confirmation - press Enter]");
    send_line_and_wait(&mut *writer, &captured, "", "first final confirmation");
    wait_for_prompt(&captured, "[Second confirmation - press Enter to proceed]");
    send_line_and_wait(&mut *writer, &captured, "", "second final confirmation");

    let wait_start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if wait_start.elapsed() >= Duration::from_secs(45) {
                    let captured_bytes = captured.lock().expect("capture lock").clone();
                    let captured_output = String::from_utf8_lossy(&captured_bytes);
                    let _ = child.kill();
                    let status = child.wait().expect("wait after PTY kill");
                    panic!(
                        "pages wizard timed out after 45s (status: {status})\nPTY output:\n{captured_output}"
                    );
                }
                thread::sleep(Duration::from_millis(40));
            }
            Err(err) => panic!("Failed polling pages wizard PTY child: {err}"),
        }
    };
    tracker.end(
        "wizard_launch",
        Some("Launch interactive pages wizard"),
        phase_start,
    );
    assert!(
        status.success(),
        "pages wizard exited unsuccessfully: {status}"
    );

    drop(writer);
    drop(pair);
    let _ = reader_handle.join();
    let captured_bytes = captured.lock().expect("capture lock").clone();
    let captured_output = String::from_utf8_lossy(&captured_bytes).into_owned();
    let site_dir = wizard_output.join("site");

    assert!(
        site_dir.join("index.html").exists(),
        "wizard should place deployable site under the requested output root"
    );
    assert!(
        wizard_output
            .join("private")
            .join("recovery-secret.txt")
            .exists(),
        "wizard should place private recovery artifacts under the requested output root"
    );
    assert!(
        !wizard_output.join("payload").exists(),
        "wizard output root must not leak intermediate payload staging"
    );
    assert!(
        captured_output.contains("Deployable site directory"),
        "wizard should report the real deployable site directory\noutput:\n{}",
        captured_output
    );
    assert!(
        captured_output.contains("Conversations: 2"),
        "wizard summary should reflect the overridden --db contents\noutput:\n{}",
        captured_output
    );
    assert!(
        captured_output.contains("cass pages --preview"),
        "wizard should suggest the built-in preview command\noutput:\n{}",
        captured_output
    );

    let config = load_config(&site_dir).expect("load wizard-generated config");
    let decryptor = DecryptionEngine::unlock_with_password(config, strong_password)
        .expect("unlock wizard-generated bundle");
    let decrypted_path = temp_dir.path().join("wizard-decrypted.db");
    decryptor
        .decrypt_to_file(&site_dir, &decrypted_path, |_, _| {})
        .expect("decrypt wizard-generated bundle");
    let conn = Connection::open(decrypted_path.to_string_lossy().as_ref())
        .expect("open wizard decrypted db");
    use frankensqlite::compat::{ConnectionExt, RowExt};
    let conversation_count: i64 = conn
        .query_row_map(
            "SELECT COUNT(*) FROM conversations",
            &[],
            |row: &frankensqlite::Row| row.get_typed(0),
        )
        .expect("count wizard decrypted conversations");
    let message_count: i64 = conn
        .query_row_map(
            "SELECT COUNT(*) FROM messages",
            &[],
            |row: &frankensqlite::Row| row.get_typed(0),
        )
        .expect("count wizard decrypted messages");
    assert_eq!(
        conversation_count, 2,
        "wizard bundle should contain the conversations from the overridden --db"
    );
    assert!(
        message_count > 0,
        "wizard bundle should contain messages from the overridden --db"
    );

    tracker.flush();
    eprintln!(
        "{{\"test\":\"test_pages_wizard_pty_respects_db_override_and_writes_bundle_root\",\"duration_ms\":{},\"status\":\"PASS\"}}",
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
    let storage = SqliteStorage::open(&db_path).expect("open storage");

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
    let storage = SqliteStorage::open(&db_path).expect("open storage");

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
    let storage = SqliteStorage::open(&db_path).expect("open storage");

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
    let storage = SqliteStorage::open(&db_path).expect("open storage");

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
    let storage = SqliteStorage::open(&db_path).expect("open storage");

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
    assert!(exclusions.is_excluded("SECRET: API key rotation"));
    assert!(exclusions.is_excluded("PRIVATE: Personal notes"));
    assert!(!exclusions.is_excluded("Fix authentication bug"));
    assert!(!exclusions.is_excluded("Implement search feature"));

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
    let storage = SqliteStorage::open(&db_path).expect("open storage");

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
