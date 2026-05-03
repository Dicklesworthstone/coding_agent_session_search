//! Layered explanation cards for robot-visible controller decisions.
//!
//! Cards are intentionally compact by default. Each card has a plain summary
//! plus optional input, evidence, and fallback-contract fields so robot callers
//! can decide whether a decision was expected, degraded, or needs operator
//! action without reverse-engineering scattered metadata fields.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub const EXPLANATION_CARD_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplanationSurface {
    SearchRobot,
    HealthRobot,
    StatusRobot,
    SourceSync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplanationDecision {
    SearchFallback,
    CacheAdmission,
    RebuildThrottle,
    SemanticUnavailable,
    SourceSyncDeferred,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplanationFallbackContract {
    pub fail_open: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realized_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_trigger: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplanationCard {
    pub schema_version: u32,
    pub card_id: String,
    pub surface: ExplanationSurface,
    pub decision: ExplanationDecision,
    pub level: u8,
    pub summary: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, Value>,
    #[serde(default)]
    pub evidence: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_contract: Option<ExplanationFallbackContract>,
}

impl ExplanationCard {
    fn new(
        card_id: impl Into<String>,
        surface: ExplanationSurface,
        decision: ExplanationDecision,
        level: u8,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: EXPLANATION_CARD_SCHEMA_VERSION,
            card_id: card_id.into(),
            surface,
            decision,
            level,
            summary: summary.into(),
            inputs: BTreeMap::new(),
            evidence: BTreeMap::new(),
            fallback_contract: None,
        }
    }

    fn input(mut self, key: impl Into<String>, value: Value) -> Self {
        self.inputs.insert(key.into(), value);
        self
    }

    fn evidence(mut self, key: impl Into<String>, value: Value) -> Self {
        self.evidence.insert(key.into(), value);
        self
    }

    fn fallback_contract(mut self, contract: ExplanationFallbackContract) -> Self {
        self.fallback_contract = Some(contract);
        self
    }
}

#[derive(Debug, Clone)]
pub struct SearchRobotExplanationInput {
    pub requested_mode: String,
    pub realized_mode: String,
    pub fallback_tier: Option<String>,
    pub fallback_reason: Option<String>,
    pub semantic_refinement: bool,
    pub wildcard_fallback: bool,
    pub cache_policy: String,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_shortfall: u64,
    pub cache_evictions: u64,
    pub cache_admission_rejects: u64,
    pub cache_ghost_entries: usize,
    pub index_rebuilding: bool,
    pub pending_sessions: Option<u64>,
}

pub fn search_robot_explanation_cards(input: SearchRobotExplanationInput) -> Vec<ExplanationCard> {
    let mut cards = Vec::new();
    if let Some(reason) = input.fallback_reason.as_deref() {
        cards.push(search_fallback_card(
            &input.requested_mode,
            &input.realized_mode,
            input.fallback_tier.as_deref(),
            reason,
            input.semantic_refinement,
        ));
        if reason.to_ascii_lowercase().contains("semantic") {
            cards.push(semantic_unavailable_card(
                None,
                input.fallback_tier.as_deref().unwrap_or("lexical"),
                reason,
                "build semantic assets or rerun with --mode lexical",
            ));
        }
    }
    if input.cache_shortfall > 0 || input.cache_evictions > 0 || input.cache_admission_rejects > 0 {
        cards.push(cache_admission_card(
            &input.cache_policy,
            input.cache_hits,
            input.cache_misses,
            input.cache_shortfall,
            input.cache_evictions,
            input.cache_admission_rejects,
            input.cache_ghost_entries,
        ));
    }
    if input.index_rebuilding {
        cards.push(rebuild_throttle_card(
            input.pending_sessions,
            "index generation is rebuilding; cursor and cache decisions stay conservative",
        ));
    }
    if input.wildcard_fallback {
        cards.push(
            ExplanationCard::new(
                "search.wildcard_fallback",
                ExplanationSurface::SearchRobot,
                ExplanationDecision::SearchFallback,
                0,
                "query broadened automatically after sparse exact matches",
            )
            .input("wildcard_fallback", json!(true))
            .fallback_contract(ExplanationFallbackContract {
                fail_open: true,
                realized_tier: Some("lexical".to_string()),
                operator_action: Some(
                    "quote terms or use explicit wildcards to control breadth".to_string(),
                ),
                rollback_trigger: Some(
                    "unexpected broad matches in the first result page".to_string(),
                ),
            }),
        );
    }
    cards
}

pub fn search_fallback_card(
    requested_mode: &str,
    realized_mode: &str,
    fallback_tier: Option<&str>,
    reason: &str,
    semantic_refinement: bool,
) -> ExplanationCard {
    ExplanationCard::new(
        "search.semantic_fallback",
        ExplanationSurface::SearchRobot,
        ExplanationDecision::SearchFallback,
        1,
        "search mode degraded but results remain available",
    )
    .input("requested_mode", json!(requested_mode))
    .input("realized_mode", json!(realized_mode))
    .input("fallback_tier", json!(fallback_tier))
    .evidence("reason", json!(reason))
    .evidence("semantic_refinement", json!(semantic_refinement))
    .fallback_contract(ExplanationFallbackContract {
        fail_open: true,
        realized_tier: fallback_tier.map(str::to_string),
        operator_action: Some("inspect semantic readiness or run with --mode lexical".to_string()),
        rollback_trigger: Some("strict semantic mode was requested".to_string()),
    })
}

pub fn cache_admission_card(
    policy: &str,
    hits: u64,
    misses: u64,
    shortfall: u64,
    evictions: u64,
    admission_rejects: u64,
    ghost_entries: usize,
) -> ExplanationCard {
    ExplanationCard::new(
        "search.cache_admission",
        ExplanationSurface::SearchRobot,
        ExplanationDecision::CacheAdmission,
        1,
        "cache policy constrained search-result reuse",
    )
    .input("policy", json!(policy))
    .evidence("hits", json!(hits))
    .evidence("misses", json!(misses))
    .evidence("shortfall", json!(shortfall))
    .evidence("evictions", json!(evictions))
    .evidence("admission_rejects", json!(admission_rejects))
    .evidence("ghost_entries", json!(ghost_entries))
    .fallback_contract(ExplanationFallbackContract {
        fail_open: true,
        realized_tier: Some("uncached_search".to_string()),
        operator_action: Some(
            "raise cache byte caps only if repeated-query p95 regresses".to_string(),
        ),
        rollback_trigger: Some(
            "cache pressure increases cold-query latency or RSS beyond budget".to_string(),
        ),
    })
}

pub fn rebuild_throttle_card(pending_sessions: Option<u64>, reason: &str) -> ExplanationCard {
    ExplanationCard::new(
        "index.rebuild_throttle",
        ExplanationSurface::StatusRobot,
        ExplanationDecision::RebuildThrottle,
        1,
        "index rebuild state makes continuation and cache decisions conservative",
    )
    .input("pending_sessions", json!(pending_sessions))
    .evidence("reason", json!(reason))
    .fallback_contract(ExplanationFallbackContract {
        fail_open: true,
        realized_tier: Some("existing_generation".to_string()),
        operator_action: Some(
            "wait for index rebuild to finish before treating cursors as stable".to_string(),
        ),
        rollback_trigger: Some(
            "rebuild remains active beyond the operator's freshness budget".to_string(),
        ),
    })
}

pub fn semantic_unavailable_card(
    requested_model: Option<&str>,
    fallback_mode: &str,
    reason: &str,
    recommended_action: &str,
) -> ExplanationCard {
    ExplanationCard::new(
        "semantic.unavailable",
        ExplanationSurface::HealthRobot,
        ExplanationDecision::SemanticUnavailable,
        1,
        "semantic refinement is unavailable; lexical behavior remains valid",
    )
    .input("requested_model", json!(requested_model))
    .input("fallback_mode", json!(fallback_mode))
    .evidence("reason", json!(reason))
    .fallback_contract(ExplanationFallbackContract {
        fail_open: true,
        realized_tier: Some(fallback_mode.to_string()),
        operator_action: Some(recommended_action.to_string()),
        rollback_trigger: Some("operator requires semantic-only results".to_string()),
    })
}

pub fn source_sync_deferral_card(
    source_id: &str,
    retryable: bool,
    deferred_until_ms: Option<i64>,
    reason: &str,
) -> ExplanationCard {
    ExplanationCard::new(
        "source.sync_deferred",
        ExplanationSurface::SourceSync,
        ExplanationDecision::SourceSyncDeferred,
        1,
        "remote source sync was deferred without blocking local search",
    )
    .input("source_id", json!(source_id))
    .input("retryable", json!(retryable))
    .input("deferred_until_ms", json!(deferred_until_ms))
    .evidence("reason", json!(reason))
    .fallback_contract(ExplanationFallbackContract {
        fail_open: true,
        realized_tier: Some("local_sources".to_string()),
        operator_action: Some("inspect source health and retry the deferred source".to_string()),
        rollback_trigger: Some("remote source is required for the requested audit".to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_search_fallback_card_shape() {
        let card = search_fallback_card(
            "hybrid",
            "lexical",
            Some("lexical"),
            "semantic context unavailable: model missing",
            false,
        );
        let value = serde_json::to_value(card).unwrap();
        assert_eq!(value["schema_version"], EXPLANATION_CARD_SCHEMA_VERSION);
        assert_eq!(value["card_id"], "search.semantic_fallback");
        assert_eq!(value["decision"], "search_fallback");
        assert_eq!(value["inputs"]["requested_mode"], "hybrid");
        assert_eq!(value["fallback_contract"]["fail_open"], true);
    }

    #[test]
    fn pins_cache_admission_card_shape() {
        let card = cache_admission_card("s3-fifo", 4, 9, 2, 3, 1, 7);
        let value = serde_json::to_value(card).unwrap();
        assert_eq!(value["card_id"], "search.cache_admission");
        assert_eq!(value["inputs"]["policy"], "s3-fifo");
        assert_eq!(value["evidence"]["evictions"], 3);
        assert_eq!(value["evidence"]["admission_rejects"], 1);
    }

    #[test]
    fn pins_rebuild_throttle_card_shape() {
        let card = rebuild_throttle_card(Some(42), "rebuild active");
        let value = serde_json::to_value(card).unwrap();
        assert_eq!(value["surface"], "status_robot");
        assert_eq!(value["decision"], "rebuild_throttle");
        assert_eq!(value["inputs"]["pending_sessions"], 42);
    }

    #[test]
    fn pins_semantic_unavailable_card_shape() {
        let card = semantic_unavailable_card(
            Some("minilm"),
            "lexical",
            "model files are absent",
            "run cass models install",
        );
        let value = serde_json::to_value(card).unwrap();
        assert_eq!(value["surface"], "health_robot");
        assert_eq!(value["decision"], "semantic_unavailable");
        assert_eq!(value["inputs"]["requested_model"], "minilm");
        assert_eq!(value["fallback_contract"]["realized_tier"], "lexical");
    }

    #[test]
    fn pins_source_sync_deferral_card_shape() {
        let card = source_sync_deferral_card("workstation", true, Some(1234), "ssh busy");
        let value = serde_json::to_value(card).unwrap();
        assert_eq!(value["surface"], "source_sync");
        assert_eq!(value["decision"], "source_sync_deferred");
        assert_eq!(value["inputs"]["source_id"], "workstation");
        assert_eq!(value["inputs"]["retryable"], true);
    }

    #[test]
    fn search_robot_cards_stay_concise_when_no_decision_needs_explaining() {
        let cards = search_robot_explanation_cards(SearchRobotExplanationInput {
            requested_mode: "hybrid".to_string(),
            realized_mode: "hybrid".to_string(),
            fallback_tier: None,
            fallback_reason: None,
            semantic_refinement: true,
            wildcard_fallback: false,
            cache_policy: "lru".to_string(),
            cache_hits: 0,
            cache_misses: 1,
            cache_shortfall: 0,
            cache_evictions: 0,
            cache_admission_rejects: 0,
            cache_ghost_entries: 0,
            index_rebuilding: false,
            pending_sessions: None,
        });
        assert!(cards.is_empty());
    }

    #[test]
    fn semantic_fallback_detection_is_case_insensitive() {
        let cards = search_robot_explanation_cards(SearchRobotExplanationInput {
            requested_mode: "hybrid".to_string(),
            realized_mode: "lexical".to_string(),
            fallback_tier: Some("lexical".to_string()),
            fallback_reason: Some("Semantic context unavailable: model missing".to_string()),
            semantic_refinement: false,
            wildcard_fallback: false,
            cache_policy: "lru".to_string(),
            cache_hits: 0,
            cache_misses: 0,
            cache_shortfall: 0,
            cache_evictions: 0,
            cache_admission_rejects: 0,
            cache_ghost_entries: 0,
            index_rebuilding: false,
            pending_sessions: None,
        });

        assert!(
            cards
                .iter()
                .any(|card| card.decision == ExplanationDecision::SemanticUnavailable),
            "semantic-unavailable card missing for capitalized reason"
        );
    }
}
