//! Kiro CLI sessions through the real `cass` binary (bead njx4p). The parser
//! lives in franken_agent_detection (`kiro`); this proves CASS discovers the
//! default store under `~/.kiro/sessions/cli`, indexes it as agent `kiro`, and
//! keeps assistant `thinking` blocks out of the searchable text.

use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const SESSION: &str = "0f5c6a1e-7d42-4b7e-9d1f-6c2b8e4a9f10";

fn write_kiro_session(home: &Path) {
    let dir = home.join(".kiro").join("sessions").join("cli");
    fs::create_dir_all(&dir).unwrap();
    let log = [
        r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"kiroprobeprompt wire the connector"}],"meta":{"timestamp":1785939877}}}"#,
        r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m2","content":[{"kind":"thinking","data":{"text":"kiroprobethinking private reasoning","signature":"sig"}},{"kind":"text","data":"kiroprobereply here is the plan"}]}}"#,
        r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m3","content":[{"kind":"toolUse","data":{"toolUseId":"tool-1","name":"execute_bash","input":{"command":"cargo test"}}}]}}"#,
        r#"{"version":"v1","kind":"ToolResults","data":{"content":[{"kind":"toolResult","data":{"toolUseId":"tool-1","content":[{"kind":"text","data":"test result: ok. 39 passed"}]}}]}}"#,
    ];
    fs::write(dir.join(format!("{SESSION}.jsonl")), log.join("\n") + "\n").unwrap();
    fs::write(
        dir.join(format!("{SESSION}.json")),
        format!(
            r#"{{"session_id":"{SESSION}","cwd":"/work/kiro-demo","title":"Kiro probe session"}}"#
        ),
    )
    .unwrap();
}

fn cass(home: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command.env_clear();
    // Keep only OS runtime essentials, never the operator's history roots.
    for key in ["PATH", "SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("APPDATA", home.join("AppData/Roaming"))
        .env("LOCALAPPDATA", home.join("AppData/Local"))
        .env("CASS_DATA_DIR", home.join("data"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("RUST_MIN_STACK", "134217728")
        .env("NO_COLOR", "1")
        .current_dir(home)
        .args(["--color", "never"]);
    command
}

fn search(home: &Path, query: &str) -> Value {
    let mut command = cass(home);
    command.args([
        "search",
        query,
        "--agent",
        "kiro",
        "--mode",
        "lexical",
        "--json",
        "--no-maintenance",
    ]);
    let output = assert_cmd::Command::from_std(command)
        .timeout(Duration::from_secs(60))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap_or(Value::Null)
}

#[test]
fn kiro_cli_session_is_indexed_searchable_and_keeps_thinking_out() {
    let home = tempfile::tempdir().unwrap();
    write_kiro_session(home.path());

    let mut index = cass(home.path());
    index.args(["index", "--full", "--json"]);
    let output = assert_cmd::Command::from_std(index)
        .timeout(Duration::from_secs(180))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let summary: Value = serde_json::from_slice(&output).unwrap_or(Value::Null);
    assert_eq!(summary["success"], true, "{summary}");
    let kiro = summary["indexing_stats"]["connectors"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|connector| connector["name"] == "kiro")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(kiro["conversations"], 1, "{summary}");

    for phrase in ["kiroprobeprompt", "kiroprobereply"] {
        let result = search(home.path(), phrase);
        let hits = result["hits"].as_array().cloned().unwrap_or_default();
        assert_eq!(hits.len(), 1, "{phrase}: {result}");
        assert_eq!(hits[0]["agent"], "kiro", "{phrase}: {result}");
        let source = hits[0]["source_path"].as_str().unwrap_or_default();
        assert!(source.ends_with(&format!("{SESSION}.jsonl")), "{result}");
    }

    // Assistant reasoning is excluded from the indexed text.
    let thinking = search(home.path(), "kiroprobethinking");
    assert_eq!(
        thinking["hits"].as_array().map(Vec::len),
        Some(0),
        "{thinking}"
    );
}
