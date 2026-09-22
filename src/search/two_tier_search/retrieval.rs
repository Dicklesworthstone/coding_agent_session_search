//! Checked, bounded retrieval for the synchronous two-tier adapter.
//!
//! Quality retrieval visits the entire pinned index in fixed-size batches, not
//! just the fast shortlist. The scan is still O(N) scoring work; only its extra
//! candidate/result memory is O(batch + k). This is not the manifest-backed
//! SemanticReader admission path and does not change its capability flags.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ops::Range;
use std::time::Instant;

use super::{
    DaemonClient, FsVectorHit, ScoredResult, SearchPhase, TwoTierError, TwoTierIndex,
    TwoTierSearchIter,
};

const QUALITY_SCORE_BATCH_SIZE: usize = 256;
const RRF_RANK_CONSTANT: f64 = 60.0;

fn index_error(reason: &str) -> TwoTierError {
    TwoTierError::IndexError(reason.to_owned())
}

fn validate_query(query: &[f32]) -> Result<(), TwoTierError> {
    if query.is_empty()
        || query.iter().any(|value| !value.is_finite())
        || query.iter().all(|value| *value == 0.0)
    {
        return Err(index_error("query embedding must be finite and nonzero"));
    }
    Ok(())
}

/// Ascending comparator: the best result comes first. Record index breaks ties
/// between distinct documents belonging to the same archive message.
fn best_first(left: &ScoredResult, right: &ScoredResult) -> Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| left.message_id.cmp(&right.message_id))
        .then_with(|| left.idx.cmp(&right.idx))
}

#[derive(Debug)]
struct RankedHit(ScoredResult);

impl PartialEq for RankedHit {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for RankedHit {}

impl PartialOrd for RankedHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedHit {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap's maximum is the WORST retained result.
        best_first(&self.0, &other.0)
    }
}

fn collect_quality_topk(
    message_ids: &[u64],
    k: usize,
    mut score_batch: impl FnMut(Range<usize>) -> Result<Vec<Option<f32>>, TwoTierError>,
) -> Result<Vec<ScoredResult>, TwoTierError> {
    let limit = k.min(message_ids.len());
    if limit == 0 {
        return Ok(Vec::new());
    }
    // Do not eagerly allocate a caller-supplied capacity (possibly usize::MAX).
    let mut retained = BinaryHeap::<RankedHit>::new();
    for start in (0..message_ids.len()).step_by(QUALITY_SCORE_BATCH_SIZE) {
        let end = start
            .saturating_add(QUALITY_SCORE_BATCH_SIZE)
            .min(message_ids.len());
        let scores = score_batch(start..end)?;
        if scores.len() != end - start {
            return Err(index_error(
                "quality backend returned a misaligned score batch",
            ));
        }
        for (offset, score) in scores.into_iter().enumerate() {
            let Some(score) = score else {
                // Absence is not a zero similarity score.
                continue;
            };
            if !score.is_finite() {
                return Err(index_error("quality backend returned a non-finite score"));
            }
            let idx = start + offset;
            let candidate = RankedHit(ScoredResult {
                idx,
                message_id: message_ids[idx],
                score,
            });
            if retained.len() < limit {
                retained.push(candidate);
            } else if let Some(mut worst) = retained.peek_mut()
                && candidate < *worst
            {
                *worst = candidate;
            }
        }
    }
    Ok(retained
        .into_sorted_vec()
        .into_iter()
        .map(|hit| hit.0)
        .collect())
}

impl TwoTierIndex {
    pub(super) fn validate_side_tables(&self) -> Result<(), TwoTierError> {
        if self.metadata.doc_count != self.doc_ids.len()
            || self.message_ids.len() != self.doc_ids.len()
            || (self.metadata.doc_count != 0 && self.fs_index.is_none())
        {
            return Err(index_error(
                "two-tier index metadata is not aligned with its reader",
            ));
        }
        Ok(())
    }

    /// Error-preserving counterpart to the legacy Vec-returning convenience API.
    pub fn try_search_fast(
        &self,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<ScoredResult>, TwoTierError> {
        self.validate_side_tables()?;
        let limit = k.min(self.len());
        if limit == 0 {
            return Ok(Vec::new());
        }
        validate_query(query)?;
        let reader = self
            .fs_index
            .as_ref()
            .ok_or_else(|| index_error("fast reader is unavailable"))?;
        let hits = reader
            .search_fast(query, limit)
            .map_err(|error| TwoTierError::IndexError(error.to_string()))?;
        if hits.len() > limit {
            return Err(index_error("fast backend exceeded its candidate limit"));
        }
        let mut seen = HashSet::new();
        for hit in &hits {
            let idx = hit.index as usize;
            let Some(doc_id) = self.doc_ids.get(idx) else {
                return Err(index_error(
                    "fast backend returned an out-of-range document",
                ));
            };
            if !hit.score.is_finite()
                || !seen.insert(idx)
                || hit.doc_id.as_str() != doc_id.encode().as_str()
            {
                return Err(index_error(
                    "fast backend returned invalid document scores or identities",
                ));
            }
        }
        let mut results = self.hits_to_scored_results(hits);
        results.sort_by(best_first);
        Ok(results)
    }

    /// Exact quality retrieval across the index, with O(k + batch) additional
    /// candidates rather than an archive-sized hit/score/sort allocation.
    pub fn try_search_quality(
        &self,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<ScoredResult>, TwoTierError> {
        self.validate_side_tables()?;
        if k == 0 || self.is_empty() {
            return Ok(Vec::new());
        }
        validate_query(query)?;
        let reader = self
            .fs_index
            .as_ref()
            .filter(|reader| reader.has_quality_index())
            .ok_or_else(|| index_error("quality reader is unavailable"))?;
        collect_quality_topk(&self.message_ids, k, |range| {
            let mut hits = Vec::with_capacity(range.len());
            for idx in range {
                let index = u32::try_from(idx)
                    .map_err(|_| index_error("document index exceeds the vector backend range"))?;
                hits.push(FsVectorHit {
                    index,
                    score: 0.0,
                    doc_id: self.doc_ids[idx].encode().into(),
                });
            }
            reader
                .quality_scores_for_hits(query, &hits)
                .map_err(|error| TwoTierError::IndexError(error.to_string()))
        })
    }
}

/// Weighted reciprocal-rank fusion over the union of independently retrieved
/// documents. Similarity magnitudes from different embedding spaces are never
/// compared. Missing membership contributes no evidence; it is not a measured
/// zero similarity. The result remains bounded to k, in deterministic order.
fn fuse_ranked_results(
    fast: &[ScoredResult],
    quality: &[ScoredResult],
    quality_weight: f32,
    k: usize,
) -> Result<Vec<ScoredResult>, TwoTierError> {
    if !quality_weight.is_finite() || !(0.0..=1.0).contains(&quality_weight) {
        return Err(index_error(
            "quality weight must be finite and between zero and one",
        ));
    }
    if k == 0 {
        return Ok(Vec::new());
    }
    let mut union = HashMap::<usize, ScoredResult>::new();
    for (ranked, weight) in [
        (fast, 1.0 - f64::from(quality_weight)),
        (quality, f64::from(quality_weight)),
    ] {
        if weight == 0.0 {
            continue;
        }
        let mut seen = HashSet::new();
        for (rank, hit) in ranked.iter().enumerate() {
            if !hit.score.is_finite()
                || !seen.insert(hit.idx)
                || (rank > 0 && ranked[rank - 1].score.total_cmp(&hit.score).is_lt())
            {
                return Err(index_error(
                    "fusion input must contain unique, finite, ranked documents",
                ));
            }
            let contribution =
                (weight * RRF_RANK_CONSTANT / (RRF_RANK_CONSTANT + rank as f64 + 1.0)) as f32;
            let entry = union.entry(hit.idx).or_insert(ScoredResult {
                idx: hit.idx,
                message_id: hit.message_id,
                score: 0.0,
            });
            if entry.message_id != hit.message_id {
                return Err(index_error("tier results disagree on document identity"));
            }
            entry.score += contribution;
        }
    }
    let mut results: Vec<_> = union.into_values().collect();
    results.sort_by(best_first);
    results.truncate(k);
    Ok(results)
}

impl<'a, D: DaemonClient> Iterator for TwoTierSearchIter<'a, D> {
    type Item = SearchPhase;

    fn next(&mut self) -> Option<Self::Item> {
        match self.phase {
            0 => {
                if self.searcher.config.quality_only {
                    self.phase = 2;
                    let start = Instant::now();
                    return Some(
                        match self.searcher.search_quality_only(&self.query, self.k) {
                            Ok(results) => SearchPhase::Refined {
                                results,
                                latency_ms: start.elapsed().as_millis() as u64,
                            },
                            Err(error) => SearchPhase::RefinementFailed {
                                error: error.to_string(),
                            },
                        },
                    );
                }
                let refine = !self.searcher.config.fast_only
                    && self.searcher.config.max_refinement_docs != 0
                    && self.searcher.config.quality_weight != 0.0
                    && self.k != 0
                    && !self.searcher.index.is_empty();
                self.phase = if refine { 1 } else { 2 };
                let limit = if refine {
                    self.k.max(self.searcher.config.max_refinement_docs)
                } else {
                    self.k
                }
                .min(self.searcher.index.len());
                let start = Instant::now();
                match self.searcher.search_fast_only(&self.query, limit) {
                    Ok(results) => {
                        // Retain a bounded candidate pool, but show only the
                        // requested page. No quality model runs until next().
                        let initial = results.iter().take(self.k).cloned().collect();
                        self.fast_results = Some(results);
                        Some(SearchPhase::Initial {
                            results: initial,
                            latency_ms: start.elapsed().as_millis() as u64,
                        })
                    }
                    Err(error) => {
                        self.phase = 2;
                        Some(SearchPhase::RefinementFailed {
                            error: error.to_string(),
                        })
                    }
                }
            }
            1 => {
                self.phase = 2;
                let start = Instant::now();
                let limit = self
                    .searcher
                    .config
                    .max_refinement_docs
                    .min(self.searcher.index.len());
                let result = self
                    .searcher
                    .search_quality_only(&self.query, limit)
                    .and_then(|quality| {
                        fuse_ranked_results(
                            self.fast_results.as_deref().unwrap_or_default(),
                            &quality,
                            self.searcher.config.quality_weight,
                            self.k,
                        )
                    });
                Some(match result {
                    Ok(results) => SearchPhase::Refined {
                        results,
                        latency_ms: start.elapsed().as_millis() as u64,
                    },
                    Err(error) => SearchPhase::RefinementFailed {
                        error: error.to_string(),
                    },
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::embedder::{Embedder, EmbedderError};
    use crate::search::two_tier_search::{
        DocumentId, TwoTierConfig, TwoTierEntry, TwoTierSearcher,
    };
    use frankensearch::{DaemonError, ModelCategory};
    use half::f16;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn hit(idx: usize, message_id: u64, score: f32) -> ScoredResult {
        ScoredResult {
            idx,
            message_id,
            score,
        }
    }

    fn signatures(hits: &[ScoredResult]) -> Vec<(usize, u64, u32)> {
        hits.iter()
            .map(|hit| (hit.idx, hit.message_id, hit.score.to_bits()))
            .collect()
    }

    #[test]
    fn bounded_quality_matches_full_sort_across_batch_boundaries_and_limits() {
        for count in [0, 1, 255, 256, 257, 1031] {
            let ids: Vec<_> = (0..count).map(|i| ((count - i) / 2) as u64).collect();
            let scores: Vec<_> = (0..count)
                .map(|i| (i % 11 != 0).then_some(((i * 37) % 113) as f32 - 60.0))
                .collect();
            for k in [0, 1, 7, 256, usize::MAX] {
                let mut scanned = 0;
                let actual = collect_quality_topk(&ids, k, |range| {
                    assert!(range.len() <= QUALITY_SCORE_BATCH_SIZE);
                    assert_eq!(range.start, scanned);
                    scanned = range.end;
                    Ok(scores[range].to_vec())
                })
                .unwrap();
                let mut expected: Vec<_> = scores
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, score)| score.map(|score| hit(idx, ids[idx], score)))
                    .collect();
                expected.sort_by(best_first);
                expected.truncate(k);
                assert_eq!(
                    signatures(&actual),
                    signatures(&expected),
                    "count={count}, k={k}"
                );
                assert_eq!(scanned, if k == 0 { 0 } else { count });
            }
        }
    }

    #[test]
    fn bounded_quality_keeps_real_zero_and_negative_scores_and_deterministic_ties() {
        let ids = [4, 3, 3, 1, 0];
        let results = collect_quality_topk(&ids, 4, |_| {
            Ok(vec![Some(0.0), Some(1.0), Some(1.0), Some(-1.0), None])
        })
        .unwrap();
        assert_eq!(
            results.iter().map(|r| r.idx).collect::<Vec<_>>(),
            [1, 2, 0, 3]
        );
        assert_eq!(results[2].score, 0.0);
        assert_eq!(results[3].score, -1.0);
    }

    #[test]
    fn malformed_quality_batches_fail_instead_of_returning_partial_rankings() {
        for scores in [
            vec![],
            vec![Some(1.0)],
            vec![Some(1.0); 3],
            vec![Some(1.0), Some(f32::NAN)],
            vec![Some(f32::INFINITY), None],
            vec![Some(f32::NEG_INFINITY), Some(1.0)],
        ] {
            assert!(collect_quality_topk(&[1, 2], 1, |_| Ok(scores.clone())).is_err());
        }
        let ids = vec![1; QUALITY_SCORE_BATCH_SIZE + 1];
        let mut calls = 0;
        let error = collect_quality_topk(&ids, 1, |range| {
            calls += 1;
            if calls == 2 {
                Err(index_error("injected backend failure"))
            } else {
                Ok(vec![Some(1.0); range.len()])
            }
        })
        .unwrap_err();
        assert_eq!(calls, 2);
        assert!(error.to_string().contains("injected backend failure"));
    }

    #[test]
    fn empty_quality_requests_do_not_invoke_backend() {
        for (ids, k) in [(&[][..], 100), (&[1][..], 0)] {
            assert!(
                collect_quality_topk(ids, k, |_| panic!("unexpected scoring"))
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn fusion_introduces_quality_only_documents_without_comparing_vector_spaces() {
        let fast = [hit(0, 10, 10000.0), hit(1, 20, 9000.0)];
        let quality = [hit(2, 30, -0.5), hit(1, 20, -0.75)];
        let fused = fuse_ranked_results(&fast, &quality, 0.7, 3).unwrap();
        assert_eq!(fused.iter().map(|r| r.idx).collect::<Vec<_>>(), [1, 2, 0]);
        assert!(fused.iter().all(|r| (0.0..=1.0).contains(&r.score)));
        let rescaled = [hit(0, 10, 0.002), hit(1, 20, 0.001)];
        assert_eq!(
            signatures(&fused),
            signatures(&fuse_ranked_results(&rescaled, &quality, 0.7, 3).unwrap())
        );
    }

    #[test]
    fn fusion_weight_endpoints_and_ties_have_deterministic_membership() {
        let fast = [hit(1, 20, 1.0)];
        let quality = [hit(2, 10, 1.0)];
        assert_eq!(
            fuse_ranked_results(&fast, &quality, 0.0, 5).unwrap()[0].idx,
            1
        );
        assert_eq!(
            fuse_ranked_results(&fast, &quality, 1.0, 5).unwrap()[0].idx,
            2
        );
        let tied = fuse_ranked_results(&fast, &quality, 0.5, 5).unwrap();
        assert_eq!(tied.iter().map(|r| r.idx).collect::<Vec<_>>(), [2, 1]);
        assert!(
            fuse_ranked_results(&fast, &quality, 0.5, 0)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn fusion_rejects_invalid_weights_scores_duplicates_order_and_identity() {
        for weight in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
            assert!(fuse_ranked_results(&[], &[], weight, 1).is_err());
        }
        for fast in [
            vec![hit(0, 1, f32::NAN)],
            vec![hit(0, 1, 1.0), hit(0, 1, 0.5)],
            vec![hit(0, 1, 0.0), hit(1, 2, 1.0)],
        ] {
            assert!(fuse_ranked_results(&fast, &[], 0.7, 2).is_err());
        }
        assert!(fuse_ranked_results(&[hit(0, 1, 1.0)], &[hit(0, 2, 1.0)], 0.7, 2).is_err());
    }

    // Synthetic providers prove orchestration, not native-model relevance.
    struct Fast {
        calls: AtomicUsize,
        vector: Vec<f32>,
    }

    impl Embedder for Fast {
        fn embed_sync(&self, _text: &str) -> Result<Vec<f32>, EmbedderError> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(self.vector.clone())
        }
        fn dimension(&self) -> usize {
            self.vector.len()
        }
        fn id(&self) -> &str {
            "retrieval-test-fast"
        }
        fn is_semantic(&self) -> bool {
            false
        }
        fn category(&self) -> ModelCategory {
            ModelCategory::HashEmbedder
        }
    }

    struct Daemon {
        calls: AtomicUsize,
        probes: AtomicUsize,
        vector: Vec<f32>,
        fail: bool,
    }

    impl DaemonClient for Daemon {
        fn id(&self) -> &str {
            "retrieval-test-daemon"
        }
        fn is_available(&self) -> bool {
            self.probes.fetch_add(1, AtomicOrdering::SeqCst);
            true
        }
        fn embed(&self, _text: &str, _request: &str) -> Result<Vec<f32>, DaemonError> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if self.fail {
                Err(DaemonError::Unavailable("injected quality failure".into()))
            } else {
                Ok(self.vector.clone())
            }
        }
        fn embed_batch(
            &self,
            _texts: &[&str],
            _request: &str,
        ) -> Result<Vec<Vec<f32>>, DaemonError> {
            panic!("query refinement must not re-embed archive documents")
        }
        fn rerank(
            &self,
            _query: &str,
            _documents: &[&str],
            _request: &str,
        ) -> Result<Vec<f32>, DaemonError> {
            panic!("retrieval must not depend on a cross-encoder")
        }
    }

    fn providers(vector: Vec<f32>, fail: bool) -> (Arc<Fast>, Arc<Daemon>) {
        (
            Arc::new(Fast {
                calls: AtomicUsize::new(0),
                vector: vec![1.0, 1.0],
            }),
            Arc::new(Daemon {
                calls: AtomicUsize::new(0),
                probes: AtomicUsize::new(0),
                vector,
                fail,
            }),
        )
    }

    fn config() -> TwoTierConfig {
        TwoTierConfig {
            fast_dimension: 2,
            quality_dimension: 2,
            max_refinement_docs: 1,
            ..Default::default()
        }
    }

    fn entry(id: u64, fast: [f32; 2], quality: [f32; 2]) -> TwoTierEntry {
        TwoTierEntry {
            doc_id: DocumentId::Session(format!("retrieval-{id}")),
            message_id: id,
            fast_embedding: fast.into_iter().map(f16::from_f32).collect(),
            quality_embedding: quality.into_iter().map(f16::from_f32).collect(),
        }
    }

    fn index(config: &TwoTierConfig) -> TwoTierIndex {
        let build_config = TwoTierConfig {
            fast_only: false,
            quality_only: false,
            ..config.clone()
        };
        TwoTierIndex::build(
            "test-fast",
            "test-quality",
            &build_config,
            [
                entry(10, [1.0, 1.0], [-1.0, -1.0]),
                entry(20, [-1.0, -1.0], [1.0, 1.0]),
                entry(30, [1.0, 0.0], [1.0, 0.0]),
            ],
        )
        .unwrap()
    }

    #[test]
    fn progressive_quality_recovers_a_document_outside_the_entire_fast_pool() {
        let config = config();
        let index = index(&config);
        let (fast, daemon) = providers(vec![1.0, 1.0], false);
        let embedder: Arc<dyn Embedder> = fast.clone();
        let searcher = TwoTierSearcher::new(&index, embedder, Some(daemon.clone()), config);
        let mut phases = searcher.search("private query", 1);
        let Some(SearchPhase::Initial {
            results: initial, ..
        }) = phases.next()
        else {
            panic!("missing initial phase");
        };
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].message_id, 10);
        assert_eq!(daemon.probes.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(daemon.calls.load(AtomicOrdering::SeqCst), 0);
        let Some(SearchPhase::Refined { results, .. }) = phases.next() else {
            panic!("missing refined phase");
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_id, 20);
        assert_eq!(
            initial[0].message_id, 10,
            "initial page remains owned by caller"
        );
        assert_eq!(fast.calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(daemon.calls.load(AtomicOrdering::SeqCst), 1);
        assert!(phases.next().is_none());
        assert!(phases.next().is_none());
    }

    #[test]
    fn quality_failure_is_not_an_empty_successful_refinement() {
        for (vector, fail) in [
            (vec![1.0], false),
            (vec![f32::NAN, 1.0], false),
            (vec![0.0, 0.0], false),
            (vec![1.0, 1.0], true),
        ] {
            let config = config();
            let index = index(&config);
            let (fast, daemon) = providers(vector, fail);
            let searcher = TwoTierSearcher::new(&index, fast, Some(daemon.clone()), config);
            let phases: Vec<_> = searcher.search("query", 1).collect();
            assert_eq!(phases.len(), 2);
            assert!(
                matches!(&phases[0], SearchPhase::Initial { results, .. } if results.len() == 1)
            );
            assert!(matches!(&phases[1], SearchPhase::RefinementFailed { .. }));
            assert_eq!(daemon.calls.load(AtomicOrdering::SeqCst), 1);
            assert!(searcher.search_quality_only("query", 1).is_err());
        }
    }

    #[test]
    fn disabled_refinement_never_contacts_daemon() {
        for mode in 0..3 {
            let mut config = config();
            match mode {
                0 => config.fast_only = true,
                1 => config.max_refinement_docs = 0,
                _ => config.quality_weight = 0.0,
            }
            let index = index(&config);
            let (fast, daemon) = providers(vec![1.0, 1.0], true);
            let searcher = TwoTierSearcher::new(&index, fast, Some(daemon.clone()), config);
            let phases: Vec<_> = searcher.search("query", 1).collect();
            assert_eq!(phases.len(), 1);
            assert!(matches!(&phases[0], SearchPhase::Initial { .. }));
            assert_eq!(daemon.probes.load(AtomicOrdering::SeqCst), 0);
            assert_eq!(daemon.calls.load(AtomicOrdering::SeqCst), 0);
        }
    }

    #[test]
    fn zero_limit_and_empty_index_do_not_load_or_probe_models() {
        for empty in [false, true] {
            for quality_only in [false, true] {
                let config = TwoTierConfig {
                    quality_only,
                    ..config()
                };
                let index = if empty {
                    TwoTierIndex::build("test-fast", "test-quality", &config, []).unwrap()
                } else {
                    index(&config)
                };
                let (fast, daemon) = providers(vec![1.0], true);
                let embedder: Arc<dyn Embedder> = fast.clone();
                let searcher = TwoTierSearcher::new(&index, embedder, Some(daemon.clone()), config);
                let k = if empty { 10 } else { 0 };
                assert!(searcher.search_fast_only("query", k).unwrap().is_empty());
                assert!(searcher.search_quality_only("query", k).unwrap().is_empty());
                let phases: Vec<_> = searcher.search("query", k).collect();
                assert_eq!(phases.len(), 1);
                assert!(matches!(
                    &phases[0],
                    SearchPhase::Initial { results, .. } | SearchPhase::Refined { results, .. }
                        if results.is_empty()
                ));
                assert_eq!(fast.calls.load(AtomicOrdering::SeqCst), 0);
                assert_eq!(daemon.probes.load(AtomicOrdering::SeqCst), 0);
                assert_eq!(daemon.calls.load(AtomicOrdering::SeqCst), 0);
            }
        }
    }

    #[test]
    fn checked_apis_reject_misaligned_metadata_and_malformed_fast_queries() {
        let mut index = index(&config());
        for query in [vec![1.0], vec![f32::INFINITY, 1.0], vec![0.0, -0.0]] {
            assert!(index.try_search_fast(&query, 1).is_err());
            assert!(index.try_search_quality(&query, 1).is_err());
        }
        index.metadata.doc_count += 1;
        assert!(index.try_search_fast(&[1.0, 1.0], 1).is_err());
        assert!(index.try_search_quality(&[1.0, 1.0], 1).is_err());
    }

    #[test]
    fn quality_only_skips_fast_inference_and_initial_page_stays_bounded() {
        for quality_only in [false, true] {
            let config = TwoTierConfig {
                quality_only,
                max_refinement_docs: 3,
                ..config()
            };
            let index = index(&config);
            let (fast, daemon) = providers(vec![1.0, 1.0], false);
            let embedder: Arc<dyn Embedder> = fast.clone();
            let searcher = TwoTierSearcher::new(&index, embedder, Some(daemon), config);
            let phases: Vec<_> = searcher.search("query", 1).collect();
            for phase in &phases {
                match phase {
                    SearchPhase::Initial { results, .. } | SearchPhase::Refined { results, .. } => {
                        assert_eq!(results.len(), 1);
                    }
                    SearchPhase::RefinementFailed { error } => panic!("{error}"),
                }
            }
            assert_eq!(phases.len(), if quality_only { 1 } else { 2 });
            assert_eq!(
                fast.calls.load(AtomicOrdering::SeqCst),
                usize::from(!quality_only)
            );
        }
    }

    #[test]
    fn real_quality_batches_preserve_canonical_side_table_alignment() {
        let config = config();
        let entries = (0..QUALITY_SCORE_BATCH_SIZE + 7)
            .map(|i| entry((1000 - i) as u64, [1.0, 0.0], [1.0, (i % 17) as f32]));
        let index = TwoTierIndex::build("test-fast", "test-quality", &config, entries).unwrap();
        let indices: Vec<_> = (0..index.len()).collect();
        let scores = index.quality_scores_for_indices(&[1.0, 1.0], &indices);
        let mut expected: Vec<_> = indices
            .into_iter()
            .zip(scores)
            .map(|(idx, score)| hit(idx, index.message_id(idx).unwrap(), score))
            .collect();
        expected.sort_by(best_first);
        expected.truncate(19);
        let results = index.try_search_quality(&[1.0, 1.0], 19).unwrap();
        assert_eq!(signatures(&results), signatures(&expected));
    }
}
