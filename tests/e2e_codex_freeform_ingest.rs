//! Real CLI regression for freeform Codex tool input reaching searchable
//! archive content. This is separate from the connector's fault/retry tests:
//! parsing a patch successfully does not prove the persisted search journey.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

const MARKER: &str = "freeformpatchretrievalsentinel";

fn cass(home: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR")
        .timeout(Duration::from_secs(240));
    command
}

fn json_output(command: &mut Command) -> Value {
    let output = command.output().expect("run isolated cass command");
    assert!(
        output.status.success(),
        "command failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).expect("cass JSON output")
}

fn search(home: &Path, data_dir: &Path) -> Value {
    json_output(
        cass(home)
            .args([
                "search",
                MARKER,
                "--json",
                "--mode",
                "lexical",
                "--no-maintenance",
                "--no-daemon",
                "--limit",
                "10",
                "--data-dir",
            ])
            .arg(data_dir),
    )
}

#[test]
fn custom_patch_input_survives_index_search_and_incremental_replay() {
    let temp = tempfile::tempdir().expect("isolated home");
    let home = temp.path();
    let data_dir = home.join("cass-data");
    let sessions = home.join(".codex/sessions");
    std::fs::create_dir_all(&sessions).expect("session directory");
    let path = sessions.join("rollout-freeform-search.jsonl");
    let input = format!(
        "*** Begin Patch\n*** Add File: src/検索.rs\n+pub const VALUE: &str = \"{MARKER}\";\n*** End Patch\n"
    );
    let rows = [
        json!({
            "timestamp": "2026-09-17T10:00:00Z",
            "type": "session_meta",
            "payload": {"id": "freeform-ingest-regression", "cwd": home.join("project")}
        }),
        json!({
            "timestamp": "2026-09-17T10:00:01Z",
            "type": "response_item",
            "payload": {
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "Create the requested Rust source file."}]
            }
        }),
        json!({
            "timestamp": "2026-09-17T10:00:02Z",
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call", "name": "apply_patch",
                "call_id": "patch-search-1", "input": input
            }
        }),
        json!({
            "timestamp": "2026-09-17T10:00:03Z",
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call_output", "call_id": "patch-search-1",
                "output": "Success. Updated the requested file."
            }
        }),
    ];
    let source = rows
        .iter()
        .map(|row| format!("{row}\n"))
        .collect::<String>();
    std::fs::write(&path, &source).expect("write protocol-shaped rollout");
    let indexed = json_output(
        cass(home)
            .args(["index", "--full", "--json", "--data-dir"])
            .arg(&data_dir),
    );
    assert_eq!(indexed["success"], true, "{indexed}");
    let first = search(home, &data_dir);
    let first_hits = first["hits"].as_array().expect("search hits");
    assert_eq!(first_hits.len(), 1, "marker occurs only in freeform input: {first}");
    assert_eq!(first_hits[0]["agent"], "codex");
    assert_eq!(first_hits[0]["source_path"], path.to_string_lossy().as_ref());
    let first_line = first_hits[0]["line_number"].clone();
    assert!(first_line.as_u64().is_some(), "canonical message locator");
    json_output(
        cass(home)
            .args(["index", "--json", "--data-dir"])
            .arg(&data_dir),
    );
    let replay = search(home, &data_dir);
    let replay_hits = replay["hits"].as_array().expect("replay hits");
    assert_eq!(replay_hits.len(), 1, "replay must not duplicate patch records: {replay}");
    assert_eq!(replay_hits[0]["line_number"], first_line);
    assert_eq!(std::fs::read_to_string(&path).expect("read source"), source);
}
