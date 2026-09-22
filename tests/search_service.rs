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
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
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
        .output()
        .unwrap();
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
        writeln!(
            input,
            "{}",
            serde_json::json!({
                "op": "search", "id": id, "query": "retainedreaderneedle", "limit": 1
            })
        )?;
    }
    // EOF is also a clean lifecycle boundary; no explicit shutdown required.
    drop(input);
    if child.wait_timeout(Duration::from_secs(30))?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("search worker did not terminate after EOF");
    }
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
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
    assert_eq!(
        std::fs::read(temp.path().join("agent_search.db"))?,
        b"must not be opened"
    );
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
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<serde_json::Value> = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(replies.len(), 3, "notifications get no response");
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 3);
    assert_eq!(replies[2]["id"], "status");
    assert_eq!(
        replies[2]["result"]["structuredContent"]["open_attempts"],
        0
    );
    assert!(!absent.exists());
    Ok(())
}

#[test]
fn cass_serve_reads_explicit_canonical_archive_without_opening_an_index() -> anyhow::Result<()> {
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::storage::sqlite::FrankenStorage;
    use serde_json::{Value, json};

    let temp = tempfile::tempdir()?;
    let db = temp.path().join("archive.db");
    let storage = FrankenStorage::open(&db)?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let outcome = storage.insert_conversation_tree(
        agent,
        None,
        &Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some("binary-canonical-service".into()),
            title: None,
            source_path: "/absent/source.jsonl".into(),
            started_at: None,
            ended_at: None,
            approx_tokens: None,
            metadata_json: json!({}),
            source_id: "local".into(),
            origin_host: None,
            messages: vec![Message {
                id: None,
                idx: 12,
                role: MessageRole::Agent,
                author: None,
                created_at: None,
                content: "complete canonical evidence δ".into(),
                extra_json: json!({}),
                snippets: Vec::new(),
            }],
        },
    )?;
    drop(storage);
    let before = std::fs::read(&db)?;
    for enabled in [false, true] {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command
            .args(["serve", "--stdio", "--index"])
            .arg(temp.path().join("absent-index"));
        if enabled {
            command.arg("--db").arg(&db);
        }
        let mut child = command
            .current_dir(temp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut input = child.stdin.take().unwrap();
        writeln!(
            input,
            "{}",
            json!({"op":"view", "id":1,
            "source_path":"/absent/source.jsonl", "source_id":"local",
            "conversation_id":outcome.conversation_id, "message_index":13})
        )?;
        writeln!(input, "{}", json!({"op":"status", "id":2}))?;
        drop(input);
        if child.wait_timeout(Duration::from_secs(20))?.is_none() {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("canonical service failed to terminate after EOF");
        }
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let replies: Vec<Value> = std::str::from_utf8(&output.stdout)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["ok"], enabled, "{}", replies[0]);
        if enabled {
            assert_eq!(
                replies[0]["result"]["messages"][0]["content"],
                "complete canonical evidence δ"
            );
            assert_eq!(replies[0]["result"]["messages"][0]["message_index"], 13);
        } else {
            assert_eq!(replies[0]["error"]["kind"], "canonical_access_disabled");
        }
        assert_eq!(replies[1]["result"]["open_attempts"], 0);
        assert_eq!(
            replies[1]["result"]["canonical_read_attempts"],
            u64::from(enabled)
        );
        assert!(!temp.path().join("absent-index").exists());
        assert_eq!(std::fs::read(&db)?, before);
    }
    Ok(())
}

// Keep every owned child reaped even when a regression assertion fails.
struct DeadlineChild(std::process::Child);

impl Drop for DeadlineChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn deadline_child(index: &std::path::Path, mcp: bool) -> std::io::Result<DeadlineChild> {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command.args(["serve", "--stdio", "--request-timeout-ms", "500", "--index"]);
    command.arg(index);
    if mcp {
        command.arg("--mcp");
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map(DeadlineChild)
}

#[test]
fn partial_jsonl_and_mcp_frames_cannot_hold_the_worker_forever() -> anyhow::Result<()> {
    for mcp in [false, true] {
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("never-opened");
        let mut child = deadline_child(&index, mcp)?;
        let mut input = child.0.stdin.take().unwrap();
        input.write_all(b"{\"unfinished\":")?;
        input.flush()?;
        // Keep stdin open: EOF would end the frame without exercising the limit.
        let status = child.0.wait_timeout(Duration::from_secs(10))?;
        assert_eq!(status.and_then(|status| status.code()), Some(124));
        assert!(!index.exists());
        drop(input);
    }
    Ok(())
}

#[test]
fn idle_connections_and_completed_requests_do_not_inherit_old_deadlines() -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader};
    for mcp in [false, true] {
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("never-opened");
        let mut child = deadline_child(&index, mcp)?;
        let mut input = child.0.stdin.take().unwrap();
        let mut output = BufReader::new(child.0.stdout.take().unwrap());
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for _ in 0..2 {
                let mut line = String::new();
                let result = output.read_line(&mut line).map(|_| line);
                if sender.send(result).is_err() {
                    break;
                }
            }
        });
        for id in [1, 2] {
            std::thread::sleep(Duration::from_millis(750));
            assert!(
                child.0.try_wait()?.is_none(),
                "idle time is not request work"
            );
            let request = if mcp {
                serde_json::json!({"jsonrpc":"2.0", "id":id, "method":"ping"})
            } else {
                serde_json::json!({"op":"status", "id":id})
            };
            writeln!(input, "{request}")?;
            input.flush()?;
            // Independent outer bound: the test must fail even if the service's
            // own watchdog regresses. DeadlineChild reaps it on an early return.
            let line = receiver.recv_timeout(Duration::from_secs(10))??;
            assert!(!line.is_empty());
            let response: serde_json::Value = serde_json::from_str(&line)?;
            assert_eq!(response["id"], id);
            assert!(response.get("error").is_none(), "{response}");
        }
        drop(input);
        assert!(
            child
                .0
                .wait_timeout(Duration::from_secs(10))?
                .is_some_and(|s| s.success())
        );
        reader.join().expect("bounded response reader panicked");
        assert!(!index.exists());
    }
    Ok(())
}

#[test]
fn blocked_response_pipes_terminate_both_transports() -> anyhow::Result<()> {
    for mcp in [false, true] {
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("never-opened");
        let mut child = deadline_child(&index, mcp)?;
        let mut input = child.0.stdin.take().unwrap();
        // Do not drain stdout. Bounded request count, with an 8-KiB correlation
        // ID for MCP, fills the pipe without loading any index or database.
        let request = if mcp {
            serde_json::json!({"jsonrpc":"2.0", "id":"x".repeat(8192), "method":"ping"})
        } else {
            serde_json::json!({"op":"status", "id":1})
        };
        let writer = std::thread::spawn(move || {
            for _ in 0..2048 {
                if writeln!(input, "{request}").is_err() {
                    break;
                }
            }
        });
        let status = child.0.wait_timeout(Duration::from_secs(15))?;
        // Kill/reap on failure before joining a writer that may be in write().
        if status.is_none() {
            let _ = child.0.kill();
            let _ = child.0.wait();
        }
        writer.join().expect("bounded input writer panicked");
        assert_eq!(status.and_then(|status| status.code()), Some(124));
        assert!(!index.exists());
    }
    Ok(())
}

#[test]
fn service_rejects_disabled_or_unbounded_deadlines_before_access() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let index = temp.path().join("never-opened");
    for value in ["0", "300001", "18446744073709551616"] {
        let mut child = DeadlineChild(
            Command::new(assert_cmd::cargo::cargo_bin!("cass"))
                .args(["serve", "--stdio", "--request-timeout-ms", value, "--index"])
                .arg(&index)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let status = child.0.wait_timeout(Duration::from_secs(10))?;
        assert!(status.is_some_and(|status| !status.success()));
        assert!(!index.exists());
    }
    Ok(())
}

#[test]
fn trickling_input_cannot_restart_the_whole_request_deadline() -> anyhow::Result<()> {
    for mcp in [false, true] {
        let temp = tempfile::tempdir()?;
        let index = temp.path().join("never-opened");
        let mut child = deadline_child(&index, mcp)?;
        let mut input = child.0.stdin.take().unwrap();
        input.write_all(b"{")?;
        input.flush()?;
        let writer = std::thread::spawn(move || {
            // Keep presenting frame bytes much faster than the 500-ms deadline.
            // A per-read/sliding timer would let this incomplete request survive
            // for five seconds; the whole-request guard must not be extended.
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(50));
                if input.write_all(b" ").and_then(|()| input.flush()).is_err() {
                    break;
                }
            }
        });
        let status = child.0.wait_timeout(Duration::from_secs(3))?;
        if status.is_none() {
            let _ = child.0.kill();
            let _ = child.0.wait();
        }
        writer.join().expect("bounded trickle writer panicked");
        assert_eq!(status.and_then(|status| status.code()), Some(124));
        assert!(!index.exists());
    }
    Ok(())
}
