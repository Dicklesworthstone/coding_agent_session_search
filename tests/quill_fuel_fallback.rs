//! GH518: real Quill refusal -> explicitly opted-in, supervised SQLite recovery.
//! No environment mutation in the test process: fuel belongs to each child.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::quill_bridge::live_segment_count;
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

fn serve(
    root: &Path,
    index: &Path,
    archive: Option<&Path>,
    enabled: bool,
    fuel: u64,
    requests: &[Value],
) -> Result<Vec<Value>> {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .args(["serve", "--stdio", "--index"])
        .arg(index)
        .current_dir(root)
        .env("CASS_QUILL_QUERY_FUEL_BUDGET", fuel.to_string())
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(archive) = archive {
        command.arg("--db").arg(archive);
    }
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
        .wait_timeout(Duration::from_secs(40))?
        .context("service did not terminate after EOF")?;
    // This fixture sends only a few small responses, well below pipe capacity.
    // Read after exit; the Drop guard reaps on every early failure as well.
    let mut stdout = String::new();
    let mut stderr = String::new();
    use std::io::Read;
    child
        .0
        .stdout
        .take()
        .context("child stdout")?
        .read_to_string(&mut stdout)?;
    child
        .0
        .stderr
        .take()
        .context("child stderr")?
        .read_to_string(&mut stderr)?;
    ensure!(status.success(), "worker exited {status}: {stderr}");
    stdout
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn document(conversation_id: i64, ordinal: usize, content: &str) -> CassDocument {
    CassDocument {
        agent: "codex".into(),
        workspace: Some("/work".into()),
        workspace_original: None,
        source_path: format!("/history/gh518/{ordinal}.jsonl"),
        msg_idx: 0,
        created_at: Some(1_700_000_000_000 + ordinal as i64),
        title: Some(format!("gh518 case {ordinal}")),
        content: content.into(),
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: Some(conversation_id),
    }
}

fn fixture(root: &Path) -> Result<(PathBuf, PathBuf, Vec<i64>)> {
    let archive = root.join("archive.db");
    let index = expected_index_dir(root);
    let storage = FrankenStorage::open(&archive)?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let mut writer = TantivyIndex::open_or_create(&index)?;
    let mut identities = Vec::new();
    for (ordinal, content) in [
        "git fixture establishes the repository test environment",
        "test isolation prevents state from leaking between cases",
    ]
    .iter()
    .enumerate()
    {
        let inserted = storage.insert_conversation_tree(
            agent,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some(format!("gh518-{ordinal}")),
                title: Some(format!("gh518 case {ordinal}")),
                source_path: format!("/history/gh518/{ordinal}.jsonl").into(),
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
                    created_at: Some(1_700_000_000_000 + ordinal as i64),
                    content: (*content).into(),
                    extra_json: json!({}),
                    snippets: Vec::new(),
                }],
            },
        )?;
        writer.add_prebuilt_documents_slice(&[document(
            inserted.conversation_id,
            ordinal,
            content,
        )])?;
        identities.push(inserted.conversation_id);
    }
    writer.commit()?;
    drop(writer);
    drop(storage);
    Ok((index, archive, identities))
}

#[test]
fn gh518_fuel_refusal_uses_explicit_archive_and_reports_actual_backend() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (index, archive, identities) = fixture(temp.path())?;
    let before = std::fs::read(&archive)?;
    let replies = serve(
        temp.path(),
        &index,
        Some(&archive),
        true,
        1,
        &[
            json!({"id":1,"op":"search","query":"git fixture","limit":1}),
            json!({"id":2,"op":"search","query":"test isolation","limit":1}),
            json!({"id":3,"op":"status"}),
        ],
    )?;
    assert_eq!(replies.len(), 3);
    for (ordinal, reply) in replies[..2].iter().enumerate() {
        assert_eq!(reply["ok"], true, "{reply}");
        let result = &reply["result"];
        assert_eq!(result["backend"], "frankensqlite");
        assert_eq!(result["degraded"], true);
        assert_eq!(result["reader_reused"], false);
        assert!(result["result_reader_epoch"].is_null());
        assert_eq!(result["fallback"]["reason"], "quill_query_fuel_exhausted");
        assert!(result["fallback"]["sqlite_lane"].is_null());
        assert_eq!(result["count"], 1);
        assert_eq!(result["hits"][0]["conversation_id"], identities[ordinal]);
        assert_eq!(result["hits"][0]["message_index"], 1);
    }
    let state = &replies[2]["result"];
    assert_eq!(state["successful_opens"], 1);
    assert_eq!(state["lexical_fallback"]["attempts"], 2);
    assert_eq!(state["lexical_fallback"]["completed"], 2);
    assert_eq!(state["canonical_database_accessed"], true);
    assert_eq!(state["maintenance_performed"], false);
    assert_eq!(std::fs::read(&archive)?, before);
    Ok(())
}

#[test]
fn gh518_db_option_alone_does_not_grant_fallback_consent() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (index, _, _) = fixture(temp.path())?;
    let corrupt = temp.path().join("must-not-open.db");
    std::fs::write(&corrupt, b"not SQLite")?;
    let replies = serve(
        temp.path(),
        &index,
        Some(&corrupt),
        false,
        1,
        &[
            json!({"id":1,"op":"search","query":"git fixture","limit":1}),
            json!({"id":2,"op":"status"}),
        ],
    )?;
    assert_eq!(replies[0]["ok"], false);
    let message = replies[0]["error"]["message"].as_str().unwrap();
    assert!(message.contains("fallback is disabled"), "{message}");
    assert!(message.contains("query fuel exhausted"), "{message}");
    assert!(message.contains("cass index --full --json"), "{message}");
    assert_eq!(replies[1]["result"]["canonical_database_accessed"], false);
    assert_eq!(replies[1]["result"]["lexical_fallback"]["attempts"], 0);
    assert_eq!(std::fs::read(&corrupt)?, b"not SQLite");
    Ok(())
}

#[test]
fn gh518_oversized_and_corrupt_fallbacks_preserve_the_original_refusal() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (index, _, _) = fixture(temp.path())?;
    let oversized = temp.path().join("oversized.db");
    std::fs::File::create(&oversized)?.set_len(64 * 1024 * 1024 + 1)?;
    let corrupt = temp.path().join("corrupt.db");
    std::fs::write(&corrupt, b"not SQLite")?;
    for path in [&oversized, &corrupt] {
        let replies = serve(
            temp.path(),
            &index,
            Some(path),
            true,
            1,
            &[
                json!({"id":1,"op":"search","query":"git fixture","limit":1}),
                json!({"id":2,"op":"status"}),
            ],
        )?;
        assert_eq!(replies[0]["ok"], false, "{}", replies[0]);
        let message = replies[0]["error"]["message"].as_str().unwrap();
        assert!(message.contains("query fuel exhausted"), "{message}");
        assert!(message.contains("fallback failed"), "{message}");
        assert!(message.contains("cass index --full --json"), "{message}");
        if path == &oversized {
            assert!(
                message.contains("exceeds the automatic search budget"),
                "{message}"
            );
        }
        assert_eq!(replies[1]["result"]["lexical_fallback"]["completed"], 0);
    }
    Ok(())
}

#[test]
fn gh518_authoritative_empty_result_never_falls_back_to_archive_matches() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (_, archive, _) = fixture(temp.path())?;
    let index = temp.path().join("different-published-snapshot");
    let mut writer = TantivyIndex::open_or_create(&index)?;
    writer.add_prebuilt_documents_slice(&[document(99, 99, "unrelated corpus")])?;
    writer.commit()?;
    drop(writer);
    let replies = serve(
        temp.path(),
        &index,
        Some(&archive),
        true,
        10_000_000,
        &[
            json!({"id":1,"op":"search","query":"git fixture","limit":1}),
            json!({"id":2,"op":"status"}),
        ],
    )?;
    assert_eq!(replies[0]["ok"], true, "{}", replies[0]);
    assert_eq!(replies[0]["result"]["count"], 0);
    assert_eq!(replies[0]["result"]["backend"], "quill");
    assert_eq!(replies[0]["result"]["degraded"], false);
    assert_eq!(replies[1]["result"]["lexical_fallback"]["attempts"], 0);
    assert_eq!(replies[1]["result"]["canonical_database_accessed"], false);
    Ok(())
}

#[test]
fn gh518_206_real_segments_serve_both_two_word_queries_without_fallback() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let index = expected_index_dir(temp.path());
    let mut writer = TantivyIndex::open_or_create(&index)?;
    for segment in 0..206 {
        writer.add_prebuilt_documents_slice(&[document(
            segment as i64 + 1,
            segment,
            &format!("git fixture test isolation segment{segment}"),
        )])?;
        writer.commit()?;
    }
    drop(writer);
    assert_eq!(
        live_segment_count(&index),
        Some(206),
        "fixture must remain fragmented"
    );
    let replies = serve(
        temp.path(),
        &index,
        None,
        false,
        80_000,
        &[
            json!({"id":1,"op":"search","query":"git fixture","limit":1}),
            json!({"id":2,"op":"search","query":"test isolation","limit":1}),
        ],
    )?;
    assert_eq!(replies.len(), 2);
    for reply in &replies {
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["result"]["count"], 1);
        assert_eq!(reply["result"]["backend"], "quill");
        assert_eq!(reply["result"]["degraded"], false);
        assert_eq!(
            reply["result"]["snapshot"]["lexical_fallback"]["attempts"],
            0
        );
    }
    Ok(())
}
