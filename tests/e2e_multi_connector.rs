//! E2E tests for multi-connector scenarios.
//!
//! These tests verify that multiple connectors work together correctly:
//! - Multiple connectors can be indexed in a single run
//! - Search returns results from all indexed connectors
//! - Agent filtering correctly isolates connector results
//! - Results are properly attributed to their source connector

use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::path::Path;

mod util;
use util::EnvGuard;
use util::e2e_log::{E2ePerformanceMetrics, PhaseTracker};

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_multi_connector", test_name)
}

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
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "Linux-specific test (XDG_DATA_HOME paths)"
)]
fn multi_connector_pipeline() {
    let tracker = tracker_for("multi_connector_pipeline");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let xdg_data = home.join("xdg_data");

    fs::create_dir_all(&xdg_data).unwrap();

    // Override env vars
    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_xdg = EnvGuard::set("XDG_DATA_HOME", xdg_data.to_string_lossy());

    // Setup fixture roots
    let dot_codex = home.join(".codex");
    let dot_claude = home.join(".claude");
    let dot_gemini = home.join(".gemini");
    let dot_config = home.join(".config");

    let _guard_codex = EnvGuard::set("CODEX_HOME", dot_codex.to_string_lossy());
    let _guard_gemini = EnvGuard::set("GEMINI_HOME", dot_gemini.to_string_lossy());

    // Phase: Create fixtures for all connectors
    let phase_start = tracker.start("setup_fixtures", Some("Create fixtures for 5 connectors"));
    make_codex_fixture(&dot_codex);
    make_claude_fixture(&dot_claude);
    make_gemini_fixture(&dot_gemini);
    make_cline_fixture(&dot_config);
    make_amp_fixture(&xdg_data);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    tracker.end("setup_fixtures", Some("Create fixtures for 5 connectors"), phase_start);

    // Phase: Full index
    let phase_start = tracker.start("run_index_full", Some("Run full index across all connectors"));
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
    tracker.end("run_index_full", Some("Run full index across all connectors"), phase_start);

    // Phase: Search all connectors
    let phase_start = tracker.start("search_all_connectors", Some("Search and verify all 5 connector results"));
    let search_start = std::time::Instant::now();
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
    let search_duration = search_start.elapsed().as_millis() as u64;

    assert!(output.status.success());
    let json_out: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json_out
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    let found_agents: std::collections::HashSet<&str> = hits
        .iter()
        .filter_map(|h| h.get("agent").and_then(|s| s.as_str()))
        .collect();

    assert!(found_agents.contains("codex"), "Missing codex hit. Found: {found_agents:?}");
    assert!(found_agents.contains("claude_code"), "Missing claude hit. Found: {found_agents:?}");
    assert!(found_agents.contains("gemini"), "Missing gemini hit. Found: {found_agents:?}");
    assert!(found_agents.contains("cline"), "Missing cline hit. Found: {found_agents:?}");
    assert!(found_agents.contains("amp"), "Missing amp hit. Found: {found_agents:?}");
    tracker.end("search_all_connectors", Some("Search and verify all 5 connector results"), phase_start);

    tracker.metrics(
        "search_all_connectors",
        &E2ePerformanceMetrics::new()
            .with_duration(search_duration)
            .with_custom("hit_count", serde_json::json!(hits.len()))
            .with_custom("agent_count", serde_json::json!(found_agents.len())),
    );

    // Phase: Incremental index test
    let phase_start = tracker.start("incremental_index", Some("Add new file and verify incremental index"));
    std::thread::sleep(std::time::Duration::from_secs(2));

    let sessions = dot_codex.join("sessions/2025/11/22");
    fs::create_dir_all(&sessions).unwrap();

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let content = format!(
        r#"{{"type": "event_msg", "timestamp": {now_ts}, "payload": {{"type": "user_message", "message": "codex_new"}}}}"#
    );
    fs::write(sessions.join("rollout-2.jsonl"), content).unwrap();

    cargo_bin_cmd!("cass")
        .arg("index")
        .arg("--data-dir")
        .arg(&data_dir)
        .env("HOME", home.to_string_lossy().as_ref())
        .env("XDG_DATA_HOME", xdg_data.to_string_lossy().as_ref())
        .env("CODEX_HOME", dot_codex.to_string_lossy().as_ref())
        .env("GEMINI_HOME", dot_gemini.to_string_lossy().as_ref())
        .assert()
        .success();

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
    assert!(!hits_inc.is_empty(), "Incremental index failed to pick up new file");
    assert_eq!(hits_inc[0]["content"], "codex_new");
    tracker.end("incremental_index", Some("Add new file and verify incremental index"), phase_start);

    // Phase: Agent filter test
    let phase_start = tracker.start("test_agent_filter", Some("Verify agent filter isolates results"));
    let filter_start = std::time::Instant::now();
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
    let filter_duration = filter_start.elapsed().as_millis() as u64;

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
    tracker.end("test_agent_filter", Some("Verify agent filter isolates results"), phase_start);

    tracker.metrics(
        "agent_filter_query",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_duration)
            .with_custom("filtered_hit_count", serde_json::json!(hits_filter.len())),
    );

    tracker.complete();
}

// ============================================================================
// Cross-platform multi-connector tests (work on macOS and Linux)
// These tests use Codex and Claude Code which rely on HOME env var
// ============================================================================

/// Creates a Codex session with specific date and content.
fn make_codex_session(
    codex_home: &Path,
    date_path: &str,
    filename: &str,
    content: &str,
    ts_millis: u64,
) {
    let sessions = codex_home.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {ts_millis}, "payload": {{"type": "user_message", "message": "{content}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{content}_response"}}}}"#,
        ts_millis + 1000
    );
    fs::write(file, sample).unwrap();
}

/// Creates a Claude Code session with specific content.
fn make_claude_session(
    claude_home: &Path,
    project_name: &str,
    filename: &str,
    content: &str,
    ts_iso: &str,
) {
    let project = claude_home.join(format!("projects/{project_name}"));
    fs::create_dir_all(&project).unwrap();
    let file = project.join(filename);
    let sample = format!(
        r#"{{"type": "user", "timestamp": "{ts_iso}", "message": {{"role": "user", "content": "{content}"}}}}
{{"type": "assistant", "timestamp": "{ts_iso}", "message": {{"role": "assistant", "content": "{content}_response"}}}}"#
    );
    fs::write(file, sample).unwrap();
}

/// Test: Multiple connectors can be indexed and searched together
#[test]
fn multi_connector_codex_and_claude() {
    let tracker = tracker_for("multi_connector_codex_and_claude");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Phase: Create fixtures
    let phase_start = tracker.start("setup_fixtures", Some("Create Codex and Claude sessions"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-multi.jsonl",
        "multitest codex_unique_content",
        1732118400000,
    );
    make_claude_session(
        &claude_home,
        "multi-project",
        "session-multi.jsonl",
        "multitest claude_unique_content",
        "2024-11-20T10:00:00Z",
    );
    tracker.end("setup_fixtures", Some("Create Codex and Claude sessions"), phase_start);

    // Phase: Index
    let phase_start = tracker.start("run_index", Some("Run full index"));
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), phase_start);

    // Phase: Search and verify
    let phase_start = tracker.start("search_multi_connector", Some("Search shared term across connectors"));
    let search_start = std::time::Instant::now();
    let output = cargo_bin_cmd!("cass")
        .args(["search", "multitest", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");
    let search_duration = search_start.elapsed().as_millis() as u64;

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    let agents: std::collections::HashSet<_> =
        hits.iter().filter_map(|h| h["agent"].as_str()).collect();

    assert!(agents.contains("codex"), "Should find codex results. Agents found: {agents:?}");
    assert!(agents.contains("claude_code"), "Should find claude_code results. Agents found: {agents:?}");
    assert!(hits.len() >= 2, "Should have at least 2 hits from different connectors");
    tracker.end("search_multi_connector", Some("Search shared term across connectors"), phase_start);

    tracker.metrics(
        "search_multi_connector",
        &E2ePerformanceMetrics::new()
            .with_duration(search_duration)
            .with_custom("hit_count", serde_json::json!(hits.len()))
            .with_custom("agent_count", serde_json::json!(agents.len())),
    );

    tracker.complete();
}

/// Test: Agent filter isolates results to specific connector
#[test]
fn multi_connector_agent_filter_isolation() {
    let tracker = tracker_for("multi_connector_agent_filter_isolation");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Phase: Setup
    let phase_start = tracker.start("setup_fixtures", Some("Create sessions with shared search term"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-iso.jsonl",
        "isolationtest codex_data",
        1732118400000,
    );
    make_claude_session(
        &claude_home,
        "iso-project",
        "session-iso.jsonl",
        "isolationtest claude_data",
        "2024-11-20T10:00:00Z",
    );
    tracker.end("setup_fixtures", Some("Create sessions with shared search term"), phase_start);

    // Phase: Index
    let phase_start = tracker.start("run_index", Some("Run full index"));
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), phase_start);

    // Phase: Filter by codex
    let phase_start = tracker.start("filter_codex", Some("Search with agent=codex filter"));
    let codex_start = std::time::Instant::now();
    let codex_output = cargo_bin_cmd!("cass")
        .args([
            "search", "isolationtest", "--agent", "codex", "--robot", "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");
    let codex_duration = codex_start.elapsed().as_millis() as u64;

    assert!(codex_output.status.success());
    let codex_json: serde_json::Value =
        serde_json::from_slice(&codex_output.stdout).expect("valid json");
    let codex_hits = codex_json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(!codex_hits.is_empty(), "Should find codex hits");
    for hit in codex_hits {
        assert_eq!(hit["agent"], "codex", "All hits should be from codex when filtering");
    }
    tracker.end("filter_codex", Some("Search with agent=codex filter"), phase_start);

    tracker.metrics(
        "filter_codex",
        &E2ePerformanceMetrics::new()
            .with_duration(codex_duration)
            .with_custom("hit_count", serde_json::json!(codex_hits.len())),
    );

    // Phase: Filter by claude_code
    let phase_start = tracker.start("filter_claude", Some("Search with agent=claude_code filter"));
    let claude_start = std::time::Instant::now();
    let claude_output = cargo_bin_cmd!("cass")
        .args([
            "search", "isolationtest", "--agent", "claude_code", "--robot", "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");
    let claude_duration = claude_start.elapsed().as_millis() as u64;

    assert!(claude_output.status.success());
    let claude_json: serde_json::Value =
        serde_json::from_slice(&claude_output.stdout).expect("valid json");
    let claude_hits = claude_json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(!claude_hits.is_empty(), "Should find claude_code hits");
    for hit in claude_hits {
        assert_eq!(hit["agent"], "claude_code", "All hits should be from claude_code when filtering");
    }
    tracker.end("filter_claude", Some("Search with agent=claude_code filter"), phase_start);

    tracker.metrics(
        "filter_claude",
        &E2ePerformanceMetrics::new()
            .with_duration(claude_duration)
            .with_custom("hit_count", serde_json::json!(claude_hits.len())),
    );

    tracker.complete();
}

/// Test: Each connector's unique content is properly indexed
#[test]
fn multi_connector_unique_content() {
    let tracker = tracker_for("multi_connector_unique_content");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Phase: Setup
    let phase_start = tracker.start("setup_fixtures", Some("Create sessions with unique content per connector"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-unique.jsonl",
        "codexonly_xyzzy uniqueterm",
        1732118400000,
    );
    make_claude_session(
        &claude_home,
        "unique-project",
        "session-unique.jsonl",
        "claudeonly_plugh uniqueterm",
        "2024-11-20T10:00:00Z",
    );
    tracker.end("setup_fixtures", Some("Create sessions with unique content per connector"), phase_start);

    // Phase: Index
    let phase_start = tracker.start("run_index", Some("Run full index"));
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), phase_start);

    // Phase: Search codex-specific content
    let phase_start = tracker.start("search_codex_unique", Some("Search for codex-specific term"));
    let codex_output = cargo_bin_cmd!("cass")
        .args(["search", "codexonly_xyzzy", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(codex_output.status.success());
    let codex_json: serde_json::Value =
        serde_json::from_slice(&codex_output.stdout).expect("valid json");
    let codex_hits = codex_json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(!codex_hits.is_empty(), "Should find codex-specific content");
    assert!(
        codex_hits.iter().all(|h| h["agent"] == "codex"),
        "Codex-specific search should only return codex results"
    );
    tracker.end("search_codex_unique", Some("Search for codex-specific term"), phase_start);

    // Phase: Search claude-specific content
    let phase_start = tracker.start("search_claude_unique", Some("Search for claude-specific term"));
    let claude_output = cargo_bin_cmd!("cass")
        .args(["search", "claudeonly_plugh", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    assert!(claude_output.status.success());
    let claude_json: serde_json::Value =
        serde_json::from_slice(&claude_output.stdout).expect("valid json");
    let claude_hits = claude_json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(!claude_hits.is_empty(), "Should find claude-specific content");
    assert!(
        claude_hits.iter().all(|h| h["agent"] == "claude_code"),
        "Claude-specific search should only return claude_code results"
    );
    tracker.end("search_claude_unique", Some("Search for claude-specific term"), phase_start);

    tracker.complete();
}

/// Test: Aggregation by agent works with multiple connectors
#[test]
fn multi_connector_aggregation() {
    let tracker = tracker_for("multi_connector_aggregation");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Phase: Setup
    let phase_start = tracker.start("setup_fixtures", Some("Create multiple sessions per connector"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-agg1.jsonl",
        "aggtest codex_first",
        1732118400000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-agg2.jsonl",
        "aggtest codex_second",
        1732204800000,
    );
    make_claude_session(
        &claude_home,
        "agg-project1",
        "session-agg1.jsonl",
        "aggtest claude_first",
        "2024-11-20T10:00:00Z",
    );
    make_claude_session(
        &claude_home,
        "agg-project2",
        "session-agg2.jsonl",
        "aggtest claude_second",
        "2024-11-21T10:00:00Z",
    );
    tracker.end("setup_fixtures", Some("Create multiple sessions per connector"), phase_start);

    // Phase: Index
    let phase_start = tracker.start("run_index", Some("Run full index"));
    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), phase_start);

    // Phase: Aggregation search
    let phase_start = tracker.start("search_aggregate", Some("Search with agent aggregation"));
    let agg_start = std::time::Instant::now();
    let output = cargo_bin_cmd!("cass")
        .args([
            "search", "aggtest", "--aggregate", "agent", "--robot", "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");
    let agg_duration = agg_start.elapsed().as_millis() as u64;

    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");

    let aggregations = json.get("aggregations").and_then(|a| a.as_object());
    assert!(aggregations.is_some(), "Should have aggregations in response");

    let aggs = aggregations.unwrap();
    let agent_agg = aggs.get("agent").and_then(|a| a.as_object());
    assert!(agent_agg.is_some(), "Should have agent aggregation");

    let buckets = agent_agg
        .unwrap()
        .get("buckets")
        .and_then(|b| b.as_array())
        .expect("Should have buckets array");

    let agent_keys: std::collections::HashSet<_> = buckets
        .iter()
        .filter_map(|b| b.get("key").and_then(|k| k.as_str()))
        .collect();

    assert!(agent_keys.contains("codex"), "Agent aggregation should include codex. Keys: {agent_keys:?}");
    assert!(agent_keys.contains("claude_code"), "Agent aggregation should include claude_code. Keys: {agent_keys:?}");
    tracker.end("search_aggregate", Some("Search with agent aggregation"), phase_start);

    tracker.metrics(
        "aggregation_query",
        &E2ePerformanceMetrics::new()
            .with_duration(agg_duration)
            .with_custom("bucket_count", serde_json::json!(buckets.len())),
    );

    tracker.complete();
}

/// Test: Incremental indexing works across multiple connectors
#[test]
fn multi_connector_incremental_index() {
    logged_test!(
        "multi_connector_incremental_index",
        "e2e_multi_connector",
        {
            let tmp = tempfile::TempDir::new().unwrap();
            let home = tmp.path();
            let codex_home = home.join(".codex");
            let claude_home = home.join(".claude");
            let data_dir = home.join("cass_data");
            fs::create_dir_all(&data_dir).unwrap();

            let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
            let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

            // Phase 1: Create initial sessions
            make_codex_session(
                &codex_home,
                "2024/11/20",
                "rollout-incr1.jsonl",
                "incrtest initial_codex",
                1732118400000,
            );
            make_claude_session(
                &claude_home,
                "incr-project1",
                "session-incr1.jsonl",
                "incrtest initial_claude",
                "2024-11-20T10:00:00Z",
            );

            // Full index
            cargo_bin_cmd!("cass")
                .args(["index", "--full", "--data-dir"])
                .arg(&data_dir)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", home)
                .assert()
                .success();

            // Verify initial sessions indexed
            let output1 = cargo_bin_cmd!("cass")
                .args(["search", "incrtest", "--robot", "--data-dir"])
                .arg(&data_dir)
                .env("HOME", home)
                .output()
                .expect("search command");

            let json1: serde_json::Value =
                serde_json::from_slice(&output1.stdout).expect("valid json");
            let hits1 = json1
                .get("hits")
                .and_then(|h| h.as_array())
                .expect("hits array");
            assert!(hits1.len() >= 2, "Should have initial sessions indexed");

            // Phase 2: Add new sessions
            std::thread::sleep(std::time::Duration::from_secs(2)); // Ensure mtime difference

            // Use current timestamps so messages aren't filtered out
            let now_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let now_iso = chrono::Utc::now().to_rfc3339();

            make_codex_session(
                &codex_home,
                "2024/11/21",
                "rollout-incr2.jsonl",
                "incrtest new_codex",
                now_ts,
            );
            make_claude_session(
                &claude_home,
                "incr-project2",
                "session-incr2.jsonl",
                "incrtest new_claude",
                &now_iso,
            );

            // Incremental index (no --full flag)
            cargo_bin_cmd!("cass")
                .args(["index", "--data-dir"])
                .arg(&data_dir)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", home)
                .assert()
                .success();

            // Verify all sessions now indexed
            let output2 = cargo_bin_cmd!("cass")
                .args(["search", "incrtest", "--robot", "--data-dir"])
                .arg(&data_dir)
                .env("HOME", home)
                .output()
                .expect("search command");

            let json2: serde_json::Value =
                serde_json::from_slice(&output2.stdout).expect("valid json");
            let hits2 = json2
                .get("hits")
                .and_then(|h| h.as_array())
                .expect("hits array");

            // Should have both old and new sessions
            assert!(
                hits2.len() > hits1.len(),
                "Incremental index should add new sessions. hits1={}, hits2={}",
                hits1.len(),
                hits2.len()
            );

            // Check specific content
            let has_initial = hits2
                .iter()
                .any(|h| h["content"].as_str().unwrap_or("").contains("initial"));
            let has_new = hits2
                .iter()
                .any(|h| h["content"].as_str().unwrap_or("").contains("new"));

            assert!(
                has_initial,
                "Should still have initial sessions after incremental index"
            );
            assert!(has_new, "Should have new sessions after incremental index");
        }
    );
}

/// Test: Multiple agent filter works correctly
#[test]
fn multi_connector_multiple_agent_filter() {
    logged_test!(
        "multi_connector_multiple_agent_filter",
        "e2e_multi_connector",
        {
            let tmp = tempfile::TempDir::new().unwrap();
            let home = tmp.path();
            let codex_home = home.join(".codex");
            let claude_home = home.join(".claude");
            let data_dir = home.join("cass_data");
            fs::create_dir_all(&data_dir).unwrap();

            let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
            let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

            make_codex_session(
                &codex_home,
                "2024/11/20",
                "rollout-maf.jsonl",
                "multiagent codex_content",
                1732118400000,
            );
            make_claude_session(
                &claude_home,
                "multi-agent-project",
                "session-maf.jsonl",
                "multiagent claude_content",
                "2024-11-20T10:00:00Z",
            );

            cargo_bin_cmd!("cass")
                .args(["index", "--full", "--data-dir"])
                .arg(&data_dir)
                .env("CODEX_HOME", &codex_home)
                .env("HOME", home)
                .assert()
                .success();

            // Filter by multiple agents (both codex and claude_code)
            let output = cargo_bin_cmd!("cass")
                .args([
                    "search",
                    "multiagent",
                    "--agent",
                    "codex",
                    "--agent",
                    "claude_code",
                    "--robot",
                    "--data-dir",
                ])
                .arg(&data_dir)
                .env("HOME", home)
                .output()
                .expect("search command");

            assert!(output.status.success());
            let json: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("valid json");
            let hits = json
                .get("hits")
                .and_then(|h| h.as_array())
                .expect("hits array");

            // Should have hits from both specified agents
            let agents: std::collections::HashSet<_> =
                hits.iter().filter_map(|h| h["agent"].as_str()).collect();

            assert!(
                agents.contains("codex") && agents.contains("claude_code"),
                "Should find results from both specified agents. Found: {agents:?}"
            );
        }
    );
}

/// Test: Empty connector doesn't break indexing of other connectors
#[test]
fn multi_connector_empty_connector() {
    logged_test!("multi_connector_empty_connector", "e2e_multi_connector", {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let codex_home = home.join(".codex");
        let data_dir = home.join("cass_data");

        // Only create data_dir and codex_home, leave claude_home empty/nonexistent
        fs::create_dir_all(&data_dir).unwrap();

        let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
        let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

        // Create only codex session
        make_codex_session(
            &codex_home,
            "2024/11/20",
            "rollout-only.jsonl",
            "singleconnector codex_only",
            1732118400000,
        );

        // Index should succeed even with non-existent claude_home
        cargo_bin_cmd!("cass")
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();

        // Search should work and return codex results
        let output = cargo_bin_cmd!("cass")
            .args(["search", "singleconnector", "--robot", "--data-dir"])
            .arg(&data_dir)
            .env("HOME", home)
            .output()
            .expect("search command");

        assert!(output.status.success());
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
        let hits = json
            .get("hits")
            .and_then(|h| h.as_array())
            .expect("hits array");

        assert!(!hits.is_empty(), "Should find codex results");
        assert!(
            hits.iter().all(|h| h["agent"] == "codex"),
            "All results should be from codex"
        );
    });
}
