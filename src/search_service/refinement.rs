//! Candidate-only cross-encoder refinement of a retained lexical shortlist.
//!
//! The input is the existing bounded index preview, never a canonical body or
//! a global vector generation. Only an explicit startup model path enables this
//! lane. Model loading is local-only and lazy; ordinary searches remain lexical.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use coding_agent_search::search::fastembed_reranker::FastEmbedReranker;
use coding_agent_search::search::reranker::rerank_texts;
use serde_json::{Value, json};

use super::Session;
use super::protocol::{self, Filters};

pub(super) const MAX_CANDIDATES: usize = 32;
pub(super) const MAX_CANDIDATE_BYTES: usize = 8192;
pub(super) const MAX_INPUT_BYTES: usize = 256 * 1024;

pub(super) fn default_candidates() -> usize {
    20
}

pub(super) fn default_limit() -> usize {
    5
}

pub(super) fn validate(
    query: &str,
    lexical_query: &str,
    filters: &Filters,
    candidate_limit: usize,
    limit: usize,
) -> Result<(), &'static str> {
    if query.trim().is_empty() || query.len() > protocol::MAX_QUERY_BYTES {
        return Err("reranking query must be nonempty and at most 4096 UTF-8 bytes");
    }
    if !(1..=MAX_CANDIDATES).contains(&candidate_limit) {
        return Err("candidate_limit must be between 1 and 32");
    }
    if limit == 0 || limit > candidate_limit {
        return Err("limit must be between 1 and candidate_limit");
    }
    protocol::validate_search(lexical_query, candidate_limit, 0, filters)
}

#[derive(Default)]
pub(super) struct Refiner {
    directory: Option<PathBuf>,
    model: Option<FastEmbedReranker>,
    load_attempts: u64,
    successful_loads: u64,
    calls_completed: u64,
}

impl Refiner {
    pub(super) fn new(directory: Option<PathBuf>) -> Self {
        Self {
            directory,
            ..Self::default()
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.directory.is_some()
    }

    pub(super) fn loaded(&self) -> bool {
        self.model.is_some()
    }

    pub(super) fn unload(&mut self) {
        drop(self.model.take());
    }

    pub(super) fn status(&self) -> Value {
        json!({
            "enabled": self.enabled(),
            "loaded": self.loaded(),
            "model_id": self.directory.as_ref().map(|_| FastEmbedReranker::reranker_id_static()),
            "model_epoch": self.model.as_ref().map(|_| self.successful_loads),
            "load_attempts": self.load_attempts,
            "successful_loads": self.successful_loads,
            "calls_completed": self.calls_completed,
            "input": "title_and_index_snippet",
            "full_message_scored": false,
            "vector_assets_loaded": false,
            "candidate_limit": MAX_CANDIDATES,
            "candidate_bytes": MAX_CANDIDATE_BYTES,
            "total_input_bytes": MAX_INPUT_BYTES,
        })
    }

    fn score(&mut self, query: &str, documents: &[String]) -> Result<(Vec<f32>, Value)> {
        // Admission is checked again at the inference boundary, independent of
        // the wire decoder. Do not allocate or load a model from unchecked k.
        ensure!(
            !query.trim().is_empty() && query.len() <= protocol::MAX_QUERY_BYTES,
            "invalid reranking query"
        );
        ensure!(
            !documents.is_empty() && documents.len() <= MAX_CANDIDATES,
            "reranking requires between 1 and 32 candidates"
        );
        let mut bytes = 0_usize;
        for text in documents {
            ensure!(
                !text.trim().is_empty() && text.len() <= MAX_CANDIDATE_BYTES,
                "invalid or oversized candidate preview"
            );
            bytes = bytes
                .checked_add(text.len())
                .and_then(|n| n.checked_add(query.len()))
                .context("input size overflow")?;
            ensure!(bytes <= MAX_INPUT_BYTES, "candidate input exceeds 256 KiB");
        }
        let reused = self.loaded();
        let started = Instant::now();
        if self.model.is_none() {
            let directory = self.directory.as_ref().context("refinement is disabled")?;
            self.load_attempts = self.load_attempts.saturating_add(1);
            // This factory reads supplied local model files. No downloader,
            // daemon fallback, embedder, vector owner or archive is involved.
            let model = FastEmbedReranker::load_from_dir(directory).context(
                "cannot load the configured local reranker; install its safetensors model separately or use ordinary search",
            )?;
            self.successful_loads = self.successful_loads.saturating_add(1);
            self.model = Some(model);
        }
        let setup_ms = started.elapsed().as_millis();
        let started = Instant::now();
        let texts: Vec<&str> = documents.iter().map(String::as_str).collect();
        // The production bridge validates one finite score per exact input ID
        // and restores input order. Never use model output IDs as archive IDs.
        let scores = rerank_texts(
            self.model.as_ref().context("reranker is not loaded")?,
            query,
            &texts,
        )
        .context("candidate reranking failed; no lexical scores were relabeled")?;
        self.calls_completed = self.calls_completed.saturating_add(1);
        Ok((
            scores,
            json!({
                "status": "applied",
                "model_reused": reused,
                "model_setup_ms": setup_ms,
                "rerank_ms": started.elapsed().as_millis(),
                "input_bytes": bytes,
                "model_input_may_be_token_truncated": true,
                "model": self.status(),
            }),
        ))
    }
}

fn preview_text(hit: &Value) -> Result<String> {
    let title = hit["title"].as_str().context("candidate title is missing")?;
    let snippet = hit["snippet"].as_str().context("candidate snippet is missing")?;
    let bytes = title
        .len()
        .checked_add(snippet.len())
        .and_then(|n| n.checked_add(1))
        .context("candidate size overflow")?;
    ensure!(bytes <= MAX_CANDIDATE_BYTES, "candidate preview is too large");
    ensure!(
        !title.trim().is_empty() || !snippet.trim().is_empty(),
        "candidate preview contains no text"
    );
    Ok(format!("{title}\n{snippet}"))
}

/// Preserve complete hits and their lexical scores. Only the order and explicit
/// reranking fields change. Equal scores retain lexical order deterministically.
fn rank(mut hits: Vec<Value>, scores: Vec<f32>, limit: usize) -> Result<Vec<Value>> {
    ensure!(
        hits.len() <= MAX_CANDIDATES && hits.len() == scores.len(),
        "reranker output does not match the admitted candidate pool"
    );
    ensure!(
        (1..=MAX_CANDIDATES).contains(&limit),
        "invalid refined result limit"
    );
    ensure!(scores.iter().all(|score| score.is_finite()), "non-finite reranking score");
    for (index, (hit, score)) in hits.iter_mut().zip(&scores).enumerate() {
        let fields = hit.as_object_mut().context("candidate is not an object")?;
        fields.insert("lexical_rank".into(), json!(index + 1));
        fields.insert("rerank_score".into(), json!(score));
    }
    let mut ranked: Vec<_> = hits.into_iter().zip(scores).enumerate().collect();
    ranked.sort_by(|(left_rank, (_, left)), (right_rank, (_, right))| {
        if left == right {
            left_rank.cmp(right_rank)
        } else {
            right.total_cmp(left).then_with(|| left_rank.cmp(right_rank))
        }
    });
    Ok(ranked.into_iter().take(limit).map(|(_, (hit, _))| hit).collect())
}

impl Session {
    pub(super) fn refine(
        &mut self,
        query: &str,
        lexical_query: &str,
        filters: Filters,
        candidate_limit: usize,
        limit: usize,
    ) -> Result<Value> {
        validate(query, lexical_query, &filters, candidate_limit, limit)
            .map_err(anyhow::Error::msg)?;
        ensure!(self.refiner.enabled(), "refinement requires --reranker-model at startup");
        // Run one bounded lexical window, under the existing request watchdog.
        // Inference never opens the global FSVI/HNSW generation or canonical DB.
        let lexical = self.search(lexical_query, filters, candidate_limit, 0)?;
        let hits = lexical["hits"].as_array().context("lexical hits are missing")?;
        ensure!(hits.len() <= candidate_limit, "lexical candidate budget exceeded");
        let count = hits.len();
        let (hits, metadata) = if hits.is_empty() {
            // Empty retrieval is not a reason to load a model or invent scores.
            (Vec::new(), json!({"status": "no_candidates", "model": self.refiner.status()}))
        } else {
            let documents = hits.iter().map(preview_text).collect::<Result<Vec<_>>>()?;
            let (scores, metadata) = self.refiner.score(query, &documents)?;
            (rank(hits.clone(), scores, limit)?, metadata)
        };
        Ok(json!({
            "hits": hits,
            "count": hits.len(),
            "limit": limit,
            "candidate_limit": candidate_limit,
            "candidates_considered": count,
            "more_lexical_candidates": lexical["has_more"],
            "ranking_scope": "bounded_lexical_candidate_pool",
            // Reranking a different pool can reorder all winners. A lexical
            // offset is not a valid continuation of this refined ranking.
            "next_offset": null,
            "preview_only": true,
            "reader_reused": lexical["reader_reused"],
            "setup_ms": lexical["setup_ms"],
            "search_ms": lexical["search_ms"],
            "refinement": metadata,
            "snapshot": self.status(),
        }))
    }
}

#[cfg(test)]
#[path = "refinement_tests.rs"]
mod tests;
