//! Codebuff's whole-source exclusions must not disable durable source reuse.
//! Run the real CLI, source ledger, canonical store and lexical search.

use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

fn cass(home: &Path, mode: &str) -> assert_cmd::Command {
    let mut command = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData/Roaming"))
        .env("LOCALAPPDATA", home.join("AppData/Local"))
        .env("CASS_DATA_DIR", home.join("data"))
        .env("CASS_STREAMING_INDEX", mode)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_MIN_STACK", "134217728")
        .current_dir(home)
        .timeout(Duration::from_secs(180));
    for name in ["SystemRoot", "WINDIR"] {
        if let Ok(value) = dotenvy::var(name) {
            command.env(name, value);
        }
    }
    // Windows uses the OS profile rather than USERPROFILE. Isolate that
    // platform via the public override; Unix keeps default-path coverage.
    #[cfg(windows)]
    command.env(
        "CASS_CODEBUFF_DATA_ROOT",
        home.join(".config/manicode/projects"),
    );
    command
}

fn write_old(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_774_113_351))
        .unwrap();
}

fn chat(home: &Path, project: &str, content: &str) -> PathBuf {
    let directory = home
        .join(".config/manicode/projects")
        .join(project)
        .join("chats/2026-03-21T17-14-03.768Z");
    fs::create_dir_all(&directory).unwrap();
    let primary = directory.join("chat-messages.json");
    write_old(
        &primary,
        &serde_json::to_vec(&json!([
            {"id":"user-1774113351457", "variant":"user", "content":content,
             "timestamp":"01:15 PM"}
        ]))
        .unwrap(),
    );
    write_old(
        &directory.join("run-state.json"),
        br#"{"sessionState":{"fileContext":{"projectRoot":"/synthetic/probe"}}}"#,
    );
    primary
}

fn index(
    home: &Path,
    mode: &str,
    step: &str,
    excluded: Option<&Path>,
    expected_skipped: usize,
    expected_parsed: usize,
) {
    let trace = home.join(format!("{step}.trace.jsonl"));
    let mut command = cass(home, mode);
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
        .args(["index", "--json", "--no-progress-events"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["success"], true, "{report}");
    assert_eq!(
        report["indexing_stats"]["scan_had_errors"], false,
        "{report}"
    );
    let log = fs::read_to_string(trace).unwrap();
    let mut skipped = 0;
    let mut parsed = 0;
    for line in log.lines() {
        let event: Value = serde_json::from_str(line).unwrap();
        assert_ne!(event["event"], "trace_truncated", "{log}");
        if event["fields"]["message"] == "source_ingest_observation"
            && event["fields"]["connector"] == "codebuff"
        {
            if event["fields"]["skipped"].as_bool().unwrap() {
                skipped += 1;
            } else {
                parsed += 1;
            }
        }
    }
    assert_eq!(
        (skipped, parsed),
        (expected_skipped, expected_parsed),
        "{log}"
    );
}

fn archive(home: &Path) -> BTreeMap<String, (i64, i64)> {
    let storage = SqliteStorage::open_readonly(&home.join("data/agent_search.db")).unwrap();
    let mut messages = BTreeMap::new();
    // On Windows the other connectors find the real profile (FOLDERID_Profile
    // ignores USERPROFILE), so a host with agent history adds conversations.
    for conversation in storage
        .list_conversations(i64::MAX, 0)
        .unwrap()
        .into_iter()
        .filter(|conversation| conversation.agent_slug == "codebuff")
    {
        let id = conversation.id.unwrap();
        for message in storage.fetch_messages(id).unwrap() {
            assert_eq!(message.created_at, Some(1_774_113_351_457));
            assert!(
                messages
                    .insert(message.content, (id, message.id.unwrap()))
                    .is_none(),
                "replay must not duplicate a canonical message"
            );
        }
    }
    messages
}

/// This fixture's source checkpoints. On Windows the other connectors find the
/// real profile, so a host with agent history adds checkpoints of its own.
fn ledger(home: &Path) -> HashMap<String, String> {
    let storage = SqliteStorage::open_readonly(&home.join("data/agent_search.db")).unwrap();
    let home = fs::canonicalize(home).unwrap();
    storage
        .source_ingest_ledger_entries()
        .unwrap()
        .into_iter()
        .filter(|(_, observation)| {
            serde_json::from_str::<Value>(observation)
                .ok()
                .and_then(|observation| observation["primary"]["path"].as_str().map(PathBuf::from))
                .map(|path| fs::canonicalize(&path).unwrap_or(path))
                .is_some_and(|path| path.starts_with(&home))
        })
        .collect()
}

fn search(home: &Path, content: &str) -> Value {
    let output = cass(home, "1")
        .args([
            "search",
            content,
            "--agent",
            "codebuff",
            "--mode",
            "lexical",
            "--json",
            "--no-maintenance",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

#[test]
fn codebuff_reuses_admitted_sources_without_freezing_sidecars_or_excluded_history() {
    for mode in ["0", "1"] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let private = chat(home, "private", "reuseprivateproof9z");
        let public = chat(home, "private-copy", "reusepublicproof7z");
        let other = chat(home, "other", "reuseotherproof8z");
        // Exclude the consulted metadata input, not just the primary. The
        // adapter must defer the whole private chat BEFORE the admission hook.
        let excluded = private.with_file_name("run-state.json");
        let inputs: Vec<_> = [&private, &public, &other]
            .into_iter()
            .flat_map(|path| [path.clone(), path.with_file_name("run-state.json")])
            .map(|path| {
                let bytes = fs::read(&path).unwrap();
                let modified = fs::metadata(&path).unwrap().modified().unwrap();
                (path, bytes, modified)
            })
            .collect();
        index(home, mode, "first", Some(&excluded), 0, 2);
        let original_ids = archive(home);
        assert_eq!(original_ids.len(), 2);
        assert!(!original_ids.contains_key("reuseprivateproof9z"));
        assert!(
            search(home, "reuseprivateproof9z")["hits"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            search(home, "reusepublicproof7z")["hits"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let first_ledger = ledger(home);
        assert_eq!(
            first_ledger.len(),
            2,
            "admitted chats need durable checkpoints"
        );
        {
            let storage = SqliteStorage::open_readonly(&home.join("data/agent_search.db")).unwrap();
            assert_eq!(
                storage.get_connector_last_scan_ts("codebuff").unwrap(),
                None,
                "source completion must not certify the deliberately incomplete store"
            );
        }
        index(home, mode, "unchanged", Some(&excluded), 2, 0);
        assert_eq!(ledger(home), first_ledger);
        assert_eq!(archive(home), original_ids);

        // Same old mtime, unchanged primary, changed sidecar length. A
        // whole-source exclusion promise is NOT a self-containment promise.
        let state = public.with_file_name("run-state.json");
        let changed_state =
            br#"{"sessionState":{"fileContext":{"projectRoot":"/synthetic/changed-workspace"}}}"#;
        write_old(&state, changed_state);
        index(home, mode, "metadata-change", Some(&excluded), 1, 1);
        assert_eq!(archive(home), original_ids);
        let changed_ledger = ledger(home);
        assert_eq!(changed_ledger.len(), 2);
        assert_eq!(
            first_ledger
                .iter()
                .filter(|(key, value)| changed_ledger.get(*key) == Some(*value))
                .count(),
            1,
            "only the metadata-dependent source observation should change"
        );
        let changed_hit = search(home, "reusepublicproof7z");
        assert_eq!(
            changed_hit["hits"][0]["workspace"],
            "/synthetic/changed-workspace"
        );
        index(home, mode, "metadata-stable", Some(&excluded), 2, 0);
        assert_eq!(ledger(home), changed_ledger);

        // Removing the exclusion must admit March history without --full or
        // source touches, while both already-certified public chats stay reused.
        index(home, mode, "include-private", None, 2, 1);
        let final_ids = archive(home);
        assert_eq!(final_ids.len(), 3);
        assert!(final_ids.contains_key("reuseprivateproof9z"));
        for (content, identity) in &original_ids {
            assert_eq!(final_ids.get(content), Some(identity));
        }
        assert_eq!(
            search(home, "reuseprivateproof9z")["hits"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let final_ledger = ledger(home);
        assert_eq!(final_ledger.len(), 3);
        index(home, mode, "all-stable", None, 3, 0);
        assert_eq!(ledger(home), final_ledger);
        assert_eq!(archive(home), final_ids);
        for (path, bytes, modified) in inputs {
            let expected = if path == state {
                &changed_state[..]
            } else {
                &bytes[..]
            };
            assert_eq!(fs::read(&path).unwrap(), expected);
            assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
        }
    }
}
