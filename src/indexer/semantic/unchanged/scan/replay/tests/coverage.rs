//! Coverage is collected from the same real rows that supply identity replay.
//! Compare against the prior indexed SQL algorithm, not ingestion-tail hints.

use super::*;
use crate::indexer::semantic::unchanged::scan::{compare_with_coverage, nonempty_conversations};
use crate::indexer::semantic::{SemanticBackfillStoragePlan, SemanticIndexer};
use crate::search::semantic_manifest::{SemanticManifest, TierKind};
use std::collections::HashSet;
use std::fs;

#[test]
fn fused_coverage_counts_blank_parents_not_passages_or_stale_tails() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = storage(temp.path(), PAGE_SIZE + 3)?;
    let parents = ids(&storage)?;
    storage.raw().execute_compat(
        "UPDATE messages SET content = '', role = 'system' WHERE conversation_id = ?1",
        &[ParamValue::from(parents[0])],
    )?;
    storage.raw().execute_compat(
        "DELETE FROM messages WHERE conversation_id IN (?1, ?2)",
        &[ParamValue::from(parents[1]), ParamValue::from(*parents.last().unwrap())],
    )?;
    storage.raw().execute_compat(
        "UPDATE messages SET content = ?1 WHERE conversation_id = ?2",
        &[ParamValue::from("compiler checkpoint Unicode 東京 ".repeat(400)), ParamValue::from(parents[2])],
    )?;
    let mut loaded = Vec::new();
    let mut inputs = Vec::new();
    let coverage = visit_with_loader(
        &storage,
        |id| { loaded.push(id); messages(&storage, id) },
        |input| { inputs.push(input); Ok(()) },
        || Ok(()),
    )?;
    assert_eq!(loaded, parents, "no body may be loaded twice to collect coverage");
    assert_eq!(coverage, ((parents.len() - 2) as u64, parents[parents.len() - 2]));
    assert_eq!(coverage, nonempty_conversations(storage.raw(), || Ok(()))?);
    assert!(inputs.len() > coverage.0 as usize, "passages are not a parent count");
    storage.close()?;
    Ok(())
}

#[test]
fn fused_coverage_matches_empty_negative_and_maximum_parent_boundaries() -> Result<()> {
    for parents in [vec![], vec![-7, -1], vec![i64::MIN, 0, i64::MAX]] {
        let temp = tempfile::tempdir()?;
        let storage = storage(temp.path(), 0)?;
        let agent: i64 = storage.raw().query_row_map(
            "SELECT id FROM agents WHERE slug = 'codex'",
            &[] as &[ParamValue], |row| row.get_typed(0),
        )?;
        storage.raw().execute("BEGIN IMMEDIATE")?;
        for (ordinal, &id) in parents.iter().enumerate() {
            storage.raw().execute_compat(
                "INSERT INTO conversations(id, agent_id, source_id, external_id, source_path)
                 VALUES (?1, ?2, 'local', ?3, ?4)",
                &[ParamValue::from(id), ParamValue::from(agent),
                    ParamValue::from(format!("coverage-{ordinal}")),
                    ParamValue::from(format!("/coverage/{ordinal}.jsonl"))],
            )?;
            storage.raw().execute_compat(
                "INSERT INTO messages(conversation_id, idx, role, content, created_at)
                 VALUES (?1, 0, 'user', 'canonical coverage', 1)",
                &[ParamValue::from(id)],
            )?;
        }
        storage.raw().execute("COMMIT")?;
        let coverage = visit(&storage, |_| Ok(()), || Ok(()))?;
        assert_eq!(coverage, (parents.len() as u64, parents.last().copied().unwrap_or(0)));
        assert_eq!(coverage, nonempty_conversations(storage.raw(), || Ok(()))?);
        storage.close()?;
    }
    Ok(())
}

#[test]
fn only_complete_exact_identity_replay_returns_coverage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let storage = storage(temp.path(), 3)?;
    let canonical: Vec<_> = engine::packet_embedding_inputs_from_storage(&storage)?
        .iter().filter_map(engine::semantic_doc_id_for_input).collect();
    assert_eq!(canonical.len(), 3);
    let sink = SemanticProgressSink::disabled();
    for mode in 0..3 {
        let mut remaining: HashSet<&str> = canonical.iter().map(String::as_str).collect();
        match mode {
            1 => { assert!(remaining.remove(canonical[0].as_str())); }
            2 => { remaining.insert("retained-document-absent-from-canonical-archive"); }
            _ => {}
        }
        let (comparison, coverage) = compare_with_coverage(
            &storage, &mut remaining, &sink, canonical.len() as u64, || Ok(()),
        )?;
        assert_eq!(comparison.matches, mode == 0);
        assert_eq!(coverage, if mode == 0 { Some((3, 3)) } else { None });
        assert_eq!(comparison.visited, if mode == 1 { 1 } else { 3 });
    }
    storage.close()?;
    Ok(())
}

#[test]
fn unchanged_document_ids_cannot_hide_changed_conversation_coverage() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let storage = storage(data, 3)?;
    let plan = SemanticBackfillStoragePlan {
        tier: TierKind::Quality,
        db_fingerprint: crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?,
        model_revision: "hash".into(),
        max_conversations: 16,
    };
    let mut manifest = SemanticManifest::default();
    let built = SemanticIndexer::new("hash", None)?.run_backfill_from_storage(
        &storage, data, &mut manifest, plan.clone(),
    )?;
    assert!(built.published);
    let canonical: HashSet<String> = engine::packet_embedding_inputs_from_storage(&storage)?
        .iter().filter_map(engine::semantic_doc_id_for_input).collect();
    let vectors = fs::read(&built.index_path)?;
    let ledger = fs::read(SemanticManifest::path(data))?;
    let receipt = data.join(crate::search::vector_index::VECTOR_INDEX_DIR)
        .join(".completed-backfill-quality-fnv1a-384.json");
    fs::write(&receipt, b"retained invalid receipt")?;
    // Document IDs omit the parent ID. Keep message/filter/content identity
    // intact while making one parent empty; its ingest-tail cache stays stale.
    storage.raw().execute(
        "UPDATE messages SET conversation_id = 2, idx = 1 WHERE conversation_id = 1",
    )?;
    assert_eq!(
        crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?,
        plan.db_fingerprint,
    );
    let mut remaining = canonical.iter().map(String::as_str).collect();
    let (comparison, coverage) = compare_with_coverage(
        &storage, &mut remaining, &SemanticProgressSink::disabled(), 3, || Ok(()),
    )?;
    assert!(comparison.matches, "this negative must preserve every document ID");
    assert_eq!(coverage, Some((2, 3)));
    let inner = engine::SemanticIndexer::new("hash", None)?;
    let outcome = crate::indexer::semantic::unchanged::prove_unchanged(
        &inner, &storage, data, &manifest, &plan,
        &SemanticProgressSink::disabled(), || Ok(()),
    )?;
    assert!(outcome.is_none(), "matching vectors cannot certify a stale parent count");
    assert_eq!(fs::read(&built.index_path)?, vectors);
    assert_eq!(fs::read(SemanticManifest::path(data))?, ledger);
    assert_eq!(fs::read(&receipt)?, b"retained invalid receipt");
    storage.close()?;
    Ok(())
}
