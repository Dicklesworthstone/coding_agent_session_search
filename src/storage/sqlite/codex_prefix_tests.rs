//! GH #559: receipt admission must agree with actual legacy canonical rows.
#![cfg(unix)]

use super::*;
use crate::connectors::{Connector, ScanContext, ScanRoot};

struct Fixture {
    _directory: tempfile::TempDir,
    storage: FrankenStorage,
    agent: i64,
    conversation: Conversation,
}

impl Fixture {
    fn new(count: usize, repeated: bool, timestamps: bool) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("rollout-legacy-admission.jsonl");
        let mut bytes = String::new();
        for idx in 0..count {
            let content = if repeated {
                "repeat the same legitimate record".to_owned()
            } else {
                format!("canonical prefix message {idx}")
            };
            let mut record = serde_json::json!({
                "type": "response_item",
                "payload": {"role": "user", "content": content}
            });
            if timestamps {
                let second = if repeated { 0 } else { idx };
                record["timestamp"] = serde_json::json!(format!("2026-11-23T12:00:{second:02}Z"));
            }
            bytes.push_str(&record.to_string());
            bytes.push('\n');
        }
        std::fs::write(&source, bytes)?;
        let ctx = ScanContext::with_roots(
            directory.path().join("cass"),
            vec![ScanRoot::local(source)],
            None,
        );
        let mut normalized = crate::connectors::codex::CodexConnector::new().scan(&ctx)?;
        assert_eq!(normalized.len(), 1);
        let conversation = crate::indexer::persist::map_to_internal(&normalized.remove(0));
        assert_eq!(conversation.messages.len(), count);
        assert!(codex_prefix_packet(&conversation));
        let storage = FrankenStorage::open(&directory.path().join("archive.db"))?;
        let agent = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".to_owned(),
            name: "Codex".to_owned(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        Ok(Self {
            _directory: directory,
            storage,
            agent,
            conversation,
        })
    }

    fn legacy(&self, count: usize) -> Conversation {
        let mut conversation = self.conversation.clone();
        conversation.messages.truncate(count);
        conversation.metadata_json = serde_json::json!({"legacy_archive": true});
        conversation.ended_at = conversation
            .messages
            .last()
            .and_then(|message| message.created_at);
        conversation
    }

    fn seed(&self, conversation: &Conversation) -> Result<i64> {
        let outcome = self.storage.insert_conversation_tree_with_analytics(
            self.agent,
            None,
            conversation,
            false,
        )?;
        assert!(outcome.conversation_inserted);
        assert!(self.storage.source_ingest_ledger_entries()?.is_empty());
        Ok(outcome.conversation_id)
    }

    fn completion(&self) -> SourceIngestLedgerEntry {
        SourceIngestLedgerEntry {
            key: "source_ingest_v1:codex-prefix-regression".to_owned(),
            observation: serde_json::json!({
                "codex_prefix": self.conversation.metadata_json.pointer("/cass/codex_stable_prefix")
            })
            .to_string(),
        }
    }

    fn ingest(&self, conversation: &Conversation, batched: bool) -> Result<InsertOutcome> {
        if batched {
            let mut outcomes = self.storage.insert_conversations_batched_with_completion(
                &[(self.agent, None, conversation)],
                false,
                Some(&self.completion()),
            )?;
            assert_eq!(outcomes.len(), 1);
            Ok(outcomes.remove(0))
        } else {
            self.storage.insert_conversation_tree_with_analytics(
                self.agent,
                None,
                conversation,
                false,
            )
        }
    }

    fn rows(&self, conversation_id: i64) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(
            self.storage.fetch_messages(conversation_id)?,
        )?)
    }
}

#[test]
fn codex_prefix_rejects_divergent_legacy_overlap_before_canonical_or_receipt_commit() -> Result<()>
{
    for batched in [false, true] {
        for field in ["content", "role", "author", "timestamp"] {
            let fixture = Fixture::new(3, false, true)?;
            let mut legacy = fixture.legacy(2);
            match field {
                "content" => legacy.messages[0].content = "unrelated legacy variant".to_owned(),
                "role" => legacy.messages[0].role = MessageRole::Agent,
                "author" => legacy.messages[0].author = Some("different author".to_owned()),
                "timestamp" => legacy.messages[0].created_at = None,
                _ => unreachable!(),
            }
            let id = fixture.seed(&legacy)?;
            let before = fixture.rows(id)?;
            let error = fixture
                .ingest(&fixture.conversation, batched)
                .err()
                .context("divergent canonical overlap was accepted")?;
            assert!(
                error.to_string().contains("canonical message idx 0"),
                "{field}: {error:#}"
            );
            assert!(!error.to_string().contains("unrelated legacy variant"));
            assert_eq!(fixture.rows(id)?, before, "{field}, batched={batched}");
            assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
        }
    }
    Ok(())
}

#[test]
fn codex_prefix_legacy_append_and_replay_use_exact_indices() -> Result<()> {
    for batched in [false, true] {
        let fixture = Fixture::new(3, false, true)?;
        let id = fixture.seed(&fixture.legacy(2))?;
        let append = fixture.ingest(&fixture.conversation, batched)?;
        assert_eq!(append.conversation_id, id);
        assert_eq!(append.inserted_indices, [2]);
        assert!(!append.conversation_inserted);
        let before = fixture.rows(id)?;
        assert!(
            fixture
                .ingest(&fixture.conversation, batched)?
                .inserted_indices
                .is_empty()
        );
        assert_eq!(fixture.rows(id)?, before);
        assert_eq!(fixture.storage.fetch_messages(id)?.len(), 3);
        assert_eq!(
            fixture.storage.source_ingest_ledger_entries()?.len(),
            usize::from(batched)
        );
    }
    Ok(())
}

#[test]
fn codex_prefix_distinct_repeated_records_survive_new_and_legacy_ingestion() -> Result<()> {
    for batched in [false, true] {
        for timestamps in [false, true] {
            for legacy_count in [0, 1] {
                let fixture = Fixture::new(3, true, timestamps)?;
                if legacy_count > 0 {
                    fixture.seed(&fixture.legacy(legacy_count))?;
                }
                let outcome = fixture.ingest(&fixture.conversation, batched)?;
                let expected: Vec<_> = (legacy_count as i64..3).collect();
                assert_eq!(outcome.inserted_indices, expected);
                let messages = fixture.storage.fetch_messages(outcome.conversation_id)?;
                assert_eq!(messages.len(), 3);
                assert_eq!(
                    messages
                        .iter()
                        .map(|message| message.idx)
                        .collect::<Vec<_>>(),
                    [0, 1, 2]
                );
                assert!(messages.windows(2).all(|pair| {
                    pair[0].content == pair[1].content && pair[0].created_at == pair[1].created_at
                }));
                assert!(
                    fixture
                        .ingest(&fixture.conversation, batched)?
                        .inserted_indices
                        .is_empty()
                );
            }
        }
    }
    Ok(())
}

#[test]
fn codex_prefix_slices_use_whole_receipt_boundary_and_reject_shorter_history() -> Result<()> {
    let fixture = Fixture::new(5, false, true)?;
    let id = fixture.seed(&fixture.legacy(3))?;
    let mut first = fixture.conversation.clone();
    first.messages = first.messages[..2].to_vec();
    // Existing idx 2 is beyond this slice but inside the unchanged receipt N=5.
    let result = fixture
        .storage
        .insert_conversations_batched_with_completion(
            &[(fixture.agent, None, &first)],
            false,
            None,
        )?;
    assert!(result[0].inserted_indices.is_empty());
    let mut second = fixture.conversation.clone();
    second.messages = second.messages[2..4].to_vec();
    let result = fixture
        .storage
        .insert_conversations_batched_with_completion(
            &[(fixture.agent, None, &second)],
            false,
            None,
        )?;
    assert_eq!(result[0].inserted_indices, [3]);
    assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
    let mut final_slice = fixture.conversation.clone();
    final_slice.messages = final_slice.messages[4..].to_vec();
    assert_eq!(fixture.ingest(&final_slice, true)?.inserted_indices, [4]);
    assert_eq!(fixture.storage.fetch_messages(id)?.len(), 5);
    assert!(
        fixture
            .ingest(&fixture.conversation, true)?
            .inserted_indices
            .is_empty()
    );

    let shorter = Fixture::new(3, false, true)?;
    let mut legacy = shorter.legacy(3);
    let mut tail = legacy.messages[2].clone();
    tail.idx = 3;
    tail.content = "canonical tail absent from admitted source".to_owned();
    legacy.messages.push(tail);
    let id = shorter.seed(&legacy)?;
    let before = shorter.rows(id)?;
    let mut slice = shorter.conversation.clone();
    slice.messages.truncate(1);
    let error = shorter
        .ingest(&slice, true)
        .err()
        .context("short source accepted")?;
    assert!(error.to_string().contains("shorter than canonical history"));
    assert_eq!(shorter.rows(id)?, before);
    assert!(shorter.storage.source_ingest_ledger_entries()?.is_empty());
    Ok(())
}

#[test]
fn codex_prefix_checks_pending_rows_and_rolls_back_the_entire_final_batch() -> Result<()> {
    let fixture = Fixture::new(3, false, true)?;
    let id = fixture.seed(&fixture.legacy(1))?;
    let before = fixture.rows(id)?;
    let mut first = fixture.conversation.clone();
    first.messages = first.messages[1..2].to_vec();
    let mut second = fixture.conversation.clone();
    second.messages = second.messages[1..].to_vec();
    second.messages[0].content = "conflicts with preceding slice in same transaction".to_owned();
    let result = fixture
        .storage
        .insert_conversations_batched_with_completion(
            &[
                (fixture.agent, None, &first),
                (fixture.agent, None, &second),
            ],
            false,
            Some(&fixture.completion()),
        );
    let error = result
        .err()
        .context("pending canonical conflict was accepted")?;
    assert!(error.to_string().contains("canonical message idx 1"));
    assert_eq!(fixture.rows(id)?, before);
    assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
    Ok(())
}

#[test]
fn codex_prefix_absent_metadata_preserves_legacy_behavior_but_malformed_proof_fails_closed()
-> Result<()> {
    let fixture = Fixture::new(3, false, true)?;
    let mut legacy = fixture.legacy(2);
    legacy.messages[0].content = "older canonical variant".to_owned();
    let id = fixture.seed(&legacy)?;
    let ordinary = fixture.legacy(3);
    assert_eq!(fixture.ingest(&ordinary, false)?.inserted_indices, [2]);
    assert_eq!(
        fixture.storage.fetch_messages(id)?[0].content,
        "older canonical variant"
    );
    let before = fixture.rows(id)?;
    let mut invalid = fixture.conversation.clone();
    invalid.metadata_json["cass"]["codex_stable_prefix"] = serde_json::json!({});
    let error = fixture
        .ingest(&invalid, true)
        .err()
        .context("malformed proof accepted")?;
    assert!(error.to_string().contains("invalid Codex prefix metadata"));
    assert_eq!(fixture.rows(id)?, before);
    assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
    Ok(())
}
