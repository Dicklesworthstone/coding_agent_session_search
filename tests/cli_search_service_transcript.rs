//! Real-binary JSON/MCP journeys over full synthetic canonical storage.
//! No provider history, lexical index, models, network, or parent env mutation.

use anyhow::{Context, Result, ensure};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use wait_timeout::ChildExt;

const WAIT: Duration = Duration::from_secs(120);
const MAX_FRAME: usize = 1024 * 1024;
const PAGE_BYTES: usize = 32 * 1024;

struct Fixture {
    root: tempfile::TempDir,
    db: PathBuf,
    source: PathBuf,
    conversation: i64,
    expected: BTreeMap<u64, String>,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("agent_search.db");
        let source = root.path().join("provider-never-created.jsonl");
        let expected = BTreeMap::from([
            (1, String::new()),
            (
                8,
                format!(
                    "{}😀{}\0tail",
                    "a".repeat(PAGE_BYTES - 1),
                    "雪".repeat(42_000)
                ),
            ),
            (102, "last message with \0 and quotes \"\\".to_owned()),
        ]);
        let storage = FrankenStorage::open(&db)?;
        let agent = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let inserted = storage.insert_conversation_tree(
            agent,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some("transcript-service-fixture".into()),
                title: Some("Transcript fixture".into()),
                source_path: source.clone(),
                started_at: None,
                ended_at: None,
                approx_tokens: None,
                metadata_json: json!({}),
                source_id: "local".into(),
                origin_host: None,
                messages: expected
                    .iter()
                    .map(|(&index, content)| Message {
                        id: None,
                        idx: (index - 1) as i64,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: None,
                        content: content.clone(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    })
                    .collect(),
            },
        )?;
        drop(storage);
        Ok(Self {
            root,
            db,
            source,
            conversation: inserted.conversation_id,
            expected,
        })
    }

    fn selection(&self) -> Value {
        json!({"source_path":self.source, "source_id":"local", "conversation_id":self.conversation})
    }

    fn image(&self) -> Result<Vec<Option<Vec<u8>>>> {
        ["", "-wal", "-shm", "-journal"]
            .into_iter()
            .map(|suffix| {
                let mut path = self.db.as_os_str().to_os_string();
                path.push(suffix);
                match fs::read(PathBuf::from(path)) {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error.into()),
                }
            })
            .collect()
    }
}

struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    replies: Receiver<Vec<u8>>,
    output_thread: Option<JoinHandle<io::Result<()>>>,
    error_thread: Option<JoinHandle<io::Result<Vec<u8>>>>,
    mcp: bool,
    sequence: u64,
}

impl Worker {
    fn start(fixture: &Fixture, mcp: bool) -> Result<Self> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
        command
            .env_clear()
            .envs(std::env::vars_os().filter(|(key, _)| {
                [
                    "PATH",
                    "SYSTEMROOT",
                    "WINDIR",
                    "TEMP",
                    "TMP",
                    "TMPDIR",
                    "LD_LIBRARY_PATH",
                    "DYLD_LIBRARY_PATH",
                    "RUST_MIN_STACK",
                ]
                .iter()
                .any(|name| key.to_string_lossy().eq_ignore_ascii_case(name))
            }));
        command
            .current_dir(fixture.root.path())
            .env("HOME", fixture.root.path())
            .env("USERPROFILE", fixture.root.path())
            .env("CASS_AUTO_REFRESH", "0")
            .env("TUI_HEADLESS", "1")
            .args(["serve", "--stdio", "--data-dir"])
            .arg(fixture.root.path())
            .arg("--db")
            .arg(&fixture.db)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if mcp {
            command.arg("--mcp");
        }
        let child = command.spawn()?;
        let (sender, replies) = mpsc::sync_channel(2);
        // Establish cleanup ownership immediately, before any fallible setup.
        let mut worker = Self {
            child,
            input: None,
            replies,
            output_thread: None,
            error_thread: None,
            mcp,
            sequence: 0,
        };
        worker.input = Some(worker.child.stdin.take().context("child stdin")?);
        let output = worker.child.stdout.take().context("child stdout")?;
        worker.output_thread = Some(thread::spawn(move || {
            let mut reader = BufReader::new(output);
            for _ in 0..128 {
                let mut bytes = Vec::new();
                let count = reader
                    .by_ref()
                    .take((MAX_FRAME + 1) as u64)
                    .read_until(b'\n', &mut bytes)?;
                if count == 0 {
                    return Ok(());
                }
                if count > MAX_FRAME || bytes.last() != Some(&b'\n') {
                    return Err(io::Error::other("child response exceeded its frame bound"));
                }
                // Never block teardown on an unsolicited stream of responses.
                if sender.try_send(bytes).is_err() {
                    return Ok(());
                }
            }
            Err(io::Error::other("child emitted too many response frames"))
        }));
        let errors = worker.child.stderr.take().context("child stderr")?;
        worker.error_thread = Some(thread::spawn(move || {
            let mut bytes = Vec::new();
            errors.take(65_537).read_to_end(&mut bytes)?;
            Ok(bytes)
        }));
        if mcp {
            let initialized = worker.exchange(json!({
                "jsonrpc":"2.0", "id":0, "method":"initialize",
                "params":{"protocolVersion":"2025-11-25", "capabilities":{},
                    "clientInfo":{"name":"transcript-fixture", "version":"1"}}
            }))?;
            ensure!(
                initialized["result"]["protocolVersion"] == "2025-11-25",
                "handshake failed"
            );
            worker.send(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))?;
            let catalog =
                worker.exchange(json!({"jsonrpc":"2.0", "id":0, "method":"tools/list"}))?;
            let tool = catalog["result"]["tools"]
                .as_array()
                .context("tool catalog")?
                .iter()
                .find(|tool| tool["name"] == "cass_transcript")
                .context("transcript tool")?;
            ensure!(
                tool["annotations"]["openWorldHint"] == false,
                "transcript must stay local"
            );
            ensure!(
                tool["annotations"]["readOnlyHint"] == true,
                "transcript must stay read-only"
            );
        }
        Ok(worker)
    }

    fn send(&mut self, value: &Value) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(bytes.len() < 16 * 1024, "fixture request bound");
        let input = self.input.as_mut().context("child input closed")?;
        input.write_all(&bytes)?;
        input.write_all(b"\n")?;
        input.flush()?;
        Ok(())
    }

    fn exchange(&mut self, value: Value) -> Result<Value> {
        self.send(&value)?;
        let bytes = self
            .replies
            .recv_timeout(WAIT)
            .context("child response deadline or EOF")?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn call(&mut self, op: &str, mut arguments: Value) -> Result<Value> {
        self.sequence += 1;
        let id = self.sequence;
        if self.mcp {
            let reply = self.exchange(json!({"jsonrpc":"2.0", "id":id, "method":"tools/call",
                "params":{"name":format!("cass_{op}"), "arguments":arguments}}))?;
            ensure!(
                reply["id"] == id && reply.get("error").is_none(),
                "MCP protocol failure: {reply}"
            );
            let result = &reply["result"];
            let text: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().context("MCP text")?)?;
            ensure!(
                text == result["structuredContent"],
                "MCP representations disagree"
            );
            Ok(if result["isError"] == true {
                json!({"ok":false, "error":text["error"]})
            } else {
                json!({"ok":true, "result":text})
            })
        } else {
            arguments["op"] = json!(op);
            arguments["id"] = json!(id);
            let reply = self.exchange(arguments)?;
            ensure!(reply["id"] == id, "JSON response ID changed");
            Ok(reply)
        }
    }

    fn result(&mut self, op: &str, arguments: Value) -> Result<Value> {
        let reply = self.call(op, arguments)?;
        ensure!(reply["ok"] == true, "service request failed: {reply}");
        Ok(reply["result"].clone())
    }

    fn finish(mut self) -> Result<()> {
        drop(self.input.take());
        let status = self
            .child
            .wait_timeout(WAIT)?
            .context("child exit deadline")?;
        ensure!(status.success(), "service exited {status}");
        if let Some(thread) = self.output_thread.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("stdout worker panicked"))??;
        }
        if let Some(thread) = self.error_thread.take() {
            let bytes = thread
                .join()
                .map_err(|_| anyhow::anyhow!("stderr worker panicked"))??;
            ensure!(bytes.len() <= 65_536, "child stderr exceeded capture bound");
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        drop(self.input.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.output_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.error_thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn json_and_mcp_recover_large_transcripts_without_an_index_or_provider_files() -> Result<()> {
    for mcp in [false, true] {
        let fixture = Fixture::new()?;
        let before = fixture.image()?;
        let mut worker = Worker::start(&fixture, mcp)?;
        let status = worker.result("status", json!({}))?;
        assert_eq!(status["canonical_read_attempts"], 0);
        let mut invalid = fixture.selection();
        invalid["limit"] = json!(0);
        assert_eq!(
            worker.call("transcript", invalid)?["error"]["kind"],
            "invalid_request"
        );
        assert_eq!(
            worker.result("status", json!({}))?["canonical_read_attempts"],
            0
        );
        let mut view = fixture.selection();
        view["message_index"] = json!(8);
        assert_eq!(
            worker.call("view", view)?["error"]["kind"],
            "canonical_payload_too_large"
        );

        let mut cursor = Value::Null;
        let mut actual: BTreeMap<u64, String> = BTreeMap::new();
        let mut ids: BTreeMap<u64, i64> = BTreeMap::new();
        let mut pages = 0_u64;
        let mut complete = false;
        for _ in 0..24 {
            let mut arguments = fixture.selection();
            arguments["cursor"] = cursor.clone();
            arguments["limit"] = json!(2);
            let result = worker.result("transcript", arguments)?;
            pages += 1;
            assert!(result["content_bytes"].as_u64().context("page bytes")? <= PAGE_BYTES as u64);
            assert_eq!(result["cross_page_snapshot"], false);
            assert_eq!(result["maintenance_performed"], false);
            for message in result["messages"].as_array().context("messages")? {
                let index = message["message_index"].as_u64().context("index")?;
                let id = message["message_id"].as_i64().context("id")?;
                if let Some(previous) = ids.insert(index, id) {
                    assert_eq!(previous, id);
                }
                let body = actual.entry(index).or_default();
                assert_eq!(message["byte_offset"].as_u64(), Some(body.len() as u64));
                body.push_str(message["content"].as_str().context("body")?);
                assert_eq!(message["end_byte_offset"].as_u64(), Some(body.len() as u64));
                assert_eq!(message["role"], "assistant");
                if message["message_complete"] == true {
                    assert_eq!(
                        body.as_str(),
                        fixture
                            .expected
                            .get(&index)
                            .context("expected message")?
                            .as_str()
                    );
                }
            }
            cursor = result["next_cursor"].clone();
            assert_eq!(result["end_of_conversation"], cursor.is_null());
            if cursor.is_null() {
                complete = true;
                break;
            }
        }
        assert!(complete && pages > 1);
        assert_eq!(actual, fixture.expected);
        let status = worker.result("status", json!({}))?;
        assert_eq!(status["canonical_reads_completed"], pages);
        assert_eq!(status["canonical_read_attempts"], pages + 1);
        assert_eq!(status["open_attempts"], 0);
        assert_eq!(status["models_loaded"], false);
        worker.finish()?;
        assert_eq!(fixture.image()?, before);
        assert!(!fixture.source.exists());
        assert!(!fixture.root.path().join("index").exists());
        assert!(!fixture.root.path().join("models").exists());
    }
    Ok(())
}

#[test]
fn changed_partial_message_is_a_typed_failure_and_service_can_restart_paging() -> Result<()> {
    for mcp in [false, true] {
        let fixture = Fixture::new()?;
        let mut worker = Worker::start(&fixture, mcp)?;
        let first = worker.result("transcript", fixture.selection())?;
        let cursor = first["next_cursor"].clone();
        assert!(cursor["byte_offset"].as_u64().context("partial offset")? > 0);
        let storage = FrankenStorage::open(&fixture.db)?;
        storage
            .raw()
            .execute("UPDATE messages SET content = 'rewritten' WHERE idx = 7")?;
        drop(storage);
        let changed = fixture.image()?;
        let mut arguments = fixture.selection();
        arguments["cursor"] = cursor;
        let reply = worker.call("transcript", arguments)?;
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["kind"], "canonical_cursor_changed");
        assert!(reply.get("result").is_none());
        let restarted = worker.result("transcript", fixture.selection())?;
        assert_eq!(restarted["messages"][1]["content"], "rewritten");
        assert_eq!(restarted["end_of_conversation"], true);
        assert_eq!(worker.result("status", json!({}))?["open_attempts"], 0);
        worker.finish()?;
        assert_eq!(fixture.image()?, changed);
    }
    Ok(())
}
