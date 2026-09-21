//! GH493's reported divergences, independently of lexical-index maintenance.
//!
//! Seed canonical rows deliberately: this suite tests archive addressing and
//! rendering, not provider ingestion or search. The separate
//! gh493_message_coordinates target retains its real search round-trip test.
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

const TOOL_TEXT: &str = "bm-li42 · File the vetted Claude Code harness bugs upstream";
const ASSISTANT_TEXT: &str = "lexical-rebuild-state\nlane status\n19:12:59 tick 6";
const WRONG_TEXT: &str = "[Tool: Read - unrelated-tool-result.txt]";

struct Fixture {
    root: TempDir,
    db: PathBuf,
    path: PathBuf,
    conversation_id: i64,
}

impl Fixture {
    fn new(pretty_json: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("agent_search.db");
        let path = root.path().join(if pretty_json {
            "session.json"
        } else {
            "session.jsonl"
        });
        let body = if pretty_json {
            serde_json::to_string_pretty(&json!({"messages": [
                {"role": "assistant", "content": WRONG_TEXT},
                {"role": "tool", "content": TOOL_TEXT},
                {"role": "assistant", "content": ASSISTANT_TEXT}
            ]}))
            .unwrap()
        } else {
            let mut lines = vec![json!({"type": "queue-operation"}).to_string(); 3455];
            for number in [67, 90] {
                lines[number - 1] = json!({"type": "user", "message": {
                    "role": "user", "content": [{"type": "tool_result", "content": TOOL_TEXT}]
                }})
                .to_string();
            }
            lines[128] = json!({"role": "assistant", "content": WRONG_TEXT}).to_string();
            lines[540] = json!({"type": "user", "message": {
                "role": "user", "content": [{"type": "tool_result", "content": "unrelated Edit"}]
            }})
            .to_string();
            lines[2374] = json!({"role": "assistant", "content": ASSISTANT_TEXT}).to_string();
            lines.join("\n") + "\n"
        };
        std::fs::write(&path, body).unwrap();
        let storage = FrankenStorage::open(&db).unwrap();
        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: "claude_code".into(),
                name: "Claude Code".into(),
                version: None,
                kind: AgentKind::Cli,
            })
            .unwrap();
        let messages = [
            (0, MessageRole::User, "first canonical message"),
            (128, MessageRole::Tool, TOOL_TEXT),
            (540, MessageRole::Agent, ASSISTANT_TEXT),
            (900, MessageRole::User, "last canonical message"),
        ]
        .into_iter()
        .map(|(idx, role, content)| Message {
            id: None,
            idx,
            role,
            author: None,
            created_at: Some(1_733_000_000_000 + idx),
            content: content.into(),
            extra_json: json!({}),
            snippets: Vec::new(),
        })
        .collect();
        storage
            .insert_conversation_tree(
                agent_id,
                None,
                &Conversation {
                    id: None,
                    agent_slug: "claude_code".into(),
                    workspace: None,
                    external_id: Some("gh493-reported-divergence".into()),
                    title: Some("Reported coordinate divergences".into()),
                    source_path: path.clone(),
                    started_at: Some(1_733_000_000_000),
                    ended_at: Some(1_733_000_001_000),
                    approx_tokens: None,
                    metadata_json: json!({}),
                    messages,
                    source_id: "local".into(),
                    origin_host: None,
                },
            )
            .unwrap();
        let ids = storage
            .raw()
            .query_map_collect(
                "SELECT id FROM conversations WHERE agent_id = ?1",
                coding_agent_search::franken_sync::params![agent_id],
                |row| row.get_typed::<i64>(0),
            )
            .unwrap();
        assert_eq!(ids.len(), 1);
        drop(storage);
        Self {
            root,
            db,
            path,
            conversation_id: ids[0],
        }
    }

    fn follow(&self, command: &str, number: usize, context: usize) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cass"))
            .arg("--db")
            .arg(&self.db)
            .arg(command)
            .arg(&self.path)
            .args([
                "--source",
                "local",
                "--conversation-id",
                &self.conversation_id.to_string(),
            ])
            .args([
                "--message-index",
                &number.to_string(),
                "-C",
                &context.to_string(),
                "--json",
            ])
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_VIEW_BUDGET_MS", "30000")
            .env_remove("CASS_OUTPUT_FORMAT")
            .env_remove("TOON_DEFAULT_FORMAT")
            .env_remove("CASS_TEST_VIEW_SLOW_MS")
            .output()
            .unwrap()
    }
}

fn rows(output: Output, command: &str) -> Vec<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    if command == "view" {
        assert_eq!(payload["coordinate_space"], "message_index");
        assert_eq!(payload["content_source"], "archive");
        payload["lines"].as_array().unwrap().clone()
    } else {
        payload.as_array().unwrap().clone()
    }
}

#[test]
fn reported_129_and_541_targets_preserve_canonical_text_and_tool_role() {
    let fixture = Fixture::new(false);
    let source_before = std::fs::read(&fixture.path).unwrap();
    let db_before = std::fs::read(&fixture.db).unwrap();
    for command in ["expand", "view"] {
        for (number, content, role) in [
            (129, TOOL_TEXT, "tool"),
            (541, ASSISTANT_TEXT, "assistant"),
        ] {
            let result = rows(fixture.follow(command, number, 0), command);
            assert_eq!(result.len(), 1);
            let target = &result[0];
            assert_eq!(target["content"], content);
            assert_eq!(target["role"], role);
            assert_eq!(target["message_index"], number);
            assert_eq!(target["line"], number);
            assert_eq!(target["conversation_id"], fixture.conversation_id);
            assert_eq!(target["source_id"], "local");
            assert_eq!(target["coordinate_space"], "message_index");
            assert_eq!(target["content_source"], "archive");
            assert_eq!(target["is_target"], true);
            assert!(target["message_id"].as_i64().unwrap() > 0);
        }
    }
    assert_eq!(std::fs::read(&fixture.path).unwrap(), source_before);
    assert_eq!(std::fs::read(&fixture.db).unwrap(), db_before);
}

#[test]
fn context_counts_messages_across_large_index_gaps_without_substituting_targets() {
    let fixture = Fixture::new(false);
    for command in ["expand", "view"] {
        let result = rows(fixture.follow(command, 541, 1), command);
        let numbers: Vec<_> = result
            .iter()
            .map(|row| row["message_index"].as_u64().unwrap())
            .collect();
        assert_eq!(numbers, [129, 541, 901]);
        assert_eq!(
            result.iter().filter(|row| row["is_target"] == true).count(),
            1
        );
        assert_eq!(result[1]["content"], ASSISTANT_TEXT);
        for missing in [128, 130, 540, 542, 2375] {
            let output = fixture.follow(command, missing, 1);
            assert!(
                !output.status.success(),
                "substituted a neighbour for {missing}"
            );
            assert!(output.stdout.is_empty(), "emitted a target for {missing}");
        }
    }
}

#[test]
fn pretty_printed_json_is_not_treated_as_a_message_numbered_file() {
    let fixture = Fixture::new(true);
    for command in ["expand", "view"] {
        let result = rows(fixture.follow(command, 541, 0), command);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["content"], ASSISTANT_TEXT);
        assert_eq!(result[0]["message_index"], 541);
        assert_eq!(result[0]["is_target"], true);
    }
}

#[test]
fn quickstart_does_not_feed_search_ordinals_to_raw_line_flags() {
    let reference = include_str!("../docs/reference/QUICK_REFERENCE.md");
    let quickstart = reference.split("## TL;DR").next().unwrap();
    for command in ["view", "expand"] {
        let line = quickstart
            .lines()
            .find(|line| line.starts_with("cass ") && line.contains(&format!(" {command} ")))
            .expect("documented follow-up");
        assert!(line.contains("--message-index"), "{line}");
        assert!(line.contains("--source"), "{line}");
        assert!(line.contains("--conversation-id"), "{line}");
        assert!(line.contains("--db"), "{line}");
        assert!(
            !line.contains(" -n ") && !line.contains(" --line "),
            "{line}"
        );
    }
    assert!(quickstart.contains("source_path,source_id,conversation_id,line_number"));
}
