//! Exercise the real binary module and its Quill-backed session regressions.
// The run entrypoint is exercised through the real binary below.
#[allow(dead_code)]
#[path = "../src/search_service.rs"]
mod search_service;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;

#[test]
fn cass_serve_dispatches_stdio_without_entering_ordinary_cli_setup() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let absent = temp.path().join("explicit-never-opened-index");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["serve", "--stdio", "--index"])
        .arg(&absent)
        .env("CASS_DATA_DIR", temp.path().join("unrelated"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    input.write_all(b"{\"op\":\"status\",\"id\":1}\n{\"op\":\"shutdown\",\"id\":2}\n")?;
    drop(input);
    if child.wait_timeout(Duration::from_secs(20))?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("stdio service did not terminate after shutdown");
    }
    let output = child.wait_with_output()?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let replies: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)?.lines()
        .map(serde_json::from_str).collect::<Result<_, _>>()?;
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[0]["result"]["loaded"], false);
    assert_eq!(replies[0]["result"]["open_attempts"], 0);
    assert_eq!(replies[1]["result"]["shutdown"], true);
    assert!(!absent.exists());
    assert!(!temp.path().join("unrelated").exists());
    Ok(())
}

#[test]
fn cass_serve_help_is_available_without_archive_access() {
    let output = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["serve", "--help"])
        .output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for flag in ["--index", "--data-dir", "--stdio"] {
        assert!(help.contains(flag), "{help}");
    }
}

#[test]
fn cass_serve_searches_repeatedly_through_the_real_cli() -> anyhow::Result<()> {
    use coding_agent_search::search::tantivy::{TantivyIndex, expected_index_dir};
    use frankensearch::quill::cass::CassDocument;

    let temp = tempfile::tempdir()?;
    let path = expected_index_dir(temp.path());
    let mut writer = TantivyIndex::open_or_create(&path)?;
    writer.add_prebuilt_documents_slice(&[CassDocument {
        agent: "codex".into(),
        workspace: Some("/work".into()),
        workspace_original: None,
        source_path: "/history/session.jsonl".into(),
        msg_idx: 12,
        created_at: Some(1_700_000_000_000),
        title: Some("fixture".into()),
        content: "retainedreaderneedle".into(),
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
        conversation_id: Some(42),
    }])?;
    writer.commit()?;
    drop(writer);
    std::fs::write(temp.path().join("agent_search.db"), b"must not be opened")?;
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["serve", "--stdio", "--data-dir"])
        .arg(temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    for id in [1, 2] {
        writeln!(input, "{}", serde_json::json!({
            "op": "search", "id": id, "query": "retainedreaderneedle", "limit": 1
        }))?;
    }
    // EOF is also a clean lifecycle boundary; no explicit shutdown required.
    drop(input);
    if child.wait_timeout(Duration::from_secs(30))?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("search worker did not terminate after EOF");
    }
    let output = child.wait_with_output()?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let replies: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)?.lines()
        .map(serde_json::from_str).collect::<Result<_, _>>()?;
    assert_eq!(replies.len(), 2);
    for (position, reply) in replies.iter().enumerate() {
        assert_eq!(reply["ok"], true, "{reply}");
        let result = &reply["result"];
        assert_eq!(result["count"], 1);
        assert_eq!(result["reader_reused"], position > 0);
        assert_eq!(result["snapshot"]["successful_opens"], 1);
        assert_eq!(result["hits"][0]["conversation_id"], 42);
        assert_eq!(result["hits"][0]["message_index"], 13);
        assert_eq!(result["hits"][0]["source_id"], "local");
    }
    assert_eq!(std::fs::read(temp.path().join("agent_search.db"))?, b"must not be opened");
    Ok(())
}

#[test]
fn cass_serve_mcp_dispatches_real_negotiation_without_archive_access() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let absent = temp.path().join("never-opened");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("cass"))
        .args(["serve", "--stdio", "--mcp", "--index"])
        .arg(&absent)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    for request in [
        serde_json::json!({"jsonrpc": "2.0", "id": "handshake", "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "fixture", "version": "1"}}}),
        serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        serde_json::json!({"jsonrpc": "2.0", "id": "catalog", "method": "tools/list"}),
        serde_json::json!({"jsonrpc": "2.0", "id": "status", "method": "tools/call",
            "params": {"name": "cass_status", "arguments": {}}}),
    ] {
        writeln!(input, "{request}")?;
    }
    drop(input);
    if child.wait_timeout(Duration::from_secs(20))?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("MCP service did not terminate after EOF");
    }
    let output = child.wait_with_output()?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let replies: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)?.lines()
        .map(serde_json::from_str).collect::<Result<_, _>>()?;
    assert_eq!(replies.len(), 3, "notifications get no response");
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 3);
    assert_eq!(replies[2]["id"], "status");
    assert_eq!(replies[2]["result"]["structuredContent"]["open_attempts"], 0);
    assert!(!absent.exists());
    Ok(())
}
