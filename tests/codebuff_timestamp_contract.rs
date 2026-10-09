//! GH#511: exercise FAD's timestamp contract through CASS, not a second parser.
//!
//! Native display clocks use the message ID's instant, but a corrupt absolute
//! timestamp must remain a parse failure even when that ID contains an instant.
//! A repaired absolute timestamp must then win over the ID when CASS retries.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

// One component per join, as discovery builds paths: a '/' inside a fragment
// survives on Windows, so an error naming the discovered path would not
// contain the fixture's spelling of it.
fn codebuff_projects(home: &Path) -> PathBuf {
    home.join(".config").join("manicode").join("projects")
}

fn chat_dir(root: &Path, project: &str) -> PathBuf {
    root.join(project)
        .join("chats")
        .join("2026-03-21T17-14-03.768Z")
}

fn cass(home: &Path, data: &Path, streaming: &str) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CASS_DATA_DIR", data)
        .env("CASS_STREAMING_INDEX", streaming)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_MIN_STACK", "134217728")
        .current_dir(home)
        .timeout(Duration::from_secs(180));
    if let Ok(system_root) = dotenvy::var("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    // Windows uses the OS profile rather than USERPROFILE. Isolate that
    // platform via the public override; Unix keeps default-path coverage.
    #[cfg(windows)]
    command.env("CASS_CODEBUFF_DATA_ROOT", codebuff_projects(home));
    command
}

fn search(home: &Path, data: &Path, streaming: &str, query: &str) -> Value {
    let output = cass(home, data, streaming)
        .args([
            "search",
            query,
            "--agent",
            "codebuff",
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
    serde_json::from_slice(&output).unwrap()
}

fn message_hit(home: &Path, data: &Path, streaming: &str, content: &str) -> Value {
    let result = search(home, data, streaming, content);
    let hits: Vec<_> = result["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|hit| hit["content"].as_str() == Some(content))
        .collect();
    assert_eq!(hits.len(), 1, "streaming={streaming}: {result}");
    hits[0].clone()
}

#[test]
fn gh511_corrupt_absolute_timestamp_is_not_hidden_by_a_valid_message_id() {
    for streaming in ["0", "1"] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let data = temp.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let root = codebuff_projects(&home);
        let good = chat_dir(&root, "a-good");
        let bad = chat_dir(&root, "z-corrupt-iso");
        let old_mtime = UNIX_EPOCH + Duration::from_secs(1_774_113_351);
        for chat in [&good, &bad] {
            fs::create_dir_all(chat).unwrap();
            let state = chat.join("run-state.json");
            fs::write(
                &state,
                br#"{"sessionState":{"fileContext":{"projectRoot":"/synthetic/probe"}}}"#,
            )
            .unwrap();
            fs::File::options()
                .write(true)
                .open(state)
                .unwrap()
                .set_modified(old_mtime)
                .unwrap();
        }
        let good = good.join("chat-messages.json");
        let bad = bad.join("chat-messages.json");
        // Unlike the existing bad-before-good fixture, the late error must
        // not discard good chats already delivered by FAD's scan callback.
        assert!(good < bad);
        let native = serde_json::to_vec(&json!([
            {"id":"user-1774113351457", "variant":"user",
             "content":"Synthetic probe question", "timestamp":"01:15 PM"},
            {"id":"ai-1774113411457", "variant":"ai",
             "content":"Synthetic probe answer", "timestamp":"01:16 PM"}
        ]))
        .unwrap();
        let mut broken = json!([
            {"id":"user-1774113471457", "variant":"user",
             "content":"corruptisomarker", "timestamp":"2026-13-45T99:00:00Z"}
        ]);
        let broken_bytes = serde_json::to_vec(&broken).unwrap();
        fs::write(&good, &native).unwrap();
        fs::write(&bad, &broken_bytes).unwrap();
        for path in [&good, &bad] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old_mtime)
                .unwrap();
        }

        let output = cass(&home, &data, streaming)
            .args(["index", "--full", "--json", "--no-progress-events"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(9), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let connector = report["indexing_stats"]["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|connector| connector["name"] == "codebuff")
            .unwrap();
        let error = connector["error"].as_str().unwrap();
        assert!(
            error.contains("invalid shared CLI timestamp at record 0")
                && error.contains("unparseable ISO-8601 timestamp"),
            "{report}"
        );
        let bad_canonical = bad.canonicalize().unwrap();
        assert!(
            error.contains(bad.to_string_lossy().as_ref())
                || error.contains(bad_canonical.to_string_lossy().as_ref()),
            "the failing transcript must be named: {report}"
        );
        assert_eq!(report["indexing_stats"]["scan_had_errors"], true);
        assert_eq!(connector["conversations"], 1, "{report}");
        assert_eq!(connector["messages"], 2, "{report}");
        let diagnostics: Vec<_> = report["indexing_stats"]["connector_diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|diagnostic| diagnostic["provider"] == "codebuff")
            .collect();
        assert!(!diagnostics.is_empty(), "{report}");
        for diagnostic in diagnostics {
            assert_eq!(diagnostic["failure_kind"], "unparseable-source");
            assert_eq!(diagnostic["retryable"], false);
            assert_eq!(diagnostic["severity"], "error");
            assert_eq!(diagnostic["disposition"], "skipped");
            let source = Path::new(diagnostic["source_path"].as_str().unwrap())
                .canonicalize()
                .unwrap();
            assert_ne!(source, data.canonicalize().unwrap(), "{diagnostic}");
            assert!(bad_canonical.starts_with(&source), "{diagnostic}");
            assert!(
                !diagnostic["safe_next_action"]
                    .as_str()
                    .unwrap()
                    .to_ascii_lowercase()
                    .contains("check permissions"),
                "{diagnostic}"
            );
        }
        assert_eq!(
            report["indexing_stats"]["connector_summary"]["codebuff"]["locked"], 0,
            "{report}"
        );
        let absent = search(&home, &data, streaming, "corruptisomarker");
        assert!(absent["hits"].as_array().unwrap().is_empty(), "{absent}");
        for (content, instant) in [
            ("Synthetic probe question", 1_774_113_351_457_i64),
            ("Synthetic probe answer", 1_774_113_411_457_i64),
        ] {
            let hit = message_hit(&home, &data, streaming, content);
            assert_eq!(hit["created_at"].as_i64(), Some(instant), "{hit}");
        }
        assert_eq!(fs::read(&good).unwrap(), native);
        assert_eq!(fs::read(&bad).unwrap(), broken_bytes);

        // Keep both the transcript and sidecar older than the failed scan.
        // A normal incremental retry must recover it without a full rebuild.
        broken[0]["timestamp"] = json!("2026-09-01T12:00:00.000Z");
        fs::write(&bad, serde_json::to_vec(&broken).unwrap()).unwrap();
        fs::File::options()
            .write(true)
            .open(&bad)
            .unwrap()
            .set_modified(old_mtime)
            .unwrap();
        let output = cass(&home, &data, streaming)
            .args(["index", "--json", "--no-progress-events"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let report: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(report["indexing_stats"]["scan_had_errors"], false);
        let recovered = message_hit(&home, &data, streaming, "corruptisomarker");
        assert_eq!(
            recovered["created_at"].as_i64(),
            Some(1_788_264_000_000),
            "the explicit ISO instant, not the March ID instant, must win: {recovered}"
        );
        assert_eq!(fs::read(&good).unwrap(), native);
        message_hit(&home, &data, streaming, "Synthetic probe question");
        message_hit(&home, &data, streaming, "Synthetic probe answer");
    }
}

/// Multiple bad sources must be visible in one CLI result. Repairing one must
/// neither hide the remaining failure nor duplicate the already retained chat.
#[test]
fn gh511_multiple_failures_recover_independently_across_indexing_modes() {
    use coding_agent_search::storage::sqlite::SqliteStorage;
    use std::collections::BTreeMap;

    fn archive(data: &Path) -> BTreeMap<String, (i64, i64)> {
        let storage = SqliteStorage::open_readonly(&data.join("agent_search.db")).unwrap();
        let mut messages = BTreeMap::new();
        // On Windows the other connectors find the real profile
        // (FOLDERID_Profile ignores USERPROFILE), so a host with agent history
        // adds conversations this test does not own.
        for conversation in storage
            .list_conversations(i64::MAX, 0)
            .unwrap()
            .into_iter()
            .filter(|conversation| conversation.agent_slug == "codebuff")
        {
            let conversation_id = conversation.id.unwrap();
            for message in storage.fetch_messages(conversation_id).unwrap() {
                assert!(
                    messages
                        .insert(message.content, (conversation_id, message.id.unwrap()))
                        .is_none(),
                    "retry must not duplicate canonical messages"
                );
            }
        }
        messages
    }

    fn error_summary(report: &Value) -> &str {
        assert_eq!(
            report["indexing_stats"]["scan_had_errors"], true,
            "{report}"
        );
        report["indexing_stats"]["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|connector| connector["name"] == "codebuff")
            .unwrap()["error"]
            .as_str()
            .unwrap()
    }

    fn names_path(message: &str, path: &Path) -> bool {
        message.contains(path.to_string_lossy().as_ref())
            || message.contains(path.canonicalize().unwrap().to_string_lossy().as_ref())
    }

    for (first_mode, retry_mode) in [("0", "1"), ("1", "0")] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let data = temp.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let root = codebuff_projects(&home);
        let bad_json = chat_dir(&root, "a-bad-json").join("chat-messages.json");
        let good = chat_dir(&root, "middle-good").join("chat-messages.json");
        let bad_iso = chat_dir(&root, "z-bad-iso").join("chat-messages.json");
        let old = UNIX_EPOCH + Duration::from_secs(1_774_113_351);
        let write_old = |path: &Path, bytes: &[u8]| {
            fs::write(path, bytes).unwrap();
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        };
        let state = br#"{"sessionState":{"fileContext":{"projectRoot":"/synthetic/probe"}}}"#;
        for primary in [&bad_json, &good, &bad_iso] {
            fs::create_dir_all(primary.parent().unwrap()).unwrap();
            write_old(&primary.with_file_name("run-state.json"), state);
        }
        let records = |id: &str, content: &str, timestamp: &str| {
            serde_json::to_vec(&json!([{
                "id":id, "variant":"user", "content":content, "timestamp":timestamp
            }]))
            .unwrap()
        };
        let healthy = records("user-1774113351457", "multifailurehealthyproof", "01:15 PM");
        let invalid_iso = records(
            "user-1774113471457",
            "multirepairedisoproof",
            "2026-13-45T99:00:00Z",
        );
        write_old(&bad_json, b"[");
        write_old(&good, &healthy);
        write_old(&bad_iso, &invalid_iso);
        let index = |mode: &str, full: bool, exit: i32| {
            let mut command = cass(&home, &data, mode);
            command.args(["index", "--json", "--no-progress-events"]);
            if full {
                command.arg("--full");
            }
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), Some(exit), "{output:?}");
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        };
        let first = index(first_mode, true, 9);
        let error = error_summary(&first);
        assert!(names_path(error, &bad_json), "{first}");
        assert!(names_path(error, &bad_iso), "{first}");
        assert!(
            error.contains("invalid Codebuff / Freebuff transcript JSON"),
            "{first}"
        );
        assert!(error.contains("unparseable ISO-8601 timestamp"), "{first}");
        let healthy_ids = archive(&data);
        assert_eq!(healthy_ids.len(), 1);
        let hit = message_hit(&home, &data, first_mode, "multifailurehealthyproof");
        assert_eq!(hit["created_at"].as_i64(), Some(1_774_113_351_457));

        // Neither an unchanged retry nor a mode switch is proof of recovery.
        let unchanged = index(retry_mode, false, 9);
        let error = error_summary(&unchanged);
        assert!(
            names_path(error, &bad_json) && names_path(error, &bad_iso),
            "{unchanged}"
        );
        assert_eq!(archive(&data), healthy_ids);
        assert_eq!(fs::read(&bad_json).unwrap(), b"[");
        assert_eq!(fs::read(&bad_iso).unwrap(), invalid_iso);

        let repaired_json = records("user-1774113411457", "multirepairedjsonproof", "01:16 PM");
        write_old(&bad_json, &repaired_json);
        let partial = index(retry_mode, false, 9);
        let error = error_summary(&partial);
        assert!(names_path(error, &bad_iso), "{partial}");
        assert!(!names_path(error, &bad_json), "{partial}");
        let partial_ids = archive(&data);
        assert_eq!(partial_ids.len(), 2);
        for (content, identity) in &healthy_ids {
            assert_eq!(partial_ids.get(content), Some(identity));
        }
        let hit = message_hit(&home, &data, retry_mode, "multirepairedjsonproof");
        assert_eq!(hit["created_at"].as_i64(), Some(1_774_113_411_457));

        let repaired_iso = records(
            "user-1774113471457",
            "multirepairedisoproof",
            "2026-09-01T12:00:00.000Z",
        );
        write_old(&bad_iso, &repaired_iso);
        let complete = index(first_mode, false, 0);
        assert_eq!(
            complete["indexing_stats"]["scan_had_errors"], false,
            "{complete}"
        );
        let final_ids = archive(&data);
        assert_eq!(final_ids.len(), 3);
        for (content, identity) in &partial_ids {
            assert_eq!(final_ids.get(content), Some(identity));
        }
        let hit = message_hit(&home, &data, first_mode, "multirepairedisoproof");
        assert_eq!(hit["created_at"].as_i64(), Some(1_788_264_000_000));
        index(retry_mode, false, 0);
        assert_eq!(archive(&data), final_ids);
        assert_eq!(fs::read(&good).unwrap(), healthy);
        assert_eq!(fs::read(&bad_json).unwrap(), repaired_json);
        assert_eq!(fs::read(&bad_iso).unwrap(), repaired_iso);
        for primary in [&bad_json, &good, &bad_iso] {
            let sidecar = primary.with_file_name("run-state.json");
            assert_eq!(fs::read(&sidecar).unwrap(), state);
            assert_eq!(fs::metadata(primary).unwrap().modified().unwrap(), old);
            assert_eq!(fs::metadata(sidecar).unwrap().modified().unwrap(), old);
        }
    }
}
