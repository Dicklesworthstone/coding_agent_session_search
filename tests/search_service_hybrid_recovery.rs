//! Real retained hash-semantic + Quill + canonical archive recovery.
//! Fuel is configured only in child environments; no mocked search or model.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

const QUERY: &str = "git fixture";
const TIME: i64 = 1_700_000_000_000;

struct Fixture {
    _root: tempfile::TempDir,
    data: PathBuf,
    archive: PathBuf,
    identities: Vec<i64>,
}

fn fixture(lexical_matches: bool) -> Result<Fixture> {
    let root = tempfile::tempdir()?;
    let data = root.path().join("data");
    std::fs::create_dir(&data)?;
    let archive = data.join("archive.db");
    let storage = FrankenStorage::open(&archive)?;
    let embedder = HashEmbedder::default();
    let vector_path = vector_index_path(&data, embedder.id());
    std::fs::create_dir_all(vector_path.parent().context("vector parent")?)?;
    let mut vectors = VectorIndex::create_with_revision(
        &vector_path,
        embedder.id(),
        coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION,
        embedder.dimension(),
        frankensearch::index::Quantization::F16,
    )?;
    let mut lexical = TantivyIndex::open_or_create(&expected_index_dir(&data))?;
    let mut identities = Vec::new();
    for (ordinal, (slug, workspace, content)) in [
        (
            "codex",
            "/work/δ",
            "git fixture persistent retained vector search",
        ),
        (
            "claude_code",
            "/work/other",
            "git fixture isolated repository testing",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let agent_id = storage.ensure_agent(&Agent {
            id: None,
            slug: slug.into(),
            name: slug.into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let workspace = PathBuf::from(workspace);
        let workspace_id = storage.ensure_workspace(&workspace, None)?;
        let source_path = root.path().join(format!("never-created-{ordinal}.jsonl"));
        let idx = 7 + i64::try_from(ordinal)? * 5;
        let timestamp = TIME + i64::try_from(ordinal)? * 1_000;
        let title = format!("Hybrid case {ordinal}");
        let outcome = storage.insert_conversation_tree(
            agent_id,
            Some(workspace_id),
            &Conversation {
                id: None,
                agent_slug: slug.into(),
                workspace: Some(workspace.clone()),
                external_id: Some(format!("hybrid-recovery-{ordinal}")),
                title: Some(title.clone()),
                source_path: source_path.clone(),
                started_at: Some(timestamp),
                ended_at: None,
                approx_tokens: None,
                metadata_json: json!({}),
                source_id: "local".into(),
                origin_host: None,
                messages: vec![Message {
                    id: None,
                    idx,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: Some(timestamp),
                    content: content.into(),
                    extra_json: json!({}),
                    snippets: Vec::new(),
                }],
            },
        )?;
        let messages = storage.fetch_messages(outcome.conversation_id)?;
        ensure!(messages.len() == 1, "fixture must hold one exact message");
        let message_id = messages[0].id.context("canonical message ID")?;
        let doc_id = SemanticDocId {
            message_id: u64::try_from(message_id)?,
            chunk_idx: 0,
            agent_id: u32::try_from(agent_id)?,
            workspace_id: u32::try_from(workspace_id)?,
            source_id: crc32fast::hash(b"local"),
            role: 1,
            created_at_ms: timestamp,
            content_hash: None,
        }
        .to_doc_id_string();
        vectors.write_record(&doc_id, &embedder.embed_sync(content)?)?;
        lexical.add_prebuilt_documents_slice(&[CassDocument {
            agent: slug.into(),
            workspace: Some(workspace.to_string_lossy().into_owned()),
            workspace_original: None,
            source_path: source_path.to_string_lossy().into_owned(),
            msg_idx: u64::try_from(idx)?,
            created_at: Some(timestamp),
            title: Some(title),
            content: if lexical_matches { content } else { "banana orchard" }.into(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
            conversation_id: Some(outcome.conversation_id),
        }])?;
        lexical.commit()?;
        identities.push(outcome.conversation_id);
        assert!(!source_path.exists(), "raw source must never be needed");
    }
    vectors.finish()?;
    drop(lexical);
    drop(storage);
    Ok(Fixture {
        _root: root,
        data,
        archive,
        identities,
    })
}

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn captured(file: &mut File, maximum: u64) -> Result<String> {
    ensure!(
        file.metadata()?.len() <= maximum,
        "child output exceeded capture budget"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

fn serve(fixture: &Fixture, enabled: bool, fuel: u64, requests: &[Value]) -> Result<Vec<Value>> {
    ensure!(requests.len() <= 8, "bounded fixture exchange");
    // Output is file-backed so waiting for exit cannot deadlock on full pipes.
    // Bound materialization before reading it. These are tiny synthetic assets;
    // the external timeout remains separate from the service's own watchdog.
    let control = tempfile::tempdir()?;
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .args(["serve", "--stdio", "--data-dir"])
        .arg(&fixture.data)
        .arg("--db")
        .arg(&fixture.archive)
        .args([
            "--semantic-embedder",
            "hash",
            "--request-timeout-ms",
            "20000",
        ])
        .args([
            "--max-resident-mib",
            "2048",
            "--admission-slots",
            "1",
            "--admission-dir",
        ])
        .arg(control.path().join("pool"))
        .current_dir(control.path())
        .env("HOME", control.path())
        .env("XDG_CONFIG_HOME", control.path().join("config"))
        .env("XDG_CACHE_HOME", control.path().join("cache"))
        .env("CASS_QUILL_QUERY_FUEL_BUDGET", fuel.to_string())
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    if enabled {
        command.arg("--lexical-fallback");
    }
    let mut child = Child(command.spawn()?);
    let mut input = child.0.stdin.take().context("child stdin")?;
    for request in requests {
        writeln!(input, "{request}")?;
    }
    drop(input);
    let status = child
        .0
        .wait_timeout(Duration::from_secs(45))?
        .context("hybrid service failed to terminate")?;
    let stdout = captured(&mut stdout, 2 * 1024 * 1024)?;
    let stderr = captured(&mut stderr, 256 * 1024)?;
    ensure!(status.success(), "service exited {status}: {stderr}");
    let replies: Vec<Value> = stdout
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    ensure!(
        replies.len() == requests.len(),
        "incomplete response exchange: {stdout}"
    );
    Ok(replies)
}

fn inventory(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut result = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if entry.file_type().is_file() {
            result.insert(
                entry.path().strip_prefix(root)?.to_path_buf(),
                std::fs::read(entry.path())?,
            );
        }
    }
    Ok(result)
}

fn hybrid(id: u64) -> Value {
    json!({"op":"semantic_search", "id":id, "query":QUERY, "mode":"hybrid", "limit":2})
}

#[test]
fn hybrid_fuel_refusal_recovers_the_lexical_leg_only_with_explicit_consent() -> Result<()> {
    let fixture = fixture(true)?;
    let before = inventory(&fixture.data)?;
    let requests = [hybrid(1), hybrid(2), json!({"op":"unload", "id":3})];
    for enabled in [false, true] {
        let replies = serve(&fixture, enabled, 1, &requests)?;
        for (ordinal, reply) in replies[..2].iter().enumerate() {
            assert_eq!(reply["ok"], true, "{reply}");
            let result = &reply["result"];
            assert_eq!(result["count"], 2, "{result}");
            assert_eq!(result["requested_mode"], "hybrid");
            assert_eq!(result["realized_mode"], if enabled { "hybrid" } else { "semantic" });
            assert_eq!(result["semantic_reused"], ordinal != 0);
            assert_eq!(result["degraded"], true);
            assert!(result["lexical_result_reader_epoch"].is_null());
            assert_eq!(result["snapshot"]["successful_opens"], 1);
            assert_eq!(result["snapshot"]["semantic"]["successful_loads"], 1);
            assert_eq!(result["snapshot"]["reader_admission"]["lease_held"], true);
            assert_eq!(result["snapshot"]["lexical_fallback"]["completed"], if enabled { ordinal + 1 } else { 0 });
            if enabled {
                assert_eq!(result["lexical_backend"], "frankensqlite");
                assert_eq!(result["lexical_fallback"]["reason"], "quill_query_fuel_exhausted");
                assert_eq!(result["lexical_fallback"]["shares_request_deadline"], true);
                assert_eq!(result["lexical_fallback"]["ranking_equivalent_to_quill"], false);
                assert!(result["lexical_degrade_reason"].is_null());
            } else {
                assert!(result["lexical_backend"].is_null());
                assert!(result["lexical_fallback"].is_null());
                assert!(result["lexical_degrade_reason"].as_str().context("native refusal")?
                    .contains("query fuel exhausted"));
            }
            for hit in result["hits"].as_array().context("hits")? {
                assert!(fixture.identities.contains(&hit["conversation_id"].as_i64().context("identity")?));
                assert_eq!(hit["source_id"], "local");
                assert!(hit.get("content").is_none());
            }
        }
        assert_eq!(replies[2]["ok"], true);
        assert_eq!(replies[2]["result"]["reader_admission"]["lease_held"], false);
        assert_eq!(replies[2]["result"]["semantic"]["loaded"], false);
        assert_eq!(inventory(&fixture.data)?, before);
    }
    Ok(())
}

#[test]
fn hybrid_authoritative_empty_quill_leg_does_not_search_the_archive() -> Result<()> {
    let fixture = fixture(false)?;
    let before = inventory(&fixture.data)?;
    let replies = serve(&fixture, true, 10_000_000, &[
        json!({"op":"search", "id":1, "query":QUERY, "limit":2}),
        hybrid(2),
    ])?;
    assert_eq!(replies[0]["ok"], true, "{}", replies[0]);
    assert_eq!(replies[0]["result"]["count"], 0, "prove the native leg is empty");
    assert_eq!(replies[0]["result"]["backend"], "quill");
    assert_eq!(replies[1]["ok"], true, "{}", replies[1]);
    let result = &replies[1]["result"];
    assert_eq!(result["realized_mode"], "hybrid");
    assert_eq!(result["count"], 2, "semantic candidates remain usable");
    assert_eq!(result["lexical_backend"], "quill");
    assert_eq!(result["lexical_result_reader_epoch"], 1);
    assert_eq!(result["degraded"], false);
    assert!(result["lexical_fallback"].is_null());
    assert_eq!(result["snapshot"]["lexical_fallback"]["attempts"], 0);
    assert_eq!(inventory(&fixture.data)?, before);
    Ok(())
}

#[test]
fn semantic_only_does_not_open_quill_or_invoke_lexical_recovery() -> Result<()> {
    let fixture = fixture(true)?;
    let before = inventory(&fixture.data)?;
    let replies = serve(&fixture, true, 1, &[
        json!({"op":"semantic_search", "id":1, "query":QUERY, "mode":"semantic", "limit":2}),
    ])?;
    assert_eq!(replies[0]["ok"], true, "{}", replies[0]);
    let result = &replies[0]["result"];
    assert_eq!(result["realized_mode"], "semantic");
    assert_eq!(result["degraded"], false);
    assert!(result["lexical_backend"].is_null());
    assert!(result["lexical_fallback"].is_null());
    assert!(result["lexical_result_reader_epoch"].is_null());
    assert_eq!(result["snapshot"]["successful_opens"], 0);
    assert_eq!(result["snapshot"]["lexical_fallback"]["attempts"], 0);
    assert_eq!(inventory(&fixture.data)?, before);
    Ok(())
}

#[test]
fn recovered_hybrid_candidates_keep_agent_workspace_source_and_time_scope() -> Result<()> {
    let fixture = fixture(true)?;
    let before = inventory(&fixture.data)?;
    // Each individual positive filter excludes the other real archive/vector
    // row. The combined request must not widen any one dimension in recovery.
    let selections = [
        json!({"agents":["codex"]}),
        json!({"workspaces":["/work/δ"]}),
        json!({"created_from":TIME, "created_to":TIME + 500}),
        json!({"agents":["codex"], "workspaces":["/work/δ"], "source_id":"local",
            "created_from":TIME, "created_to":TIME + 500}),
    ];
    let mut requests = Vec::new();
    for (ordinal, filters) in selections.into_iter().enumerate() {
        let mut request = hybrid(u64::try_from(ordinal)? + 1);
        request["filters"] = filters;
        requests.push(request);
    }
    let mut absent_source = hybrid(5);
    absent_source["filters"] = json!({"source_id":"different-source"});
    requests.push(absent_source);
    let replies = serve(&fixture, true, 1, &requests)?;
    for (ordinal, reply) in replies[..4].iter().enumerate() {
        assert_eq!(reply["ok"], true, "{reply}");
        let result = &reply["result"];
        assert_eq!(result["realized_mode"], "hybrid");
        assert_eq!(result["lexical_backend"], "frankensqlite");
        assert_eq!(result["degraded"], true);
        assert_eq!(result["count"], 1, "{result}");
        assert_eq!(result["hits"][0]["conversation_id"], fixture.identities[0]);
        assert_eq!(result["hits"][0]["message_index"], 8);
        assert_eq!(result["hits"][0]["source_id"], "local");
        assert!(result["lexical_result_reader_epoch"].is_null());
        assert_eq!(result["snapshot"]["lexical_fallback"]["completed"], ordinal + 1);
    }
    assert_eq!(replies[4]["ok"], true, "{}", replies[4]);
    assert_eq!(replies[4]["result"]["count"], 0, "source scope must not widen");
    assert_eq!(inventory(&fixture.data)?, before);
    Ok(())
}

#[test]
fn recovered_hybrid_pages_apply_offset_after_fusion_without_duplicates() -> Result<()> {
    let fixture = fixture(true)?;
    let before = inventory(&fixture.data)?;
    let replies = serve(&fixture, true, 1, &[
        hybrid(1),
        json!({"op":"semantic_search", "id":2, "query":QUERY,
            "mode":"hybrid", "limit":1, "offset":0}),
        json!({"op":"semantic_search", "id":3, "query":QUERY,
            "mode":"hybrid", "limit":1, "offset":1}),
    ])?;
    for reply in &replies {
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["result"]["realized_mode"], "hybrid");
        assert_eq!(reply["result"]["lexical_backend"], "frankensqlite");
    }
    let all = replies[0]["result"]["hits"].as_array().context("whole page")?;
    assert_eq!(all.len(), 2);
    assert_ne!(all[0]["conversation_id"], all[1]["conversation_id"]);
    for ordinal in 0..2 {
        let page = &replies[ordinal + 1]["result"];
        assert_eq!(page["count"], 1);
        assert_eq!(page["offset"], ordinal);
        assert_eq!(page["hits"][0], all[ordinal], "offset must follow fusion");
    }
    assert_eq!(replies[1]["result"]["has_more"], true);
    assert_eq!(replies[1]["result"]["next_offset"], 1);
    assert!(replies[2]["result"]["has_more"].is_null());
    assert!(replies[2]["result"]["next_offset"].is_null());
    assert_eq!(replies[2]["result"]["snapshot"]["lexical_fallback"]["completed"], 3);
    assert_eq!(inventory(&fixture.data)?, before);
    Ok(())
}

#[test]
fn missing_quill_index_keeps_semantic_results_without_authorizing_archive_recovery() -> Result<()> {
    let fixture = fixture(true)?;
    let expected = expected_index_dir(&fixture.data);
    // Preserve the complete fixture index under a different path. A missing
    // reader is not a fuel refusal and cannot authorize the archive lane.
    std::fs::rename(&expected, fixture.data.join("retained-lexical-index"))?;
    let before = inventory(&fixture.data)?;
    let replies = serve(&fixture, true, 1, &[hybrid(1)])?;
    assert_eq!(replies[0]["ok"], true, "{}", replies[0]);
    let result = &replies[0]["result"];
    assert_eq!(result["realized_mode"], "semantic");
    assert_eq!(result["degraded"], true);
    assert_eq!(result["count"], 2);
    assert!(result["lexical_backend"].is_null());
    assert!(result["lexical_fallback"].is_null());
    let reason = result["lexical_degrade_reason"].as_str().context("reader error")?;
    assert!(!reason.contains("query fuel exhausted"), "{reason}");
    assert_eq!(result["snapshot"]["successful_opens"], 0);
    assert_eq!(result["snapshot"]["lexical_fallback"]["attempts"], 0);
    assert!(!expected.exists(), "search must never recreate the missing index");
    assert_eq!(inventory(&fixture.data)?, before);
    Ok(())
}
