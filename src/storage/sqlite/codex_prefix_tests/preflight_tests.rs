//! Watch admission probes must classify conflicts without canonical writes.
use super::*;

type Snapshot = Vec<(&'static str, Vec<String>)>;

fn persisted_snapshot(storage: &FrankenStorage) -> Result<Snapshot> {
    [
        "conversations",
        "messages",
        "meta",
        "conversation_external_tail_lookup",
        "agents",
        "sources",
    ]
    .into_iter()
    .map(|table| {
        let mut rows: Vec<_> = storage
            .raw()
            .query(&format!("SELECT * FROM {table}"))?
            .into_iter()
            .map(|row| format!("{:?}", row.values()))
            .collect();
        rows.sort();
        Ok((table, rows))
    })
    .collect()
}

fn read_only_probe(
    fixture: &Fixture,
    conversation: &Conversation,
) -> Result<Option<CodexCanonicalRefusal>> {
    let before = persisted_snapshot(&fixture.storage)?;
    fixture.storage.raw().execute("PRAGMA query_only = 1")?;
    let result = fixture
        .storage
        .codex_canonical_preflight_refusal(conversation);
    // Starting another transaction proves even an error/refusal ended the
    // probe's snapshot instead of leaving cleanup to an ignored Drop error.
    let mut next = fixture.storage.raw().transaction()?;
    next.rollback()?;
    let after = persisted_snapshot(&fixture.storage)?;
    fixture.storage.raw().execute("PRAGMA query_only = 0")?;
    assert_eq!(after, before, "preflight changed persisted state");
    result
}

#[test]
fn codex_preflight_preserves_legacy_metadata_and_missing_derived_lookup() -> Result<()> {
    for batched in [false, true] {
        for append in [false, true] {
            for missing_lookup in [false, true] {
                let fixture = Fixture::new(3, false, true)?;
                let id = fixture.seed(&fixture.legacy(if append { 2 } else { 3 }))?;
                if missing_lookup {
                    fixture.storage.raw().execute(
                        "DELETE FROM conversation_external_tail_lookup",
                    )?;
                }
                let metadata = fixture.metadata_columns(id)?;
                assert!(read_only_probe(&fixture, &fixture.conversation)?.is_none());
                assert_eq!(fixture.metadata_columns(id)?, metadata);
                assert!(
                    fixture.metadata(id)?.pointer("/cass/codex_prefix_required").is_none(),
                    "a read-only compatible result cannot perform a legacy upgrade"
                );
                let lookup_count: i64 = fixture.storage.raw().query_row_map(
                    "SELECT COUNT(*) FROM conversation_external_tail_lookup",
                    fparams![],
                    |row| row.get_typed(0),
                )?;
                assert_eq!(lookup_count, i64::from(!missing_lookup));
                assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());

                // Only the existing authoritative writer may publish the
                // prospective protocol marker and any completion receipt.
                let written = fixture.ingest(&fixture.conversation, batched)?;
                assert_eq!(written.conversation_id, id);
                assert_eq!(written.inserted_indices.len(), usize::from(append));
                assert_eq!(
                    fixture.metadata(id)?.pointer("/cass/codex_prefix_required"),
                    Some(&serde_json::Value::Bool(true))
                );
                assert_eq!(
                    fixture.storage.source_ingest_ledger_entries()?.len(),
                    usize::from(batched)
                );
            }
        }
    }
    Ok(())
}

#[test]
fn codex_preflight_returns_only_typed_exact_history_refusals() -> Result<()> {
    for case in ["projection", "boundary", "protocol"] {
        let fixture = Fixture::new(3, false, true)?;
        let (id, incoming) = match case {
            "projection" => {
                let mut legacy = fixture.legacy(2);
                legacy.messages[0].content = "retained canonical variant".to_owned();
                (fixture.seed(&legacy)?, fixture.conversation.clone())
            }
            "boundary" => {
                let mut legacy = fixture.legacy(3);
                let mut extra = legacy.messages[2].clone();
                extra.idx = 3;
                extra.content = "retained row beyond the parsed boundary".to_owned();
                legacy.messages.push(extra);
                (fixture.seed(&legacy)?, fixture.conversation.clone())
            }
            "protocol" => (
                fixture.seed(&fixture.conversation)?,
                fixture.legacy(3),
            ),
            _ => unreachable!(),
        };
        // Known legacy conflicts must also be discoverable without the
        // derived cache; the probe must not populate it as a side effect.
        fixture.storage.raw().execute("DELETE FROM conversation_external_tail_lookup")?;
        let refusal = read_only_probe(&fixture, &incoming)?
            .with_context(|| format!("{case}: expected a known canonical refusal"))?;
        assert_eq!(refusal.provider, StablePrefixProvider::Codex);
        assert_eq!(refusal.conversation_id, id);
        assert_eq!(refusal.source_path, incoming.source_path);
        assert_eq!(refusal.external_id, incoming.external_id);
        match case {
            "projection" => assert!(matches!(
                refusal.reason, CodexCanonicalRefusalReason::ProjectionMismatch(0)
            )),
            "boundary" => assert!(matches!(
                refusal.reason, CodexCanonicalRefusalReason::HistoryBeyondPrefix
            )),
            "protocol" => assert!(matches!(
                refusal.reason, CodexCanonicalRefusalReason::PrefixRequired
            )),
            _ => unreachable!(),
        }
    }
    Ok(())
}

#[test]
fn codex_preflight_uses_exact_external_identity_and_allows_path_aliases() -> Result<()> {
    let fixture = Fixture::new(3, false, true)?;
    let id = fixture.seed(&fixture.legacy(2))?;
    let mut alias = fixture.conversation.clone();
    alias.source_path = fixture._directory.path().join("same-session-alias.jsonl");
    alias.metadata_json["cass"]["codex_stable_prefix"]["source_path"] =
        serde_json::to_value(&alias.source_path)?;
    assert!(read_only_probe(&fixture, &alias)?.is_none());
    let append = fixture.ingest(&alias, true)?;
    assert_eq!(append.conversation_id, id);
    assert_eq!(append.inserted_indices, [2]);

    // A genuinely differing retained message is a known refusal only for
    // this source+agent+external identity, even if another packet shares path.
    fixture.storage.raw().execute_compat(
        "UPDATE messages SET content = 'retained differing row'
         WHERE conversation_id = ?1 AND idx = 0",
        fparams![id],
    )?;
    let refusal = read_only_probe(&fixture, &alias)?.context("alias lost exact identity")?;
    assert_eq!(refusal.conversation_id, id);
    assert_eq!(refusal.source_path, alias.source_path);
    for field in ["source", "agent", "external", "missing-external"] {
        let mut other = fixture.conversation.clone();
        match field {
            "source" => {
                other.source_id = "remote-preflight-fixture".to_owned();
                other.origin_host = Some("remote-preflight-fixture".to_owned());
                other.metadata_json["cass"]["origin"] =
                    serde_json::json!({"kind": "ssh", "host": "remote-preflight-fixture"});
                assert_ne!(
                    normalized_conversation_for_storage(&other).source_id,
                    fixture.conversation.source_id
                );
            }
            "agent" => other.agent_slug = "claude_code".to_owned(),
            "external" => other.external_id = Some("different-session".to_owned()),
            "missing-external" => other.external_id = None,
            _ => unreachable!(),
        }
        assert!(read_only_probe(&fixture, &other)?.is_none(), "{field}");
    }
    Ok(())
}

#[test]
fn codex_preflight_preserves_ordinary_packets_and_rejects_malformed_unknown_prefixes() -> Result<()> {
    let fixture = Fixture::new(3, false, true)?;
    fixture.seed(&fixture.legacy(2))?;
    assert!(read_only_probe(&fixture, &fixture.legacy(3))?.is_none());
    for case in [
        "proof", "schema", "provider", "path", "ambiguous-providers",
        "repeated-index", "outside-index",
    ] {
        for unknown_identity in [false, true] {
            let mut malformed = fixture.conversation.clone();
            if unknown_identity {
                malformed.external_id = None;
            }
            match case {
                "proof" => {
                    malformed.metadata_json["cass"]["codex_stable_prefix"] =
                        serde_json::json!("not a receipt");
                }
                "schema" => {
                    malformed.metadata_json["cass"]["codex_stable_prefix"]["schema_version"] =
                        serde_json::json!(2);
                }
                "provider" => {
                    malformed.metadata_json["cass"]["codex_stable_prefix"]["provider"] =
                        serde_json::json!("claude_code");
                }
                "path" => {
                    malformed.metadata_json["cass"]["codex_stable_prefix"]["source_path"] =
                        serde_json::json!("/foreign/prefix-source.jsonl");
                }
                "ambiguous-providers" => {
                    let mut foreign =
                        malformed.metadata_json["cass"]["codex_stable_prefix"].clone();
                    foreign["provider"] = serde_json::json!("claude_code");
                    malformed.metadata_json["cass"]["claude_stable_prefix"] = foreign;
                }
                "repeated-index" => malformed.messages[1].idx = malformed.messages[0].idx,
                "outside-index" => malformed.messages[2].idx = 3,
                _ => unreachable!(),
            }
            let error = read_only_probe(&fixture, &malformed)
                .err().context("malformed prefix became a no-known-conflict result")?;
            assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_none(), "{case}");
        }
    }
    Ok(())
}

#[test]
fn codex_preflight_metadata_checks_keep_binary_precedence_and_output_budget() -> Result<()> {
    let mut at_limit = serde_json::json!({"padding": ""});
    let overhead = at_limit.to_string().len();
    at_limit["padding"] =
        serde_json::json!("x".repeat(MAX_SOURCE_PREFIX_METADATA_BYTES - overhead));
    let at_limit = at_limit.to_string();
    assert_eq!(at_limit.len(), MAX_SOURCE_PREFIX_METADATA_BYTES);
    let cases = [
        ("json-invalid", Some("INVALID_CANONICAL_JSON".to_owned()), None),
        (
            "binary-invalid",
            Some("{\"legacy_archive\":true}".to_owned()),
            Some(vec![0xc1]),
        ),
        (
            "combined-oversize",
            Some(" ".repeat(MAX_SOURCE_PREFIX_METADATA_BYTES / 2)),
            Some(vec![0; MAX_SOURCE_PREFIX_METADATA_BYTES / 2 + 1]),
        ),
        ("root-array", Some("[]".to_owned()), None),
        ("output-budget", Some(at_limit), None),
    ];
    for (case, json, binary) in cases {
        for divergent in [false, true] {
            let fixture = Fixture::new(3, false, true)?;
            let mut legacy = fixture.legacy(2);
            if divergent {
                legacy.messages[0].content = "also differs from the incoming projection".to_owned();
            }
            let id = fixture.seed(&legacy)?;
            fixture.replace_metadata(id, json.as_deref(), binary.as_deref())?;
            let error = read_only_probe(&fixture, &fixture.conversation)
                .err().with_context(|| format!("{case}: invalid metadata became compatible"))?;
            assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_none(), "{case}");
            let expected = match case {
                "json-invalid" => "JSON metadata",
                "binary-invalid" => "MessagePack metadata",
                "root-array" => "non-object",
                _ => "byte budget",
            };
            assert!(error.to_string().contains(expected), "{case}: {error:#}");
        }
    }
    // Valid binary metadata remains authoritative even when JSON disagrees.
    let fixture = Fixture::new(3, false, true)?;
    let id = fixture.seed(&fixture.legacy(2))?;
    let binary = rmp_serde::to_vec(&serde_json::json!({"legacy_archive": true}))?;
    fixture.replace_metadata(id, Some("INVALID_CANONICAL_JSON"), Some(&binary))?;
    assert!(read_only_probe(&fixture, &fixture.conversation)?.is_none());
    Ok(())
}

#[test]
fn codex_preflight_success_cannot_bypass_a_later_canonical_conflict() -> Result<()> {
    for batched in [false, true] {
        let fixture = Fixture::new(3, false, true)?;
        let id = fixture.seed(&fixture.legacy(2))?;
        assert!(read_only_probe(&fixture, &fixture.conversation)?.is_none());
        fixture.storage.raw().execute_compat(
            "UPDATE messages SET content = 'changed after the read-only snapshot'
             WHERE conversation_id = ?1 AND idx = 0",
            fparams![id],
        )?;
        let before = persisted_snapshot(&fixture.storage)?;
        let error = fixture.ingest(&fixture.conversation, batched)
            .err().context("an earlier compatible probe bypassed the write guard")?;
        assert!(matches!(
            error.downcast_ref::<CodexCanonicalRefusal>().map(|refusal| &refusal.reason),
            Some(CodexCanonicalRefusalReason::ProjectionMismatch(0))
        ));
        assert_eq!(persisted_snapshot(&fixture.storage)?, before);
        assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
    }
    Ok(())
}

#[test]
fn codex_preflight_and_writers_reject_a_raced_foreign_derived_lookup() -> Result<()> {
    for provider in [StablePrefixProvider::Codex, StablePrefixProvider::ClaudeCode] {
        for batched in [false, true] {
            for foreign_dimension in ["source", "agent", "external", "stale-external"] {
                let fixture = Fixture::for_provider(3, false, true, provider)?;
                let id = fixture.seed(&fixture.legacy(2))?;
                let mut foreign = fixture.legacy(2);
                foreign.source_path = fixture._directory.path().join("foreign-session.jsonl");
                foreign.messages[0].content = "foreign retained canonical data".to_owned();
                let mut foreign_agent = fixture.agent;
                match foreign_dimension {
                    "source" => {
                        foreign.source_id = "remote-foreign-fixture".to_owned();
                        foreign.origin_host = Some("remote-foreign-fixture".to_owned());
                    }
                    "agent" => {
                        let foreign_slug = match provider {
                            StablePrefixProvider::Codex => "claude_code",
                            StablePrefixProvider::ClaudeCode => "codex",
                        };
                        foreign.agent_slug = foreign_slug.to_owned();
                        foreign_agent = fixture.storage.ensure_agent(&Agent {
                            id: None,
                            slug: foreign_slug.to_owned(),
                            name: foreign_slug.to_owned(),
                            version: None,
                            kind: AgentKind::Cli,
                        })?;
                    }
                    "external" | "stale-external" => {
                        foreign.external_id = Some("foreign-external".to_owned());
                    }
                    _ => unreachable!(),
                }
                let foreign_id = fixture.storage.insert_conversation_tree_with_analytics(
                    foreign_agent, None, &foreign, false,
                )?.conversation_id;
                assert_ne!(foreign_id, id);
                assert!(read_only_probe(&fixture, &fixture.conversation)?.is_none());
                let lookup_key = conversation_external_lookup_key(
                    &fixture.conversation.source_id,
                    fixture.agent,
                    fixture.conversation.external_id.as_deref().unwrap(),
                );
                if foreign_dimension == "stale-external" {
                    fixture.storage.raw().execute_compat(
                        "UPDATE conversations SET external_id = 'rekeyed-session' WHERE id = ?1",
                        fparams![id],
                    )?;
                } else {
                    fixture.storage.raw().execute_compat(
                        "UPDATE conversation_external_tail_lookup
                         SET conversation_id = ?1 WHERE lookup_key = ?2",
                        fparams![foreign_id, lookup_key.as_str()],
                    )?;
                }
                let before = persisted_snapshot(&fixture.storage)?;
                // The last preflight succeeded. The actual writer must still
                // reject an ID that no longer represents the exact canonical key.
                let error = fixture.ingest(&fixture.conversation, batched)
                    .err().context("a stale preflight admitted a foreign canonical target")?;
                assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_none());
                assert!(error.to_string().contains("exact external identity"), "{error:#}");
                assert_eq!(persisted_snapshot(&fixture.storage)?, before);

                if provider == StablePrefixProvider::Codex {
                    let error = read_only_probe(&fixture, &fixture.conversation)
                        .err().context("a misbound cache became an isolatable history conflict")?;
                    assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_none());
                    assert!(error.to_string().contains("exact canonical identity"), "{error:#}");
                } else {
                    // This Codex-only probe makes no conclusion for Claude,
                    // including after its generalized write guard rejected.
                    assert!(read_only_probe(&fixture, &fixture.conversation)?.is_none());
                }
                assert!(fixture.storage.source_ingest_ledger_entries()?.is_empty());
            }
        }
    }
    Ok(())
}

#[test]
fn codex_preflight_query_failure_is_terminal_and_ends_the_snapshot() -> Result<()> {
    let fixture = Fixture::new(3, false, true)?;
    fixture.seed(&fixture.legacy(2))?;
    fixture.storage.raw().execute(
        "ALTER TABLE conversation_external_tail_lookup RENAME TO preflight_missing_lookup",
    )?;
    let error = fixture.storage.codex_canonical_preflight_refusal(&fixture.conversation)
        .err().context("a missing canonical lookup table was silently ignored")?;
    assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_none());
    let mut next = fixture.storage.raw().transaction()?;
    next.rollback()?;
    fixture.storage.raw().execute(
        "ALTER TABLE preflight_missing_lookup RENAME TO conversation_external_tail_lookup",
    )?;
    assert!(read_only_probe(&fixture, &fixture.conversation)?.is_none());
    Ok(())
}

#[test]
fn codex_preflight_preserves_foreign_provider_error_precedence() -> Result<()> {
    for batched in [false, true] {
        for prefix in [false, true] {
            let fixture = Fixture::new(3, false, true)?;
            let mut legacy = fixture.legacy(2);
            legacy.messages[0].content = "also conflicts with the source projection".to_owned();
            let id = fixture.seed(&legacy)?;
            fixture.replace_metadata(
                id,
                Some(r#"{"cass":{"codex_prefix_required":true,"claude_prefix_required":true}}"#),
                None,
            )?;
            let incoming = if prefix {
                fixture.conversation.clone()
            } else {
                fixture.legacy(3)
            };
            let error = read_only_probe(&fixture, &incoming)
                .err().context("foreign canonical provider became a known refusal")?;
            assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_none());
            assert!(error.to_string().contains("prefix provider differs"), "{error:#}");
            let before = persisted_snapshot(&fixture.storage)?;
            let error = fixture.ingest(&incoming, batched)
                .err().context("writer accepted another provider's canonical requirement")?;
            assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_none());
            assert!(error.to_string().contains("prefix provider differs"), "{error:#}");
            assert_eq!(persisted_snapshot(&fixture.storage)?, before);
        }
    }
    Ok(())
}

#[test]
fn codex_preflight_does_not_certify_claude_or_weaken_its_write_guard() -> Result<()> {
    for batched in [false, true] {
        let fixture = Fixture::for_provider(3, false, true, StablePrefixProvider::ClaudeCode)?;
        let id = fixture.seed(&fixture.legacy(2))?;
        let mut alias = fixture.conversation.clone();
        alias.source_path = fixture._directory.path().join("same-claude-session-alias.jsonl");
        alias.metadata_json["cass"]["claude_stable_prefix"]["source_path"] =
            serde_json::to_value(&alias.source_path)?;
        assert!(read_only_probe(&fixture, &alias)?.is_none());
        let append = fixture.ingest(&alias, batched)?;
        assert_eq!(append.conversation_id, id);
        assert_eq!(append.inserted_indices, [2]);
        let metadata = fixture.metadata(id)?;
        assert_eq!(
            metadata.pointer("/cass/claude_prefix_required"),
            Some(&serde_json::Value::Bool(true))
        );
        assert!(metadata.pointer("/cass/codex_prefix_required").is_none());

        for case in ["divergent", "ordinary", "substituted", "unsupported-agent"] {
            let mut incoming = alias.clone();
            match case {
                "divergent" => {
                    incoming.messages[0].content = "a different Claude canonical turn".to_owned();
                }
                "ordinary" => incoming = fixture.legacy(3),
                "substituted" => {
                    incoming.metadata_json["cass"]["claude_stable_prefix"]["provider"] =
                        serde_json::json!("codex");
                }
                "unsupported-agent" => incoming.agent_slug = "unsupported-prefix-agent".to_owned(),
                _ => unreachable!(),
            }
            // A Codex-scoped probe must not claim authority for Claude or
            // another agent, even when the authoritative writer will reject.
            assert!(read_only_probe(&fixture, &incoming)?.is_none(), "{case}");
            let before = persisted_snapshot(&fixture.storage)?;
            let error = fixture.ingest(&incoming, batched)
                .err().with_context(|| format!("{case}: Claude write guard was bypassed"))?;
            if case == "divergent" || case == "ordinary" {
                let refusal = error.downcast_ref::<CodexCanonicalRefusal>()
                    .context("Claude canonical refusal lost its provider")?;
                assert_eq!(refusal.provider, StablePrefixProvider::ClaudeCode);
                assert_eq!(refusal.conversation_id, id);
                assert_eq!(refusal.source_path, incoming.source_path);
                match case {
                    "divergent" => assert!(matches!(
                        refusal.reason, CodexCanonicalRefusalReason::ProjectionMismatch(0)
                    )),
                    "ordinary" => assert!(matches!(
                        refusal.reason, CodexCanonicalRefusalReason::PrefixRequired
                    )),
                    _ => unreachable!(),
                }
            } else {
                assert!(error.downcast_ref::<CodexCanonicalRefusal>().is_none(), "{case}");
            }
            assert_eq!(persisted_snapshot(&fixture.storage)?, before, "{case}");
        }
    }
    Ok(())
}
