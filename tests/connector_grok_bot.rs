//! Grok Bot desktop chat cache through the real `cass` binary (GH #447, bead
//! 2l1b0.13). The fixture is the reporter's sanitized Grok Bot 0.44.0 replica
//! (13 of 200 entries, every entry kind; GH #447), byte-identical to
//! franken_agent_detection's `tests/fixtures/grok_bot_reporter_0440.json`.
//! The parser lives in FAD (`grok_bot`); this proves CASS finds the store at
//! the default macOS location with no override, keeps exactly the seven chat
//! entries in order, and never reads a replica outside that directory.

use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const REPORTER_SAMPLE: &str = include_str!("fixtures/grok_bot/reporter_0440.json");
/// Lowercase unpadded base32 of the reporter's replica key
/// `sand.client.slice.account.auth0%7Cuser_00000000000000000000000000.transcript.replicas.81da155f-cc4c-58b3-aadf-770858d5e55c`.
const BLOB: &str = "onqw4zbomnwgszlooqxhg3djmnss4yldmnxxk3tufzqxk5digastoq3vonsxexzqgaydambqgaydambqgaydambqgaydambqgaydambqfz2heyloonrxe2lqoqxhezlqnruwgyltfy4dczdbge2tkzrnmnrtiyzngu4gemznmfqwizrng43taobvhbsdkzjvgvrq.blob";

fn grok_bot_config(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("Grok Bot")
}

fn cass(home: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command.env_clear();
    // Keep only OS runtime essentials; no CASS_GROK_BOT_DATA_ROOT override.
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

fn run_json(mut command: Command, timeout_secs: u64) -> Value {
    let output = assert_cmd::Command::from_std({
        command.arg("--json");
        command
    })
    .timeout(Duration::from_secs(timeout_secs))
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    serde_json::from_slice(&output).unwrap_or(Value::Null)
}

/// Full index of `home`; returns the grok_bot connector's conversation count.
fn index(home: &Path) -> u64 {
    let mut command = cass(home);
    command.args(["index", "--full"]);
    let summary = run_json(command, 180);
    assert_eq!(summary["success"], true, "{summary}");
    summary["indexing_stats"]["connectors"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|connector| connector["name"] == "grok_bot")
        .and_then(|connector| connector["conversations"].as_u64())
        .unwrap_or(0)
}

fn search(home: &Path, query: &str) -> Vec<Value> {
    let mut command = cass(home);
    command.args([
        "search",
        query,
        "--agent",
        "grok_bot",
        "--mode",
        "lexical",
        "--no-maintenance",
    ]);
    let result = run_json(command, 60);
    result["hits"].as_array().cloned().unwrap_or_default()
}

#[test]
fn grok_bot_default_store_indexes_the_reporter_replica_chat_only() {
    let home = tempfile::tempdir().unwrap();
    let persistence = grok_bot_config(home.path()).join("sand-client-persistence");
    fs::create_dir_all(&persistence).unwrap();
    let blob = persistence.join(BLOB);
    fs::write(&blob, REPORTER_SAMPLE).unwrap();

    assert_eq!(index(home.path()), 1);
    assert_eq!(fs::read_to_string(&blob).unwrap(), REPORTER_SAMPLE);

    // The canonical rows are the seven chat entries, in source order, with the
    // app's own entry ids, timestamps and text; the six non-chat entries
    // (secret request, event, attachment, approval, widget, connector) are not.
    let sample: Value = serde_json::from_str(REPORTER_SAMPLE).unwrap();
    let entries = sample["value"]["entries"].as_array().unwrap();
    let storage = FrankenStorage::open_readonly(&home.path().join("data/agent_search.db")).unwrap();
    let conversations = storage.list_conversations(10, 0).unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].agent_slug, "grok_bot");
    assert!(
        conversations[0]
            .source_path
            .ends_with(Path::new("Grok Bot/sand-client-persistence").join(BLOB)),
        "{:?}",
        conversations[0].source_path
    );
    assert!(conversations[0].workspace.is_none());
    let messages = storage
        .fetch_messages(conversations[0].id.unwrap())
        .unwrap();
    let chat = [1, 2, 3, 4, 5, 7, 12];
    assert_eq!(messages.len(), chat.len());
    for (idx, (message, source)) in messages.iter().zip(chat).enumerate() {
        let entry = &entries[source];
        let text = if entry["kind"] == "message" {
            &entry["content"]
        } else {
            &entry["message"]["content"]
        };
        assert_eq!(message.idx, i64::try_from(idx).unwrap());
        assert_eq!(message.content, text.as_str().unwrap());
        assert_eq!(message.created_at, entry["timestampMs"].as_i64());
        assert_eq!(message.extra_json["grok_bot_entry_id"], entry["id"]);
        let user = entry["kind"] == "message" && entry["role"] == "user";
        assert_eq!(
            message.role.to_string(),
            if user { "User" } else { "Agent" }
        );
    }
    drop(storage);

    // "consec" occurs only in the first user message (the sanitizer truncated it).
    let hits = search(home.path(), "consec");
    assert!(!hits.is_empty());
    for hit in &hits {
        assert_eq!(hit["agent"], "grok_bot", "{hit}");
        let source = hit["source_path"].as_str().unwrap_or_default();
        assert!(source.ends_with(BLOB), "{hit}");
    }
    // Tokens that occur only inside the secret-request payload.
    for token in ["f62cc8", "c0989e"] {
        assert!(search(home.path(), token).is_empty(), "{token} leaked");
    }
}

#[test]
fn grok_bot_replica_outside_the_persistence_directory_is_not_indexed() {
    let home = tempfile::tempdir().unwrap();
    let config = grok_bot_config(home.path());
    fs::create_dir_all(config.join("Session Storage")).unwrap();
    // A valid replica beside, not inside, sand-client-persistence.
    fs::write(config.join(BLOB), REPORTER_SAMPLE).unwrap();
    fs::write(config.join("Session Storage").join(BLOB), REPORTER_SAMPLE).unwrap();

    assert_eq!(index(home.path()), 0);
    let storage = FrankenStorage::open_readonly(&home.path().join("data/agent_search.db")).unwrap();
    assert!(storage.list_conversations(10, 0).unwrap().is_empty());
}
