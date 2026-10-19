//! Real-process coverage for supervised one-shot semantic/hybrid retrieval.
//! Hash vectors exercise transport and retrieval identity, NOT neural quality.
//! Every child is isolated; no model download or production archive is used.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::embedder::Embedder;
use coding_agent_search::search::hash_embedder::HashEmbedder;
use coding_agent_search::search::tantivy::{TantivyIndex, expected_index_dir};
use coding_agent_search::search::vector_index::{SemanticDocId, VectorIndex, vector_index_path};
use coding_agent_search::storage::sqlite::FrankenStorage;
use frankensearch::quill::cass::CassDocument;
use serde_json::{Value, json};
use wait_timeout::ChildExt;

const TEXT: &str = "semanticneedle persistent retained vector search";
const WORKSPACE: &str = "/work/semantic-fixture";
const CREATED: i64 = 1_700_000_000_000;

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fixture(root: &Path, with_vectors: bool) -> Result<(PathBuf, i64)> {
    let database = root.join("agent_search.db");
    let storage = FrankenStorage::open(&database)?;
    let agent_id = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let workspace_id = storage.ensure_workspace(&PathBuf::from(WORKSPACE), None)?;
    let source_path = root.join("raw-history-never-created.jsonl");
    let outcome = storage.insert_conversation_tree(
        agent_id,
        Some(workspace_id),
        &Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: Some(WORKSPACE.into()),
            external_id: Some("once-semantic-fixture".into()),
            title: Some("Global retrieval".into()),
            source_path: source_path.clone(),
            started_at: Some(CREATED),
            ended_at: None,
            approx_tokens: None,
            metadata_json: json!({}),
            source_id: "local".into(),
            origin_host: None,
            messages: vec![Message {
                id: None,
                idx: 7,
                role: MessageRole::Agent,
                author: None,
                created_at: Some(CREATED),
                content: TEXT.into(),
                extra_json: json!({}),
                snippets: Vec::new(),
            }],
        },
    )?;
    if with_vectors {
        let embedder = HashEmbedder::default();
        let path = vector_index_path(root, embedder.id());
        std::fs::create_dir_all(path.parent().context("vector parent")?)?;
        let mut vectors = VectorIndex::create_with_revision(
            &path,
            embedder.id(),
            coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION,
            embedder.dimension(),
            frankensearch::index::Quantization::F16,
        )?;
        for message in storage.fetch_messages(outcome.conversation_id)? {
            let identity = SemanticDocId {
                message_id: u64::try_from(message.id.context("stored message ID")?)?,
                chunk_idx: 0,
                agent_id: u32::try_from(agent_id)?,
                workspace_id: u32::try_from(workspace_id)?,
                source_id: crc32fast::hash(b"local"),
                role: 1,
                created_at_ms: CREATED,
                content_hash: None,
            };
            vectors.write_record(
                &identity.to_doc_id_string(),
                &embedder.embed_sync(&message.content)?,
            )?;
        }
        vectors.finish()?;
    }
    drop(storage);
    let mut writer = TantivyIndex::open_or_create(&expected_index_dir(root))?;
    writer.add_prebuilt_documents_slice(&[CassDocument {
        agent: "codex".into(),
        workspace: Some(WORKSPACE.into()),
        workspace_original: None,
        source_path: source_path.to_string_lossy().into_owned(),
        msg_idx: 7,
        created_at: Some(CREATED),
        title: Some("Global retrieval".into()),
        content: TEXT.into(),
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: Some(outcome.conversation_id),
    }])?;
    writer.commit()?;
    drop(writer);
    Ok((database, outcome.conversation_id))
}

// Capture only this tiny synthetic archive. No mtime assumptions: compare names
// and bytes before/after all readers are destroyed by the supervised process.
fn snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Option<Vec<u8>>>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(!kind.is_symlink(), "fixture contains an unexpected symlink");
            if kind.is_dir() {
                files.insert(entry.path().strip_prefix(root)?.to_path_buf(), None);
                pending.push(entry.path());
            } else {
                files.insert(
                    entry.path().strip_prefix(root)?.to_path_buf(),
                    Some(std::fs::read(entry.path())?),
                );
            }
        }
    }
    Ok(files)
}

/// Keep stdin open through process exit. A one-shot accidentally entering the
/// frame loop must fail the external deadline, not pass because stdin hit EOF.
/// The optional frame enables a real stdio oracle using the same fresh assets.
fn execute(
    root: &Path,
    database: &Path,
    arguments: &[&str],
    frame: Option<&Value>,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let capture = tempfile::tempdir()?;
    let out_path = capture.path().join("stdout");
    let err_path = capture.path().join("stderr");
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command.env_clear();
    // Preserve OS loader/temp essentials, not ambient CASS configuration.
    for name in ["PATH", "SystemRoot", "SystemDrive", "TEMP", "TMP", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .args(["serve", "--data-dir"])
        .arg(root)
        .arg("--db")
        .arg(database)
        .args(["--semantic-embedder", "hash", "--request-timeout-ms", "10000"])
        .args(arguments)
        .current_dir(root)
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_EXTERNAL_EMBEDDINGS", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(std::fs::File::create(&out_path)?)
        .stderr(std::fs::File::create(&err_path)?);
    let mut child = Child(command.spawn()?);
    let mut input = child.0.stdin.take().context("child stdin")?;
    if let Some(frame) = frame {
        writeln!(input, "{frame}")?;
        writeln!(input, "{}", json!({"op": "shutdown", "id": 2}))?;
        input.flush()?;
    }
    let status = child
        .0
        .wait_timeout(Duration::from_secs(30))?
        .context("one-shot search blocked on stdin or exceeded its external deadline")?;
    drop(input);
    for path in [&out_path, &err_path] {
        ensure!(
            std::fs::metadata(path)?.len() <= 2 * 1024 * 1024,
            "oversized child output"
        );
    }
    Ok((status, std::fs::read(out_path)?, std::fs::read(err_path)?))
}

fn successful_reply(status: ExitStatus, out: &[u8], err: &[u8]) -> Result<Value> {
    ensure!(
        status.success(),
        "{status}: {}",
        String::from_utf8_lossy(err)
    );
    ensure!(
        err.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(err)
    );
    let reply: Value = serde_json::from_slice(out)?;
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["ok"], true);
    assert!(out.ends_with(b"\n"));
    Ok(reply)
}

#[test]
fn semantic_one_shot_matches_stdio_and_preserves_identity_filters_and_supervision() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (database, conversation) = fixture(root.path(), true)?;
    let before = snapshot(root.path())?;
    let (status, out, err) = execute(
        root.path(),
        &database,
        &[
            "--query",
            TEXT,
            "--mode",
            "semantic",
            "--limit",
            "1",
            "--agent",
            "CODEX",
            "--workspace",
            WORKSPACE,
            "--source-id",
            "local",
            "--approximate",
            "--created-from",
            "1700000000000",
            "--created-to",
            "1700000000000",
        ],
        None,
    )?;
    let reply = successful_reply(status, &out, &err)?;
    let result = &reply["result"];
    assert_eq!(result["realized_mode"], "semantic");
    assert_eq!(result["count"], 1);
    assert_eq!(result["hits"][0]["conversation_id"], conversation);
    assert_eq!(result["hits"][0]["message_index"], 8);
    assert_eq!(result["hits"][0]["agent"], "codex");
    assert_eq!(result["hits"][0]["source_id"], "local");
    assert_eq!(result["hits"][0]["workspace"], WORKSPACE);
    assert!(result["hits"][0].get("content").is_none());
    assert_eq!(result["snapshot"]["open_attempts"], 0);
    assert_eq!(result["snapshot"]["semantic"]["successful_loads"], 1);
    assert_eq!(result["snapshot"]["semantic"]["neural"], false);
    assert_eq!(result["snapshot"]["memory_supervision"]["enabled"], true);
    assert_eq!(
        result["snapshot"]["limits"]["timeout_scope"],
        "validated_arguments_through_reader_teardown_and_response_flush"
    );
    assert_eq!(result["ann"]["requested"], true);
    assert_eq!(result["ann"]["used"], false);
    assert_eq!(
        result["ann"]["unavailable_reason"],
        "persistent_native_ann_requires_reclaimable_owners"
    );
    let frame = json!({"op": "semantic_search", "id": 1, "query": TEXT,
        "mode": "semantic", "limit": 1, "approximate": true,
        "filters": {"agents": ["CODEX"], "workspaces": [WORKSPACE], "source_id": "local",
            "created_from": CREATED, "created_to": CREATED}});
    let (status, out, err) = execute(root.path(), &database, &["--stdio"], Some(&frame))?;
    ensure!(
        status.success(),
        "{status}: {}",
        String::from_utf8_lossy(&err)
    );
    let replies = std::str::from_utf8(&out)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<Value>, _>>()?;
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["ok"], true);
    assert_eq!(replies[0]["result"]["hits"], result["hits"]);
    assert_eq!(
        replies[0]["result"]["realized_mode"],
        result["realized_mode"]
    );
    assert_eq!(snapshot(root.path())?, before);
    Ok(())
}

#[test]
fn hybrid_one_shot_uses_both_legs_and_filters_before_returning_the_page() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (database, conversation) = fixture(root.path(), true)?;
    let before = snapshot(root.path())?;
    for (workspace, expected) in [(WORKSPACE, 1), ("/other/workspace", 0)] {
        let (status, out, err) = execute(
            root.path(),
            &database,
            &[
                "--query",
                TEXT,
                "--mode",
                "hybrid",
                "--workspace",
                workspace,
                "--limit",
                "1",
            ],
            None,
        )?;
        let reply = successful_reply(status, &out, &err)?;
        assert_eq!(reply["result"]["realized_mode"], "hybrid");
        assert_eq!(reply["result"]["count"], expected);
        if expected > 0 {
            assert_eq!(reply["result"]["hits"][0]["conversation_id"], conversation);
        }
    }
    assert_eq!(snapshot(root.path())?, before);
    Ok(())
}

#[test]
fn missing_vectors_fail_semantic_but_hybrid_retains_explicit_lexical_fallback() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (database, conversation) = fixture(root.path(), false)?;
    let before = snapshot(root.path())?;
    let (status, out, err) = execute(
        root.path(),
        &database,
        &["--query", TEXT, "--mode", "semantic"],
        None,
    )?;
    assert_eq!(status.code(), Some(1));
    assert!(out.is_empty());
    let failure: Value = serde_json::from_slice(&err)?;
    assert_eq!(failure["ok"], false);
    assert_eq!(failure["error"]["kind"], "semantic_unavailable");
    let (status, out, err) = execute(
        root.path(),
        &database,
        &["--query", TEXT, "--mode", "hybrid"],
        None,
    )?;
    let reply = successful_reply(status, &out, &err)?;
    assert_eq!(reply["result"]["realized_mode"], "lexical");
    assert_eq!(reply["result"]["count"], 1);
    assert_eq!(reply["result"]["hits"][0]["conversation_id"], conversation);
    assert!(
        !reply["result"]["semantic_fallback_reason"]
            .as_str()
            .context("explicit hybrid fallback reason")?
            .is_empty()
    );
    assert_eq!(snapshot(root.path())?, before);
    Ok(())
}

#[test]
fn semantic_capability_does_not_change_the_default_lexical_query() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (database, conversation) = fixture(root.path(), false)?;
    let before = snapshot(root.path())?;
    let (status, out, err) = execute(root.path(), &database, &["--query", TEXT], None)?;
    let reply = successful_reply(status, &out, &err)?;
    assert_eq!(reply["result"]["backend"], "quill");
    assert_eq!(reply["result"]["hits"][0]["conversation_id"], conversation);
    assert_eq!(
        reply["result"]["snapshot"]["canonical_database_accessed"],
        false
    );
    assert_eq!(reply["result"]["snapshot"]["semantic"]["loaded"], false);
    assert_eq!(snapshot(root.path())?, before);
    Ok(())
}

#[test]
fn invalid_semantic_and_hybrid_scopes_fail_before_creating_storage() -> Result<()> {
    let root = tempfile::tempdir()?;
    let database = root.path().join("absent.db");
    for mode in ["semantic", "hybrid"] {
        for invalid in [
            vec!["--limit", "0"],
            vec!["--offset", "1024"],
            vec!["--agent", "codex", "--agent", "nonsense-agent"],
            vec!["--created-from", "2", "--created-to", "1"],
        ] {
            let mut args = vec!["--query", TEXT, "--mode", mode];
            args.extend(invalid);
            let (status, out, err) = execute(root.path(), &database, &args, None)?;
            assert_eq!(status.code(), Some(2));
            assert!(out.is_empty());
            let reply: Value = serde_json::from_slice(&err)?;
            assert_eq!(reply["error"]["kind"], "invalid_request");
            assert!(snapshot(root.path())?.is_empty());
        }
    }
    Ok(())
}
