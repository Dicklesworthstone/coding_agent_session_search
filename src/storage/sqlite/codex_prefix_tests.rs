//! GH #559: receipt admission must agree with actual legacy canonical rows.
#![cfg(unix)]

use super::*;
use crate::connectors::codex::StablePrefixProvider;
use crate::connectors::{Connector, ScanContext, ScanRoot};

struct Fixture {
    _directory: tempfile::TempDir,
    storage: FrankenStorage,
    agent: i64,
    conversation: Conversation,
}

impl Fixture {
    fn new(count: usize, repeated: bool, timestamps: bool) -> Result<Self> {
        Self::for_provider(
            count,
            repeated,
            timestamps,
            crate::connectors::codex::StablePrefixProvider::Codex,
        )
    }

    fn for_provider(
        count: usize,
        repeated: bool,
        timestamps: bool,
        provider: crate::connectors::codex::StablePrefixProvider,
    ) -> Result<Self> {
        use crate::connectors::codex::StablePrefixProvider;
        let directory = tempfile::tempdir()?;
        let source = directory.path().join(match provider {
            StablePrefixProvider::Codex => "rollout-legacy-admission.jsonl",
            StablePrefixProvider::ClaudeCode => "claude-legacy-admission.jsonl",
            StablePrefixProvider::PiAgent => ".pi/agent/sessions/project/legacy_admission.jsonl",
            StablePrefixProvider::Omp => ".omp/agent/sessions/project/legacy_admission.jsonl",
        });
        std::fs::create_dir_all(source.parent().unwrap())?;
        let mut bytes = String::new();
        if matches!(
            provider,
            StablePrefixProvider::PiAgent | StablePrefixProvider::Omp
        ) {
            bytes.push_str(
                &serde_json::json!({
                    "type":"session", "id":"pi-family-canonical-prefix", "version":3
                })
                .to_string(),
            );
            bytes.push('\n');
        }
        for idx in 0..count {
            let content = if repeated {
                "repeat the same legitimate record".to_owned()
            } else {
                format!("canonical prefix message {idx}")
            };
            let mut record = match provider {
                StablePrefixProvider::Codex => serde_json::json!({
                    "type": "response_item",
                    "payload": {"role": "user", "content": content}
                }),
                StablePrefixProvider::ClaudeCode => serde_json::json!({
                    "type": "user", "uuid": format!("claude-prefix-{idx}"),
                    "message": {"role": "user", "content": content}
                }),
                StablePrefixProvider::PiAgent | StablePrefixProvider::Omp => serde_json::json!({
                    "type": "message", "id": format!("pi-prefix-{idx}"),
                    "message": {"role": "user", "content": content}
                }),
            };
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
        let mut normalized = match provider {
            StablePrefixProvider::Codex => {
                crate::connectors::codex::CodexConnector::new().scan(&ctx)?
            }
            StablePrefixProvider::ClaudeCode => {
                crate::connectors::claude_code::ClaudeCodeConnector::new().scan(&ctx)?
            }
            StablePrefixProvider::PiAgent => {
                crate::connectors::pi_agent::PiAgentConnector::new().scan(&ctx)?
            }
            StablePrefixProvider::Omp => {
                crate::connectors::omp::OmpConnector::new().scan(&ctx)?
            }
        };
        assert_eq!(normalized.len(), 1);
        let conversation = crate::indexer::persist::map_to_internal(&normalized.remove(0));
        assert_eq!(conversation.messages.len(), count);
        assert!(source_prefix_packet(&conversation));
        let storage = FrankenStorage::open(&directory.path().join("archive.db"))?;
        let agent = storage.ensure_agent(&Agent {
            id: None,
            slug: provider.slug().to_owned(),
            name: provider.slug().to_owned(),
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
        let provider = crate::connectors::codex::StablePrefixProvider::from_slug(
            &self.conversation.agent_slug,
        )
        .expect("fixture provider");
        let mut observation = serde_json::json!({});
        observation[provider.observation_key()] =
            self.conversation.metadata_json["cass"][provider.metadata_key()].clone();
        SourceIngestLedgerEntry {
            key: format!("source_ingest_v1:{}-prefix-regression", provider.slug()),
            observation: observation.to_string(),
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
fn claude_prefix_preserves_repeated_turns_and_requires_future_proven_admission() -> Result<()> {
    for batched in [false, true] {
        let fixture = Fixture::for_provider(
            3,
            true,
            true,
            crate::connectors::codex::StablePrefixProvider::ClaudeCode,
        )?;
        let id = fixture.seed(&fixture.legacy(1))?;
        let appended = fixture.ingest(&fixture.conversation, batched)?;
        assert_eq!(appended.conversation_id, id);
        assert_eq!(appended.inserted_indices, [1, 2]);
        let rows = fixture.rows(id)?;
        assert_eq!(rows.as_array().context("canonical rows")?.len(), 3);
        assert!(
            fixture
                .ingest(&fixture.conversation, batched)?
                .inserted_indices
                .is_empty()
        );
        assert_eq!(fixture.rows(id)?, rows);
        let error = fixture
            .ingest(&fixture.legacy(3), batched)
            .err()
            .context("ordinary replay bypassed the durable Claude prefix requirement")?;
        assert!(error.to_string().contains("requires prefix admission"));
        let refusal = error
            .downcast_ref::<CodexCanonicalRefusal>()
            .context("Claude requirement lost its typed refusal")?;
        assert_eq!(refusal.provider, StablePrefixProvider::ClaudeCode);
        assert_eq!(refusal.source_path, fixture.conversation.source_path);
        assert_eq!(refusal.external_id, fixture.conversation.external_id);
        assert_eq!(refusal.conversation_id, id);
        assert!(matches!(
            refusal.reason,
            CodexCanonicalRefusalReason::PrefixRequired
        ));
        assert_eq!(fixture.rows(id)?, rows);
    }
    Ok(())
}

#[test]
fn claude_prefix_rejects_divergent_canonical_history_and_provider_substitution() -> Result<()> {
    for batched in [false, true] {
        let fixture = Fixture::for_provider(
            3,
            false,
            true,
            crate::connectors::codex::StablePrefixProvider::ClaudeCode,
        )?;
        let mut legacy = fixture.legacy(2);
        legacy.messages[0].content = "a different canonical Claude turn".to_owned();
        let id = fixture.seed(&legacy)?;
        let rows = fixture.rows(id)?;
        let error = fixture
            .ingest(&fixture.conversation, batched)
            .err()
            .context("divergent Claude overlap was accepted")?;
        assert!(error.to_string().contains("canonical message idx 0"));
        let refusal = error
            .downcast_ref::<CodexCanonicalRefusal>()
            .context("Claude overlap lost its typed refusal")?;
        assert_eq!(refusal.provider, StablePrefixProvider::ClaudeCode);
        assert_eq!(refusal.source_path, fixture.conversation.source_path);
        assert_eq!(refusal.external_id, fixture.conversation.external_id);
        assert_eq!(refusal.conversation_id, id);
        assert!(matches!(
            refusal.reason,
            CodexCanonicalRefusalReason::ProjectionMismatch(0)
        ));
        assert_eq!(fixture.rows(id)?, rows);
        assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
        let mut substituted = fixture.conversation.clone();
        substituted.metadata_json["cass"]["claude_stable_prefix"]["provider"] =
            serde_json::json!("codex");
        let error = fixture
            .ingest(&substituted, batched)
            .err()
            .context("foreign provider proof was accepted")?;
        assert!(error.to_string().contains("invalid Claude prefix metadata"));
        assert!(
            error.downcast_ref::<CodexCanonicalRefusal>().is_none(),
            "provider substitution is not an isolatable canonical disagreement"
        );
        let mut malformed = fixture.conversation.clone();
        malformed.metadata_json["cass"]["claude_stable_prefix"] = serde_json::json!({});
        let error = fixture
            .ingest(&malformed, batched)
            .err()
            .context("malformed Claude proof was accepted")?;
        assert!(error.to_string().contains("invalid Claude prefix metadata"));
        assert!(
            error.downcast_ref::<CodexCanonicalRefusal>().is_none(),
            "malformed metadata is not an isolatable canonical disagreement"
        );
        assert_eq!(fixture.rows(id)?, rows);
        assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
    }
    Ok(())
}

#[test]
fn new_prefix_conversation_refuses_a_foreign_provider_before_any_insert() -> Result<()> {
    let fixture = Fixture::for_provider(
        1,
        false,
        true,
        crate::connectors::codex::StablePrefixProvider::ClaudeCode,
    )?;
    let mut incoming = fixture.conversation.clone();
    incoming.metadata_json["cass"]["claude_stable_prefix"]["provider"] = serde_json::json!("codex");
    assert!(fixture.ingest(&incoming, true).is_err());
    assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
    assert!(
        fixture
            .ingest(&fixture.conversation, true)?
            .conversation_inserted
    );
    Ok(())
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
            let refusal = error
                .downcast_ref::<CodexCanonicalRefusal>()
                .context("canonical overlap lost its typed refusal")?;
            assert_eq!(refusal.provider, StablePrefixProvider::Codex);
            assert_eq!(refusal.source_path, fixture.conversation.source_path);
            assert_eq!(refusal.external_id, fixture.conversation.external_id);
            assert_eq!(refusal.conversation_id, id);
            assert!(matches!(
                refusal.reason,
                CodexCanonicalRefusalReason::ProjectionMismatch(0)
            ));
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
    let refusal = error
        .downcast_ref::<CodexCanonicalRefusal>()
        .context("short source lost its typed refusal")?;
    assert_eq!(refusal.provider, StablePrefixProvider::Codex);
    assert!(matches!(
        refusal.reason,
        CodexCanonicalRefusalReason::HistoryBeyondPrefix
    ));
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
    assert!(
        error.downcast_ref::<CodexCanonicalRefusal>().is_none(),
        "malformed metadata is not an isolatable canonical disagreement"
    );
    assert_eq!(fixture.rows(id)?, before);
    assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
    Ok(())
}


impl Fixture {
    fn metadata_columns(&self, conversation_id: i64) -> Result<(Option<String>, Option<Vec<u8>>)> {
        Ok(self.storage.raw().query_row_map(
            "SELECT metadata_json, metadata_bin FROM conversations WHERE id = ?1",
            fparams![conversation_id],
            |row| Ok((row.get_typed(0)?, row.get_typed(1)?)),
        )?)
    }

    fn metadata(&self, conversation_id: i64) -> Result<serde_json::Value> {
        Ok(self.storage.raw().query_row_map(
            "SELECT metadata_json, metadata_bin FROM conversations WHERE id = ?1",
            fparams![conversation_id],
            |row| Ok(franken_read_metadata_compat(row, 0, 1)),
        )?)
    }

    fn replace_metadata(
        &self,
        conversation_id: i64,
        json: Option<&str>,
        binary: Option<&[u8]>,
    ) -> Result<()> {
        self.storage.raw().execute_compat(
            "UPDATE conversations SET metadata_json = ?1, metadata_bin = ?2 WHERE id = ?3",
            fparams![json, binary, conversation_id],
        )?;
        Ok(())
    }

    fn ordinary_append(&self) -> Conversation {
        let mut conversation = self.legacy(self.conversation.messages.len());
        let mut next = conversation.messages.last().unwrap().clone();
        next.idx += 1;
        next.created_at = next.created_at.map(|timestamp| timestamp + 1_000);
        next.content = "new suffix from the strict parser".to_owned();
        conversation.ended_at = next.created_at;
        conversation.messages.push(next);
        conversation
    }
}

#[test]
fn codex_prefix_requirement_blocks_downgrade_for_new_and_existing_receipt_metadata() -> Result<()> {
    for batched in [false, true] {
        for old_receipt_without_requirement in [false, true] {
            let fixture = Fixture::new(3, false, true)?;
            let id = fixture.seed(&fixture.conversation)?;
            assert_eq!(
                fixture.metadata(id)?.pointer("/cass/codex_prefix_required"),
                Some(&serde_json::Value::Bool(true))
            );
            if old_receipt_without_requirement {
                // Older native prefix rows already retained a full receipt,
                // but predate the explicit canonical protocol requirement.
                let bytes = rmp_serde::to_vec(&fixture.conversation.metadata_json)?;
                fixture.replace_metadata(id, None, Some(&bytes))?;
                assert!(fixture.metadata(id)?.pointer("/cass/codex_prefix_required").is_none());
            }
            let rows = fixture.rows(id)?;
            let metadata = fixture.metadata_columns(id)?;
            let mut strict = fixture.ordinary_append();
            strict.messages[0].content = "rewritten prefix under another parser mode".to_owned();
            let error = fixture.ingest(&strict, batched).err().context("parser downgrade merged")?;
            assert!(error.to_string().contains("requires prefix admission"));
            let refusal = error
                .downcast_ref::<CodexCanonicalRefusal>()
                .context("required prefix lost its typed refusal")?;
            assert_eq!(refusal.provider, StablePrefixProvider::Codex);
            assert!(matches!(
                refusal.reason,
                CodexCanonicalRefusalReason::PrefixRequired
            ));
            assert_eq!(fixture.rows(id)?, rows);
            assert_eq!(fixture.metadata_columns(id)?, metadata);
            assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
        }
    }
    Ok(())
}

#[test]
fn codex_prefix_requirement_preserves_legacy_metadata_for_append_and_noop_upgrades() -> Result<()> {
    for batched in [false, true] {
        for binary in [false, true] {
            for append in [false, true] {
                let fixture = Fixture::new(3, false, true)?;
                let seed_count = if append { 2 } else { 3 };
                let legacy = fixture.legacy(seed_count);
                let id = fixture.seed(&legacy)?;
                let preserved = serde_json::json!({
                    "legacy_archive": true,
                    "provider": {"retained": ["one", 2, false]},
                    "cass": {"workspace_original": "/remote/project", "unrelated": {"keep": true}}
                });
                let json = serde_json::to_string(&preserved)?;
                let packed = rmp_serde::to_vec(&preserved)?;
                fixture.replace_metadata(
                    id,
                    (!binary).then_some(json.as_str()),
                    binary.then_some(packed.as_slice()),
                )?;
                let outcome = fixture.ingest(&fixture.conversation, batched)?;
                assert_eq!(outcome.inserted_indices.len(), usize::from(append));
                let mut expected = preserved;
                expected["cass"]["codex_prefix_required"] = serde_json::Value::Bool(true);
                assert_eq!(fixture.metadata(id)?, expected);
                assert!(fixture.metadata(id)?.pointer("/cass/codex_stable_prefix").is_none(),
                    "a protocol marker must not fabricate a source publication receipt");
                let rows = fixture.rows(id)?;
                let metadata = fixture.metadata_columns(id)?;
                let receipts = fixture.storage.source_ingest_ledger_entries()?;
                let error = fixture.ingest(&fixture.ordinary_append(), batched)
                    .err().context("legacy upgrade lost its protocol requirement")?;
                assert!(error.to_string().contains("requires prefix admission"));
                assert_eq!(fixture.rows(id)?, rows);
                assert_eq!(fixture.metadata_columns(id)?, metadata);
                assert_eq!(fixture.storage.source_ingest_ledger_entries()?, receipts);
                assert!(fixture.ingest(&fixture.conversation, batched)?.inserted_indices.is_empty());
            }
        }
    }
    Ok(())
}

#[test]
fn codex_prefix_requirement_rolls_back_with_later_batch_refusal() -> Result<()> {
    let fixture = Fixture::new(3, false, true)?;
    let id = fixture.seed(&fixture.legacy(1))?;
    let rows = fixture.rows(id)?;
    let metadata = fixture.metadata_columns(id)?;
    let strict = fixture.ordinary_append();
    let error = fixture.storage.insert_conversations_batched_with_completion(
        &[
            (fixture.agent, None, &fixture.conversation),
            (fixture.agent, None, &strict),
        ],
        false,
        Some(&fixture.completion()),
    ).err().context("a later strict packet ignored the preceding prefix admission")?;
    assert!(error.to_string().contains("requires prefix admission"));
    assert_eq!(fixture.rows(id)?, rows);
    assert_eq!(fixture.metadata_columns(id)?, metadata);
    assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
    Ok(())
}

#[test]
fn codex_prefix_integrity_metadata_is_bounded_and_strictly_decoded() -> Result<()> {
    let ordinary = "{\"legacy_archive\":true}".to_owned();
    let mut at_limit = serde_json::json!({"padding": ""});
    let empty_len = at_limit.to_string().len();
    at_limit["padding"] =
        serde_json::json!("x".repeat(MAX_SOURCE_PREFIX_METADATA_BYTES - empty_len));
    let at_limit = at_limit.to_string();
    assert_eq!(at_limit.len(), MAX_SOURCE_PREFIX_METADATA_BYTES);
    let cases = [
        ("json-invalid", Some("SECRET_INVALID_METADATA".to_owned()), None),
        ("bin-invalid", Some(ordinary), Some(vec![0xc1])),
        ("json-large", Some("{".repeat(MAX_SOURCE_PREFIX_METADATA_BYTES + 1)), None),
        ("bin-large", None, Some(vec![0; MAX_SOURCE_PREFIX_METADATA_BYTES + 1])),
        ("output-large", Some(at_limit), None),
        ("root-array", Some("[]".to_owned()), None),
    ];
    for (name, json, binary) in cases {
        let fixture = Fixture::new(3, false, true)?;
        let id = fixture.seed(&fixture.legacy(2))?;
        fixture.replace_metadata(id, json.as_deref(), binary.as_deref())?;
        let rows = fixture.rows(id)?;
        let metadata = fixture.metadata_columns(id)?;
        let error = fixture.ingest(&fixture.conversation, true)
            .err().with_context(|| format!("{name}: unsafe metadata was replaced"))?;
        let message = error.to_string();
        match name {
            "json-invalid" => assert!(message.contains("JSON metadata")),
            "bin-invalid" => assert!(message.contains("MessagePack metadata")),
            "root-array" => assert!(message.contains("non-object")),
            _ => assert!(message.contains("byte budget"), "{name}: {error:#}"),
        }
        assert!(!format!("{error:#}").contains("SECRET_INVALID_METADATA"));
        assert_eq!(fixture.rows(id)?, rows, "{name}");
        assert_eq!(fixture.metadata_columns(id)?, metadata, "{name}");
        assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
    }
    Ok(())
}

#[test]
fn codex_prefix_regular_to_explicit_symlink_cannot_downgrade_canonical_admission() -> Result<()> {
    use std::os::unix::fs::symlink;
    use franken_agent_detection::connectors::{SourceCompletion, SourceScanHooks};

    for legacy_upgrade in [false, true] {
        for batched in [false, true] {
            let fixture = Fixture::new(2, false, true)?;
            let id = if legacy_upgrade {
                let id = fixture.seed(&fixture.legacy(1))?;
                fixture.ingest(&fixture.conversation, batched)?;
                id
            } else {
                fixture.seed(&fixture.conversation)?
            };
            let source = &fixture.conversation.source_path;
            let original_key_path = fs::canonicalize(source)?;
            let original = fs::read_to_string(source)?;
            let retained = source.with_extension("retained");
            let replacement = source.with_file_name("symlink-target.jsonl");
            fs::rename(source, &retained)?;
            let appended = serde_json::json!({
                "type": "response_item", "timestamp": "2026-11-23T12:00:02Z",
                "payload": {"role": "user", "content": "new strict suffix"}
            });
            fs::write(&replacement, format!("{}{appended}\n",
                original.replace("canonical prefix message 0", "changed first source message")))?;
            fs::File::open(&replacement)?.set_modified(
                SystemTime::now() - Duration::from_secs(7200)
            )?;
            symlink(&replacement, source)?;
            assert_ne!(fs::canonicalize(source)?, original_key_path,
                "the canonical-path ledger key must change in this regression");
            let ctx = ScanContext::with_roots(
                fixture._directory.path().join("cass"),
                vec![ScanRoot::local(source.clone())],
                None,
            );
            let mut should_scan = |observed: &crate::connectors::DiscoveredSourceFile| {
                // Simulate the actual changed-key cache miss. Core strict
                // parsing deliberately retains support for explicit symlinks.
                crate::connectors::codex::set_previous_prefix_receipt(&observed.source_path, None)
                    .unwrap();
                true
            };
            let mut completions = 0;
            let mut complete = |done: &SourceCompletion| {
                assert!(crate::connectors::codex::current_prefix_receipt(&done.source.source_path).is_none());
                completions += 1;
                Ok(())
            };
            let mut delivered = Vec::new();
            crate::connectors::codex::CodexConnector::new().scan_with_source_boundaries(
                &ctx,
                &mut SourceScanHooks {
                    should_scan_source: Some(&mut should_scan),
                    on_source_complete: Some(&mut complete),
                },
                &mut |conversation| {
                    delivered.push(conversation);
                    Ok(())
                },
            )?;
            assert_eq!(completions, 1);
            assert_eq!(delivered.len(), 1);
            let strict = crate::indexer::persist::map_to_internal(&delivered.remove(0));
            assert!(!source_prefix_packet(&strict));
            assert_eq!(strict.external_id, fixture.conversation.external_id);
            assert_eq!(strict.source_path, *source);
            assert_eq!(strict.messages.len(), 3);
            assert_eq!(strict.messages[0].content, "changed first source message");
            let rows = fixture.rows(id)?;
            let metadata = fixture.metadata_columns(id)?;
            let receipts = fixture.storage.source_ingest_ledger_entries()?;
            let error = fixture.ingest(&strict, batched).err().context("symlink downgrade merged")?;
            assert!(error.to_string().contains("requires prefix admission"));
            assert_eq!(fixture.rows(id)?, rows);
            assert_eq!(fixture.metadata_columns(id)?, metadata);
            assert_eq!(fixture.storage.source_ingest_ledger_entries()?, receipts);
            assert_eq!(fs::read_to_string(&retained)?, original);
        }
    }
    Ok(())
}

#[path = "codex_prefix_tests/preflight_tests.rs"]
mod preflight_tests;
