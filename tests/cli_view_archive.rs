//! `cass view` archive-only resolution regression suite.
//!
//! Bead: coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.2.3
//! ("Make cass view resolve archive-only rows when source_path is stale").
//!
//! When a session's source file has moved, vanished, or belongs to a
//! remote/mapped workspace, `cass view` must still return the conversation
//! content from the canonical DB/archive row — file reads are a fast path, not
//! the only path. The robot JSON then states `source_exists=false` and
//! `archive_only=true`, and a genuinely-missing archive row is reported with a
//! distinct error from a present-but-unindexed file.
//!
//! These tests build an archive DB row whose `source_path` does not exist on
//! disk (covering moved-Linux, macOS, remote-source_id, and vanished-file
//! scenarios), then drive the real `cass` binary and assert the JSON contract.

use assert_cmd::Command;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

mod util;
use util::cass_bin;

const ARCHIVED_BODY: &str = "archived-only conversation body line";

/// Insert one conversation whose `source_path` is `source_path` (which the
/// caller leaves absent on disk to simulate a stale/moved/vanished source).
fn seed_archive_row(db_path: &Path, source_path: &str, source_id: &str, origin_host: Option<&str>) {
    let storage = SqliteStorage::open(db_path).expect("open archive db");
    let agent_id = storage
        .ensure_agent(&Agent {
            id: None,
            slug: "claude_code".to_string(),
            name: "Claude Code".to_string(),
            version: None,
            kind: AgentKind::Cli,
        })
        .expect("ensure agent");
    let conversation = Conversation {
        id: None,
        agent_slug: "claude_code".to_string(),
        workspace: Some(PathBuf::from("/tmp/ws")),
        external_id: Some(format!("archive-only-{source_id}")),
        title: Some("Archived Session".to_string()),
        source_path: PathBuf::from(source_path),
        started_at: Some(1_733_000_000_000),
        ended_at: Some(1_733_000_010_000),
        approx_tokens: None,
        metadata_json: serde_json::json!({}),
        messages: vec![Message {
            id: None,
            idx: 0,
            role: MessageRole::User,
            author: Some("me".to_string()),
            created_at: Some(1_733_000_000_000),
            content: ARCHIVED_BODY.to_string(),
            extra_json: serde_json::json!({}),
            snippets: Vec::new(),
        }],
        source_id: source_id.to_string(),
        origin_host: origin_host.map(ToOwned::to_owned),
    };
    storage
        .insert_conversation_tree(agent_id, None, &conversation)
        .expect("insert conversation tree");
}

fn run_view_json(
    db_path: &Path,
    source_path: &str,
    source_id: &str,
) -> (Value, std::process::Output) {
    let output = Command::new(cass_bin())
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .args([
            "view",
            source_path,
            "--source",
            source_id,
            "--json",
            "--db",
            db_path.to_str().unwrap(),
        ])
        .output()
        .expect("run cass view");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value = serde_json::from_str::<Value>(stdout.trim())
        .unwrap_or_else(|e| panic!("view stdout not valid JSON ({e}); stdout:\n{stdout}"));
    (value, output)
}

fn assert_archive_only(label: &str, json: &Value) {
    assert_eq!(
        json["source_exists"], false,
        "{label}: source_exists should be false: {json}"
    );
    assert_eq!(
        json["archive_only"], true,
        "{label}: archive_only should be true: {json}"
    );
    let content: String = json["lines"]
        .as_array()
        .expect("lines array")
        .iter()
        .filter_map(|l| l["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        content.contains(ARCHIVED_BODY),
        "{label}: archived content should be returned from the DB row: {json}"
    );
}

#[test]
fn view_resolves_vanished_linux_source_from_archive() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("agent_search.db");
    // A /home path that does not exist on disk (vanished/moved Linux source).
    let stale = "/home/operator/.claude/projects/gone/session-vanished.jsonl";
    seed_archive_row(&db, stale, "local", None);
    let (json, _) = run_view_json(&db, stale, "local");
    assert_archive_only("vanished-linux", &json);
}

#[test]
fn view_resolves_macos_source_path_from_archive() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("agent_search.db");
    // A macOS /Users path that cannot exist on the Linux test host.
    let macos = "/Users/alice/Library/Application Support/cass/sessions/s.jsonl";
    seed_archive_row(&db, macos, "local", None);
    let (json, _) = run_view_json(&db, macos, "local");
    assert_archive_only("macos-path", &json);
}

#[test]
fn view_resolves_remote_source_id_from_archive() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("agent_search.db");
    let remote = "/remote/work-laptop/.codex/sessions/s.jsonl";
    seed_archive_row(&db, remote, "work-laptop", Some("work-laptop"));
    let (json, _) = run_view_json(&db, remote, "work-laptop");
    assert_archive_only("remote-source-id", &json);
}

#[test]
fn view_marks_present_file_as_source_exists() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("agent_search.db");
    // A real file on disk: view should read it directly and NOT mark archive_only.
    let present = tmp.path().join("present.jsonl");
    std::fs::write(
        &present,
        "{\"role\":\"user\",\"content\":\"live file line\"}\n",
    )
    .expect("write present file");
    let present_str = present.to_string_lossy().to_string();
    seed_archive_row(&db, &present_str, "local", None);
    let (json, _) = run_view_json(&db, &present_str, "local");
    assert_eq!(
        json["source_exists"], true,
        "present file => source_exists=true: {json}"
    );
    assert_eq!(
        json["archive_only"], false,
        "present file => archive_only=false: {json}"
    );
}

#[test]
fn view_distinguishes_missing_archive_row_from_missing_file() {
    let tmp = TempDir::new().expect("tempdir");
    let db = tmp.path().join("agent_search.db");
    // Empty DB (no archive rows) + a nonexistent path with an explicit source.
    SqliteStorage::open(&db).expect("create empty db");
    let missing = "/home/operator/.claude/projects/none/missing.jsonl";
    let output = Command::new(cass_bin())
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .args([
            "view",
            missing,
            "--source",
            "local",
            "--json",
            "--db",
            db.to_str().unwrap(),
        ])
        .output()
        .expect("run cass view");
    // Non-zero exit; the error names the missing archive row distinctly.
    assert!(
        !output.status.success(),
        "missing archive row + missing file should error"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let lc = combined.to_lowercase();
    assert!(
        lc.contains("archive row missing") || lc.contains("no archived row"),
        "error should distinguish a missing archive row: {combined}"
    );
}
