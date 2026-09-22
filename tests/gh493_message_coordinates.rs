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
    // Exercise the optimized full JSON serializer, general metadata path,
    // and explicit field projection: all must carry the same archive key.
    for extra in [
        vec![],
        vec!["--robot-meta"],
        vec![
            "--fields",
            "conversation_id,line_number,source_id,source_path,content",
        ],
    ] {
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
                .args(extra)
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

fn pack_candidate(
    source: &str,
    path: &str,
    conversation_id: Option<i64>,
    number: Option<usize>,
    hash: u64,
) -> coding_agent_search::search::pack_planner::PackCandidate {
    use coding_agent_search::search::pack_planner::PackCandidate;
    use coding_agent_search::search::query::{MatchType, SearchHit};
    PackCandidate::from_search_hit(
        &SearchHit {
            title: "pack identity".into(),
            snippet: "pack evidence".into(),
            content: format!("distinct pack evidence {hash}"),
            content_hash: hash,
            conversation_id,
            score: 1.0,
            source_path: path.into(),
            agent: "claude_code".into(),
            workspace: "/work".into(),
            workspace_original: None,
            created_at: Some(1_733_000_000_000),
            line_number: number,
            match_type: MatchType::Exact,
            source_id: source.into(),
            origin_kind: "local".into(),
            origin_host: None,
        },
        1,
        0,
    )
}

#[test]
fn pack_indices_and_evidence_ids_bind_the_complete_canonical_identity() {
    use coding_agent_search::search::pack_planner::{PackPlanRequest, plan_answer_pack};
    for number in [None, Some(0), Some(1), Some(8), Some(129), Some(541)] {
        let candidate = pack_candidate("local", "/shared/provider.db", Some(42), number, 1);
        assert_eq!(candidate.message_index, number.filter(|&value| value != 0));
        assert!(candidate.line_start.is_none());
        assert!(!candidate.citation_verified);
    }
    // Identical content and source spans in separate single-item packs must
    // still identify the right conversation and message. Content deduplication
    // WITHIN a pack is intentionally unchanged.
    let candidates = vec![
        pack_candidate("local", "/shared/provider.db", Some(42), Some(8), 1),
        pack_candidate("local", "/shared/provider.db", Some(43), Some(8), 1),
        pack_candidate("local", "/shared/provider.db", Some(42), Some(9), 1),
        pack_candidate("remote-host", "/shared/provider.db", Some(42), Some(8), 1),
        pack_candidate("local", "/shared/provider.db", None, Some(8), 1),
        pack_candidate("local", "/shared/provider.db", Some(42), None, 1),
        // These collided with newline-delimited citation hashing.
        pack_candidate("local\n/shared", "provider.db", Some(42), Some(8), 1),
        pack_candidate("local", "/shared\nprovider.db", Some(42), Some(8), 1),
    ];
    let mut candidate_ids = std::collections::HashSet::new();
    let mut evidence_ids = std::collections::HashSet::new();
    for candidate in candidates {
        assert!(candidate_ids.insert(candidate.candidate_id.clone()));
        let request = PackPlanRequest {
            candidates: vec![candidate],
            ..Default::default()
        };
        let plan = plan_answer_pack(request.clone()).unwrap();
        let repeat = plan_answer_pack(request).unwrap();
        assert_eq!(plan.evidence.len(), 1);
        assert_eq!(plan.evidence[0].id, repeat.evidence[0].id);
        assert!(evidence_ids.insert(plan.evidence[0].id.clone()));
    }
}

#[test]
fn pack_session_caps_and_source_summaries_count_shared_path_conversations() {
    use coding_agent_search::search::pack_planner::{
        PackOmittedReason, PackPlanRequest, PackRenderRequest, plan_answer_pack,
        render_answer_pack_value_without_trust_correlation,
    };
    let candidates = vec![
        pack_candidate("local", "/shared/provider.db", Some(11), Some(1), 11),
        pack_candidate("local", "/shared/provider.db", Some(22), Some(1), 22),
        pack_candidate("local", "/shared/provider.db", Some(11), Some(2), 12),
    ];
    for cap in [1, 2] {
        let mut request = PackPlanRequest {
            candidates: candidates.clone(),
            ..Default::default()
        };
        request.limits.max_sessions = cap;
        let plan = plan_answer_pack(request.clone()).unwrap();
        assert_eq!(plan.selected_session_count, cap);
        assert_eq!(plan.selected_evidence_count, if cap == 1 { 2 } else { 3 });
        if cap == 1 {
            assert_eq!(plan.omitted.len(), 1);
            assert_eq!(
                plan.omitted[0].reason,
                PackOmittedReason::MaxSessionsReached
            );
        }
        request.candidates.reverse();
        let reversed = plan_answer_pack(request).unwrap();
        assert_eq!(
            plan.evidence
                .iter()
                .map(|item| &item.id)
                .collect::<Vec<_>>(),
            reversed
                .evidence
                .iter()
                .map(|item| &item.id)
                .collect::<Vec<_>>(),
            "equal-ranked shared paths must not make selection depend on input order",
        );
        let value = render_answer_pack_value_without_trust_correlation(
            &plan,
            &PackRenderRequest::default(),
        )
        .unwrap();
        assert_eq!(value["realized"]["selected_session_count"], cap);
        assert_eq!(value["pack"]["source_summary"][0]["session_count"], cap);
    }
}

#[test]
fn pack_physical_overlap_deduplication_is_scoped_to_the_source() {
    use coding_agent_search::search::pack_planner::{PackPlanRequest, plan_answer_pack};
    let mut left = pack_candidate("local", "/shared/session.jsonl", Some(11), Some(8), 11);
    left.line_start = Some(4);
    left.line_end = Some(7);
    left.citation_verified = true;
    let mut right = pack_candidate(
        "work-laptop",
        "/shared/session.jsonl",
        Some(22),
        Some(8),
        22,
    );
    right.line_start = left.line_start;
    right.line_end = left.line_end;
    right.citation_verified = true;
    let plan = plan_answer_pack(PackPlanRequest {
        candidates: vec![left.clone(), right.clone()],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        plan.selected_evidence_count, 2,
        "same pathname on different hosts is not one file"
    );
    right.source_id = "local".into();
    let plan = plan_answer_pack(PackPlanRequest {
        candidates: vec![left, right],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        plan.selected_evidence_count, 1,
        "actual same-source physical overlap is still deduplicated"
    );
}

#[test]
fn pack_renderer_coordinates_are_self_describing_without_bypassing_privacy() {
    use coding_agent_search::search::pack_planner::{
        PackPlanRequest, PackRenderFormat, PackRenderRequest, plan_answer_pack,
        render_answer_pack_without_trust_correlation,
    };
    let mut candidate = pack_candidate(
        "alice.internal",
        "/home/alice/history.jsonl",
        Some(42),
        Some(8),
        1,
    );
    candidate.origin_kind = "ssh".into();
    candidate.origin_host = Some("alice.internal".into());
    let plan = plan_answer_pack(PackPlanRequest {
        candidates: vec![candidate],
        ..Default::default()
    })
    .unwrap();
    for format in [
        PackRenderFormat::Json,
        PackRenderFormat::CompactJson,
        PackRenderFormat::Jsonl,
        PackRenderFormat::Markdown,
    ] {
        let request = PackRenderRequest {
            format,
            ..Default::default()
        };
        let output = render_answer_pack_without_trust_correlation(&plan, &request).unwrap();
        assert!(!output.contains("alice.internal"));
        assert!(!output.contains("/home/alice"));
        if format == PackRenderFormat::Markdown {
            assert!(output.contains("conversation_id=42 message_index=8 (1-based)"));
        } else if format == PackRenderFormat::Jsonl {
            let lines: Vec<Value> = output
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(lines[0]["schema_version"], "cass.pack.v2");
            let citation = &lines
                .iter()
                .find(|line| line.get("evidence").is_some())
                .unwrap()["evidence"]["citation"];
            assert_eq!(citation["message_index_base"], 1);
            assert_eq!(citation["message_index"], 8);
        } else {
            let payload: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(payload["schema_version"], "cass.pack.v2");
            let citation = &payload["evidence"][0]["citation"];
            assert_eq!(citation["message_index_base"], 1);
            assert_eq!(citation["message_index"], 8);
            assert_eq!(citation["conversation_id"], 42);
            assert!(citation["line_start"].is_null());
        }
    }
}

#[test]
fn actual_pack_citations_round_trip_unchanged_with_shared_paths_and_missing_sources() {
    let fixture = Fixture::new(&[0, 1, 7]);
    let storage = FrankenStorage::open(&fixture.db).unwrap();
    let other = seed(&storage, &fixture.path, "codex", &[0, 1, 7]);
    let targets = [
        (
            fixture.conversation_id,
            "PACK493TARGET first conversation evidence",
        ),
        (other, "PACK493TARGET second conversation evidence"),
    ];
    for (conversation_id, content) in targets {
        storage
            .raw()
            .execute_compat(
                "UPDATE messages SET content = ?1 WHERE conversation_id = ?2 AND idx = 1",
                coding_agent_search::franken_sync::params![content, conversation_id],
            )
            .unwrap();
    }
    drop(storage);
    for (conversation_id, _) in targets {
        let indexed = fixture
            .command("index")
            .arg("--data-dir")
            .arg(&fixture.data)
            .arg("--reconcile-conversation")
            .arg(conversation_id.to_string())
            .arg("--json")
            .output()
            .unwrap();
        assert!(
            indexed.status.success(),
            "{}",
            String::from_utf8_lossy(&indexed.stderr)
        );
    }
    // Both the previous canonical message and physical file line 2 exist and
    // are plausible WRONG targets. A mistaken offset must not pass this test.
    let wrong = decode(fixture.follow(
        "view",
        1,
        &["--conversation-id", &fixture.conversation_id.to_string()],
    ));
    assert_eq!(wrong["lines"][0]["content"], "canonical neighbour");
    let source = std::fs::read(&fixture.path).unwrap();
    let retained_source = fixture.path.with_extension("retained");
    std::fs::rename(&fixture.path, &retained_source).unwrap();
    let before = std::fs::read(&fixture.db).unwrap();
    for (format, args) in [
        ("json", vec!["--json"]),
        ("compact", vec!["--robot-format", "compact"]),
        ("jsonl", vec!["--robot-format", "jsonl"]),
        ("minimal", vec!["--json", "--fields", "minimal"]),
        (
            "custom",
            vec![
                "--json",
                "--fields",
                "schema_version,evidence[].citation,evidence[].excerpt",
            ],
        ),
    ] {
        let output = fixture
            .command("pack")
            .args([
                "PACK493TARGET",
                "--mode",
                "lexical",
                "--freshness-policy",
                "allow-stale",
                "--max-sessions",
                "2",
                "--max-tokens",
                "20000",
                "--timeout",
                "30000",
                "--require-evidence",
            ])
            .arg("--data-dir")
            .arg(&fixture.data)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{format}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let items: Vec<Value> = if format == "jsonl" {
            let records: Vec<Value> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(records[0]["schema_version"], "cass.pack.v2");
            records
                .into_iter()
                .filter_map(|mut row| row.get_mut("evidence").map(Value::take))
                .collect()
        } else {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["schema_version"], "cass.pack.v2");
            if format == "json" || format == "compact" {
                assert_eq!(value["realized"]["selected_session_count"], 2);
                assert_eq!(value["pack"]["source_summary"][0]["session_count"], 2);
            }
            value["evidence"].as_array().unwrap().clone()
        };
        assert_eq!(
            items.len(),
            2,
            "{format}: distinct conversations cannot be collapsed"
        );
        let mut seen = std::collections::HashSet::new();
        for item in items {
            let citation = &item["citation"];
            assert_eq!(citation["message_index_base"], 1);
            assert_eq!(citation["message_index"], 2);
            assert!(citation["line_start"].is_null());
            let cid = citation["conversation_id"].as_i64().unwrap();
            assert!(seen.insert(cid));
            let expected = targets.iter().find(|(id, _)| *id == cid).unwrap().1;
            assert_eq!(item["excerpt"], expected);
            for command in ["view", "expand"] {
                let value = decode(
                    fixture
                        .command(command)
                        .arg(citation["source_path"].as_str().unwrap())
                        .args(["--source", citation["source_id"].as_str().unwrap()])
                        .arg("--conversation-id")
                        .arg(cid.to_string())
                        .arg("--message-index")
                        .arg(citation["message_index"].as_u64().unwrap().to_string())
                        .args(["-C", "0", "--json"])
                        .output()
                        .unwrap(),
                );
                let rows = if command == "view" {
                    &value["lines"]
                } else {
                    &value
                };
                assert_eq!(rows.as_array().unwrap().len(), 1);
                assert_eq!(rows[0]["content"], expected);
                assert_eq!(rows[0]["conversation_id"], cid);
                assert_eq!(rows[0]["message_index"], 2);
                assert_eq!(rows[0]["is_target"], true);
            }
        }
    }
    let capped = decode(
        fixture
            .command("pack")
            .args([
                "PACK493TARGET",
                "--mode",
                "lexical",
                "--freshness-policy",
                "allow-stale",
                "--json",
                "--max-sessions",
                "1",
                "--max-tokens",
                "20000",
                "--timeout",
                "30000",
                "--require-evidence",
            ])
            .arg("--data-dir")
            .arg(&fixture.data)
            .output()
            .unwrap(),
    );
    assert_eq!(capped["realized"]["selected_session_count"], 1);
    assert_eq!(capped["evidence"].as_array().unwrap().len(), 1);
    assert_eq!(std::fs::read(&fixture.db).unwrap(), before);
    assert_eq!(std::fs::read(&retained_source).unwrap(), source);
    assert!(!fixture.path.exists());
}
