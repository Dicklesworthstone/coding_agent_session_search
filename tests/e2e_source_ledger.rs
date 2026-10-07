//! GH512: persisted source reuse through the real CLI, canonical store and search.

use coding_agent_search::franken_sync::SqliteValue;
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn cass(home: &Path, streaming: &str) -> assert_cmd::Command {
    let mut command = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("CASS_DATA_DIR", home.join("data"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_STREAMING_INDEX", streaming)
        .env("CASS_AUTO_REFRESH", "0")
        .env("RUST_MIN_STACK", "134217728")
        .env("NO_COLOR", "1")
        .current_dir(home)
        .timeout(Duration::from_secs(240))
        .args(["--color", "never"]);
    command
}

fn row(provider: &str, source: usize, message: usize) -> Value {
    let content = format!("ghledger{source:03}message{message:03}z");
    if provider == "claude_code" {
        json!({"type":"user", "sessionId":format!("ledger-{source}"),
            "uuid":format!("ledger-{source}-{message}"),
            "timestamp":"2026-08-01T10:00:01Z", "cwd":"/work/ledger",
            "message":{"role":"user", "content":content}})
    } else {
        json!({"timestamp":"2026-08-01T10:00:01Z", "type":"response_item",
            "payload":{"type":"message", "role":"user",
                "content":[{"type":"input_text", "text":content}]}})
    }
}

fn transcript(directory: &Path, provider: &str, source: usize) -> PathBuf {
    let name = if provider == "claude_code" {
        "session"
    } else {
        "rollout"
    };
    let path = directory.join(format!("{name}-{source}.jsonl"));
    let mut content = String::new();
    if provider == "codex" {
        content.push_str(&format!(
            "{}\n",
            json!({"timestamp":"2026-08-01T10:00:00Z",
            "type":"session_meta", "payload":{"id":format!("ledger-{source}"),
            "cwd":"/work/ledger"}})
        ));
    }
    content.push_str(&format!("{}\n", row(provider, source, 0)));
    fs::write(&path, content).unwrap();
    path
}

fn index(home: &Path, streaming: &str, step: &str, skipped: usize, parsed: usize) {
    index_with_exclusion(home, streaming, step, skipped, parsed, None);
}

fn index_with_exclusion(
    home: &Path,
    streaming: &str,
    step: &str,
    skipped: usize,
    parsed: usize,
    excluded: Option<&Path>,
) {
    let trace = home.join(format!("{step}.trace.jsonl"));
    let mut command = cass(home, streaming);
    if let Some(excluded) = excluded {
        command.env("CASS_EXCLUDE_PATHS", excluded);
    }
    let output = command
        .env(
            "CASS_TRACE_FILTER",
            "warn,coding_agent_search::indexer=debug",
        )
        .arg("--trace-file")
        .arg(&trace)
        .args(["index", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let summary: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(summary["success"], true, "{summary}");
    let log = fs::read_to_string(trace).unwrap();
    let observations: Vec<_> = log
        .lines()
        .filter_map(|line| {
            let event: Value = serde_json::from_str(line).unwrap();
            assert_ne!(event["event"], "trace_truncated", "{log}");
            (event["fields"]["message"] == "source_ingest_observation").then(|| {
                event["fields"]["skipped"]
                    .as_bool()
                    .expect("boolean reuse decision")
            })
        })
        .collect();
    assert_eq!(
        observations.iter().filter(|&&value| value).count(),
        skipped,
        "{log}"
    );
    assert_eq!(
        observations.iter().filter(|&&value| !value).count(),
        parsed,
        "{log}"
    );
}

/// Canonical IDs as well as content must survive skipped and replayed sources.
fn archive(home: &Path, sources: usize, appended: bool) -> BTreeMap<String, (i64, i64)> {
    let contents = archive_contents(home, sources);
    assert_eq!(contents.len(), sources + usize::from(appended));
    for source in 0..sources {
        assert!(contents.contains_key(&format!("ghledger{source:03}message000z")));
    }
    if appended {
        assert!(contents.contains_key("ghledger000message001z"));
    }
    contents
}

fn archive_contents(home: &Path, sources: usize) -> BTreeMap<String, (i64, i64)> {
    let storage = SqliteStorage::open_readonly(&home.join("data/agent_search.db")).unwrap();
    let conversations = storage.list_conversations(100, 0).unwrap();
    assert_eq!(conversations.len(), sources);
    let mut contents = BTreeMap::new();
    for conversation in conversations {
        let id = conversation.id.unwrap();
        for message in storage.fetch_messages(id).unwrap() {
            assert!(
                contents
                    .insert(message.content, (id, message.id.unwrap()))
                    .is_none()
            );
        }
    }
    contents
}

fn source_ledger(home: &Path) -> std::collections::HashMap<String, String> {
    let storage = SqliteStorage::open_readonly(&home.join("data/agent_search.db")).unwrap();
    storage.source_ingest_ledger_entries().unwrap()
}

fn assert_searchable(home: &Path, marker: &str) {
    let output = cass(home, "1")
        .args([
            "search",
            marker,
            "--mode",
            "lexical",
            "--json",
            "--no-maintenance",
            "--no-daemon",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output).unwrap();
    let hits = result["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{result}");
    assert_eq!(hits[0]["content"], marker, "{result}");
}

fn directory_observation(path: &Path) -> Value {
    let metadata = fs::metadata(path).unwrap();
    let value = json!({"path":path, "size":metadata.len(),
        "mtime_ns":metadata.modified().unwrap().duration_since(std::time::UNIX_EPOCH)
            .unwrap().as_nanos().to_string()});
    #[cfg(unix)]
    let value = {
        use std::os::unix::fs::MetadataExt;
        let mut value = value;
        value["device"] = json!(metadata.dev());
        value["inode"] = json!(metadata.ino());
        value["ctime"] = json!([metadata.ctime(), metadata.ctime_nsec()]);
        value
    };
    value
}

#[test]
fn gh512_persisted_legacy_reuse_and_primary_append_reach_search_in_both_modes() {
    let reusable = env!("CASS_SOURCE_INGEST_REUSE") == "true";
    for provider in ["claude_code", "codex"] {
        for streaming in ["0", "1"] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path();
            let directory = home.join(if provider == "claude_code" {
                ".claude/projects/ledger"
            } else {
                ".codex/sessions/2026/08/01"
            });
            fs::create_dir_all(&directory).unwrap();
            let first = transcript(&directory, provider, 0);
            transcript(&directory, provider, 1);
            index(home, streaming, "initial", 0, 2);
            let initial_ids = archive(home, 2, false);
            let parent_before = directory_observation(&directory);
            let storage = SqliteStorage::open(&home.join("data/agent_search.db")).unwrap();
            let rows = storage.source_ingest_ledger_entries().unwrap();
            assert_eq!(rows.len(), 2);
            let mut legacy_rows = Vec::new();
            for (key, observation) in rows {
                let mut saved: Value = serde_json::from_str(&observation).unwrap();
                assert_eq!(saved["dependencies"], json!([]));
                // Keep the real producer contract: exercise legacy row shape,
                // never authorize a completion from an incompatible parser.
                saved["dependencies"] = json!([parent_before.clone()]);
                let legacy = saved.to_string();
                storage
                    .raw()
                    .execute_with_params(
                        "UPDATE meta SET value = ?1 WHERE key = ?2",
                        &[
                            SqliteValue::Text(legacy.clone().into()),
                            SqliteValue::Text(key.clone().into()),
                        ],
                    )
                    .unwrap();
                legacy_rows.push((key, legacy));
            }
            drop(storage);
            transcript(&directory, provider, 2);
            // Force an observable directory change even on coarse timestamp
            // filesystems. Non-transcript siblings are not parse candidates.
            for attempt in 0..1024 {
                if directory_observation(&directory) != parent_before {
                    break;
                }
                fs::write(directory.join(format!("clock-{attempt}.tmp")), b"sibling").unwrap();
            }
            assert_ne!(directory_observation(&directory), parent_before);
            index(
                home,
                streaming,
                "grown",
                if reusable { 2 } else { 0 },
                if reusable { 1 } else { 3 },
            );
            let grown_ids = archive(home, 3, false);
            for (content, id) in &initial_ids {
                assert_eq!(grown_ids.get(content), Some(id));
            }
            let storage = SqliteStorage::open_readonly(&home.join("data/agent_search.db")).unwrap();
            let grown_rows = storage.source_ingest_ledger_entries().unwrap();
            assert_eq!(grown_rows.len(), 3);
            if reusable {
                for (key, legacy) in legacy_rows {
                    assert_eq!(
                        grown_rows.get(&key),
                        Some(&legacy),
                        "reuse must not require rewriting legacy rows"
                    );
                }
            }
            drop(storage);
            for source in 0..3 {
                assert_searchable(home, &format!("ghledger{source:03}message000z"));
            }
            // An actual new message, not a whitespace-only rewrite, must be
            // imported despite its old timestamp and previously certified row.
            let mut content = fs::read_to_string(&first).unwrap();
            content.push_str(&format!("{}\n", row(provider, 0, 1)));
            fs::write(&first, content).unwrap();
            index(
                home,
                streaming,
                "primary-appended",
                if reusable { 2 } else { 0 },
                if reusable { 1 } else { 3 },
            );
            let appended_ids = archive(home, 3, true);
            for (content, id) in &grown_ids {
                assert_eq!(appended_ids.get(content), Some(id));
            }
            assert_searchable(home, "ghledger000message001z");
            index(
                home,
                streaming,
                "unchanged-again",
                if reusable { 3 } else { 0 },
                if reusable { 0 } else { 3 },
            );
            assert_eq!(archive(home, 3, true), appended_ids);
        }
    }
}


#[test]
fn gh512_whole_source_exclusions_reuse_allowed_files_without_losing_deferred_sources() {
    let reusable = env!("CASS_SOURCE_INGEST_REUSE") == "true";
    for provider in ["claude_code", "codex"] {
        for streaming in ["0", "1"] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path();
            let directory = home.join(if provider == "claude_code" {
                ".claude/projects/ledger"
            } else {
                ".codex/sessions/2026/08/01"
            });
            fs::create_dir_all(&directory).unwrap();
            let first = transcript(&directory, provider, 0);
            let excluded = transcript(&directory, provider, 1);
            let excluded_bytes = fs::read(&excluded).unwrap();
            let excluded_mtime = fs::metadata(&excluded).unwrap().modified().unwrap();
            index_with_exclusion(
                home,
                streaming,
                "excluded-initial",
                0,
                1,
                Some(&excluded),
            );
            let initial = archive(home, 1, false);
            let initial_ledger = source_ledger(home);
            assert_eq!(
                initial_ledger.len(),
                1,
                "a whole admitted source must be certified"
            );

            transcript(&directory, provider, 2);
            index_with_exclusion(
                home,
                streaming,
                "excluded-grown",
                usize::from(reusable),
                if reusable { 1 } else { 2 },
                Some(&excluded),
            );
            let grown = archive_contents(home, 2);
            assert_eq!(grown.len(), 2);
            assert_eq!(
                grown.get("ghledger000message000z"),
                initial.get("ghledger000message000z")
            );
            assert!(grown.contains_key("ghledger002message000z"));
            assert!(!grown.contains_key("ghledger001message000z"));
            let grown_ledger = source_ledger(home);
            assert_eq!(grown_ledger.len(), 2);
            for (key, value) in &initial_ledger {
                assert_eq!(
                    grown_ledger.get(key),
                    Some(value),
                    "no cleanup reparse or row rewrite"
                );
            }
            assert_searchable(home, "ghledger002message000z");

            let mut content = fs::read_to_string(&first).unwrap();
            content.push_str(&format!("{}\n", row(provider, 0, 1)));
            fs::write(&first, content).unwrap();
            index_with_exclusion(
                home,
                streaming,
                "excluded-appended",
                usize::from(reusable),
                if reusable { 1 } else { 2 },
                Some(&excluded),
            );
            let appended = archive_contents(home, 2);
            assert_eq!(appended.len(), 3);
            for (content, id) in &grown {
                assert_eq!(appended.get(content), Some(id));
            }
            assert!(!appended.contains_key("ghledger001message000z"));
            assert_searchable(home, "ghledger000message001z");

            assert_eq!(fs::read(&excluded).unwrap(), excluded_bytes);
            assert_eq!(
                fs::metadata(&excluded).unwrap().modified().unwrap(),
                excluded_mtime
            );
            index(
                home,
                streaming,
                "exclusion-removed",
                if reusable { 2 } else { 0 },
                if reusable { 1 } else { 3 },
            );
            let recovered = archive(home, 3, true);
            for (content, id) in &appended {
                assert_eq!(recovered.get(content), Some(id));
            }
            assert_eq!(source_ledger(home).len(), 3);
            assert_searchable(home, "ghledger001message000z");
            index(
                home,
                streaming,
                "exclusion-final",
                if reusable { 3 } else { 0 },
                if reusable { 0 } else { 3 },
            );
            assert_eq!(archive(home, 3, true), recovered);
        }
    }
}
