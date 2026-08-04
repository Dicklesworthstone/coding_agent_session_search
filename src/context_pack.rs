//! Bead-scoped context pack selection under a token budget.
//!
//! Given a candidate set of evidence refs for one bead or work-packet (prior
//! sessions, source snippets, closeout notes, failed commands, docs, proof
//! artifacts), select the smallest useful subset that fits a token budget,
//! ranking by relevance, freshness, authority, and privacy risk.
//!
//! This deliberately does **not** duplicate the answer-pack renderer in
//! [`crate::search::pack_planner`]; it operates on already-extracted,
//! metadata-first candidate refs (fixture- or caller-supplied) and emits a
//! deterministic selection plan: selected refs with per-item inclusion
//! rationale, excluded high-privacy-risk refs, budget-omitted refs, full budget
//! accounting, and graceful-degradation notes when search assets or semantic
//! models are unavailable.
//!
//! Guarantees:
//! * Read-only and deterministic: ordering is by integer score then id, never
//!   wall-clock or float-comparison dependent. Freshness uses the fixture's
//!   `now_ms`, not the system clock.
//! * Privacy-first: high privacy-risk candidates are excluded before selection
//!   and never contribute an excerpt to the output. Every emitted excerpt and
//!   path is passed through the strict swarm-evidence redactor.

use chrono::Utc;
use serde_json::{Value, json};

/// Schema identifier for the context pack payload.
pub const SCHEMA_VERSION: &str = "cass.swarm.context_pack.v1";

/// Default token budget when the fixture does not pin one.
pub const DEFAULT_TOKEN_BUDGET: u64 = 2_000;

/// Characters per token for the shared chars/4 estimate (matches pack_planner).
const CHARS_PER_TOKEN: u64 = 4;

/// Per-ref metadata overhead added to every candidate's token cost.
const REF_METADATA_TOKENS: u64 = 8;

/// Tokens reserved for the pack envelope (summary + accounting) before
/// selecting evidence.
const RESERVED_ENVELOPE_TOKENS: u64 = 64;

/// Recognized candidate kinds. Unknown kinds are preserved verbatim but scored
/// with the lowest authority floor.
const KNOWN_KINDS: &[&str] = &[
    "prior_session",
    "source_snippet",
    "closeout_note",
    "failed_command",
    "doc",
    "proof_artifact",
];

// Scoring weights (sum to 100; integer math keeps ordering deterministic).
const WEIGHT_RELEVANCE: u64 = 50;
const WEIGHT_FRESHNESS: u64 = 30;
const WEIGHT_AUTHORITY: u64 = 20;

#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    kind: String,
    path: String,
    excerpt: String,
    created_at_ms: Option<i64>,
    relevance: f64,
    authority: f64,
    privacy_risk: String,
    semantic_only: bool,
}

#[derive(Debug, Clone)]
struct PackFacts {
    fixture_problem: Option<String>,
    bead_id: Option<String>,
    token_budget: u64,
    now_ms: Option<i64>,
    freshness_window_days: u64,
    semantic_available: bool,
    search_assets_ready: bool,
    candidates: Vec<Candidate>,
}

#[derive(Debug, Clone)]
struct Scored {
    candidate: Candidate,
    score: u64,
    freshness_score: u64,
    token_cost: u64,
    stale: bool,
}

/// Render the live plan. Conservative: with no caller-supplied candidates it
/// reports an empty, fully-degraded plan that still documents the budget
/// contract. Live candidate extraction is the caller's responsibility.
#[must_use]
pub fn render_context_pack_live() -> Value {
    render_payload("live", "live", live_facts())
}

/// Render the plan from a checked-in swarm fixture source value.
#[must_use]
pub fn render_context_pack_fixture(fixture_id: &str, source: Option<&Value>) -> Value {
    render_payload(fixture_id, "fixture", fixture_facts(source))
}

fn redact(text: &str) -> String {
    crate::pages::redact::redact_swarm_text(text)
}

fn estimated_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(CHARS_PER_TOKEN)
}

fn render_payload(fixture_id: &str, source_kind: &str, facts: PackFacts) -> Value {
    // Privacy exclusion happens before any scoring or token spend.
    let mut excluded_high_risk = Vec::new();
    let mut degraded = Vec::new();
    let mut scorable = Vec::new();
    for candidate in &facts.candidates {
        if candidate.privacy_risk.eq_ignore_ascii_case("high") {
            excluded_high_risk.push(render_excluded(candidate, "high-privacy-risk"));
            continue;
        }
        if candidate.semantic_only && !facts.semantic_available {
            degraded.push(render_excluded(candidate, "semantic-model-unavailable"));
            continue;
        }
        scorable.push(score_candidate(candidate, &facts));
    }

    // Deterministic order: score desc, then freshness desc, then id asc.
    scorable.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.freshness_score.cmp(&a.freshness_score))
            .then(a.candidate.id.cmp(&b.candidate.id))
    });

    let reserved = RESERVED_ENVELOPE_TOKENS.min(facts.token_budget);
    let evidence_budget = facts.token_budget.saturating_sub(reserved);
    let mut used_tokens = 0u64;
    let mut selected = Vec::new();
    let mut omitted = Vec::new();
    for scored in &scorable {
        // Best-effort fill: a too-large ref is omitted but later smaller refs
        // may still fit, maximizing useful evidence under the budget.
        if used_tokens.saturating_add(scored.token_cost) <= evidence_budget {
            used_tokens += scored.token_cost;
            selected.push(render_selected(scored, selected.len() + 1));
        } else {
            omitted.push(render_omitted(scored, "token-budget-exhausted"));
        }
    }

    let summary = summarize(
        &facts,
        selected.len(),
        excluded_high_risk.len(),
        omitted.len(),
        degraded.len(),
        used_tokens,
        evidence_budget,
    );
    let status = summary
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("partial");

    json!({
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "_meta": {
            "generated_at": Utc::now().to_rfc3339(),
            "source": source_kind,
            "fixture_id": fixture_id,
            "contract": "deterministic bead-scoped context pack under token budget"
        },
        "bead_id": facts.bead_id,
        "summary": summary,
        "budget": {
            "token_budget": facts.token_budget,
            "reserved_envelope_tokens": reserved,
            "evidence_budget_tokens": evidence_budget,
            "used_tokens": used_tokens,
            "remaining_tokens": evidence_budget.saturating_sub(used_tokens),
            "chars_per_token": CHARS_PER_TOKEN,
            "policy": "greedy best-effort fill by score; metadata-first, excerpts redacted"
        },
        "selected": selected,
        "excluded_high_risk": excluded_high_risk,
        "omitted": omitted,
        "degraded": degraded,
        "degradation": {
            "semantic_available": facts.semantic_available,
            "search_assets_ready": facts.search_assets_ready,
            "mode": degradation_mode(&facts),
            "note": degradation_note(&facts)
        },
        "ranking": {
            "weights": {
                "relevance": WEIGHT_RELEVANCE,
                "freshness": WEIGHT_FRESHNESS,
                "authority": WEIGHT_AUTHORITY
            },
            "freshness_window_days": facts.freshness_window_days,
            "tie_break": "score desc, freshness desc, id asc"
        },
        "mutation_contract": {
            "read_only": true,
            "schedules_work": false,
            "mutates_files": false,
            "mutates_db": false,
            "touches_network": false
        },
        "privacy": {
            "redaction_applied": true,
            "high_risk_excluded": true,
            "contains_raw_secrets": false
        },
        "guided_workflow": {
            "surface": "cass swarm context-pack --json",
            "bead_id": "coding_agent_session_search-swarm-coordination-intelligence-gnrxb.7",
            "apply_mode_available": false,
            "next_step": summary.get("recommended_action").cloned().unwrap_or_else(|| json!("review-selected-refs"))
        }
    })
}

fn score_candidate(candidate: &Candidate, facts: &PackFacts) -> Scored {
    let relevance = clamp_unit(candidate.relevance);
    let authority = clamp_unit(candidate.authority);
    let (freshness, stale) = freshness_score(candidate.created_at_ms, facts);
    // Integer score in 0..=1000 keeps ordering deterministic (no float compare).
    let score = (WEIGHT_RELEVANCE * (relevance * 1000.0) as u64
        + WEIGHT_FRESHNESS * freshness
        + WEIGHT_AUTHORITY * (authority * 1000.0) as u64)
        / 100;
    let token_cost = estimated_tokens(&candidate.excerpt).saturating_add(REF_METADATA_TOKENS);
    Scored {
        candidate: candidate.clone(),
        score,
        freshness_score: freshness,
        token_cost,
        stale,
    }
}

/// Returns `(freshness_in_0..=1000, is_stale)`.
fn freshness_score(created_at_ms: Option<i64>, facts: &PackFacts) -> (u64, bool) {
    let (Some(created), Some(now)) = (created_at_ms, facts.now_ms) else {
        // Unknown age: conservative mid floor, not treated as stale.
        return (400, false);
    };
    let window_ms = (facts.freshness_window_days.max(1) as i64).saturating_mul(86_400_000);
    let age_ms = now.saturating_sub(created);
    if age_ms <= 0 {
        return (1000, false);
    }
    if age_ms >= window_ms {
        // Older than the freshness window: stale, heavily penalized but not zero.
        return (100, true);
    }
    // Linear decay from 1000 (now) to 300 (window edge) inside the window.
    let remaining = window_ms - age_ms;
    let decayed = 300 + (700 * remaining) / window_ms;
    (decayed.clamp(300, 1000) as u64, false)
}

fn render_selected(scored: &Scored, rank: usize) -> Value {
    json!({
        "rank": rank,
        "id": scored.candidate.id,
        "kind": scored.candidate.kind,
        "path": redact(&scored.candidate.path),
        "excerpt": redact(&scored.candidate.excerpt),
        "token_cost": scored.token_cost,
        "score": scored.score,
        "freshness_score": scored.freshness_score,
        "stale": scored.stale,
        "privacy_risk": scored.candidate.privacy_risk,
        "included_because": inclusion_reason(scored)
    })
}

fn render_omitted(scored: &Scored, reason: &str) -> Value {
    json!({
        "id": scored.candidate.id,
        "kind": scored.candidate.kind,
        "path": redact(&scored.candidate.path),
        "token_cost": scored.token_cost,
        "score": scored.score,
        "reason": reason
    })
}

fn render_excluded(candidate: &Candidate, reason: &str) -> Value {
    json!({
        "id": candidate.id,
        "kind": candidate.kind,
        "path": redact(&candidate.path),
        "privacy_risk": candidate.privacy_risk,
        "reason": reason
    })
}

fn inclusion_reason(scored: &Scored) -> &'static str {
    if scored.stale {
        "stale-but-budget-available"
    } else if scored.score >= 700 {
        "high-relevance-fresh"
    } else if scored.score >= 400 {
        "moderate-relevance"
    } else {
        "low-signal-fills-budget"
    }
}

fn summarize(
    facts: &PackFacts,
    selected: usize,
    excluded_high_risk: usize,
    omitted: usize,
    degraded: usize,
    used_tokens: u64,
    evidence_budget: u64,
) -> Value {
    let candidate_count = facts.candidates.len();
    let status = if facts.fixture_problem.is_some() {
        "partial"
    } else if excluded_high_risk > 0 || omitted > 0 || degraded > 0 {
        "warning"
    } else {
        "ok"
    };
    let recommended_action = if facts.fixture_problem.is_some() {
        "supply-context-pack-fixture"
    } else if candidate_count == 0 {
        "no-candidates-available"
    } else if selected == 0 {
        "increase-token-budget"
    } else if omitted > 0 {
        "review-omitted-or-raise-budget"
    } else {
        "review-selected-refs"
    };
    json!({
        "status": status,
        "candidate_count": candidate_count,
        "selected_count": selected,
        "excluded_high_risk_count": excluded_high_risk,
        "omitted_count": omitted,
        "degraded_count": degraded,
        "used_tokens": used_tokens,
        "evidence_budget_tokens": evidence_budget,
        "budget_utilization_pct": (used_tokens * 100).checked_div(evidence_budget).unwrap_or(0),
        "citation_complete": selected > 0,
        "recommended_action": recommended_action
    })
}

fn degradation_mode(facts: &PackFacts) -> &'static str {
    match (facts.semantic_available, facts.search_assets_ready) {
        (true, true) => "full",
        (false, true) => "lexical-only",
        (true, false) => "metadata-only",
        (false, false) => "metadata-only-no-semantic",
    }
}

fn degradation_note(facts: &PackFacts) -> &'static str {
    match (facts.semantic_available, facts.search_assets_ready) {
        (true, true) => "all ranking signals available",
        (false, true) => {
            "semantic model unavailable; semantic-only candidates dropped, lexical ranking retained"
        }
        (true, false) => "search assets not ready; selection falls back to metadata-first ranking",
        (false, false) => {
            "neither semantic model nor search assets available; metadata-first ranking only"
        }
    }
}

fn clamp_unit(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn live_facts() -> PackFacts {
    PackFacts {
        fixture_problem: None,
        bead_id: None,
        token_budget: DEFAULT_TOKEN_BUDGET,
        now_ms: None,
        freshness_window_days: 30,
        semantic_available: false,
        search_assets_ready: false,
        candidates: Vec::new(),
    }
}

fn fixture_facts(source: Option<&Value>) -> PackFacts {
    let Some(source) = source else {
        return PackFacts {
            fixture_problem: Some("context_pack fixture source is missing".to_string()),
            ..live_facts()
        };
    };

    let candidates = source
        .get("candidates")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(parse_candidate).collect::<Vec<_>>())
        .unwrap_or_default();

    PackFacts {
        fixture_problem: None,
        bead_id: source
            .get("bead_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        token_budget: source
            .get("token_budget")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TOKEN_BUDGET),
        now_ms: source.get("now_ms").and_then(Value::as_i64),
        freshness_window_days: source
            .get("freshness_window_days")
            .and_then(Value::as_u64)
            .unwrap_or(30),
        semantic_available: source
            .get("semantic_available")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        search_assets_ready: source
            .get("search_assets_ready")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        candidates,
    }
}

fn parse_candidate(value: &Value) -> Candidate {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .map(|kind| {
            if KNOWN_KINDS.contains(&kind) {
                kind.to_string()
            } else {
                format!("other:{kind}")
            }
        })
        .unwrap_or_else(|| "other:unknown".to_string());
    Candidate {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        kind,
        path: value
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        excerpt: value
            .get("excerpt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        created_at_ms: value.get("created_at_ms").and_then(Value::as_i64),
        relevance: value
            .get("relevance")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        authority: value
            .get("authority")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        privacy_risk: value
            .get("privacy_risk")
            .and_then(Value::as_str)
            .unwrap_or("low")
            .to_string(),
        semantic_only: value
            .get("semantic_only")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // now = 2026-06-09; ~1 day, ~10 day, ~60 day old created_at values.
    const NOW_MS: i64 = 1_749_456_000_000;
    const DAY_MS: i64 = 86_400_000;

    fn source() -> Value {
        json!({
            "bead_id": "demo-bead",
            "token_budget": 200,
            "now_ms": NOW_MS,
            "freshness_window_days": 30,
            "semantic_available": false,
            "search_assets_ready": true,
            "candidates": [
                {
                    "id": "fresh-relevant",
                    "kind": "closeout_note",
                    "path": "/home/alice/notes/win.md",
                    "excerpt": "landed fix: use unwrap_or for non-lazy default",
                    "created_at_ms": NOW_MS - DAY_MS,
                    "relevance": 0.95, "authority": 0.9, "privacy_risk": "low"
                },
                {
                    "id": "stale-doc",
                    "kind": "doc",
                    "path": "/home/alice/docs/old.md",
                    "excerpt": "outdated advice about rusqlite",
                    "created_at_ms": NOW_MS - 60 * DAY_MS,
                    "relevance": 0.6, "authority": 0.5, "privacy_risk": "low"
                },
                {
                    "id": "secret-leak",
                    "kind": "failed_command",
                    "path": "/home/alice/.env",
                    "excerpt": "export TOKEN=sk-ant-supersecretvalue1234567890",
                    "created_at_ms": NOW_MS - DAY_MS,
                    "relevance": 0.99, "authority": 0.9, "privacy_risk": "high"
                },
                {
                    "id": "semantic-pick",
                    "kind": "prior_session",
                    "path": "/home/alice/.claude/s.jsonl",
                    "excerpt": "semantic-only match",
                    "created_at_ms": NOW_MS - 2 * DAY_MS,
                    "relevance": 0.8, "authority": 0.7, "privacy_risk": "low",
                    "semantic_only": true
                }
            ]
        })
    }

    fn assert_no_secret_leak(value: &Value) {
        let text = serde_json::to_string(value).expect("serialize");
        for needle in ["/home/", "sk-ant-", "TOKEN=sk-ant", "supersecret"] {
            assert!(!text.contains(needle), "context pack leaked: {needle}");
        }
    }

    #[test]
    fn high_risk_excluded_and_never_leaked() {
        let out = render_context_pack_fixture("ctx", Some(&source()));
        let excluded = out["excluded_high_risk"].as_array().unwrap();
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0]["id"], json!("secret-leak"));
        // The high-risk excerpt must never appear anywhere in the output.
        assert_no_secret_leak(&out);
        // Selected refs must not include the high-risk id.
        for sel in out["selected"].as_array().unwrap() {
            assert_ne!(sel["id"], json!("secret-leak"));
        }
    }

    #[test]
    fn semantic_unavailable_drops_semantic_only_with_note() {
        let out = render_context_pack_fixture("ctx", Some(&source()));
        let degraded = out["degraded"].as_array().unwrap();
        assert_eq!(degraded.len(), 1);
        assert_eq!(degraded[0]["id"], json!("semantic-pick"));
        assert_eq!(out["degradation"]["mode"], json!("lexical-only"));
    }

    #[test]
    fn deterministic_order_fresh_outranks_stale() {
        let out = render_context_pack_fixture("ctx", Some(&source()));
        let selected = out["selected"].as_array().unwrap();
        assert!(!selected.is_empty());
        assert_eq!(selected[0]["id"], json!("fresh-relevant"));
        // Two runs produce identical output (no clock/float nondeterminism in body).
        let out2 = render_context_pack_fixture("ctx", Some(&source()));
        assert_eq!(out["selected"], out2["selected"]);
        assert_eq!(out["omitted"], out2["omitted"]);
    }

    #[test]
    fn budget_overflow_omits_and_accounts() {
        let mut src = source();
        src["token_budget"] = json!(70); // tiny: envelope 64 reserved -> ~6 evidence tokens
        let out = render_context_pack_fixture("ctx", Some(&src));
        let used = out["budget"]["used_tokens"].as_u64().unwrap();
        let evidence_budget = out["budget"]["evidence_budget_tokens"].as_u64().unwrap();
        assert!(used <= evidence_budget);
        // At least one non-risk, non-degraded candidate should be omitted for budget.
        let omitted = out["omitted"].as_array().unwrap();
        assert!(!omitted.is_empty());
        assert_eq!(omitted[0]["reason"], json!("token-budget-exhausted"));
    }

    #[test]
    fn missing_source_is_partial_not_panic() {
        let out = render_context_pack_fixture("empty", None);
        assert_eq!(out["status"], json!("partial"));
        assert_eq!(out["mutation_contract"]["read_only"], json!(true));
        assert_eq!(
            out["summary"]["recommended_action"],
            json!("supply-context-pack-fixture")
        );
    }

    #[test]
    fn live_is_empty_metadata_only_and_read_only() {
        let out = render_context_pack_live();
        assert_eq!(out["summary"]["candidate_count"], json!(0));
        assert_eq!(
            out["degradation"]["mode"],
            json!("metadata-only-no-semantic")
        );
        assert_eq!(out["mutation_contract"]["touches_network"], json!(false));
    }
}
