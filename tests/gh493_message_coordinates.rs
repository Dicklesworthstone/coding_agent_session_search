//! Search ordinals and physical lines must never be silently interchanged.
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture {
    _root: TempDir,
    db: PathBuf,
    data: PathBuf,
    path: PathBuf,
    conversation_id: i64,
}

fn seed(storage: &FrankenStorage, path: &Path, agent: &str, indices: &[i64]) -> i64 {
    let agent_id = storage
        .ensure_agent(&Agent {
            id: None,
            slug: agent.into(),
            name: agent.into(),
            version: None,
            kind: AgentKind::Cli,
        })
        .unwrap();
    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &Conversation {
                id: None,
                agent_slug: agent.into(),
                workspace: None,
                external_id: Some(format!("gh493-{agent}")),
                title: Some("GH493 message anchor regression".into()),
                source_path: path.into(),
                started_at: Some(1_733_000_000_000),
                ended_at: Some(1_733_000_002_000),
                approx_tokens: None,
                metadata_json: json!({}),
                messages: indices
                    .iter()
                    .enumerate()
                    .map(|(position, &idx)| Message {
                        id: None,
                        idx,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: Some(1_733_000_000_000 + position as i64),
                        content: if position == 1 {
                            "ANCHOR493TARGET"
                        } else {
                            "canonical neighbour"
                        }
                        .into(),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    })
                    .collect(),
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
    ids[0]
}

impl Fixture {
    fn new(indices: &[i64]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("cass");
        std::fs::create_dir(&data).unwrap();
        let db = data.join("agent_search.db");
        let path = root.path().join("session.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"queue-operation\"}\n",
                "{\"role\":\"assistant\",\"content\":\"PLAUSIBLE WRONG MESSAGE\"}\n",
                "\n",
                "{\"type\":\"attachment\"}\n",
                "{\"role\":\"assistant\",\"content\":\"ANCHOR493TARGET\"}\n",
            ),
        )
        .unwrap();
        let storage = FrankenStorage::open(&db).unwrap();
        let conversation_id = seed(&storage, &path, "claude_code", indices);
        drop(storage);
        Self {
            _root: root,
            db,
            data,
            path,
            conversation_id,
        }
    }

    fn command(&self, subcommand: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
        command
            .arg("--db")
            .arg(&self.db)
            .arg(subcommand)
            .env_remove("CASS_OUTPUT_FORMAT")
            .env_remove("TOON_DEFAULT_FORMAT")
            .env_remove("CASS_TEST_VIEW_SLOW_MS")
            .env("CASS_VIEW_BUDGET_MS", "30000");
        command
    }

    fn follow(&self, subcommand: &str, index: usize, extra: &[&str]) -> Output {
        self.command(subcommand)
            .arg(&self.path)
            .args(["--message-index", &index.to_string(), "-C", "0", "--json"])
            .args(extra)
            .output()
            .unwrap()
    }
}

fn decode(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_target(payload: &Value, command: &str, number: usize, cid: i64) {
    let rows = if command == "view" {
        &payload["lines"]
    } else {
        payload
    };
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{payload}");
    assert_eq!(rows[0]["content"], "ANCHOR493TARGET");
    assert_eq!(rows[0]["is_target"], true);
    assert_eq!(rows[0]["message_index"], number);
    assert_eq!(rows[0]["conversation_id"], cid);
    assert_eq!(rows[0]["source_id"], "local");
    assert_eq!(rows[0]["coordinate_space"], "message_index");
    assert!(rows[0]["message_id"].as_i64().unwrap() > 0);
}

#[test]
fn physical_line_and_indexed_message_are_distinct_explicit_coordinates() {
    let fixture = Fixture::new(&[0, 1, 2]);
    for command in ["expand", "view"] {
        assert_target(
            &decode(fixture.follow(command, 2, &[])),
            command,
            2,
            fixture.conversation_id,
        );
    }
    let raw = decode(
        fixture
            .command("expand")
            .arg(&fixture.path)
            .args(["--line", "2", "-C", "0", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(raw[0]["content"], "PLAUSIBLE WRONG MESSAGE");
}

#[test]
fn actual_search_hit_round_trips_through_both_followup_commands() {
    let fixture = Fixture::new(&[0, 1, 2]);
    let indexed = fixture
        .command("index")
        .arg("--data-dir")
        .arg(&fixture.data)
        .arg("--reconcile-conversation")
        .arg(fixture.conversation_id.to_string())
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let found = decode(
        fixture
            .command("search")
            .args([
                "ANCHOR493TARGET",
                "--mode",
                "lexical",
                "--json",
                "--limit",
                "5",
            ])
            .arg("--data-dir")
            .arg(&fixture.data)
            .output()
            .unwrap(),
    );
    let hits = found["hits"].as_array().expect("search hits");
    let hit = hits
        .iter()
        .find(|hit| hit["content"] == "ANCHOR493TARGET")
        .expect("canonical hit");
    let index = hit["line_number"].as_u64().unwrap() as usize;
    assert_eq!(index, 2);
    assert_eq!(hit["source_path"], fixture.path.to_string_lossy().as_ref());
    for command in ["expand", "view"] {
        let output = fixture.follow(
            command,
            index,
            &[
                "--source",
                hit["source_id"].as_str().unwrap(),
                "--conversation-id",
                &fixture.conversation_id.to_string(),
            ],
        );
        assert_target(&decode(output), command, index, fixture.conversation_id);
    }
}

#[test]
fn sparse_indices_are_not_vector_positions_and_missing_indices_fail() {
    let fixture = Fixture::new(&[0, 7, 12]);
    for command in ["expand", "view"] {
        assert_target(
            &decode(fixture.follow(command, 8, &[])),
            command,
            8,
            fixture.conversation_id,
        );
        for missing in [0, 2, 7, 99, usize::MAX] {
            let output = fixture.follow(command, missing, &[]);
            assert!(!output.status.success(), "accepted missing index {missing}");
            assert!(output.stdout.is_empty(), "emitted a target on error");
        }
    }
}

#[test]
fn changed_source_file_never_replaces_archived_hit_content() {
    let fixture = Fixture::new(&[0, 1]);
    std::fs::write(
        &fixture.path,
        "{\"messages\":[{\"content\":\"unrelated replacement\"}]}\n",
    )
    .unwrap();
    for command in ["expand", "view"] {
        assert_target(
            &decode(fixture.follow(command, 2, &["--source", "local"])),
            command,
            2,
            fixture.conversation_id,
        );
    }
}

#[test]
fn wrong_source_or_conversation_never_falls_back_to_live_file() {
    let fixture = Fixture::new(&[0, 1]);
    for command in ["expand", "view"] {
        for flags in [
            vec!["--source", "work-laptop"],
            vec!["--conversation-id", "99999"],
            vec!["--source", ""],
        ] {
            let output = fixture.follow(command, 2, &flags);
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
        }
    }
}

#[test]
fn shared_path_requires_explicit_conversation_identity() {
    let fixture = Fixture::new(&[0, 1]);
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    let other = seed(&storage, &fixture.path, "codex", &[0, 1]);
    assert_ne!(other, fixture.conversation_id);
    drop(storage);
    for command in ["expand", "view"] {
        let ambiguous = fixture.follow(command, 2, &["--source", "local"]);
        assert!(!ambiguous.status.success());
        assert!(ambiguous.stdout.is_empty());
        assert_target(
            &decode(fixture.follow(
                command,
                2,
                &[
                    "--source",
                    "local",
                    "--conversation-id",
                    &fixture.conversation_id.to_string(),
                ],
            )),
            command,
            2,
            fixture.conversation_id,
        );
    }
}

#[test]
fn missing_archive_is_not_created_and_does_not_use_file_lines() {
    let fixture = Fixture::new(&[0, 1]);
    let missing = fixture.data.join("does-not-exist.db");
    for command in ["expand", "view"] {
        let output = Command::new(env!("CARGO_BIN_EXE_cass"))
            .arg("--db")
            .arg(&missing)
            .arg(command)
            .arg(&fixture.path)
            .args(["--message-index", "2", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!missing.exists());
    }
}

#[test]
fn raw_expand_never_snaps_blank_malformed_or_past_eof_to_a_target() {
    let fixture = Fixture::new(&[0, 1]);
    std::fs::write(
        &fixture.path,
        "{\"content\":\"first\"}\n\nmalformed\n{\"content\":\"last\"}\n",
    )
    .unwrap();
    for line in ["0", "2", "3", "99"] {
        let output = fixture
            .command("expand")
            .arg(&fixture.path)
            .args(["--line", line, "-C", "0", "--json"])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "accepted nonexistent raw message {line}"
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn conflicting_selectors_are_rejected_and_huge_context_does_not_overflow() {
    let fixture = Fixture::new(&[0, 1, 2]);
    for command in ["expand", "view"] {
        let output = fixture.follow(command, 2, &["--line", "2"]);
        assert!(!output.status.success());
        let payload = decode(
            fixture
                .command(command)
                .arg(&fixture.path)
                .args([
                    "--message-index",
                    "2",
                    "-C",
                    &usize::MAX.to_string(),
                    "--json",
                ])
                .output()
                .unwrap(),
        );
        let rows = if command == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().filter(|row| row["is_target"] == true).count(),
            1
        );
    }
}
