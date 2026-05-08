//! Deterministic answer-pack evidence selection.
//!
//! This module is intentionally independent of the CLI. `cass pack` will wire it
//! to search, source health, renderers, and robot docs in later beads.

use std::cmp::Ordering;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::query::SearchHit;

const TOKEN_ESTIMATE_CHARS_PER_TOKEN: usize = 4;
const DEFAULT_FRESHNESS_WINDOW_SECONDS: i64 = 30 * 24 * 60 * 60;
const PACK_CANDIDATE_LIMIT_CAP: usize = 2_048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackPlannerLimits {
    pub max_tokens: usize,
    pub max_sessions: usize,
    pub max_evidence: usize,
    pub context_lines: usize,
    pub max_excerpt_chars: usize,
}

impl Default for PackPlannerLimits {
    fn default() -> Self {
        Self {
            max_tokens: 12_000,
            max_sessions: 8,
            max_evidence: 24,
            context_lines: 3,
            max_excerpt_chars: 1_600,
        }
    }
}

impl PackPlannerLimits {
    pub fn validate(&self) -> Result<(), PackPlannerLimitError> {
        validate_range("max_tokens", self.max_tokens, 1_024, 200_000)?;
        validate_range("max_sessions", self.max_sessions, 1, 64)?;
        validate_range("max_evidence", self.max_evidence, 1, 256)?;
        validate_range("context_lines", self.context_lines, 0, 40)?;
        validate_range("max_excerpt_chars", self.max_excerpt_chars, 80, 8_000)?;
        Ok(())
    }
}

fn validate_range(
    field: &'static str,
    value: usize,
    min: usize,
    max: usize,
) -> Result<(), PackPlannerLimitError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(PackPlannerLimitError {
            field,
            value,
            min,
            max,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackPlannerLimitError {
    pub field: &'static str,
    pub value: usize,
    pub min: usize,
    pub max: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackFreshnessPolicy {
    #[default]
    PreferRecent,
    Strict,
    AllowStale,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackEvidenceRole {
    AssistantConclusion,
    ToolResult,
    UserRequirement,
    ToolCallArgument,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackSourceReadiness {
    #[default]
    Healthy,
    StaleReadable,
    IncompleteMetadata,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackCandidate {
    pub candidate_id: String,
    pub source_path: String,
    pub source_id: String,
    pub origin_kind: String,
    pub origin_host: Option<String>,
    pub workspace: String,
    pub workspace_original: Option<String>,
    pub agent: String,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
    pub conversation_id: Option<i64>,
    pub message_index: Option<usize>,
    pub content_hash: String,
    pub span_hash: String,
    pub created_at_ms: Option<i64>,
    pub indexed_at_ms: Option<i64>,
    pub match_type: String,
    pub excerpt: String,
    pub role: PackEvidenceRole,
    pub lexical_score: Option<f64>,
    pub semantic_score: Option<f64>,
    pub hybrid_rank: Option<usize>,
    pub matched_terms: Vec<String>,
    pub matched_phrases: Vec<String>,
    pub query_term_count: usize,
    pub query_phrase_count: usize,
    pub source_readiness: PackSourceReadiness,
    pub source_explicitly_requested: bool,
}

impl PackCandidate {
    pub fn from_search_hit(
        hit: &SearchHit,
        query_term_count: usize,
        query_phrase_count: usize,
    ) -> Self {
        let line_start = hit.line_number;
        let source_id = if hit.source_id.trim().is_empty() {
            "local".to_string()
        } else {
            hit.source_id.trim().to_string()
        };
        let origin_kind = if hit.origin_kind.trim().is_empty() {
            "local".to_string()
        } else {
            hit.origin_kind.trim().to_string()
        };
        let content_hash = format!("{:016x}", hit.content_hash);
        let candidate_id = format!(
            "{}:{}:{}",
            source_id,
            hit.source_path,
            line_start.unwrap_or_default()
        );
        Self {
            candidate_id,
            source_path: hit.source_path.clone(),
            source_id,
            origin_kind,
            origin_host: hit.origin_host.clone(),
            workspace: hit.workspace.clone(),
            workspace_original: hit.workspace_original.clone(),
            agent: hit.agent.clone(),
            line_start,
            line_end: line_start,
            conversation_id: hit.conversation_id,
            message_index: None,
            content_hash: content_hash.clone(),
            span_hash: content_hash,
            created_at_ms: hit.created_at,
            indexed_at_ms: None,
            match_type: format!("{:?}", hit.match_type),
            excerpt: if hit.content.is_empty() {
                hit.snippet.clone()
            } else {
                hit.content.clone()
            },
            role: PackEvidenceRole::Unknown,
            lexical_score: Some(hit.score as f64),
            semantic_score: None,
            hybrid_rank: None,
            matched_terms: Vec::new(),
            matched_phrases: Vec::new(),
            query_term_count,
            query_phrase_count,
            source_readiness: PackSourceReadiness::Healthy,
            source_explicitly_requested: false,
        }
    }

    fn session_key(&self) -> (&str, &str) {
        (&self.source_id, &self.source_path)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackPlanRequest {
    pub now_ms: i64,
    pub limits: PackPlannerLimits,
    pub freshness_policy: PackFreshnessPolicy,
    pub freshness_window_seconds: i64,
    pub candidates: Vec<PackCandidate>,
    pub explain_selection: bool,
}

impl Default for PackPlanRequest {
    fn default() -> Self {
        Self {
            now_ms: 0,
            limits: PackPlannerLimits::default(),
            freshness_policy: PackFreshnessPolicy::PreferRecent,
            freshness_window_seconds: DEFAULT_FRESHNESS_WINDOW_SECONDS,
            candidates: Vec::new(),
            explain_selection: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedAnswerPack {
    pub candidate_count: usize,
    pub selected_evidence_count: usize,
    pub selected_session_count: usize,
    pub estimated_tokens: usize,
    pub diagnostics: PackPlannerDiagnostics,
    pub evidence: Vec<PlannedPackEvidence>,
    pub omitted: Vec<OmittedPackCandidate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackPlannerDiagnostics {
    pub candidate_fetch_limit: usize,
    pub budget: PackPlannerBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackPlannerBudget {
    pub max_tokens: usize,
    pub metadata_tokens: usize,
    pub outline_tokens: usize,
    pub evidence_tokens: usize,
    pub omitted_tokens: usize,
    pub max_output_tokens_with_overflow: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedPackEvidence {
    pub id: String,
    pub rank: usize,
    pub excerpt: String,
    pub excerpt_truncated: bool,
    pub estimated_tokens: usize,
    pub candidate: PackCandidate,
    pub selection: PackSelectionScore,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OmittedPackCandidate {
    pub candidate_id: String,
    pub source_path: String,
    pub line_start: Option<usize>,
    pub agent: String,
    pub reason: PackOmittedReason,
    pub score: f64,
    pub estimated_tokens: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackOmittedReason {
    TokenBudgetExhausted,
    MaxSessionsReached,
    MaxEvidenceReached,
    DuplicateContent,
    SameSessionLowerRank,
    StaleUnderStrictPolicy,
    SourceUnavailable,
    RedactedToEmpty,
    FieldMaskExcluded,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PackSelectionScore {
    pub score: f64,
    pub relevance_score: f64,
    pub coverage_score: f64,
    pub freshness_score: f64,
    pub source_diversity_score: f64,
    pub source_authority_score: f64,
    pub role_score: f64,
    pub citation_quality_score: f64,
    pub duplicate_penalty: f64,
    pub token_cost: usize,
    pub selected_reason: PackSelectedReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackSelectedReason {
    HighRelevance,
    FreshEvidence,
    SourceDiversity,
    StrongCitation,
    BudgetFit,
}

pub fn pack_candidate_fetch_limit(
    limits: &PackPlannerLimits,
) -> Result<usize, PackPlannerLimitError> {
    limits.validate()?;
    Ok(limits
        .max_evidence
        .saturating_mul(8)
        .max(limits.max_sessions.saturating_mul(16))
        .clamp(64, PACK_CANDIDATE_LIMIT_CAP))
}

pub fn pack_planner_budget(
    limits: &PackPlannerLimits,
) -> Result<PackPlannerBudget, PackPlannerLimitError> {
    limits.validate()?;
    Ok(pack_planner_budget_unchecked(limits.max_tokens))
}

fn pack_planner_budget_unchecked(max_tokens: usize) -> PackPlannerBudget {
    let metadata_tokens = percent_tokens(max_tokens, 15);
    let outline_tokens = percent_tokens(max_tokens, 15);
    let evidence_tokens = percent_tokens(max_tokens, 60);
    let omitted_tokens = max_tokens
        .saturating_sub(metadata_tokens)
        .saturating_sub(outline_tokens)
        .saturating_sub(evidence_tokens);
    PackPlannerBudget {
        max_tokens,
        metadata_tokens,
        outline_tokens,
        evidence_tokens,
        omitted_tokens,
        max_output_tokens_with_overflow: max_tokens.saturating_add(max_tokens / 20),
    }
}

fn percent_tokens(max_tokens: usize, percent: usize) -> usize {
    max_tokens.saturating_mul(percent) / 100
}

#[derive(Debug, Clone)]
struct ScoredCandidate {
    index: usize,
    score: PackSelectionScore,
    excerpt: String,
    excerpt_truncated: bool,
}

#[derive(Debug, Default)]
struct SelectedState {
    source_ids: HashSet<String>,
    sessions: HashSet<(String, String)>,
    span_hashes: HashSet<String>,
    content_hashes: HashSet<String>,
    ranges: Vec<(String, Option<usize>, Option<usize>)>,
}

pub fn plan_answer_pack(
    request: PackPlanRequest,
) -> Result<PlannedAnswerPack, PackPlannerLimitError> {
    request.limits.validate()?;

    let candidate_count = request.candidates.len();
    let diagnostics = PackPlannerDiagnostics {
        candidate_fetch_limit: pack_candidate_fetch_limit(&request.limits)?,
        budget: pack_planner_budget_unchecked(request.limits.max_tokens),
    };
    let lexical_range = ScoreRange::from_values(
        request
            .candidates
            .iter()
            .filter_map(|candidate| finite_score(candidate.lexical_score)),
    );
    let semantic_range = ScoreRange::from_values(
        request
            .candidates
            .iter()
            .filter_map(|candidate| finite_score(candidate.semantic_score)),
    );

    let mut remaining: Vec<usize> = (0..request.candidates.len()).collect();
    let mut selected = Vec::new();
    let mut omitted = Vec::new();
    let mut selected_state = SelectedState::default();
    let mut used_tokens = 0usize;

    while !remaining.is_empty() && selected.len() < request.limits.max_evidence {
        let mut best: Option<ScoredCandidate> = None;
        let mut next_remaining = Vec::with_capacity(remaining.len());

        for candidate_index in remaining.iter().copied() {
            let candidate = &request.candidates[candidate_index];
            if let Some(reason) = hard_omission_reason(candidate, &request, &selected_state) {
                let score = score_candidate(
                    candidate,
                    &request,
                    &selected_state,
                    lexical_range,
                    semantic_range,
                    0,
                );
                omitted.push(omitted_candidate(candidate, reason, score));
                continue;
            }

            let (excerpt, excerpt_truncated) =
                truncate_excerpt(&candidate.excerpt, request.limits.max_excerpt_chars);
            if excerpt.trim().is_empty() {
                let score = score_candidate(
                    candidate,
                    &request,
                    &selected_state,
                    lexical_range,
                    semantic_range,
                    0,
                );
                omitted.push(omitted_candidate(
                    candidate,
                    PackOmittedReason::RedactedToEmpty,
                    score,
                ));
                continue;
            }

            next_remaining.push(candidate_index);
            let token_cost = estimated_tokens(&excerpt);
            let score = score_candidate(
                candidate,
                &request,
                &selected_state,
                lexical_range,
                semantic_range,
                token_cost,
            );
            let scored = ScoredCandidate {
                index: candidate_index,
                score,
                excerpt,
                excerpt_truncated,
            };

            if best.as_ref().is_none_or(|current| {
                candidate_ordering(
                    &scored,
                    &request.candidates[scored.index],
                    current,
                    &request.candidates[current.index],
                )
                .is_lt()
            }) {
                best = Some(scored);
            }
        }

        let Some(best_candidate) = best else {
            remaining = next_remaining;
            break;
        };

        next_remaining.retain(|candidate_index| *candidate_index != best_candidate.index);
        remaining = next_remaining;
        let candidate = &request.candidates[best_candidate.index];

        if used_tokens.saturating_add(best_candidate.score.token_cost)
            > diagnostics.budget.evidence_tokens
        {
            omitted.push(omitted_candidate(
                candidate,
                PackOmittedReason::TokenBudgetExhausted,
                best_candidate.score,
            ));
            continue;
        }

        let session_key = candidate.session_key();
        if !selected_state
            .sessions
            .contains(&(session_key.0.to_string(), session_key.1.to_string()))
            && selected_state.sessions.len() >= request.limits.max_sessions
        {
            omitted.push(omitted_candidate(
                candidate,
                PackOmittedReason::MaxSessionsReached,
                best_candidate.score,
            ));
            continue;
        }

        used_tokens = used_tokens.saturating_add(best_candidate.score.token_cost);
        selected_state
            .source_ids
            .insert(candidate.source_id.clone());
        selected_state
            .sessions
            .insert((candidate.source_id.clone(), candidate.source_path.clone()));
        selected_state
            .span_hashes
            .insert(candidate.span_hash.clone());
        selected_state
            .content_hashes
            .insert(candidate.content_hash.clone());
        selected_state.ranges.push((
            candidate.source_path.clone(),
            candidate.line_start,
            candidate.line_end,
        ));

        selected.push(PlannedPackEvidence {
            id: evidence_id(candidate),
            rank: selected.len() + 1,
            excerpt: best_candidate.excerpt,
            excerpt_truncated: best_candidate.excerpt_truncated,
            estimated_tokens: best_candidate.score.token_cost,
            candidate: candidate.clone(),
            selection: best_candidate.score,
        });
    }

    for candidate_index in remaining {
        let candidate = &request.candidates[candidate_index];
        let score = score_candidate(
            candidate,
            &request,
            &selected_state,
            lexical_range,
            semantic_range,
            estimated_tokens(&candidate.excerpt),
        );
        omitted.push(omitted_candidate(
            candidate,
            PackOmittedReason::MaxEvidenceReached,
            score,
        ));
    }

    Ok(PlannedAnswerPack {
        candidate_count,
        selected_evidence_count: selected.len(),
        selected_session_count: selected_state.sessions.len(),
        estimated_tokens: used_tokens,
        diagnostics,
        evidence: selected,
        omitted,
    })
}

fn hard_omission_reason(
    candidate: &PackCandidate,
    request: &PackPlanRequest,
    selected_state: &SelectedState,
) -> Option<PackOmittedReason> {
    if matches!(candidate.source_readiness, PackSourceReadiness::Unavailable) {
        return Some(PackOmittedReason::SourceUnavailable);
    }
    if is_stale_under_strict_policy(candidate, request) {
        return Some(PackOmittedReason::StaleUnderStrictPolicy);
    }
    if selected_state.span_hashes.contains(&candidate.span_hash)
        || selected_state
            .content_hashes
            .contains(&candidate.content_hash)
        || selected_state
            .ranges
            .iter()
            .any(|(source_path, start, end)| {
                source_path == &candidate.source_path
                    && line_ranges_overlap(*start, *end, candidate.line_start, candidate.line_end)
            })
    {
        return Some(PackOmittedReason::DuplicateContent);
    }
    None
}

fn is_stale_under_strict_policy(candidate: &PackCandidate, request: &PackPlanRequest) -> bool {
    if !matches!(request.freshness_policy, PackFreshnessPolicy::Strict) {
        return false;
    }
    let Some(created_at_ms) = candidate.created_at_ms else {
        return true;
    };
    let max_age_ms = request.freshness_window_seconds.saturating_mul(1_000);
    request.now_ms.saturating_sub(created_at_ms) > max_age_ms
}

fn line_ranges_overlap(
    left_start: Option<usize>,
    left_end: Option<usize>,
    right_start: Option<usize>,
    right_end: Option<usize>,
) -> bool {
    let (Some(left_start), Some(right_start)) = (left_start, right_start) else {
        return false;
    };
    let left_end = left_end.unwrap_or(left_start);
    let right_end = right_end.unwrap_or(right_start);
    left_start <= right_end && right_start <= left_end
}

fn score_candidate(
    candidate: &PackCandidate,
    request: &PackPlanRequest,
    selected_state: &SelectedState,
    lexical_range: ScoreRange,
    semantic_range: ScoreRange,
    token_cost: usize,
) -> PackSelectionScore {
    let relevance_score = relevance_score(candidate, lexical_range, semantic_range);
    let coverage_score = coverage_score(candidate);
    let freshness_score = freshness_score(candidate, request);
    let source_diversity_score = source_diversity_score(candidate, selected_state);
    let source_authority_score = source_authority_score(candidate);
    let role_score = role_score(candidate.role);
    let citation_quality_score = citation_quality_score(candidate);
    let duplicate_penalty = duplicate_penalty(candidate, selected_state);
    let score = 0.35 * relevance_score
        + 0.20 * coverage_score
        + 0.15 * freshness_score
        + 0.10 * source_diversity_score
        + 0.10 * role_score
        + 0.05 * source_authority_score
        + 0.05 * citation_quality_score
        - duplicate_penalty;

    PackSelectionScore {
        score,
        relevance_score,
        coverage_score,
        freshness_score,
        source_diversity_score,
        source_authority_score,
        role_score,
        citation_quality_score,
        duplicate_penalty,
        token_cost,
        selected_reason: selected_reason(
            relevance_score,
            freshness_score,
            source_diversity_score,
            citation_quality_score,
        ),
    }
}

#[derive(Debug, Clone, Copy)]
struct ScoreRange {
    min: f64,
    max: f64,
    has_value: bool,
}

impl ScoreRange {
    fn from_values(values: impl Iterator<Item = f64>) -> Self {
        let mut range = Self {
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            has_value: false,
        };
        for value in values {
            range.has_value = true;
            range.min = range.min.min(value);
            range.max = range.max.max(value);
        }
        range
    }

    fn normalize(self, value: Option<f64>) -> f64 {
        let Some(value) = finite_score(value) else {
            return 0.0;
        };
        if !self.has_value {
            return 0.0;
        }
        if (self.max - self.min).abs() < f64::EPSILON {
            return if value > 0.0 { 1.0 } else { 0.0 };
        }
        ((value - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
    }
}

fn finite_score(score: Option<f64>) -> Option<f64> {
    score.filter(|value| value.is_finite())
}

fn relevance_score(
    candidate: &PackCandidate,
    lexical_range: ScoreRange,
    semantic_range: ScoreRange,
) -> f64 {
    let lexical = lexical_range.normalize(candidate.lexical_score);
    let semantic = semantic_range.normalize(candidate.semantic_score);
    let hybrid = candidate
        .hybrid_rank
        .map(|rank| 1.0 / rank.max(1) as f64)
        .unwrap_or(0.0);
    lexical.max(semantic).max(hybrid).clamp(0.0, 1.0)
}

fn coverage_score(candidate: &PackCandidate) -> f64 {
    let denominator = candidate
        .query_term_count
        .saturating_add(candidate.query_phrase_count.saturating_mul(2));
    if denominator == 0 {
        return 0.0;
    }
    let numerator = candidate
        .matched_terms
        .len()
        .saturating_add(candidate.matched_phrases.len().saturating_mul(2));
    (numerator as f64 / denominator as f64).clamp(0.0, 1.0)
}

fn freshness_score(candidate: &PackCandidate, request: &PackPlanRequest) -> f64 {
    let Some(created_at_ms) = candidate.created_at_ms else {
        return match request.freshness_policy {
            PackFreshnessPolicy::PreferRecent => 0.25,
            PackFreshnessPolicy::Strict => 0.0,
            PackFreshnessPolicy::AllowStale => 1.0,
        };
    };
    let age_ms = request.now_ms.saturating_sub(created_at_ms).max(0);
    let window_ms = request
        .freshness_window_seconds
        .max(1)
        .saturating_mul(1_000);
    if age_ms <= window_ms {
        return 1.0;
    }
    let max_decay_ms = window_ms.saturating_mul(4);
    if age_ms >= max_decay_ms {
        0.0
    } else {
        1.0 - ((age_ms - window_ms) as f64 / (max_decay_ms - window_ms) as f64)
    }
}

fn source_diversity_score(candidate: &PackCandidate, selected_state: &SelectedState) -> f64 {
    let session_key = (candidate.source_id.clone(), candidate.source_path.clone());
    if selected_state.sessions.contains(&session_key) {
        0.0
    } else if selected_state.source_ids.contains(&candidate.source_id) {
        0.5
    } else {
        1.0
    }
}

fn source_authority_score(candidate: &PackCandidate) -> f64 {
    match (
        candidate.source_explicitly_requested,
        candidate.origin_kind.as_str(),
        candidate.source_readiness,
    ) {
        (true, _, PackSourceReadiness::Healthy) => 1.0,
        (_, "local", PackSourceReadiness::Healthy) => 1.0,
        (_, _, PackSourceReadiness::Healthy) => 0.9,
        (_, _, PackSourceReadiness::StaleReadable) => 0.6,
        (_, _, PackSourceReadiness::IncompleteMetadata) => 0.4,
        (_, _, PackSourceReadiness::Unavailable) => 0.0,
    }
}

fn role_score(role: PackEvidenceRole) -> f64 {
    match role {
        PackEvidenceRole::AssistantConclusion | PackEvidenceRole::ToolResult => 1.0,
        PackEvidenceRole::UserRequirement => 0.85,
        PackEvidenceRole::ToolCallArgument => 0.65,
        PackEvidenceRole::Unknown => 0.5,
    }
}

fn citation_quality_score(candidate: &PackCandidate) -> f64 {
    let has_path = !candidate.source_path.trim().is_empty();
    let has_source = !candidate.source_id.trim().is_empty();
    let has_agent = !candidate.agent.trim().is_empty();
    let has_line_span = candidate.line_start.is_some() && candidate.line_end.is_some();
    if has_path && has_source && has_agent && has_line_span {
        1.0
    } else if has_path && has_source && has_agent {
        0.75
    } else if has_path && has_agent {
        0.5
    } else {
        0.0
    }
}

fn duplicate_penalty(candidate: &PackCandidate, selected_state: &SelectedState) -> f64 {
    if selected_state.span_hashes.contains(&candidate.span_hash) {
        return 1.0;
    }
    if selected_state
        .content_hashes
        .contains(&candidate.content_hash)
    {
        return 0.5;
    }
    if selected_state
        .ranges
        .iter()
        .any(|(source_path, start, end)| {
            source_path == &candidate.source_path
                && line_ranges_overlap(*start, *end, candidate.line_start, candidate.line_end)
        })
    {
        return 0.25;
    }
    0.0
}

fn selected_reason(
    relevance_score: f64,
    freshness_score: f64,
    source_diversity_score: f64,
    citation_quality_score: f64,
) -> PackSelectedReason {
    let scores = [
        (relevance_score, PackSelectedReason::HighRelevance),
        (freshness_score, PackSelectedReason::FreshEvidence),
        (source_diversity_score, PackSelectedReason::SourceDiversity),
        (citation_quality_score, PackSelectedReason::StrongCitation),
        (0.0, PackSelectedReason::BudgetFit),
    ];
    scores
        .into_iter()
        .max_by(|(left, _), (right, _)| left.total_cmp(right))
        .map(|(_, reason)| reason)
        .unwrap_or(PackSelectedReason::BudgetFit)
}

fn candidate_ordering(
    left: &ScoredCandidate,
    left_candidate: &PackCandidate,
    right: &ScoredCandidate,
    right_candidate: &PackCandidate,
) -> Ordering {
    right
        .score
        .score
        .total_cmp(&left.score.score)
        .then_with(|| {
            right
                .score
                .relevance_score
                .total_cmp(&left.score.relevance_score)
        })
        .then_with(|| {
            compare_newer_first(left_candidate.created_at_ms, right_candidate.created_at_ms)
        })
        .then_with(|| left_candidate.source_id.cmp(&right_candidate.source_id))
        .then_with(|| left_candidate.source_path.cmp(&right_candidate.source_path))
        .then_with(|| {
            compare_optional_usize_low_first(left_candidate.line_start, right_candidate.line_start)
        })
        .then_with(|| {
            left_candidate
                .content_hash
                .cmp(&right_candidate.content_hash)
        })
}

fn compare_newer_first(left: Option<i64>, right: Option<i64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_optional_usize_low_first(left: Option<usize>, right: Option<usize>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn omitted_candidate(
    candidate: &PackCandidate,
    reason: PackOmittedReason,
    score: PackSelectionScore,
) -> OmittedPackCandidate {
    OmittedPackCandidate {
        candidate_id: candidate.candidate_id.clone(),
        source_path: candidate.source_path.clone(),
        line_start: candidate.line_start,
        agent: candidate.agent.clone(),
        reason,
        score: score.score,
        estimated_tokens: score.token_cost,
    }
}

fn truncate_excerpt(excerpt: &str, max_chars: usize) -> (String, bool) {
    if excerpt.chars().count() <= max_chars {
        return (excerpt.to_string(), false);
    }
    let keep_chars = max_chars.saturating_sub(3);
    let mut out: String = excerpt.chars().take(keep_chars).collect();
    out.push_str("...");
    (out, true)
}

fn estimated_tokens(text: &str) -> usize {
    text.chars()
        .count()
        .div_ceil(TOKEN_ESTIMATE_CHARS_PER_TOKEN)
}

fn evidence_id(candidate: &PackCandidate) -> String {
    let mut hasher_input = String::new();
    hasher_input.push_str(&candidate.source_id);
    hasher_input.push('\n');
    hasher_input.push_str(&candidate.source_path);
    hasher_input.push('\n');
    hasher_input.push_str(&candidate.line_start.unwrap_or_default().to_string());
    hasher_input.push('\n');
    hasher_input.push_str(&candidate.line_end.unwrap_or_default().to_string());
    hasher_input.push('\n');
    hasher_input.push_str(&candidate.span_hash);
    let hash = blake3::hash(hasher_input.as_bytes());
    format!("ev_{}", &hash.to_hex()[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, source_id: &str, source_path: &str, score: f64) -> PackCandidate {
        PackCandidate {
            candidate_id: id.to_string(),
            source_path: source_path.to_string(),
            source_id: source_id.to_string(),
            origin_kind: if source_id == "local" {
                "local".to_string()
            } else {
                "ssh".to_string()
            },
            origin_host: None,
            workspace: "/work".to_string(),
            workspace_original: None,
            agent: "codex".to_string(),
            line_start: Some(10),
            line_end: Some(12),
            conversation_id: None,
            message_index: None,
            content_hash: format!("{id}_content"),
            span_hash: format!("{id}_span"),
            created_at_ms: Some(1_000_000),
            indexed_at_ms: Some(1_000_000),
            match_type: "exact".to_string(),
            excerpt: "0123456789abcdef".to_string(),
            role: PackEvidenceRole::AssistantConclusion,
            lexical_score: Some(score),
            semantic_score: None,
            hybrid_rank: None,
            matched_terms: vec!["pack".to_string()],
            matched_phrases: Vec::new(),
            query_term_count: 1,
            query_phrase_count: 0,
            source_readiness: PackSourceReadiness::Healthy,
            source_explicitly_requested: false,
        }
    }

    fn request(candidates: Vec<PackCandidate>) -> PackPlanRequest {
        PackPlanRequest {
            now_ms: 1_000_000,
            limits: PackPlannerLimits {
                max_tokens: 1_024,
                max_sessions: 8,
                max_evidence: 24,
                context_lines: 3,
                max_excerpt_chars: 80,
            },
            freshness_policy: PackFreshnessPolicy::PreferRecent,
            freshness_window_seconds: 60,
            candidates,
            explain_selection: false,
        }
    }

    #[test]
    fn empty_corpus_returns_empty_plan() {
        let plan = plan_answer_pack(request(Vec::new())).unwrap();

        assert_eq!(plan.candidate_count, 0);
        assert_eq!(plan.selected_evidence_count, 0);
        assert_eq!(plan.diagnostics.candidate_fetch_limit, 192);
        assert!(plan.evidence.is_empty());
        assert!(plan.omitted.is_empty());
    }

    #[test]
    fn candidate_fetch_limit_matches_contract_formula() {
        let mut limits = PackPlannerLimits {
            max_tokens: 12_000,
            max_sessions: 2,
            max_evidence: 3,
            context_lines: 3,
            max_excerpt_chars: 1_600,
        };

        assert_eq!(pack_candidate_fetch_limit(&limits).unwrap(), 64);

        limits.max_sessions = 20;
        assert_eq!(pack_candidate_fetch_limit(&limits).unwrap(), 320);

        limits.max_sessions = 64;
        limits.max_evidence = 256;
        assert_eq!(
            pack_candidate_fetch_limit(&limits).unwrap(),
            PACK_CANDIDATE_LIMIT_CAP
        );
    }

    #[test]
    fn token_budget_reserves_documented_sections() {
        let budget = pack_planner_budget(&PackPlannerLimits {
            max_tokens: 12_000,
            max_sessions: 8,
            max_evidence: 24,
            context_lines: 3,
            max_excerpt_chars: 1_600,
        })
        .unwrap();

        assert_eq!(budget.metadata_tokens, 1_800);
        assert_eq!(budget.outline_tokens, 1_800);
        assert_eq!(budget.evidence_tokens, 7_200);
        assert_eq!(budget.omitted_tokens, 1_200);
        assert_eq!(budget.max_output_tokens_with_overflow, 12_600);
        assert_eq!(
            budget.metadata_tokens
                + budget.outline_tokens
                + budget.evidence_tokens
                + budget.omitted_tokens,
            budget.max_tokens
        );
    }

    #[test]
    fn duplicate_content_is_omitted_after_first_selection() {
        let first = candidate("a", "local", "/s/a.jsonl", 10.0);
        let mut duplicate = candidate("b", "local", "/s/b.jsonl", 9.0);
        duplicate.content_hash = first.content_hash.clone();

        let plan = plan_answer_pack(request(vec![first, duplicate])).unwrap();

        assert_eq!(plan.selected_evidence_count, 1);
        assert_eq!(plan.omitted.len(), 1);
        assert_eq!(plan.omitted[0].reason, PackOmittedReason::DuplicateContent);
    }

    #[test]
    fn duplicate_span_and_overlapping_ranges_are_omitted_once() {
        let span_source = candidate("span-source", "local", "/s/span.jsonl", 10.0);
        let mut span_duplicate = candidate("span-dup", "remote", "/s/other.jsonl", 9.0);
        span_duplicate.span_hash = span_source.span_hash.clone();

        let range_source = candidate("range-source", "local", "/s/range.jsonl", 8.0);
        let mut range_duplicate = candidate("range-dup", "remote", "/s/range.jsonl", 7.0);
        range_duplicate.line_start = Some(11);
        range_duplicate.line_end = Some(14);

        let plan = plan_answer_pack(request(vec![
            span_source,
            span_duplicate,
            range_source,
            range_duplicate,
        ]))
        .unwrap();

        let omitted_ids: Vec<_> = plan
            .omitted
            .iter()
            .map(|omitted| (omitted.candidate_id.as_str(), omitted.reason))
            .collect();
        assert_eq!(
            omitted_ids,
            vec![
                ("span-dup", PackOmittedReason::DuplicateContent),
                ("range-dup", PackOmittedReason::DuplicateContent),
            ]
        );
    }

    #[test]
    fn exact_token_budget_boundary_selects_until_budget_exhausted() {
        let mut first = candidate("a", "local", "/s/a.jsonl", 10.0);
        first.excerpt = "12345678".to_string();
        let mut second = candidate("b", "remote", "/s/b.jsonl", 9.0);
        second.excerpt = "abcdefgh".to_string();

        let mut req = request(vec![first, second]);
        req.limits.max_tokens = 1_024;
        req.limits.max_excerpt_chars = 4_096;
        let evidence_budget = pack_planner_budget(&req.limits).unwrap().evidence_tokens;
        req.candidates[0].excerpt = "x".repeat(evidence_budget * TOKEN_ESTIMATE_CHARS_PER_TOKEN);
        req.candidates[1].excerpt = "y".repeat(4);

        let plan = plan_answer_pack(req).unwrap();

        assert_eq!(plan.selected_evidence_count, 1);
        assert_eq!(plan.estimated_tokens, evidence_budget);
        assert_eq!(
            plan.omitted[0].reason,
            PackOmittedReason::TokenBudgetExhausted
        );
    }

    #[test]
    fn oversized_high_score_candidate_can_be_skipped_for_budget_fit() {
        let mut oversized = candidate("oversized", "local", "/s/oversized.jsonl", 10.0);
        let mut fitting = candidate("fit", "remote", "/s/fit.jsonl", 9.0);

        let mut req = request(vec![oversized.clone(), fitting.clone()]);
        req.limits.max_excerpt_chars = 8_000;
        let evidence_budget = pack_planner_budget(&req.limits).unwrap().evidence_tokens;
        oversized.excerpt = "x".repeat((evidence_budget + 1) * TOKEN_ESTIMATE_CHARS_PER_TOKEN);
        fitting.excerpt = "y".repeat(TOKEN_ESTIMATE_CHARS_PER_TOKEN);
        req.candidates = vec![oversized, fitting];

        let plan = plan_answer_pack(req).unwrap();

        assert_eq!(plan.evidence[0].candidate.candidate_id, "fit");
        assert_eq!(plan.omitted.len(), 1);
        assert_eq!(
            plan.omitted[0].reason,
            PackOmittedReason::TokenBudgetExhausted
        );
    }

    #[test]
    fn source_diversity_changes_second_pick() {
        let first = candidate("a", "local", "/s/a.jsonl", 10.0);
        let same_source = candidate("b", "local", "/s/b.jsonl", 9.9);
        let different_source = candidate("c", "remote", "/s/c.jsonl", 9.9);

        let mut req = request(vec![first, same_source, different_source]);
        req.limits.max_evidence = 2;

        let plan = plan_answer_pack(req).unwrap();

        assert_eq!(plan.evidence[0].candidate.candidate_id, "a");
        assert_eq!(plan.evidence[1].candidate.candidate_id, "c");
    }

    #[test]
    fn strict_freshness_omits_stale_or_unknown_timestamps() {
        let mut stale = candidate("old", "local", "/s/old.jsonl", 10.0);
        stale.created_at_ms = Some(0);
        let mut unknown = candidate("unknown", "remote", "/s/unknown.jsonl", 9.0);
        unknown.created_at_ms = None;

        let mut req = request(vec![stale, unknown]);
        req.freshness_policy = PackFreshnessPolicy::Strict;
        req.freshness_window_seconds = 60;

        let plan = plan_answer_pack(req).unwrap();

        assert!(plan.evidence.is_empty());
        assert_eq!(plan.omitted.len(), 2);
        assert!(
            plan.omitted
                .iter()
                .all(|omitted| omitted.reason == PackOmittedReason::StaleUnderStrictPolicy)
        );
    }

    #[test]
    fn null_timestamps_sort_last_when_scores_tie() {
        let mut unknown = candidate("unknown", "a", "/a.jsonl", 1.0);
        unknown.created_at_ms = None;
        let mut timestamped = candidate("timestamped", "z", "/z.jsonl", 1.0);
        timestamped.created_at_ms = Some(1_000_000);

        let mut req = request(vec![unknown, timestamped]);
        req.freshness_policy = PackFreshnessPolicy::AllowStale;

        let plan = plan_answer_pack(req).unwrap();

        assert_eq!(plan.evidence[0].candidate.candidate_id, "timestamped");
    }

    #[test]
    fn freshness_policy_scores_unknown_timestamps_explicitly() {
        let mut unknown = candidate("unknown", "local", "/s/unknown.jsonl", 1.0);
        unknown.created_at_ms = None;
        let mut req = request(vec![unknown.clone()]);

        req.freshness_policy = PackFreshnessPolicy::PreferRecent;
        assert_eq!(freshness_score(&unknown, &req), 0.25);

        req.freshness_policy = PackFreshnessPolicy::AllowStale;
        assert_eq!(freshness_score(&unknown, &req), 1.0);

        req.freshness_policy = PackFreshnessPolicy::Strict;
        assert_eq!(freshness_score(&unknown, &req), 0.0);
    }

    #[test]
    fn lexical_score_drives_relevance_when_semantic_is_absent() {
        let plan =
            plan_answer_pack(request(vec![candidate("a", "local", "/s/a.jsonl", 7.0)])).unwrap();

        assert_eq!(plan.selected_evidence_count, 1);
        assert!(plan.evidence[0].selection.relevance_score > 0.0);
    }

    #[test]
    fn stable_tie_breaks_do_not_depend_on_input_order() {
        let mut later_path = candidate("z", "remote", "/z.jsonl", 1.0);
        later_path.line_start = Some(50);
        let mut earlier_path = candidate("a", "local", "/a.jsonl", 1.0);
        earlier_path.line_start = Some(50);

        let left =
            plan_answer_pack(request(vec![later_path.clone(), earlier_path.clone()])).unwrap();
        let right = plan_answer_pack(request(vec![earlier_path, later_path])).unwrap();

        assert_eq!(left.evidence[0].candidate.source_path, "/a.jsonl");
        assert_eq!(right.evidence[0].candidate.source_path, "/a.jsonl");
    }

    #[test]
    fn stable_ordering_keeps_cursor_like_page_boundaries() {
        let candidates = vec![
            candidate("e", "remote", "/e.jsonl", 1.0),
            candidate("b", "remote", "/b.jsonl", 1.0),
            candidate("d", "remote", "/d.jsonl", 1.0),
            candidate("a", "remote", "/a.jsonl", 1.0),
            candidate("c", "remote", "/c.jsonl", 1.0),
        ];
        let mut reversed = candidates.clone();
        reversed.reverse();

        let left = plan_answer_pack(request(candidates)).unwrap();
        let right = plan_answer_pack(request(reversed)).unwrap();
        let left_ids: Vec<_> = left
            .evidence
            .iter()
            .map(|evidence| evidence.candidate.candidate_id.as_str())
            .collect();
        let right_ids: Vec<_> = right
            .evidence
            .iter()
            .map(|evidence| evidence.candidate.candidate_id.as_str())
            .collect();

        assert_eq!(
            left_ids.chunks(2).collect::<Vec<_>>(),
            right_ids.chunks(2).collect::<Vec<_>>()
        );
    }

    #[test]
    fn omitted_reasons_serialize_to_documented_snake_case() {
        let reasons = [
            (
                PackOmittedReason::TokenBudgetExhausted,
                "token_budget_exhausted",
            ),
            (
                PackOmittedReason::MaxSessionsReached,
                "max_sessions_reached",
            ),
            (
                PackOmittedReason::MaxEvidenceReached,
                "max_evidence_reached",
            ),
            (PackOmittedReason::DuplicateContent, "duplicate_content"),
            (
                PackOmittedReason::SameSessionLowerRank,
                "same_session_lower_rank",
            ),
            (
                PackOmittedReason::StaleUnderStrictPolicy,
                "stale_under_strict_policy",
            ),
            (PackOmittedReason::SourceUnavailable, "source_unavailable"),
            (PackOmittedReason::RedactedToEmpty, "redacted_to_empty"),
            (PackOmittedReason::FieldMaskExcluded, "field_mask_excluded"),
        ];

        for (reason, expected) in reasons {
            assert_eq!(serde_json::to_value(reason).unwrap(), expected);
        }
    }
}
