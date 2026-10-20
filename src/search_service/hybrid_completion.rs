//! Complete a failed hybrid validation without paying for lexical retrieval twice.
//!
//! These candidates belong only to the current request and its pinned Quill
//! reader. They are not a cache, a fused page, or a fresh archive snapshot.

use anyhow::{Result, ensure};
use coding_agent_search::search::query::SearchHit;
use serde_json::{Value, json};

use super::{Mode, Refusal, Session, ann_metadata, protocol};

pub(super) struct NativePage {
    pub hits: Vec<SearchHit>,
    /// Candidate prefix retrieved at offset zero, before fusion/pagination.
    pub window: usize,
    pub reader_epoch: u64,
    pub reader_reused: bool,
    pub setup_ms: u128,
    pub search_ms: u128,
}

#[derive(Clone, Copy)]
pub(super) struct Request {
    pub mode: Mode,
    pub limit: usize,
    pub offset: usize,
    pub approximate: bool,
    pub semantic_reused: bool,
    pub semantic_setup_ms: u128,
}

/// Only a final semantic change refusal can select a previously completed
/// native page. This function deliberately has no query/filter input, engine
/// invocation, archive open, new deadline or retry callback.
pub(super) fn after_invalidation(
    session: &mut Session,
    error: anyhow::Error,
    native: Option<NativePage>,
    request: Request,
    lexical_failure: Option<&str>,
) -> Result<Value> {
    let reason = format!("{error:#}");
    session.semantic.last_failure = Some(reason.clone());
    if request.mode != Mode::Hybrid
        || !matches!(error.downcast_ref::<Refusal>(), Some(Refusal::Changed))
    {
        return Err(error);
    }
    let Some(native) = native else {
        // An archive recovery might have overlapped the change this witness
        // detected. Neither those candidates nor invalid semantic candidates
        // can stand in for a pinned native page. Do not rerun either engine.
        let detail = lexical_failure.unwrap_or(
            "archive-recovered lexical candidates are not a pinned Quill snapshot",
        );
        return Err(error.context(format!(
            "no reusable native lexical page after semantic invalidation; \
             no lexical query was repeated; {detail}"
        )));
    };
    if session.client.is_none()
        || native.reader_epoch == 0
        || native.reader_epoch != session.successful_opens
    {
        return Err(error.context(
            "the native lexical reader epoch is no longer retained; no lexical query was repeated",
        ));
    }
    ensure!(
        (1..=protocol::MAX_LIMIT).contains(&request.limit)
            && native.window <= protocol::MAX_WINDOW
            && native.hits.len() <= native.window
            && request
                .offset
                .checked_add(request.limit)
                .and_then(|end| end.checked_add(1))
                .is_some_and(|end| end <= native.window),
        "retained lexical page does not cover the admitted request window"
    );
    // Start from native scores and native order, never from the fused winners.
    // Even an empty native page is authoritative; it must not trigger SQLite.
    let mut hits: Vec<_> = native
        .hits
        .into_iter()
        .skip(request.offset)
        .take(request.limit + 1)
        .collect();
    let has_next = hits.len() > request.limit;
    let next_offset = request.offset.checked_add(request.limit).filter(|next| {
        has_next
            && next
                .checked_add(request.limit)
                .and_then(|end| end.checked_add(1))
                .is_some_and(|end| end <= protocol::MAX_WINDOW)
    });
    hits.truncate(request.limit);
    let hits = super::super::summarize_hits(hits)?;
    session.queries_completed = session.queries_completed.saturating_add(1);
    Ok(json!({
        "hits": hits, "count": hits.len(), "limit": request.limit, "offset": request.offset,
        "has_more": if has_next { Some(true) } else { None }, "next_offset": next_offset,
        "page_window_exhausted": has_next && next_offset.is_none(), "preview_only": true,
        "backend": "quill", "degraded": true, "fallback": null,
        "reader_reused": native.reader_reused, "result_reader_epoch": native.reader_epoch,
        "requested_mode": request.mode, "realized_mode": "lexical",
        "semantic_fallback_reason": reason, "semantic_reused": request.semantic_reused,
        "semantic_setup_ms": request.semantic_setup_ms,
        "setup_ms": native.setup_ms, "search_ms": native.search_ms,
        "lexical_candidates_reused": true, "candidate_window": native.window,
        "ann": ann_metadata(request.approximate), "snapshot": session.status(),
    }))
}

#[cfg(test)]
#[path = "hybrid_completion_tests.rs"]
mod tests;
