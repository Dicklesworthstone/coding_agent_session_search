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
            .env("HOME", self._root.path())
            .env("XDG_CONFIG_HOME", self._root.path().join("config"))
            .env("XDG_DATA_HOME", self._root.path().join("data"))
            .env("XDG_CACHE_HOME", self._root.path().join("cache"))
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
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
    assert_eq!(rows[0]["content_source"], "archive");
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
    let hit_conversation_id = hit["conversation_id"]
        .as_i64()
        .expect("search conversation id");
    assert_eq!(hit_conversation_id, fixture.conversation_id);
    assert_eq!(hit["source_path"], fixture.path.to_string_lossy().as_ref());
    for command in ["expand", "view"] {
        let output = fixture.follow(
            command,
            index,
            &[
                "--source",
                hit["source_id"].as_str().unwrap(),
                "--conversation-id",
                &hit_conversation_id.to_string(),
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

#[test]
fn pasted_search_fields_and_snake_case_selectors_use_canonical_messages() {
    let fixture = Fixture::new(&[0, 7, 12]);
    for subcommand in ["expand", "view"] {
        for selector in [
            "line_number=8",
            "line-number=8",
            "message_index=8",
            "message-index=8",
        ] {
            let payload = decode(
                fixture
                    .command(subcommand)
                    .arg(format!("source_path={}", fixture.path.display()))
                    .arg("source_id=local")
                    .arg(format!("conversation_id={}", fixture.conversation_id))
                    .args([selector, "context=0", "--json"])
                    .output()
                    .unwrap(),
            );
            assert_target(&payload, subcommand, 8, fixture.conversation_id);
        }
        let payload = decode(
            fixture
                .command(subcommand)
                .arg(&fixture.path)
                .args(["--message_index", "8", "-C", "0", "--json"])
                .output()
                .unwrap(),
        );
        assert_target(&payload, subcommand, 8, fixture.conversation_id);
    }
}

#[test]
fn pasted_search_coordinates_cannot_override_explicit_raw_coordinates() {
    let fixture = Fixture::new(&[0, 1]);
    for subcommand in ["expand", "view"] {
        let output = fixture
            .command(subcommand)
            .arg(&fixture.path)
            .args(["--line", "2", "line_number=2", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn missing_source_still_resolves_the_archived_message_without_mutation() {
    let fixture = Fixture::new(&[0, 1]);
    let moved = fixture.path.with_extension("saved-jsonl");
    std::fs::rename(&fixture.path, &moved).unwrap();
    let db_before = std::fs::read(&fixture.db).unwrap();
    let source_before = std::fs::read(&moved).unwrap();
    for subcommand in ["expand", "view"] {
        let payload = decode(fixture.follow(subcommand, 2, &["--source", "LOCAL"]));
        assert_target(&payload, subcommand, 2, fixture.conversation_id);
        if subcommand == "view" {
            assert_eq!(payload["archive_only"], true);
            assert_eq!(payload["source_exists"], false);
        }
    }
    assert_eq!(std::fs::read(&fixture.db).unwrap(), db_before);
    assert_eq!(std::fs::read(&moved).unwrap(), source_before);
    assert!(!fixture.path.exists());
}

#[test]
fn an_empty_conversation_cannot_hide_shared_path_ambiguity() {
    let fixture = Fixture::new(&[0, 1]);
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    seed(&storage, &fixture.path, "codex", &[]);
    drop(storage);
    for subcommand in ["expand", "view"] {
        let output = fixture.follow(subcommand, 2, &[]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "ambiguous-source");
    }
}

#[test]
fn canonical_indices_are_discoverable_as_integers() {
    let fixture = Fixture::new(&[0, 1]);
    let capabilities = decode(
        fixture
            .command("capabilities")
            .arg("--json")
            .output()
            .unwrap(),
    );
    for name in ["expand", "view"] {
        let command = capabilities["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|command| command["name"] == name)
            .unwrap();
        let index = command["arguments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|arg| arg["name"] == "message-index")
            .unwrap();
        assert_eq!(index["value_type"], "integer");
        assert!(
            command["description"]
                .as_str()
                .unwrap()
                .contains("--message-index")
        );
    }
}

#[test]
fn malformed_archive_is_not_repaired_or_replaced_with_a_live_file() {
    let fixture = Fixture::new(&[0, 1]);
    let broken = fixture.data.join("broken.db");
    let bytes = b"not an SQLite database";
    std::fs::write(&broken, bytes).unwrap();
    for subcommand in ["expand", "view"] {
        let output = Command::new(env!("CARGO_BIN_EXE_cass"))
            .env_remove("CASS_OUTPUT_FORMAT")
            .env_remove("TOON_DEFAULT_FORMAT")
            .arg("--db")
            .arg(&broken)
            .arg(subcommand)
            .arg(&fixture.path)
            .args(["--message-index", "2", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(std::fs::read(&broken).unwrap(), bytes);
        assert!(!fixture.data.join("broken.db-wal").exists());
        assert!(!fixture.data.join("broken.db-shm").exists());
    }
}

#[test]
fn physical_lines_never_fall_back_to_archive_when_source_is_missing() {
    let fixture = Fixture::new(&[0, 7, 12]);
    std::fs::rename(&fixture.path, fixture.path.with_extension("retained")).unwrap();
    let before = std::fs::read(&fixture.db).unwrap();
    for command in ["view", "expand"] {
        for json in [false, true] {
            let mut process = fixture.command(command);
            process.arg(&fixture.path).args(["--line", "2", "-C", "0"]);
            if json {
                process.arg("--json");
            }
            let output = process.output().unwrap();
            assert_eq!(output.status.code(), Some(3));
            assert!(
                output.stdout.is_empty(),
                "an archive row became a physical target"
            );
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            assert!(diagnostic.contains("--message-index"), "{diagnostic}");
        }
        assert_target(
            &decode(fixture.follow(command, 8, &[])),
            command,
            8,
            fixture.conversation_id,
        );
    }
    assert_eq!(std::fs::read(&fixture.db).unwrap(), before);
}

#[test]
fn physical_read_error_cannot_turn_into_a_successful_archive_target() {
    let fixture = Fixture::new(&[0, 1, 2]);
    let broken = b"{\"content\":\"first physical record\"}\n\xff\n";
    std::fs::write(&fixture.path, broken).unwrap();
    let before = std::fs::read(&fixture.db).unwrap();
    for command in ["view", "expand"] {
        let output = fixture
            .command(command)
            .arg(&fixture.path)
            .args(["--line", "2", "-C", "0", "--json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(9));
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["kind"], "file-read");
        assert_target(
            &decode(fixture.follow(command, 2, &[])),
            command,
            2,
            fixture.conversation_id,
        );
    }
    assert_eq!(std::fs::read(&fixture.db).unwrap(), before);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), broken);
}

#[test]
fn indexed_non_jsonl_files_do_not_change_physical_coordinate_meaning() {
    let mut fixture = Fixture::new(&[0, 1, 2]);
    fixture.path = fixture.path.with_extension("json");
    std::fs::write(
        &fixture.path,
        "[\n  {\"content\":\"physical second line\"}\n]\n",
    )
    .unwrap();
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    storage
        .raw()
        .execute_compat(
            "UPDATE conversations SET source_path = ?1 WHERE id = ?2",
            coding_agent_search::franken_sync::params![
                fixture.path.to_string_lossy().as_ref(),
                fixture.conversation_id
            ],
        )
        .unwrap();
    drop(storage);
    let before = std::fs::read(&fixture.db).unwrap();
    let payload = decode(
        fixture
            .command("view")
            .arg(&fixture.path)
            .args(["--line", "2", "-C", "0", "--json"])
            .output()
            .unwrap(),
    );
    assert_eq!(payload["coordinate_space"], "file_line");
    assert_eq!(payload["content_source"], "file");
    assert_eq!(payload["total_lines"], 3);
    assert_eq!(
        payload["lines"][0]["content"],
        "  {\"content\":\"physical second line\"}"
    );
    assert_eq!(payload["lines"][0]["is_target"], true);
    let refused = fixture
        .command("expand")
        .arg(&fixture.path)
        .args(["--line", "2", "-C", "0", "--json"])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(9));
    assert!(refused.stdout.is_empty());
    assert_target(
        &decode(fixture.follow("expand", 2, &[])),
        "expand",
        2,
        fixture.conversation_id,
    );
    assert_eq!(std::fs::read(&fixture.db).unwrap(), before);
}

#[test]
fn physical_targets_cannot_claim_remote_or_conversation_identity() {
    let fixture = Fixture::new(&[0, 7, 12]);
    // A real remote archive row shares the same path as a plausible local file.
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    storage
        .raw()
        .execute(
            "INSERT INTO sources (id, kind, host_label, created_at, updated_at)
             VALUES ('work-laptop', 'ssh', 'work-laptop', 1, 1)",
        )
        .unwrap();
    storage
        .raw()
        .execute_compat(
            "UPDATE conversations SET source_id = 'work-laptop', origin_host = 'work-laptop' WHERE id = ?1",
            coding_agent_search::franken_sync::params![fixture.conversation_id],
        )
        .unwrap();
    drop(storage);
    let before = std::fs::read(&fixture.db).unwrap();
    let source = std::fs::read(&fixture.path).unwrap();
    for command in ["view", "expand"] {
        for flags in [
            vec!["--source".to_string(), "work-laptop".to_string()],
            vec![
                "--conversation-id".to_string(),
                fixture.conversation_id.to_string(),
            ],
        ] {
            let output = fixture
                .command(command)
                .arg(&fixture.path)
                .args(["--line", "2", "-C", "0", "--json"])
                .args(flags)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2));
            assert!(output.stdout.is_empty());
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            assert!(diagnostic.contains("--message-index"), "{diagnostic}");
        }
        let payload = decode(fixture.follow(command, 8, &["--source", "work-laptop"]));
        let rows = if command == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        assert_eq!(rows[0]["content"], "ANCHOR493TARGET");
        assert_eq!(rows[0]["source_id"], "work-laptop");
        assert_eq!(rows[0]["is_target"], true);
    }
    assert_eq!(std::fs::read(&fixture.db).unwrap(), before);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), source);
}

#[test]
fn physical_context_keeps_file_numbers_while_expand_skips_non_records() {
    let fixture = Fixture::new(&[0, 1]);
    std::fs::write(
        &fixture.path,
        concat!(
            "{\"content\":\"before\"}\n",
            "\n",
            "malformed\n",
            "{\"content\":\"physical target\"}\n",
            "\n",
            "malformed\n",
            "{\"content\":\"after\"}\n",
            "\n",
        ),
    )
    .unwrap();
    for command in ["view", "expand"] {
        let payload = decode(
            fixture
                .command(command)
                .arg(&fixture.path)
                .args(["--line", "4", "-C", "1", "--json"])
                .output()
                .unwrap(),
        );
        let rows = if command == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        let rows = rows.as_array().unwrap();
        let numbers: Vec<_> = rows
            .iter()
            .map(|row| row["line"].as_u64().unwrap())
            .collect();
        assert_eq!(
            numbers,
            if command == "view" {
                vec![3, 4, 5]
            } else {
                vec![1, 4, 7]
            }
        );
        assert_eq!(
            rows.iter().filter(|row| row["is_target"] == true).count(),
            1
        );
        assert_eq!(rows[1]["line"], 4);
        for row in rows {
            assert_eq!(row["coordinate_space"], "file_line");
            assert_eq!(row["content_source"], "file");
            assert_eq!(row["file_line"], row["line"]);
            assert!(row.get("message_index").is_none());
            assert!(row.get("conversation_id").is_none());
        }
    }
}

#[test]
fn unanchored_archive_browsing_remains_available_without_a_physical_target() {
    let fixture = Fixture::new(&[0, 7, 12]);
    std::fs::rename(&fixture.path, fixture.path.with_extension("retained")).unwrap();
    let payload = decode(
        fixture
            .command("view")
            .arg(&fixture.path)
            .args([
                "--conversation-id",
                &fixture.conversation_id.to_string(),
                "--source",
                "local",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(payload["target_line"], Value::Null);
    assert_eq!(payload["archive_only"], true);
    let rows = payload["lines"].as_array().unwrap();
    assert!(!rows.is_empty());
    assert!(
        rows.iter()
            .all(|row| row["highlighted"] != true && row["is_target"] != true)
    );
}

#[test]
fn search_hit_serialization_preserves_available_identity_without_inventing_one() {
    use coding_agent_search::search::query::{MatchType, SearchHit};
    let mut hit = SearchHit {
        title: "identity".into(),
        snippet: "anchor".into(),
        content: "anchor".into(),
        content_hash: 123,
        conversation_id: Some(42),
        score: 1.0,
        source_path: "/shared/provider.db".into(),
        agent: "codex".into(),
        workspace: "/work".into(),
        workspace_original: None,
        created_at: None,
        line_number: Some(8),
        match_type: MatchType::Exact,
        source_id: "local".into(),
        origin_kind: "local".into(),
        origin_host: None,
    };
    let value = serde_json::to_value(&hit).unwrap();
    assert_eq!(value["conversation_id"], 42);
    assert_eq!(value["line_number"], 8);
    assert!(value.get("content_hash").is_none());
    hit.conversation_id = None;
    let value = serde_json::to_value(&hit).unwrap();
    assert_eq!(value.get("conversation_id"), Some(&Value::Null));
}

#[test]
fn shared_path_search_hits_round_trip_in_every_robot_projection() {
    let fixture = Fixture::new(&[0, 7, 12]);
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    let other = seed(&storage, &fixture.path, "codex", &[0, 7, 12]);
    drop(storage);
    assert_ne!(fixture.conversation_id, other);
    for conversation_id in [fixture.conversation_id, other] {
        let indexed = fixture
            .command("index")
            .arg("--data-dir")
            .arg(&fixture.data)
            .arg("--reconcile-conversation")
            .arg(conversation_id.to_string())
            .arg("--json")
            .output()
            .unwrap();
        decode(indexed);
    }
    let db_before = std::fs::read(&fixture.db).unwrap();
    let source_before = std::fs::read(&fixture.path).unwrap();
    let expected: std::collections::BTreeSet<_> =
        [fixture.conversation_id, other].into_iter().collect();
    for format in ["json", "compact", "jsonl"] {
        for fields in [
            None,
            Some("minimal"),
            Some("summary"),
            Some("all"),
            Some("source_path,line_number,source_id,conversation_id"),
        ] {
            // Plain JSON exercises the handwritten fast serializers. Truncation
            // forces the general projection; JSONL and compact use that path too.
            for truncate in [false, true] {
                let mut command = fixture.command("search");
                command
                    .args([
                        "ANCHOR493TARGET",
                        "--mode",
                        "lexical",
                        "--robot-format",
                        format,
                        "--limit",
                        "5",
                        "--no-maintenance",
                    ])
                    .arg("--data-dir")
                    .arg(&fixture.data);
                if let Some(fields) = fields {
                    command.args(["--fields", fields]);
                }
                if truncate {
                    command.args(["--max-content-length", "8"]);
                }
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "format={format} fields={fields:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let hits: Vec<Value> = if format == "jsonl" {
                    String::from_utf8(output.stdout)
                        .unwrap()
                        .lines()
                        .map(|line| serde_json::from_str::<Value>(line).unwrap())
                        .filter(|value| value.get("source_path").is_some())
                        .collect()
                } else {
                    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
                    payload["hits"].as_array().expect("search hits").clone()
                };
                assert_eq!(hits.len(), 2, "format={format} fields={fields:?}: {hits:?}");
                let mut seen = std::collections::BTreeSet::new();
                for hit in hits {
                    let cid = hit["conversation_id"]
                        .as_i64()
                        .expect("canonical identity must survive output");
                    assert!(
                        seen.insert(cid),
                        "shared paths must not collapse distinct conversations"
                    );
                    assert_eq!(hit["source_id"], "local");
                    assert_eq!(hit["source_path"], fixture.path.to_string_lossy().as_ref());
                    assert_eq!(hit["line_number"], 8);
                    if fields == Some("minimal") {
                        assert_eq!(hit.as_object().unwrap().len(), 5);
                        assert!(hit.get("content").is_none());
                    } else if fields == Some("summary") {
                        assert_eq!(
                            hit.as_object().unwrap().len(),
                            7 + usize::from(hit.get("title_truncated").is_some())
                        );
                        assert!(hit.get("content").is_none());
                    } else if fields == Some("source_path,line_number,source_id,conversation_id") {
                        assert_eq!(hit.as_object().unwrap().len(), 4);
                    }
                    for subcommand in ["view", "expand"] {
                        let payload = decode(fixture.follow(
                            subcommand,
                            8,
                            &[
                                "--source",
                                hit["source_id"].as_str().unwrap(),
                                "--conversation-id",
                                &cid.to_string(),
                            ],
                        ));
                        assert_target(&payload, subcommand, 8, cid);
                    }
                }
                assert_eq!(seen, expected);
            }
        }
    }
    // Caller-selected masks may deliberately omit identity; do not silently
    // expand them or invent a conversation id to make the follow-up succeed.
    let payload = decode(
        fixture
            .command("search")
            .args([
                "ANCHOR493TARGET",
                "--mode",
                "lexical",
                "--json",
                "--fields",
                "source_path,line_number",
                "--limit",
                "5",
                "--no-maintenance",
            ])
            .arg("--data-dir")
            .arg(&fixture.data)
            .output()
            .unwrap(),
    );
    let hits = payload["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2);
    for hit in hits {
        assert_eq!(hit.as_object().unwrap().len(), 2);
    }
    assert_eq!(std::fs::read(&fixture.db).unwrap(), db_before);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), source_before);
}

#[test]
fn corrected_robot_failures_are_one_error_envelope_and_success_still_teaches() {
    let fixture = Fixture::new(&[0, 7, 12]);
    let formats: &[&[&str]] = &[
        &["--json"],
        &["--robot"],
        &["--robot-format", "json"],
        &["--robot-format", "compact"],
        &["--robot-format", "jsonl"],
        &["--format=json"],
        &[], // Environment-selected structured output follows the same contract.
    ];
    for subcommand in ["view", "expand"] {
        for flags in formats {
            let mut command = fixture.command(subcommand);
            command
                .arg(format!("source_path={}", fixture.path.display()))
                .args([
                    "source_id=local",
                    "conversation_id=999999",
                    "line_number=8",
                    "context=0",
                ])
                .args(*flags);
            if flags.is_empty() {
                command.env("CASS_OUTPUT_FORMAT", "json");
            }
            let output = command.output().unwrap();
            assert!(!output.status.success());
            assert!(output.stdout.is_empty(), "failed recovery emitted a target");
            let error: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|err| {
                panic!(
                    "{subcommand} {flags:?}: {err}; stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            assert_eq!(error["error"]["kind"], "indexed-session-required");
            assert_eq!(error["error"]["retryable"], false);
            assert!(
                error["error"]["hint"]
                    .as_str()
                    .is_some_and(|hint| hint.contains("same search hit"))
            );
        }
        let output = fixture
            .command(subcommand)
            .arg(format!("source_path={}", fixture.path.display()))
            .args(["source_id=local", "line_number=8", "context=0", "--json"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("note: auto-corrected:"),
            "successful robot recovery still teaches the canonical syntax"
        );
        assert_target(&decode(output), subcommand, 8, fixture.conversation_id);

        let human = fixture
            .command(subcommand)
            .arg(format!("source_path={}", fixture.path.display()))
            .args(["line_number=8", "conversation_id=999999", "context=0"])
            .output()
            .unwrap();
        assert!(!human.status.success());
        assert!(
            String::from_utf8_lossy(&human.stderr)
                .contains("Note: Your command was auto-corrected:"),
            "human diagnostics must retain their teaching note"
        );
    }
}
