use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::path::Path;

mod util;
use util::EnvGuard;

fn make_codex_fixture(root: &Path) {
    let sessions = root.join("sessions/2025/11/21");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-1.jsonl");
    // Modern Codex JSONL format (envelope)
    let sample = r#"{"type": "event_msg", "timestamp": 1700000000000, "payload": {"type": "user_message", "message": "codex_user"}}
{"type": "response_item", "timestamp": 1700000001000, "payload": {"role": "assistant", "content": "codex_assistant"}}
"#;
    fs::write(file, sample).unwrap();
}

fn make_claude_fixture(root: &Path) {
    let project = root.join("projects/test-project");
    fs::create_dir_all(&project).unwrap();
    let file = project.join("session.jsonl");
    // Claude Code format
    let sample = r#"{"type": "user", "timestamp": "2023-11-21T10:00:00Z", "message": {"role": "user", "content": "claude_user"}}
{"type": "assistant", "timestamp": "2023-11-21T10:00:05Z", "message": {"role": "assistant", "content": "claude_assistant"}}
"#;
    fs::write(file, sample).unwrap();
}

fn make_gemini_fixture(root: &Path) {
    let project_hash = root.join("tmp/hash123/chats");
    fs::create_dir_all(&project_hash).unwrap();
    let file = project_hash.join("session-1.json"); // Must start with session-
    // Gemini CLI format
    let sample = r#"{
  "messages": [
    {"role": "user", "timestamp": 1700000000000, "content": "gemini_user"},
    {"role": "model", "timestamp": 1700000001000, "content": "gemini_assistant"}
  ]
}"#;
    fs::write(file, sample).unwrap();
}

fn make_cline_fixture(root: &Path) {
    let task_dir = root.join("Code/User/globalStorage/saoudrizwan.claude-dev/task_123");
    fs::create_dir_all(&task_dir).unwrap();

    let ui_messages = task_dir.join("ui_messages.json");
    let sample = r#"[
  {"role": "user", "ts": 1700000000000, "content": "cline_user"},
  {"role": "assistant", "ts": 1700000001000, "content": "cline_assistant"}
]"#;
    fs::write(ui_messages, sample).unwrap();

    let metadata = task_dir.join("task_metadata.json");
    fs::write(metadata, r#"{"id": "task_123", "title": "Cline Task"}"#).unwrap();
}

fn make_amp_fixture(root: &Path) {
    let amp_dir = root.join("amp/cache");
    fs::create_dir_all(&amp_dir).unwrap();
    let file = amp_dir.join("thread_abc.json");
    let sample = r#"{"messages": [
        {"role": "user", "created_at": 1700000000000, "content": "amp_user"},
        {"role": "assistant", "created_at": 1700000001000, "content": "amp_assistant"}
    ]}"#;
    fs::write(file, sample).unwrap();
}

#[test]
fn multi_connector_pipeline() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let xdg_data = home.join("xdg_data");
    let _config_home = home.join(".config"); // For Cline on Linux usually, but our fixture path was mostly hardcoded in the connector? 
    // ClineConnector uses:
    // dirs::home_dir().join(".config/Code/User/globalStorage/saoudrizwan.claude-dev")
    // So we just need HOME set correctly.

    fs::create_dir_all(&xdg_data).unwrap();

    // Override env vars
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg_data.to_string_lossy());

    // Setup fixture roots
    let dot_codex = home.join(".codex");
    let dot_claude = home.join(".claude");
    let dot_gemini = home.join(".gemini");
    let dot_config = home.join(".config"); // for cline
    // Amp uses XDG_DATA_HOME/amp which is xdg_data/amp

    // Specific env overrides for connectors that support it
    let _guard_codex = EnvGuard::set("CODEX_HOME", dot_codex.to_string_lossy());
    let _guard_gemini = EnvGuard::set("GEMINI_HOME", dot_gemini.to_string_lossy());

    // Create fixtures
    make_codex_fixture(&dot_codex);
    make_claude_fixture(&dot_claude);
    make_gemini_fixture(&dot_gemini);
    make_cline_fixture(&dot_config); // Will be under .config/Code/... which matches Linux path relative to HOME
    make_amp_fixture(&xdg_data);

    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // 1. INDEX
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .env("HOME", home.to_string_lossy().as_ref())
        .env("XDG_DATA_HOME", xdg_data.to_string_lossy().as_ref())
        .env("CODEX_HOME", dot_codex.to_string_lossy().as_ref())
        .env("GEMINI_HOME", dot_gemini.to_string_lossy().as_ref())
        .assert()
        .success();

    // 2. SEARCH (Robot mode)
    // Search for "user" - should find hits from all 5 agents
    let output = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("user")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .env("HOME", home.to_string_lossy().as_ref())
        .env("XDG_DATA_HOME", xdg_data.to_string_lossy().as_ref())
        .output()
        .expect("failed to execute search");

    assert!(output.status.success());
    let json_out: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");

    // Check results
    let hits = json_out
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    let found_agents: std::collections::HashSet<&str> = hits
        .iter()
        .filter_map(|h| h.get("agent").and_then(|s| s.as_str()))
        .collect();

    assert!(
        found_agents.contains("codex"),
        "Missing codex hit. Found: {:?}",
        found_agents
    );
    assert!(
        found_agents.contains("claude_code"),
        "Missing claude hit. Found: {:?}",
        found_agents
    );
    assert!(
        found_agents.contains("gemini"),
        "Missing gemini hit. Found: {:?}",
        found_agents
    );
    assert!(
        found_agents.contains("cline"),
        "Missing cline hit. Found: {:?}",
        found_agents
    );
    assert!(
        found_agents.contains("amp"),
        "Missing amp hit. Found: {:?}",
        found_agents
    );

    // 3. INCREMENTAL TEST
    // Ensure mtime is strictly greater than last scan
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Add a new file to Codex with CURRENT timestamp so message isn't filtered out
    let sessions = dot_codex.join("sessions/2025/11/22");
    fs::create_dir_all(&sessions).unwrap();

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // Use modern envelope format
    let content = format!(
        r#"{{"type": "event_msg", "timestamp": {}, "payload": {{"type": "user_message", "message": "codex_new"}}}}"#,
        now_ts
    );
    fs::write(sessions.join("rollout-2.jsonl"), content).unwrap();

    // Index again (incremental)
    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Search for "codex_new"
    let output_inc = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("codex_new")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .output()
        .expect("failed to execute search");

    let json_inc: serde_json::Value =
        serde_json::from_slice(&output_inc.stdout).expect("valid json");
    let hits_inc = json_inc
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");
    assert!(
        !hits_inc.is_empty(),
        "Incremental index failed to pick up new file"
    );
    assert_eq!(hits_inc[0]["content"], "codex_new");

    // 4. FILTER TEST
    // Filter by agent=claude_code
    let output_filter = cargo_bin_cmd!("cass")
        .arg("search")
        .arg("user")
        .arg("--agent")
        .arg("claude_code")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .output()
        .expect("failed to execute search");

    let json_filter: serde_json::Value =
        serde_json::from_slice(&output_filter.stdout).expect("valid json");
    let hits_filter = json_filter
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    for hit in hits_filter {
        assert_eq!(hit["agent"], "claude_code");
    }
    assert!(!hits_filter.is_empty());
}
