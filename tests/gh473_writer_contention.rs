//! Real-binary GH473 regression: another process keeps a pinned archive reader
//! alive across changed and unchanged source ingestion. No substitute SQL engine.
use assert_cmd::Command;
use coding_agent_search::franken_sync::compat::RowExt;
use coding_agent_search::search::tantivy::{
    expected_index_dir, searchable_index_fingerprint, searchable_index_summary,
};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

fn command(home: &Path, db: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command.env_clear();
    for key in ["PATH", "SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_MIN_STACK", "134217728")
        .arg("--db")
        .arg(db)
        // This is a coarse regression bound below the old 60-second wait,
        // not a microbenchmark or a two-second busy-host timing assumption.
        .timeout(Duration::from_secs(45));
    command
}

fn message_content(ordinal: usize) -> String {
    format!(
        "gh473canonicaltoken persistent archive evidence for writer and reader coexistence message {ordinal}"
    )
}

fn append_message(path: &Path, home: &Path, ordinal: usize) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(
        file,
        "{}",
        json!({
            "type": "user", "sessionId": "gh473-live-reader",
            "uuid": format!("gh473-live-reader-{ordinal}"),
            "timestamp": "2025-11-12T18:31:18.697Z", "cwd": home.to_string_lossy(),
            "message": {"role": "user", "content": message_content(ordinal)}
        })
    )
    .unwrap();
}

fn messages(storage: &FrankenStorage) -> Vec<(i64, String)> {
    storage
        .raw()
        .query("SELECT id, content FROM messages ORDER BY id")
        .unwrap()
        .iter()
        .map(|row| (row.get_typed(0).unwrap(), row.get_typed(1).unwrap()))
        .collect()
}

fn index(home: &Path, data: &Path, db: &Path) {
    let output = command(home, db)
        .args([
            "index",
            "--full",
            "--json",
            "--no-progress-events",
            "--data-dir",
        ])
        .arg(data)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index failed: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["success"], true, "{payload}");
}

#[test]
fn gh473_real_cli_write_and_noop_replays_preserve_pinned_reader_and_lexical_state() {
    let temp = tempfile::TempDir::new().unwrap();
    let home = temp.path();
    fs::write(home.join(".env"), "").unwrap();
    let data = home.join("data");
    let db = data.join("agent_search.db");
    let project = home.join(".claude/projects/-gh473-live-reader");
    fs::create_dir_all(&project).unwrap();
    let source = project.join("gh473-live-reader.jsonl");
    append_message(&source, home, 0);
    index(home, &data, &db);

    let reader = FrankenStorage::open_readonly(&db).unwrap();
    reader.raw().execute("BEGIN").unwrap();
    let pinned_messages = messages(&reader);
    assert_eq!(pinned_messages.len(), 1);
    assert_eq!(pinned_messages[0].1, message_content(0));
    let initial_ledger = reader.source_ingest_ledger_entries().unwrap();
    assert_eq!(initial_ledger.len(), 1);
    let (key, mut observation) = initial_ledger.into_iter().next().unwrap();
    let index_path = expected_index_dir(&data);
    let mut expected_messages = None;

    for pass in 0..3 {
        if pass == 0 {
            append_message(&source, home, 1);
        } else {
            // Change the observation without changing parsed messages, forcing
            // source reprocessing instead of the unchanged-file ledger shortcut.
            writeln!(OpenOptions::new().append(true).open(&source).unwrap()).unwrap();
        }
        index(home, &data, &db);
        assert_eq!(
            messages(&reader),
            pinned_messages,
            "reader snapshot changed on pass {pass}"
        );
        let current = FrankenStorage::open_readonly(&db).unwrap();
        let rows = messages(&current);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], pinned_messages[0], "existing identity changed");
        assert_eq!(rows[1].1, message_content(1));
        let conversation_count = current
            .raw()
            .query_row("SELECT COUNT(*) FROM conversations")
            .unwrap()
            .get_typed::<i64>(0)
            .unwrap();
        assert_eq!(conversation_count, 1);
        if let Some(expected) = &expected_messages {
            assert_eq!(
                &rows, expected,
                "no-op replay changed canonical identities or content"
            );
        } else {
            expected_messages = Some(rows);
        }
        let ledger = current.source_ingest_ledger_entries().unwrap();
        assert_eq!(ledger.len(), 1);
        let actual = ledger.get(&key).expect("same source identity");
        assert_ne!(
            actual, &observation,
            "changed source was not acknowledged on pass {pass}"
        );
        observation.clone_from(actual);

        // Inspect the publication before any search could heal a stale index.
        let checkpoint: Value = serde_json::from_slice(
            &fs::read(index_path.join(".lexical-rebuild-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(checkpoint["completed"], true, "{checkpoint}");
        assert_eq!(checkpoint["db"]["total_conversations"], 1);
        assert_eq!(checkpoint["db"]["total_messages"], 2);
        assert_eq!(checkpoint["indexed_docs"], 2);
        let fingerprint = searchable_index_fingerprint(&index_path).unwrap().unwrap();
        assert_eq!(
            checkpoint["committed_meta_fingerprint"].as_str(),
            Some(fingerprint.as_str())
        );
        assert_eq!(
            searchable_index_summary(&index_path).unwrap().unwrap().docs,
            2
        );
        current.close_without_checkpoint().unwrap();
    }
    reader.raw().execute("ROLLBACK").unwrap();
    reader.close_without_checkpoint().unwrap();
    let reopened = FrankenStorage::open_readonly(&db).unwrap();
    assert_eq!(messages(&reopened), expected_messages.unwrap());
    assert_eq!(
        reopened.source_ingest_ledger_entries().unwrap().get(&key),
        Some(&observation)
    );
    reopened.close_without_checkpoint().unwrap();
}
