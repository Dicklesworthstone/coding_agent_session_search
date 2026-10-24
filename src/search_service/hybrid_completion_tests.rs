//! Real Quill candidates and real file-witness invalidation at the completion
//! boundary. These are not a full concurrent semantic-search workload.

use super::*;
use anyhow::Context;
use coding_agent_search::search::query::{FieldMask, SearchFilters};
use coding_agent_search::search::tantivy::TantivyIndex;
use frankensearch::quill::cass::CassDocument;
use std::path::Path;

fn request(limit: usize, offset: usize) -> Request {
    Request {
        mode: Mode::Hybrid,
        limit,
        offset,
        approximate: true,
        semantic_reused: true,
        semantic_setup_ms: 11,
    }
}

fn fixture(root: &Path) -> Result<Session> {
    let path = root.join("lexical");
    let mut writer = TantivyIndex::open_or_create(&path)?;
    for id in 1..=24 {
        writer.add_prebuilt_documents_slice(&[CassDocument {
            agent: "codex".into(),
            workspace: Some("/work/δ".into()),
            workspace_original: None,
            source_path: format!("/never-opened/{id}.jsonl"),
            msg_idx: 7,
            created_at: Some(1_700_000_000_000 + id),
            title: Some(format!("Native document {id}")),
            content: format!("git fixture test isolation native{id}"),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
            conversation_id: Some(id),
        }])?;
        writer.commit()?;
    }
    drop(writer);
    assert_eq!(
        coding_agent_search::search::quill_bridge::live_segment_count(&path),
        Some(24)
    );
    let mut session = Session::new(path);
    session.ensure_loaded()?;
    // Any new fallback open would fail. Completed native pages do not need it.
    session.archive = Some(root.join("missing.db"));
    session.lexical_recovery.enabled = true;
    Ok(session)
}

fn native(session: &Session, query: &str, window: usize) -> Result<NativePage> {
    let hits = session.client.as_ref().context("native reader")?.search(
        query,
        SearchFilters::default(),
        window,
        0,
        FieldMask::new(false, true, true, false),
    )?;
    Ok(NativePage {
        hits,
        window,
        reader_epoch: session.successful_opens,
        reader_reused: true,
        setup_ms: 3,
        search_ms: 7,
    })
}

fn invalidate(session: &mut Session, root: &Path) -> Result<anyhow::Error> {
    let path = root.join("semantic-witness");
    std::fs::write(&path, b"before")?;
    session.semantic.guard.add(path.clone())?;
    session.semantic.check()?;
    // Changing length makes this deterministic even on coarse-mtime platforms.
    std::fs::write(path, b"changed-generation")?;
    Ok(session
        .semantic
        .check()
        .expect_err("the actual file witness must invalidate the semantic generation"))
}

fn assert_no_retrieval(session: &Session, opens: u64) {
    assert_eq!(session.open_attempts, opens);
    assert_eq!(session.successful_opens, opens);
    assert_eq!(session.lexical_recovery.status()["attempts"], 0);
    assert!(!session.archive.as_ref().unwrap().exists());
}

#[test]
fn invalidation_preserves_native_scores_identities_and_pagination_for_both_queries() -> Result<()> {
    for query in ["git fixture", "test isolation"] {
        let root = tempfile::tempdir()?;
        let mut session = fixture(root.path())?;
        let page = native(&session, query, 12)?;
        assert_eq!(page.hits.len(), 12);
        let expected = super::super::super::summarize_hits(
            page.hits.iter().skip(2).take(3).cloned().collect(),
        )?;
        let error = invalidate(&mut session, root.path())?;
        let opens = session.successful_opens;
        let result = after_invalidation(&mut session, error, Some(page), request(3, 2), None)?;
        assert_eq!(result["hits"], json!(expected));
        assert_eq!(result["count"], 3);
        assert_eq!(result["has_more"], true);
        assert_eq!(result["next_offset"], 5);
        assert_eq!(result["backend"], "quill");
        assert_eq!(result["realized_mode"], "lexical");
        assert_eq!(result["degraded"], true);
        assert!(result["fallback"].is_null());
        assert_eq!(result["result_reader_epoch"], opens);
        assert_eq!(result["lexical_candidates_reused"], true);
        assert_eq!(result["setup_ms"], 3);
        assert_eq!(result["search_ms"], 7);
        assert_eq!(result["semantic_setup_ms"], 11);
        assert_eq!(result["ann"]["used"], false);
        assert_eq!(result["snapshot"]["semantic"]["requires_reload"], true);
        assert_eq!(session.queries_completed, 1);
        assert_no_retrieval(&session, opens);
        assert_eq!(
            super::super::error_kind(&session.semantic.check().unwrap_err()),
            "semantic_reload_required"
        );
    }
    Ok(())
}

#[test]
fn authoritative_empty_native_page_never_reopens_an_archive_after_invalidation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut session = fixture(root.path())?;
    let page = native(&session, "zzzznotpresent", 6)?;
    assert!(page.hits.is_empty());
    let error = invalidate(&mut session, root.path())?;
    let result = after_invalidation(&mut session, error, Some(page), request(2, 0), None)?;
    assert_eq!(result["count"], 0);
    assert!(result["has_more"].is_null());
    assert!(result["next_offset"].is_null());
    assert_eq!(result["backend"], "quill");
    assert_no_retrieval(&session, 1);
    Ok(())
}

#[test]
fn lost_or_archive_recovered_lexical_leg_is_not_retried_after_invalidation() -> Result<()> {
    let root = tempfile::tempdir()?;
    for failure in [
        None,
        Some("Quill query fuel exhausted; fallback is disabled"),
    ] {
        let mut session = Session::new(root.path().join("never-created-index"));
        session.archive = Some(root.path().join("never-created-archive"));
        session.lexical_recovery.enabled = true;
        let error = invalidate(&mut session, root.path())?;
        let error = after_invalidation(&mut session, error, None, request(1, 0), failure)
            .expect_err("no independent native page survived");
        assert_eq!(super::super::error_kind(&error), "semantic_reload_required");
        assert!(error.to_string().contains("no lexical query was repeated"));
        if let Some(failure) = failure {
            assert!(error.to_string().contains(failure));
        }
        assert_eq!(session.queries_completed, 0);
        assert_no_retrieval(&session, 0);
        assert!(!session.index.exists());
    }
    Ok(())
}

#[test]
fn semantic_only_and_unrelated_failures_cannot_publish_a_lexical_page() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut session = fixture(root.path())?;
    let page = native(&session, "git fixture", 6)?;
    let error = invalidate(&mut session, root.path())?;
    let semantic = Request {
        mode: Mode::Semantic,
        ..request(1, 0)
    };
    let error = after_invalidation(&mut session, error, Some(page), semantic, None).unwrap_err();
    assert_eq!(super::super::error_kind(&error), "semantic_reload_required");
    let page = native(&session, "test isolation", 6)?;
    let error = after_invalidation(
        &mut session,
        anyhow::anyhow!("query cancelled"),
        Some(page),
        request(1, 0),
        None,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "query cancelled");
    assert_eq!(session.queries_completed, 0);
    assert_no_retrieval(&session, 1);
    Ok(())
}

#[test]
fn invalid_page_windows_refuse_before_publishing_a_success() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut session = fixture(root.path())?;
    for (limit, offset, window) in [(0, 0, 6), (101, 0, 200), (1, usize::MAX, 6), (5, 2, 6)] {
        let page = native(&session, "git fixture", window)?;
        let error = after_invalidation(
            &mut session,
            Refusal::Changed.into(),
            Some(page),
            request(limit, offset),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("admitted request window"));
    }
    let mut page = native(&session, "git fixture", 6)?;
    page.window = 2;
    let error = after_invalidation(
        &mut session,
        Refusal::Changed.into(),
        Some(page),
        request(1, 0),
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("admitted request window"));
    assert_eq!(session.queries_completed, 0);
    assert_no_retrieval(&session, 1);
    Ok(())
}

#[test]
fn native_page_cannot_claim_an_unloaded_or_replaced_reader_epoch() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut session = fixture(root.path())?;
    let page = native(&session, "git fixture", 6)?;
    let stale = native(&session, "test isolation", 6)?;
    session.unload();
    let error = after_invalidation(
        &mut session,
        Refusal::Changed.into(),
        Some(page),
        request(1, 0),
        None,
    )
    .unwrap_err();
    assert_eq!(super::super::error_kind(&error), "semantic_reload_required");
    assert!(error.to_string().contains("reader epoch"));
    session.ensure_loaded()?;
    assert_eq!(session.successful_opens, 2);
    let error = after_invalidation(
        &mut session,
        Refusal::Changed.into(),
        Some(stale),
        request(1, 0),
        None,
    )
    .unwrap_err();
    assert_eq!(super::super::error_kind(&error), "semantic_reload_required");
    assert!(error.to_string().contains("reader epoch"));
    assert_eq!(session.queries_completed, 0);
    assert_no_retrieval(&session, 2);
    Ok(())
}
