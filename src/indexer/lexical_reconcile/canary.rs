//! Verify a precise endpoint message, not whichever conversation ranks first.

use std::collections::HashSet;
use anyhow::{Result, ensure};
use frankensearch::quill::QuillSearchIndex;
use frankensearch::quill::cass::{
    CassConversationKey, CassDerivedColumns, CassDocument, cass_document_identity, field,
};
use frankensearch::quill::query::{
    BooleanClause, CassQueryFilters, CassQueryParser, Occur, Query, QueryValue,
};
use frankensearch::quill::schema::CASS_SEMANTIC_SCHEMA;
use crate::search::quill_bridge::{search_paginated, stored_i64, stored_text, stored_u64};

const PAGE_SIZE: usize = 64;
const MAX_CANDIDATES: usize = 4096;

/// Read-only endpoint check. The reader is opened once by the caller after
/// publication and is never refreshed during verification. Missing evidence
/// returns false; engine/read errors and exhausted work budgets remain errors.
/// A text token narrows discovery, but its absence never waives verification.
pub(super) fn verify(
    reader: &QuillSearchIndex,
    document: &CassDocument,
    token: Option<&str>,
) -> Result<bool> {
    verify_with_budget(reader, document, token, MAX_CANDIDATES)
}

fn verify_with_budget(
    reader: &QuillSearchIndex,
    document: &CassDocument,
    token: Option<&str>,
    max_candidates: usize,
) -> Result<bool> {
    let mut clauses = Vec::new();
    if let Some(token) = token {
        let parser = CassQueryParser::new(CASS_SEMANTIC_SCHEMA)?;
        let parsed = parser.parse(token, &CassQueryFilters::default());
        clauses.push(BooleanClause::new(Occur::Must, parsed.query));
    }
    // Numeric output, short code and short non-Latin words may have no canary
    // token. They still project to real documents. Discover those candidates
    // through the indexed source/agent/workspace keywords, then apply exactly
    // the same identity and stored-column checks as for a lexical canary.
    for (field, value) in [
        (field::SOURCE_ID, QueryValue::Str(document.source_id.clone())),
        (field::AGENT, QueryValue::Str(document.agent.clone())),
    ] {
        clauses.push(BooleanClause::new(Occur::Must, Query::set(field, vec![value])));
    }
    if let Some(workspace) = &document.workspace {
        clauses.push(BooleanClause::new(Occur::Must,
            Query::set(field::WORKSPACE, vec![QueryValue::Str(workspace.clone())])));
    }
    // Keep candidate discovery on text/keyword postings. The pinned Quill
    // numeric scorer uses the physical rather than live segment domain after
    // an upsert tombstones a sealed row (frankensearch#49). A numeric msg_idx
    // or timestamp prefilter therefore breaks the very replay we must verify.
    // These prefilters are redundant: the opaque identity and exact stored
    // columns below remain authoritative, including absent optional values.
    // Do not catch query errors, weaken the scorer invariant, or clear the
    // recovery checkpoint when this bounded verification cannot finish.
    // conversation_id and source_path are STORED ONLY. They cannot be used as
    // indexed predicates. Verify the complete opaque identity and those stored
    // columns below; bounded pagination prevents unrelated equal-score rows
    // from turning the first ranked page into a false absence verdict.
    let query = Query::boolean(clauses, None);
    let identity = cass_document_identity(&document.source_id,
        CassConversationKey::for_document(document.as_ref()), document.msg_idx);
    let preview = CassDerivedColumns::derive(document.as_ref()).preview;
    let mut offset = 0;
    let mut seen = HashSet::new();
    loop {
        ensure!(offset < max_candidates,
            "lexical reconcile canary exhausted its candidate budget; checkpoint retained");
        let limit = PAGE_SIZE.min(max_candidates - offset);
        let page = search_paginated(reader, &query, limit, offset, false)?;
        ensure!(page.hits.len() <= limit, "canary backend exceeded its page limit");
        for hit in &page.hits {
            ensure!(hit.bm25_score.is_finite() && seen.insert(hit.global_docid),
                "canary backend returned an invalid or repeated candidate");
            if hit.document_id != identity { continue; }
            // A match elsewhere in the conversation cannot certify this exact
            // prefix/tail message. Stored-content preview is an endpoint check,
            // not a claim of a full engine content-witness comparison.
            return Ok(
                stored_i64(reader, field::CONVERSATION_ID, hit.global_docid)? == document.conversation_id
                && stored_text(reader, field::SOURCE_PATH, hit.global_docid)?.as_deref() == Some(document.source_path.as_str())
                && stored_text(reader, field::SOURCE_ID, hit.global_docid)?.as_deref() == Some(document.source_id.as_str())
                && stored_u64(reader, field::MSG_IDX, hit.global_docid)? == Some(document.msg_idx)
                && stored_i64(reader, field::CREATED_AT, hit.global_docid)? == document.created_at
                && stored_text(reader, field::AGENT, hit.global_docid)?.as_deref() == Some(document.agent.as_str())
                && stored_text(reader, field::WORKSPACE, hit.global_docid)?.as_deref() == document.workspace.as_deref()
                && stored_text(reader, field::PREVIEW, hit.global_docid)?.as_deref() == Some(preview.as_str())
            );
        }
        if page.hits.len() < limit { return Ok(false); }
        offset += page.hits.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::tantivy::TantivyIndex;

    fn document(id: i64, path: &str, content: &str) -> CassDocument {
        CassDocument {
            agent: "codex".into(), workspace: Some("/work".into()), workspace_original: None,
            source_path: path.into(), msg_idx: 0, created_at: Some(1_700_000_000_000),
            title: None, content: content.into(), source_id: "local".into(),
            origin_kind: "local".into(), origin_host: None, conversation_id: Some(id),
        }
    }

    #[test]
    fn buried_endpoint_is_found_without_accepting_a_different_source() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut index = TantivyIndex::open_or_create(&temp.path().join("index"))?;
        let target = document(1000, "/target", &format!("common target {}", "padding ".repeat(512)));
        let mut docs: Vec<_> = (1..=192).map(|id| document(id, &format!("/other/{id}"), "common")).collect();
        docs.push(target.clone());
        index.add_prebuilt_documents_slice(&docs)?;
        index.commit()?;
        let reader = index.reader()?;
        let parser = CassQueryParser::new(CASS_SEMANTIC_SCHEMA)?;
        let parsed = parser.parse("common", &CassQueryFilters::default());
        let old_page = search_paginated(&reader, &parsed.query, 25, 0, false)?;
        let identity = cass_document_identity(&target.source_id,
            CassConversationKey::for_document(target.as_ref()), target.msg_idx);
        assert_eq!(old_page.hits.len(), 25);
        assert!(old_page.hits.iter().all(|hit| hit.document_id != identity),
            "fixture must reproduce the old top-25 blind spot");
        assert!(verify(&reader, &target, Some("common"))?);
        let mut foreign = target.clone(); foreign.source_path = "/wrong-source".into();
        assert!(!verify(&reader, &foreign, Some("common"))?);
        let error = verify_with_budget(&reader, &target, Some("common"), 64).unwrap_err();
        assert!(error.to_string().contains("candidate budget"));
        Ok(())
    }

    #[test]
    fn another_message_in_the_same_conversation_cannot_pass_the_endpoint_check() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut index = TantivyIndex::open_or_create(&temp.path().join("index"))?;
        let expected = document(42, "/same", "common prefix");
        let mut suffix = expected.clone(); suffix.msg_idx = 1;
        index.add_prebuilt_documents_slice(&[suffix])?;
        index.commit()?;
        let reader = index.reader()?;
        assert!(!verify(&reader, &expected, Some("common"))?);
        assert!(!verify(&reader, &expected, None)?);
        drop(reader);
        index.upsert_prebuilt_documents_slice(std::slice::from_ref(&expected))?;
        index.commit()?;
        assert!(verify(&index.reader()?, &expected, Some("common"))?);
        Ok(())
    }

    #[test]
    fn endpoint_preview_mismatch_cannot_be_hidden_by_a_matching_title_or_id() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut index = TantivyIndex::open_or_create(&temp.path().join("index"))?;
        let expected = document(42, "/same", "common wanted");
        let actual = document(42, "/same", "common different");
        index.add_prebuilt_documents_slice(&[actual])?;
        index.commit()?;
        assert!(!verify(&index.reader()?, &expected, Some("common"))?);
        Ok(())
    }

    #[test]
    fn replay_canaries_survive_sealed_tombstones_and_keep_snapshot_identity() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut index = TantivyIndex::open_or_create(&temp.path().join("index"))?;
        let original = document(42, "/same", "common original");
        let replacement = document(42, "/same", "common replacement");
        let mut sibling = document(42, "/same", "common sibling");
        sibling.msg_idx = 7;
        sibling.created_at = Some(1_700_000_000_123);
        let other = document(43, "/other", "common other");
        index.add_prebuilt_documents_slice(&[original.clone(), sibling.clone(), other.clone()])?;
        index.commit()?;
        let before = index.reader()?;

        // Replace one row in a sealed segment without compacting it. Then
        // replay the exact source twice, like production reconciliation.
        for docs in [vec![replacement.clone()], vec![replacement.clone(), sibling.clone()]] {
            index.upsert_prebuilt_documents_slice(&docs)?;
            index.commit()?;
            assert_eq!(index.doc_count()?, 3);
            let current = index.reader()?;
            assert!(verify(&current, &replacement, Some("common"))?);
            assert!(verify(&current, &sibling, Some("common"))?);
            assert!(verify(&current, &other, Some("common"))?);
            assert!(!verify(&current, &original, Some("common"))?);
        }
        // Verification may not reopen the latest generation for hydration.
        assert!(verify(&before, &original, Some("common"))?);
        assert!(!verify(&before, &replacement, Some("common"))?);
        Ok(())
    }

    #[test]
    fn numeric_and_optional_metadata_are_verified_without_numeric_prefilters() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut index = TantivyIndex::open_or_create(&temp.path().join("index"))?;
        let expected = document(42, "/same", "common wanted");
        index.add_prebuilt_documents_slice(std::slice::from_ref(&expected))?;
        index.commit()?;
        let reader = index.reader()?;
        assert!(verify(&reader, &expected, Some("common"))?);

        let mut wrong_time = expected.clone();
        wrong_time.created_at = Some(1_700_000_000_001);
        let mut absent_time = expected.clone();
        absent_time.created_at = None;
        let mut absent_workspace = expected.clone();
        absent_workspace.workspace = None;
        let mut wrong_index = expected.clone();
        wrong_index.msg_idx = 1;
        for wrong in [wrong_time, absent_time, absent_workspace, wrong_index] {
            assert!(!verify(&reader, &wrong, Some("common"))?);
        }

        let mut sparse = document(43, "/sparse", "common sparse");
        sparse.created_at = None;
        sparse.workspace = None;
        index.upsert_prebuilt_documents_slice(std::slice::from_ref(&sparse))?;
        index.commit()?;
        let reader = index.reader()?;
        assert!(verify(&reader, &sparse, Some("common"))?);
        let mut invented_time = sparse.clone();
        invented_time.created_at = Some(1_700_000_000_000);
        assert!(!verify(&reader, &invented_time, Some("common"))?);
        Ok(())
    }

    #[test]
    fn tokenless_endpoints_require_exact_published_evidence() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut index = TantivyIndex::open_or_create(&temp.path().join("index"))?;
        let expected = document(42, "/numeric", "1234 5678");
        let mut sibling = expected.clone();
        sibling.msg_idx = 1;
        index.add_prebuilt_documents_slice(&[sibling])?;
        index.commit()?;
        let before = index.reader()?;
        assert!(!verify(&before, &expected, None)?);

        index.upsert_prebuilt_documents_slice(std::slice::from_ref(&expected))?;
        index.commit()?;
        let reader = index.reader()?;
        assert!(verify(&reader, &expected, None)?);
        assert!(!verify(&before, &expected, None)?, "do not reopen the snapshot");
        let mut wrong_content = expected.clone();
        wrong_content.content = "1234 9999".into();
        let mut wrong_path = expected.clone();
        wrong_path.source_path = "/different".into();
        let mut wrong_source = expected.clone();
        wrong_source.source_id = "remote".into();
        let mut wrong_time = expected.clone();
        wrong_time.created_at = None;
        for wrong in [wrong_content, wrong_path, wrong_source, wrong_time] {
            assert!(!verify(&reader, &wrong, None)?);
        }
        Ok(())
    }

    #[test]
    fn tokenless_verification_handles_short_code_unicode_and_replay() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut index = TantivyIndex::open_or_create(&temp.path().join("index"))?;
        let docs = vec![
            document(1, "/code", "x=1; y=2;"),
            document(2, "/unicode", "你好 世界"),
        ];
        index.add_prebuilt_documents_slice(&docs)?;
        index.commit()?;
        // Exercise tombstoned sealed rows as well as first publication.
        for _ in 0..2 {
            index.upsert_prebuilt_documents_slice(&docs)?;
            index.commit()?;
            let reader = index.reader()?;
            for doc in &docs {
                assert!(verify(&reader, doc, None)?);
            }
        }
        Ok(())
    }

    #[test]
    fn tokenless_candidate_exhaustion_is_an_error_not_a_success() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut index = TantivyIndex::open_or_create(&temp.path().join("index"))?;
        index.add_prebuilt_documents_slice(&[
            document(1, "/one", "1234"),
            document(2, "/two", "5678"),
        ])?;
        index.commit()?;
        let reader = index.reader()?;
        let missing = document(3, "/missing", "9999");
        for budget in [0, 1] {
            let error = verify_with_budget(&reader, &missing, None, budget).unwrap_err();
            assert!(error.to_string().contains("candidate budget"));
        }
        assert!(!verify(&reader, &missing, None)?);
        Ok(())
    }
}
