//! E2E tests for multi-connector scenarios.
//!
//! These tests verify that multiple connectors work together correctly:
//! - Multiple connectors can be indexed in a single run
//! - Search returns results from all indexed connectors
//! - Agent filtering correctly isolates connector results
//! - Results are properly attributed to their source connector

use std::fs;
use std::path::Path;

mod util;
use util::e2e_log::{E2eError, E2eErrorContext, E2ePerformanceMetrics, PhaseTracker};

fn tracker_for(test_name: &str) -> PhaseTracker {
    PhaseTracker::new("e2e_multi_connector", test_name)
}

/// GH #423: Codebuff and Freebuff share one Manicode store with no per-chat
/// writer marker, so their history indexes under the `codebuff` lineage,
/// attributed to the run state's project root. The records follow FAD's
/// source-schema fixtures (public TypeScript persistence schema), not captured
/// native app history.
#[test]
fn codebuff_cli_indexes_shared_manicode_history_and_updates_native_messages() {
    use serde_json::{Value, json};
    use std::time::{Duration, SystemTime};

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let project = home.join(".config/manicode/projects/demo");
    let chat = project.join("chats/2026-09-01T12-00-00.000Z");
    std::fs::create_dir_all(&chat).unwrap();
    let transcript = chat.join("chat-messages.json");
    let write_transcript = |answer: &str, modified: SystemTime| {
        let records = json!([
            {"id":"user-native-1", "variant":"user", "content":"codebuffneedle how do I pin the toolchain",
             "timestamp":"2026-09-01T12:00:00.000Z"},
            {"id":"ai-native-2", "variant":"ai", "content": answer,
             "timestamp":"2026-09-01T12:00:05.000Z"}
        ]);
        std::fs::write(&transcript, serde_json::to_vec(&records).unwrap()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&transcript)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    };
    let start = SystemTime::now() - Duration::from_secs(3600);
    write_transcript("codebuffprior: pin it in rust-toolchain.toml", start);
    std::fs::write(
        chat.join("run-state.json"),
        serde_json::to_vec(&json!({
            "sessionState": {"fileContext": {"projectRoot": "/work/codebuff-demo", "cwd": "/work/codebuff-demo"}}
        }))
        .unwrap(),
    )
    .unwrap();
    // Negative: JSON outside a chats/<id>/chat-messages.json slot is not history.
    std::fs::write(
        project.join("notes.json"),
        br#"[{"id":"stray","variant":"user","content":"codebuffstray"}]"#,
    )
    .unwrap();

    let command = || {
        let mut command = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command
            .env_clear()
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("CASS_DATA_DIR", &data)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728")
            .current_dir(&home)
            .timeout(Duration::from_secs(180));
        if let Ok(system_root) = dotenvy::var("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
    };
    let search = |needle: &str| -> Vec<Value> {
        let output = command()
            .args([
                "search",
                needle,
                "--agent",
                "codebuff",
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
        result["hits"].as_array().cloned().unwrap_or_default()
    };
    let messages = || -> i64 {
        let output = command()
            .args(["stats", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice::<Value>(&output).unwrap()["messages"]
            .as_i64()
            .unwrap()
    };

    command()
        .args(["index", "--full", "--json"])
        .assert()
        .success();
    let hits = search("codebuffneedle");
    assert!(!hits.is_empty(), "codebuff history must be searchable");
    for hit in &hits {
        assert_eq!(hit["agent"], "codebuff", "{hit}");
        assert_eq!(hit["workspace"], "/work/codebuff-demo", "{hit}");
    }
    assert!(
        search("codebuffstray").is_empty(),
        "stray JSON must not index"
    );
    assert_eq!(messages(), 2);

    // An edited native message updates in place instead of duplicating. A real
    // edit is newer than the previous run, which incremental discovery requires.
    write_transcript("revised codebuffrevision answer", SystemTime::now());
    assert_eq!(search("codebuffprior").len(), 1);
    command().args(["index", "--json"]).assert().success();
    assert_eq!(search("codebuffrevision").len(), 1);
    assert!(
        search("codebuffprior").is_empty(),
        "the replaced answer must leave the lexical index"
    );
    assert_eq!(messages(), 2, "a native-ID edit must not append a message");
}

/// The GH #511 reporter's store layout under an isolated home: one native
/// Codebuff chat in `project` whose run state names `/synthetic/probe`.
fn gh511_codebuff_chat(
    home: &Path,
    project: &str,
    records: &serde_json::Value,
) -> std::path::PathBuf {
    let chat = home
        .join(".config/manicode/projects")
        .join(project)
        .join("chats/2026-03-21T17-14-03.768Z");
    std::fs::create_dir_all(&chat).unwrap();
    let transcript = chat.join("chat-messages.json");
    std::fs::write(&transcript, serde_json::to_vec(records).unwrap()).unwrap();
    std::fs::write(
        chat.join("run-state.json"),
        br#"{"sessionState":{"fileContext":{"projectRoot":"/synthetic/probe"}}}"#,
    )
    .unwrap();
    transcript
}

/// `cass` that sees only the isolated home and data dir. In particular,
/// `env_clear` leaves XDG_CONFIG_HOME, XDG_DATA_HOME and
/// CASS_CODEBUFF_DATA_ROOT unset, matching the reporter's macOS default-path
/// invocation. The Codebuff binary need not be installed to find its history.
fn gh511_cass(home: &Path, data: &Path) -> assert_cmd::Command {
    let mut command = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
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

/// The one Codebuff hit for the message whose content is `content`, from a
/// search for `needle`. Every message also matches through its conversation
/// title (the first user message), so a search can return several hits.
fn gh511_message_hit(home: &Path, data: &Path, needle: &str, content: &str) -> serde_json::Value {
    let output = gh511_cass(home, data)
        .args([
            "search",
            needle,
            "--agent",
            "codebuff",
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
    let result: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let hits: Vec<serde_json::Value> = result["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|hit| hit["content"] == content)
        .cloned()
        .collect();
    assert_eq!(hits.len(), 1, "{result}");
    hits[0].clone()
}

/// The reporter's native transcript: `timestamp` is a locale time of day.
fn gh511_native_records() -> serde_json::Value {
    serde_json::json!([
        {"id":"user-1774113351457", "variant":"user", "content":"Synthetic probe question",
         "timestamp":"01:15 PM"},
        {"id":"ai-1774113411457", "variant":"ai", "content":"Synthetic probe answer",
         "timestamp":"01:16 PM"}
    ])
}

fn gh511_connector_stats(report: &serde_json::Value) -> &serde_json::Value {
    report["indexing_stats"]["connectors"]
        .as_array()
        .expect("connector summaries")
        .iter()
        .find(|connector| connector["name"] == "codebuff")
        .expect("default discovery found the Codebuff store")
}

fn gh511_assert_parse_diagnostic(report: &serde_json::Value, data: &Path, transcript: &Path) {
    let reported_transcript = transcript.to_string_lossy();
    let transcript = transcript.canonicalize().unwrap();
    let connector = gh511_connector_stats(report);
    let error = connector["error"].as_str().unwrap();
    assert!(
        error.contains(reported_transcript.as_ref())
            || error.contains(&transcript.display().to_string()),
        "the connector error must identify the failed transcript: {connector}"
    );
    assert_eq!(
        report["indexing_stats"]["scan_had_errors"], true,
        "{report}"
    );
    let codebuff: Vec<_> = report["indexing_stats"]["connector_diagnostics"]
        .as_array()
        .expect("connector diagnostics")
        .iter()
        .filter(|diagnostic| diagnostic["provider"] == "codebuff")
        .collect();
    assert!(!codebuff.is_empty(), "{report}");
    for diagnostic in codebuff {
        assert_eq!(
            diagnostic["failure_kind"], "unparseable-source",
            "{diagnostic}"
        );
        assert_eq!(diagnostic["severity"], "error", "{diagnostic}");
        assert_eq!(diagnostic["retryable"], false, "{diagnostic}");
        assert_eq!(diagnostic["disposition"], "skipped", "{diagnostic}");
        let source = Path::new(diagnostic["source_path"].as_str().unwrap())
            .canonicalize()
            .unwrap();
        assert_ne!(source, data.canonicalize().unwrap(), "{diagnostic}");
        assert!(
            transcript.starts_with(&source),
            "the diagnostic must identify the transcript or its scan root: {diagnostic}"
        );
        assert!(
            !diagnostic["safe_next_action"]
                .as_str()
                .unwrap()
                .contains("check permissions"),
            "{diagnostic}"
        );
    }
    assert_eq!(
        report["indexing_stats"]["connector_summary"]["codebuff"]["locked"], 0,
        "{report}"
    );
}

fn gh511_assert_native_messages(home: &Path, data: &Path, transcript: &Path) {
    for (needle, content, instant) in [
        (
            "question",
            "Synthetic probe question",
            1_774_113_351_457_i64,
        ),
        ("answer", "Synthetic probe answer", 1_774_113_411_457),
    ] {
        let hit = gh511_message_hit(home, data, needle, content);
        assert_eq!(hit["created_at"].as_i64(), Some(instant), "{hit}");
        assert_eq!(hit["workspace"], "/synthetic/probe", "{hit}");
        assert_eq!(
            Path::new(hit["source_path"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            transcript.canonicalize().unwrap(),
            "{hit}"
        );
    }
}

/// GH #511 (bead b7rhs): a Codebuff transcript the connector could not
/// interpret failed the codebuff scan, and cass reported `unreadable-source`
/// with "check permissions" against the cass data directory. The failure is in
/// the content, so it must be `unparseable-source`, not retryable on unchanged
/// bytes, and name a scan root that holds the transcript. The fixture is the
/// reporter's second probe: a variant ("assistant") the CLI never writes.
#[test]
fn gh511_unparseable_codebuff_transcript_is_not_an_unreadable_data_dir() {
    use serde_json::{Value, json};

    for streaming in ["0", "1"] {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let transcript = gh511_codebuff_chat(
            &home,
            "probe",
            &json!([
                {"id":"user-1774113351457", "variant":"user", "content":"Synthetic probe question",
                 "timestamp":"2026-03-21T17:15:51.457Z"},
                {"id":"ai-1774113411457", "variant":"assistant", "content":"Synthetic probe answer",
                 "timestamp":"2026-03-21T17:16:51.457Z"}
            ]),
        );
        let before = fs::read(&transcript).unwrap();
        let output = gh511_cass(&home, &data)
            .env("CASS_STREAMING_INDEX", streaming)
            .args(["index", "--full", "--json", "--no-progress-events"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(9),
            "streaming={streaming}: {output:?}"
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let connector = gh511_connector_stats(&report);
        assert!(
            connector["error"]
                .as_str()
                .is_some_and(|error| error.contains("variant at record 1")),
            "the connector error still names the bad record: {connector}"
        );
        gh511_assert_parse_diagnostic(&report, &data, &transcript);
        assert_eq!(fs::read(&transcript).unwrap(), before);
    }
}

/// GH #511: native Codebuff writes `timestamp` as a locale time of day
/// ("01:15 PM"), which franken-agent-detection 0.3.3 rejected for every
/// native transcript. The reporter's transcript now indexes, and each message
/// carries the instant its ID's `Date.now()` recorded; nothing is derived
/// from the time of day. Both indexing modes must discover the home-relative
/// store with XDG_CONFIG_HOME and the Codebuff root override unset.
#[test]
fn gh511_native_time_of_day_transcript_indexes_with_its_id_instants() {
    for streaming in ["0", "1"] {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let transcript = gh511_codebuff_chat(&home, "probe", &gh511_native_records());
        let before = fs::read(&transcript).unwrap();
        let output = gh511_cass(&home, &data)
            .env("CASS_STREAMING_INDEX", streaming)
            .args(["index", "--full", "--json", "--no-progress-events"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let connector = gh511_connector_stats(&report);
        assert_eq!(
            connector["conversations"], 1,
            "streaming={streaming}: {report}"
        );
        assert_eq!(connector["messages"], 2, "streaming={streaming}: {report}");
        assert!(connector.get("error").is_none(), "{connector}");
        assert_eq!(
            report["indexing_stats"]["scan_had_errors"], false,
            "{report}"
        );
        assert_eq!(
            report["indexing_stats"]["connector_summary"]["codebuff"]["indexed"], 1,
            "{report}"
        );
        gh511_assert_native_messages(&home, &data, &transcript);
        assert_eq!(fs::read(&transcript).unwrap(), before);
    }
}

/// GH #511: a real default-store enumeration failure must name the directory
/// that could not be read, retaining the I/O cause before any source is found.
/// Root runs drop only the child process's privileges so chmod is meaningful.
#[test]
#[cfg(unix)]
fn gh511_default_discovery_permission_error_names_the_manicode_directory() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
    use std::os::unix::process::CommandExt;

    let tmp = tempfile::Builder::new()
        .prefix("cass-gh511-permissions-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let root = fs::metadata(tmp.path()).unwrap().uid() == 0;
    let binary = if root {
        // The build tree may be beneath a root-only directory. Copying just
        // the executable keeps that tree's permissions unchanged.
        let executable = tmp.path().join("cass");
        fs::copy(assert_cmd::cargo::cargo_bin!("cass"), &executable).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        executable
    } else {
        assert_cmd::cargo::cargo_bin!("cass").to_path_buf()
    };

    for streaming in ["0", "1"] {
        let home = tmp.path().join(format!("home-{streaming}"));
        let data = tmp.path().join(format!("data-{streaming}"));
        fs::create_dir_all(&data).unwrap();
        let transcript = gh511_codebuff_chat(&home, "unreadable-project", &gh511_native_records());
        let before = fs::read(&transcript).unwrap();
        for path in [
            home.clone(),
            home.join(".config"),
            home.join(".config/manicode"),
            home.join(".config/manicode/projects"),
        ] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let denied = home.join(".config/manicode/projects/unreadable-project");
        let original_permissions = fs::metadata(&denied).unwrap().permissions();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
        let mut command = std::process::Command::new(&binary); // ubs:ignore[rust.security.command-executable] — Cargo's test binary or its byte-identical copy in this private test directory.
        command
            .env_clear()
            .env("HOME", &home)
            .env("CASS_DATA_DIR", &data)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_STREAMING_INDEX", streaming)
            .env("RUST_MIN_STACK", "134217728")
            .current_dir(&home)
            .args(["index", "--full", "--json", "--no-progress-events"]);
        if root {
            chown(&home, Some(65_534), Some(65_534)).unwrap();
            chown(&data, Some(65_534), Some(65_534)).unwrap();
            command.gid(65_534).uid(65_534);
        }
        let result = assert_cmd::Command::from(command)
            .timeout(std::time::Duration::from_secs(180))
            .output();
        // Restore access before any assertion, including a subprocess error.
        fs::set_permissions(&denied, original_permissions).unwrap();
        let output = result.unwrap();
        assert_eq!(
            output.status.code(),
            Some(9),
            "streaming={streaming}: {output:?}"
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let connector = gh511_connector_stats(&report);
        let error = connector["error"].as_str().unwrap();
        assert!(
            error.contains("cannot enumerate Codebuff / Freebuff history")
                && error.to_ascii_lowercase().contains("permission denied")
                && (error.contains(&denied.display().to_string())
                    || error.contains(&denied.canonicalize().unwrap().display().to_string())),
            "the discovery error must preserve the failed path and I/O cause: {connector}"
        );
        assert_eq!(
            report["indexing_stats"]["scan_had_errors"], true,
            "{report}"
        );
        let diagnostics: Vec<_> = report["indexing_stats"]["connector_diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|diagnostic| diagnostic["provider"] == "codebuff")
            .collect();
        assert!(!diagnostics.is_empty(), "{report}");
        let mut named_failed_directory = false;
        for diagnostic in diagnostics {
            assert_eq!(
                diagnostic["failure_kind"], "unreadable-source",
                "{diagnostic}"
            );
            assert_eq!(diagnostic["retryable"], true, "{diagnostic}");
            let source = Path::new(diagnostic["source_path"].as_str().unwrap())
                .canonicalize()
                .unwrap();
            assert_ne!(source, data.canonicalize().unwrap(), "{diagnostic}");
            assert!(
                denied.canonicalize().unwrap().starts_with(&source),
                "{diagnostic}"
            );
            named_failed_directory |= source == denied.canonicalize().unwrap();
        }
        assert!(named_failed_directory, "{report}");
        assert_eq!(fs::read(&transcript).unwrap(), before);
    }
}

/// GH #511: one Codebuff transcript that does not parse (here truncated
/// mid-write) stopped the scan, so every chat after it in path order went
/// unindexed (franken-agent-detection 0.3.4 and earlier). The bad chat sorts
/// first here. The good chat now indexes, the run still exits 9, and the
/// connector error names the bad transcript and its cause. Batch mode must
/// retain FAD's successful callbacks too, and "busy" in a project name must
/// not turn a parse failure into a lock diagnosis.
#[test]
fn gh511_one_unparseable_codebuff_transcript_does_not_hide_the_others() {
    use serde_json::{Value, json};

    for streaming in ["0", "1"] {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let truncated = gh511_codebuff_chat(&home, "a-busy-project", &json!([]));
        fs::write(&truncated, br#"[{"id":"user-1774113351457""#).unwrap();
        let good = gh511_codebuff_chat(&home, "z-good", &gh511_native_records());
        assert!(
            truncated < good,
            "the bad transcript must be discovered first"
        );
        let modified_before_failure =
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_774_113_351);
        let before: Vec<_> = [
            truncated.clone(),
            truncated.with_file_name("run-state.json"),
            good.clone(),
            good.with_file_name("run-state.json"),
        ]
        .into_iter()
        .map(|path| {
            // Both the transcript and its run state predate this scan, so a
            // fresh sidecar cannot accidentally rescue an advanced watermark.
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(modified_before_failure)
                .unwrap();
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
        let output = gh511_cass(&home, &data)
            .env("CASS_STREAMING_INDEX", streaming)
            .args(["index", "--full", "--json", "--no-progress-events"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(9),
            "streaming={streaming}: {output:?}"
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let connector = gh511_connector_stats(&report);
        assert!(
            connector["error"]
                .as_str()
                .unwrap()
                .contains("invalid Codebuff / Freebuff transcript JSON"),
            "{connector}"
        );
        gh511_assert_parse_diagnostic(&report, &data, &truncated);
        assert_eq!(
            connector["conversations"], 1,
            "streaming={streaming}: {report}"
        );
        assert_eq!(connector["messages"], 2, "streaming={streaming}: {report}");
        assert_eq!(
            report["indexing_stats"]["connector_summary"]["codebuff"]["indexed"], 1,
            "{report}"
        );
        gh511_assert_native_messages(&home, &data, &good);
        for (path, bytes) in before {
            assert_eq!(fs::read(&path).unwrap(), bytes, "{}", path.display());
        }

        // A failed scan must not advance the connector watermark. Repair the
        // bad source but retain its pre-failure mtime: a normal incremental
        // retry must ingest it and keep the good chat without duplication.
        fs::write(
            &truncated,
            serde_json::to_vec(&json!([
                {"id":"user-1774113471457", "variant":"user", "content":"Synthetic recovered question",
                 "timestamp":"01:17 PM"}
            ]))
            .unwrap(),
        )
        .unwrap();
        fs::File::options()
            .write(true)
            .open(&truncated)
            .unwrap()
            .set_modified(modified_before_failure)
            .unwrap();
        let retry = gh511_cass(&home, &data)
            .env("CASS_STREAMING_INDEX", streaming)
            .args(["index", "--json", "--no-progress-events"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let retry: Value = serde_json::from_slice(&retry).unwrap();
        assert_eq!(retry["indexing_stats"]["scan_had_errors"], false, "{retry}");
        assert!(
            gh511_connector_stats(&retry).get("error").is_none(),
            "{retry}"
        );
        let recovered =
            gh511_message_hit(&home, &data, "recovered", "Synthetic recovered question");
        assert_eq!(
            recovered["created_at"].as_i64(),
            Some(1_774_113_471_457),
            "{recovered}"
        );
        gh511_assert_native_messages(&home, &data, &good);
    }
}

/// A Codex rollout holding one question with `needle` and its answer.
fn gh513_rollout_text(needle: &str) -> String {
    [
        serde_json::json!({"timestamp": "2026-08-01T10:00:00Z", "type": "session_meta",
            "payload": {"cwd": "/synthetic/gh513"}}),
        serde_json::json!({"timestamp": "2026-08-01T10:00:01Z", "type": "response_item",
            "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": format!("question {needle}")}]}}),
        serde_json::json!({"timestamp": "2026-08-01T10:00:02Z", "type": "response_item",
            "payload": {"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "the answer"}]}}),
    ]
    .iter()
    .map(|record| format!("{record}\n"))
    .collect()
}

/// A session as Codex's `local_thread_store_compression` leaves it:
/// `rollout-*.jsonl.zst`, one zstd frame that records the decoded length,
/// with the plain rollout's modification time (here 30 days ago).
fn gh513_compressed_rollout(home: &Path, name: &str, text: &str) -> std::path::PathBuf {
    let day = home.join(".codex/sessions/2026/08/01");
    fs::create_dir_all(&day).unwrap();
    let path = day.join(format!("rollout-2026-08-01T10-00-00-{name}.jsonl.zst"));
    fs::write(&path, zstd::bulk::compress(text.as_bytes(), 3).unwrap()).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old)
        .unwrap();
    path
}

/// Codex hits for `needle`, as `(source_path, conversation_id)`.
fn gh513_hits(home: &Path, data: &Path, needle: &str) -> Vec<(String, serde_json::Value)> {
    let output = gh511_cass(home, data)
        .args([
            "search",
            needle,
            "--agent",
            "codex",
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
    let result: serde_json::Value = serde_json::from_slice(&output).unwrap();
    result["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hit| {
            (
                hit["source_path"].as_str().unwrap_or_default().to_owned(),
                hit["conversation_id"].clone(),
            )
        })
        .collect()
}

fn gh513_codex_conversations(home: &Path, data: &Path) -> serde_json::Value {
    let output = gh511_cass(home, data)
        .args(["stats", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stats: serde_json::Value = serde_json::from_slice(&output).unwrap();
    stats["by_agent"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["agent"] == "codex")
        .map_or(serde_json::Value::Null, |row| row["count"].clone())
}

/// GH #513: both Codex passes (franken-agent-detection's parse and cass's
/// enrichment) read a compressed rollout's decoded text, so the connector
/// gives the same conversation, id included, for either form of one rollout.
#[test]
fn gh513_a_compressed_rollout_scans_and_enriches_like_its_plain_form() {
    use coding_agent_search::connectors::{
        Connector, ScanContext, ScanRoot, codex::CodexConnector,
    };
    let response = |payload: serde_json::Value| {
        serde_json::json!({"type": "response_item", "timestamp": "2026-08-01T10:00:05.000Z",
            "payload": payload})
    };
    let text = [
        response(serde_json::json!({"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "compressed parity question"}]})),
        response(
            serde_json::json!({"type": "custom_tool_call", "name": "apply_patch",
            "call_id": "patch-1", "input": "*** Begin Patch\n+compressed_parity\n*** End Patch\n"}),
        ),
        response(
            serde_json::json!({"type": "function_call_output", "call_id": "patch-1",
            "output": "patch applied"}),
        ),
    ]
    .iter()
    .map(|record| format!("{record}\n"))
    .collect::<String>();
    let scan = |compressed: bool, enrich: bool| -> Vec<serde_json::Value> {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".codex");
        let day = home.join("sessions/2026/08/01");
        fs::create_dir_all(&day).unwrap();
        let plain = day.join("rollout-2026-08-01T10-00-00-parity.jsonl");
        let written = if compressed {
            plain.with_extension("jsonl.zst")
        } else {
            plain
        };
        if compressed {
            fs::write(&written, zstd::bulk::compress(text.as_bytes(), 3).unwrap()).unwrap();
        } else {
            fs::write(&written, &text).unwrap();
        }
        let ctx = ScanContext::with_roots(
            dir.path().join("cass-data"),
            vec![ScanRoot::local(home)],
            None,
        );
        let conversations = if enrich {
            CodexConnector::new().scan(&ctx).unwrap()
        } else {
            franken_agent_detection::CodexConnector::new()
                .scan(&ctx)
                .unwrap()
        };
        conversations
            .into_iter()
            .map(|conversation| {
                assert_eq!(conversation.source_path, written);
                let mut value = serde_json::to_value(conversation).unwrap();
                value.as_object_mut().unwrap().remove("source_path");
                value
            })
            .collect()
    };
    let plain = scan(false, true);
    assert_eq!(plain.len(), 1);
    assert_eq!(
        plain[0]["external_id"],
        "2026/08/01/rollout-2026-08-01T10-00-00-parity"
    );
    // The fixture needs the enrichment pass: without it the messages differ.
    assert_ne!(plain[0]["messages"], scan(false, false)[0]["messages"]);
    assert_eq!(scan(true, true), plain);
}

/// GH #513 (bead 3u22z): a Codex home holding only compressed sessions (the
/// reporter's 1,599 of 1,609) indexes every one of them, searchably.
#[test]
fn gh513_codex_sessions_compressed_to_jsonl_zst_are_indexed() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let first = gh513_compressed_rollout(&home, "first", &gh513_rollout_text("gh513firstneedle"));
    gh513_compressed_rollout(&home, "second", &gh513_rollout_text("gh513secondneedle"));
    gh511_cass(&home, &data)
        .args(["index", "--full", "--json", "--no-progress-events"])
        .assert()
        .success();
    assert_eq!(gh513_codex_conversations(&home, &data), 2);
    let hits = gh513_hits(&home, &data, "gh513firstneedle");
    assert!(
        !hits.is_empty(),
        "the compressed session's text is searchable"
    );
    let name = first.file_name().unwrap();
    assert!(
        hits.iter()
            .all(|(path, _)| Path::new(path).file_name() == Some(name)),
        "{hits:?}"
    );
}

/// GH #513: a session indexed as `rollout-*.jsonl` that Codex then compresses
/// (plain file removed, modification time kept) is the same session, not a
/// second one.
#[test]
fn gh513_a_session_compressed_after_indexing_is_not_duplicated() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let text = gh513_rollout_text("gh513sameneedle");
    let packed = gh513_compressed_rollout(&home, "same", &text);
    let plain = packed.with_extension("");
    let mtime = fs::metadata(&packed).unwrap().modified().unwrap();
    fs::rename(&packed, packed.with_extension("parked")).unwrap();
    fs::write(&plain, &text).unwrap();
    gh511_cass(&home, &data)
        .args(["index", "--full", "--json", "--no-progress-events"])
        .assert()
        .success();
    let before = gh513_hits(&home, &data, "gh513sameneedle");
    assert!(!before.is_empty());
    // The comparison below needs a real id, not two missing ones.
    assert!(before[0].1.is_i64(), "{before:?}");

    // Codex compresses it: the compressed file keeps the plain file's mtime.
    fs::remove_file(&plain).unwrap();
    fs::rename(packed.with_extension("parked"), &packed).unwrap();
    fs::File::options()
        .write(true)
        .open(&packed)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    gh511_cass(&home, &data)
        .args(["index", "--full", "--json", "--no-progress-events"])
        .assert()
        .success();
    assert_eq!(gh513_codex_conversations(&home, &data), 1);
    let after = gh513_hits(&home, &data, "gh513sameneedle");
    assert!(
        after.iter().all(|(_, id)| *id == before[0].1),
        "before {before:?} after {after:?}"
    );
}

/// GH #513: an archive whose Codex watermark an older build recorded picks
/// up compressed sessions older than that watermark on its next plain
/// `cass index`, in both ingest modes, and records the scan contract so the
/// run after it has a cutoff again.
#[test]
fn gh513_an_older_codex_watermark_does_not_hide_old_compressed_sessions() {
    use coding_agent_search::storage::sqlite::FrankenStorage;
    for streaming in ["0", "1"] {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let data = tmp.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let plain_day = home.join(".codex/sessions/2026/10/01");
        fs::create_dir_all(&plain_day).unwrap();
        fs::write(
            plain_day.join("rollout-2026-10-01T10-00-00-plain.jsonl"),
            gh513_rollout_text("gh513plainneedle"),
        )
        .unwrap();
        gh511_cass(&home, &data)
            .env("CASS_STREAMING_INDEX", streaming)
            .args(["index", "--full", "--json", "--no-progress-events"])
            .assert()
            .success();
        let db = data.join("agent_search.db");
        {
            // Leave the archive as an older build would: a Codex watermark
            // recorded without the scan contract.
            let storage = FrankenStorage::open(&db).unwrap();
            let state = storage.connector_scan_states(&["codex"]).unwrap()["codex"];
            assert!(state.last_scan_ts.is_some(), "streaming={streaming}");
            assert!(!state.scan_contract_pending, "streaming={streaming}");
            storage
                .raw()
                .execute("DELETE FROM meta WHERE key = 'scan_contract:connector:codex'")
                .unwrap();
            assert!(
                storage.connector_scan_states(&["codex"]).unwrap()["codex"].scan_contract_pending
            );
        }

        // Compressed long ago, so older than the watermark.
        let packed =
            gh513_compressed_rollout(&home, "backlog", &gh513_rollout_text("gh513backlogneedle"));
        gh511_cass(&home, &data)
            .env("CASS_STREAMING_INDEX", streaming)
            .args(["index", "--json", "--no-progress-events"])
            .assert()
            .success();
        let hits = gh513_hits(&home, &data, "gh513backlogneedle");
        assert!(
            hits.iter()
                .any(|(path, _)| Path::new(path).file_name() == packed.file_name()),
            "streaming={streaming}: {hits:?}"
        );
        let storage = FrankenStorage::open_readonly(&db).unwrap();
        assert!(
            !storage.connector_scan_states(&["codex"]).unwrap()["codex"].scan_contract_pending,
            "streaming={streaming}: the scan recorded the contract"
        );
    }
}

/// GH #499 (bead 2l1b0.49): once an incremental run tombstones a row inside a
/// sealed Quill segment, every date-filtered search failed with "posting
/// cursor invariant failed: Boolean children belong to different segment
/// domains" (exit 9) on frankensearch-quill 0.3.1. The indexer tombstones on
/// the revised-native-message path, so a Codebuff edit reproduces it. Twelve
/// messages keep the tombstone density under the engine's compaction
/// threshold, and the window covers only some of them: a window over every
/// row lowers to a whole-segment match and never reached the bug.
#[test]
fn gh499_date_window_after_a_native_revision_tombstones_a_sealed_segment() {
    use coding_agent_search::search::{quill_bridge, tantivy};
    use serde_json::{Value, json};
    use std::collections::BTreeSet;
    use std::time::{Duration, SystemTime};

    /// Tombstones and segments summed over every Quill MANIFEST under
    /// `root`: a single generation or each shard of a federated bundle.
    fn manifest_totals(root: &Path) -> (u64, usize) {
        let mut totals = (0, 0);
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            if dir.join("MANIFEST").is_file()
                && let Some(live) = quill_bridge::manifest_live_doc_count(&dir)
            {
                totals.0 += live.tombstones;
                totals.1 += live.segments;
            }
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    pending.push(entry.path());
                }
            }
        }
        totals
    }

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let chat = home.join(".config/manicode/projects/window/chats/2026-08-01T12-00-00.000Z");
    fs::create_dir_all(&chat).unwrap();
    let transcript = chat.join("chat-messages.json");
    // Message i is dated 2026-08-(i+1); message 5 is the one revised.
    let write_transcript = |revised: bool, modified: SystemTime| {
        let records: Vec<Value> = (0..12)
            .map(|i| {
                let text = if i == 5 && revised {
                    format!("windowmsg{i} gh499marker revisedanswer")
                } else {
                    format!("windowmsg{i} gh499marker originalanswer{i}")
                };
                json!({
                    "id": format!("native-{i}"),
                    "variant": if i % 2 == 0 { "user" } else { "ai" },
                    "content": text,
                    "timestamp": format!("2026-08-{:02}T12:00:00.000Z", i + 1),
                })
            })
            .collect();
        fs::write(&transcript, serde_json::to_vec(&records).unwrap()).unwrap();
        fs::File::options()
            .write(true)
            .open(&transcript)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    };
    write_transcript(false, SystemTime::now() - Duration::from_secs(3600));

    let command = || {
        let mut command = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command
            .env_clear()
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("CASS_DATA_DIR", &data)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728")
            .env("TZ", "UTC")
            .current_dir(&home)
            .timeout(Duration::from_secs(180));
        if let Ok(system_root) = dotenvy::var("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
    };
    let index = |args: &[&str]| {
        let started = std::time::Instant::now();
        let output = command().args(args).output().unwrap();
        eprintln!(
            "{}",
            json!({"step": "index", "args": args, "exit": output.status.code(),
                   "elapsed_ms": started.elapsed().as_millis()})
        );
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };

    index(&["index", "--full", "--json"]);
    write_transcript(true, SystemTime::now());
    index(&["index", "--json"]);

    let index_root = tantivy::expected_index_dir(&data);
    let (tombstones, segments) = manifest_totals(&index_root);
    eprintln!(
        "{}",
        json!({"step": "manifest", "root": index_root, "tombstones": tombstones,
               "segments": segments})
    );
    assert!(
        tombstones > 0,
        "the fixture needs a tombstone in a sealed segment ({segments} segments)"
    );

    let output = command()
        .args([
            "search",
            "gh499marker",
            "--agent",
            "codebuff",
            "--since",
            "2026-08-04",
            "--until",
            "2026-08-09",
            "--mode",
            "lexical",
            "--robot",
            "--limit",
            "50",
            "--no-maintenance",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!(
        "{}",
        json!({"step": "search", "exit": output.status.code(), "stderr_tail":
               stderr.lines().rev().take(3).collect::<Vec<_>>()})
    );
    assert!(
        output.status.success(),
        "a date-filtered search over a tombstoned segment must succeed: {stderr}"
    );
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    let contents: Vec<&str> = payload["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["content"].as_str().unwrap_or_default())
        .collect();
    let messages: BTreeSet<usize> = contents
        .iter()
        .filter_map(|content| {
            let start = content.find("windowmsg")? + "windowmsg".len();
            content[start..]
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
        .collect();
    eprintln!("{}", json!({"step": "hits", "messages": messages}));
    // 2026-08-04 through 2026-08-09 are messages 3..=8.
    assert_eq!(
        messages,
        (3..=8).collect::<BTreeSet<usize>>(),
        "{contents:?}"
    );
    assert!(
        contents
            .iter()
            .any(|content| content.contains("revisedanswer")),
        "the live replacement is served: {contents:?}"
    );
    assert!(
        !contents
            .iter()
            .any(|content| content.contains("originalanswer5")),
        "the tombstoned row is not: {contents:?}"
    );
}

/// Generated rolling windows use the reporter-confirmed Grok Bot 0.44.0
/// envelope and chat fields (GH447 comment 5592555144). They exercise CASS
/// ingestion, not a live macOS application or complete cloud history.
#[test]
fn grok_bot_fifo_cli_preserves_evicted_and_new_messages() {
    use coding_agent_search::storage::sqlite::FrankenStorage;
    use serde_json::{Value, json};
    use std::time::Duration;

    fn encoded_key(key: &str) -> String {
        let alphabet = b"abcdefghijklmnopqrstuvwxyz234567";
        key.bytes()
            .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
            .collect::<Vec<_>>()
            .chunks(5)
            .map(|chunk| {
                let value =
                    chunk.iter().fold(0_u8, |value, bit| (value << 1) | *bit) << (5 - chunk.len());
                char::from(alphabet[usize::from(value)])
            })
            .collect()
    }
    fn command(home: &Path, data: &Path, root: &Path, streaming: &str) -> assert_cmd::Command {
        let mut command = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command
            .env_clear()
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("CASS_GROK_BOT_DATA_ROOT", root)
            .env("CASS_DATA_DIR", data)
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_STREAMING_INDEX", streaming)
            .env("CASS_AUTO_REFRESH", "0")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728")
            .current_dir(home)
            .timeout(Duration::from_secs(180));
        if let Ok(system_root) = dotenvy::var("SystemRoot") {
            command.env("SystemRoot", system_root);
        }
        command
    }
    fn search(home: &Path, data: &Path, root: &Path, needle: &str, streaming: &str) -> Value {
        let output = command(home, data, root, streaming)
            .args([
                "search",
                needle,
                "--agent",
                "grok_bot",
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
        assert_ne!(
            result.pointer("/budget/timed_out").and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            result["hits"].is_array(),
            "search must return verified hits"
        );
        result
    }
    for streaming in ["0", "1"] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join("Grok Bot 空間/sand-client-persistence");
        let data = home.join("archive");
        fs::create_dir_all(&root).unwrap();
        let key = "sand.client.slice.account.auth0%7Cuser_00000000000000000000000000.transcript.replicas.81da155f-cc4c-58b3-aadf-770858d5e55c";
        let path = root.join(format!("{}.blob", encoded_key(key)));
        let window = |start: u32| {
            json!({"schemaVersion":1,"value":{"entries":
            (start..start + 200).map(|native| json!({
                "kind":"send-message","id":format!("entry-{native}"),
                "message":{"type":"text","content": match native {
                    1 => "groknativeevictedneedle", 201 => "groknativenewtailneedle", _ => "identical timestamp and content",
                }},"timestampMs":1_767_225_622_759_i64,
                "secretRequest":{"content":"grokexcludedsecretneedle"},
            })).collect::<Vec<_>>()}})
        };
        fs::write(&path, window(1).to_string()).unwrap();
        let original_source = fs::read(&path).unwrap();
        command(home, &data, &root, streaming)
            .args(["index", "--full", "--json", "--no-progress-events"])
            .assert()
            .success();
        assert_eq!(fs::read(&path).unwrap(), original_source);
        let (conversation_id, before) = {
            let storage = FrankenStorage::open_readonly(&data.join("agent_search.db")).unwrap();
            let conversations = storage.list_conversations(10, 0).unwrap();
            assert_eq!(conversations.len(), 1);
            assert_eq!(conversations[0].agent_slug, "grok_bot");
            assert_eq!(conversations[0].metadata_json["history_complete"], false);
            assert!(conversations[0].workspace.is_none());
            let id = conversations[0].id.unwrap();
            let messages = storage.fetch_messages(id).unwrap();
            assert_eq!(messages.len(), 200);
            (id, serde_json::to_value(messages).unwrap())
        };
        fs::write(&path, window(2).to_string()).unwrap();
        let rolled_source = fs::read(&path).unwrap();
        // Each command is a new process. Incremental, replay, and explicit full
        // rebuild must all preserve the canonical 201-message archive.
        for full in [false, false, true] {
            let mut index = command(home, &data, &root, streaming);
            index.args(["index", "--json", "--no-progress-events"]);
            if full {
                index.arg("--full");
            }
            index.assert().success();
            assert_eq!(fs::read(&path).unwrap(), rolled_source);
            let storage = FrankenStorage::open_readonly(&data.join("agent_search.db")).unwrap();
            let messages = storage.fetch_messages(conversation_id).unwrap();
            assert_eq!(messages.len(), 201, "streaming={streaming} full={full}");
            assert_eq!(serde_json::to_value(&messages[..200]).unwrap(), before);
            assert_eq!(messages[200].idx, 200);
            assert_eq!(messages[200].extra_json["grok_bot_entry_id"], "entry-201");
            drop(storage);
            for needle in ["groknativeevictedneedle", "groknativenewtailneedle"] {
                let result = search(home, &data, &root, needle, streaming);
                let hits = result["hits"].as_array().unwrap();
                assert_eq!(
                    hits.len(),
                    1,
                    "canonical old/new message must remain searchable"
                );
                assert_eq!(hits[0]["agent"], "grok_bot");
                let view = command(home, &data, &root, streaming)
                    .args(["view"])
                    .arg(&path)
                    // A search hit's line_number is the canonical message
                    // ordinal; since #493 (5173f7db) follow-ups name it with
                    // --message-index, and --line is only a physical JSONL line.
                    .args([
                        "--source",
                        hits[0]["source_id"].as_str().unwrap(),
                        "--conversation-id",
                        &conversation_id.to_string(),
                        "--message-index",
                        &hits[0]["line_number"].as_u64().unwrap().to_string(),
                        "--json",
                    ])
                    .assert()
                    .success()
                    .get_output()
                    .stdout
                    .clone();
                let value: Value = serde_json::from_slice(&view).unwrap();
                assert!(
                    value.to_string().contains(needle),
                    "canonical view must retain the searched message"
                );
            }
            assert!(
                search(home, &data, &root, "grokexcludedsecretneedle", streaming)["hits"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

fn truncate_output(bytes: &[u8], max_len: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() > max_len {
        format!(
            "{}... [truncated {} bytes]",
            &s[..max_len],
            s.len() - max_len
        )
    } else {
        s.to_string()
    }
}

fn make_codex_fixture(root: &Path) {
    let sessions = root.join("sessions/2025/11/21");
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join("rollout-1.jsonl");
    // Modern Codex JSONL format (envelope)
    let sample = r#"{"type": "event_msg", "timestamp": 1700000000000, "payload": {"type": "user_message", "message": "codex_user"}}
{"type": "response_item", "timestamp": 1700000001000, "payload": {"role": "assistant", "content": "codex_assistant"}}
"#;
    fs::write(file, sample).unwrap();
}

fn make_claude_fixture(root: &Path) {
    let project = root.join("projects/test-project");
    fs::create_dir_all(&project).unwrap();
    let file = project.join("session.jsonl");
    // Claude Code format
    let sample = r#"{"type": "user", "timestamp": "2023-11-21T10:00:00Z", "message": {"role": "user", "content": "claude_user"}}
{"type": "assistant", "timestamp": "2023-11-21T10:00:05Z", "message": {"role": "assistant", "content": "claude_assistant"}}
"#;
    fs::write(file, sample).unwrap();
}

fn make_gemini_fixture(root: &Path) {
    let project_hash = root.join("tmp/hash123/chats");
    fs::create_dir_all(&project_hash).unwrap();
    let file = project_hash.join("session-1.json"); // Must start with session-
    // Gemini CLI format
    let sample = r#"{
  "messages": [
    {"role": "user", "timestamp": 1700000000000, "content": "gemini_user"},
    {"role": "model", "timestamp": 1700000001000, "content": "gemini_assistant"}
  ]
}"#;
    fs::write(file, sample).unwrap();
}

fn make_cline_fixture(root: &Path) {
    let task_dir = root.join("Code/User/globalStorage/saoudrizwan.claude-dev/task_123");
    fs::create_dir_all(&task_dir).unwrap();

    let ui_messages = task_dir.join("ui_messages.json");
    let sample = r#"[
  {"role": "user", "ts": 1700000000000, "content": "cline_user"},
  {"role": "assistant", "ts": 1700000001000, "content": "cline_assistant"}
]"#;
    fs::write(ui_messages, sample).unwrap();

    let metadata = task_dir.join("task_metadata.json");
    fs::write(metadata, r#"{"id": "task_123", "title": "Cline Task"}"#).unwrap();
}

fn make_amp_fixture(root: &Path) {
    let amp_dir = root.join("amp/cache");
    fs::create_dir_all(&amp_dir).unwrap();
    let file = amp_dir.join("thread_abc.json");
    let sample = r#"{"messages": [
        {"role": "user", "created_at": 1700000000000, "content": "amp_user"},
        {"role": "assistant", "created_at": 1700000001000, "content": "amp_assistant"}
    ]}"#;
    fs::write(file, sample).unwrap();
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "Linux-specific test (XDG_DATA_HOME paths)"
)]
fn multi_connector_pipeline() {
    let tracker = tracker_for("multi_connector_pipeline");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let xdg_data = home.join("xdg_data");

    fs::create_dir_all(&xdg_data).unwrap();

    // Setup fixture roots
    let dot_codex = home.join(".codex");
    let dot_claude = home.join(".claude");
    let dot_gemini = home.join(".gemini");
    let dot_config = home.join(".config");

    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&dot_codex);

    // Phase: Create fixtures for all connectors
    let phase_start = tracker.start("setup_fixtures", Some("Create fixtures for 5 connectors"));
    make_codex_fixture(&dot_codex);
    make_claude_fixture(&dot_claude);
    make_gemini_fixture(&dot_gemini);
    make_cline_fixture(&dot_config);
    make_amp_fixture(&xdg_data);
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();
    tracker.end(
        "setup_fixtures",
        Some("Create fixtures for 5 connectors"),
        phase_start,
    );

    // Phase: Full index
    let phase_start = tracker.start(
        "run_index_full",
        Some("Run full index across all connectors"),
    );
    let idx_output = command_env
        .cass_assert_command()
        .arg("index")
        .arg("--full")
        .arg("--data-dir")
        .arg(&data_dir)
        .env("HOME", home.to_string_lossy().as_ref())
        .env("XDG_DATA_HOME", xdg_data.to_string_lossy().as_ref())
        .env("CODEX_HOME", dot_codex.to_string_lossy().as_ref())
        .env("GEMINI_HOME", dot_gemini.to_string_lossy().as_ref())
        .output()
        .expect("failed to spawn cass index --full");
    if !idx_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full (multi_connector_pipeline)")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(idx_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&idx_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&idx_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass index --full failed", "COMMAND_FAILED").with_context(ctx),
        );
        panic!(
            "cass index --full failed (exit {:?}): {}",
            idx_output.status.code(),
            truncate_output(&idx_output.stderr, 500)
        );
    }
    tracker.end(
        "run_index_full",
        Some("Run full index across all connectors"),
        phase_start,
    );

    // Phase: Search all connectors
    let phase_start = tracker.start(
        "search_all_connectors",
        Some("Search and verify all 5 connector results"),
    );
    let search_start = std::time::Instant::now();
    let output = command_env
        .cass_assert_command()
        .arg("search")
        .arg("user")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .env("HOME", home.to_string_lossy().as_ref())
        .env("XDG_DATA_HOME", xdg_data.to_string_lossy().as_ref())
        .output()
        .expect("failed to execute search");
    let search_duration = search_start.elapsed().as_millis() as u64;

    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass search user --robot")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&output.stderr, 1000)),
            );
        tracker.fail(E2eError::with_type("cass search failed", "COMMAND_FAILED").with_context(ctx));
        panic!(
            "cass search failed (exit {:?}): {}",
            output.status.code(),
            truncate_output(&output.stderr, 500)
        );
    }
    let json_out: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json_out
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    let found_agents: std::collections::HashSet<&str> = hits
        .iter()
        .filter_map(|h| h.get("agent").and_then(|s| s.as_str()))
        .collect();

    assert!(
        found_agents.contains("codex"),
        "Missing codex hit. Found: {found_agents:?}"
    );
    assert!(
        found_agents.contains("claude_code"),
        "Missing claude hit. Found: {found_agents:?}"
    );
    assert!(
        found_agents.contains("gemini"),
        "Missing gemini hit. Found: {found_agents:?}"
    );
    assert!(
        found_agents.contains("cline"),
        "Missing cline hit. Found: {found_agents:?}"
    );
    assert!(
        found_agents.contains("amp"),
        "Missing amp hit. Found: {found_agents:?}"
    );
    tracker.end(
        "search_all_connectors",
        Some("Search and verify all 5 connector results"),
        phase_start,
    );

    tracker.metrics(
        "search_all_connectors",
        &E2ePerformanceMetrics::new()
            .with_duration(search_duration)
            .with_custom("hit_count", serde_json::json!(hits.len()))
            .with_custom("agent_count", serde_json::json!(found_agents.len())),
    );

    // Phase: Incremental index test
    let phase_start = tracker.start(
        "incremental_index",
        Some("Add new file and verify incremental index"),
    );
    std::thread::sleep(std::time::Duration::from_secs(2));

    let sessions = dot_codex.join("sessions/2025/11/22");
    fs::create_dir_all(&sessions).unwrap();

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let content = format!(
        r#"{{"type": "event_msg", "timestamp": {now_ts}, "payload": {{"type": "user_message", "message": "codex_new"}}}}"#
    );
    fs::write(sessions.join("rollout-2.jsonl"), content).unwrap();

    let incr_idx_output = command_env
        .cass_assert_command()
        .arg("index")
        .arg("--data-dir")
        .arg(&data_dir)
        .env("HOME", home.to_string_lossy().as_ref())
        .env("XDG_DATA_HOME", xdg_data.to_string_lossy().as_ref())
        .env("CODEX_HOME", dot_codex.to_string_lossy().as_ref())
        .env("GEMINI_HOME", dot_gemini.to_string_lossy().as_ref())
        .output()
        .expect("failed to spawn cass index (incremental)");
    if !incr_idx_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index (incremental)")
            .capture_cwd()
            .add_state(
                "exit_code",
                serde_json::json!(incr_idx_output.status.code()),
            )
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&incr_idx_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&incr_idx_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass index (incremental) failed", "COMMAND_FAILED")
                .with_context(ctx),
        );
        panic!(
            "cass index (incremental) failed (exit {:?}): {}",
            incr_idx_output.status.code(),
            truncate_output(&incr_idx_output.stderr, 500)
        );
    }

    let output_inc = command_env
        .cass_assert_command()
        .arg("search")
        .arg("codex_new")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .output()
        .expect("failed to execute search");

    let json_inc: serde_json::Value =
        serde_json::from_slice(&output_inc.stdout).expect("valid json");
    let hits_inc = json_inc
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");
    assert!(
        !hits_inc.is_empty(),
        "Incremental index failed to pick up new file"
    );
    assert_eq!(hits_inc[0]["content"], "codex_new");
    tracker.end(
        "incremental_index",
        Some("Add new file and verify incremental index"),
        phase_start,
    );

    // Phase: Agent filter test
    let phase_start = tracker.start(
        "test_agent_filter",
        Some("Verify agent filter isolates results"),
    );
    let filter_start = std::time::Instant::now();
    let output_filter = command_env
        .cass_assert_command()
        .arg("search")
        .arg("user")
        .arg("--agent")
        .arg("claude_code")
        .arg("--robot")
        .arg("--data-dir")
        .arg(&data_dir)
        .output()
        .expect("failed to execute search");
    let filter_duration = filter_start.elapsed().as_millis() as u64;

    let json_filter: serde_json::Value =
        serde_json::from_slice(&output_filter.stdout).expect("valid json");
    let hits_filter = json_filter
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    for hit in hits_filter {
        assert_eq!(hit["agent"], "claude_code");
    }
    assert!(!hits_filter.is_empty());
    tracker.end(
        "test_agent_filter",
        Some("Verify agent filter isolates results"),
        phase_start,
    );

    tracker.metrics(
        "agent_filter_query",
        &E2ePerformanceMetrics::new()
            .with_duration(filter_duration)
            .with_custom("filtered_hit_count", serde_json::json!(hits_filter.len())),
    );

    tracker.complete();
}

// ============================================================================
// Cross-platform multi-connector tests (work on macOS and Linux)
// These tests use Codex and Claude Code which rely on HOME env var
// ============================================================================

/// Creates a Codex session with specific date and content.
fn make_codex_session(
    codex_home: &Path,
    date_path: &str,
    filename: &str,
    content: &str,
    ts_millis: u64,
) {
    let sessions = codex_home.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {ts_millis}, "payload": {{"type": "user_message", "message": "{content}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{content}_response"}}}}"#,
        ts_millis + 1000
    );
    fs::write(file, sample).unwrap();
}

/// Creates a Claude Code session with specific content.
fn make_claude_session(
    claude_home: &Path,
    project_name: &str,
    filename: &str,
    content: &str,
    ts_iso: &str,
) {
    let project = claude_home.join(format!("projects/{project_name}"));
    fs::create_dir_all(&project).unwrap();
    let file = project.join(filename);
    let sample = format!(
        r#"{{"type": "user", "timestamp": "{ts_iso}", "message": {{"role": "user", "content": "{content}"}}}}
{{"type": "assistant", "timestamp": "{ts_iso}", "message": {{"role": "assistant", "content": "{content}_response"}}}}"#
    );
    fs::write(file, sample).unwrap();
}

/// Test: Multiple connectors can be indexed and searched together
#[test]
fn multi_connector_codex_and_claude() {
    let tracker = tracker_for("multi_connector_codex_and_claude");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    // Phase: Create fixtures
    let phase_start = tracker.start("setup_fixtures", Some("Create Codex and Claude sessions"));
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-multi.jsonl",
        "multitest codex_unique_content",
        1732118400000,
    );
    make_claude_session(
        &claude_home,
        "multi-project",
        "session-multi.jsonl",
        "multitest claude_unique_content",
        "2024-11-20T10:00:00Z",
    );
    tracker.end(
        "setup_fixtures",
        Some("Create Codex and Claude sessions"),
        phase_start,
    );

    // Phase: Index
    let phase_start = tracker.start("run_index", Some("Run full index"));
    let idx_output = command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("failed to spawn cass index --full");
    if !idx_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full (codex_and_claude)")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(idx_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&idx_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&idx_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass index --full failed", "COMMAND_FAILED").with_context(ctx),
        );
        panic!(
            "cass index --full failed (exit {:?}): {}",
            idx_output.status.code(),
            truncate_output(&idx_output.stderr, 500)
        );
    }
    tracker.end("run_index", Some("Run full index"), phase_start);

    // Phase: Search and verify
    let phase_start = tracker.start(
        "search_multi_connector",
        Some("Search shared term across connectors"),
    );
    let search_start = std::time::Instant::now();
    let output = command_env
        .cass_assert_command()
        .args(["search", "multitest", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");
    let search_duration = search_start.elapsed().as_millis() as u64;

    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass search multitest --robot")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&output.stderr, 1000)),
            );
        tracker.fail(E2eError::with_type("cass search failed", "COMMAND_FAILED").with_context(ctx));
        panic!(
            "cass search failed (exit {:?}): {}",
            output.status.code(),
            truncate_output(&output.stderr, 500)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    let agents: std::collections::HashSet<_> =
        hits.iter().filter_map(|h| h["agent"].as_str()).collect();

    assert!(
        agents.contains("codex"),
        "Should find codex results. Agents found: {agents:?}"
    );
    assert!(
        agents.contains("claude_code"),
        "Should find claude_code results. Agents found: {agents:?}"
    );
    assert!(
        hits.len() >= 2,
        "Should have at least 2 hits from different connectors"
    );
    tracker.end(
        "search_multi_connector",
        Some("Search shared term across connectors"),
        phase_start,
    );

    tracker.metrics(
        "search_multi_connector",
        &E2ePerformanceMetrics::new()
            .with_duration(search_duration)
            .with_custom("hit_count", serde_json::json!(hits.len()))
            .with_custom("agent_count", serde_json::json!(agents.len())),
    );

    tracker.complete();
}

/// Test: Agent filter isolates results to specific connector
#[test]
fn multi_connector_agent_filter_isolation() {
    let tracker = tracker_for("multi_connector_agent_filter_isolation");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    // Phase: Setup
    let phase_start = tracker.start(
        "setup_fixtures",
        Some("Create sessions with shared search term"),
    );
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-iso.jsonl",
        "isolationtest codex_data",
        1732118400000,
    );
    make_claude_session(
        &claude_home,
        "iso-project",
        "session-iso.jsonl",
        "isolationtest claude_data",
        "2024-11-20T10:00:00Z",
    );
    tracker.end(
        "setup_fixtures",
        Some("Create sessions with shared search term"),
        phase_start,
    );

    // Phase: Index
    let phase_start = tracker.start("run_index", Some("Run full index"));
    let idx_output = command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("failed to spawn cass index --full");
    if !idx_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full (agent_filter_isolation)")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(idx_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&idx_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&idx_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass index --full failed", "COMMAND_FAILED").with_context(ctx),
        );
        panic!(
            "cass index --full failed (exit {:?}): {}",
            idx_output.status.code(),
            truncate_output(&idx_output.stderr, 500)
        );
    }
    tracker.end("run_index", Some("Run full index"), phase_start);

    // Phase: Filter by codex
    let phase_start = tracker.start("filter_codex", Some("Search with agent=codex filter"));
    let codex_start = std::time::Instant::now();
    let codex_output = command_env
        .cass_assert_command()
        .args([
            "search",
            "isolationtest",
            "--agent",
            "codex",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");
    let codex_duration = codex_start.elapsed().as_millis() as u64;

    if !codex_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass search isolationtest --agent codex --robot")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(codex_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&codex_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&codex_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass search --agent codex failed", "COMMAND_FAILED")
                .with_context(ctx),
        );
        panic!(
            "cass search --agent codex failed (exit {:?}): {}",
            codex_output.status.code(),
            truncate_output(&codex_output.stderr, 500)
        );
    }
    let codex_json: serde_json::Value =
        serde_json::from_slice(&codex_output.stdout).expect("valid json");
    let codex_hits = codex_json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(!codex_hits.is_empty(), "Should find codex hits");
    for hit in codex_hits {
        assert_eq!(
            hit["agent"], "codex",
            "All hits should be from codex when filtering"
        );
    }
    tracker.end(
        "filter_codex",
        Some("Search with agent=codex filter"),
        phase_start,
    );

    tracker.metrics(
        "filter_codex",
        &E2ePerformanceMetrics::new()
            .with_duration(codex_duration)
            .with_custom("hit_count", serde_json::json!(codex_hits.len())),
    );

    // Phase: Filter by claude_code
    let phase_start = tracker.start(
        "filter_claude",
        Some("Search with agent=claude_code filter"),
    );
    let claude_start = std::time::Instant::now();
    let claude_output = command_env
        .cass_assert_command()
        .args([
            "search",
            "isolationtest",
            "--agent",
            "claude_code",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");
    let claude_duration = claude_start.elapsed().as_millis() as u64;

    if !claude_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass search isolationtest --agent claude_code --robot")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(claude_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&claude_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&claude_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass search --agent claude_code failed", "COMMAND_FAILED")
                .with_context(ctx),
        );
        panic!(
            "cass search --agent claude_code failed (exit {:?}): {}",
            claude_output.status.code(),
            truncate_output(&claude_output.stderr, 500)
        );
    }
    let claude_json: serde_json::Value =
        serde_json::from_slice(&claude_output.stdout).expect("valid json");
    let claude_hits = claude_json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(!claude_hits.is_empty(), "Should find claude_code hits");
    for hit in claude_hits {
        assert_eq!(
            hit["agent"], "claude_code",
            "All hits should be from claude_code when filtering"
        );
    }
    tracker.end(
        "filter_claude",
        Some("Search with agent=claude_code filter"),
        phase_start,
    );

    tracker.metrics(
        "filter_claude",
        &E2ePerformanceMetrics::new()
            .with_duration(claude_duration)
            .with_custom("hit_count", serde_json::json!(claude_hits.len())),
    );

    tracker.complete();
}

/// Test: Each connector's unique content is properly indexed
#[test]
fn multi_connector_unique_content() {
    let tracker = tracker_for("multi_connector_unique_content");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    // Phase: Setup
    let phase_start = tracker.start(
        "setup_fixtures",
        Some("Create sessions with unique content per connector"),
    );
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-unique.jsonl",
        "codexonly_xyzzy uniqueterm",
        1732118400000,
    );
    make_claude_session(
        &claude_home,
        "unique-project",
        "session-unique.jsonl",
        "claudeonly_plugh uniqueterm",
        "2024-11-20T10:00:00Z",
    );
    tracker.end(
        "setup_fixtures",
        Some("Create sessions with unique content per connector"),
        phase_start,
    );

    // Phase: Index
    let phase_start = tracker.start("run_index", Some("Run full index"));
    let idx_output = command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("failed to spawn cass index --full");
    if !idx_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full (unique_content)")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(idx_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&idx_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&idx_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass index --full failed", "COMMAND_FAILED").with_context(ctx),
        );
        panic!(
            "cass index --full failed (exit {:?}): {}",
            idx_output.status.code(),
            truncate_output(&idx_output.stderr, 500)
        );
    }
    tracker.end("run_index", Some("Run full index"), phase_start);

    // Phase: Search codex-specific content
    let phase_start = tracker.start(
        "search_codex_unique",
        Some("Search for codex-specific term"),
    );
    let codex_output = command_env
        .cass_assert_command()
        .args(["search", "codexonly_xyzzy", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    if !codex_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass search codexonly_xyzzy --robot")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(codex_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&codex_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&codex_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass search codexonly_xyzzy failed", "COMMAND_FAILED")
                .with_context(ctx),
        );
        panic!(
            "cass search failed (exit {:?}): {}",
            codex_output.status.code(),
            truncate_output(&codex_output.stderr, 500)
        );
    }
    let codex_json: serde_json::Value =
        serde_json::from_slice(&codex_output.stdout).expect("valid json");
    let codex_hits = codex_json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(!codex_hits.is_empty(), "Should find codex-specific content");
    assert!(
        codex_hits.iter().all(|h| h["agent"] == "codex"),
        "Codex-specific search should only return codex results"
    );
    tracker.end(
        "search_codex_unique",
        Some("Search for codex-specific term"),
        phase_start,
    );

    // Phase: Search claude-specific content
    let phase_start = tracker.start(
        "search_claude_unique",
        Some("Search for claude-specific term"),
    );
    let claude_output = command_env
        .cass_assert_command()
        .args(["search", "claudeonly_plugh", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    if !claude_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass search claudeonly_plugh --robot")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(claude_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&claude_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&claude_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass search claudeonly_plugh failed", "COMMAND_FAILED")
                .with_context(ctx),
        );
        panic!(
            "cass search failed (exit {:?}): {}",
            claude_output.status.code(),
            truncate_output(&claude_output.stderr, 500)
        );
    }
    let claude_json: serde_json::Value =
        serde_json::from_slice(&claude_output.stdout).expect("valid json");
    let claude_hits = claude_json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        !claude_hits.is_empty(),
        "Should find claude-specific content"
    );
    assert!(
        claude_hits.iter().all(|h| h["agent"] == "claude_code"),
        "Claude-specific search should only return claude_code results"
    );
    tracker.end(
        "search_claude_unique",
        Some("Search for claude-specific term"),
        phase_start,
    );

    tracker.complete();
}

/// Test: Aggregation by agent works with multiple connectors
#[test]
fn multi_connector_aggregation() {
    let tracker = tracker_for("multi_connector_aggregation");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    // Phase: Setup
    let phase_start = tracker.start(
        "setup_fixtures",
        Some("Create multiple sessions per connector"),
    );
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-agg1.jsonl",
        "aggtest codex_first",
        1732118400000,
    );
    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-agg2.jsonl",
        "aggtest codex_second",
        1732204800000,
    );
    make_claude_session(
        &claude_home,
        "agg-project1",
        "session-agg1.jsonl",
        "aggtest claude_first",
        "2024-11-20T10:00:00Z",
    );
    make_claude_session(
        &claude_home,
        "agg-project2",
        "session-agg2.jsonl",
        "aggtest claude_second",
        "2024-11-21T10:00:00Z",
    );
    tracker.end(
        "setup_fixtures",
        Some("Create multiple sessions per connector"),
        phase_start,
    );

    // Phase: Index
    let phase_start = tracker.start("run_index", Some("Run full index"));
    let idx_output = command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("failed to spawn cass index --full");
    if !idx_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full (aggregation)")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(idx_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&idx_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&idx_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass index --full failed", "COMMAND_FAILED").with_context(ctx),
        );
        panic!(
            "cass index --full failed (exit {:?}): {}",
            idx_output.status.code(),
            truncate_output(&idx_output.stderr, 500)
        );
    }
    tracker.end("run_index", Some("Run full index"), phase_start);

    // Phase: Aggregation search
    let phase_start = tracker.start("search_aggregate", Some("Search with agent aggregation"));
    let agg_start = std::time::Instant::now();
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "aggtest",
            "--aggregate",
            "agent",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");
    let agg_duration = agg_start.elapsed().as_millis() as u64;

    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass search aggtest --aggregate agent --robot")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass search --aggregate failed", "COMMAND_FAILED")
                .with_context(ctx),
        );
        panic!(
            "cass search --aggregate failed (exit {:?}): {}",
            output.status.code(),
            truncate_output(&output.stderr, 500)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");

    let aggregations = json.get("aggregations").and_then(|a| a.as_object());
    assert!(
        aggregations.is_some(),
        "Should have aggregations in response"
    );

    let aggs = aggregations.unwrap();
    let agent_agg = aggs.get("agent").and_then(|a| a.as_object());
    assert!(agent_agg.is_some(), "Should have agent aggregation");

    let buckets = agent_agg
        .unwrap()
        .get("buckets")
        .and_then(|b| b.as_array())
        .expect("Should have buckets array");

    let agent_keys: std::collections::HashSet<_> = buckets
        .iter()
        .filter_map(|b| b.get("key").and_then(|k| k.as_str()))
        .collect();

    assert!(
        agent_keys.contains("codex"),
        "Agent aggregation should include codex. Keys: {agent_keys:?}"
    );
    assert!(
        agent_keys.contains("claude_code"),
        "Agent aggregation should include claude_code. Keys: {agent_keys:?}"
    );
    tracker.end(
        "search_aggregate",
        Some("Search with agent aggregation"),
        phase_start,
    );

    tracker.metrics(
        "aggregation_query",
        &E2ePerformanceMetrics::new()
            .with_duration(agg_duration)
            .with_custom("bucket_count", serde_json::json!(buckets.len())),
    );

    tracker.complete();
}

/// Test: Incremental indexing works across multiple connectors
#[test]
fn multi_connector_incremental_index() {
    let tracker = tracker_for("multi_connector_incremental_index");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    // Phase: Create initial sessions
    let phase_start = tracker.start(
        "setup_initial_fixtures",
        Some("Create initial sessions for both connectors"),
    );
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-incr1.jsonl",
        "incrtest initial_codex",
        1732118400000,
    );
    make_claude_session(
        &claude_home,
        "incr-project1",
        "session-incr1.jsonl",
        "incrtest initial_claude",
        "2024-11-20T10:00:00Z",
    );
    tracker.end(
        "setup_initial_fixtures",
        Some("Create initial sessions for both connectors"),
        phase_start,
    );

    // Phase: Full index
    let phase_start = tracker.start("run_full_index", Some("Run full index"));
    let idx_output = command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("failed to spawn cass index --full");
    if !idx_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full (incremental_index)")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(idx_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&idx_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&idx_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass index --full failed", "COMMAND_FAILED").with_context(ctx),
        );
        panic!(
            "cass index --full failed (exit {:?}): {}",
            idx_output.status.code(),
            truncate_output(&idx_output.stderr, 500)
        );
    }
    tracker.end("run_full_index", Some("Run full index"), phase_start);

    // Phase: Verify initial index
    let phase_start = tracker.start(
        "verify_initial_index",
        Some("Verify initial sessions indexed"),
    );
    let output1 = command_env
        .cass_assert_command()
        .args(["search", "incrtest", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    let json1: serde_json::Value = serde_json::from_slice(&output1.stdout).expect("valid json");
    let hits1 = json1
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");
    assert!(hits1.len() >= 2, "Should have initial sessions indexed");
    tracker.end(
        "verify_initial_index",
        Some("Verify initial sessions indexed"),
        phase_start,
    );

    // Phase: Add new sessions and run incremental index
    let phase_start = tracker.start(
        "incremental_index",
        Some("Add new sessions and run incremental index"),
    );
    std::thread::sleep(std::time::Duration::from_secs(2));

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let now_iso = chrono::Utc::now().to_rfc3339();

    make_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-incr2.jsonl",
        "incrtest new_codex",
        now_ts,
    );
    make_claude_session(
        &claude_home,
        "incr-project2",
        "session-incr2.jsonl",
        "incrtest new_claude",
        &now_iso,
    );

    let incr_start = std::time::Instant::now();
    let incr_idx_output = command_env
        .cass_assert_command()
        .args(["index", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("failed to spawn cass index (incremental)");
    if !incr_idx_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index (incremental)")
            .capture_cwd()
            .add_state(
                "exit_code",
                serde_json::json!(incr_idx_output.status.code()),
            )
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&incr_idx_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&incr_idx_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass index (incremental) failed", "COMMAND_FAILED")
                .with_context(ctx),
        );
        panic!(
            "cass index (incremental) failed (exit {:?}): {}",
            incr_idx_output.status.code(),
            truncate_output(&incr_idx_output.stderr, 500)
        );
    }
    let incr_duration = incr_start.elapsed().as_millis() as u64;
    tracker.end(
        "incremental_index",
        Some("Add new sessions and run incremental index"),
        phase_start,
    );

    tracker.metrics(
        "incremental_index",
        &E2ePerformanceMetrics::new().with_duration(incr_duration),
    );

    // Phase: Verify incremental results
    let phase_start = tracker.start(
        "verify_incremental",
        Some("Verify all sessions indexed after incremental"),
    );
    let output2 = command_env
        .cass_assert_command()
        .args(["search", "incrtest", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    let json2: serde_json::Value = serde_json::from_slice(&output2.stdout).expect("valid json");
    let hits2 = json2
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(
        hits2.len() > hits1.len(),
        "Incremental index should add new sessions. hits1={}, hits2={}",
        hits1.len(),
        hits2.len()
    );

    let has_initial = hits2
        .iter()
        .any(|h| h["content"].as_str().unwrap_or("").contains("initial"));
    let has_new = hits2
        .iter()
        .any(|h| h["content"].as_str().unwrap_or("").contains("new"));

    assert!(
        has_initial,
        "Should still have initial sessions after incremental index"
    );
    assert!(has_new, "Should have new sessions after incremental index");
    tracker.end(
        "verify_incremental",
        Some("Verify all sessions indexed after incremental"),
        phase_start,
    );

    tracker.metrics(
        "incremental_results",
        &E2ePerformanceMetrics::new()
            .with_custom("initial_hit_count", serde_json::json!(hits1.len()))
            .with_custom("final_hit_count", serde_json::json!(hits2.len())),
    );

    tracker.complete();
}

/// Test: Multiple agent filter works correctly
#[test]
fn multi_connector_multiple_agent_filter() {
    let tracker = tracker_for("multi_connector_multiple_agent_filter");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let claude_home = home.join(".claude");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    // Phase: Setup
    let phase_start = tracker.start(
        "setup_fixtures",
        Some("Create sessions for both connectors"),
    );
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-maf.jsonl",
        "multiagent codex_content",
        1732118400000,
    );
    make_claude_session(
        &claude_home,
        "multi-agent-project",
        "session-maf.jsonl",
        "multiagent claude_content",
        "2024-11-20T10:00:00Z",
    );
    tracker.end(
        "setup_fixtures",
        Some("Create sessions for both connectors"),
        phase_start,
    );

    // Phase: Index
    let phase_start = tracker.start("run_index", Some("Run full index"));
    let idx_output = command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("failed to spawn cass index --full");
    if !idx_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full (multiple_agent_filter)")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(idx_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&idx_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&idx_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass index --full failed", "COMMAND_FAILED").with_context(ctx),
        );
        panic!(
            "cass index --full failed (exit {:?}): {}",
            idx_output.status.code(),
            truncate_output(&idx_output.stderr, 500)
        );
    }
    tracker.end("run_index", Some("Run full index"), phase_start);

    // Phase: Multi-agent filter search
    let phase_start = tracker.start(
        "search_multi_agent_filter",
        Some("Search with multiple --agent filters"),
    );
    let search_start = std::time::Instant::now();
    let output = command_env
        .cass_assert_command()
        .args([
            "search",
            "multiagent",
            "--agent",
            "codex",
            "--agent",
            "claude_code",
            "--robot",
            "--data-dir",
        ])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");
    let search_duration = search_start.elapsed().as_millis() as u64;

    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass search multiagent --agent codex --agent claude_code --robot")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass search --agent multi failed", "COMMAND_FAILED")
                .with_context(ctx),
        );
        panic!(
            "cass search --agent multi failed (exit {:?}): {}",
            output.status.code(),
            truncate_output(&output.stderr, 500)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    let agents: std::collections::HashSet<_> =
        hits.iter().filter_map(|h| h["agent"].as_str()).collect();

    assert!(
        agents.contains("codex") && agents.contains("claude_code"),
        "Should find results from both specified agents. Found: {agents:?}"
    );
    tracker.end(
        "search_multi_agent_filter",
        Some("Search with multiple --agent filters"),
        phase_start,
    );

    tracker.metrics(
        "multi_agent_filter_query",
        &E2ePerformanceMetrics::new()
            .with_duration(search_duration)
            .with_custom("hit_count", serde_json::json!(hits.len()))
            .with_custom("agent_count", serde_json::json!(agents.len())),
    );

    tracker.complete();
}

/// Test: Empty connector doesn't break indexing of other connectors
#[test]
fn multi_connector_empty_connector() {
    let tracker = tracker_for("multi_connector_empty_connector");
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");

    fs::create_dir_all(&data_dir).unwrap();

    let command_env = tracker
        .command_environment()
        .with_home(home)
        .with_codex_home(&codex_home);

    // Phase: Setup (only codex, no claude)
    let phase_start = tracker.start(
        "setup_fixtures",
        Some("Create only Codex session, no Claude"),
    );
    make_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-only.jsonl",
        "singleconnector codex_only",
        1732118400000,
    );
    tracker.end(
        "setup_fixtures",
        Some("Create only Codex session, no Claude"),
        phase_start,
    );

    // Phase: Index with missing connector
    let phase_start = tracker.start("run_index", Some("Index with non-existent claude_home"));
    let idx_output = command_env
        .cass_assert_command()
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .output()
        .expect("failed to spawn cass index --full");
    if !idx_output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass index --full (empty_connector)")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(idx_output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&idx_output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&idx_output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass index --full failed", "COMMAND_FAILED").with_context(ctx),
        );
        panic!(
            "cass index --full failed (exit {:?}): {}",
            idx_output.status.code(),
            truncate_output(&idx_output.stderr, 500)
        );
    }
    tracker.end(
        "run_index",
        Some("Index with non-existent claude_home"),
        phase_start,
    );

    // Phase: Search and verify
    let phase_start = tracker.start(
        "verify_results",
        Some("Search and verify codex-only results"),
    );
    let output = command_env
        .cass_assert_command()
        .args(["search", "singleconnector", "--robot", "--data-dir"])
        .arg(&data_dir)
        .env("HOME", home)
        .output()
        .expect("search command");

    if !output.status.success() {
        let ctx = E2eErrorContext::new()
            .with_command("cass search singleconnector --robot")
            .capture_cwd()
            .add_state("exit_code", serde_json::json!(output.status.code()))
            .add_state(
                "stdout_tail",
                serde_json::json!(truncate_output(&output.stdout, 1000)),
            )
            .add_state(
                "stderr_tail",
                serde_json::json!(truncate_output(&output.stderr, 1000)),
            );
        tracker.fail(
            E2eError::with_type("cass search singleconnector failed", "COMMAND_FAILED")
                .with_context(ctx),
        );
        panic!(
            "cass search failed (exit {:?}): {}",
            output.status.code(),
            truncate_output(&output.stderr, 500)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let hits = json
        .get("hits")
        .and_then(|h| h.as_array())
        .expect("hits array");

    assert!(!hits.is_empty(), "Should find codex results");
    assert!(
        hits.iter().all(|h| h["agent"] == "codex"),
        "All results should be from codex"
    );
    tracker.end(
        "verify_results",
        Some("Search and verify codex-only results"),
        phase_start,
    );

    tracker.complete();
}

/// GH #511: watch-once uses the same callback contract as initial indexing.
/// A failed transcript must retain good chats, report the actual error even
/// when nothing parses, and leave an older repaired source eligible for retry.
#[test]
fn gh511_watch_once_retains_good_chats_and_retries_failed_sources() {
    use serde_json::{Value, json};

    for with_good_chat in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let data = tmp.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let truncated = gh511_codebuff_chat(&home, "a-busy-project", &json!([]));
        fs::write(&truncated, br#"[{"id":"user-1774113351457""#).unwrap();
        let good =
            with_good_chat.then(|| gh511_codebuff_chat(&home, "z-good", &gh511_native_records()));
        let modified_before_failure =
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_774_113_351);
        let source_paths = [truncated.clone()]
            .into_iter()
            .chain(good.iter().cloned())
            .flat_map(|path| [path.with_file_name("run-state.json"), path]);
        let before: Vec<_> = source_paths
            .map(|path| {
                fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(modified_before_failure)
                    .unwrap();
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        let projects = home.join(".config/manicode/projects");
        let output = gh511_cass(&home, &data)
            .args(["index", "--watch", "--watch-once"])
            .arg(&projects)
            .args(["--json", "--no-progress-events"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(9),
            "with_good_chat={with_good_chat}: {output:?}"
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        gh511_assert_parse_diagnostic(&report, &data, &truncated);
        assert_eq!(report["success"], false, "{report}");
        assert_eq!(report["partial"], true, "{report}");
        assert_eq!(report["coverage_status"], "incomplete", "{report}");
        let connector = gh511_connector_stats(&report);
        assert_eq!(
            connector["conversations"],
            usize::from(with_good_chat),
            "{report}"
        );
        assert_eq!(
            connector["messages"],
            2 * usize::from(with_good_chat),
            "{report}"
        );
        assert!(
            connector["error"]
                .as_str()
                .unwrap()
                .contains("invalid Codebuff / Freebuff transcript JSON"),
            "{connector}"
        );
        assert!(!data.join("watch_state.json").exists());
        if let Some(good) = &good {
            gh511_assert_native_messages(&home, &data, good);
        }
        for (path, bytes) in before {
            assert_eq!(fs::read(&path).unwrap(), bytes, "{}", path.display());
        }

        fs::write(
            &truncated,
            serde_json::to_vec(&json!([
                {"id":"user-1774113471457", "variant":"user", "content":"Synthetic watch recovered question",
                 "timestamp":"01:17 PM"}
            ]))
            .unwrap(),
        )
        .unwrap();
        fs::File::options()
            .write(true)
            .open(&truncated)
            .unwrap()
            .set_modified(modified_before_failure)
            .unwrap();
        let retry = gh511_cass(&home, &data)
            .args(["index", "--watch", "--watch-once"])
            .arg(&projects)
            .args(["--json", "--no-progress-events"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let retry: Value = serde_json::from_slice(&retry).unwrap();
        assert_eq!(retry["indexing_stats"]["scan_had_errors"], false, "{retry}");
        assert!(
            gh511_connector_stats(&retry).get("error").is_none(),
            "{retry}"
        );
        assert_ne!(retry["partial"], true, "{retry}");
        assert!(!data.join("watch_state.json").exists());
        let recovered = gh511_message_hit(
            &home,
            &data,
            "recovered",
            "Synthetic watch recovered question",
        );
        assert_eq!(recovered["created_at"].as_i64(), Some(1_774_113_471_457));
        assert_eq!(recovered["workspace"], "/synthetic/probe");
        if let Some(good) = &good {
            gh511_assert_native_messages(&home, &data, good);
        }
    }
}
