//! Deterministic answer-pack evidence selection.
//!
//! This module is intentionally independent of the CLI. `cass pack` will wire it
//! to search, source health, renderers, and robot docs in later beads.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::indexer::redact_secrets::redact_text;
use crate::robot_budget_envelope::BudgetBlock;

use super::query::{MatchType, SearchHit};

const TOKEN_ESTIMATE_CHARS_PER_TOKEN: usize = 4;
const DEFAULT_FRESHNESS_WINDOW_SECONDS: i64 = 30 * 24 * 60 * 60;
const PACK_CANDIDATE_LIMIT_CAP: usize = 2_048;
const REDACTED_VALUE_MARKER: &str = "[REDACTED]";
const REDACTED_PATH_PREFIX: &str = "[REDACTED_PATH]";
const REDACTED_REMOTE_HOST_MARKER: &str = "[REDACTED_REMOTE_HOST]";
const REDACTED_SOURCE_MARKER: &str = "[REDACTED_SOURCE]";
const REDACTED_ENCRYPTED_PAYLOAD_MARKER: &str = "[REDACTED_ENCRYPTED_PAYLOAD]";

static PRIVATE_PATH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?x)
        (?:
            (?:/home/[^/\s"'`<>\[\](){}:,;]+|/Users/[^/\s"'`<>\[\](){}:,;]+|~)
            (?:/[^\s"'`<>\[\](){}:,;]+)*
          |
            [A-Za-z]:\\Users\\[^\\\s"'`<>\[\](){}:,;]+
            (?:\\[^\s"'`<>\[\](){}:,;]+)*
        )
        "#,
    )
    .expect("private path redaction regex")
});

static ENCRYPTED_PAYLOAD_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:encrypted_(?:payload(?:_material)?|content|blob|material)|ciphertext)\b\s*[:=]\s*[A-Za-z0-9+/=_:.-]{16,}",
    )
    .expect("encrypted payload redaction regex")
});

static PRIVATE_HOST_LABEL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:[A-Za-z0-9._%+-]+@)?[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*(?:\.internal|\.local)\b",
    )
    .expect("private host label redaction regex")
});

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackLexicalReadiness {
    #[default]
    Ready,
    Stale,
    Missing,
    Rebuilding,
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackSemanticReadiness {
    #[default]
    NotReported,
    Joined,
    FallbackLexical,
    Unavailable,
    Disabled,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackSourceSyncGapKind {
    RemoteStale,
    SourcePruned,
    SyncDeferred,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackSourceSyncGap {
    pub source_id: String,
    pub origin_kind: String,
    pub kind: PackSourceSyncGapKind,
    pub lag_seconds: Option<i64>,
    pub last_synced_at_ms: Option<i64>,
    pub recommended_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackReadinessSnapshot {
    pub index_generation: Option<String>,
    pub lexical_readiness: PackLexicalReadiness,
    pub semantic_readiness: PackSemanticReadiness,
    pub active_rebuild: bool,
    pub lock_state: Option<String>,
    pub missing_database: bool,
    pub source_sync_gaps: Vec<PackSourceSyncGap>,
    pub recommended_action: Option<String>,
}

impl Default for PackReadinessSnapshot {
    fn default() -> Self {
        Self {
            index_generation: None,
            lexical_readiness: PackLexicalReadiness::Ready,
            semantic_readiness: PackSemanticReadiness::NotReported,
            active_rebuild: false,
            lock_state: None,
            missing_database: false,
            source_sync_gaps: Vec::new(),
            recommended_action: None,
        }
    }
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
            match_type: match_type_robot_name(hit.match_type).to_string(),
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

fn match_type_robot_name(match_type: MatchType) -> &'static str {
    match match_type {
        MatchType::Exact => "exact",
        MatchType::Prefix => "prefix",
        MatchType::Suffix => "suffix",
        MatchType::Substring => "substring",
        MatchType::Wildcard => "wildcard",
        MatchType::ImplicitWildcard => "implicit_wildcard",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackRenderFormat {
    Json,
    CompactJson,
    Jsonl,
    Toon,
    Markdown,
}

impl PackRenderFormat {
    fn label(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::CompactJson => "compact",
            Self::Jsonl => "jsonl",
            Self::Toon => "toon",
            Self::Markdown => "markdown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRenderRequest {
    pub query_text: String,
    pub normalized_query: String,
    pub generated_at_ms: i64,
    pub elapsed_ms: u64,
    pub budget: BudgetBlock,
    pub request_id: Option<String>,
    pub format: PackRenderFormat,
    pub limits: PackPlannerLimits,
    pub search_mode: String,
    pub fallback_mode: Option<String>,
    pub semantic_joined: bool,
    pub freshness_policy: PackFreshnessPolicy,
    pub freshness_window_seconds: i64,
    pub redaction_policy: String,
    pub sensitive_output: bool,
    pub skill_content_included: bool,
    pub explain_selection: bool,
    pub readiness: PackReadinessSnapshot,
}

impl Default for PackRenderRequest {
    fn default() -> Self {
        Self {
            query_text: String::new(),
            normalized_query: String::new(),
            generated_at_ms: 0,
            elapsed_ms: 0,
            budget: BudgetBlock {
                elapsed_ms: 0,
                budget_ms: 8_000,
                timed_out: false,
                skipped_sections: Vec::new(),
                recommended_next_probe: None,
            },
            request_id: None,
            format: PackRenderFormat::Json,
            limits: PackPlannerLimits::default(),
            search_mode: "hybrid".to_string(),
            fallback_mode: None,
            semantic_joined: false,
            freshness_policy: PackFreshnessPolicy::PreferRecent,
            freshness_window_seconds: DEFAULT_FRESHNESS_WINDOW_SECONDS,
            redaction_policy: "strict".to_string(),
            sensitive_output: false,
            skill_content_included: false,
            explain_selection: false,
            readiness: PackReadinessSnapshot::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackRenderError {
    pub format: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedAnswerPack {
    schema_version: &'static str,
    query: RenderedQuery,
    #[serde(rename = "_meta")]
    meta: RenderedMeta,
    budget: BudgetBlock,
    limits: RenderedLimits,
    realized: RenderedRealized,
    health: RenderedHealth,
    freshness: RenderedFreshness,
    pack: RenderedPack,
    evidence: Vec<RenderedEvidence>,
    omitted: RenderedOmitted,
    privacy: RenderedPrivacy,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedQuery {
    text: String,
    normalized: String,
    filters: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedMeta {
    request_id: Option<String>,
    generated_at_ms: i64,
    elapsed_ms: u64,
    partial: bool,
    format: &'static str,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedLimits {
    max_tokens: usize,
    estimated_tokens: usize,
    max_sessions: usize,
    max_evidence: usize,
    context_lines: usize,
    max_excerpt_chars: usize,
    field_mask: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedRealized {
    search_mode: String,
    fallback_mode: Option<String>,
    semantic_joined: bool,
    candidate_count: usize,
    selected_evidence_count: usize,
    selected_session_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedHealth {
    healthy: bool,
    recommended_action: Option<String>,
    index_state: &'static str,
    index_generation: Option<String>,
    lexical_readiness: &'static str,
    semantic_state: &'static str,
    active_rebuild: bool,
    lock_state: Option<String>,
    missing_database: bool,
    source_sync_gaps: Vec<RenderedSourceSyncGap>,
    source_readiness: Vec<RenderedSourceReadiness>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedSourceReadiness {
    source_id: String,
    origin_kind: String,
    readiness: &'static str,
    healthy: bool,
    evidence_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedSourceSyncGap {
    source_id: String,
    origin_kind: String,
    kind: &'static str,
    lag_seconds: Option<i64>,
    last_synced_at_ms: Option<i64>,
    recommended_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedFreshness {
    policy: &'static str,
    window_seconds: i64,
    newest_evidence_at_ms: Option<i64>,
    oldest_evidence_at_ms: Option<i64>,
    stale_evidence_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedPack {
    title: String,
    answer_outline: Vec<RenderedOutlineItem>,
    source_summary: Vec<RenderedSourceSummary>,
    handoff: Vec<RenderedHandoffItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedOutlineItem {
    rank: usize,
    heading: String,
    evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedSourceSummary {
    source_id: String,
    origin_kind: String,
    session_count: usize,
    evidence_count: usize,
    newest_evidence_at_ms: Option<i64>,
    healthy: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedHandoffItem {
    rank: usize,
    kind: &'static str,
    text: String,
    evidence_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedEvidence {
    id: String,
    rank: usize,
    excerpt: String,
    excerpt_truncated: bool,
    estimated_tokens: usize,
    citation: RenderedCitation,
    /// Metadata-only trust/provenance verdict for this evidence item (bead
    /// 5u82n.3). Advisory; mirrors the per-hit `trust` block on `cass search`.
    trust: crate::search::trust_scoring::TrustAssessment,
    selection: RenderedSelection,
    roles: Vec<&'static str>,
    matched_terms: Vec<String>,
    redactions: Vec<RenderedRedaction>,
    #[serde(skip)]
    source_readiness: PackSourceReadiness,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedCitation {
    source_path: String,
    source_id: String,
    origin_kind: String,
    origin_host: Option<String>,
    workspace: String,
    workspace_original: Option<String>,
    agent: String,
    line_start: Option<usize>,
    line_end: Option<usize>,
    message_index: Option<usize>,
    conversation_id: Option<i64>,
    content_hash: String,
    span_hash: String,
    excerpt_sha256: String,
    created_at_ms: Option<i64>,
    indexed_at_ms: Option<i64>,
    freshness_age_seconds: Option<i64>,
    match_type: String,
    verified: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedSelection {
    score: f64,
    token_cost: usize,
    selected_reason: PackSelectedReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    relevance_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    coverage_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_diversity_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_authority_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    citation_quality_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duplicate_penalty: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedRedaction {
    kind: String,
    start_char: usize,
    end_char: usize,
    replacement: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedOmitted {
    count: usize,
    items: Vec<OmittedPackCandidate>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RenderedPrivacy {
    redaction_policy: String,
    redaction_applied: bool,
    sensitive_output: bool,
    skill_content_included: bool,
    redaction_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Default)]
struct SourceAccumulator {
    origin_kind: String,
    sessions: BTreeSet<String>,
    evidence_count: usize,
    newest_evidence_at_ms: Option<i64>,
    healthy: bool,
    worst_readiness: PackSourceReadiness,
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
                // ubs:ignore — compares public citation paths, never secret material.
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
            // ubs:ignore — compares public citation paths, never secret material.
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

pub fn render_answer_pack(
    plan: &PlannedAnswerPack,
    request: &PackRenderRequest,
) -> Result<String, PackRenderError> {
    let envelope = rendered_answer_pack(plan, request);
    match request.format {
        PackRenderFormat::Json => {
            serde_json::to_string_pretty(&envelope).map_err(|err| render_error(request, err))
        }
        PackRenderFormat::CompactJson => {
            serde_json::to_string(&envelope).map_err(|err| render_error(request, err))
        }
        PackRenderFormat::Jsonl => render_answer_pack_jsonl(&envelope, request),
        PackRenderFormat::Toon => {
            let value =
                serde_json::to_value(&envelope).map_err(|err| render_error(request, err))?;
            Ok(toon::encode(value, Some(pack_toon_encode_options())))
        }
        PackRenderFormat::Markdown => Ok(render_answer_pack_markdown(&envelope)),
    }
}

pub fn render_answer_pack_value(
    plan: &PlannedAnswerPack,
    request: &PackRenderRequest,
) -> Result<serde_json::Value, PackRenderError> {
    serde_json::to_value(rendered_answer_pack(plan, request))
        .map_err(|err| render_error(request, err))
}

fn render_error(error: &PackRenderRequest, err: serde_json::Error) -> PackRenderError {
    PackRenderError {
        format: error.format.label(),
        message: err.to_string(),
    }
}

fn rendered_answer_pack(
    plan: &PlannedAnswerPack,
    request: &PackRenderRequest,
) -> RenderedAnswerPack {
    // q4pau: build the commit/bead/release correlation index once per pack render
    // (fail-open empty index off-repo); its repo root anchors cwd-relative
    // workspace matching for each evidence item.
    let correlation = crate::search::trust_correlation::build_for_cwd();
    let query_workspace = correlation.project_workspace();
    let evidence = plan
        .evidence
        .iter()
        .map(|item| rendered_evidence(item, request, &correlation, query_workspace.as_deref()))
        .collect::<Vec<_>>();
    let mut envelope_redactions = Vec::new();
    let query_text = redact_pack_output_text(&request.query_text, &mut envelope_redactions);
    let normalized_query =
        redact_pack_output_text(&normalized_query(request), &mut envelope_redactions);
    let pack_title = redact_pack_output_text(&pack_title(request), &mut envelope_redactions);
    let source_summary = rendered_source_summary(&evidence);
    let source_readiness = rendered_source_readiness(&evidence);
    let source_sync_gaps = rendered_source_sync_gaps(
        &request.readiness.source_sync_gaps,
        &mut envelope_redactions,
    );
    let stale_evidence_count = stale_evidence_count(&evidence, request);
    let redacted_count = plan
        .omitted
        .iter()
        // ubs:ignore — compares a public omission enum, not secret material.
        .filter(|omitted| omitted.reason == PackOmittedReason::RedactedToEmpty)
        .count();
    let (omitted_items, omitted_redactions) = rendered_omitted_items(&plan.omitted);
    let semantic_readiness = effective_semantic_readiness(request);
    let health_is_healthy = health_is_healthy(request, &source_readiness);
    let mut warnings = readiness_warnings(
        request,
        semantic_readiness,
        &source_readiness,
        evidence.is_empty(),
        &mut envelope_redactions,
    );
    let recommended_action =
        readiness_recommended_action(request, semantic_readiness, health_is_healthy)
            .map(|action| redact_pack_output_text(&action, &mut envelope_redactions));
    let index_generation = request
        .readiness
        .index_generation
        .as_deref()
        .map(|generation| redact_pack_output_text(generation, &mut envelope_redactions));
    let lock_state = request
        .readiness
        .lock_state
        .as_deref()
        .map(|state| redact_pack_output_text(state, &mut envelope_redactions));
    let redaction_counts = redaction_counts(
        redacted_count,
        &evidence,
        &omitted_redactions,
        &envelope_redactions,
    );
    let redaction_applied = !redaction_counts.is_empty();
    if redaction_applied {
        warnings.push("privacy_redactions_applied".to_string());
    }

    RenderedAnswerPack {
        schema_version: "cass.pack.v1",
        query: RenderedQuery {
            text: query_text,
            normalized: normalized_query,
            filters: BTreeMap::new(),
        },
        meta: RenderedMeta {
            request_id: request.request_id.clone(),
            generated_at_ms: request.generated_at_ms,
            elapsed_ms: request.elapsed_ms,
            partial: request.budget.timed_out || !request.budget.skipped_sections.is_empty(),
            format: request.format.label(),
            warnings: warnings.clone(),
        },
        budget: request.budget.clone(),
        limits: RenderedLimits {
            max_tokens: request.limits.max_tokens,
            estimated_tokens: plan.estimated_tokens,
            max_sessions: request.limits.max_sessions,
            max_evidence: request.limits.max_evidence,
            context_lines: request.limits.context_lines,
            max_excerpt_chars: request.limits.max_excerpt_chars,
            field_mask: "standard",
        },
        realized: RenderedRealized {
            search_mode: request.search_mode.clone(),
            fallback_mode: request.fallback_mode.clone(),
            semantic_joined: request.semantic_joined,
            candidate_count: plan.candidate_count,
            selected_evidence_count: plan.selected_evidence_count,
            selected_session_count: plan.selected_session_count,
        },
        health: RenderedHealth {
            healthy: health_is_healthy,
            recommended_action,
            index_state: lexical_readiness_label(request.readiness.lexical_readiness),
            index_generation,
            lexical_readiness: lexical_readiness_label(request.readiness.lexical_readiness),
            semantic_state: semantic_readiness_label(semantic_readiness),
            active_rebuild: request.readiness.active_rebuild,
            lock_state,
            missing_database: request.readiness.missing_database,
            source_sync_gaps,
            source_readiness,
        },
        freshness: RenderedFreshness {
            policy: freshness_policy_label(request.freshness_policy),
            window_seconds: request.freshness_window_seconds,
            newest_evidence_at_ms: evidence
                .iter()
                .filter_map(|item| item.citation.created_at_ms)
                .max(),
            oldest_evidence_at_ms: evidence
                .iter()
                .filter_map(|item| item.citation.created_at_ms)
                .min(),
            stale_evidence_count,
        },
        pack: RenderedPack {
            title: pack_title,
            answer_outline: rendered_outline(&evidence),
            source_summary,
            handoff: rendered_handoff(&evidence),
        },
        evidence,
        omitted: RenderedOmitted {
            count: plan.omitted.len(),
            items: omitted_items,
        },
        privacy: RenderedPrivacy {
            redaction_policy: request.redaction_policy.clone(),
            redaction_applied,
            sensitive_output: request.sensitive_output,
            skill_content_included: request.skill_content_included,
            redaction_counts,
        },
        warnings,
    }
}

fn health_is_healthy(
    request: &PackRenderRequest,
    source_readiness: &[RenderedSourceReadiness],
) -> bool {
    matches!(
        request.readiness.lexical_readiness,
        PackLexicalReadiness::Ready
    ) && !request.readiness.active_rebuild
        && !request.readiness.missing_database
        && request.readiness.lock_state.is_none()
        && request.readiness.source_sync_gaps.is_empty()
        && source_readiness.iter().all(|source| source.healthy)
}

fn readiness_warnings(
    request: &PackRenderRequest,
    semantic_readiness: PackSemanticReadiness,
    source_readiness: &[RenderedSourceReadiness],
    no_evidence: bool,
    redactions: &mut Vec<RenderedRedaction>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if no_evidence {
        warnings.push("no_evidence_found".to_string());
    }
    match request.readiness.lexical_readiness {
        PackLexicalReadiness::Ready => {}
        PackLexicalReadiness::Stale => warnings.push("lexical_index_stale".to_string()),
        PackLexicalReadiness::Missing => warnings.push("lexical_index_missing".to_string()),
        PackLexicalReadiness::Rebuilding => warnings.push("lexical_index_rebuilding".to_string()),
        PackLexicalReadiness::Unknown => warnings.push("lexical_index_unknown".to_string()),
    }
    match semantic_readiness {
        PackSemanticReadiness::FallbackLexical => {
            warnings.push("semantic_fallback_lexical".to_string());
        }
        PackSemanticReadiness::Unavailable => {
            warnings.push("semantic_unavailable_lexical_fallback".to_string());
        }
        PackSemanticReadiness::Disabled => warnings.push("semantic_disabled".to_string()),
        PackSemanticReadiness::Joined | PackSemanticReadiness::NotReported => {}
    }
    if request.readiness.active_rebuild {
        warnings.push("active_rebuild".to_string());
    }
    if request.readiness.lock_state.is_some() {
        warnings.push("index_lock_active".to_string());
    }
    if request.readiness.missing_database {
        warnings.push("missing_database".to_string());
    }
    for gap in &request.readiness.source_sync_gaps {
        let source_id = redacted_source_label(&gap.source_id, &gap.origin_kind, redactions);
        warnings.push(format!(
            "source_sync_gap:{}:{}",
            source_id,
            source_sync_gap_kind_label(gap.kind)
        ));
    }
    for source in source_readiness.iter().filter(|source| !source.healthy) {
        warnings.push(format!(
            "source_readiness:{}:{}",
            source.source_id, source.readiness
        ));
    }
    warnings
}

fn readiness_recommended_action(
    request: &PackRenderRequest,
    semantic_readiness: PackSemanticReadiness,
    health_is_healthy: bool,
) -> Option<String> {
    if let Some(action) = trimmed_optional_string(request.readiness.recommended_action.as_deref()) {
        return Some(action);
    }
    if request.readiness.missing_database {
        return Some("run cass index --full".to_string());
    }
    match request.readiness.lexical_readiness {
        PackLexicalReadiness::Ready => {}
        PackLexicalReadiness::Stale => {
            return Some("refresh lexical index with cass index --full".to_string());
        }
        PackLexicalReadiness::Missing => {
            return Some("build lexical index with cass index --full".to_string());
        }
        PackLexicalReadiness::Rebuilding => {
            return Some("wait for active rebuild or inspect cass status --json".to_string());
        }
        PackLexicalReadiness::Unknown => {
            return Some("inspect cass health --json".to_string());
        }
    }
    if request.readiness.active_rebuild {
        return Some("wait for active rebuild or inspect cass status --json".to_string());
    }
    if request.readiness.lock_state.is_some() {
        return Some("inspect cass status --json for active locks".to_string());
    }
    if !request.readiness.source_sync_gaps.is_empty() {
        return Some("inspect cass sources sync --json and source status".to_string());
    }
    if !health_is_healthy {
        return Some("inspect cass health --json and source sync status".to_string());
    }
    if matches!(
        semantic_readiness,
        PackSemanticReadiness::FallbackLexical | PackSemanticReadiness::Unavailable
    ) {
        return Some(
            "continue with lexical evidence or install semantic model explicitly".to_string(),
        );
    }
    None
}

fn trimmed_optional_string(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn normalized_query(request: &PackRenderRequest) -> String {
    if request.normalized_query.trim().is_empty() {
        request.query_text.trim().to_string()
    } else {
        request.normalized_query.trim().to_string()
    }
}

fn pack_title(request: &PackRenderRequest) -> String {
    let normalized = normalized_query(request);
    if normalized.is_empty() {
        "answer pack".to_string()
    } else {
        normalized
    }
}

fn redact_pack_output_text(input: &str, redactions: &mut Vec<RenderedRedaction>) -> String {
    let mut output = input.to_string();

    let encrypted_redacted = ENCRYPTED_PAYLOAD_RE.replace_all(&output, |_: &Captures<'_>| {
        REDACTED_ENCRYPTED_PAYLOAD_MARKER
    });
    if let Cow::Owned(redacted) = encrypted_redacted {
        push_full_redaction(
            redactions,
            "encrypted_payload",
            &output,
            REDACTED_ENCRYPTED_PAYLOAD_MARKER,
        );
        output = redacted;
    }

    if let Cow::Owned(redacted) = redact_text(&output) {
        push_full_redaction(redactions, "secret", &output, REDACTED_VALUE_MARKER);
        output = redacted;
    }

    let host_redacted =
        PRIVATE_HOST_LABEL_RE.replace_all(&output, |_: &Captures<'_>| REDACTED_REMOTE_HOST_MARKER);
    if let Cow::Owned(redacted) = host_redacted {
        push_full_redaction(
            redactions,
            "remote_host",
            &output,
            REDACTED_REMOTE_HOST_MARKER,
        );
        output = redacted;
    }

    let path_redacted = PRIVATE_PATH_RE.replace_all(&output, |captures: &Captures<'_>| {
        redacted_private_path_marker(&captures[0])
    });
    if let Cow::Owned(redacted) = path_redacted {
        push_full_redaction(
            redactions,
            "private_path",
            &output,
            &format!("{REDACTED_PATH_PREFIX}/<name>"),
        );
        output = redacted;
    }

    output
}

fn redacted_source_label(
    source_id: &str,
    origin_kind: &str,
    redactions: &mut Vec<RenderedRedaction>,
) -> String {
    if is_remote_origin(origin_kind) && !source_id.trim().is_empty() {
        push_full_redaction(redactions, "remote_host", source_id, REDACTED_SOURCE_MARKER);
        REDACTED_SOURCE_MARKER.to_string()
    } else {
        redact_pack_output_text(source_id, redactions)
    }
}

fn rendered_origin_host(
    origin_host: Option<&str>,
    redactions: &mut Vec<RenderedRedaction>,
) -> Option<String> {
    let host = origin_host?;
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return Some(String::new());
    }
    push_full_redaction(redactions, "remote_host", host, REDACTED_REMOTE_HOST_MARKER);
    Some(REDACTED_REMOTE_HOST_MARKER.to_string())
}

fn is_remote_origin(origin_kind: &str) -> bool {
    !origin_kind.trim().eq_ignore_ascii_case("local")
}

fn redacted_private_path_marker(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let components = trimmed
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    let basename = match components.as_slice() {
        ["home", _] | ["Users", _] | [_, "Users", _] | ["~"] => "home",
        _ => components
            .last()
            .copied()
            .filter(|component| *component != "~")
            .unwrap_or("path"),
    };
    format!("{REDACTED_PATH_PREFIX}/{basename}")
}

fn push_full_redaction(
    redactions: &mut Vec<RenderedRedaction>,
    kind: &str,
    original: &str,
    replacement: &str,
) {
    if original.is_empty() {
        return;
    }
    redactions.push(RenderedRedaction {
        kind: kind.to_string(),
        start_char: 0,
        end_char: original.chars().count(),
        replacement: replacement.to_string(),
    });
}

/// Map a pack evidence candidate's source story to the trust source-kind. An
/// unavailable source is archive-only (source-unhealthy) for trust purposes;
/// otherwise classify by origin (live local file vs reachable remote mirror).
fn pack_trust_source_kind(
    readiness: PackSourceReadiness,
    origin_kind: &str,
) -> crate::search::trust_scoring::SourceTrustKind {
    use crate::search::trust_scoring::SourceTrustKind;
    if matches!(readiness, PackSourceReadiness::Unavailable) {
        SourceTrustKind::ArchiveOnly
    } else if origin_kind.eq_ignore_ascii_case(crate::sources::provenance::LOCAL_SOURCE_ID) {
        SourceTrustKind::LocalPresent
    } else {
        SourceTrustKind::RemoteMirror
    }
}

/// Map the pack's realized search mode (the requested mode plus any lexical
/// fallback) to the trust [`RealizedMode`](crate::search::trust_scoring::RealizedMode).
fn pack_trust_realized_mode(
    search_mode: &str,
    fallback_mode: Option<&str>,
) -> crate::search::trust_scoring::RealizedMode {
    use crate::search::trust_scoring::RealizedMode;
    if fallback_mode.is_some_and(|mode| mode.eq_ignore_ascii_case("lexical")) {
        return RealizedMode::Lexical;
    }
    match search_mode.to_ascii_lowercase().as_str() {
        "lexical" => RealizedMode::Lexical,
        "semantic" => RealizedMode::Semantic,
        _ => RealizedMode::Hybrid,
    }
}

/// Build the metadata-only trust verdict for one pack evidence item (beads
/// 5u82n.3 + q4pau). Advisory only; the same scoring core that powers the
/// per-hit `cass search` verdict. Recency/source/mode always contribute;
/// `query_workspace` drives a cwd-relative workspace match, and for on-project
/// evidence `correlation` links the excerpt to a closed bead / commit / proof /
/// release when it references a known identifier.
fn pack_trust_assessment(
    candidate: &PackCandidate,
    request: &PackRenderRequest,
    correlation: &crate::search::trust_correlation::CorrelationIndex,
    query_workspace: Option<&str>,
) -> crate::search::trust_scoring::TrustAssessment {
    use crate::search::trust_correlation::{correlate, proof_for, scan_text, workspace_matches};
    use crate::search::trust_scoring::{HitTrustContext, assess_trust, derive_trust_signals};
    let workspace = {
        let ws = candidate.workspace.trim();
        (!ws.is_empty()).then(|| ws.to_string())
    };
    let on_project = match (query_workspace, workspace.as_deref()) {
        (None, _) => None,
        (Some(q), Some(w)) => Some(workspace_matches(q, w)),
        (Some(_), None) => Some(false),
    };
    let mut ctx = HitTrustContext {
        created_at_ms: candidate.created_at_ms,
        now_ms: request.generated_at_ms,
        workspace,
        query_workspace: None,
        source_kind: pack_trust_source_kind(candidate.source_readiness, &candidate.origin_kind),
        realized_mode: pack_trust_realized_mode(
            &request.search_mode,
            request.fallback_mode.as_deref(),
        ),
        ..HitTrustContext::default()
    };
    if matches!(on_project, Some(true)) {
        let scan = scan_text(&[&candidate.excerpt]);
        let link = correlate(correlation, &scan);
        if !link.is_empty() {
            ctx.outcome = link.outcome;
            ctx.linked_closed_bead = link.linked_closed_bead;
            ctx.linked_lessons = link.linked_lessons;
            if let Some(commit) = link.linked_commit {
                ctx.release_tag = correlation.release_tag_for_commit(&commit);
                ctx.linked_commit = Some(commit);
            }
            ctx.proof = proof_for(
                ctx.outcome,
                ctx.linked_commit.is_some(),
                ctx.release_tag.is_some(),
            );
        }
    }
    let mut signals = derive_trust_signals(&ctx);
    signals.workspace_match = on_project.unwrap_or(true);
    assess_trust(&signals)
}

fn rendered_evidence(
    item: &PlannedPackEvidence,
    request: &PackRenderRequest,
    correlation: &crate::search::trust_correlation::CorrelationIndex,
    query_workspace: Option<&str>,
) -> RenderedEvidence {
    let candidate = &item.candidate;
    let mut redactions = Vec::new();
    let excerpt = redact_pack_output_text(&item.excerpt, &mut redactions);
    let source_id = redacted_source_label(
        &candidate.source_id,
        &candidate.origin_kind,
        &mut redactions,
    );
    let source_path = redact_pack_output_text(&candidate.source_path, &mut redactions);
    let workspace = redact_pack_output_text(&candidate.workspace, &mut redactions);
    let workspace_original = candidate
        .workspace_original
        .as_deref()
        .map(|workspace| redact_pack_output_text(workspace, &mut redactions));
    let origin_host = rendered_origin_host(candidate.origin_host.as_deref(), &mut redactions);
    let citation = RenderedCitation {
        source_path,
        source_id,
        origin_kind: candidate.origin_kind.clone(),
        origin_host,
        workspace,
        workspace_original,
        agent: redact_pack_output_text(&candidate.agent, &mut redactions),
        line_start: candidate.line_start,
        line_end: candidate.line_end,
        message_index: candidate.message_index,
        conversation_id: candidate.conversation_id,
        content_hash: candidate.content_hash.clone(),
        span_hash: candidate.span_hash.clone(),
        excerpt_sha256: sha256_hex(&item.excerpt),
        created_at_ms: candidate.created_at_ms,
        indexed_at_ms: candidate.indexed_at_ms,
        freshness_age_seconds: candidate
            .created_at_ms
            .map(|created| request.generated_at_ms.saturating_sub(created).max(0) / 1_000),
        match_type: candidate.match_type.clone(),
        verified: candidate.line_start.is_some() && !candidate.source_path.trim().is_empty(),
    };
    let trust = pack_trust_assessment(candidate, request, correlation, query_workspace);
    RenderedEvidence {
        id: item.id.clone(),
        rank: item.rank,
        excerpt,
        excerpt_truncated: item.excerpt_truncated,
        estimated_tokens: item.estimated_tokens,
        citation,
        trust,
        selection: rendered_selection(item.selection, request.explain_selection),
        roles: rendered_roles(candidate.role),
        matched_terms: candidate
            .matched_terms
            .iter()
            .map(|term| redact_pack_output_text(term, &mut redactions))
            .collect(),
        redactions,
        source_readiness: candidate.source_readiness,
    }
}

fn rendered_selection(selection: PackSelectionScore, explain: bool) -> RenderedSelection {
    RenderedSelection {
        score: selection.score,
        token_cost: selection.token_cost,
        selected_reason: selection.selected_reason,
        relevance_score: explain.then_some(selection.relevance_score),
        coverage_score: explain.then_some(selection.coverage_score),
        freshness_score: explain.then_some(selection.freshness_score),
        source_diversity_score: explain.then_some(selection.source_diversity_score),
        source_authority_score: explain.then_some(selection.source_authority_score),
        role_score: explain.then_some(selection.role_score),
        citation_quality_score: explain.then_some(selection.citation_quality_score),
        duplicate_penalty: explain.then_some(selection.duplicate_penalty),
    }
}

fn rendered_roles(role: PackEvidenceRole) -> Vec<&'static str> {
    if matches!(role, PackEvidenceRole::Unknown) {
        Vec::new()
    } else {
        vec![evidence_role_label(role)]
    }
}

fn rendered_outline(evidence: &[RenderedEvidence]) -> Vec<RenderedOutlineItem> {
    evidence
        .iter()
        .map(|item| RenderedOutlineItem {
            rank: item.rank,
            heading: outline_heading(item),
            evidence_ids: vec![item.id.clone()],
        })
        .collect()
}

fn outline_heading(item: &RenderedEvidence) -> String {
    item.matched_terms
        .first()
        .map(|term| format!("Evidence for {term}"))
        .unwrap_or_else(|| {
            format!(
                "{} evidence from {}",
                item.citation.agent, item.citation.source_id
            )
        })
}

fn rendered_handoff(evidence: &[RenderedEvidence]) -> Vec<RenderedHandoffItem> {
    evidence
        .iter()
        .map(|item| RenderedHandoffItem {
            rank: item.rank,
            kind: handoff_kind(item),
            text: compact_excerpt(&item.excerpt, 220),
            evidence_ids: vec![item.id.clone()],
        })
        .collect()
}

fn handoff_kind(item: &RenderedEvidence) -> &'static str {
    match item.roles.first().copied() {
        Some("assistant_conclusion") => "decision",
        Some("tool_result") => "fact",
        Some("user_requirement") => "next_step",
        Some("tool_call_argument") => "fact",
        _ => "fact",
    }
}

fn rendered_source_summary(evidence: &[RenderedEvidence]) -> Vec<RenderedSourceSummary> {
    let mut sources: BTreeMap<(String, String), SourceAccumulator> = BTreeMap::new();
    for item in evidence {
        let key = (
            item.citation.source_id.clone(),
            item.citation.origin_kind.clone(),
        );
        let entry = sources.entry(key).or_insert_with(|| SourceAccumulator {
            origin_kind: item.citation.origin_kind.clone(),
            healthy: true,
            ..SourceAccumulator::default()
        });
        entry.sessions.insert(item.citation.source_path.clone());
        entry.evidence_count += 1;
        entry.newest_evidence_at_ms =
            newer_timestamp(entry.newest_evidence_at_ms, item.citation.created_at_ms);
        let readiness = item.citation_source_readiness();
        entry.healthy &= matches!(readiness, PackSourceReadiness::Healthy);
        if source_readiness_rank(readiness) > source_readiness_rank(entry.worst_readiness) {
            entry.worst_readiness = readiness;
        }
    }

    sources
        .into_iter()
        .map(|((source_id, _), source)| RenderedSourceSummary {
            source_id,
            origin_kind: source.origin_kind,
            session_count: source.sessions.len(),
            evidence_count: source.evidence_count,
            newest_evidence_at_ms: source.newest_evidence_at_ms,
            healthy: source.healthy,
        })
        .collect()
}

fn rendered_source_readiness(evidence: &[RenderedEvidence]) -> Vec<RenderedSourceReadiness> {
    let mut sources: BTreeMap<(String, String), SourceAccumulator> = BTreeMap::new();
    for item in evidence {
        let key = (
            item.citation.source_id.clone(),
            item.citation.origin_kind.clone(),
        );
        let entry = sources.entry(key).or_insert_with(|| SourceAccumulator {
            origin_kind: item.citation.origin_kind.clone(),
            healthy: true,
            ..SourceAccumulator::default()
        });
        entry.evidence_count += 1;
        let readiness = item.citation_source_readiness();
        entry.healthy &= matches!(readiness, PackSourceReadiness::Healthy);
        if source_readiness_rank(readiness) > source_readiness_rank(entry.worst_readiness) {
            entry.worst_readiness = readiness;
        }
    }

    sources
        .into_iter()
        .map(|((source_id, _), source)| RenderedSourceReadiness {
            source_id,
            origin_kind: source.origin_kind,
            readiness: source_readiness_label(source.worst_readiness),
            healthy: source.healthy,
            evidence_count: source.evidence_count,
        })
        .collect()
}

fn rendered_source_sync_gaps(
    gaps: &[PackSourceSyncGap],
    redactions: &mut Vec<RenderedRedaction>,
) -> Vec<RenderedSourceSyncGap> {
    gaps.iter()
        .map(|gap| RenderedSourceSyncGap {
            source_id: redacted_source_label(&gap.source_id, &gap.origin_kind, redactions),
            origin_kind: gap.origin_kind.clone(),
            kind: source_sync_gap_kind_label(gap.kind),
            lag_seconds: gap.lag_seconds,
            last_synced_at_ms: gap.last_synced_at_ms,
            recommended_action: gap
                .recommended_action
                .as_deref()
                .map(|action| redact_pack_output_text(action, redactions)),
        })
        .collect()
}

fn stale_evidence_count(evidence: &[RenderedEvidence], request: &PackRenderRequest) -> usize {
    evidence
        .iter()
        .filter(|item| evidence_is_stale(item, request))
        .count()
}

fn evidence_is_stale(item: &RenderedEvidence, request: &PackRenderRequest) -> bool {
    let Some(created_at_ms) = item.citation.created_at_ms else {
        return !matches!(request.freshness_policy, PackFreshnessPolicy::AllowStale);
    };
    let window_ms = request
        .freshness_window_seconds
        .max(1)
        .saturating_mul(1_000);
    request.generated_at_ms.saturating_sub(created_at_ms).max(0) > window_ms
}

impl RenderedEvidence {
    fn citation_source_readiness(&self) -> PackSourceReadiness {
        if !self.citation.verified {
            PackSourceReadiness::IncompleteMetadata
        } else {
            self.source_readiness
        }
    }
}

fn newer_timestamp(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn rendered_omitted_items(
    omitted: &[OmittedPackCandidate],
) -> (Vec<OmittedPackCandidate>, Vec<RenderedRedaction>) {
    let mut redactions = Vec::new();
    let items = omitted
        .iter()
        .map(|item| {
            let mut rendered = item.clone();
            rendered.candidate_id =
                rendered_omitted_candidate_id(&rendered.candidate_id, &mut redactions);
            rendered.source_path = redact_pack_output_text(&rendered.source_path, &mut redactions);
            rendered.agent = redact_pack_output_text(&rendered.agent, &mut redactions);
            rendered
        })
        .collect();
    (items, redactions)
}

fn rendered_omitted_candidate_id(
    candidate_id: &str,
    redactions: &mut Vec<RenderedRedaction>,
) -> String {
    if candidate_id_contains_source_material(candidate_id) {
        let hash = sha256_hex(candidate_id);
        let replacement = format!("omitted_{}", &hash[..16]);
        push_full_redaction(redactions, "candidate_id", candidate_id, &replacement);
        return replacement;
    }
    redact_pack_output_text(candidate_id, redactions)
}

fn candidate_id_contains_source_material(candidate_id: &str) -> bool {
    candidate_id.contains(':')
        || candidate_id.contains('/')
        || candidate_id.contains('\\')
        || candidate_id.contains('~')
}

fn redaction_counts(
    redacted_count: usize,
    evidence: &[RenderedEvidence],
    omitted_redactions: &[RenderedRedaction],
    envelope_redactions: &[RenderedRedaction],
) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    if redacted_count > 0 {
        counts.insert("redacted_to_empty".to_string(), redacted_count);
    }
    for redaction in evidence
        .iter()
        .flat_map(|item| item.redactions.iter())
        .chain(omitted_redactions.iter())
        .chain(envelope_redactions.iter())
    {
        *counts.entry(redaction.kind.clone()).or_default() += 1;
    }
    counts
}

fn render_answer_pack_jsonl(
    envelope: &RenderedAnswerPack,
    request: &PackRenderRequest,
) -> Result<String, PackRenderError> {
    let mut lines = Vec::with_capacity(envelope.evidence.len() + 4);
    lines.push(json_line(
        serde_json::json!({
            "_meta": &envelope.meta,
            "budget": &envelope.budget,
        }),
        request,
    )?);
    lines.push(json_line(
        serde_json::json!({ "pack": &envelope.pack }),
        request,
    )?);
    for evidence in &envelope.evidence {
        lines.push(json_line(
            serde_json::json!({ "evidence": evidence }),
            request,
        )?);
    }
    lines.push(json_line(
        serde_json::json!({ "omitted": &envelope.omitted }),
        request,
    )?);
    lines.push(json_line(
        serde_json::json!({ "privacy": &envelope.privacy }),
        request,
    )?);
    Ok(lines.join("\n"))
}

fn json_line(
    value: serde_json::Value,
    request: &PackRenderRequest,
) -> Result<String, PackRenderError> {
    serde_json::to_string(&value).map_err(|err| render_error(request, err))
}

fn render_answer_pack_markdown(envelope: &RenderedAnswerPack) -> String {
    let mut out = String::new();
    out.push_str("# ");
    out.push_str(&markdown_line(&envelope.pack.title));
    if !envelope.warnings.is_empty() {
        out.push_str("\n\n## Warnings\n");
        for warning in &envelope.warnings {
            out.push_str("- ");
            out.push_str(&markdown_line(warning));
            out.push('\n');
        }
    }
    out.push_str("\n## Handoff\n");
    if envelope.pack.handoff.is_empty() {
        out.push_str("- No evidence selected.\n");
    } else {
        for item in &envelope.pack.handoff {
            out.push_str("- ");
            out.push_str(&markdown_line(&item.text));
            out.push_str(" [");
            out.push_str(&item.evidence_ids.join(", "));
            out.push_str("]\n");
        }
    }

    out.push_str("\n## Evidence\n");
    if envelope.evidence.is_empty() {
        out.push_str("- No cited evidence.\n");
    } else {
        for item in &envelope.evidence {
            out.push('[');
            out.push_str(&item.id);
            out.push_str("] ");
            out.push_str(&markdown_line(&item.citation.agent));
            out.push(' ');
            out.push_str(&markdown_line(&item.citation.source_id));
            out.push(' ');
            out.push_str(&markdown_line(&item.citation.source_path));
            if let Some(line_start) = item.citation.line_start {
                out.push(':');
                out.push_str(&line_start.to_string());
                // ubs:ignore — compares public citation line numbers, not secret material.
                if item.citation.line_end != item.citation.line_start
                    && let Some(line_end) = item.citation.line_end
                {
                    out.push('-');
                    out.push_str(&line_end.to_string());
                }
            }
            out.push('\n');
        }
    }

    if envelope.omitted.count > 0 {
        out.push_str("\n## Omitted\n");
        for item in &envelope.omitted.items {
            out.push_str("- ");
            out.push_str(omitted_reason_label(item.reason));
            out.push_str(": ");
            out.push_str(&markdown_line(&item.source_path));
            if let Some(line_start) = item.line_start {
                out.push(':');
                out.push_str(&line_start.to_string());
            }
            out.push('\n');
        }
    }
    out
}

fn compact_excerpt(excerpt: &str, max_chars: usize) -> String {
    let line = markdown_line(excerpt);
    if line.chars().count() <= max_chars {
        return line;
    }
    let mut compact = line
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    compact.push_str("...");
    compact
}

fn markdown_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn pack_toon_encode_options() -> toon::EncodeOptions {
    toon::EncodeOptions {
        indent: None,
        delimiter: None,
        key_folding: Some(toon::options::KeyFoldingMode::Off),
        flatten_depth: None,
        replacer: None,
    }
}

fn freshness_policy_label(policy: PackFreshnessPolicy) -> &'static str {
    match policy {
        PackFreshnessPolicy::PreferRecent => "prefer-recent",
        PackFreshnessPolicy::Strict => "strict",
        PackFreshnessPolicy::AllowStale => "allow-stale",
    }
}

fn evidence_role_label(role: PackEvidenceRole) -> &'static str {
    match role {
        PackEvidenceRole::AssistantConclusion => "assistant_conclusion",
        PackEvidenceRole::ToolResult => "tool_result",
        PackEvidenceRole::UserRequirement => "user_requirement",
        PackEvidenceRole::ToolCallArgument => "tool_call_argument",
        PackEvidenceRole::Unknown => "unknown",
    }
}

fn omitted_reason_label(reason: PackOmittedReason) -> &'static str {
    match reason {
        PackOmittedReason::TokenBudgetExhausted => "token_budget_exhausted",
        PackOmittedReason::MaxSessionsReached => "max_sessions_reached",
        PackOmittedReason::MaxEvidenceReached => "max_evidence_reached",
        PackOmittedReason::DuplicateContent => "duplicate_content",
        PackOmittedReason::SameSessionLowerRank => "same_session_lower_rank",
        PackOmittedReason::StaleUnderStrictPolicy => "stale_under_strict_policy",
        PackOmittedReason::SourceUnavailable => "source_unavailable",
        PackOmittedReason::RedactedToEmpty => "redacted_to_empty",
        PackOmittedReason::FieldMaskExcluded => "field_mask_excluded",
    }
}

fn source_readiness_rank(readiness: PackSourceReadiness) -> usize {
    match readiness {
        PackSourceReadiness::Healthy => 0,
        PackSourceReadiness::StaleReadable => 1,
        PackSourceReadiness::IncompleteMetadata => 2,
        PackSourceReadiness::Unavailable => 3,
    }
}

fn source_readiness_label(readiness: PackSourceReadiness) -> &'static str {
    match readiness {
        PackSourceReadiness::Healthy => "healthy",
        PackSourceReadiness::StaleReadable => "stale_readable",
        PackSourceReadiness::IncompleteMetadata => "incomplete_metadata",
        PackSourceReadiness::Unavailable => "unavailable",
    }
}

fn lexical_readiness_label(readiness: PackLexicalReadiness) -> &'static str {
    match readiness {
        PackLexicalReadiness::Ready => "ready",
        PackLexicalReadiness::Stale => "stale",
        PackLexicalReadiness::Missing => "missing",
        PackLexicalReadiness::Rebuilding => "rebuilding",
        PackLexicalReadiness::Unknown => "unknown",
    }
}

fn semantic_readiness_label(readiness: PackSemanticReadiness) -> &'static str {
    match readiness {
        PackSemanticReadiness::NotReported => "not_reported",
        PackSemanticReadiness::Joined => "joined",
        PackSemanticReadiness::FallbackLexical => "fallback_lexical",
        PackSemanticReadiness::Unavailable => "unavailable",
        PackSemanticReadiness::Disabled => "disabled",
    }
}

fn source_sync_gap_kind_label(kind: PackSourceSyncGapKind) -> &'static str {
    match kind {
        PackSourceSyncGapKind::RemoteStale => "remote_stale",
        PackSourceSyncGapKind::SourcePruned => "source_pruned",
        PackSourceSyncGapKind::SyncDeferred => "sync_deferred",
        PackSourceSyncGapKind::Unknown => "unknown",
    }
}

fn effective_semantic_readiness(request: &PackRenderRequest) -> PackSemanticReadiness {
    if !matches!(
        request.readiness.semantic_readiness,
        PackSemanticReadiness::NotReported
    ) {
        return request.readiness.semantic_readiness;
    }
    if request.semantic_joined {
        return PackSemanticReadiness::Joined;
    }
    if request
        .fallback_mode
        .as_deref()
        .is_some_and(|mode| mode.eq_ignore_ascii_case("lexical"))
    {
        return PackSemanticReadiness::FallbackLexical;
    }
    PackSemanticReadiness::NotReported
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, source_id: &str, source_path: &str, score: f64) -> PackCandidate {
        PackCandidate {
            candidate_id: id.to_string(),
            source_path: source_path.to_string(),
            source_id: source_id.to_string(),
            // ubs:ignore — compares a public source-kind fixture label, not a secret.
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

    fn render_request(format: PackRenderFormat) -> PackRenderRequest {
        PackRenderRequest {
            query_text: "pack handoff".to_string(),
            normalized_query: "pack handoff".to_string(),
            generated_at_ms: 1_060_000,
            elapsed_ms: 7,
            budget: BudgetBlock {
                elapsed_ms: 7,
                budget_ms: 8_000,
                timed_out: false,
                skipped_sections: Vec::new(),
                recommended_next_probe: None,
            },
            request_id: Some("req-1".to_string()),
            format,
            limits: PackPlannerLimits {
                max_tokens: 1_024,
                max_sessions: 8,
                max_evidence: 24,
                context_lines: 3,
                max_excerpt_chars: 80,
            },
            search_mode: "hybrid".to_string(),
            fallback_mode: Some("lexical".to_string()),
            semantic_joined: false,
            freshness_policy: PackFreshnessPolicy::PreferRecent,
            freshness_window_seconds: 60,
            redaction_policy: "strict".to_string(),
            sensitive_output: false,
            skill_content_included: false,
            explain_selection: false,
            readiness: PackReadinessSnapshot::default(),
        }
    }

    fn timed_out_budget() -> BudgetBlock {
        BudgetBlock {
            elapsed_ms: 125,
            budget_ms: 100,
            timed_out: true,
            skipped_sections: vec![
                "semantic_refinement".to_string(),
                "source_health".to_string(),
            ],
            recommended_next_probe: Some("cass health --json".to_string()),
        }
    }

    #[test]
    fn from_search_hit_uses_robot_match_type_spelling() {
        let hit = SearchHit {
            title: "session".to_string(),
            snippet: "fallback".to_string(),
            content: "fallback content".to_string(),
            content_hash: 42,
            conversation_id: Some(7),
            score: 3.5,
            source_path: "/s/fallback.jsonl".to_string(),
            agent: "codex".to_string(),
            workspace: "/work".to_string(),
            workspace_original: None,
            created_at: Some(1_000_000),
            line_number: Some(12),
            match_type: MatchType::ImplicitWildcard,
            source_id: "local".to_string(),
            origin_kind: "local".to_string(),
            origin_host: None,
        };

        let candidate = PackCandidate::from_search_hit(&hit, 1, 0);

        assert_eq!(candidate.match_type, "implicit_wildcard");
    }

    #[test]
    fn render_compact_json_base_pack_matches_golden_shape() {
        let mut item = candidate("base", "local", "/s/base.jsonl", 10.0);
        item.excerpt = "Planner output cites existing evidence.".to_string();
        item.match_type = "implicit_wildcard".to_string();
        let plan = plan_answer_pack(request(vec![item])).unwrap();
        let req = render_request(PackRenderFormat::CompactJson);

        let rendered = render_answer_pack(&plan, &req).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert!(!rendered.contains('\n'));
        assert_eq!(value, render_answer_pack_value(&plan, &req).unwrap());
        assert_eq!(value["schema_version"], "cass.pack.v1");
        assert_eq!(value["_meta"]["format"], "compact");
        assert_eq!(value["_meta"]["partial"], false);
        assert_eq!(
            value["budget"],
            serde_json::json!({
                "elapsed_ms": 7,
                "budget_ms": 8_000,
                "timed_out": false,
                "skipped_sections": [],
                "recommended_next_probe": null,
            })
        );
        assert_eq!(value["query"]["text"], "pack handoff");
        assert_eq!(value["realized"]["fallback_mode"], "lexical");
        assert_eq!(
            value["evidence"][0]["citation"]["source_path"],
            "/s/base.jsonl"
        );
        assert_eq!(
            value["evidence"][0]["citation"]["match_type"],
            "implicit_wildcard"
        );
        assert_eq!(
            value["pack"]["handoff"][0]["evidence_ids"][0],
            value["evidence"][0]["id"]
        );
    }

    #[test]
    fn render_jsonl_empty_pack_matches_golden_line_order() {
        let plan = plan_answer_pack(request(Vec::new())).unwrap();
        let req = render_request(PackRenderFormat::Jsonl);

        let rendered = render_answer_pack(&plan, &req).unwrap();
        let lines: Vec<_> = rendered.lines().collect();

        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("{\"_meta\":"));
        assert!(lines[1].starts_with("{\"pack\":"));
        assert!(lines[2].starts_with("{\"omitted\":"));
        assert!(lines[3].starts_with("{\"privacy\":"));
        let meta: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let omitted: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(
            meta["_meta"]["warnings"],
            serde_json::json!(["no_evidence_found", "semantic_fallback_lexical"])
        );
        assert_eq!(meta["_meta"]["partial"], false);
        assert_eq!(meta["budget"]["budget_ms"], 8_000);
        assert_eq!(meta["budget"]["skipped_sections"], serde_json::json!([]));
        assert_eq!(omitted["omitted"]["count"], 0);
    }

    #[test]
    fn render_budget_timeout_is_partial_across_structured_formats() -> Result<(), String> {
        let plan = plan_answer_pack(request(vec![candidate(
            "budget-timeout",
            "local",
            "/s/budget-timeout.jsonl",
            10.0,
        )]))
        .map_err(|err| format!("{err:?}"))?;
        let expected_budget = serde_json::json!({
            "elapsed_ms": 125,
            "budget_ms": 100,
            "timed_out": true,
            "skipped_sections": ["semantic_refinement", "source_health"],
            "recommended_next_probe": "cass health --json",
        });

        for format in [PackRenderFormat::Json, PackRenderFormat::CompactJson] {
            let mut req = render_request(format);
            req.budget = timed_out_budget();

            let rendered = render_answer_pack(&plan, &req)
                .map_err(|err| format!("{} render failed: {}", err.format, err.message))?;
            let value: serde_json::Value =
                serde_json::from_str(&rendered).map_err(|err| err.to_string())?;

            assert_eq!(value["_meta"]["partial"], true);
            assert_eq!(value["budget"], expected_budget);
            let evidence = value["evidence"]
                .as_array()
                .ok_or_else(|| "structured pack evidence must be an array".to_string())?;
            assert_eq!(evidence.len(), 1);
        }

        let mut jsonl_req = render_request(PackRenderFormat::Jsonl);
        jsonl_req.budget = timed_out_budget();
        let jsonl = render_answer_pack(&plan, &jsonl_req)
            .map_err(|err| format!("{} render failed: {}", err.format, err.message))?;
        let header_line = jsonl
            .lines()
            .next()
            .ok_or_else(|| "JSONL pack must include a header line".to_string())?;
        let header: serde_json::Value =
            serde_json::from_str(header_line).map_err(|err| err.to_string())?;
        assert_eq!(header["_meta"]["partial"], true);
        assert_eq!(header["budget"], expected_budget);

        let mut toon_req = render_request(PackRenderFormat::Toon);
        toon_req.budget = timed_out_budget();
        let value = render_answer_pack_value(&plan, &toon_req)
            .map_err(|err| format!("{} render failed: {}", err.format, err.message))?;
        let toon = render_answer_pack(&plan, &toon_req)
            .map_err(|err| format!("{} render failed: {}", err.format, err.message))?;
        assert_eq!(value["_meta"]["partial"], true);
        assert_eq!(value["budget"], expected_budget);
        assert_eq!(toon, toon::encode(value, Some(pack_toon_encode_options())));
        Ok(())
    }

    #[test]
    fn render_budget_skips_are_partial_without_timeout() -> Result<(), String> {
        let plan = plan_answer_pack(request(Vec::new())).map_err(|err| format!("{err:?}"))?;
        let mut req = render_request(PackRenderFormat::Json);
        req.budget = BudgetBlock {
            elapsed_ms: 75,
            budget_ms: 100,
            timed_out: false,
            skipped_sections: vec!["selection_explanations".to_string()],
            recommended_next_probe: Some("cass pack --help".to_string()),
        };

        let value = render_answer_pack_value(&plan, &req)
            .map_err(|err| format!("{} render failed: {}", err.format, err.message))?;

        assert_eq!(value["_meta"]["partial"], true);
        assert_eq!(value["budget"]["timed_out"], false);
        assert_eq!(
            value["budget"]["skipped_sections"],
            serde_json::json!(["selection_explanations"])
        );
        Ok(())
    }

    #[test]
    fn render_markdown_duplicate_omission_matches_golden_text() {
        let first = candidate("a", "local", "/s/a.jsonl", 10.0);
        let mut duplicate = candidate("b", "local", "/s/b.jsonl", 9.0);
        duplicate.content_hash = first.content_hash.clone();
        let plan = plan_answer_pack(request(vec![first, duplicate])).unwrap();
        let req = render_request(PackRenderFormat::Markdown);
        let evidence_id = &plan.evidence[0].id;

        let rendered = render_answer_pack(&plan, &req).unwrap();

        assert_eq!(
            rendered,
            format!(
                "# pack handoff\n\n\
                 ## Warnings\n\
                 - semantic_fallback_lexical\n\n\
                 ## Handoff\n\
                 - 0123456789abcdef [{evidence_id}]\n\n\
                 ## Evidence\n\
                 [{evidence_id}] codex local /s/a.jsonl:10-12\n\n\
                 ## Omitted\n\
                 - duplicate_content: /s/b.jsonl:10\n"
            )
        );
    }

    #[test]
    fn render_stale_source_pack_marks_health_and_freshness() {
        let mut stale = candidate("stale", "remote", "/s/stale.jsonl", 10.0);
        stale.source_readiness = PackSourceReadiness::StaleReadable;
        let plan = plan_answer_pack(request(vec![stale])).unwrap();
        let req = render_request(PackRenderFormat::Json);

        let value = render_answer_pack_value(&plan, &req).unwrap();

        assert_eq!(value["health"]["healthy"], false);
        assert_eq!(
            value["health"]["recommended_action"],
            "inspect cass health --json and source sync status"
        );
        assert_eq!(
            value["health"]["source_readiness"][0]["readiness"],
            "stale_readable"
        );
        assert_eq!(value["freshness"]["stale_evidence_count"], 0);
        assert_eq!(value["pack"]["source_summary"][0]["healthy"], false);
    }

    #[test]
    fn render_stale_evidence_count_uses_age_not_source_readiness() {
        let mut old_healthy = candidate("old-healthy", "local", "/s/old.jsonl", 10.0);
        old_healthy.created_at_ms = Some(999_999);
        let mut recent_stale_source =
            candidate("recent-stale-source", "remote", "/s/recent.jsonl", 9.0);
        recent_stale_source.created_at_ms = Some(1_060_000);
        recent_stale_source.source_readiness = PackSourceReadiness::StaleReadable;

        let plan = plan_answer_pack(request(vec![old_healthy, recent_stale_source])).unwrap();
        let req = render_request(PackRenderFormat::Json);

        let value = render_answer_pack_value(&plan, &req).unwrap();

        assert_eq!(value["freshness"]["window_seconds"], 60);
        assert_eq!(value["freshness"]["stale_evidence_count"], 1);
        assert_eq!(value["health"]["healthy"], false);
        assert!(
            value["health"]["source_readiness"]
                .as_array()
                .unwrap()
                .iter()
                // ubs:ignore — compares a public readiness wire label, not a secret.
                .any(|source| source["readiness"] == "stale_readable")
        );
    }

    #[test]
    fn render_healthy_readiness_reports_generation_and_ready_states() {
        let plan = plan_answer_pack(request(vec![candidate(
            "healthy",
            "local",
            "/s/healthy.jsonl",
            10.0,
        )]))
        .unwrap();
        let mut req = render_request(PackRenderFormat::Json);
        req.fallback_mode = None;
        req.semantic_joined = true;
        req.readiness.index_generation = Some("lexical-generation-42".to_string());
        req.readiness.semantic_readiness = PackSemanticReadiness::Joined;

        let value = render_answer_pack_value(&plan, &req).unwrap();

        assert_eq!(value["health"]["healthy"], true);
        assert_eq!(value["health"]["index_state"], "ready");
        assert_eq!(value["health"]["index_generation"], "lexical-generation-42");
        assert_eq!(value["health"]["lexical_readiness"], "ready");
        assert_eq!(value["health"]["semantic_state"], "joined");
        assert_eq!(
            value["health"]["recommended_action"],
            serde_json::Value::Null
        );
        assert_eq!(value["warnings"], serde_json::json!([]));
    }

    #[test]
    fn render_stale_lexical_readiness_reports_action() {
        let plan = plan_answer_pack(request(vec![candidate(
            "stale-index",
            "local",
            "/s/stale-index.jsonl",
            10.0,
        )]))
        .unwrap();
        let mut req = render_request(PackRenderFormat::Json);
        req.readiness.lexical_readiness = PackLexicalReadiness::Stale;

        let value = render_answer_pack_value(&plan, &req).unwrap();

        assert_eq!(value["health"]["healthy"], false);
        assert_eq!(value["health"]["index_state"], "stale");
        assert_eq!(value["health"]["lexical_readiness"], "stale");
        assert_eq!(
            value["health"]["recommended_action"],
            "refresh lexical index with cass index --full"
        );
        assert_eq!(value["warnings"][0], "lexical_index_stale");
    }

    #[test]
    fn render_semantic_unavailable_keeps_lexical_health_truthful() {
        let plan = plan_answer_pack(request(vec![candidate(
            "semantic",
            "local",
            "/s/semantic.jsonl",
            10.0,
        )]))
        .unwrap();
        let mut req = render_request(PackRenderFormat::Json);
        req.readiness.semantic_readiness = PackSemanticReadiness::Unavailable;

        let value = render_answer_pack_value(&plan, &req).unwrap();

        assert_eq!(value["health"]["healthy"], true);
        assert_eq!(value["health"]["semantic_state"], "unavailable");
        assert_eq!(
            value["health"]["recommended_action"],
            "continue with lexical evidence or install semantic model explicitly"
        );
        assert_eq!(
            value["warnings"],
            serde_json::json!(["semantic_unavailable_lexical_fallback"])
        );
    }

    #[test]
    fn render_active_rebuild_marks_pack_not_fresh() {
        let plan = plan_answer_pack(request(vec![candidate(
            "rebuild",
            "local",
            "/s/rebuild.jsonl",
            10.0,
        )]))
        .unwrap();
        let mut req = render_request(PackRenderFormat::Json);
        req.readiness.active_rebuild = true;
        req.readiness.lock_state = Some("writer-lock".to_string());

        let value = render_answer_pack_value(&plan, &req).unwrap();

        assert_eq!(value["health"]["healthy"], false);
        assert_eq!(value["health"]["active_rebuild"], true);
        assert_eq!(value["health"]["lock_state"], "writer-lock");
        assert_eq!(
            value["health"]["recommended_action"],
            "wait for active rebuild or inspect cass status --json"
        );
        assert!(
            value["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning == "active_rebuild")
        );
    }

    #[test]
    fn render_missing_db_reports_reindex_action() {
        let plan = plan_answer_pack(request(Vec::new())).unwrap();
        let mut req = render_request(PackRenderFormat::Json);
        req.readiness.missing_database = true;
        req.readiness.lexical_readiness = PackLexicalReadiness::Missing;

        let value = render_answer_pack_value(&plan, &req).unwrap();

        assert_eq!(value["health"]["healthy"], false);
        assert_eq!(value["health"]["missing_database"], true);
        assert_eq!(value["health"]["index_state"], "missing");
        assert_eq!(
            value["health"]["recommended_action"],
            "run cass index --full"
        );
        assert!(
            value["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning == "missing_database")
        );
    }

    #[test]
    fn render_source_sync_gap_and_pruned_source_metadata() {
        let plan = plan_answer_pack(request(vec![candidate(
            "remote-gap",
            "remote",
            "/s/remote-gap.jsonl",
            10.0,
        )]))
        .unwrap();
        let mut req = render_request(PackRenderFormat::Json);
        req.readiness.source_sync_gaps = vec![
            PackSourceSyncGap {
                source_id: "remote".to_string(),
                origin_kind: "ssh".to_string(),
                kind: PackSourceSyncGapKind::RemoteStale,
                lag_seconds: Some(3_600),
                last_synced_at_ms: Some(1_000_000),
                recommended_action: Some("run cass sources sync --json".to_string()),
            },
            PackSourceSyncGap {
                source_id: "old-laptop".to_string(),
                origin_kind: "ssh".to_string(),
                kind: PackSourceSyncGapKind::SourcePruned,
                lag_seconds: None,
                last_synced_at_ms: None,
                recommended_action: Some("remove or refresh pruned source".to_string()),
            },
        ];

        let value = render_answer_pack_value(&plan, &req).unwrap();

        assert_eq!(value["health"]["healthy"], false);
        assert_eq!(
            value["health"]["source_sync_gaps"][0]["kind"],
            "remote_stale"
        );
        assert_eq!(
            value["health"]["source_sync_gaps"][1]["kind"],
            "source_pruned"
        );
        assert_eq!(
            value["health"]["recommended_action"],
            "inspect cass sources sync --json and source status"
        );
        assert!(
            value["warnings"]
                .as_array()
                .unwrap()
                .iter()
                // ubs:ignore — compares a public diagnostic fixture string, not a secret.
                .any(|warning| warning == "source_sync_gap:[REDACTED_SOURCE]:source_pruned")
        );
        assert_eq!(
            value["health"]["source_sync_gaps"][1]["source_id"],
            REDACTED_SOURCE_MARKER
        );
    }

    #[test]
    fn render_pack_redacts_freeform_query_and_health_strings() {
        let home_path = "/home/alice/projects/private";
        let host = "alice-workstation.internal";
        let token = format!("sk-{}", "abcdefghijklmnopqrstuv");
        let plan = plan_answer_pack(request(vec![candidate(
            "health-redaction",
            "local",
            "/s/health.jsonl",
            10.0,
        )]))
        .unwrap();
        let mut req = render_request(PackRenderFormat::Json);
        req.query_text = format!("investigate {token} at {home_path}");
        req.normalized_query = req.query_text.clone();
        req.readiness.lexical_readiness = PackLexicalReadiness::Stale;
        req.readiness.index_generation = Some(format!("generation from {home_path}"));
        req.readiness.lock_state = Some(format!("writer lock held by {host}"));
        req.readiness.recommended_action = Some(format!("inspect {home_path} on {host}"));
        req.readiness.source_sync_gaps = vec![PackSourceSyncGap {
            source_id: host.to_string(),
            origin_kind: "ssh".to_string(),
            kind: PackSourceSyncGapKind::RemoteStale,
            lag_seconds: Some(60),
            last_synced_at_ms: Some(1_000_000),
            recommended_action: Some(format!("sync {home_path} from {host}")),
        }];

        let rendered = render_answer_pack(&plan, &req).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        for raw in [&token, home_path, host] {
            assert!(!rendered.contains(raw));
        }
        assert_eq!(
            value["query"]["text"],
            "investigate [REDACTED] at [REDACTED_PATH]/private"
        );
        assert_eq!(value["pack"]["title"], value["query"]["normalized"]);
        assert_eq!(
            value["health"]["source_sync_gaps"][0]["source_id"],
            REDACTED_SOURCE_MARKER
        );
        assert!(
            value["health"]["recommended_action"]
                .as_str()
                .unwrap()
                .contains("[REDACTED_PATH]/private")
        );
        assert!(
            value["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|warning| warning == "privacy_redactions_applied")
        );
        assert!(
            value["privacy"]["redaction_counts"]["private_path"]
                .as_u64()
                .unwrap()
                >= 3
        );
    }

    #[test]
    fn render_redacted_empty_pack_reports_privacy_counts() {
        let mut redacted = candidate("redacted", "local", "/s/redacted.jsonl", 9.0);
        redacted.excerpt = " \n\t ".to_string();
        let plan = plan_answer_pack(request(vec![redacted])).unwrap();
        let req = render_request(PackRenderFormat::Json);

        let value = render_answer_pack_value(&plan, &req).unwrap();

        assert_eq!(value["privacy"]["redaction_applied"], true);
        assert_eq!(value["privacy"]["redaction_counts"]["redacted_to_empty"], 1);
        assert_eq!(value["omitted"]["items"][0]["reason"], "redacted_to_empty");
        assert_eq!(
            value["warnings"],
            serde_json::json!([
                "no_evidence_found",
                "semantic_fallback_lexical",
                "privacy_redactions_applied"
            ])
        );
    }

    #[test]
    fn render_pack_redacts_api_keys_and_bearer_tokens_in_json_and_markdown() {
        let api_key = format!("sk-{}", "12345678901234567890");
        let bearer = "Bearer abcdefghijklmnopqrst";
        let mut secret = candidate("secret", "local", "/s/secret.jsonl", 10.0);
        secret.excerpt = format!("Use {api_key} and Authorization: {bearer}.");
        let plan = plan_answer_pack(request(vec![secret])).unwrap();
        let json_req = render_request(PackRenderFormat::Json);
        let markdown_req = render_request(PackRenderFormat::Markdown);

        let json_rendered = render_answer_pack(&plan, &json_req).unwrap();
        let markdown_rendered = render_answer_pack(&plan, &markdown_req).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_rendered).unwrap();

        assert!(!json_rendered.contains(&api_key));
        assert!(!json_rendered.contains(bearer));
        assert!(!markdown_rendered.contains(&api_key));
        assert!(!markdown_rendered.contains(bearer));
        assert!(json_rendered.contains(REDACTED_VALUE_MARKER));
        assert!(markdown_rendered.contains(REDACTED_VALUE_MARKER));
        assert_eq!(value["privacy"]["redaction_applied"], true);
        assert_eq!(value["privacy"]["redaction_counts"]["secret"], 1);
        assert_eq!(value["evidence"][0]["redactions"][0]["kind"], "secret");
    }

    #[test]
    fn render_pack_redacts_home_directory_paths_in_evidence_and_omitted_output() {
        let source_path = "/home/alice/projects/private/session.jsonl";
        let duplicate_path = "/Users/alice/projects/private/duplicate.jsonl";
        let mut first = candidate("private-path", "local", source_path, 10.0);
        first.workspace = "/home/alice/projects/private".to_string();
        first.workspace_original = Some("/home/alice/projects/private".to_string());
        first.excerpt = format!("Open {source_path} before reading ~/notes/private.md");
        let mut duplicate = candidate("duplicate-private-path", "local", duplicate_path, 9.0);
        duplicate.candidate_id = format!("old-laptop:{duplicate_path}:10");
        duplicate.content_hash = first.content_hash.clone();
        let plan = plan_answer_pack(request(vec![first, duplicate])).unwrap();
        let json_req = render_request(PackRenderFormat::Json);
        let markdown_req = render_request(PackRenderFormat::Markdown);

        let json_rendered = render_answer_pack(&plan, &json_req).unwrap();
        let markdown_rendered = render_answer_pack(&plan, &markdown_req).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_rendered).unwrap();

        for raw in ["/home/alice", "/Users/alice", "~/notes", "old-laptop"] {
            assert!(!json_rendered.contains(raw));
            assert!(!markdown_rendered.contains(raw));
        }
        assert_eq!(
            value["evidence"][0]["citation"]["source_path"],
            "[REDACTED_PATH]/session.jsonl"
        );
        assert_eq!(
            value["omitted"]["items"][0]["source_path"],
            "[REDACTED_PATH]/duplicate.jsonl"
        );
        assert!(
            value["omitted"]["items"][0]["candidate_id"]
                .as_str()
                .unwrap()
                .starts_with("omitted_")
        );
        assert!(markdown_rendered.contains("[REDACTED_PATH]/session.jsonl"));
        assert!(markdown_rendered.contains("[REDACTED_PATH]/duplicate.jsonl"));
        assert_eq!(value["privacy"]["redaction_applied"], true);
        assert!(
            value["privacy"]["redaction_counts"]["private_path"]
                .as_u64()
                .unwrap()
                >= 2
        );
        assert_eq!(value["privacy"]["redaction_counts"]["candidate_id"], 1);
    }

    #[test]
    fn render_pack_redacts_remote_host_details_from_citation_contract() {
        let mut remote = candidate(
            "remote-host",
            "alice-workstation.internal",
            "/s/remote.jsonl",
            10.0,
        );
        remote.origin_kind = "ssh".to_string();
        remote.origin_host = Some("alice@workstation.internal".to_string());
        let plan = plan_answer_pack(request(vec![remote])).unwrap();
        let req = render_request(PackRenderFormat::Json);

        let rendered = render_answer_pack(&plan, &req).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert!(!rendered.contains("alice-workstation.internal"));
        assert!(!rendered.contains("alice@workstation.internal"));
        assert_eq!(
            value["evidence"][0]["citation"]["source_id"],
            REDACTED_SOURCE_MARKER
        );
        assert_eq!(
            value["evidence"][0]["citation"]["origin_host"],
            REDACTED_REMOTE_HOST_MARKER
        );
        assert_eq!(value["privacy"]["redaction_counts"]["remote_host"], 2);
    }

    #[test]
    fn render_pack_redacts_encrypted_payload_material() {
        let payload = "encrypted_payload_material=abcdef0123456789abcdef0123456789";
        let mut encrypted = candidate("encrypted", "local", "/s/chatgpt.jsonl", 10.0);
        encrypted.agent = "chatgpt".to_string();
        encrypted.excerpt = format!("Skipped encrypted ChatGPT block: {payload}.");
        let plan = plan_answer_pack(request(vec![encrypted])).unwrap();
        let json_req = render_request(PackRenderFormat::Json);
        let markdown_req = render_request(PackRenderFormat::Markdown);

        let json_rendered = render_answer_pack(&plan, &json_req).unwrap();
        let markdown_rendered = render_answer_pack(&plan, &markdown_req).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json_rendered).unwrap();

        assert!(!json_rendered.contains(payload));
        assert!(!markdown_rendered.contains(payload));
        assert!(json_rendered.contains(REDACTED_ENCRYPTED_PAYLOAD_MARKER));
        assert!(markdown_rendered.contains(REDACTED_ENCRYPTED_PAYLOAD_MARKER));
        assert_eq!(value["privacy"]["redaction_counts"]["encrypted_payload"], 1);
        assert_eq!(
            value["evidence"][0]["redactions"][0]["replacement"],
            REDACTED_ENCRYPTED_PAYLOAD_MARKER
        );
    }

    #[test]
    fn render_toon_matches_existing_toon_encoder() {
        let plan = plan_answer_pack(request(vec![candidate(
            "toon",
            "local",
            "/s/toon.jsonl",
            10.0,
        )]))
        .unwrap();
        let req = render_request(PackRenderFormat::Toon);
        let value = render_answer_pack_value(&plan, &req).unwrap();

        let rendered = render_answer_pack(&plan, &req).unwrap();

        assert_eq!(
            rendered,
            toon::encode(value, Some(pack_toon_encode_options()))
        );
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
    fn unavailable_and_redacted_empty_candidates_are_omitted_once() {
        let mut unavailable = candidate("unavailable", "remote", "/s/down.jsonl", 10.0);
        unavailable.source_readiness = PackSourceReadiness::Unavailable;
        let mut redacted = candidate("redacted", "local", "/s/redacted.jsonl", 9.0);
        redacted.excerpt = " \n\t ".to_string();

        let plan = plan_answer_pack(request(vec![unavailable, redacted])).unwrap();

        assert!(plan.evidence.is_empty());
        let omitted_reasons: Vec<_> = plan
            .omitted
            .iter()
            .map(|omitted| (omitted.candidate_id.as_str(), omitted.reason))
            .collect();
        assert_eq!(
            omitted_reasons,
            vec![
                ("unavailable", PackOmittedReason::SourceUnavailable),
                ("redacted", PackOmittedReason::RedactedToEmpty),
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
    fn session_cap_omits_new_sessions_but_allows_existing_session_evidence() {
        let first = candidate("a", "local", "/s/a.jsonl", 10.0);
        let mut same_session = candidate("b", "local", "/s/a.jsonl", 9.0);
        same_session.line_start = Some(20);
        same_session.line_end = Some(22);
        let new_session = candidate("c", "remote", "/s/c.jsonl", 8.0);

        let mut req = request(vec![first, same_session, new_session]);
        req.limits.max_sessions = 1;
        req.limits.max_evidence = 3;

        let plan = plan_answer_pack(req).unwrap();

        let selected_ids: Vec<_> = plan
            .evidence
            .iter()
            .map(|evidence| evidence.candidate.candidate_id.as_str())
            .collect();
        assert_eq!(selected_ids, vec!["a", "b"]);
        assert_eq!(plan.omitted.len(), 1);
        assert_eq!(plan.omitted[0].candidate_id, "c");
        assert_eq!(
            plan.omitted[0].reason,
            PackOmittedReason::MaxSessionsReached
        );
    }

    #[test]
    fn evidence_cap_omits_remaining_candidates_once() {
        let first = candidate("a", "local", "/s/a.jsonl", 10.0);
        let second = candidate("b", "remote", "/s/b.jsonl", 9.0);
        let third = candidate("c", "remote", "/s/c.jsonl", 8.0);

        let mut req = request(vec![first, second, third]);
        req.limits.max_evidence = 1;

        let plan = plan_answer_pack(req).unwrap();

        assert_eq!(plan.evidence.len(), 1);
        assert_eq!(plan.evidence[0].candidate.candidate_id, "a");
        let omitted_reasons: Vec<_> = plan
            .omitted
            .iter()
            .map(|omitted| (omitted.candidate_id.as_str(), omitted.reason))
            .collect();
        assert_eq!(
            omitted_reasons,
            vec![
                ("b", PackOmittedReason::MaxEvidenceReached),
                ("c", PackOmittedReason::MaxEvidenceReached),
            ]
        );
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
                // ubs:ignore — compares a public omission enum, not secret material.
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
