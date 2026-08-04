//! Search-quality evaluation harness: qrels-based relevance + trust metrics.
//!
//! Bead: coding_agent_session_search-guided-ops-repro-trust-5u82n.7
//! ("Create search quality evaluation harness with qrels and drift reports").
//!
//! ## Why
//!
//! cass can pass every unit test while *search quality* silently regresses — a
//! ranking change, a trust-scoring tweak, or a query-rewrite edit can quietly
//! stop returning the result an operator expects. Relevance is not something a
//! type checker can prove. This module reduces a small, checked-in set of
//! relevance judgments ([`Qrel`]) plus the results a real search returned
//! ([`ObservedHit`]) into a compact, reviewable [`QualityReport`] — recall@k,
//! precision@k, MRR, latency, realized/fallback mode, and a trust-tier
//! distribution — so a relevance or trust change is *visible* in a diff before
//! release.
//!
//! ## Pure, deterministic, metadata-only (no raw private text)
//!
//! Every function here is pure and does no I/O, so the same inputs always yield
//! the same report — safe to pin in golden tests. The report carries only
//! **metadata**: authored query text, sanitized document refs (a stable id such
//! as a session-file stem — never the conversation body), modes, latencies, and
//! numeric metrics. Document refs are sanitized again at the scoring boundary,
//! so a caller cannot accidentally preserve raw paths, whitespace, or injection
//! punctuation in a report. The real-binary gate additionally plants private
//! body text in its corpus and proves that exact text is absent from both
//! artifacts; [`tests::report_holds_no_session_body`] exercises the boundary
//! sanitizer directly.
//! Distributions use [`BTreeMap`] so the serialized key order is deterministic.
//!
//! ## Shape
//!
//! The harness (an E2E gate or a future robot command) is responsible for the
//! *live* half — seeding a curated corpus, running the real binary, and reducing
//! each hit to an [`ObservedHit`]. This module owns the *pure* half: scoring a
//! single query ([`evaluate`]), assembling the full [`QualityReport`]
//! ([`build_report`]), rendering a human/markdown summary ([`render_markdown`]),
//! and computing a regression/drift diff between two runs ([`diff_reports`]).

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Stable schema version for the search-quality wire format.
pub const SEARCH_QUALITY_SCHEMA_VERSION: u32 = 1;

/// Floating-point tolerance for treating two metric values as equal (used by the
/// pass gate and the regression diff so exact-fraction comparisons are robust).
const METRIC_EPS: f64 = 1e-9;

/// Stable report-safe marker for an observed hit whose document reference has
/// no identifier characters after sanitization. Keeping the slot (instead of
/// dropping it) ensures malformed hits still reduce precision and fail the
/// unexpected-ref gate.
const INVALID_DOC_REF: &str = "__invalid_doc_ref__";

/// One checked-in relevance judgment: a query and the set of document refs that
/// *should* be retrieved within its top-`k`. Authored, reviewable data — the
/// `query` is a search term, never private conversation text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Qrel {
    /// Stable query id (e.g. `q-code-heavy`), used to join runs across reports.
    pub id: String,
    /// The query text to issue.
    pub query: String,
    /// Requested search mode label (`lexical` / `semantic` / `hybrid`), advisory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Document refs expected in the top-`k` (a stable id such as a file stem).
    pub expected_refs: Vec<String>,
    /// Cutoff for recall@k / precision@k.
    pub k: usize,
    /// Optional human note describing the fixture category this judgment covers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One retrieved hit, reduced to metadata-only fields — never the body text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedHit {
    /// 1-indexed rank in the returned result list.
    pub rank: usize,
    /// Stable, sanitized document ref (e.g. session-file stem).
    pub doc_ref: String,
    /// Snake_case trust tier when the search ran with `--robot-meta`; `None`
    /// for the byte-identical fast path that omits the advisory verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_tier: Option<String>,
}

/// A single query's live run: the judgment plus what the real search returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRun {
    /// The judgment this run is evaluated against.
    pub qrel: Qrel,
    /// The reduced, ranked hits the search returned.
    pub observed: Vec<ObservedHit>,
    /// Realized search mode reported in `_meta.search_mode` (advisory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realized_mode: Option<String>,
    /// Fallback tier reported in `_meta.fallback_tier` (e.g. `lexical`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_tier: Option<String>,
    /// Wall-clock latency of the query in milliseconds.
    pub latency_ms: u64,
}

/// The scored outcome for one query: metrics plus the per-query diff (which
/// expected refs were missing, which observed refs were unexpected).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryEvaluation {
    /// Mirrors [`Qrel::id`].
    pub id: String,
    /// The query text that was issued.
    pub query: String,
    /// Requested mode label, echoed from the judgment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_mode: Option<String>,
    /// Realized mode from the live run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realized_mode: Option<String>,
    /// Fallback tier from the live run, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_tier: Option<String>,
    /// The recall/precision cutoff.
    pub k: usize,
    /// Distinct expected refs (sorted for a stable diff).
    pub expected_refs: Vec<String>,
    /// Observed refs in rank order.
    pub observed_refs: Vec<String>,
    /// `|expected ∩ observed[..k]| / |expected|` (`0.0` when nothing is
    /// expected, because an empty judgment is not a meaningful quality gate).
    pub recall_at_k: f64,
    /// `|expected ∩ observed[..k]| / min(k, |observed|)` (`0.0` when no results).
    pub precision_at_k: f64,
    /// `1 / rank` of the first expected ref in the observed list (`0.0` if none).
    pub mrr: f64,
    /// Query latency in milliseconds.
    pub latency_ms: u64,
    /// Expected refs absent from the top-`k` (the actionable diff).
    pub missing_refs: Vec<String>,
    /// Top-`k` observed refs that were not expected.
    pub unexpected_refs: Vec<String>,
    /// True only when the judgment is non-empty and the top-`k` contains every
    /// expected ref with no unexpected refs.
    pub passed: bool,
}

/// Aggregate metrics over every evaluated query in a report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AggregateMetrics {
    /// Number of queries evaluated.
    pub query_count: usize,
    /// Queries whose every expected ref was retrieved within top-`k`.
    pub passed_count: usize,
    /// Queries that missed at least one expected ref.
    pub failed_count: usize,
    /// Mean recall@k across queries (`0.0` for an empty report).
    pub mean_recall_at_k: f64,
    /// Mean precision@k across queries.
    pub mean_precision_at_k: f64,
    /// Mean reciprocal rank across queries.
    pub mean_mrr: f64,
    /// Mean query latency in milliseconds.
    pub mean_latency_ms: f64,
    /// Count of observed hits by trust tier (deterministic key order).
    pub trust_tier_distribution: BTreeMap<String, usize>,
    /// Count of queries by realized search mode (deterministic key order).
    pub realized_mode_counts: BTreeMap<String, usize>,
}

/// The full, reviewable evaluation artifact for one run of the suite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityReport {
    /// Mirrors [`SEARCH_QUALITY_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Optional human label for the run (e.g. a suite name); never private text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Per-query evaluations, in input order.
    pub queries: Vec<QueryEvaluation>,
    /// Aggregate roll-up.
    pub aggregate: AggregateMetrics,
}

/// The per-query delta between a baseline and a current report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryDiff {
    /// The query id (present in both reports).
    pub id: String,
    /// `current.recall_at_k - baseline.recall_at_k`.
    pub recall_delta: f64,
    /// `current.precision_at_k - baseline.precision_at_k`.
    pub precision_delta: f64,
    /// `current.mrr - baseline.mrr`.
    pub mrr_delta: f64,
    /// Refs retrieved in the baseline but missing now (a relevance regression).
    pub newly_missing_refs: Vec<String>,
    /// True when any metric dropped or a previously-found ref went missing.
    pub regressed: bool,
}

/// A regression/drift report between two suite runs, keyed by query id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegressionDiff {
    /// `current.mean_recall_at_k - baseline.mean_recall_at_k`.
    pub mean_recall_delta: f64,
    /// `current.mean_precision_at_k - baseline.mean_precision_at_k`.
    pub mean_precision_delta: f64,
    /// `current.mean_mrr - baseline.mean_mrr`.
    pub mean_mrr_delta: f64,
    /// Per-query deltas for queries present in both reports (sorted by id).
    pub per_query: Vec<QueryDiff>,
    /// Ids only in the baseline (a query was dropped from the current run).
    pub dropped_query_ids: Vec<String>,
    /// Ids only in the current run (a query was added).
    pub added_query_ids: Vec<String>,
    /// Ids of queries that regressed (sorted).
    pub regressed_query_ids: Vec<String>,
    /// True when any query regressed or a query was dropped.
    pub has_regression: bool,
}

/// Keep only characters safe for a structured document ref (alphanumerics and id
/// punctuation). Drops whitespace, path separators, quotes, and anything else, so
/// a ref cannot smuggle raw text, a path, or an injection phrase into the report.
pub fn sanitize_doc_ref(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .take(128)
        .collect()
}

/// Accept only a document reference that is already in canonical report-safe
/// form. Sanitizing a raw path or free-form value in place can preserve its
/// identifying text after merely dropping separators (for example an email
/// address loses `@` but remains recognizable). At the scoring boundary that
/// is not safe enough: callers must supply a canonical identifier, otherwise
/// the report records the invalid-reference sentinel instead of transformed
/// private text.
fn canonical_doc_ref(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let sanitized = sanitize_doc_ref(trimmed);
    (!sanitized.is_empty() && sanitized == trimmed).then_some(sanitized)
}

/// Distinct expected refs as a set of borrowed strs.
fn distinct(refs: &[String]) -> BTreeSet<&str> {
    refs.iter().map(String::as_str).collect()
}

/// Count distinct expected refs present in the first `k` observed refs.
fn relevant_in_topk(expected: &BTreeSet<&str>, observed_ranked: &[String], k: usize) -> usize {
    let mut found: BTreeSet<&str> = BTreeSet::new();
    for r in observed_ranked.iter().take(k) {
        if expected.contains(r.as_str()) {
            found.insert(r.as_str());
        }
    }
    found.len()
}

/// `recall@k = |expected ∩ observed[..k]| / |expected|`. An empty `expected`
/// returns `0.0`: a relevance gate with no authored judgment must not pass
/// vacuously.
pub fn recall_at_k(expected: &[String], observed_ranked: &[String], k: usize) -> f64 {
    let exp = distinct(expected);
    if exp.is_empty() {
        return 0.0;
    }
    relevant_in_topk(&exp, observed_ranked, k) as f64 / exp.len() as f64
}

/// `precision@k = |expected ∩ observed[..k]| / min(k, |observed|)`. Dividing by
/// the number of slots actually considered (rather than `k`) keeps the metric
/// honest on a small curated corpus where fewer than `k` results exist. Returns
/// `0.0` when no results were considered.
pub fn precision_at_k(expected: &[String], observed_ranked: &[String], k: usize) -> f64 {
    let exp = distinct(expected);
    let considered = k.min(observed_ranked.len());
    if considered == 0 {
        return 0.0;
    }
    relevant_in_topk(&exp, observed_ranked, k) as f64 / considered as f64
}

/// `MRR = 1 / rank` of the first expected ref in the observed list (1-indexed),
/// or `0.0` if no expected ref was retrieved.
pub fn reciprocal_rank(expected: &[String], observed_ranked: &[String]) -> f64 {
    let exp = distinct(expected);
    for (idx, r) in observed_ranked.iter().enumerate() {
        if exp.contains(r.as_str()) {
            return 1.0 / (idx as f64 + 1.0);
        }
    }
    0.0
}

/// Observed refs in ascending rank order (ties broken by ref for determinism).
fn observed_refs_ranked(observed: &[ObservedHit]) -> Vec<String> {
    let mut hits: Vec<&ObservedHit> = observed.iter().collect();
    hits.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.doc_ref.cmp(&b.doc_ref)));
    hits.into_iter()
        .map(|hit| canonical_doc_ref(&hit.doc_ref).unwrap_or_else(|| INVALID_DOC_REF.to_string()))
        .collect()
}

/// Score one query run against its judgment. Pure and deterministic.
pub fn evaluate(run: &QueryRun) -> QueryEvaluation {
    let qrel = &run.qrel;
    let expected_refs: Vec<String> = qrel
        .expected_refs
        .iter()
        .filter_map(|doc_ref| canonical_doc_ref(doc_ref))
        .collect();
    let observed_refs = observed_refs_ranked(&run.observed);

    let recall = recall_at_k(&expected_refs, &observed_refs, qrel.k);
    let precision = precision_at_k(&expected_refs, &observed_refs, qrel.k);
    let mrr = reciprocal_rank(&expected_refs, &observed_refs);

    // Distinct, sorted expected refs (BTreeSet iteration is sorted) and the
    // top-k observed set, for a stable per-query diff.
    let exp_set: BTreeSet<&str> = expected_refs.iter().map(String::as_str).collect();
    let topk: BTreeSet<&str> = observed_refs
        .iter()
        .take(qrel.k)
        .map(String::as_str)
        .collect();

    let expected_sorted: Vec<String> = exp_set.iter().map(|r| (*r).to_string()).collect();
    let missing_refs: Vec<String> = exp_set
        .difference(&topk)
        .map(|r| (*r).to_string())
        .collect();
    let unexpected_refs: Vec<String> = topk
        .difference(&exp_set)
        .map(|r| (*r).to_string())
        .collect();

    let passed = !expected_sorted.is_empty()
        && qrel.k > 0
        && (recall - 1.0).abs() < METRIC_EPS
        && (precision - 1.0).abs() < METRIC_EPS
        && unexpected_refs.is_empty();

    QueryEvaluation {
        id: qrel.id.clone(),
        query: qrel.query.clone(),
        requested_mode: qrel.mode.clone(),
        realized_mode: run.realized_mode.clone(),
        fallback_tier: run.fallback_tier.clone(),
        k: qrel.k,
        expected_refs: expected_sorted,
        observed_refs,
        recall_at_k: recall,
        precision_at_k: precision,
        mrr,
        latency_ms: run.latency_ms,
        missing_refs,
        unexpected_refs,
        passed,
    }
}

/// Mean of an `f64` iterator, returning `0.0` for an empty input (never `NaN`).
fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

/// Build the full report from a set of query runs. Pure and deterministic.
pub fn build_report(runs: &[QueryRun]) -> QualityReport {
    build_report_labeled(runs, None)
}

/// Build the full report with an optional run label.
pub fn build_report_labeled(runs: &[QueryRun], label: Option<String>) -> QualityReport {
    let queries: Vec<QueryEvaluation> = runs.iter().map(evaluate).collect();

    let recalls: Vec<f64> = queries.iter().map(|q| q.recall_at_k).collect();
    let precisions: Vec<f64> = queries.iter().map(|q| q.precision_at_k).collect();
    let mrrs: Vec<f64> = queries.iter().map(|q| q.mrr).collect();
    let latencies: Vec<f64> = queries.iter().map(|q| q.latency_ms as f64).collect();

    let passed_count = queries.iter().filter(|q| q.passed).count();

    let mut trust_tier_distribution: BTreeMap<String, usize> = BTreeMap::new();
    for run in runs {
        for hit in &run.observed {
            if let Some(tier) = &hit.trust_tier {
                *trust_tier_distribution.entry(tier.clone()).or_insert(0) += 1;
            }
        }
    }

    let mut realized_mode_counts: BTreeMap<String, usize> = BTreeMap::new();
    for run in runs {
        if let Some(mode) = &run.realized_mode {
            *realized_mode_counts.entry(mode.clone()).or_insert(0) += 1;
        }
    }

    let aggregate = AggregateMetrics {
        query_count: queries.len(),
        passed_count,
        failed_count: queries.len() - passed_count,
        mean_recall_at_k: mean(&recalls),
        mean_precision_at_k: mean(&precisions),
        mean_mrr: mean(&mrrs),
        mean_latency_ms: mean(&latencies),
        trust_tier_distribution,
        realized_mode_counts,
    };

    QualityReport {
        schema_version: SEARCH_QUALITY_SCHEMA_VERSION,
        label,
        queries,
        aggregate,
    }
}

/// Compute the regression/drift diff of `current` against `baseline`, joined by
/// query id. A query regresses when any of recall/precision/MRR drops below its
/// baseline (beyond [`METRIC_EPS`]) or a previously-retrieved ref goes missing.
pub fn diff_reports(baseline: &QualityReport, current: &QualityReport) -> RegressionDiff {
    let base_by_id: BTreeMap<&str, &QueryEvaluation> = baseline
        .queries
        .iter()
        .map(|q| (q.id.as_str(), q))
        .collect();
    let cur_by_id: BTreeMap<&str, &QueryEvaluation> =
        current.queries.iter().map(|q| (q.id.as_str(), q)).collect();

    let dropped_query_ids: Vec<String> = base_by_id
        .keys()
        .filter(|id| !cur_by_id.contains_key(*id))
        .map(|id| (*id).to_string())
        .collect();
    let added_query_ids: Vec<String> = cur_by_id
        .keys()
        .filter(|id| !base_by_id.contains_key(*id))
        .map(|id| (*id).to_string())
        .collect();

    let mut per_query: Vec<QueryDiff> = Vec::new();
    let mut regressed_query_ids: Vec<String> = Vec::new();

    // Iterate baseline ids in sorted order (BTreeMap) for a deterministic diff.
    for (id, base) in &base_by_id {
        let Some(cur) = cur_by_id.get(id) else {
            continue;
        };
        let recall_delta = cur.recall_at_k - base.recall_at_k;
        let precision_delta = cur.precision_at_k - base.precision_at_k;
        let mrr_delta = cur.mrr - base.mrr;

        let base_expected: BTreeSet<&str> = base.expected_refs.iter().map(String::as_str).collect();
        let cur_expected: BTreeSet<&str> = cur.expected_refs.iter().map(String::as_str).collect();
        let base_found: BTreeSet<&str> = base
            .observed_refs
            .iter()
            .take(base.k)
            .map(String::as_str)
            .filter(|r| base_expected.contains(*r))
            .collect();
        let cur_found: BTreeSet<&str> = cur
            .observed_refs
            .iter()
            .take(cur.k)
            .map(String::as_str)
            .filter(|r| cur_expected.contains(*r))
            .collect();
        let newly_missing_refs: Vec<String> = base_found
            .difference(&cur_found)
            .map(|r| (*r).to_string())
            .collect();

        let regressed = recall_delta < -METRIC_EPS
            || precision_delta < -METRIC_EPS
            || mrr_delta < -METRIC_EPS
            || !newly_missing_refs.is_empty();
        if regressed {
            regressed_query_ids.push((*id).to_string());
        }

        per_query.push(QueryDiff {
            id: (*id).to_string(),
            recall_delta,
            precision_delta,
            mrr_delta,
            newly_missing_refs,
            regressed,
        });
    }

    let has_regression = !regressed_query_ids.is_empty() || !dropped_query_ids.is_empty();

    RegressionDiff {
        mean_recall_delta: current.aggregate.mean_recall_at_k - baseline.aggregate.mean_recall_at_k,
        mean_precision_delta: current.aggregate.mean_precision_at_k
            - baseline.aggregate.mean_precision_at_k,
        mean_mrr_delta: current.aggregate.mean_mrr - baseline.aggregate.mean_mrr,
        per_query,
        dropped_query_ids,
        added_query_ids,
        regressed_query_ids,
        has_regression,
    }
}

/// A compact, joined representation of an optional string for a markdown cell.
fn cell(opt: &Option<String>) -> &str {
    opt.as_deref().unwrap_or("—")
}

/// Render a deterministic markdown summary of a [`QualityReport`]. The output
/// carries only metadata (query text, refs, modes, numbers) — never body text.
pub fn render_markdown(report: &QualityReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Search Quality Report");
    let _ = writeln!(out);
    if let Some(label) = &report.label {
        let _ = writeln!(out, "**Suite:** {label}");
        let _ = writeln!(out);
    }
    let agg = &report.aggregate;
    let _ = writeln!(out, "**Schema version:** {}", report.schema_version);
    let _ = writeln!(
        out,
        "**Queries:** {} ({} passed, {} failed)",
        agg.query_count, agg.passed_count, agg.failed_count
    );
    let _ = writeln!(out);

    let _ = writeln!(out, "## Aggregate");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Metric | Value |");
    let _ = writeln!(out, "| --- | --- |");
    let _ = writeln!(out, "| mean recall@k | {:.4} |", agg.mean_recall_at_k);
    let _ = writeln!(out, "| mean precision@k | {:.4} |", agg.mean_precision_at_k);
    let _ = writeln!(out, "| mean MRR | {:.4} |", agg.mean_mrr);
    let _ = writeln!(out, "| mean latency (ms) | {:.1} |", agg.mean_latency_ms);
    let _ = writeln!(out);

    let _ = writeln!(out, "## Trust-tier distribution");
    let _ = writeln!(out);
    if agg.trust_tier_distribution.is_empty() {
        let _ = writeln!(out, "_no trust verdicts observed_");
    } else {
        let _ = writeln!(out, "| Tier | Count |");
        let _ = writeln!(out, "| --- | --- |");
        for (tier, count) in &agg.trust_tier_distribution {
            let _ = writeln!(out, "| {tier} | {count} |");
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Realized search mode");
    let _ = writeln!(out);
    if agg.realized_mode_counts.is_empty() {
        let _ = writeln!(out, "_not reported_");
    } else {
        let _ = writeln!(out, "| Mode | Queries |");
        let _ = writeln!(out, "| --- | --- |");
        for (mode, count) in &agg.realized_mode_counts {
            let _ = writeln!(out, "| {mode} | {count} |");
        }
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "## Per-query");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| id | query | mode | recall@k | precision@k | MRR | latency_ms | missing | status |"
    );
    let _ = writeln!(
        out,
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- |"
    );
    for q in &report.queries {
        let missing = if q.missing_refs.is_empty() {
            "—".to_string()
        } else {
            q.missing_refs.join(",")
        };
        let status = if q.passed { "pass" } else { "FAIL" };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {:.4} | {:.4} | {:.4} | {} | {} | {} |",
            q.id,
            q.query,
            cell(&q.realized_mode),
            q.recall_at_k,
            q.precision_at_k,
            q.mrr,
            q.latency_ms,
            missing,
            status
        );
    }
    out
}

/// Render a deterministic markdown summary of a [`RegressionDiff`] (the drift
/// report). Carries only metadata — ids, refs, and numeric deltas.
pub fn render_diff_markdown(diff: &RegressionDiff) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Search Quality Drift");
    let _ = writeln!(out);
    let verdict = if diff.has_regression {
        "REGRESSION"
    } else {
        "no regression"
    };
    let _ = writeln!(out, "**Verdict:** {verdict}");
    let _ = writeln!(
        out,
        "**Mean deltas:** recall {:+.4}, precision {:+.4}, MRR {:+.4}",
        diff.mean_recall_delta, diff.mean_precision_delta, diff.mean_mrr_delta
    );
    if !diff.dropped_query_ids.is_empty() {
        let _ = writeln!(
            out,
            "**Dropped queries:** {}",
            diff.dropped_query_ids.join(",")
        );
    }
    if !diff.added_query_ids.is_empty() {
        let _ = writeln!(out, "**Added queries:** {}", diff.added_query_ids.join(","));
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| id | Δrecall | Δprecision | ΔMRR | newly_missing | regressed |"
    );
    let _ = writeln!(out, "| --- | --- | --- | --- | --- | --- |");
    for q in &diff.per_query {
        let missing = if q.newly_missing_refs.is_empty() {
            "—".to_string()
        } else {
            q.newly_missing_refs.join(",")
        };
        let _ = writeln!(
            out,
            "| {} | {:+.4} | {:+.4} | {:+.4} | {} | {} |",
            q.id,
            q.recall_delta,
            q.precision_delta,
            q.mrr_delta,
            missing,
            if q.regressed { "yes" } else { "no" }
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// Compare two metric values within tolerance (avoids brittle float `==`).
    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn hit(rank: usize, doc_ref: &str, tier: Option<&str>) -> ObservedHit {
        ObservedHit {
            rank,
            doc_ref: doc_ref.to_string(),
            trust_tier: tier.map(str::to_string),
        }
    }

    fn qrel(id: &str, query: &str, expected: &[&str], k: usize) -> Qrel {
        Qrel {
            id: id.to_string(),
            query: query.to_string(),
            mode: Some("hybrid".to_string()),
            expected_refs: refs(expected),
            k,
            note: None,
        }
    }

    // ---- metric math --------------------------------------------------------

    #[test]
    fn recall_perfect_partial_zero_and_empty_judgment() {
        assert!(approx(
            recall_at_k(&refs(&["a", "b"]), &refs(&["a", "b", "c"]), 5),
            1.0
        ));
        assert!(approx(
            recall_at_k(&refs(&["a", "b"]), &refs(&["a", "c", "b"]), 2),
            0.5
        ));
        assert!(approx(
            recall_at_k(&refs(&["x"]), &refs(&["a", "b"]), 5),
            0.0
        ));
        // No authored expectation is an invalid gate, not a free pass.
        assert!(approx(recall_at_k(&[], &refs(&["a"]), 5), 0.0));
    }

    #[test]
    fn precision_divides_by_slots_considered() {
        // 2 relevant of 3 considered → 2/3.
        assert!(approx(
            precision_at_k(&refs(&["a", "b"]), &refs(&["a", "b", "c"]), 5),
            2.0 / 3.0
        ));
        // 1 relevant of min(2,3)=2 considered → 1/2.
        assert!(approx(
            precision_at_k(&refs(&["a", "b"]), &refs(&["a", "c", "b"]), 2),
            0.5
        ));
        // No results considered → 0.0, never NaN.
        assert!(approx(precision_at_k(&refs(&["a"]), &[], 5), 0.0));
    }

    #[test]
    fn mrr_uses_first_relevant_rank() {
        assert!(approx(
            reciprocal_rank(&refs(&["a"]), &refs(&["a", "b"])),
            1.0
        ));
        assert!(approx(
            reciprocal_rank(&refs(&["b"]), &refs(&["a", "b"])),
            0.5
        ));
        assert!(approx(
            reciprocal_rank(&refs(&["z"]), &refs(&["a", "b"])),
            0.0
        ));
    }

    // ---- per-query evaluation ----------------------------------------------

    #[test]
    fn evaluate_reports_missing_and_unexpected_diff() {
        let run = QueryRun {
            qrel: qrel("q1", "foo", &["a", "b"], 2),
            observed: vec![hit(1, "a", Some("unverified")), hit(2, "c", Some("stale"))],
            realized_mode: Some("hybrid".to_string()),
            fallback_tier: None,
            latency_ms: 12,
        };
        let e = evaluate(&run);
        assert!(approx(e.recall_at_k, 0.5));
        assert_eq!(e.missing_refs, refs(&["b"]));
        assert_eq!(e.unexpected_refs, refs(&["c"]));
        assert!(!e.passed, "one expected ref missing → not passed");
        // Observed refs are returned in rank order.
        assert_eq!(e.observed_refs, refs(&["a", "c"]));
    }

    #[test]
    fn evaluate_full_recall_passes() {
        let run = QueryRun {
            qrel: qrel("q2", "bar", &["a", "b"], 5),
            observed: vec![hit(1, "a", None), hit(2, "b", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 3,
        };
        let e = evaluate(&run);
        assert!(approx(e.recall_at_k, 1.0));
        assert!(e.passed);
        assert!(e.missing_refs.is_empty());
    }

    #[test]
    fn evaluate_rejects_unexpected_refs_despite_full_recall() {
        let run = QueryRun {
            qrel: qrel("q-precision", "bar", &["a"], 5),
            observed: vec![hit(1, "a", None), hit(2, "spurious", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 3,
        };
        let evaluation = evaluate(&run);
        assert!(approx(evaluation.recall_at_k, 1.0));
        assert!(evaluation.precision_at_k < 1.0);
        assert_eq!(evaluation.unexpected_refs, refs(&["spurious"]));
        assert!(
            !evaluation.passed,
            "full recall must not hide false positives"
        );
    }

    #[test]
    fn evaluate_keeps_invalid_observed_ref_as_unexpected_slot() {
        let run = QueryRun {
            qrel: qrel("q-invalid-ref", "bar", &["a"], 5),
            observed: vec![hit(1, "a", None), hit(2, "///", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 3,
        };
        let evaluation = evaluate(&run);
        assert_eq!(evaluation.unexpected_refs, refs(&[INVALID_DOC_REF]));
        assert!(evaluation.precision_at_k < 1.0);
        assert!(!evaluation.passed);
    }

    #[test]
    fn evaluate_rejects_empty_expected_refs() {
        let run = QueryRun {
            qrel: qrel("q-empty", "bar", &[], 5),
            observed: Vec::new(),
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 3,
        };
        let evaluation = evaluate(&run);
        assert!(approx(evaluation.recall_at_k, 0.0));
        assert!(!evaluation.passed, "empty qrels must never pass vacuously");
    }

    #[test]
    fn evaluate_rejects_noncanonical_expected_refs_without_echoing_them() {
        let private = "/private/session/private.user@example.invalid";
        let run = QueryRun {
            qrel: qrel("q-private-expected", "bar", &[private], 5),
            observed: vec![hit(1, private, None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 3,
        };
        let evaluation = evaluate(&run);
        assert!(evaluation.expected_refs.is_empty());
        assert_eq!(evaluation.observed_refs, refs(&[INVALID_DOC_REF]));
        assert!(!evaluation.passed);
        let encoded = serde_json::to_string(&evaluation).unwrap();
        assert!(!encoded.contains("private.user"));
    }

    #[test]
    fn evaluate_sorts_observed_by_rank() {
        // Hits supplied out of order are scored in rank order.
        let run = QueryRun {
            qrel: qrel("q3", "baz", &["b"], 1),
            observed: vec![hit(2, "x", None), hit(1, "b", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 1,
        };
        let e = evaluate(&run);
        assert_eq!(e.observed_refs, refs(&["b", "x"]));
        // `b` is rank 1 → MRR 1.0 and recall@1 = 1.0.
        assert!(approx(e.mrr, 1.0));
        assert!(approx(e.recall_at_k, 1.0));
    }

    // ---- aggregation --------------------------------------------------------

    #[test]
    fn build_report_aggregates_means_and_distributions() {
        let runs = vec![
            QueryRun {
                qrel: qrel("a", "qa", &["d1"], 5),
                observed: vec![hit(1, "d1", Some("unverified"))],
                realized_mode: Some("hybrid".to_string()),
                fallback_tier: None,
                latency_ms: 10,
            },
            QueryRun {
                qrel: qrel("b", "qb", &["d2", "d3"], 5),
                observed: vec![
                    hit(1, "d3", Some("stale")),
                    hit(2, "d2", Some("unverified")),
                ],
                realized_mode: Some("hybrid".to_string()),
                fallback_tier: Some("lexical".to_string()),
                latency_ms: 20,
            },
        ];
        let report = build_report(&runs);
        assert_eq!(report.aggregate.query_count, 2);
        assert_eq!(report.aggregate.passed_count, 2);
        assert!(approx(report.aggregate.mean_recall_at_k, 1.0));
        assert!(approx(report.aggregate.mean_precision_at_k, 1.0));
        assert!(approx(report.aggregate.mean_latency_ms, 15.0));
        // Trust tiers counted across all observed hits.
        assert_eq!(
            report.aggregate.trust_tier_distribution.get("unverified"),
            Some(&2)
        );
        assert_eq!(
            report.aggregate.trust_tier_distribution.get("stale"),
            Some(&1)
        );
        assert_eq!(
            report.aggregate.realized_mode_counts.get("hybrid"),
            Some(&2)
        );
    }

    #[test]
    fn empty_report_has_zero_means_not_nan() {
        let report = build_report(&[]);
        assert_eq!(report.aggregate.query_count, 0);
        assert!(report.aggregate.mean_recall_at_k.is_finite());
        assert!(approx(report.aggregate.mean_recall_at_k, 0.0));
        assert!(approx(report.aggregate.mean_mrr, 0.0));
    }

    #[test]
    fn build_report_is_deterministic() {
        let runs = vec![QueryRun {
            qrel: qrel("a", "qa", &["d1", "d2"], 5),
            observed: vec![
                hit(1, "d1", Some("unverified")),
                hit(2, "d2", Some("stale")),
            ],
            realized_mode: Some("hybrid".to_string()),
            fallback_tier: None,
            latency_ms: 7,
        }];
        let a = serde_json::to_string(&build_report(&runs)).unwrap();
        let b = serde_json::to_string(&build_report(&runs)).unwrap();
        assert_eq!(a, b, "same input must serialize identically");
    }

    #[test]
    fn report_round_trips_through_json() {
        let runs = vec![QueryRun {
            qrel: qrel("a", "qa", &["d1"], 5),
            observed: vec![hit(1, "d1", Some("unverified"))],
            realized_mode: Some("hybrid".to_string()),
            fallback_tier: None,
            latency_ms: 5,
        }];
        let report = build_report(&runs);
        let json = serde_json::to_value(&report).unwrap();
        let back: QualityReport = serde_json::from_value(json).unwrap();
        assert_eq!(back, report);
    }

    // ---- regression / drift -------------------------------------------------

    #[test]
    fn diff_flags_recall_regression_and_newly_missing() {
        let baseline = build_report(&[QueryRun {
            qrel: qrel("a", "qa", &["d1", "d2"], 5),
            observed: vec![hit(1, "d1", None), hit(2, "d2", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 1,
        }]);
        // Current loses d2.
        let current = build_report(&[QueryRun {
            qrel: qrel("a", "qa", &["d1", "d2"], 5),
            observed: vec![hit(1, "d1", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 1,
        }]);
        let diff = diff_reports(&baseline, &current);
        assert!(diff.has_regression);
        assert_eq!(diff.regressed_query_ids, refs(&["a"]));
        assert_eq!(diff.per_query[0].newly_missing_refs, refs(&["d2"]));
        assert!(diff.mean_recall_delta < 0.0);
    }

    #[test]
    fn diff_no_regression_when_improved_or_equal() {
        let baseline = build_report(&[QueryRun {
            qrel: qrel("a", "qa", &["d1", "d2"], 5),
            observed: vec![hit(1, "d1", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 1,
        }]);
        let current = build_report(&[QueryRun {
            qrel: qrel("a", "qa", &["d1", "d2"], 5),
            observed: vec![hit(1, "d1", None), hit(2, "d2", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 1,
        }]);
        let diff = diff_reports(&baseline, &current);
        assert!(!diff.has_regression);
        assert!(diff.regressed_query_ids.is_empty());
        assert!(diff.mean_recall_delta > 0.0);
    }

    #[test]
    fn diff_reports_dropped_and_added_queries() {
        let baseline = build_report(&[QueryRun {
            qrel: qrel("a", "qa", &["d1"], 5),
            observed: vec![hit(1, "d1", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 1,
        }]);
        let current = build_report(&[QueryRun {
            qrel: qrel("b", "qb", &["d2"], 5),
            observed: vec![hit(1, "d2", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 1,
        }]);
        let diff = diff_reports(&baseline, &current);
        assert_eq!(diff.dropped_query_ids, refs(&["a"]));
        assert_eq!(diff.added_query_ids, refs(&["b"]));
        // A dropped query counts as a regression (coverage shrank).
        assert!(diff.has_regression);
    }

    // ---- rendering + redaction ---------------------------------------------

    #[test]
    fn render_markdown_is_deterministic_and_has_sections() {
        let runs = vec![QueryRun {
            qrel: qrel("a", "qa", &["d1"], 5),
            observed: vec![hit(1, "d1", Some("unverified"))],
            realized_mode: Some("hybrid".to_string()),
            fallback_tier: None,
            latency_ms: 9,
        }];
        let report = build_report(&runs);
        let a = render_markdown(&report);
        let b = render_markdown(&report);
        assert_eq!(a, b);
        assert!(a.contains("# Search Quality Report"));
        assert!(a.contains("## Aggregate"));
        assert!(a.contains("## Trust-tier distribution"));
        assert!(a.contains("## Per-query"));
    }

    /// Even a caller that violates the metadata contract cannot preserve a raw
    /// path/email-shaped value, including a recognizable separator-stripped
    /// transformation, in the report's document-ref fields.
    #[test]
    fn report_holds_no_session_body() {
        let private = "private.user@example.invalid";
        let runs = vec![QueryRun {
            qrel: qrel("p", "privacytopic", &["privacydoc"], 5),
            // Deliberately violate the harness contract by passing a path-like
            // raw value at the metadata boundary. The scoring core must still
            // sanitize it before the report is assembled.
            observed: vec![hit(
                1,
                &format!("/private/session/{private}"),
                Some("unverified"),
            )],
            realized_mode: Some("hybrid".to_string()),
            fallback_tier: None,
            latency_ms: 4,
        }];
        let report = build_report(&runs);
        let json = serde_json::to_string(&report).unwrap();
        let md = render_markdown(&report);
        assert_eq!(
            report.queries[0].observed_refs,
            refs(&[INVALID_DOC_REF]),
            "non-canonical metadata must become the stable invalid-ref sentinel"
        );
        assert!(
            !json.contains(private),
            "JSON report must not leak body text"
        );
        assert!(
            !json.contains("private.userexample.invalid"),
            "JSON report must not leak a separator-stripped private marker"
        );
        assert!(
            !md.contains(private),
            "markdown report must not leak body text"
        );
    }

    #[test]
    fn sanitize_doc_ref_drops_paths_and_whitespace() {
        let dirty = "/home/alice/rollout-foo bar.jsonl 'or'1=1";
        let clean = sanitize_doc_ref(dirty);
        assert!(!clean.contains('/'), "no path separators: {clean}");
        assert!(!clean.contains(' '), "no whitespace: {clean}");
        assert!(!clean.contains('\''), "no quotes: {clean}");
        assert!(
            clean
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')),
            "only id-safe chars: {clean}"
        );
    }

    #[test]
    fn render_diff_markdown_marks_regression() {
        let baseline = build_report(&[QueryRun {
            qrel: qrel("a", "qa", &["d1", "d2"], 5),
            observed: vec![hit(1, "d1", None), hit(2, "d2", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 1,
        }]);
        let current = build_report(&[QueryRun {
            qrel: qrel("a", "qa", &["d1", "d2"], 5),
            observed: vec![hit(1, "d1", None)],
            realized_mode: None,
            fallback_tier: None,
            latency_ms: 1,
        }]);
        let diff = diff_reports(&baseline, &current);
        let md = render_diff_markdown(&diff);
        assert!(md.contains("REGRESSION"));
        assert!(md.contains("d2"));
    }
}
