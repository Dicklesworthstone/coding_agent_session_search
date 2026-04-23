use assert_cmd::cargo::cargo_bin_cmd;
use coding_agent_search::model::types::{Agent, AgentKind};
use coding_agent_search::pages::bundle::{BundleBuilder, BundleResult};
use coding_agent_search::pages::encrypt::{DecryptionEngine, EncryptionEngine, load_config};
use coding_agent_search::pages::export::{ExportEngine, ExportFilter, PathMode};
use coding_agent_search::pages::key_management::{key_add_password, key_list, key_revoke};
use coding_agent_search::pages::verify::verify_bundle;
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

#[path = "util/mod.rs"]
mod util;

use util::ConversationFixtureBuilder;

const TEST_PASSWORD: &str = "test-password";
const TEST_PASSWORD_2: &str = "second-password";
const TEST_RECOVERY_SECRET: &[u8] = b"recovery-secret-bytes";

struct PipelineArtifacts {
    export_db_path: std::path::PathBuf,
    bundle: BundleResult,
}

fn build_pipeline(temp_dir: &TempDir) -> PipelineArtifacts {
    let data_dir = temp_dir.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();

    // 1. Setup: Create fixtures and populate DB
    setup_db(&data_dir);

    // 2. Export (simulating `cass pages --export-only`)
    let export_staging = temp_dir.path().join("export_staging");
    fs::create_dir_all(&export_staging).unwrap();
    let export_db_path = export_staging.join("export.db");
    let source_db_path = data_dir.join("agent_search.db");

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
        .expect("ExportEngine execution failed");
    assert_eq!(
        stats.conversations_processed, 1,
        "Should export 1 conversation"
    );
    assert!(export_db_path.exists(), "Export database should exist");

    // 3. Encrypt (simulating Wizard/Encrypt Step)
    let encrypt_dir = temp_dir.path().join("encrypt_staging");
    let mut enc_engine = EncryptionEngine::new(1024 * 1024).expect("valid chunk size"); // 1MB chunks
    enc_engine
        .add_password_slot(TEST_PASSWORD)
        .expect("Failed to add password slot");
    enc_engine
        .add_recovery_slot(TEST_RECOVERY_SECRET)
        .expect("Failed to add recovery slot");

    let _enc_config = enc_engine
        .encrypt_file(&export_db_path, &encrypt_dir, |_, _| {})
        .expect("Encryption failed");

    assert!(encrypt_dir.join("config.json").exists());
    assert!(encrypt_dir.join("payload").exists());

    // 4. Bundle (simulating Bundle Step)
    let bundle_dir = temp_dir.path().join("bundle");
    let builder = BundleBuilder::new()
        .title("E2E Test Archive")
        .description("Test archive for E2E pipeline")
        .generate_qr(false) // Skip QR generation to avoid dependency issues
        .recovery_secret(Some(TEST_RECOVERY_SECRET.to_vec()));

    let bundle = builder
        .build(&encrypt_dir, &bundle_dir, |_, _| {})
        .expect("Bundle failed");

    assert!(bundle.site_dir.join("index.html").exists());
    assert!(bundle.private_dir.join("recovery-secret.txt").exists());

    PipelineArtifacts {
        export_db_path,
        bundle,
    }
}

#[test]
fn test_pages_export_pipeline_e2e() {
    let temp_dir = TempDir::new().unwrap();
    let artifacts = build_pipeline(&temp_dir);
    let bundle_root = artifacts.bundle.site_dir.parent().expect("bundle root");

    // Verify (CLI)
    // Run `cass pages --verify <bundle_root>` to validate the bundle integrity and structure
    let mut cmd = cargo_bin_cmd!("cass");
    let assert = cmd
        .arg("pages")
        .arg("--verify")
        .arg(bundle_root)
        .arg("--json")
        .assert();

    assert.success();
}

#[test]
fn test_pages_pipeline_decrypt_roundtrip() {
    let temp_dir = TempDir::new().unwrap();
    let artifacts = build_pipeline(&temp_dir);
    let bundle_root = artifacts.bundle.site_dir.parent().expect("bundle root");

    // Unlock with password and decrypt
    let config = load_config(bundle_root).expect("load config");
    let decryptor =
        DecryptionEngine::unlock_with_password(config, TEST_PASSWORD).expect("unlock password");
    let decrypted_path = temp_dir.path().join("decrypted.db");
    decryptor
        .decrypt_to_file(bundle_root, &decrypted_path, |_, _| {})
        .expect("decrypt with password");

    assert_eq!(
        fs::read(&artifacts.export_db_path).unwrap(),
        fs::read(&decrypted_path).unwrap()
    );

    // Unlock with recovery secret and decrypt
    let config = load_config(bundle_root).expect("load config");
    let decryptor = DecryptionEngine::unlock_with_recovery(config, TEST_RECOVERY_SECRET)
        .expect("unlock recovery");
    let decrypted_recovery_path = temp_dir.path().join("decrypted_recovery.db");
    decryptor
        .decrypt_to_file(bundle_root, &decrypted_recovery_path, |_, _| {})
        .expect("decrypt with recovery");

    assert_eq!(
        fs::read(&artifacts.export_db_path).unwrap(),
        fs::read(&decrypted_recovery_path).unwrap()
    );
}

#[test]
fn test_pages_config_validate_cli() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("pages-config.json");
    let config = r#"{
  "filters": {
    "agents": ["claude-code"],
    "path_mode": "relative"
  },
  "encryption": {
    "password": "test-password-123",
    "no_encryption": false,
    "i_understand_risks": false,
    "generate_recovery": true,
    "generate_qr": false,
    "compression": "deflate",
    "chunk_size": 8388608
  },
  "bundle": {
    "title": "Test Archive",
    "description": "CLI config validation test",
    "hide_metadata": false
  },
  "deployment": {
    "target": "local",
    "output_dir": "./cass-export",
    "repo": null,
    "branch": null
  }
}"#;

    fs::write(&config_path, config).unwrap();

    let mut cmd = cargo_bin_cmd!("cass");
    let assert = cmd
        .arg("pages")
        .arg("--config")
        .arg(&config_path)
        .arg("--validate-config")
        .arg("--json")
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let payload: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(payload["valid"].as_bool(), Some(true));
}

#[test]
fn test_pages_bundle_key_add_revoke_cycle() {
    let temp_dir = TempDir::new().unwrap();
    let artifacts = build_pipeline(&temp_dir);
    let bundle_root = artifacts.bundle.site_dir.parent().expect("bundle root");

    // Add second password slot
    let slot_id = key_add_password(bundle_root, TEST_PASSWORD, TEST_PASSWORD_2).unwrap();
    assert_eq!(
        slot_id, 2,
        "Expected slot id 2 after password+recovery slots"
    );

    let list = key_list(bundle_root).unwrap();
    assert_eq!(list.active_slots, 3);

    // Revoke original password slot using second password
    let revoke = key_revoke(bundle_root, TEST_PASSWORD_2, 0).unwrap();
    assert_eq!(revoke.revoked_slot_id, 0);
    assert_eq!(revoke.remaining_slots, 2);

    // Original password should fail
    let config = load_config(bundle_root).unwrap();
    assert!(DecryptionEngine::unlock_with_password(config, TEST_PASSWORD).is_err());

    // Second password should still work
    let config = load_config(bundle_root).unwrap();
    assert!(DecryptionEngine::unlock_with_password(config, TEST_PASSWORD_2).is_ok());

    let verified = verify_bundle(bundle_root, false).expect("verify after key mutation");
    assert_eq!(verified.status, "valid");
}

#[test]
fn test_pages_bundle_verify_detects_corruption() {
    let temp_dir = TempDir::new().unwrap();
    let artifacts = build_pipeline(&temp_dir);
    let site_dir = &artifacts.bundle.site_dir;

    // Baseline: verify passes
    let baseline = verify_bundle(site_dir, false).expect("verify baseline");
    assert_eq!(baseline.status, "valid");

    // Corrupt a payload chunk
    let payload_dir = site_dir.join("payload");
    let chunk = fs::read_dir(&payload_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().map(|e| e == "bin").unwrap_or(false))
        .expect("payload chunk");
    fs::write(&chunk, b"corrupted payload").unwrap();

    let result = verify_bundle(site_dir, false).expect("verify after corruption");
    assert_eq!(result.status, "invalid");
}

#[test]
fn test_secret_scan_gating() {
    let temp_dir = TempDir::new().unwrap();

    // Setup XDG_DATA_HOME structure
    let xdg_data_home = temp_dir.path().join("xdg_data");
    let cass_data_dir = xdg_data_home.join("coding-agent-search");
    fs::create_dir_all(&cass_data_dir).unwrap();

    setup_db_with_secret(&cass_data_dir);

    // 1. Scan secrets (report only)
    let mut cmd = cargo_bin_cmd!("cass");
    let output = cmd
        .env("XDG_DATA_HOME", &xdg_data_home)
        .arg("pages")
        .arg("--scan-secrets")
        .arg("--json")
        .output()
        .unwrap();

    if !output.status.success() {
        eprintln!("Stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(
        output.status.success(),
        "Scan should succeed in report mode"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid json output");

    let findings = json.get("findings").expect("findings field in output");
    let findings_array = findings.as_array().expect("findings should be array");
    assert!(!findings_array.is_empty(), "Should detect inserted secret");

    // Check that we found the specific type of secret (openai_key pattern)
    let found_api_key = findings_array.iter().any(|f| {
        f.get("kind")
            .and_then(|k| k.as_str())
            .map(|s| s == "openai_key")
            .unwrap_or(false)
    });
    assert!(
        found_api_key,
        "Should detect the test API key (openai_key pattern)"
    );

    // 2. Fail on secrets
    let mut cmd_fail = cargo_bin_cmd!("cass");
    cmd_fail
        .env("XDG_DATA_HOME", &xdg_data_home)
        .arg("pages")
        .arg("--scan-secrets")
        .arg("--fail-on-secrets")
        .assert()
        .failure(); // Should exit with non-zero code
}

fn setup_db(data_dir: &Path) {
    setup_db_internal(data_dir, false);
}

fn setup_db_with_secret(data_dir: &Path) {
    setup_db_internal(data_dir, true);
}

fn setup_db_internal(data_dir: &Path, include_secret: bool) {
    let db_path = data_dir.join("agent_search.db");
    if let Some(p) = db_path.parent() {
        fs::create_dir_all(p).unwrap();
    }

    // Initialize DB with schema
    let storage = SqliteStorage::open(&db_path).expect("Failed to open storage");

    // Create Agent
    let agent = Agent {
        id: None,
        slug: "claude_code".to_string(),
        name: "Claude Code".to_string(),
        version: None,
        kind: AgentKind::Cli,
    };
    let agent_id = storage.ensure_agent(&agent).expect("ensure agent");

    // Create Workspace
    let workspace_path = Path::new("/home/user/projects/test");
    let workspace_id = Some(
        storage
            .ensure_workspace(workspace_path, None)
            .expect("ensure workspace"),
    );

    let content = if include_secret {
        // Use a valid OpenAI key format (sk- followed by 20+ alphanumeric chars, no extra hyphens)
        "I accidentally pasted my key: sk-TESTabcdefghijklmnopqrstuvwxyz012345"
    } else {
        "Agent response 1"
    };

    // Create a fixture conversation
    let conversation = ConversationFixtureBuilder::new("claude_code")
        .title("Test Conversation")
        .workspace(workspace_path)
        .source_path("/home/user/.claude/projects/test/session.jsonl")
        .messages(5)
        .with_content(0, "User message 1")
        .with_content(1, content)
        .build_conversation();

    // Insert into DB
    storage
        .insert_conversation_tree(agent_id, workspace_id, &conversation)
        .expect("Failed to insert conversation");
}
