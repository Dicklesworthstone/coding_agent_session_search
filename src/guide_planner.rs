//! Intent-to-command planner for guided safe workflows (bead
//! `coding_agent_session_search-guided-ops-repro-trust-5u82n.1`).
//!
//! `cass guide [INTENT] --json` is a robot-first surface that turns an operator
//! intent (`fix-ci`, `investigate-search-miss`, `prepare-release`,
//! `repair-assets`, `export-session`, `onboard-source`, …) into an exact, safe,
//! advisory command plan. It builds on the existing triage/health/status/doctor
//! readiness surfaces and the workflow macro registry (bead `…5u82n.10`) — the
//! macros are the source of truth for each journey's ordered steps, proof gates,
//! privacy tier, rch rules, rollback notes, and stop conditions.
//!
//! Contract: **read-only / no-mutation by default**. The planner renders what a
//! journey *would* do, classifies live preflight readiness honestly (satisfied
//! vs unmet vs needs-confirmation), names the forbidden shortcuts an agent must
//! never take in this repo, and points at deeper robot-safe surfaces for cost
//! and privacy detail. Nothing here executes a step. When an intent cannot be
//! mapped, it falls back to the known-intent catalog plus docs/discovery
//! pointers instead of guessing.

use chrono::Utc;
use serde_json::{Value, json};

/// Schema identifier for the guide plan payload.
pub const SCHEMA_VERSION: &str = "cass.guide.plan.v1";

/// Bead that owns this surface.
pub const BEAD_ID: &str = "coding_agent_session_search-guided-ops-repro-trust-5u82n.1";

/// Canonical operator intents mapped to a workflow-macro id. The friendly key
/// is what an operator most naturally types; the macro registry's own
/// `intent_aliases` are resolved in addition to these (see [`resolve_intent`]).
struct IntentMapping {
    /// Friendly canonical intent keyword (kebab-case).
    intent: &'static str,
    /// The workflow-macro id this intent drives.
    macro_id: &'static str,
    /// One-line summary for the catalog / unknown-intent fallback.
    summary: &'static str,
}

const INTENT_CATALOG: &[IntentMapping] = &[
    IntentMapping {
        intent: "investigate-search-miss",
        macro_id: "investigate-no-hit-search",
        summary: "Diagnose a search that returns no hits (readiness, two-tier explain, coverage).",
    },
    IntentMapping {
        intent: "fix-ci",
        macro_id: "fix-ci-regression",
        summary: "Find the first failing gate, reproduce it locally, apply the minimal fix.",
    },
    IntentMapping {
        intent: "prepare-release",
        macro_id: "prepare-release",
        summary: "Run the verification gauntlet, verify channels, review the changelog/tag plan.",
    },
    IntentMapping {
        intent: "repair-assets",
        macro_id: "repair-derived-assets",
        summary: "Classify stale/corrupt derived assets, plan the rebuild, rebuild only the stale ones.",
    },
    IntentMapping {
        intent: "onboard-source",
        macro_id: "onboard-source",
        summary: "Preview privacy exposure, dry-sync a new source, then index it.",
    },
    IntentMapping {
        intent: "export-session",
        macro_id: "export-encrypted-session",
        summary: "Preview exposure, confirm the key/recipient policy, produce a redacted encrypted export.",
    },
    IntentMapping {
        intent: "support-capsule",
        macro_id: "create-support-capsule",
        summary: "Preview exposure, gather redacted evidence, produce a share-safe support capsule.",
    },
];

/// Forbidden shortcuts every guided journey must reflect for THIS repo. These
/// are described, not emitted as runnable destructive strings, so the plan can
/// never be copy-pasted into harm. Mirrors AGENTS.md and the no-worktrees /
/// shared-checkout swarm reality.
struct ForbiddenShortcut {
    shortcut: &'static str,
    why: &'static str,
}

const CORE_FORBIDDEN_SHORTCUTS: &[ForbiddenShortcut] = &[
    ForbiddenShortcut {
        shortcut: "hard-reset-working-tree",
        why: "Never discard changes with a hard git reset — revert with a follow-up commit so concurrent agents' work is preserved.",
    },
    ForbiddenShortcut {
        shortcut: "force-clean-untracked",
        why: "Never force-clean untracked files in this shared checkout; you may delete another agent's in-flight work.",
    },
    ForbiddenShortcut {
        shortcut: "hand-delete-derived-index",
        why: "The lexical/semantic index is a derived asset owned by the atomic publish pipeline; rebuild via the recommended command, never remove the index directory by hand.",
    },
    ForbiddenShortcut {
        shortcut: "bare-interactive-cli",
        why: "Bare `cass` or `bv` launches a TUI that blocks automation; always pass --json/--robot.",
    },
];

const BUILD_FORBIDDEN_SHORTCUT: ForbiddenShortcut = ForbiddenShortcut {
    shortcut: "build-on-interactive-pane",
    why: "Offload cargo build/test/clippy through rch with a per-agent CARGO_TARGET_DIR to avoid compilation storms.",
};

/// Map a workflow-macro id to a `cass swarm resource-plan --action` value for a
/// deeper, host-aware cost/risk estimate. `None` means the journey has no
/// heavy resource action to model (e.g. a pure build/test loop).
fn resource_action_for_macro(macro_id: &str) -> Option<&'static str> {
    match macro_id {
        "repair-derived-assets" | "onboard-source" => Some("full-index"),
        "investigate-no-hit-search" => Some("semantic-backfill"),
        "prepare-release" => Some("release-verification"),
        "export-encrypted-session" => Some("html-export"),
        "create-support-capsule" => Some("support-capsule"),
        _ => None,
    }
}

/// The outcome of mapping a raw operator intent to a macro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentResolution {
    /// The raw string the operator supplied.
    pub raw: String,
    /// Normalised form (trimmed, lowercased, separators collapsed to `-`).
    pub normalized: String,
    /// The resolved macro id, if any.
    pub macro_id: Option<String>,
    /// How the resolution matched: `intent-keyword` | `macro-id` | `macro-alias`.
    pub matched_via: Option<&'static str>,
}

impl IntentResolution {
    #[must_use]
    pub fn recognized(&self) -> bool {
        self.macro_id.is_some()
    }
}

/// Normalise an intent/alias string for matching: trim, lowercase, and collapse
/// whitespace/underscores to single hyphens.
fn normalize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_dash = false;
    for ch in raw.trim().chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            last_dash = false;
            ch.to_ascii_lowercase()
        } else {
            // Any separator (space, underscore, hyphen, slash, dot) collapses.
            if last_dash {
                continue;
            }
            last_dash = true;
            '-'
        };
        out.push(mapped);
    }
    // Trim a trailing collapse char.
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Resolve a raw operator intent to a macro id. Resolution order is
/// deterministic: friendly intent keyword first, then exact macro id, then the
/// macro registry's own normalised `intent_aliases`.
#[must_use]
pub fn resolve_intent(raw: &str) -> IntentResolution {
    let normalized = normalize(raw);

    // 1. Friendly canonical intent keyword.
    if let Some(entry) = INTENT_CATALOG.iter().find(|e| e.intent == normalized) {
        return IntentResolution {
            raw: raw.to_string(),
            normalized,
            macro_id: Some(entry.macro_id.to_string()),
            matched_via: Some("intent-keyword"),
        };
    }

    // Pull the live registry once to learn macro ids + their aliases.
    let registry = crate::workflow_macros::render_workflow_macros_live();
    let macros = registry.get("macros").and_then(Value::as_array);

    // 2. Exact macro id.
    if let Some(macros) = macros
        && macros
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .any(|id| id == normalized)
    {
        return IntentResolution {
            raw: raw.to_string(),
            normalized: normalized.clone(),
            macro_id: Some(normalized),
            matched_via: Some("macro-id"),
        };
    }

    // 3. Registry intent_aliases (normalised on both sides).
    if let Some(macros) = macros {
        for m in macros {
            let Some(id) = m.get("id").and_then(Value::as_str) else {
                continue;
            };
            let aliases = m.get("intent_aliases").and_then(Value::as_array);
            if let Some(aliases) = aliases
                && aliases
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|alias| normalize(alias) == normalized)
            {
                return IntentResolution {
                    raw: raw.to_string(),
                    normalized,
                    macro_id: Some(id.to_string()),
                    matched_via: Some("macro-alias"),
                };
            }
        }
    }

    IntentResolution {
        raw: raw.to_string(),
        normalized,
        macro_id: None,
        matched_via: None,
    }
}

/// The known-intent catalog as JSON (for catalog mode and unknown-intent help).
#[must_use]
pub fn known_intents() -> Vec<Value> {
    INTENT_CATALOG
        .iter()
        .map(|e| {
            json!({
                "intent": e.intent,
                "macro_id": e.macro_id,
                "summary": e.summary
            })
        })
        .collect()
}

/// Per-fact preflight status. A fact the planner cannot determine from live
/// signals is `needs-confirmation`, NOT `unmet`, so an operator-context fact
/// (e.g. `version_bumped`) is never falsely reported as blocking.
fn classify_prerequisite(fact: &str, facts: Option<&Value>) -> &'static str {
    let Some(obj) = facts.and_then(Value::as_object) else {
        return "needs-confirmation";
    };
    match obj.get(fact).and_then(Value::as_bool) {
        Some(true) => "satisfied",
        Some(false) => "unmet",
        None => "needs-confirmation",
    }
}

/// Build a typed preflight-facts object from the handful of signals the live CLI
/// can actually determine. Facts left as `None` are intentionally omitted so the
/// planner reports them `needs-confirmation` rather than guessing.
#[must_use]
pub fn preflight_facts(
    index_present: Option<bool>,
    db_present: Option<bool>,
    search_assets_ready: Option<bool>,
    disk_headroom_ok: Option<bool>,
    sources_config_writable: Option<bool>,
) -> Value {
    let mut map = serde_json::Map::new();
    let mut insert = |key: &str, val: Option<bool>| {
        if let Some(v) = val {
            map.insert(key.to_string(), Value::Bool(v));
        }
    };
    insert("index_present", index_present);
    insert("db_present", db_present);
    insert("search_assets_ready", search_assets_ready);
    insert("disk_headroom_ok", disk_headroom_ok);
    insert("sources_config_writable", sources_config_writable);
    Value::Object(map)
}

/// Render the single rendered macro object for a resolved id (no facts: we
/// overlay our own honest preflight classification instead of the registry's
/// binary readiness). Returns `None` if the id is somehow unknown.
fn render_macro(macro_id: &str) -> Option<Value> {
    let source = json!({ "macro": macro_id });
    let out = crate::workflow_macros::render_workflow_macros_fixture("guide", Some(&source));
    out.get("macros")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .cloned()
}

fn meta(source_kind: &str, fixture_id: &str) -> Value {
    json!({
        "generated_at": Utc::now().to_rfc3339(),
        "source": source_kind,
        "fixture_id": fixture_id,
        "bead_id": BEAD_ID,
        "contract": "advisory intent-to-command planner"
    })
}

/// The read-only/no-mutation contract every guide payload carries.
fn mutation_contract() -> Value {
    json!({
        "read_only": true,
        "apply_mode": false,
        "schedules_work": false,
        "mutates_files": false,
        "mutates_db": false,
        "touches_network": false
    })
}

/// Catalog mode: no intent supplied. Lists the known intents and how to drive
/// the planner. Read-only and always `ok`.
#[must_use]
pub fn render_guide_catalog(source_kind: &str, fixture_id: &str) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "status": "ok",
        "_meta": meta(source_kind, fixture_id),
        "intent": Value::Null,
        "recommended_action": "select-an-intent",
        "known_intents": known_intents(),
        "usage": {
            "surface": "cass guide <intent> --json",
            "example_intents": INTENT_CATALOG.iter().map(|e| e.intent).collect::<Vec<_>>(),
            "docs": "cass robot-docs guide",
            "discovery": "cass capabilities --json"
        },
        "mutation_contract": mutation_contract()
    })
}

/// Unknown-intent fallback: the raw intent could not be mapped. Points at the
/// known-intent catalog and robot-safe docs/discovery surfaces — never guesses.
fn render_unknown_intent(res: &IntentResolution, source_kind: &str, fixture_id: &str) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "status": "warning",
        "_meta": meta(source_kind, fixture_id),
        "intent": {
            "raw": res.raw,
            "normalized": res.normalized,
            "recognized": false,
            "resolved_macro": Value::Null,
            "matched_via": Value::Null
        },
        "recommended_action": "select-a-known-intent-or-see-docs",
        "known_intents": known_intents(),
        "fallback": {
            "docs": "cass robot-docs guide",
            "discovery": "cass capabilities --json",
            "note": "No workflow macro matched this intent. Pick a known intent above, or use the docs/discovery surfaces to find the right command."
        },
        "mutation_contract": mutation_contract()
    })
}

/// Render the full guide plan for a raw operator intent.
///
/// * `raw_intent` — the operator's intent string. Empty/whitespace yields the
///   catalog.
/// * `facts` — optional preflight-facts object (see [`preflight_facts`]); `None`
///   means readiness is `unknown` and every prerequisite is `needs-confirmation`.
/// * `source_kind` — `"live"` or `"fixture"` (recorded in `_meta`).
/// * `fixture_id` — identifier recorded in `_meta` (use `"live"` for live runs).
#[must_use]
pub fn render_guide_plan(
    raw_intent: &str,
    facts: Option<&Value>,
    source_kind: &str,
    fixture_id: &str,
) -> Value {
    if raw_intent.trim().is_empty() {
        return render_guide_catalog(source_kind, fixture_id);
    }

    let res = resolve_intent(raw_intent);
    let Some(macro_id) = res.macro_id.clone() else {
        return render_unknown_intent(&res, source_kind, fixture_id);
    };
    let Some(m) = render_macro(&macro_id) else {
        // Registry could not render a resolved id — treat as unknown rather than
        // emit a half-formed plan.
        return render_unknown_intent(&res, source_kind, fixture_id);
    };

    let title = m
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let privacy_tier = m
        .get("privacy_tier")
        .and_then(Value::as_str)
        .unwrap_or("low")
        .to_string();

    // --- Prerequisites: honest 3-state classification against live facts. ---
    let required_facts: Vec<&str> = m
        .get("required_preflight_facts")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let prerequisites: Vec<Value> = required_facts
        .iter()
        .map(|&fact| json!({ "fact": fact, "status": classify_prerequisite(fact, facts) }))
        .collect();
    let any_unmet = prerequisites
        .iter()
        .any(|p| p.get("status").and_then(Value::as_str) == Some("unmet"));
    let any_pending = prerequisites
        .iter()
        .any(|p| p.get("status").and_then(Value::as_str) == Some("needs-confirmation"));
    let readiness = if facts.is_none() {
        "unknown"
    } else if any_unmet {
        "blocked"
    } else if any_pending {
        "needs-confirmation"
    } else {
        "ready"
    };

    // --- Steps: carry the macro recipe verbatim, add order + rch target dir. ---
    let suggested_target_dir = format!("/data/tmp/cass-{macro_id}-target");
    let mut any_offload = false;
    let steps: Vec<Value> = m
        .get("steps")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .enumerate()
                .map(|(idx, step)| {
                    let rch_rule = step.get("rch_rule").and_then(Value::as_str).unwrap_or("none");
                    let offloads = rch_rule == "offload-build";
                    any_offload = any_offload || offloads;
                    json!({
                        "order": idx + 1,
                        "intent": step.get("intent").cloned().unwrap_or(Value::Null),
                        "command": step.get("command").cloned().unwrap_or(Value::Null),
                        "proof_gate": step.get("proof_gate").cloned().unwrap_or(Value::Null),
                        "mutates": step.get("mutates").and_then(Value::as_bool).unwrap_or(false),
                        "rch_rule": rch_rule,
                        "rch_target_dir": if offloads { json!(suggested_target_dir) } else { Value::Null }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mutating_step_count = steps
        .iter()
        .filter(|s| s.get("mutates").and_then(Value::as_bool) == Some(true))
        .count();

    // --- Forbidden shortcuts: core set + a build-offload reminder if relevant. ---
    let mut forbidden: Vec<Value> = CORE_FORBIDDEN_SHORTCUTS
        .iter()
        .map(|f| json!({ "shortcut": f.shortcut, "why": f.why }))
        .collect();
    if any_offload {
        forbidden.push(json!({
            "shortcut": BUILD_FORBIDDEN_SHORTCUT.shortcut,
            "why": BUILD_FORBIDDEN_SHORTCUT.why
        }));
    }

    // --- Cost / risk: deterministic coarse band + pointer to the what-if surface. ---
    let risk_level = if privacy_tier == "sensitive" {
        "high"
    } else if mutating_step_count > 0 {
        "medium"
    } else {
        "low"
    };
    let resource_action = resource_action_for_macro(&macro_id);
    let cost_risk = json!({
        "risk_level": risk_level,
        "mutating_step_count": mutating_step_count,
        "resource_action": resource_action,
        "detailed_estimate_via": resource_action
            .map(|a| format!("cass swarm resource-plan --action {a} --json")),
        "note": "Coarse advisory band; consult the resource-plan surface for a host-aware CPU/memory/disk/time estimate before any mutating step."
    });

    // --- Privacy: tier + a preview pointer when a journey reads/exports data. ---
    let has_privacy_preview_step = m.get("steps").and_then(Value::as_array).is_some_and(|arr| {
        arr.iter()
            .any(|s| s.get("command").and_then(Value::as_str) == Some("privacy.preview-exposure"))
    });
    let privacy = json!({
        "tier": privacy_tier,
        "preview_via": if has_privacy_preview_step {
            json!("cass swarm privacy-preview --json")
        } else {
            Value::Null
        },
        "note": match privacy_tier.as_str() {
            "sensitive" => "Sensitive journey: confirm key/recipient policy and review the exposure preview before producing any artifact.",
            "redacted" => "Redacted journey: output is scrubbed, but preview the exposure before proceeding to confirm no sensitive paths or secrets leak.",
            _ => "Low-sensitivity journey: read-only diagnostics with no private session text in the plan.",
        }
    });

    let recommended_action = match readiness {
        "blocked" => "satisfy-prerequisites-then-follow-plan",
        "needs-confirmation" => "confirm-prerequisites-then-follow-plan",
        "ready" => "follow-plan-steps",
        _ => "gather-preflight-facts-or-follow-plan",
    };
    let status = if readiness == "blocked" {
        "warning"
    } else {
        "ok"
    };

    // Robot-safe next surfaces for deeper detail (all --json, never bare TUI).
    let mut next_surfaces = vec![
        json!({ "purpose": "docs", "command": "cass robot-docs guide" }),
        json!({ "purpose": "macro-detail", "command": format!("cass swarm macros --fixture-id {macro_id} --json") }),
    ];
    if has_privacy_preview_step {
        next_surfaces.push(
            json!({ "purpose": "privacy-preview", "command": "cass swarm privacy-preview --json" }),
        );
    }
    if let Some(a) = resource_action {
        next_surfaces.push(json!({ "purpose": "resource-estimate", "command": format!("cass swarm resource-plan --action {a} --json") }));
    }

    json!({
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "_meta": meta(source_kind, fixture_id),
        "intent": {
            "raw": res.raw,
            "normalized": res.normalized,
            "recognized": true,
            "resolved_macro": macro_id,
            "matched_via": res.matched_via
        },
        "readiness": readiness,
        "recommended_action": recommended_action,
        "plan": {
            "macro_id": macro_id,
            "title": title,
            "privacy_tier": privacy_tier,
            "prerequisites": prerequisites,
            "steps": steps,
            "required_proof_gates": m.get("proof_gates").cloned().unwrap_or_else(|| json!([])),
            "forbidden_shortcuts": forbidden,
            "rch": {
                "rules": m.get("rch_rules").cloned().unwrap_or(Value::Null),
                "offload_required": any_offload,
                "suggested_target_dir": if any_offload { json!(suggested_target_dir) } else { Value::Null }
            },
            "cost_risk": cost_risk,
            "privacy": privacy,
            "stop_conditions": m.get("stop_conditions").cloned().unwrap_or_else(|| json!([])),
            "rollback_notes": m.get("rollback_notes").cloned().unwrap_or(Value::Null)
        },
        "next_surfaces": next_surfaces,
        "mutation_contract": mutation_contract()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(pairs: &[(&str, bool)]) -> Value {
        let mut m = serde_json::Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), Value::Bool(*v));
        }
        Value::Object(m)
    }

    #[test]
    fn normalize_collapses_separators() {
        assert_eq!(normalize("  Fix CI  "), "fix-ci");
        assert_eq!(
            normalize("investigate_search_miss"),
            "investigate-search-miss"
        );
        assert_eq!(normalize("repair--assets"), "repair-assets");
        assert_eq!(normalize("Export/Session"), "export-session");
    }

    #[test]
    fn resolves_friendly_keyword() {
        let r = resolve_intent("fix-ci");
        assert!(r.recognized());
        assert_eq!(r.macro_id.as_deref(), Some("fix-ci-regression"));
        assert_eq!(r.matched_via, Some("intent-keyword"));
    }

    #[test]
    fn resolves_macro_id_directly() {
        let r = resolve_intent("repair-derived-assets");
        assert_eq!(r.macro_id.as_deref(), Some("repair-derived-assets"));
        assert_eq!(r.matched_via, Some("macro-id"));
    }

    #[test]
    fn resolves_registry_alias() {
        // "rebuild index" is an alias of repair-derived-assets in the registry.
        let r = resolve_intent("rebuild index");
        assert_eq!(r.macro_id.as_deref(), Some("repair-derived-assets"));
        assert_eq!(r.matched_via, Some("macro-alias"));
    }

    #[test]
    fn unknown_intent_is_not_recognized() {
        let r = resolve_intent("teleport-the-database");
        assert!(!r.recognized());
        assert!(r.matched_via.is_none());
    }

    #[test]
    fn every_catalog_intent_resolves_to_a_real_macro() {
        let registry = crate::workflow_macros::render_workflow_macros_live();
        let ids: Vec<&str> = registry["macros"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .collect();
        for entry in INTENT_CATALOG {
            assert!(
                ids.contains(&entry.macro_id),
                "catalog intent `{}` maps to unknown macro `{}`",
                entry.intent,
                entry.macro_id
            );
            let r = resolve_intent(entry.intent);
            assert_eq!(r.macro_id.as_deref(), Some(entry.macro_id));
        }
    }

    #[test]
    fn catalog_mode_lists_intents_and_is_read_only() {
        let out = render_guide_plan("", None, "live", "live");
        assert_eq!(out["status"], json!("ok"));
        assert_eq!(out["recommended_action"], json!("select-an-intent"));
        assert_eq!(out["mutation_contract"]["read_only"], json!(true));
        assert!(out["known_intents"].as_array().unwrap().len() >= 7);
    }

    #[test]
    fn unknown_intent_falls_back_to_docs() {
        let out = render_guide_plan("make-me-a-sandwich", None, "live", "live");
        assert_eq!(out["status"], json!("warning"));
        assert_eq!(out["intent"]["recognized"], json!(false));
        assert_eq!(out["fallback"]["docs"], json!("cass robot-docs guide"));
        assert_eq!(
            out["recommended_action"],
            json!("select-a-known-intent-or-see-docs")
        );
        assert_eq!(out["mutation_contract"]["read_only"], json!(true));
    }

    #[test]
    fn healthy_repair_assets_is_ready() {
        // repair-derived-assets needs db_present + disk_headroom_ok.
        let f = facts(&[("db_present", true), ("disk_headroom_ok", true)]);
        let out = render_guide_plan("repair-assets", Some(&f), "fixture", "guide-healthy");
        assert_eq!(out["readiness"], json!("ready"));
        assert_eq!(out["status"], json!("ok"));
        assert_eq!(out["recommended_action"], json!("follow-plan-steps"));
        assert_eq!(out["plan"]["macro_id"], json!("repair-derived-assets"));
        let prereqs = out["plan"]["prerequisites"].as_array().unwrap();
        assert!(prereqs.iter().all(|p| p["status"] == json!("satisfied")));
    }

    #[test]
    fn missing_index_blocks_search_miss() {
        // investigate-no-hit-search needs index_present + search_assets_ready.
        let f = facts(&[("index_present", false), ("search_assets_ready", false)]);
        let out = render_guide_plan(
            "investigate-search-miss",
            Some(&f),
            "fixture",
            "guide-missing-index",
        );
        assert_eq!(out["readiness"], json!("blocked"));
        assert_eq!(out["status"], json!("warning"));
        assert_eq!(
            out["recommended_action"],
            json!("satisfy-prerequisites-then-follow-plan")
        );
        let prereqs = out["plan"]["prerequisites"].as_array().unwrap();
        assert!(
            prereqs
                .iter()
                .any(|p| p["fact"] == json!("index_present") && p["status"] == json!("unmet"))
        );
    }

    #[test]
    fn undeterminable_fact_is_needs_confirmation_not_unmet() {
        // prepare-release needs git_clean_or_known + version_bumped; supply only
        // git_clean_or_known so version_bumped is omitted (undeterminable).
        let f = facts(&[("git_clean_or_known", true)]);
        let out = render_guide_plan("prepare-release", Some(&f), "fixture", "guide-release");
        assert_eq!(out["readiness"], json!("needs-confirmation"));
        assert_eq!(out["status"], json!("ok"));
        let prereqs = out["plan"]["prerequisites"].as_array().unwrap();
        assert!(
            prereqs.iter().any(|p| p["fact"] == json!("version_bumped")
                && p["status"] == json!("needs-confirmation"))
        );
        assert!(
            prereqs
                .iter()
                .any(|p| p["fact"] == json!("git_clean_or_known")
                    && p["status"] == json!("satisfied"))
        );
    }

    #[test]
    fn no_facts_yields_unknown_readiness() {
        let out = render_guide_plan("fix-ci", None, "live", "live");
        assert_eq!(out["readiness"], json!("unknown"));
        let prereqs = out["plan"]["prerequisites"].as_array().unwrap();
        assert!(
            prereqs
                .iter()
                .all(|p| p["status"] == json!("needs-confirmation"))
        );
    }

    #[test]
    fn sensitive_export_is_high_risk_with_privacy_preview() {
        let out = render_guide_plan("export-session", None, "live", "live");
        assert_eq!(out["plan"]["privacy_tier"], json!("sensitive"));
        assert_eq!(out["plan"]["cost_risk"]["risk_level"], json!("high"));
        assert_eq!(
            out["plan"]["privacy"]["preview_via"],
            json!("cass swarm privacy-preview --json")
        );
    }

    #[test]
    fn plan_is_always_read_only_even_with_mutating_steps() {
        // fix-ci-regression and others have mutating steps; the PLAN never mutates.
        for intent in [
            "fix-ci",
            "repair-assets",
            "onboard-source",
            "export-session",
        ] {
            let out = render_guide_plan(intent, None, "live", "live");
            assert_eq!(
                out["mutation_contract"]["read_only"],
                json!(true),
                "{intent}"
            );
            assert_eq!(
                out["mutation_contract"]["mutates_db"],
                json!(false),
                "{intent}"
            );
            assert_eq!(
                out["mutation_contract"]["mutates_files"],
                json!(false),
                "{intent}"
            );
        }
    }

    #[test]
    fn steps_carry_dotted_intents_not_runnable_cli() {
        // The macro recipe must stay structured — no copy-pasteable cass/bv.
        let out = render_guide_plan("fix-ci", None, "live", "live");
        let steps_text = serde_json::to_string(&out["plan"]["steps"]).unwrap();
        assert!(
            !steps_text.contains("cass "),
            "steps must not embed runnable `cass `"
        );
        assert!(
            !steps_text.contains("bv "),
            "steps must not embed runnable `bv `"
        );
    }

    #[test]
    fn forbidden_shortcuts_are_described_not_runnable() {
        let out = render_guide_plan("repair-assets", None, "live", "live");
        let forbidden = serde_json::to_string(&out["plan"]["forbidden_shortcuts"]).unwrap();
        // Described as kebab labels, never literal destructive command strings.
        assert!(forbidden.contains("hand-delete-derived-index"));
        assert!(!forbidden.contains("rm -rf"));
        assert!(!forbidden.to_ascii_lowercase().contains("git reset --hard"));
    }

    #[test]
    fn offload_step_gets_target_dir_and_build_warning() {
        let out = render_guide_plan("fix-ci", None, "live", "live");
        assert_eq!(out["plan"]["rch"]["offload_required"], json!(true));
        let dir = out["plan"]["rch"]["suggested_target_dir"].as_str().unwrap();
        assert!(dir.starts_with("/data/tmp/cass-"));
        let forbidden = serde_json::to_string(&out["plan"]["forbidden_shortcuts"]).unwrap();
        assert!(forbidden.contains("build-on-interactive-pane"));
    }

    #[test]
    fn deterministic_output_for_same_inputs() {
        let f = facts(&[("db_present", true), ("disk_headroom_ok", true)]);
        let a = render_guide_plan("repair-assets", Some(&f), "fixture", "x");
        let b = render_guide_plan("repair-assets", Some(&f), "fixture", "x");
        // Strip the timestamp before comparing.
        let strip = |mut v: Value| {
            if let Some(meta) = v.get_mut("_meta").and_then(Value::as_object_mut) {
                meta.remove("generated_at");
            }
            v
        };
        assert_eq!(strip(a), strip(b));
    }

    #[test]
    fn preflight_facts_omits_none() {
        let f = preflight_facts(Some(true), Some(false), None, None, Some(true));
        assert_eq!(f["index_present"], json!(true));
        assert_eq!(f["db_present"], json!(false));
        assert_eq!(f["sources_config_writable"], json!(true));
        assert!(f.get("search_assets_ready").is_none());
        assert!(f.get("disk_headroom_ok").is_none());
    }

    #[test]
    fn every_intent_renders_required_planner_fields() {
        // Acceptance-criteria scenarios all expose the contract fields.
        for entry in INTENT_CATALOG {
            let out = render_guide_plan(entry.intent, None, "live", "live");
            let plan = &out["plan"];
            for field in [
                "macro_id",
                "title",
                "privacy_tier",
                "prerequisites",
                "steps",
                "required_proof_gates",
                "forbidden_shortcuts",
                "rch",
                "cost_risk",
                "privacy",
                "stop_conditions",
                "rollback_notes",
            ] {
                assert!(!plan[field].is_null(), "{}: missing {field}", entry.intent);
            }
            assert!(
                !plan["steps"].as_array().unwrap().is_empty(),
                "{}",
                entry.intent
            );
        }
    }
}
