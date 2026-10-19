//! GH #532: real-binary service scope admission and retrieval identity checks.
//! Use real Quill documents; no models, source discovery or canonical DB.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use coding_agent_search::search::tantivy::{TantivyIndex, expected_index_dir};
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

fn fixture(root: &Path) -> Result<PathBuf> {
    let index = expected_index_dir(root);
    let mut writer = TantivyIndex::open_or_create(&index)?;
    for (conversation_id, agent) in [(1, "claude_code"), (2, "claude_code"), (3, "codex")] {
        writer.add_prebuilt_documents_slice(&[CassDocument {
            agent: agent.into(),
            workspace: Some("/work".into()),
            workspace_original: None,
            // ubs:ignore[rust.perf.string-alloc-in-loop] -- This fixed three-conversation fixture needs distinct source identities.
            source_path: format!("/history/gh532/{conversation_id}.jsonl"),
            msg_idx: 0,
            created_at: Some(1_700_000_000_000 + conversation_id),
            // ubs:ignore[rust.perf.string-alloc-in-loop] -- This fixed three-conversation fixture needs distinct titles.
            title: Some(format!("scope fixture {conversation_id}")),
            // ubs:ignore[rust.perf.string-alloc-in-loop] -- This fixed three-conversation fixture needs distinct message bodies.
            content: format!("scopeprobe paper cuts in conversation {conversation_id}"),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
            conversation_id: Some(conversation_id),
        }])?;
    }
    writer.commit()?;
    drop(writer);
    Ok(index)
}

fn run(
    root: &Path,
    index: &Path,
    args: &[&str],
    frames: &[Value],
) -> Result<(ExitStatus, Vec<Value>, Vec<u8>)> {
    let capture = tempfile::tempdir()?;
    let stdout_path = capture.path().join("stdout");
    let stderr_path = capture.path().join("stderr");
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .args(["serve", "--index"])
        .arg(index)
        .args(args)
        .args(["--request-timeout-ms", "10000"])
        .current_dir(root)
        .env("HOME", root)
        .env("CASS_DATA_DIR", root)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_LOG", "error")
        .env_remove("CASS_DB_PATH")
        .env_remove("CASS_OUTPUT_FORMAT")
        .env_remove("TOON_DEFAULT_FORMAT")
        .env_remove("CASS_QUILL_QUERY_FUEL_BUDGET")
        .stdin(Stdio::piped())
        // Files, not undrained pipes: multiple hit pages cannot deadlock the
        // parent while it waits. Refuse oversized captures before allocating.
        .stdout(std::fs::File::create(&stdout_path)?)
        .stderr(std::fs::File::create(&stderr_path)?);
    let mut child = Child(command.spawn()?);
    let mut input = child.0.stdin.take().context("service stdin")?;
    for frame in frames {
        writeln!(input, "{frame}")?;
    }
    drop(input);
    let status = child
        .0
        .wait_timeout(Duration::from_secs(40))?
        .context("service failed to exit under its external test deadline")?;
    for path in [&stdout_path, &stderr_path] {
        ensure!(
            std::fs::metadata(path)?.len() <= 1024 * 1024,
            "oversized test output"
        );
    }
    let stdout = std::fs::read_to_string(stdout_path)?;
    let stderr = std::fs::read(stderr_path)?;
    let replies = stdout
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<Value>, _>>()?;
    Ok((status, replies, stderr))
}

fn identities(result: &Value) -> Result<BTreeSet<i64>> {
    result["hits"]
        .as_array()
        .context("result hits")?
        .iter()
        .map(|hit| {
            hit["conversation_id"]
                .as_i64()
                .context("conversation identity")
        })
        .collect()
}

fn search(id: u64, agents: &[&str]) -> Value {
    json!({"id": id, "op": "search", "query": "scopeprobe", "filters": {"agents": agents}})
}

#[test]
fn stdio_aliases_union_and_known_absent_agents_preserve_actual_hit_sets() -> Result<()> {
    let root = tempfile::tempdir()?;
    let index = fixture(root.path())?;
    let (status, replies, stderr) = run(
        root.path(),
        &index,
        &["--stdio"],
        &[
            search(1, &["claude_code"]),
            search(2, &["claude"]),
            search(3, &["claude-code"]),
            search(4, &["Claude_Code"]),
            search(5, &[" CLAUDE-CODE "]),
            search(6, &["claude", "claude-code", "CODEX"]),
            search(7, &["aider"]),
            json!({"id": 8, "op": "status"}),
        ],
    )?;
    ensure!(
        status.success(),
        "{status}: {}",
        String::from_utf8_lossy(&stderr)
    );
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies.len(), 8);
    for reply in &replies[..7] {
        // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
        assert_eq!(reply["ok"], true, "{reply}");
        // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
        assert_eq!(reply["result"]["backend"], "quill");
    }
    for reply in &replies[..5] {
        // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
        assert_eq!(identities(&reply["result"])?, BTreeSet::from([1, 2]));
    }
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(
        identities(&replies[5]["result"])?,
        BTreeSet::from([1, 2, 3])
    );
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert!(identities(&replies[6]["result"])?.is_empty());
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies[7]["result"]["successful_opens"], 1);
    Ok(())
}

#[test]
fn invalid_json_scopes_fail_before_reader_admission_and_do_not_poison_later_requests() -> Result<()>
{
    let root = tempfile::tempdir()?;
    let index = fixture(root.path())?;
    let (status, replies, stderr) = run(
        root.path(),
        &index,
        &["--stdio"],
        &[
            search(1, &["nonsense-agent"]),
            search(2, &["claude", "claud_code"]),
            json!({"id": 3, "op": "semantic_search", "query": "scopeprobe",
                "filters": {"agents": ["nonsense-agent"]}}),
            json!({"id": 4, "op": "refine", "query": "scopeprobe", "lexical_query": "scopeprobe",
                "filters": {"agents": ["codex", "nonsense-agent"]}}),
            json!({"id": 5, "op": "status"}),
            search(6, &["claude"]),
            json!({"id": 7, "op": "status"}),
        ],
    )?;
    ensure!(
        status.success(),
        "{status}: {}",
        String::from_utf8_lossy(&stderr)
    );
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies.len(), 7);
    for reply in &replies[..4] {
        // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
        assert_eq!(reply["ok"], false, "{reply}");
        // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unknown agent filter")
        );
        // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
        assert!(reply.get("result").is_none());
    }
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies[4]["result"]["successful_opens"], 0);
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies[4]["result"]["canonical_database_accessed"], false);
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies[5]["ok"], true);
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(identities(&replies[5]["result"])?, BTreeSet::from([1, 2]));
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies[6]["result"]["successful_opens"], 1);
    Ok(())
}

#[test]
fn mcp_agent_aliases_retrieve_hits_and_unknown_names_are_invalid_params() -> Result<()> {
    let root = tempfile::tempdir()?;
    let index = fixture(root.path())?;
    let (status, replies, stderr) = run(
        root.path(),
        &index,
        &["--stdio", "--mcp"],
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "gh532-test", "version": "1"}}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "cass_search", "arguments": {
                    "query": "scopeprobe", "filters": {"agents": ["claude-code"]}}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                "name": "cass_search", "arguments": {
                    "query": "scopeprobe", "filters": {"agents": ["claude", "claud_code"]}}}}),
        ],
    )?;
    ensure!(
        status.success(),
        "{status}: {}",
        String::from_utf8_lossy(&stderr)
    );
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies.len(), 3);
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies[1]["id"], 2);
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies[1]["result"]["isError"], false);
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(
        identities(&replies[1]["result"]["structuredContent"])?,
        BTreeSet::from([1, 2])
    );
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies[2]["id"], 3);
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(replies[2]["error"]["code"], -32602);
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert!(
        replies[2]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown agent filter")
    );
    Ok(())
}

#[test]
fn one_shot_agent_aliases_retrieve_hits_and_invalid_names_exit_two_before_storage() -> Result<()> {
    let root = tempfile::tempdir()?;
    let index = fixture(root.path())?;
    for alias in ["claude", "claude-code", "Claude_Code"] {
        let (status, replies, stderr) = run(
            root.path(),
            &index,
            &["--query", "scopeprobe", "--agent", alias],
            &[],
        )?;
        ensure!(
            status.success(),
            "{alias}: {status}: {}",
            String::from_utf8_lossy(&stderr)
        );
        // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
        assert_eq!(replies.len(), 1);
        // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
        assert_eq!(replies[0]["ok"], true);
        // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
        assert_eq!(identities(&replies[0]["result"])?, BTreeSet::from([1, 2]));
    }
    let missing = root.path().join("must-not-open");
    let (status, replies, stderr) = run(
        root.path(),
        &missing,
        &[
            "--query",
            "scopeprobe",
            "--agent",
            "codex",
            "--agent",
            "claud_code",
        ],
        &[],
    )?;
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(status.code(), Some(2));
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert!(
        replies.is_empty(),
        "invalid scope emitted a success payload"
    );
    let error: Value = serde_json::from_slice(&stderr)?;
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert_eq!(error["ok"], false);
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("claud_code")
    );
    // ubs:ignore[rust.panic.assert-macros] -- Test assertions must fail when the agent-scope contract regresses.
    assert!(!missing.exists());
    Ok(())
}
