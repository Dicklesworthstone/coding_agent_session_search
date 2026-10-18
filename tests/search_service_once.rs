//! Real-process coverage for one-shot supervised lexical search.
//! Keep stdin open to prove that --query does not accidentally enter stdio mode.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::tantivy::{TantivyIndex, expected_index_dir};
use coding_agent_search::storage::sqlite::FrankenStorage;
use frankensearch::quill::cass::CassDocument;
use serde_json::{Value, json};
use wait_timeout::ChildExt;

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn execute(root: &Path, index: &Path, arguments: &[&str], fuel: u64) -> Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let mut child = Child(Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["serve", "--index"])
        .arg(index)
        .args(["--request-timeout-ms", "5000"])
        .args(arguments)
        .current_dir(root)
        .env("CASS_QUILL_QUERY_FUEL_BUDGET", fuel.to_string())
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?);
    let input = child.0.stdin.take().context("stdin")?;
    let stdout = child.0.stdout.take().context("stdout")?;
    let stderr = child.0.stderr.take().context("stderr")?;
    // Drain both pipes while waiting: even valid service envelopes can exceed
    // a platform's pipe capacity. Readers also bound retained test output.
    fn capture(stream: impl Read) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        stream.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    let out = std::thread::spawn(move || capture(stdout));
    let err = std::thread::spawn(move || capture(stderr));
    let status = child.0.wait_timeout(Duration::from_secs(15))?
        .context("one-shot worker waited for stdin or exceeded its deadline")?;
    drop(input);
    let out = out.join().map_err(|_| anyhow::anyhow!("stdout reader panicked"))??;
    let err = err.join().map_err(|_| anyhow::anyhow!("stderr reader panicked"))??;
    ensure!(out.len() <= 2 * 1024 * 1024 && err.len() <= 2 * 1024 * 1024, "unbounded child output");
    Ok((status, out, err))
}

fn fixture(root: &Path) -> Result<(PathBuf, PathBuf, i64)> {
    let db = root.join("archive.db");
    let index = expected_index_dir(root);
    let storage = FrankenStorage::open(&db)?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let content = "git fixture establishes a supervised search regression";
    let inserted = storage.insert_conversation_tree(agent, None, &Conversation {
        id: None,
        agent_slug: "codex".into(),
        workspace: None,
        external_id: Some("one-shot".into()),
        title: Some("one-shot fixture".into()),
        source_path: "/history/once.jsonl".into(),
        started_at: None,
        ended_at: None,
        approx_tokens: None,
        metadata_json: json!({}),
        source_id: "local".into(),
        origin_host: None,
        messages: vec![Message {
            id: None,
            idx: 0,
            role: MessageRole::Agent,
            author: None,
            created_at: Some(1_700_000_000_000),
            content: content.into(),
            extra_json: json!({}),
            snippets: Vec::new(),
        }],
    })?;
    let mut writer = TantivyIndex::open_or_create(&index)?;
    writer.add_prebuilt_documents_slice(&[CassDocument {
        agent: "codex".into(),
        workspace: Some("/work".into()),
        workspace_original: None,
        source_path: "/history/once.jsonl".into(),
        msg_idx: 0,
        created_at: Some(1_700_000_000_000),
        title: Some("one-shot fixture".into()),
        content: content.into(),
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: Some(inserted.conversation_id),
    }])?;
    writer.commit()?;
    drop(writer);
    drop(storage);
    Ok((index, db, inserted.conversation_id))
}

#[test]
fn one_shot_returns_filtered_hits_with_stdin_still_open_and_no_archive_access() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (index, _, identity) = fixture(temp.path())?;
    let (status, out, err) = execute(temp.path(), &index, &[
        "--query", "git fixture", "--limit", "1", "--agent", "codex",
        "--workspace", "/work", "--source-id", "local",
        "--created-from", "1700000000000", "--created-to", "1700000000000",
    ], 10_000_000)?;
    ensure!(status.success(), "{status}: {}", String::from_utf8_lossy(&err));
    assert!(err.is_empty());
    let reply: Value = serde_json::from_slice(&out)?;
    assert_eq!(reply["ok"], true);
    let result = &reply["result"];
    assert_eq!(result["hits"][0]["conversation_id"], identity);
    assert_eq!(result["count"], 1);
    assert_eq!(result["backend"], "quill");
    assert_eq!(result["snapshot"]["canonical_database_accessed"], false);
    assert_eq!(result["snapshot"]["maintenance_performed"], false);
    assert_eq!(result["snapshot"]["memory_supervision"]["enabled"], true);
    assert_eq!(result["snapshot"]["limits"]["timeout_scope"],
        "validated_arguments_through_reader_teardown_and_response_flush");
    Ok(())
}

#[test]
fn one_shot_fuel_recovery_is_explicit_and_reports_the_changed_backend() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (index, db, identity) = fixture(temp.path())?;
    let before = std::fs::read(&db)?;
    let db_arg = db.to_str().context("UTF-8 fixture path")?;
    let (status, out, err) = execute(temp.path(), &index,
        &["--query", "git fixture", "--db", db_arg], 1)?;
    assert_eq!(status.code(), Some(1));
    assert!(out.is_empty());
    let failure: Value = serde_json::from_slice(&err)?;
    assert_eq!(failure["ok"], false);
    assert!(failure["error"]["message"].as_str().unwrap().contains("fallback is disabled"));
    let (status, out, err) = execute(temp.path(), &index,
        &["--query", "git fixture", "--db", db_arg, "--lexical-fallback"], 1)?;
    ensure!(status.success(), "{status}: {}", String::from_utf8_lossy(&err));
    let reply: Value = serde_json::from_slice(&out)?;
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["result"]["backend"], "frankensqlite");
    assert_eq!(reply["result"]["degraded"], true);
    assert_eq!(reply["result"]["hits"][0]["conversation_id"], identity);
    assert_eq!(reply["result"]["fallback"]["shares_request_deadline"], true);
    assert!(reply["result"]["result_reader_epoch"].is_null());
    assert_eq!(std::fs::read(&db)?, before);
    Ok(())
}

#[test]
fn one_shot_authoritative_empty_does_not_open_a_corrupt_fallback_archive() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (index, _, _) = fixture(temp.path())?;
    let corrupt = temp.path().join("never-open.db");
    std::fs::write(&corrupt, b"not a database")?;
    let (status, out, err) = execute(temp.path(), &index, &[
        "--query", "zzzznonexistenttoken", "--lexical-fallback", "--db",
        corrupt.to_str().context("UTF-8 fixture path")?,
    ], 10_000_000)?;
    ensure!(status.success(), "{status}: {}", String::from_utf8_lossy(&err));
    let reply: Value = serde_json::from_slice(&out)?;
    assert_eq!(reply["result"]["count"], 0);
    assert_eq!(reply["result"]["degraded"], false);
    assert_eq!(reply["result"]["snapshot"]["canonical_database_accessed"], false);
    assert_eq!(std::fs::read(&corrupt)?, b"not a database");
    Ok(())
}

#[test]
fn one_shot_invalid_work_is_rejected_before_creating_any_storage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let index = temp.path().join("must-not-create");
    for args in [
        vec!["--query", " "],
        vec!["--query", "needle", "--limit", "0"],
        vec!["--query", "needle", "--offset", "1024"],
        vec!["--query", "needle", "--created-from", "2", "--created-to", "1"],
    ] {
        let (status, out, err) = execute(temp.path(), &index, &args, 1)?;
        assert_eq!(status.code(), Some(2));
        assert!(out.is_empty());
        let reply: Value = serde_json::from_slice(&err)?;
        assert_eq!(reply["error"]["kind"], "invalid_request");
        assert!(!index.exists());
    }
    Ok(())
}
