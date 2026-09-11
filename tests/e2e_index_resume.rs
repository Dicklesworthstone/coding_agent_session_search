//! GH426: real CLI interruption and exact-source resume over generated Claude logs.
use coding_agent_search::franken_sync::SqliteValue;
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn source_term(source: usize) -> String {
    // Unquoted CASS queries also search edge n-grams: resumeproof1 matches
    // resumeproof10. A terminated source ID cannot prefix another source ID.
    format!("resumeproof{source:03}z")
}

fn seed(home: &Path, count: usize) {
    let root = home.join(".claude/projects/resume");
    fs::create_dir_all(&root).unwrap();
    for source in 0..count {
        let term = source_term(source);
        let rows=(0..32).map(|message|json!({"type":"user","sessionId":format!("resume-{source}"),
            "uuid":format!("resume-{source}-{message}"),"timestamp":"2026-08-01T10:00:00Z","cwd":"/work/resume",
            "message":{"role":"user","content":format!("{term} exactresume{source}message{message} message {message}")}}).to_string())
            .collect::<Vec<_>>().join("\n");
        fs::write(
            root.join(format!("session-{source}.jsonl")),
            format!("{rows}\n"),
        )
        .unwrap();
    }
}

fn command(home: &Path, streaming: &str) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("CASS_DATA_DIR", home.join("data"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_STREAMING_INDEX", streaming)
        .env("CASS_AUTO_REFRESH", "0")
        .env("RUST_MIN_STACK", "134217728")
        .env("NO_COLOR", "1")
        .current_dir(home)
        .args(["--color", "never"]);
    command
}

fn index_command(home: &Path, streaming: &str, trace: &Path) -> Command {
    let mut command = command(home, streaming);
    // Robot stderr deliberately ignores RUST_LOG and suppresses DEBUG/INFO.
    // Use the explicit production trace sink without enabling dependency
    // DEBUG output. Each invocation gets its own file because traces append.
    command
        .env(
            "CASS_TRACE_FILTER",
            "warn,coding_agent_search::indexer=debug",
        )
        .arg("--trace-file")
        .arg(trace)
        .args(["index", "--json"]);
    command
}

fn source_observations(log: &str) -> Vec<bool> {
    log.lines()
        .filter_map(|line| {
            let event: Value = serde_json::from_str(line).expect("valid completed trace JSONL");
            assert_ne!(event["event"], "trace_truncated", "{log}");
            if event["fields"]["message"] != "source_ingest_observation" {
                return None;
            }
            Some(
                event["fields"]["skipped"]
                    .as_bool()
                    .expect("source observation carries a boolean skip decision"),
            )
        })
        .collect()
}

fn verify_completed_summary(output: &[u8], conversations: usize) {
    let summary: Value = serde_json::from_slice(output).expect("completed index JSON");
    assert_eq!(summary["success"], true, "{summary}");
    assert_eq!(summary["conversations"], conversations, "{summary}");
    assert_eq!(summary["messages"], conversations * 32, "{summary}");
    assert_eq!(
        summary["indexing_stats"]["total_conversations"], conversations,
        "{summary}"
    );
    assert_eq!(
        summary["indexing_stats"]["total_messages"],
        conversations * 32,
        "{summary}"
    );
    assert_eq!(
        summary["indexing_stats"]["connector_summary"]["claude"]["indexed"], conversations,
        "{summary}"
    );
}

fn verify_archive(home: &Path, count: usize) {
    let storage = SqliteStorage::open_readonly(&home.join("data/agent_search.db")).unwrap();
    let conversations = storage.list_conversations(1000, 0).unwrap();
    assert_eq!(conversations.len(), count);
    for conversation in conversations {
        assert_eq!(
            storage
                .fetch_messages(conversation.id.unwrap())
                .unwrap()
                .len(),
            32
        );
    }
    drop(storage);
    for source in 0..count {
        let output = assert_cmd::Command::from_std(command(home, "1"))
            .args([
                "search",
                &source_term(source),
                "--mode",
                "lexical",
                "--json",
                "--no-maintenance",
                "--limit",
                "100",
            ])
            .timeout(Duration::from_secs(30))
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let result: Value = serde_json::from_slice(&output).unwrap();
        let hits = result["hits"].as_array().unwrap();
        assert_eq!(
            hits.len(),
            32,
            "lexical gap or duplicate for source {source}"
        );
        for message in 0..32 {
            let token = format!("exactresume{source}message{message}");
            assert_eq!(
                hits.iter()
                    .filter(|hit| hit["content"].as_str().is_some_and(|content| content
                        .split_whitespace()
                        .any(|word| word == token)))
                    .count(),
                1,
                "missing or duplicated lexical message {source}/{message}"
            );
        }
        assert_ne!(
            result.pointer("/budget/timed_out").and_then(Value::as_bool),
            Some(true)
        );
    }
}

#[test]
fn gh426_bounded_stop_resumes_only_uncommitted_sources_in_both_modes() {
    // Mutable path dependencies have no Cargo-pinned parser source identity;
    // those developer builds deliberately reparse instead of trusting skips.
    let reusable = env!("CASS_SOURCE_INGEST_REUSE") == "true";
    for streaming in ["0", "1"] {
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), 8);
        let stopped_trace = home.path().join("stopped-trace.jsonl");
        let stopped =
            assert_cmd::Command::from_std(index_command(home.path(), streaming, &stopped_trace))
                .env("CASS_INDEX_MAX_SOURCE_COMMITS", "2")
                .timeout(Duration::from_secs(120))
                .assert()
                .failure()
                .get_output()
                .clone();
        assert!(String::from_utf8_lossy(&stopped.stderr).contains("interrupted"));
        let storage =
            SqliteStorage::open_readonly(&home.path().join("data/agent_search.db")).unwrap();
        assert_eq!(storage.source_ingest_ledger_entries().unwrap().len(), 2);
        assert_eq!(storage.list_conversations(100, 0).unwrap().len(), 2);
        drop(storage);
        let resumed_trace = home.path().join("resumed-trace.jsonl");
        let resumed =
            assert_cmd::Command::from_std(index_command(home.path(), streaming, &resumed_trace))
                .timeout(Duration::from_secs(180))
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
        verify_completed_summary(&resumed, if reusable { 6 } else { 8 });
        let log = fs::read_to_string(&resumed_trace).unwrap();
        let observations = source_observations(&log);
        assert_eq!(
            observations.iter().filter(|&&skipped| skipped).count(),
            if reusable { 2 } else { 0 },
            "{log}"
        );
        assert_eq!(
            observations.iter().filter(|&&skipped| !skipped).count(),
            if reusable { 6 } else { 8 },
            "{log}"
        );
        verify_archive(home.path(), 8);
        // Old binaries may have certified the same unchanged bytes using a
        // parser with different behavior. Reparse missing/old producer contracts
        // while retaining the six valid completions and canonical message IDs.
        let storage = SqliteStorage::open(&home.path().join("data/agent_search.db")).unwrap();
        let entries = storage.source_ingest_ledger_entries().unwrap();
        assert_eq!(entries.len(), 8);
        let canonical_ids = |storage: &SqliteStorage| {
            let mut ids = storage
                .list_conversations(100, 0)
                .unwrap()
                .into_iter()
                .map(|conversation| {
                    let id = conversation.id.unwrap();
                    let messages = storage
                        .fetch_messages(id)
                        .unwrap()
                        .into_iter()
                        .map(|message| message.id)
                        .collect::<Vec<_>>();
                    (id, messages)
                })
                .collect::<Vec<_>>();
            ids.sort();
            ids
        };
        let original_ids = canonical_ids(&storage);
        for (index, (key, observation)) in entries.iter().take(2).enumerate() {
            let mut observation: Value = serde_json::from_str(observation).unwrap();
            if index == 0 {
                observation
                    .as_object_mut()
                    .unwrap()
                    .remove("producer_contract");
            } else {
                observation["producer_contract"] = json!("previous-parser-contract");
            }
            storage
                .raw()
                .execute_with_params(
                    "UPDATE meta SET value = ?1 WHERE key = ?2",
                    &[
                        SqliteValue::Text(observation.to_string().into()),
                        SqliteValue::Text(key.clone().into()),
                    ],
                )
                .unwrap();
        }
        drop(storage);
        let upgraded_trace = home.path().join("upgraded-trace.jsonl");
        let upgraded =
            assert_cmd::Command::from_std(index_command(home.path(), streaming, &upgraded_trace))
                .timeout(Duration::from_secs(180))
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
        verify_completed_summary(&upgraded, if reusable { 2 } else { 8 });
        let log = fs::read_to_string(&upgraded_trace).unwrap();
        let observations = source_observations(&log);
        for (skipped, expected) in [
            (false, if reusable { 2 } else { 8 }),
            (true, if reusable { 6 } else { 0 }),
        ] {
            assert_eq!(
                observations
                    .iter()
                    .filter(|&&observed| observed == skipped)
                    .count(),
                expected,
                "{log}"
            );
        }
        verify_archive(home.path(), 8);
        let storage =
            SqliteStorage::open_readonly(&home.path().join("data/agent_search.db")).unwrap();
        assert_eq!(canonical_ids(&storage), original_ids);
        assert_eq!(storage.source_ingest_ledger_entries().unwrap().len(), 8);
        drop(storage);
        // A changed completed source must parse again, even with timestamps
        // inside the old provider history. Replaying identical rows stays idempotent.
        let changed = home.path().join(".claude/projects/resume/session-0.jsonl");
        let mut rows = fs::read_to_string(&changed).unwrap();
        rows.push('\n');
        fs::write(&changed, rows).unwrap();
        let replay_trace = home.path().join("replay-trace.jsonl");
        let replay =
            assert_cmd::Command::from_std(index_command(home.path(), streaming, &replay_trace))
                .timeout(Duration::from_secs(180))
                .assert()
                .success()
                .get_output()
                .stdout
                .clone();
        verify_completed_summary(&replay, if reusable { 1 } else { 8 });
        let log = fs::read_to_string(&replay_trace).unwrap();
        let observations = source_observations(&log);
        assert_eq!(
            observations.iter().filter(|&&skipped| !skipped).count(),
            if reusable { 1 } else { 8 },
            "{log}"
        );
        verify_archive(home.path(), 8);
    }
}

#[cfg(unix)]
#[test]
fn gh426_sigterm_and_sigint_stop_at_commit_boundary_and_resume() {
    struct Guard(Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for (signal, exit) in [("-TERM", 143), ("-INT", 130)] {
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), 80);
        let stdout = home.path().join("stdout");
        let stderr = home.path().join("stderr");
        let trace = home.path().join("signal-trace.jsonl");
        fs::File::create(&trace).unwrap();
        let mut child = Guard(
            index_command(home.path(), "1", &trace)
                .stdout(Stdio::from(fs::File::create(stdout).unwrap()))
                .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        while !fs::read_to_string(&trace)
            .unwrap()
            .lines()
            // The live reader may catch the writer partway through a line;
            // only a fully decoded committed event authorizes the signal.
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["fields"]["message"] == "source_ingest_committed")
        {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "index exited before signal: {}",
                fs::read_to_string(&stderr).unwrap()
            );
            assert!(Instant::now() < deadline, "no source commit before signal");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            Command::new("kill")
                .args([signal, &child.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "graceful signal shutdown timed out: {}",
                fs::read_to_string(&stderr).unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(
            status.code(),
            Some(exit),
            "{}",
            fs::read_to_string(&stderr).unwrap()
        );
        let resumed_trace = home.path().join("signal-resumed-trace.jsonl");
        assert_cmd::Command::from_std(index_command(home.path(), "1", &resumed_trace))
            .timeout(Duration::from_secs(240))
            .assert()
            .success();
        verify_archive(home.path(), 80);
    }
}

#[cfg(unix)]
#[test]
fn gh426_batch_fallback_sigterm_and_sigint_preserve_committed_prefix() {
    struct Guard(Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for (signal, exit) in [("-TERM", 143), ("-INT", 130)] {
        let home = tempfile::tempdir().unwrap();
        // Claude completes through the boundary producer first. Aider then
        // exercises the real non-streaming fallback, one durable batch at a
        // time. Alternating prompt/answer blocks produce 32 native messages.
        seed(home.path(), 1);
        for source in 1..81 {
            let term = source_term(source);
            let root = if source == 1 {
                home.path().to_path_buf()
            } else {
                home.path().join(format!("aider/project-{source}"))
            };
            fs::create_dir_all(&root).unwrap();
            let history = (0..32)
                .map(|message| {
                    format!(
                        "{}{term} exactresume{source}message{message} message {message}\n\n",
                        if message % 2 == 0 { "> " } else { "" },
                    )
                })
                .collect::<String>();
            fs::write(root.join(".aider.chat.history.md"), history).unwrap();
        }
        let trace = home.path().join("fallback-signal-trace.jsonl");
        let stderr = home.path().join("fallback-stderr");
        let stdout = home.path().join("fallback-stdout");
        let mut child = Guard(
            index_command(home.path(), "0", &trace)
                .env("CASS_AIDER_DATA_ROOT", home.path())
                .env("CASS_NON_WATCH_INGEST_CHUNK_SIZE", "1")
                .arg("--robot-trace-ingest")
                .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
                .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let log = fs::read_to_string(&stderr).unwrap();
            let commits = log
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|event| {
                    event["event"] == "ingest_batch"
                        && event["status"] == "ok"
                        && event["inserted_conversations"]
                            .as_u64()
                            .is_some_and(|count| count > 0)
                })
                .count();
            if commits >= 2 {
                break;
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "index exited before fallback signal: {log}"
            );
            assert!(
                Instant::now() < deadline,
                "no committed fallback batch before signal: {log}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            Command::new("kill")
                .args([signal, &child.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "fallback graceful stop timed out: {}",
                fs::read_to_string(&stderr).unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(
            status.code(),
            Some(exit),
            "{}",
            fs::read_to_string(&stderr).unwrap()
        );
        let storage =
            SqliteStorage::open_readonly(&home.path().join("data/agent_search.db")).unwrap();
        let before = storage.list_conversations(1000, 0).unwrap();
        assert!(
            before.len() >= 2 && before.len() < 81,
            "signal must leave a real committed prefix"
        );
        let prefix: Vec<_> = before
            .into_iter()
            .map(|conversation| {
                let id = conversation.id.unwrap();
                let messages = storage.fetch_messages(id).unwrap();
                assert_eq!(messages.len(), 32);
                (
                    id,
                    messages
                        .into_iter()
                        .map(|message| message.id)
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        drop(storage);
        let resumed_trace = home.path().join("fallback-resumed-trace.jsonl");
        assert_cmd::Command::from_std(index_command(home.path(), "0", &resumed_trace))
            .env("CASS_AIDER_DATA_ROOT", home.path())
            .env("CASS_NON_WATCH_INGEST_CHUNK_SIZE", "1")
            .timeout(Duration::from_secs(240))
            .assert()
            .success();
        verify_archive(home.path(), 81);
        let storage =
            SqliteStorage::open_readonly(&home.path().join("data/agent_search.db")).unwrap();
        for (id, expected_ids) in prefix {
            let ids = storage
                .fetch_messages(id)
                .unwrap()
                .into_iter()
                .map(|message| message.id)
                .collect::<Vec<_>>();
            assert_eq!(
                ids, expected_ids,
                "fallback replay must preserve committed canonical IDs"
            );
        }
    }
}
