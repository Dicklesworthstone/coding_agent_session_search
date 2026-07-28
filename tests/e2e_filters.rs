//! E2E tests for filter combinations.
//!
//! Tests all filter combinations work correctly end-to-end:
//! - Agent filter (--agent)
//! - Time filters (--since, --until, --days, --today, --week)
//! - Workspace filter (--workspace)
//! - Combined filters

use std::fs;
use std::path::Path;

mod util;
use util::e2e_log::{E2ePerformanceMetrics, PhaseTracker};

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_filters", test_name)
}

/// Creates a Codex session with specific date and content.
/// Timestamp should be in milliseconds.
fn make_codex_session_at(
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

/// Creates a Claude Code session with specific date and content.
fn make_claude_session_at(claude_home: &Path, project_name: &str, content: &str, ts_iso: &str) {
    let project = claude_home.join(format!("projects/{project_name}"));
    fs::create_dir_all(&project).unwrap();
    let file = project.join("session.jsonl");
    let sample = format!(
        r#"{{"type": "user", "timestamp": "{ts_iso}", "message": {{"role": "user", "content": "{content}"}}}}
{{"type": "assistant", "timestamp": "{ts_iso}", "message": {{"role": "assistant", "content": "{content}_response"}}}}"#
    );
    fs::write(file, sample).unwrap();
}

/// Test: Agent filter correctly limits results to specified connector
#[test]
fn filter_by_agent_codex() {
    let tracker = tracker_for("filter_by_agent_codex");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    let ps = tracker.start("setup_fixtures", Some("Create codex and claude sessions"));
    make_codex_session_at(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "codex_specific agenttest",
        1732118400000,
    );
    make_claude_session_at(
        &claude_home,
        "test-project",
        "claude_specific agenttest",
        "2024-11-20T10:00:00Z",
    );
    tracker.end(
        "setup_fixtures",
        Some("Create codex and claude sessions"),
        ps,
    );

    let ps = tracker.start("run_index", Some("Run full index"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), ps);

    let ps = tracker.start("test_agent_filter", Some("Search with --agent codex"));
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "agenttest",
            "--agent",
            "codex",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");
    let filter_duration = ps.elapsed().as_millis() as u64;
    tracker.end("test_agent_filter", Some("Search with --agent codex"), ps);

    let ps = tracker.start("verify_results", Some("Verify only codex hits returned"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    for hit in hits {
        assert_eq!(
            hit["agent"], "codex",
            "Expected only codex hits when filtering by agent=codex"
        );
    }
    assert!(!hits.is_empty(), "Should find at least one codex hit");
    tracker.end(
        "verify_results",
        Some("Verify only codex hits returned"),
        ps,
    );

    tracker.metrics(
        "filter_query_agent",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_duration)
            .with_custom("result_count", serde_json::json!(hits.len())),
    );
}

/// Test: Time filter --since correctly limits results
#[test]
fn filter_by_time_since() {
    let tracker = tracker_for("filter_by_time_since");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    let ps = tracker.start("setup_fixtures", Some("Create old and new sessions"));
    make_codex_session_at(
        &codex_home,
        "2024/11/15",
        "rollout-old.jsonl",
        "oldsession sincetest",
        1731682800000,
    );
    make_codex_session_at(
        &codex_home,
        "2024/11/25",
        "rollout-new.jsonl",
        "newsession sincetest",
        1732546800000,
    );
    tracker.end("setup_fixtures", Some("Create old and new sessions"), ps);

    let ps = tracker.start("run_index", Some("Run full index"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), ps);

    let ps = tracker.start("test_since_filter", Some("Search with --since 2024-11-20"));
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "sincetest",
            "--since",
            "2024-11-20",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    let filter_duration = ps.elapsed().as_millis() as u64;
    tracker.end(
        "test_since_filter",
        Some("Search with --since 2024-11-20"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify only new session returned"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        !hits.is_empty(),
        "Should find at least one hit with since filter"
    );
    for hit in hits {
        let content = hit["content"].as_str().unwrap_or("");
        assert!(
            content.contains("newsession"),
            "Should only find new session since 2024-11-20, got: {}",
            content
        );
    }
    tracker.end(
        "verify_results",
        Some("Verify only new session returned"),
        ps,
    );

    tracker.metrics(
        "filter_query_since",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_duration)
            .with_custom("result_count", serde_json::json!(hits.len())),
    );
}

/// Test: Time filter --until correctly limits results
#[test]
fn filter_by_time_until() {
    let tracker = tracker_for("filter_by_time_until");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    let ps = tracker.start("setup_fixtures", Some("Create old and new sessions"));
    make_codex_session_at(
        &codex_home,
        "2024/11/15",
        "rollout-old.jsonl",
        "oldsession untiltest",
        1731682800000,
    );
    make_codex_session_at(
        &codex_home,
        "2024/11/25",
        "rollout-new.jsonl",
        "newsession untiltest",
        1732546800000,
    );
    tracker.end("setup_fixtures", Some("Create old and new sessions"), ps);

    let ps = tracker.start("run_index", Some("Run full index"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), ps);

    let ps = tracker.start("test_until_filter", Some("Search with --until 2024-11-20"));
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "untiltest",
            "--until",
            "2024-11-20",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    let filter_duration = ps.elapsed().as_millis() as u64;
    tracker.end(
        "test_until_filter",
        Some("Search with --until 2024-11-20"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify only old session returned"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        !hits.is_empty(),
        "Should find at least one hit with until filter"
    );
    for hit in hits {
        let content = hit["content"].as_str().unwrap_or("");
        assert!(
            content.contains("oldsession"),
            "Should only find old session until 2024-11-20, got: {}",
            content
        );
    }
    tracker.end(
        "verify_results",
        Some("Verify only old session returned"),
        ps,
    );

    tracker.metrics(
        "filter_query_until",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_duration)
            .with_custom("result_count", serde_json::json!(hits.len())),
    );
}

/// Test: Combined time filters (--since AND --until) for date range
#[test]
fn filter_by_time_range() {
    let tracker = tracker_for("filter_by_time_range");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    let ps = tracker.start(
        "setup_fixtures",
        Some("Create early, middle, and late sessions"),
    );
    make_codex_session_at(
        &codex_home,
        "2024/11/10",
        "rollout-early.jsonl",
        "earlysession rangetest",
        1731250800000,
    );
    make_codex_session_at(
        &codex_home,
        "2024/11/20",
        "rollout-middle.jsonl",
        "middlesession rangetest",
        1732114800000,
    );
    make_codex_session_at(
        &codex_home,
        "2024/11/30",
        "rollout-late.jsonl",
        "latesession rangetest",
        1732978800000,
    );
    tracker.end(
        "setup_fixtures",
        Some("Create early, middle, and late sessions"),
        ps,
    );

    let ps = tracker.start("run_index", Some("Run full index"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), ps);

    let ps = tracker.start(
        "test_range_filter",
        Some("Search with --since/--until date range"),
    );
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "rangetest",
            "--since",
            "2024-11-15",
            "--until",
            "2024-11-25",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    let filter_duration = ps.elapsed().as_millis() as u64;
    tracker.end(
        "test_range_filter",
        Some("Search with --since/--until date range"),
        ps,
    );

    let ps = tracker.start(
        "verify_results",
        Some("Verify only middle session returned"),
    );
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        !hits.is_empty(),
        "Should find at least one hit in date range"
    );
    for hit in hits {
        let content = hit["content"].as_str().unwrap_or("");
        assert!(
            content.contains("middlesession"),
            "Should only find middle session in range, got: {}",
            content
        );
    }
    tracker.end(
        "verify_results",
        Some("Verify only middle session returned"),
        ps,
    );

    tracker.metrics(
        "filter_query_range",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_duration)
            .with_custom("result_count", serde_json::json!(hits.len())),
    );
}

/// Test: Combined agent + time filter
#[test]
fn filter_combined_agent_and_time() {
    let tracker = tracker_for("filter_combined_agent_and_time");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    let ps = tracker.start("setup_fixtures", Some("Create codex and claude sessions"));
    make_codex_session_at(
        &codex_home,
        "2024/11/15",
        "rollout-old.jsonl",
        "codex_combined_old combinedtest",
        1731682800000,
    );
    make_codex_session_at(
        &codex_home,
        "2024/11/25",
        "rollout-new.jsonl",
        "codex_combined_new combinedtest",
        1732546800000,
    );
    make_claude_session_at(
        &claude_home,
        "project-old",
        "claude_combined_old combinedtest",
        "2024-11-15T10:00:00Z",
    );
    make_claude_session_at(
        &claude_home,
        "project-new",
        "claude_combined_new combinedtest",
        "2024-11-25T10:00:00Z",
    );
    tracker.end(
        "setup_fixtures",
        Some("Create codex and claude sessions"),
        ps,
    );

    let ps = tracker.start("run_index", Some("Run full index"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), ps);

    let ps = tracker.start(
        "test_combined_filter",
        Some("Search with --agent codex --since"),
    );
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "combinedtest",
            "--agent",
            "codex",
            "--since",
            "2024-11-20",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    let filter_duration = ps.elapsed().as_millis() as u64;
    tracker.end(
        "test_combined_filter",
        Some("Search with --agent codex --since"),
        ps,
    );

    let ps = tracker.start(
        "verify_results",
        Some("Verify only new codex session returned"),
    );
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        !hits.is_empty(),
        "Should find at least one hit with combined filters"
    );
    for hit in hits {
        assert_eq!(hit["agent"], "codex", "Should only find codex hits");
        let content = hit["content"].as_str().unwrap_or("");
        assert!(
            content.contains("codex_combined_new"),
            "Should only find new codex session, got: {}",
            content
        );
    }
    tracker.end(
        "verify_results",
        Some("Verify only new codex session returned"),
        ps,
    );

    tracker.metrics(
        "filter_query_combined",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_duration)
            .with_custom("result_count", serde_json::json!(hits.len())),
    );
}

/// Test: Empty result set when filters exclude everything
#[test]
fn filter_no_matches() {
    let tracker = tracker_for("filter_no_matches");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    let ps = tracker.start("setup_fixtures", Some("Create November session"));
    make_codex_session_at(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "november nomatchtest",
        1732114800000,
    );
    tracker.end("setup_fixtures", Some("Create November session"), ps);

    let ps = tracker.start("run_index", Some("Run full index"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), ps);

    let ps = tracker.start(
        "test_no_match_filter",
        Some("Search with impossible date filter"),
    );
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "nomatchtest",
            "--until",
            "2024-10-01",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    let filter_duration = ps.elapsed().as_millis() as u64;
    tracker.end(
        "test_no_match_filter",
        Some("Search with impossible date filter"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify empty result set"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        hits.is_empty(),
        "Should find no hits when filter excludes all results"
    );
    tracker.end("verify_results", Some("Verify empty result set"), ps);

    tracker.metrics(
        "filter_query_no_match",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_duration)
            .with_custom("result_count", serde_json::json!(0)),
    );
}

/// Test: Workspace filter using --workspace flag
#[test]
fn filter_by_workspace() {
    let tracker = tracker_for("filter_by_workspace");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker.command_environment().with_home(home);

    let workspace_alpha = "/projects/workspace-alpha";
    let workspace_beta = "/projects/workspace-beta";

    let ps = tracker.start("setup_fixtures", Some("Create workspace-specific sessions"));
    let project_a = claude_home.join("projects/project-a");
    fs::create_dir_all(&project_a).unwrap();
    let sample_a = format!(
        r#"{{"type": "user", "timestamp": "2024-11-20T10:00:00Z", "cwd": "{workspace_alpha}", "message": {{"role": "user", "content": "workspace_alpha workspacetest"}}}}
{{"type": "assistant", "timestamp": "2024-11-20T10:00:05Z", "cwd": "{workspace_alpha}", "message": {{"role": "assistant", "content": "workspace_alpha_response workspacetest"}}}}"#
    );
    fs::write(project_a.join("session-alpha.jsonl"), sample_a).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(100));

    let project_b = claude_home.join("projects/project-b");
    fs::create_dir_all(&project_b).unwrap();
    let sample_b = format!(
        r#"{{"type": "user", "timestamp": "2024-11-20T11:00:00Z", "cwd": "{workspace_beta}", "message": {{"role": "user", "content": "workspace_beta workspacetest"}}}}
{{"type": "assistant", "timestamp": "2024-11-20T11:00:05Z", "cwd": "{workspace_beta}", "message": {{"role": "assistant", "content": "workspace_beta_response workspacetest"}}}}"#
    );
    fs::write(project_b.join("session-beta.jsonl"), sample_b).unwrap();
    tracker.end(
        "setup_fixtures",
        Some("Create workspace-specific sessions"),
        ps,
    );

    let ps = tracker.start("run_index", Some("Run full index"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), ps);

    let ps = tracker.start(
        "test_workspace_filter",
        Some("Search with --workspace filter"),
    );
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "workspacetest",
            "--workspace",
            workspace_alpha,
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");
    let filter_duration = ps.elapsed().as_millis() as u64;
    tracker.end(
        "test_workspace_filter",
        Some("Search with --workspace filter"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify only workspace-alpha hits"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        !hits.is_empty(),
        "Should find at least one hit with workspace filter"
    );
    for hit in hits {
        let ws = hit["workspace"].as_str().unwrap_or("");
        assert_eq!(
            ws, workspace_alpha,
            "Should only find content from workspace-alpha, got workspace: {}",
            ws
        );
    }
    tracker.end(
        "verify_results",
        Some("Verify only workspace-alpha hits"),
        ps,
    );

    tracker.metrics(
        "filter_query_workspace",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_duration)
            .with_custom("result_count", serde_json::json!(hits.len())),
    );
}

/// Test: Days filter (--days N)
#[test]
fn filter_by_days() {
    let tracker = tracker_for("filter_by_days");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let thirty_days_ago = now - (30 * 24 * 60 * 60 * 1000);

    let ps = tracker.start("setup_fixtures", Some("Create recent and old sessions"));
    make_codex_session_at(
        &codex_home,
        "2024/12/01",
        "rollout-recent.jsonl",
        "recentsession daystest",
        now,
    );
    make_codex_session_at(
        &codex_home,
        "2024/11/01",
        "rollout-old.jsonl",
        "oldsession daystest",
        thirty_days_ago,
    );
    tracker.end("setup_fixtures", Some("Create recent and old sessions"), ps);

    let ps = tracker.start("run_index", Some("Run full index"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), ps);

    let ps = tracker.start("test_days_filter", Some("Search with --days 7"));
    let output = command_env
        .cass_assert_command()
        .args(["search", "daystest", "--days", "7", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    let filter_duration = ps.elapsed().as_millis() as u64;
    tracker.end("test_days_filter", Some("Search with --days 7"), ps);

    let ps = tracker.start(
        "verify_results",
        Some("Verify only recent session returned"),
    );
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        !hits.is_empty(),
        "Should find at least one hit with days filter"
    );
    for hit in hits {
        let content = hit["content"].as_str().unwrap_or("");
        assert!(
            content.contains("recentsession"),
            "Should only find recent session with --days 7, got: {}",
            content
        );
    }
    tracker.end(
        "verify_results",
        Some("Verify only recent session returned"),
        ps,
    );

    tracker.metrics(
        "filter_query_days",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_duration)
            .with_custom("result_count", serde_json::json!(hits.len())),
    );
}

// =============================================================================
// Source filter tests (--source flag)
// =============================================================================

/// Test: search --source local filters to local sources only
#[test]
fn filter_by_source_local() {
    let tracker = tracker_for("filter_by_source_local");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    let ps = tracker.start("setup_fixtures", Some("Create local codex session"));
    make_codex_session_at(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "localsession sourcetest",
        1732118400000,
    );
    tracker.end("setup_fixtures", Some("Create local codex session"), ps);

    let ps = tracker.start("run_index", Some("Run full index"));
    command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .assert()
        .success();
    tracker.end("run_index", Some("Run full index"), ps);

    let ps = tracker.start("test_source_local", Some("Search with --source local"));
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "sourcetest",
            "--source",
            "local",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    let filter_duration = ps.elapsed().as_millis() as u64;
    tracker.end("test_source_local", Some("Search with --source local"), ps);

    let ps = tracker.start("verify_results", Some("Verify local source hits"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        !hits.is_empty(),
        "Should find local sessions with --source local"
    );

    for hit in hits {
        let source = hit
            .get("source_id")
            .and_then(|s| s.as_str())
            .unwrap_or("local");
        assert_eq!(
            source, "local",
            "All hits should be from local source, got: {}",
            source
        );
    }
    tracker.end("verify_results", Some("Verify local source hits"), ps);

    tracker.metrics(
        "filter_query_source_local",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_duration)
            .with_custom("result_count", serde_json::json!(hits.len())),
    );
}

/// Test: search --source with specific source name filters correctly
#[test]
fn filter_by_source_specific_name() {
    let tracker = tracker_for("filter_by_source_specific_name");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-1.jsonl",
            "searchdata specifictest",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start(
        "test_source_specific",
        Some("Search with --source local name"),
    );
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "specifictest",
            "--source",
            "local",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    let filter_duration = ps.elapsed().as_millis() as u64;
    tracker.end(
        "test_source_specific",
        Some("Search with --source local name"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify results found"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        !hits.is_empty(),
        "Should find sessions when filtering by specific source name 'local'"
    );
    tracker.end("verify_results", Some("Verify results found"), ps);

    tracker.metrics(
        "filter_query_source_specific",
        &E2ePerformanceMetrics::new().with_duration(filter_duration),
    );
}

/// Test: search --source with nonexistent source returns empty results
#[test]
fn filter_by_source_nonexistent() {
    let tracker = tracker_for("filter_by_source_nonexistent");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-1.jsonl",
            "somedata nonexistentsourcetest",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start(
        "test_source_nonexistent",
        Some("Search with nonexistent source"),
    );
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "nonexistentsourcetest",
            "--source",
            "nonexistent-laptop",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    tracker.end(
        "test_source_nonexistent",
        Some("Search with nonexistent source"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify empty results"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        hits.is_empty(),
        "Should find no hits when filtering by nonexistent source"
    );
    tracker.end("verify_results", Some("Verify empty results"), ps);
}

/// Test: search --source remote returns empty when no remote sources exist
#[test]
fn filter_by_source_remote_empty() {
    let tracker = tracker_for("filter_by_source_remote_empty");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create local session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-1.jsonl",
            "localonly remotefiltertest",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start("test_source_remote", Some("Search with --source remote"));
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "remotefiltertest",
            "--source",
            "remote",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    tracker.end(
        "test_source_remote",
        Some("Search with --source remote"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify no remote hits"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        hits.is_empty(),
        "Should find no remote hits when only local sessions exist"
    );
    tracker.end("verify_results", Some("Verify no remote hits"), ps);
}

/// Test: search --source all returns all sources (explicit)
#[test]
fn filter_by_source_all_explicit() {
    let tracker = tracker_for("filter_by_source_all_explicit");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-1.jsonl",
            "allsources allsourcetest",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start("test_source_all", Some("Search with --source all"));
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "allsourcetest",
            "--source",
            "all",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    tracker.end("test_source_all", Some("Search with --source all"), ps);

    let ps = tracker.start("verify_results", Some("Verify results found"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(!hits.is_empty(), "Should find sessions with --source all");
    tracker.end("verify_results", Some("Verify results found"), ps);
}

/// Test: search --source remote returns empty when no remote data indexed
#[test]
fn filter_by_source_remote_returns_empty_without_remote_indexing() {
    let tracker = tracker_for("filter_by_source_remote_returns_empty_without_remote_indexing");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create local session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-local.jsonl",
            "searchabledata remotefiltertest",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start("test_source_remote", Some("Search with --source remote"));
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "remotefiltertest",
            "--source",
            "remote",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    tracker.end(
        "test_source_remote",
        Some("Search with --source remote"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify empty remote results"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        hits.is_empty(),
        "Remote filter should return empty when no remote data indexed"
    );
    tracker.end("verify_results", Some("Verify empty remote results"), ps);
}

/// Test: search --source with specific source name returns empty for nonexistent sources
#[test]
fn filter_by_source_specific_unindexed_source() {
    let tracker = tracker_for("filter_by_source_specific_unindexed_source");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create local session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-local.jsonl",
            "searchabledata specificsourcetest",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start(
        "test_source_unindexed",
        Some("Search with unindexed source name"),
    );
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "specificsourcetest",
            "--source",
            "work-laptop",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("search command");
    tracker.end(
        "test_source_unindexed",
        Some("Search with unindexed source name"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify empty results"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        hits.is_empty(),
        "Filtering by unindexed source should return empty results"
    );
    tracker.end("verify_results", Some("Verify empty results"), ps);
}

// =============================================================================
// Timeline source filter tests
// =============================================================================

/// Test: timeline --source local shows only local sessions
#[test]
fn timeline_source_local() {
    let tracker = tracker_for("timeline_source_local");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-1.jsonl",
            "timelinelocal sessiondata",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start(
        "test_timeline_source_local",
        Some("Timeline with --source local"),
    );
    let output = command_env
        .cass_assert_command()
        .args(["timeline", "--source", "local", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("timeline command");
    tracker.end(
        "test_timeline_source_local",
        Some("Timeline with --source local"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify timeline structure"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");

    assert!(
        json.get("groups").is_some() || json.get("total_sessions").is_some(),
        "Timeline should return valid data structure"
    );
    tracker.end("verify_results", Some("Verify timeline structure"), ps);
}

/// Test: timeline --source remote with no remote data
#[test]
fn timeline_source_remote_empty() {
    let tracker = tracker_for("timeline_source_remote_empty");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-1.jsonl",
            "timelineremote sessiondata",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start(
        "test_timeline_source_remote",
        Some("Timeline with --source remote"),
    );
    let output = command_env
        .cass_assert_command()
        .args(["timeline", "--source", "remote", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("timeline command");
    tracker.end(
        "test_timeline_source_remote",
        Some("Timeline with --source remote"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify 0 remote sessions"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");

    let total = json
        .get("total_sessions")
        .and_then(|t| t.as_i64())
        .unwrap_or(0);
    assert_eq!(
        total, 0,
        "Timeline with --source remote should show 0 sessions when no remote data"
    );
    tracker.end("verify_results", Some("Verify 0 remote sessions"), ps);
}

/// Test: timeline --source specific-name
#[test]
fn timeline_source_specific() {
    let tracker = tracker_for("timeline_source_specific");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-1.jsonl",
            "timelinespecific data",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start(
        "test_timeline_source_specific",
        Some("Timeline with specific source"),
    );
    let output = command_env
        .cass_assert_command()
        .args(["timeline", "--source", "local", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("timeline command");
    tracker.end(
        "test_timeline_source_specific",
        Some("Timeline with specific source"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify valid timeline structure"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");

    assert!(
        json.get("groups").is_some() || json.get("total_sessions").is_some(),
        "Timeline with --source local should return valid structure"
    );
    tracker.end(
        "verify_results",
        Some("Verify valid timeline structure"),
        ps,
    );
}

// =============================================================================
// Stats source filter tests
// =============================================================================

/// Test: stats --source local filters stats to local
#[test]
fn stats_source_local() {
    let tracker = tracker_for("stats_source_local");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-1.jsonl",
            "statslocal data",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start("test_stats_source_local", Some("Stats with --source local"));
    let output = command_env
        .cass_assert_command()
        .args(["stats", "--source", "local", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("stats command");
    tracker.end(
        "test_stats_source_local",
        Some("Stats with --source local"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify local stats"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");

    let count = json
        .get("conversations")
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    assert!(
        count > 0,
        "Stats with --source local should show local conversations"
    );

    let filter = json
        .get("source_filter")
        .and_then(|f| f.as_str())
        .unwrap_or("");
    assert_eq!(filter, "local", "source_filter should be 'local' in output");
    tracker.end("verify_results", Some("Verify local stats"), ps);
}

/// Test: stats --source remote shows 0 when no remote data
#[test]
fn stats_source_remote_empty() {
    let tracker = tracker_for("stats_source_remote_empty");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-1.jsonl",
            "statsremote data",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start(
        "test_stats_source_remote",
        Some("Stats with --source remote"),
    );
    let output = command_env
        .cass_assert_command()
        .args(["stats", "--source", "remote", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("stats command");
    tracker.end(
        "test_stats_source_remote",
        Some("Stats with --source remote"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify 0 remote conversations"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");

    let count = json
        .get("conversations")
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    assert_eq!(
        count, 0,
        "Stats with --source remote should show 0 when no remote data"
    );
    tracker.end("verify_results", Some("Verify 0 remote conversations"), ps);
}

/// Test: stats --by-source groups by source
#[test]
fn stats_by_source_grouping() {
    let tracker = tracker_for("stats_by_source_grouping");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create session and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-1.jsonl",
            "bysource data",
            1732118400000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start("test_stats_by_source", Some("Stats with --by-source"));
    let output = command_env
        .cass_assert_command()
        .args(["stats", "--by-source", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("stats command");
    tracker.end("test_stats_by_source", Some("Stats with --by-source"), ps);

    let ps = tracker.start("verify_results", Some("Verify by_source grouping"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");

    let by_source = json.get("by_source");
    assert!(
        by_source.is_some(),
        "Stats --by-source should include 'by_source' field in JSON"
    );

    if let Some(sources) = by_source.and_then(|s| s.as_array()) {
        assert!(
            !sources.is_empty(),
            "by_source should have at least one entry"
        );
        let first_source = sources[0]
            .get("source_id")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        assert_eq!(first_source, "local", "First source should be 'local'");
    }
    tracker.end("verify_results", Some("Verify by_source grouping"), ps);
}

/// Test: stats --by-source with source filter combination
#[test]
fn stats_by_source_with_filter() {
    let tracker = tracker_for("stats_by_source_with_filter");

    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    tracker.phase("setup_and_index", "Create sessions and index", || {
        make_codex_session_at(
            &codex_home,
            "2024/11/20",
            "rollout-1.jsonl",
            "statsbyfilter data1",
            1732118400000,
        );
        make_codex_session_at(
            &codex_home,
            "2024/11/21",
            "rollout-2.jsonl",
            "statsbyfilter data2",
            1732204800000,
        );
        command_env
            .cass_assert_command()
            .args(["index", "--full", "--data-dir"])
            .arg(&data_dir)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", home)
            .assert()
            .success();
    });

    let ps = tracker.start(
        "test_stats_by_source_filtered",
        Some("Stats --by-source --source local"),
    );
    let output = command_env
        .cass_assert_command()
        .args([
            "stats",
            "--by-source",
            "--source",
            "local",
            "--json",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .env("CODEX_HOME", &codex_home)
        .output()
        .expect("stats command");
    tracker.end(
        "test_stats_by_source_filtered",
        Some("Stats --by-source --source local"),
        ps,
    );

    let ps = tracker.start("verify_results", Some("Verify filtered by_source data"));
    assert!(
        output.status.success(),
        "command failed: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");

    let by_source = json.get("by_source").and_then(|s| s.as_array());
    assert!(by_source.is_some(), "Stats should include by_source array");

    if let Some(sources) = by_source {
        let local_source = sources
            .iter()
            .find(|s| s.get("source_id").and_then(|id| id.as_str()) == Some("local"));
        assert!(local_source.is_some(), "Should have local source entry");

        if let Some(local) = local_source {
            let count = local
                .get("conversations")
                .and_then(|c| c.as_i64())
                .unwrap_or(0);
            assert!(
                count >= 2,
                "Local source should have at least 2 conversations, got {}",
                count
            );
        }
    }
    tracker.end("verify_results", Some("Verify filtered by_source data"), ps);
}
