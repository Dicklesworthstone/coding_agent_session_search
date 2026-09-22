//! Bounded canonical context hydration through the real CLI and archive.
use coding_agent_search::franken_sync::compat::ConnectionExt;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;

// These tests execute the exact selection algorithm used by production.
#[path = "../src/followup_coordinates/window.rs"]
mod window;

const RECORD_BYTES: usize = 8 * 1024 * 1024;
const WINDOW_RECORDS: usize = 4096;

struct Fixture {
    root: TempDir,
    db: PathBuf,
    source: PathBuf,
    conversation_id: i64,
}

impl Fixture {
    fn new() -> anyhow::Result<Self> {
        let root = tempfile::tempdir()?;
        let db = root.path().join("agent_search.db");
        let source = root.path().join("absent-transcript.jsonl");
        let storage = FrankenStorage::open(&db)?;
        let agent_id = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let outcome = storage.insert_conversation_tree(
            agent_id,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some("windowed-followup".into()),
                title: Some("Windowed follow-up".into()),
                source_path: source.clone(),
                started_at: None,
                ended_at: None,
                approx_tokens: None,
                metadata_json: json!({}),
                messages: [0, 7, 12, 99, 1000]
                    .into_iter()
                    .map(|idx| Message {
                        id: None,
                        idx,
                        role: MessageRole::Agent,
                        author: None,
                        created_at: None,
                        content: format!("canonical message idx {idx}"),
                        extra_json: json!({}),
                        snippets: Vec::new(),
                    })
                    .collect(),
                source_id: "local".into(),
                origin_host: None,
            },
        )?;
        drop(storage);
        Ok(Self {
            root,
            db,
            source,
            conversation_id: outcome.conversation_id,
        })
    }

    fn command(&self, subcommand: &str, number: usize, context: usize) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
        command
            .arg("--db")
            .arg(&self.db)
            .arg(subcommand)
            .arg(&self.source)
            .args([
                "--message-index",
                &number.to_string(),
                "-C",
                &context.to_string(),
                "--source",
                "local",
                "--json",
            ])
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_VIEW_BUDGET_MS", "30000")
            .env_remove("CASS_TEST_VIEW_SLOW_MS")
            .env_remove("CASS_OUTPUT_FORMAT")
            .env_remove("TOON_DEFAULT_FORMAT");
        command
    }

    fn poison_excluded_roles(&self) -> anyhow::Result<()> {
        let storage = FrankenStorage::open(&self.db)?;
        // A BLOB cannot decode as Option<String>. This makes accidental full
        // transcript hydration fail deterministically, without timing/RSS tests.
        storage.raw().execute_compat(
            "UPDATE messages SET role = ?1 WHERE conversation_id = ?2 AND idx IN (0, 1000)",
            coding_agent_search::franken_sync::params![vec![255_u8], self.conversation_id],
        )?;
        Ok(())
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

fn assert_resource_refusal(output: Output) {
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "no partial canonical target may escape"
    );
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["error"]["kind"], "followup-resource-limit");
    assert_eq!(error["error"]["retryable"], false);
}

#[test]
fn canonical_byte_guards_cover_multibyte_nul_and_role_payloads_without_reading_excluded_bodies()
-> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let storage = FrankenStorage::open(&fixture.db)?;
    // Both character-counting and TEXT length up to NUL would understate this.
    let oversized = format!("\0{}", "é".repeat(RECORD_BYTES / 2));
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1 WHERE conversation_id = ?2 AND idx = 0",
        coding_agent_search::franken_sync::params![oversized.as_str(), fixture.conversation_id],
    )?;
    storage.raw().execute_compat(
        "UPDATE messages SET role = ?1 WHERE conversation_id = ?2 AND idx = 7",
        coding_agent_search::franken_sync::params![oversized.as_str(), fixture.conversation_id],
    )?;
    let complete = "before\0after 漢字 😀";
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1 WHERE conversation_id = ?2 AND idx = 12",
        coding_agent_search::franken_sync::params![complete, fixture.conversation_id],
    )?;
    drop(storage);
    let before = std::fs::read(&fixture.db)?;
    for command in ["view", "expand"] {
        let payload = decode(fixture.command(command, 13, 0).output()?);
        let rows = if command == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        assert_eq!(
            rows[0]["content"], complete,
            "NUL is not a truncation boundary"
        );
        assert_eq!(rows[0]["conversation_id"], fixture.conversation_id);
        for target in [1, 8] {
            assert_resource_refusal(fixture.command(command, target, 0).output()?);
        }
    }
    assert_eq!(std::fs::read(&fixture.db)?, before);
    assert!(!fixture.source.exists());
    Ok(())
}

#[test]
fn canonical_total_window_bytes_fail_closed_while_narrow_same_identity_succeeds()
-> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let storage = FrankenStorage::open(&fixture.db)?;
    let body = "x".repeat(7 * 1024 * 1024);
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1 WHERE conversation_id = ?2",
        coding_agent_search::franken_sync::params![body.as_str(), fixture.conversation_id],
    )?;
    drop(storage);
    let before = std::fs::read(&fixture.db)?;
    for command in ["view", "expand"] {
        // Five individually admissible rows exceed the aggregate 32 MiB cap.
        assert_resource_refusal(fixture.command(command, 13, 2).output()?);
        let payload = decode(fixture.command(command, 13, 0).output()?);
        let rows = if command == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["content"].as_str().unwrap(), body);
        assert_eq!(rows[0]["message_index"], 13);
    }
    assert_eq!(std::fs::read(&fixture.db)?, before);
    Ok(())
}

#[test]
fn canonical_actual_window_count_is_bounded_but_tail_metadata_still_counts() -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let storage = FrankenStorage::open(&fixture.db)?;
    storage.raw().execute("BEGIN IMMEDIATE")?;
    for idx in 2000..2000 + WINDOW_RECORDS {
        storage.raw().execute_compat(
            "INSERT INTO messages (conversation_id, idx, role, content) VALUES (?1, ?2, 'user', 'bounded tail')",
            coding_agent_search::franken_sync::params![fixture.conversation_id, idx as i64],
        )?;
    }
    storage.raw().execute("COMMIT")?;
    drop(storage);
    let before = std::fs::read(&fixture.db)?;
    for command in ["view", "expand"] {
        assert_resource_refusal(fixture.command(command, 1, usize::MAX).output()?);
        let payload = decode(fixture.command(command, 13, 0).output()?);
        let rows = if command == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["content"], "canonical message idx 12");
        if command == "view" {
            assert_eq!(payload["total_messages"], WINDOW_RECORDS + 5);
        }
    }
    assert_eq!(std::fs::read(&fixture.db)?, before);
    Ok(())
}

#[test]
fn sparse_context_counts_messages_and_preserves_complete_identity() -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    let before = std::fs::read(&fixture.db)?;
    for command in ["view", "expand"] {
        let payload = decode(fixture.command(command, 13, 1).output()?);
        let rows = if command == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        let rows = rows.as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row["message_index"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [8, 13, 100]
        );
        assert_eq!(rows[1]["content"], "canonical message idx 12");
        assert_eq!(
            rows.iter().filter(|row| row["is_target"] == true).count(),
            1
        );
        for row in rows {
            assert_eq!(row["conversation_id"], fixture.conversation_id);
            assert_eq!(row["source_id"], "local");
            assert_eq!(row["content_source"], "archive");
            assert_eq!(row["coordinate_space"], "message_index");
        }
        if command == "view" {
            assert_eq!(payload["total_messages"], 5);
            assert_eq!(payload["archive_only"], true);
        }
    }
    assert_eq!(std::fs::read(&fixture.db)?, before);
    Ok(())
}

#[test]
fn excluded_payloads_are_skipped_but_requested_errors_fail_closed() -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    fixture.poison_excluded_roles()?;
    let before = std::fs::read(&fixture.db)?;
    for command in ["view", "expand"] {
        for context in [0, 1] {
            let payload = decode(fixture.command(command, 13, context).output()?);
            let rows = if command == "view" {
                &payload["lines"]
            } else {
                &payload
            };
            let rows = rows.as_array().unwrap();
            assert_eq!(rows.len(), context * 2 + 1);
            assert_eq!(rows[context]["content"], "canonical message idx 12");
        }
        // The same poison must be an error once it enters the requested window.
        let output = fixture.command(command, 13, 2).output()?;
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "no partially certified target on error"
        );
        let output = fixture.command(command, 1001, 0).output()?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
    assert_eq!(std::fs::read(&fixture.db)?, before);
    Ok(())
}

#[test]
fn ambiguous_identity_is_refused_before_payload_hydration() -> anyhow::Result<()> {
    let fixture = Fixture::new()?;
    fixture.poison_excluded_roles()?;
    let storage = FrankenStorage::open(&fixture.db)?;
    storage.raw().execute_compat(
        "INSERT INTO conversations(agent_id, external_id, source_path, source_id)
         SELECT agent_id, 'empty-second-session', source_path, source_id FROM conversations WHERE id = ?1",
        coding_agent_search::franken_sync::params![fixture.conversation_id],
    )?;
    drop(storage);
    for command in ["view", "expand"] {
        let output = fixture.command(command, 13, 1).output()?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("ambiguous-source"));
        let payload = decode(
            fixture
                .command(command, 13, 1)
                .arg("--conversation-id")
                .arg(fixture.conversation_id.to_string())
                .output()?,
        );
        let rows = if command == "view" {
            &payload["lines"]
        } else {
            &payload
        };
        assert_eq!(rows[1]["content"], "canonical message idx 12");
    }
    Ok(())
}
