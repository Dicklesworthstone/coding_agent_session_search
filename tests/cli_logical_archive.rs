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

fn import_with_lexical_rebuild(home: &Path, input: &Path, target: &Path, identical: bool) -> Output {
    let mut cmd = command(home);
    cmd.args(["archive", "import"])
        .arg(input)
        .args(["--archive-id", "cli-archive", "--include-private", "--rebuild-index", "--output"])
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
    receipt(command(home)
        .args(["search", query, "--mode", "lexical", "--robot", "--no-maintenance", "--limit", "20", "--data-dir"])
        .arg(data_dir)
        .output().unwrap())
}

fn portable_search_fixture(home: &Path) -> (std::path::PathBuf, Value, std::path::PathBuf) {
    let original = home.join("original-search-source");
    fs::create_dir(&original).unwrap();
    let source = original.join("agent_search.db");
    let missing_source = home.join("vanished-provider/shared.jsonl");
    let storage = SqliteStorage::open(&source).unwrap();
    for (agent, source_id) in [("claude_code", "remote-a"), ("codex", "remote-b")] {
        let agent_id = storage.ensure_agent(&Agent {
            id: None, slug: agent.into(), name: agent.into(), version: None, kind: AgentKind::Cli,
        }).unwrap();
        storage.insert_conversation_tree(agent_id, None, &Conversation {
            id: None, agent_slug: agent.into(), workspace: None,
            external_id: Some(format!("indexed-{source_id}")),
            title: Some(format!("Portable search {source_id}")),
            source_path: missing_source.clone(), started_at: Some(1_733_000_000_000),
            ended_at: None, approx_tokens: None, metadata_json: serde_json::json!({}),
            messages: [0, 7].into_iter().map(|idx| Message {
                id: None, idx, role: MessageRole::User, author: None,
                created_at: Some(1_733_000_000_000 + idx),
                content: format!("PORTABLENEEDLE complete recovered evidence {source_id} at {idx} δ"),
                extra_json: serde_json::json!({}), snippets: Vec::new(),
            }).collect(),
            source_id: source_id.into(), origin_host: Some(source_id.into()),
        }).unwrap();
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
    let imported = receipt(import_with_lexical_rebuild(root.path(), &input, &target, false));
    assert_eq!(imported["content_sha256"], exported["content_sha256"]);
    assert_eq!(imported["destination_status"], "created");
    assert_eq!(imported["derived_search_assets"], "lexical_rebuilt_semantic_not_built");
    assert_eq!(imported["lexical_rebuild"]["indexed_documents"], 4);
    assert_eq!(imported["lexical_rebuild"]["provider_scan_performed"], false);
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
        let viewed = receipt(command(root.path())
            .args(["view", missing_source.to_str().unwrap(), "--source", source_id,
                "--conversation-id", &conversation.to_string(), "--message-index", &ordinal.to_string(),
                "-C", "0", "--json", "--data-dir"])
            .arg(&data).output().unwrap());
        assert_eq!(viewed["lines"][0]["source_id"], source_id);
        assert_eq!(viewed["lines"][0]["conversation_id"], conversation);
        assert_eq!(viewed["lines"][0]["message_index"], ordinal);
        assert_eq!(viewed["lines"][0]["content"],
            format!("PORTABLENEEDLE complete recovered evidence {source_id} at {} δ", ordinal - 1));
    }
    let repeated = receipt(import_with_lexical_rebuild(root.path(), &input, &target, true));
    assert_eq!(repeated["destination_status"], "unchanged");
    assert_eq!(repeated["lexical_rebuild"]["indexed_documents"], 4);
    assert_eq!(search_recovered(root.path(), &data, "PORTABLENEEDLE")["hits"].as_array().unwrap().len(), 4);
    assert_eq!(fs::read(&target).unwrap(), database_bytes);
    assert_eq!(fs::read(&input).unwrap(), input_bytes);
    assert_eq!(export(root.path(), &target, &root.path().join("reindexed.jsonl"))["content_sha256"], exported["content_sha256"]);
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
    let imported = receipt(import_with_lexical_rebuild(root.path(), &input, &data.join("agent_search.db"), false));
    assert_eq!(imported["lexical_rebuild"]["indexed_documents"], 0);
    assert_eq!(imported["content_sha256"], exported["content_sha256"]);
    assert_eq!(search_recovered(root.path(), &data, "unmatched")["hits"].as_array().unwrap().len(), 0);
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
    assert_eq!(export(root.path(), &target, &root.path().join("after-rebuild-refusal.jsonl"))["content_sha256"], exported["content_sha256"]);
    let before = fs::read(&target).unwrap();
    fs::rename(&obstruction, data.join("retained-lock-obstruction")).unwrap();
    let retried = receipt(import_with_lexical_rebuild(root.path(), &input, &target, true));
    assert_eq!(retried["destination_status"], "unchanged");
    assert_eq!(retried["lexical_rebuild"]["indexed_documents"], 4);
    assert_eq!(fs::read(&target).unwrap(), before);
    assert_eq!(search_recovered(root.path(), &data, "PORTABLENEEDLE")["hits"].as_array().unwrap().len(), 4);
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
    receipt(import_with_lexical_rebuild(root.path(), &input, &target, false));
    assert_eq!(
        search_recovered(root.path(), &data, "PORTABLENEEDLE")["hits"]
            .as_array().unwrap().len(),
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
            .as_array().unwrap().len(),
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
    let result = receipt(import_with_lexical_rebuild(root.path(), &input, &target, false));
    assert_eq!(result["lexical_rebuild"]["indexed_documents"], 4);
    assert_eq!(
        search_recovered(root.path(), &data, "LOCALHISTORYMUSTSTAYOUT")["hits"]
            .as_array().unwrap().len(),
        0
    );
    assert_eq!(
        search_recovered(root.path(), &data, "PORTABLENEEDLE")["hits"]
            .as_array().unwrap().len(),
        4
    );
    assert_eq!(fs::read(&history).unwrap(), history_bytes);
    assert_eq!(
        export(root.path(), &target, &root.path().join("after-local-sentinel.jsonl"))["content_sha256"],
        exported["content_sha256"]
    );
    assert!(!root.path().join("unused-default").exists());
}
