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
