//! FAD #28 (bead coding_agent_session_search-8dv4h): conversations in Pi's
//! experimental durable harness stores (`@earendil-works/pi-durable`), read
//! through the real `cass` binary. The store is built with the upstream
//! version-1 SQLite schema at the host's default location; the records are
//! generated fixtures, not evidence about a user's private store.

use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::ConnectionExt;
use coding_agent_search::franken_sync::params;
use coding_agent_search::raw_mirror::{RawMirrorCaptureInput, capture_source_file};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

/// The upstream schema (`storage/sqlite/migrations.ts`, version 1).
const SCHEMA: &[&str] = &[
    "CREATE TABLE durable_schema (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), version INTEGER NOT NULL CHECK (version >= 0)) STRICT",
    "CREATE TABLE durable_metadata (singleton INTEGER PRIMARY KEY CHECK (singleton = 1), next_id TEXT NOT NULL, next_seq INTEGER NOT NULL) STRICT",
    "CREATE TABLE record_ids (id INTEGER PRIMARY KEY, record_type TEXT NOT NULL) STRICT",
    "CREATE TABLE conversations (id INTEGER PRIMARY KEY, owner_conversation_id INTEGER, owner_task_id INTEGER, record TEXT NOT NULL CHECK (json_valid(record))) STRICT",
    "CREATE TABLE entries (id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL, head INTEGER, commit_seq INTEGER NOT NULL, record TEXT NOT NULL CHECK (json_valid(record))) STRICT",
    "CREATE INDEX entries_by_conversation ON entries (conversation_id, id DESC)",
    "CREATE TABLE tasks (id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL, kind TEXT NOT NULL, status TEXT NOT NULL, abort_requested INTEGER NOT NULL, background INTEGER NOT NULL, record TEXT NOT NULL) STRICT",
    "CREATE TABLE submissions (id INTEGER PRIMARY KEY, conversation_id INTEGER NOT NULL, request_id TEXT, status TEXT NOT NULL, record TEXT NOT NULL) STRICT",
    "CREATE TABLE documents (id INTEGER PRIMARY KEY, kind TEXT NOT NULL, family INTEGER NOT NULL, key_value TEXT NOT NULL, scope_kind TEXT NOT NULL, owner_id INTEGER NOT NULL, created_at INTEGER NOT NULL, retired_at INTEGER, record TEXT NOT NULL CHECK (json_valid(record))) STRICT",
    "CREATE TABLE document_revisions (document_id INTEGER NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('base', 'delta')), version INTEGER NOT NULL, content TEXT NOT NULL CHECK (json_valid(content)), PRIMARY KEY (document_id, seq)) STRICT",
];

const STORE_TIME: i64 = 1_767_600_000_000;

/// Words cass must never index or copy: a running task's private checkpoint
/// and the agent instructions held in the conversation's pi.agent document.
const PRIVATE_MARKERS: [&str; 2] = ["pidurabletaskcheckpoint", "piduraleinstructions"];

/// `<home>/.pi/agent/experimental/durable-sessions/<cwd-hash>/<ms>-<uuid>/session.sqlite`.
fn default_store_path(home: &Path) -> PathBuf {
    home.join(".pi/agent/experimental/durable-sessions")
        .join("0123456789abcdef01234567")
        .join("1767600000000-0123abcd-4567-89ef-0123-456789abcdef")
        .join("session.sqlite")
}

fn user(text: &str, ts: i64) -> Value {
    json!({"role": "user", "content": text, "timestamp": ts})
}

fn assistant(text: &str, ts: i64) -> Value {
    json!({"role": "assistant", "content": [{"type": "text", "text": text}],
           "api": "anthropic-messages", "provider": "anthropic", "model": "claude-x",
           "usage": {"input": 3, "output": 2, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 5},
           "stopReason": "stop", "timestamp": ts})
}

/// One store holding a lead conversation (1) and a task-owned child (5).
fn write_store(path: &Path, schema_version: i64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(path.to_string_lossy().as_ref()).unwrap();
    for statement in SCHEMA {
        conn.execute(statement).unwrap();
    }
    conn.execute_compat(
        "INSERT INTO durable_schema (singleton, version) VALUES (1, ?1)",
        params![schema_version],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO tasks VALUES (7, 1, 'pi.generation', 'running', 0, 0, ?1)",
        params![
            json!({"id": 7, "state": {"status": "running", "checkpoint": PRIVATE_MARKERS[0]}})
                .to_string()
        ],
    )
    .unwrap();
    for (id, record) in [
        (1_i64, json!({"id": 1})),
        (
            5,
            json!({"id": 5, "owner": {"conversationId": 1, "taskId": 7}}),
        ),
    ] {
        conn.execute_compat(
            "INSERT INTO conversations (id, owner_conversation_id, owner_task_id, record) VALUES (?1, NULL, NULL, ?2)",
            params![id, record.to_string()],
        )
        .unwrap();
    }
    let agent = json!({"id": 2, "kind": "pi.agent",
        "scope": {"kind": "conversation", "conversationId": 1},
        "history": "rewindable", "fork": "asOf"});
    conn.execute_compat(
        "INSERT INTO documents (id, kind, family, key_value, scope_kind, owner_id, created_at, retired_at, record) \
         VALUES (2, '\"pi.agent\"', 0, '', 'conversation', 1, 1, NULL, ?1)",
        params![agent.to_string()],
    )
    .unwrap();
    conn.execute_compat(
        "INSERT INTO document_revisions (document_id, seq, kind, version, content) VALUES (2, 1, 'base', 1, ?1)",
        params![json!({"cwd": "/work/durable-repo", "instructions": PRIVATE_MARKERS[1]}).to_string()],
    )
    .unwrap();
    let entries = [
        (
            10_i64,
            1_i64,
            "pi.user",
            user("piduraleleadquestion", STORE_TIME),
        ),
        (
            11,
            1,
            "pi.assistant",
            assistant("piduraleleadanswer", STORE_TIME + 1),
        ),
        (24, 5, "pi.user", user("piduralechildtask", STORE_TIME + 2)),
        (
            26,
            5,
            "pi.assistant",
            assistant("piduralechildanswer", STORE_TIME + 3),
        ),
    ];
    for (seq, (id, conversation, kind, message)) in (2_i64..).zip(entries) {
        let record =
            json!({"id": id, "conversationId": conversation, "kind": kind, "model": [message]});
        conn.execute_compat(
            "INSERT INTO entries (id, conversation_id, head, commit_seq, record) VALUES (?1, ?2, NULL, ?3, ?4)",
            params![id, conversation, seq, record.to_string()],
        )
        .unwrap();
    }
}

fn cass(home: &Path, data: &Path) -> assert_cmd::Command {
    let mut command = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("CASS_DATA_DIR", data)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_MIN_STACK", "134217728")
        .current_dir(home)
        .timeout(std::time::Duration::from_secs(180));
    if let Ok(system_root) = dotenvy::var("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
}

/// `pi_durable` hits for `needle`, as `(conversation_id, content)`.
fn hits(home: &Path, data: &Path, needle: &str) -> Vec<(Value, String)> {
    let output = cass(home, data)
        .args([
            "search",
            needle,
            "--agent",
            "pi_durable",
            "--mode",
            "lexical",
            "--json",
            "--no-maintenance",
            "--timeout",
            "10000",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output).unwrap();
    result["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hit| {
            (
                hit["conversation_id"].clone(),
                hit["content"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn pi_durable_conversations(home: &Path, data: &Path) -> Value {
    let output = cass(home, data)
        .args(["stats", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stats: Value = serde_json::from_slice(&output).unwrap();
    stats["by_agent"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["agent"] == "pi_durable")
        .map_or(Value::Null, |row| row["count"].clone())
}

fn files_holding(root: &Path, needle: &str) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            fs::read(entry.path())
                .map(|bytes| {
                    bytes
                        .windows(needle.len())
                        .any(|window| window == needle.as_bytes())
                })
                .unwrap_or(false)
        })
        .map(|entry| entry.into_path())
        .collect()
}

#[test]
fn a_durable_store_indexes_its_lead_and_child_conversations() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let store = default_store_path(&home);
    write_store(&store, 1);
    let before = fs::read(&store).unwrap();

    cass(&home, &data)
        .args(["index", "--full", "--json", "--no-progress-events"])
        .assert()
        .success();
    assert_eq!(pi_durable_conversations(&home, &data), 2);

    let lead = hits(&home, &data, "piduraleleadanswer");
    let child = hits(&home, &data, "piduralechildanswer");
    assert!(
        lead.iter()
            .any(|(_, content)| content.contains("piduraleleadanswer")),
        "{lead:?}"
    );
    assert!(
        child
            .iter()
            .any(|(_, content)| content.contains("piduralechildanswer")),
        "{child:?}"
    );
    let lead_id = &lead
        .iter()
        .find(|(_, c)| c.contains("piduraleleadanswer"))
        .unwrap()
        .0;
    let child_id = &child
        .iter()
        .find(|(_, c)| c.contains("piduralechildanswer"))
        .unwrap()
        .0;
    assert!(lead_id.is_i64() && child_id.is_i64(), "{lead:?} {child:?}");
    assert_ne!(
        lead_id, child_id,
        "the task-owned child is its own conversation"
    );

    // Never indexed, never copied: the private task checkpoint and the
    // agent instructions stay only in the source store.
    for marker in PRIVATE_MARKERS {
        assert!(
            hits(&home, &data, marker).is_empty(),
            "{marker} was indexed"
        );
        assert_eq!(
            files_holding(&data, marker),
            Vec::<PathBuf>::new(),
            "{marker}"
        );
    }
    assert_eq!(
        fs::read(&store).unwrap(),
        before,
        "the reader mutated the store"
    );
}

#[test]
fn a_durable_store_is_not_raw_mirrored() {
    let tmp = tempfile::tempdir().unwrap();
    let store = default_store_path(tmp.path());
    write_store(&store, 1);
    let data = tmp.path().join("capture-data");
    for suffix in ["", "-wal", "-shm"] {
        let source = PathBuf::from(format!("{}{suffix}", store.display()));
        let error = capture_source_file(RawMirrorCaptureInput {
            data_dir: &data,
            provider: "pi_durable",
            source_id: "fixture",
            origin_kind: "local",
            origin_host: None,
            source_path: &source,
            db_links: &[],
        })
        .unwrap_err();
        assert!(
            error.to_string().contains("disabled_sensitive_container"),
            "{error}"
        );
    }
    assert!(!data.exists(), "denial must precede mirror initialization");
}

#[test]
fn a_durable_store_with_an_unsupported_schema_version_is_not_indexed() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    write_store(&default_store_path(&home), 2);
    cass(&home, &data)
        .args(["index", "--full", "--json", "--no-progress-events"])
        .assert()
        .success();
    assert_eq!(pi_durable_conversations(&home, &data), Value::Null);
    assert!(hits(&home, &data, "piduraleleadanswer").is_empty());
}
