//! Real-binary logical recovery: receipt parity, privacy, idempotence and conflicts.

use std::fs;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

use assert_cmd::Command;
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde_json::Value;

fn command(home: &Path) -> Command {
    // Bind this test to Cargo's freshly built executable, not a PATH installation
    // or an external CARGO_BIN_EXE_cass override containing a stale binary.
    let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("CASS_DATA_DIR", home.join("unused-default"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_VIEW_BUDGET_MS", "30000")
        .env_remove("CASS_TEST_VIEW_SLOW_MS")
        .env_remove("CASS_OUTPUT_FORMAT")
        .env_remove("TOON_DEFAULT_FORMAT")
        .timeout(Duration::from_secs(90));
    command
}

fn receipt(output: Output) -> Value {
    assert!(
        output.status.success(),
        "archive command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one JSON success receipt")
}

fn export(home: &Path, source: &Path, output: &Path) -> Value {
    receipt(
        command(home)
            .args(["archive", "export", "--db"])
            .arg(source)
            .args([
                "--archive-id",
                "cli-archive",
                "--include-private",
                "--output",
            ])
            .arg(output)
            .output()
            .unwrap(),
    )
}

fn import(home: &Path, input: &Path, output: &Path, identical: bool) -> Output {
    let mut cmd = command(home);
    cmd.args(["archive", "import"])
        .arg(input)
        .args([
            "--archive-id",
            "cli-archive",
            "--include-private",
            "--output",
        ])
        .arg(output);
    if identical {
        cmd.arg("--if-identical");
    }
    cmd.output().unwrap()
}

#[test]
fn real_binary_round_trip_and_repeat_import_have_truthful_json_receipts() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.db");
    drop(SqliteStorage::open(&source).unwrap());
    let input = root.path().join("history.jsonl");
    let exported = export(root.path(), &source, &input);
    let target = root.path().join("restored.db");
    let imported = receipt(import(root.path(), &input, &target, false));
    assert_eq!(imported["operation"], "import");
    assert_eq!(imported["destination_status"], "created");
    assert_eq!(imported["content_sha256"], exported["content_sha256"]);
    assert_eq!(imported["tables"], exported["tables"]);
    assert_eq!(
        imported["derived_search_assets"],
        "omitted_rebuild_required"
    );
    let before = fs::read(&target).unwrap();
    let repeated = receipt(import(root.path(), &input, &target, true));
    assert_eq!(repeated["destination_status"], "unchanged");
    assert_eq!(repeated["content_sha256"], exported["content_sha256"]);
    assert_eq!(before, fs::read(&target).unwrap());
    let after = root.path().join("restored.jsonl");
    assert_eq!(
        export(root.path(), &target, &after)["content_sha256"],
        exported["content_sha256"]
    );
    assert!(!root.path().join("unused-default").exists());
    assert!(!root.path().join("data/cass/models").exists());
}

#[test]
fn restored_remote_sessions_are_readable_without_the_original_archive_or_sources() {
    let root = tempfile::tempdir().unwrap();
    let source_directory = root.path().join("original");
    fs::create_dir(&source_directory).unwrap();
    let source = source_directory.join("agent_search.db");
    let stale_path = root.path().join("vanished/provider/session.jsonl");
    assert!(!stale_path.exists());
    let storage = SqliteStorage::open(&source).unwrap();
    let mut identities = Vec::new();
    for (agent, source_id, target) in [
        ("claude_code", "remote-a", "restored claude evidence δ"),
        ("codex", "remote-b", "restored codex evidence 日本語"),
    ] {
        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: agent.to_owned(),
                name: agent.to_owned(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();
        let external_id = format!("portable-{source_id}");
        storage
            .insert_conversation_tree(agent_id, None, &Conversation {
                id: None,
                agent_slug: agent.to_owned(),
                workspace: None,
                external_id: Some(external_id.clone()),
                title: Some(format!("Recovered {source_id}")),
                source_path: stale_path.clone(),
                started_at: Some(1_733_000_000_000),
                ended_at: None,
                approx_tokens: None,
                metadata_json: serde_json::json!({"provider_origin": source_id}),
                // More than one private replay batch, with a sparse final ordinal.
                messages: (0..130)
                    .map(|position| Message {
                        id: None,
                        idx: if position == 129 { 1024 } else { position },
                        role: MessageRole::Agent,
                        author: Some(agent.to_owned()),
                        created_at: Some(1_733_000_000_000 + position),
                        content: if position == 129 {
                            target.to_owned()
                        } else {
                            format!("{source_id} retained neighbour {position}")
                        },
                        extra_json: serde_json::json!({"provider_usage": {"input_tokens": position}}),
                        snippets: Vec::new(),
                    })
                    .collect(),
                source_id: source_id.to_owned(),
                origin_host: Some(source_id.to_owned()),
            })
            .unwrap();
        let ids: Vec<i64> = storage
            .raw()
            .query_map_collect(
                "SELECT id FROM conversations WHERE external_id = ?1",
                coding_agent_search::franken_sync::params![external_id.as_str()],
                |row| row.get_typed(0),
            )
            .unwrap();
        assert_eq!(ids.len(), 1);
        identities.push((source_id, ids[0], target));
    }
    drop(storage);
    let input = root.path().join("portable.jsonl");
    let exported = export(root.path(), &source, &input);
    assert_eq!(exported["tables"]["messages"], 260);
    let input_bytes = fs::read(&input).unwrap();
    // Preserve the source files, but make their original names unavailable.
    fs::rename(&source_directory, root.path().join("retired-source")).unwrap();
    assert!(!source.exists());
    let restored = root.path().join("restored.db");
    let imported = receipt(import(root.path(), &input, &restored, false));
    assert_eq!(imported["content_sha256"], exported["content_sha256"]);
    let database_bytes = fs::read(&restored).unwrap();
    for (source_id, cid, target) in identities {
        for operation in ["view", "expand"] {
            let payload = receipt(
                command(root.path())
                    .arg("--db")
                    .arg(&restored)
                    .arg(operation)
                    .arg(&stale_path)
                    .args([
                        "--source",
                        source_id,
                        "--conversation-id",
                        &cid.to_string(),
                        "--message-index",
                        "1025",
                        "-C",
                        "0",
                        "--json",
                    ])
                    .output()
                    .unwrap(),
            );
            let rows = if operation == "view" {
                &payload["lines"]
            } else {
                &payload
            };
            let rows = rows.as_array().expect("canonical follow-up rows");
            assert_eq!(rows.len(), 1, "{payload}");
            assert_eq!(rows[0]["content"], target);
            assert_eq!(rows[0]["source_id"], source_id);
            assert_eq!(rows[0]["conversation_id"], cid);
            assert_eq!(rows[0]["message_index"], 1025);
            assert_eq!(rows[0]["content_source"], "archive");
        }
    }
    assert!(!source.exists());
    assert!(!stale_path.exists());
    assert_eq!(fs::read(&input).unwrap(), input_bytes);
    assert_eq!(fs::read(&restored).unwrap(), database_bytes);
    assert_eq!(
        receipt(import(root.path(), &input, &restored, true))["destination_status"],
        "unchanged"
    );
    assert_eq!(
        export(
            root.path(),
            &restored,
            &root.path().join("after-followup.jsonl")
        )["content_sha256"],
        exported["content_sha256"]
    );
}

#[test]
fn real_binary_requires_privacy_acknowledgement_and_never_overwrites_conflicts() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.db");
    drop(SqliteStorage::open(&source).unwrap());
    let input = root.path().join("history.jsonl");
    export(root.path(), &source, &input);
    let target = root.path().join("restored.db");
    let output = command(root.path())
        .args(["archive", "import"])
        .arg(&input)
        .args(["--archive-id", "cli-archive", "--output"])
        .arg(&target)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(serde_json::from_slice::<Value>(&output.stderr).is_ok());
    assert!(!target.exists());

    receipt(import(root.path(), &input, &target, false));
    let writer = Connection::open(target.to_str().unwrap()).unwrap();
    writer
        .execute("INSERT INTO meta (key, value) VALUES ('private_note', 'SECRET-DO-NOT-ECHO')")
        .unwrap();
    writer.close().unwrap();
    let before = fs::read(&target).unwrap();
    for identical in [false, true] {
        let output = import(root.path(), &input, &target, identical);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(serde_json::from_slice::<Value>(&output.stderr).is_ok());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("SECRET-DO-NOT-ECHO"));
        assert_eq!(before, fs::read(&target).unwrap());
    }
}

/// Bead ukg62: every archive failure used to exit 2 ("usage, do not retry")
/// with one kind, so a digest mismatch and a missing file looked like typos.
/// Each class now has its own exit code, kind and retryability, chosen by
/// error type; a clean export/verify round trip still exits 0.
#[test]
fn real_binary_failure_classes_have_distinct_exit_codes_and_kinds() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path();
    let source = home.join("source.db");
    drop(SqliteStorage::open(&source).unwrap());
    let failure = |args: &[&std::ffi::OsStr]| -> (i32, String, bool) {
        let output = command(home).arg("archive").args(args).output().unwrap();
        assert!(output.stdout.is_empty(), "failures write no receipt");
        let payload: Value = serde_json::from_slice(&output.stderr).expect("one JSON error");
        let error = &payload["error"];
        let code = output.status.code().unwrap();
        assert_eq!(error["code"], code, "{payload}");
        (
            code,
            error["kind"].as_str().unwrap().to_owned(),
            error["retryable"].as_bool().unwrap(),
        )
    };
    let usage = (2, "logical-archive-usage".to_owned(), false);

    // Usage: a missing required flag, and a missing privacy acknowledgement.
    let target = home.join("usage.jsonl");
    assert_eq!(
        failure(&[
            "export".as_ref(),
            "--db".as_ref(),
            source.as_os_str(),
            "--include-private".as_ref(),
            "--output".as_ref(),
            target.as_os_str(),
        ]),
        usage
    );
    assert_eq!(
        failure(&[
            "export".as_ref(),
            "--db".as_ref(),
            source.as_os_str(),
            "--archive-id".as_ref(),
            "cli-archive".as_ref(),
            "--output".as_ref(),
            target.as_os_str(),
        ]),
        usage
    );
    assert!(!target.exists());

    // Success: a clean round trip.
    let input = home.join("history.jsonl");
    export(home, &source, &input);
    receipt(
        command(home)
            .args(["archive", "verify"])
            .arg(&input)
            .output()
            .unwrap(),
    );

    // Integrity: one flipped digest character in an otherwise valid export.
    let text = fs::read_to_string(&input).unwrap();
    let marker = "\"content_sha256\":\"";
    let at = text.rfind(marker).unwrap() + marker.len();
    let mut bytes = text.into_bytes();
    bytes[at] = if bytes[at] == b'0' { b'1' } else { b'0' };
    let corrupt = home.join("corrupt.jsonl");
    fs::write(&corrupt, bytes).unwrap();
    assert_eq!(
        failure(&["verify".as_ref(), corrupt.as_os_str()]),
        (5, "logical-archive-integrity".to_owned(), false)
    );

    // I/O: the input does not exist. This is not an integrity verdict.
    let io = (14, "logical-archive-io".to_owned(), true);
    let missing = home.join("missing.jsonl");
    assert_eq!(failure(&["verify".as_ref(), missing.as_os_str()]), io);

    // Busy: another holder keeps the destination lock past the five-second wait.
    let busy = home.join("busy.jsonl");
    let lock = fs::File::create(home.join(".busy.jsonl.logical-archive.lock")).unwrap();
    lock.lock().unwrap();
    assert_eq!(
        failure(&[
            "export".as_ref(),
            "--db".as_ref(),
            source.as_os_str(),
            "--archive-id".as_ref(),
            "cli-archive".as_ref(),
            "--include-private".as_ref(),
            "--output".as_ref(),
            busy.as_os_str(),
        ]),
        (7, "logical-archive-busy".to_owned(), true)
    );
    drop(lock);
    assert!(!busy.exists());

    // I/O, not usage: an unwritable destination whose path contains "usage".
    // Root bypasses directory permissions, which would make this vacuous.
    #[cfg(unix)]
    if !running_as_root() {
        use std::os::unix::fs::PermissionsExt;
        let sealed = home.join("usage-readonly");
        fs::create_dir(&sealed).unwrap();
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o555)).unwrap();
        let output = sealed.join("history.jsonl");
        let result = failure(&[
            "export".as_ref(),
            "--db".as_ref(),
            source.as_os_str(),
            "--archive-id".as_ref(),
            "cli-archive".as_ref(),
            "--include-private".as_ref(),
            "--output".as_ref(),
            output.as_os_str(),
        ]);
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(result, io);
        assert!(!output.exists());
    }
}

/// Whether the test runs as uid 0, where directory permissions are bypassed.
#[cfg(unix)]
fn running_as_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| output.stdout.trim_ascii() == b"0")
}

#[test]
fn real_binary_never_publishes_a_valid_prefix_or_an_unknown_version() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.db");
    drop(SqliteStorage::open(&source).unwrap());
    let input = root.path().join("history.jsonl");
    export(root.path(), &source, &input);
    let original = fs::read(&input).unwrap();
    let newline = original.iter().position(|byte| *byte == b'\n').unwrap();
    let mut header: Value = serde_json::from_slice(&original[..newline]).unwrap();
    header["header"]["schema_version"] = Value::from(999);
    let mut unknown = serde_json::to_vec(&header).unwrap();
    unknown.extend_from_slice(&original[newline..]);
    for (index, bytes) in [original[..original.len() - 1].to_vec(), unknown]
        .into_iter()
        .enumerate()
    {
        let invalid = root.path().join(format!("invalid-{index}.jsonl"));
        fs::write(&invalid, bytes).unwrap();
        let target = root.path().join(format!("target-{index}.db"));
        let output = import(root.path(), &invalid, &target, false);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(serde_json::from_slice::<Value>(&output.stderr).is_ok());
        assert!(!target.exists());
    }
}

#[cfg(unix)]
#[test]
fn real_binary_verify_refuses_a_fifo_without_waiting_for_a_writer() {
    use std::os::unix::fs::FileTypeExt;
    let root = tempfile::tempdir().unwrap();
    let input = root.path().join("history.fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&input)
            .status()
            .expect("create FIFO fixture")
            .success()
    );
    assert!(fs::symlink_metadata(&input).unwrap().file_type().is_fifo());
    // No process opens the write end. The old File::open blocks indefinitely;
    // the external deadline prevents the regression itself hanging the suite.
    let output = command(root.path())
        .timeout(Duration::from_secs(5))
        .args(["archive", "verify"])
        .arg(&input)
        .output()
        .expect("verification must return a diagnostic, not time out");
    assert!(
        output.status.code().is_some(),
        "verifier was killed instead of refusing input"
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).expect("one JSON error");
    assert!(error.to_string().contains("regular, non-symlink"));
    assert!(fs::symlink_metadata(&input).unwrap().file_type().is_fifo());
    assert!(!root.path().join("unused-default").exists());
}

#[cfg(unix)]
#[test]
fn real_binary_verify_preserves_regular_input_and_refuses_a_link_to_it() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.db");
    drop(SqliteStorage::open(&source).unwrap());
    let input = root.path().join("history.jsonl");
    let exported = export(root.path(), &source, &input);
    let before = fs::read(&input).unwrap();
    let verified = receipt(
        command(root.path())
            .args(["archive", "verify"])
            .arg(&input)
            .output()
            .unwrap(),
    );
    assert_eq!(verified["operation"], "verify");
    assert_eq!(verified["content_sha256"], exported["content_sha256"]);
    assert_eq!(verified["tables"], exported["tables"]);
    let linked = root.path().join("linked.jsonl");
    symlink(&input, &linked).unwrap();
    let output = command(root.path())
        .args(["archive", "verify"])
        .arg(&linked)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).expect("one JSON error");
    assert!(error.to_string().contains("regular, non-symlink"));
    assert_eq!(fs::read(&input).unwrap(), before);
    assert_eq!(fs::read_link(&linked).unwrap(), input);
    assert!(!root.path().join("unused-default").exists());
}

fn import_with_lexical_rebuild(
    home: &Path,
    input: &Path,
    target: &Path,
    identical: bool,
) -> Output {
    let mut cmd = command(home);
    cmd.args(["archive", "import"])
        .arg(input)
        .args([
            "--archive-id",
            "cli-archive",
            "--include-private",
            "--rebuild-index",
            "--output",
        ])
        .arg(target);
    if identical {
        cmd.arg("--if-identical");
    }
    cmd.output().unwrap()
}

fn search_recovered(home: &Path, data_dir: &Path, query: &str) -> Value {
    // No --db: the recovered profile must be usable by the ordinary CLI.
    // --no-maintenance prevents a search-triggered repair from hiding a failed
    // import-time rebuild. Explicit lexical mode needs no installed model.
    receipt(
        command(home)
            .args([
                "search",
                query,
                "--mode",
                "lexical",
                "--robot",
                "--no-maintenance",
                "--limit",
                "20",
                "--data-dir",
            ])
            .arg(data_dir)
            .output()
            .unwrap(),
    )
}

fn portable_search_fixture(home: &Path) -> (std::path::PathBuf, Value, std::path::PathBuf) {
    let original = home.join("original-search-source");
    fs::create_dir(&original).unwrap();
    let source = original.join("agent_search.db");
    let missing_source = home.join("vanished-provider/shared.jsonl");
    let storage = SqliteStorage::open(&source).unwrap();
    for (agent, source_id) in [("claude_code", "remote-a"), ("codex", "remote-b")] {
        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: agent.into(),
                name: agent.into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();
        storage
            .insert_conversation_tree(
                agent_id,
                None,
                &Conversation {
                    id: None,
                    agent_slug: agent.into(),
                    workspace: None,
                    external_id: Some(format!("indexed-{source_id}")),
                    title: Some(format!("Portable search {source_id}")),
                    source_path: missing_source.clone(),
                    started_at: Some(1_733_000_000_000),
                    ended_at: None,
                    approx_tokens: None,
                    metadata_json: serde_json::json!({}),
                    messages: [0, 7]
                        .into_iter()
                        .map(|idx| Message {
                            id: None,
                            idx,
                            role: MessageRole::User,
                            author: None,
                            created_at: Some(1_733_000_000_000 + idx),
                            content: format!(
                                "PORTABLENEEDLE complete recovered evidence {source_id} at {idx} δ"
                            ),
                            extra_json: serde_json::json!({}),
                            snippets: Vec::new(),
                        })
                        .collect(),
                    source_id: source_id.into(),
                    origin_host: Some(source_id.into()),
                },
            )
            .unwrap();
    }
    drop(storage);
    let input = home.join("searchable-history.jsonl");
    let exported = export(home, &source, &input);
    fs::rename(&original, home.join("retired-search-source")).unwrap();
    assert!(!source.exists());
    assert!(!missing_source.exists());
    (input, exported, missing_source)
}

#[test]
fn indexed_import_completes_the_source_less_search_to_canonical_evidence_journey() {
    let root = tempfile::tempdir().unwrap();
    let (input, exported, missing_source) = portable_search_fixture(root.path());
    let input_bytes = fs::read(&input).unwrap();
    let data = root.path().join("recovered-profile");
    fs::create_dir(&data).unwrap();
    let target = data.join("agent_search.db");
    let imported = receipt(import_with_lexical_rebuild(
        root.path(),
        &input,
        &target,
        false,
    ));
    assert_eq!(imported["content_sha256"], exported["content_sha256"]);
    assert_eq!(imported["destination_status"], "created");
    assert_eq!(
        imported["derived_search_assets"],
        "lexical_rebuilt_semantic_not_built"
    );
    assert_eq!(imported["lexical_rebuild"]["indexed_documents"], 4);
    assert_eq!(
        imported["lexical_rebuild"]["provider_scan_performed"],
        false
    );
    assert_eq!(imported["lexical_rebuild"]["semantic_assets_built"], false);
    let database_bytes = fs::read(&target).unwrap();
    let searched = search_recovered(root.path(), &data, "PORTABLENEEDLE");
    let hits = searched["hits"].as_array().expect("ordinary search hits");
    assert_eq!(hits.len(), 4, "{searched}");
    let mut coordinates = std::collections::BTreeSet::new();
    for hit in hits {
        let source_id = hit["source_id"].as_str().unwrap();
        let conversation = hit["conversation_id"].as_i64().unwrap();
        let ordinal = hit["line_number"].as_u64().unwrap();
        assert!(matches!(source_id, "remote-a" | "remote-b"));
        assert!(matches!(ordinal, 1 | 8));
        assert_eq!(hit["source_path"], missing_source.to_str().unwrap());
        assert!(coordinates.insert((source_id, conversation, ordinal)));
        let viewed = receipt(
            command(root.path())
                .arg("--db")
                .arg(&target)
                .args([
                    "view",
                    missing_source.to_str().unwrap(),
                    "--source",
                    source_id,
                    "--conversation-id",
                    &conversation.to_string(),
                    "--message-index",
                    &ordinal.to_string(),
                    "-C",
                    "0",
                    "--json",
                ])
                .output()
                .unwrap(),
        );
        assert_eq!(viewed["lines"][0]["source_id"], source_id);
        assert_eq!(viewed["lines"][0]["conversation_id"], conversation);
        assert_eq!(viewed["lines"][0]["message_index"], ordinal);
        assert_eq!(
            viewed["lines"][0]["content"],
            format!(
                "PORTABLENEEDLE complete recovered evidence {source_id} at {} δ",
                ordinal - 1
            )
        );
    }
    let repeated = receipt(import_with_lexical_rebuild(
        root.path(),
        &input,
        &target,
        true,
    ));
    assert_eq!(repeated["destination_status"], "unchanged");
    assert_eq!(repeated["lexical_rebuild"]["indexed_documents"], 4);
    assert_eq!(
        search_recovered(root.path(), &data, "PORTABLENEEDLE")["hits"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(fs::read(&target).unwrap(), database_bytes);
    assert_eq!(fs::read(&input).unwrap(), input_bytes);
    assert_eq!(
        export(root.path(), &target, &root.path().join("reindexed.jsonl"))["content_sha256"],
        exported["content_sha256"]
    );
    assert!(!missing_source.exists());
    assert!(!root.path().join("unused-default").exists());
    assert!(!data.join("models").exists());
    assert!(!data.join("vector_index").exists());
}

#[test]
fn indexed_empty_archive_produces_a_readable_empty_lexical_generation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("empty.db");
    drop(SqliteStorage::open(&source).unwrap());
    let input = root.path().join("empty.jsonl");
    let exported = export(root.path(), &source, &input);
    let data = root.path().join("empty-recovered");
    fs::create_dir(&data).unwrap();
    let imported = receipt(import_with_lexical_rebuild(
        root.path(),
        &input,
        &data.join("agent_search.db"),
        false,
    ));
    assert_eq!(imported["lexical_rebuild"]["indexed_documents"], 0);
    assert_eq!(imported["content_sha256"], exported["content_sha256"]);
    assert_eq!(
        search_recovered(root.path(), &data, "unmatched")["hits"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert!(!root.path().join("unused-default").exists());
}

#[test]
fn failed_index_rebuild_retains_the_complete_restore_and_identical_retry_can_finish() {
    let root = tempfile::tempdir().unwrap();
    let (input, exported, _) = portable_search_fixture(root.path());
    let data = root.path().join("retry-profile");
    fs::create_dir(&data).unwrap();
    // Actual filesystem refusal at index-run admission, after canonical import.
    let obstruction = data.join("index-run.lock");
    fs::create_dir(&obstruction).unwrap();
    let target = data.join("agent_search.db");
    let failed = import_with_lexical_rebuild(root.path(), &input, &target, false);
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    let error: Value = serde_json::from_slice(&failed.stderr).expect("one JSON failure");
    assert!(error.to_string().contains("is retained"), "{error}");
    assert!(error.to_string().contains("--if-identical --rebuild-index"));
    assert!(target.is_file());
    assert_eq!(
        export(
            root.path(),
            &target,
            &root.path().join("after-rebuild-refusal.jsonl")
        )["content_sha256"],
        exported["content_sha256"]
    );
    let before = fs::read(&target).unwrap();
    fs::rename(&obstruction, data.join("retained-lock-obstruction")).unwrap();
    let retried = receipt(import_with_lexical_rebuild(
        root.path(),
        &input,
        &target,
        true,
    ));
    assert_eq!(retried["destination_status"], "unchanged");
    assert_eq!(retried["lexical_rebuild"]["indexed_documents"], 4);
    assert_eq!(fs::read(&target).unwrap(), before);
    assert_eq!(
        search_recovered(root.path(), &data, "PORTABLENEEDLE")["hits"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn indexed_import_rejects_bad_layouts_and_invalid_streams_before_any_search_build() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.db");
    drop(SqliteStorage::open(&source).unwrap());
    let input = root.path().join("input.jsonl");
    export(root.path(), &source, &input);
    let data = root.path().join("destination");
    fs::create_dir(&data).unwrap();
    let wrong = data.join("not-the-profile-database.db");
    let result = import_with_lexical_rebuild(root.path(), &input, &wrong, false);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("agent_search.db"));
    assert_eq!(fs::read_dir(&data).unwrap().count(), 0);
    let invalid = root.path().join("truncated.jsonl");
    let bytes = fs::read(&input).unwrap();
    fs::write(&invalid, &bytes[..bytes.len() - 1]).unwrap();
    let target = data.join("agent_search.db");
    let result = import_with_lexical_rebuild(root.path(), &invalid, &target, false);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(serde_json::from_slice::<Value>(&result.stderr).is_ok());
    assert!(!target.exists());
    assert!(!data.join("index").exists());
    assert!(!data.join("index-run.lock").exists());
    assert_eq!(fs::read(&input).unwrap(), bytes);
}

fn index_image(data: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let index = coding_agent_search::search::tantivy::expected_index_dir(data);
    let mut image = std::collections::BTreeMap::new();
    for entry in walkdir::WalkDir::new(&index).follow_links(false) {
        let entry = entry.unwrap();
        assert!(!entry.file_type().is_symlink(), "unexpected index link");
        if entry.file_type().is_file() {
            image.insert(
                entry.path().strip_prefix(&index).unwrap().to_path_buf(),
                fs::read(entry.path()).unwrap(),
            );
        }
    }
    assert!(!image.is_empty(), "the index snapshot must not be vacuous");
    image
}

#[test]
fn conflicting_indexed_import_preserves_the_previous_searchable_generation() {
    let root = tempfile::tempdir().unwrap();
    let (input, exported, _) = portable_search_fixture(root.path());
    let data = root.path().join("preserved-profile");
    fs::create_dir(&data).unwrap();
    let target = data.join("agent_search.db");
    receipt(import_with_lexical_rebuild(
        root.path(),
        &input,
        &target,
        false,
    ));
    assert_eq!(
        search_recovered(root.path(), &data, "PORTABLENEEDLE")["hits"]
            .as_array()
            .unwrap()
            .len(),
        4
    );

    // Valid, complete input with the same caller-assigned archive ID but
    // different canonical contents must not acquire rebuild authority.
    let different = root.path().join("different.db");
    drop(SqliteStorage::open(&different).unwrap());
    let conflict = root.path().join("different.jsonl");
    let changed = export(root.path(), &different, &conflict);
    assert_ne!(changed["content_sha256"], exported["content_sha256"]);
    let db_before = fs::read(&target).unwrap();
    let index_before = index_image(&data);
    let input_before = fs::read(&conflict).unwrap();
    for identical in [false, true] {
        let failed = import_with_lexical_rebuild(root.path(), &conflict, &target, identical);
        assert!(!failed.status.success());
        assert!(failed.stdout.is_empty());
        assert!(serde_json::from_slice::<Value>(&failed.stderr).is_ok());
        assert_eq!(fs::read(&target).unwrap(), db_before);
        assert_eq!(index_image(&data), index_before);
        assert_eq!(fs::read(&conflict).unwrap(), input_before);
    }
    assert_eq!(
        search_recovered(root.path(), &data, "PORTABLENEEDLE")["hits"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn indexed_restore_ignores_discoverable_local_provider_histories() {
    let root = tempfile::tempdir().unwrap();
    let (input, exported, _) = portable_search_fixture(root.path());
    // A real provider-shaped session in the isolated HOME is deliberately NOT
    // part of the exported canonical archive. Recovery must not mix it in.
    let project = root.path().join(".claude/projects/-test-local-history");
    fs::create_dir_all(&project).unwrap();
    let history = project.join("local-sentinel.jsonl");
    let record = serde_json::json!({
        "parentUuid": null, "cwd": "/test/local-history",
        "sessionId": "local-sentinel", "version": "2.0.37",
        "gitBranch": "main", "type": "user", "uuid": "local-message",
        "timestamp": "2026-01-20T09:00:00.000Z",
        "message": {"role": "user", "content": "LOCALHISTORYMUSTSTAYOUT unrelated local session"}
    });
    let history_bytes = format!("{record}\n").into_bytes();
    fs::write(&history, &history_bytes).unwrap();
    let data = root.path().join("isolated-recovered");
    fs::create_dir(&data).unwrap();
    let target = data.join("agent_search.db");
    let result = receipt(import_with_lexical_rebuild(
        root.path(),
        &input,
        &target,
        false,
    ));
    assert_eq!(result["lexical_rebuild"]["indexed_documents"], 4);
    assert_eq!(
        search_recovered(root.path(), &data, "LOCALHISTORYMUSTSTAYOUT")["hits"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        search_recovered(root.path(), &data, "PORTABLENEEDLE")["hits"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(fs::read(&history).unwrap(), history_bytes);
    assert_eq!(
        export(
            root.path(),
            &target,
            &root.path().join("after-local-sentinel.jsonl")
        )["content_sha256"],
        exported["content_sha256"]
    );
    assert!(!root.path().join("unused-default").exists());
}

#[test]
fn backup_search_finds_verified_evidence_without_a_database_or_provider_files() {
    let root = tempfile::tempdir().unwrap();
    let (input, exported, absent_source) = portable_search_fixture(root.path());
    let before = fs::read(&input).unwrap();
    let entries = fs::read_dir(root.path()).unwrap().count();
    let output = receipt(
        command(root.path())
            .args(["archive", "search"])
            .arg(&input)
            .args([
                "--contains",
                "PORTABLENEEDLE",
                "--limit",
                "3",
                "--include-private",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(output["content_sha256"], exported["content_sha256"]);
    assert_eq!(output["matches"], 4);
    assert_eq!(output["has_more"], true);
    assert_eq!(output["database_opened"], false);
    assert_eq!(output["provider_files_opened"], false);
    assert_eq!(output["match_mode"], "literal_case_sensitive");
    let hits = output["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3);
    for hit in hits {
        assert_eq!(hit["source_path"], absent_source.to_str().unwrap());
        let source = hit["source_id"].as_str().unwrap();
        let idx = hit["message_index"].as_u64().unwrap() - 1;
        assert!(matches!(source, "remote-a" | "remote-b"));
        assert!(matches!(idx, 0 | 7));
        assert_eq!(
            hit["snippet"],
            format!("PORTABLENEEDLE complete recovered evidence {source} at {idx} δ")
        );
    }
    let scoped = receipt(
        command(root.path())
            .args(["archive", "search"])
            .arg(&input)
            .args([
                "--contains",
                "PORTABLENEEDLE",
                "--include-private",
                "--conversation-id",
            ])
            .arg(hits[0]["conversation_id"].as_i64().unwrap().to_string())
            .output()
            .unwrap(),
    );
    assert_eq!(scoped["matches"], 2);
    assert_eq!(fs::read(&input).unwrap(), before);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), entries);
    assert!(!root.path().join("unused-default").exists());
    assert!(!absent_source.exists());
}

#[test]
fn backup_search_withholds_all_results_until_complete_validation_and_private_consent() {
    let root = tempfile::tempdir().unwrap();
    let (input, _, _) = portable_search_fixture(root.path());
    let denied = command(root.path())
        .args(["archive", "search"])
        .arg(&input)
        .args(["--contains", "PORTABLENEEDLE"])
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert!(denied.stdout.is_empty());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--include-private"));
    let truncated = root.path().join("incomplete-search.jsonl");
    let bytes = fs::read(&input).unwrap();
    fs::write(&truncated, &bytes[..bytes.len() - 1]).unwrap();
    let denied = command(root.path())
        .args(["archive", "search"])
        .arg(&truncated)
        .args([
            "--contains",
            "PORTABLENEEDLE",
            "--include-private",
            "--limit",
            "1",
        ])
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert!(denied.stdout.is_empty());
    assert!(serde_json::from_slice::<Value>(&denied.stderr).is_ok());
    assert!(!root.path().join("unused-default").exists());
}

#[test]
fn backup_search_to_complete_view_preserves_snapshot_source_and_sparse_context() {
    let root = tempfile::tempdir().unwrap();
    let (input, exported, absent_source) = portable_search_fixture(root.path());
    let before = fs::read(&input).unwrap();
    let entries = fs::read_dir(root.path()).unwrap().count();
    let searched = receipt(
        command(root.path())
            .args(["archive", "search"])
            .arg(&input)
            .args(["--contains", "PORTABLENEEDLE", "--include-private"])
            .output()
            .unwrap(),
    );
    let digest = searched["content_sha256"].as_str().unwrap();
    for hit in searched["hits"].as_array().unwrap() {
        let viewed = receipt(
            command(root.path())
                .args(["archive", "view"])
                .arg(&input)
                .args([
                    "--message-id",
                    &hit["message_id"].as_i64().unwrap().to_string(),
                    "--content-sha256",
                    digest,
                    "--context",
                    "1",
                    "--include-private",
                ])
                .output()
                .unwrap(),
        );
        assert_eq!(viewed["content_sha256"], exported["content_sha256"]);
        assert_eq!(viewed["source_id"], hit["source_id"]);
        assert_eq!(viewed["conversation_id"], hit["conversation_id"]);
        assert_eq!(viewed["source_path"], absent_source.to_str().unwrap());
        assert_eq!(viewed["preview_only"], false);
        assert_eq!(viewed["database_opened"], false);
        assert_eq!(viewed["provider_files_opened"], false);
        let messages = viewed["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["message_index"], 1);
        assert_eq!(messages[1]["message_index"], 8);
        let target = messages
            .iter()
            .find(|message| message["is_target"] == true)
            .unwrap();
        assert_eq!(target["message_id"], hit["message_id"]);
        assert_eq!(target["content"], hit["snippet"]);
    }
    assert_eq!(fs::read(&input).unwrap(), before);
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), entries);
    assert!(!root.path().join("unused-default").exists());
    assert!(!absent_source.exists());
}

#[test]
fn backup_view_refuses_wrong_snapshots_missing_ids_and_private_output_without_consent() {
    let root = tempfile::tempdir().unwrap();
    let (input, exported, _) = portable_search_fixture(root.path());
    let digest = exported["content_sha256"].as_str().unwrap();
    for (id, hash, private) in [
        (1, "0".repeat(64), true),
        (i64::MAX, digest.to_owned(), true),
        (1, digest.to_owned(), false),
    ] {
        let mut cmd = command(root.path());
        cmd.args(["archive", "view"]).arg(&input).args([
            "--message-id",
            &id.to_string(),
            "--content-sha256",
            &hash,
        ]);
        if private {
            cmd.arg("--include-private");
        }
        let result = cmd.output().unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(serde_json::from_slice::<Value>(&result.stderr).is_ok());
    }
    assert!(!root.path().join("unused-default").exists());
}

#[test]
fn backup_cursor_pages_all_matches_and_rejects_reuse_with_different_criteria() {
    let root = tempfile::tempdir().unwrap();
    let (input, exported, _) = portable_search_fixture(root.path());
    let mut cursor: Option<String> = None;
    let mut ids = std::collections::BTreeSet::new();
    for page in 0..4 {
        let mut cmd = command(root.path());
        cmd.args(["archive", "search"]).arg(&input).args([
            "--contains",
            "PORTABLENEEDLE",
            "--include-private",
            "--limit",
            "1",
        ]);
        if let Some(cursor) = &cursor {
            cmd.args(["--cursor", cursor]);
        }
        let result = receipt(cmd.output().unwrap());
        assert_eq!(result["content_sha256"], exported["content_sha256"]);
        assert_eq!(result["matches"], 4);
        assert_eq!(result["matches_after_cursor"], 4 - page);
        let hits = result["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert!(ids.insert(hits[0]["message_id"].as_i64().unwrap()));
        cursor = result["next_cursor"].as_str().map(str::to_owned);
        assert_eq!(cursor.is_some(), page < 3);
        if let Some(cursor) = &cursor {
            let refused = command(root.path())
                .args(["archive", "search"])
                .arg(&input)
                .args([
                    "--contains",
                    "different query",
                    "--include-private",
                    "--cursor",
                    cursor,
                ])
                .output()
                .unwrap();
            assert!(!refused.status.success());
            assert!(refused.stdout.is_empty());
            assert!(String::from_utf8_lossy(&refused.stderr).contains("different query"));
        }
    }
    assert_eq!(ids.len(), 4);
    assert!(!root.path().join("unused-default").exists());
}
