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
pub(super) fn verify(
    reader: &QuillSearchIndex,
    document: &CassDocument,
    token: Option<&str>,
) -> Result<Option<bool>> {
    verify_with_budget(reader, document, token, MAX_CANDIDATES)
}

fn verify_with_budget(
    reader: &QuillSearchIndex,
    document: &CassDocument,
    token: Option<&str>,
    max_candidates: usize,
) -> Result<Option<bool>> {
    let Some(token) = token else { return Ok(None); };
    let parser = CassQueryParser::new(CASS_SEMANTIC_SCHEMA)?;
    let parsed = parser.parse(token, &CassQueryFilters::default());
    let mut clauses = vec![BooleanClause::new(Occur::Must, parsed.query)];
    for (field, value) in [
        (field::MSG_IDX, QueryValue::U64(document.msg_idx)),
        (field::SOURCE_ID, QueryValue::Str(document.source_id.clone())),
        (field::AGENT, QueryValue::Str(document.agent.clone())),
    ] {
        clauses.push(BooleanClause::new(Occur::Must, Query::set(field, vec![value])));
    }
    if let Some(created_at) = document.created_at {
        clauses.push(BooleanClause::new(Occur::Must,
            Query::set(field::CREATED_AT, vec![QueryValue::I64(created_at)])));
    }
    if let Some(workspace) = &document.workspace {
        clauses.push(BooleanClause::new(Occur::Must,
            Query::set(field::WORKSPACE, vec![QueryValue::Str(workspace.clone())])));
    }
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
            return Ok(Some(
                stored_i64(reader, field::CONVERSATION_ID, hit.global_docid)? == document.conversation_id
                && stored_text(reader, field::SOURCE_PATH, hit.global_docid)?.as_deref() == Some(document.source_path.as_str())
                && stored_text(reader, field::SOURCE_ID, hit.global_docid)?.as_deref() == Some(document.source_id.as_str())
                && stored_u64(reader, field::MSG_IDX, hit.global_docid)? == Some(document.msg_idx)
                && stored_text(reader, field::PREVIEW, hit.global_docid)?.as_deref() == Some(preview.as_str())
            ));
        }
        if page.hits.len() < limit { return Ok(Some(false)); }
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
        assert_eq!(verify(&reader, &target, Some("common"))?, Some(true));
        let mut foreign = target.clone(); foreign.source_path = "/wrong-source".into();
        assert_eq!(verify(&reader, &foreign, Some("common"))?, Some(false));
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
        assert_eq!(verify(&reader, &expected, Some("common"))?, Some(false));
        assert_eq!(verify(&reader, &expected, None)?, None);
        drop(reader);
        index.upsert_prebuilt_documents_slice(std::slice::from_ref(&expected))?;
        index.commit()?;
        assert_eq!(verify(&index.reader()?, &expected, Some("common"))?, Some(true));
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
        assert_eq!(verify(&index.reader()?, &expected, Some("common"))?, Some(false));
        Ok(())
    }
}
