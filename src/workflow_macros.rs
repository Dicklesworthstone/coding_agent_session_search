//! Versioned workflow macro registry for repeatable CASS operator journeys.
//!
//! Macros are data-driven, advisory recipes the guided planner can consume. Each
//! declares intent aliases, required preflight facts, ordered steps (structured
//! intents — never bare `cass`/`bv` command strings), proof gates, optional
//! mutations, rollback notes, a privacy tier, rch rules, and stop conditions.
//!
//! This surface is advisory only: it renders a macro and whether its preflight
//! facts are satisfied. Execution requires an explicit apply/run mode in a later
//! bead — nothing here mutates state.

use chrono::Utc;
use serde_json::{Value, json};

/// Schema identifier for the workflow macros payload.
pub const SCHEMA_VERSION: &str = "cass.swarm.workflow_macros.v1";

/// One ordered step in a macro. `command` is a structured dotted intent (e.g.
/// `search.two-tier`), never a runnable `cass …` string, so rendered docs can't
/// be blindly copy-pasted.
struct MacroStep {
    intent: &'static str,
    command: &'static str,
    proof_gate: &'static str,
    mutates: bool,
    rch_rule: &'static str,
}

/// A versioned operator-journey recipe.
struct WorkflowMacro {
    id: &'static str,
    title: &'static str,
    intent_aliases: &'static [&'static str],
    privacy_tier: &'static str,
    required_preflight_facts: &'static [&'static str],
    steps: &'static [MacroStep],
    proof_gates: &'static [&'static str],
    optional_mutations: &'static [&'static str],
    rollback_notes: &'static str,
    rch_rules: &'static str,
    stop_conditions: &'static [&'static str],
}

const MACROS: &[WorkflowMacro] = &[
    WorkflowMacro {
        id: "investigate-no-hit-search",
        title: "Investigate a search that returns no hits",
        intent_aliases: &[
            "no-hit search",
            "empty search results",
            "search returns nothing",
        ],
        privacy_tier: "low",
        required_preflight_facts: &["index_present", "search_assets_ready"],
        steps: &[
            MacroStep {
                intent: "check readiness of search assets",
                command: "search.readiness",
                proof_gate: "readiness-ok",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "re-run query in two-tier mode with explain",
                command: "search.two-tier-explain",
                proof_gate: "explain-rendered",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "inspect lexical vs semantic coverage",
                command: "diag.search-coverage",
                proof_gate: "coverage-reported",
                mutates: false,
                rch_rule: "none",
            },
        ],
        proof_gates: &["readiness-ok", "explain-rendered"],
        optional_mutations: &["rebuild-semantic-index"],
        rollback_notes: "All steps are read-only; the optional rebuild is resumable and leaves the prior index until it completes.",
        rch_rules: "Offload any index rebuild to a dedicated CARGO_TARGET_DIR; never rebuild on the interactive pane.",
        stop_conditions: &[
            "readiness reports blocked",
            "query returns hits after re-run",
        ],
    },
    WorkflowMacro {
        id: "fix-ci-regression",
        title: "Diagnose and fix a CI regression",
        intent_aliases: &["ci failing", "fix broken build", "regression triage"],
        privacy_tier: "low",
        required_preflight_facts: &["git_clean_or_known", "ci_logs_available"],
        steps: &[
            MacroStep {
                intent: "identify the first failing gate",
                command: "diag.ci-first-failure",
                proof_gate: "first-failure-found",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "reproduce the failing gate locally",
                command: "verify.reproduce-gate",
                proof_gate: "repro-confirmed",
                mutates: false,
                rch_rule: "offload-build",
            },
            MacroStep {
                intent: "apply the minimal fix and re-run the gate",
                command: "verify.rerun-gate",
                proof_gate: "gate-green",
                mutates: true,
                rch_rule: "offload-build",
            },
        ],
        proof_gates: &["first-failure-found", "repro-confirmed", "gate-green"],
        optional_mutations: &["apply-source-fix"],
        rollback_notes: "Source edits are tracked in git; revert with a follow-up commit, never a hard reset.",
        rch_rules: "Run --all-targets gates via rch with a per-agent target dir.",
        stop_conditions: &["gate is green", "regression is upstream/not in this repo"],
    },
    WorkflowMacro {
        id: "prepare-release",
        title: "Prepare and verify a release",
        intent_aliases: &["cut a release", "release readiness", "ship version"],
        privacy_tier: "low",
        required_preflight_facts: &["git_clean_or_known", "version_bumped"],
        steps: &[
            MacroStep {
                intent: "run the full verification gauntlet",
                command: "verify.release-gauntlet",
                proof_gate: "gauntlet-green",
                mutates: false,
                rch_rule: "offload-build",
            },
            MacroStep {
                intent: "verify distribution channels",
                command: "release.verify-channels",
                proof_gate: "channels-ready",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "review the changelog and tag plan",
                command: "release.changelog-review",
                proof_gate: "changelog-approved",
                mutates: false,
                rch_rule: "none",
            },
        ],
        proof_gates: &["gauntlet-green", "channels-ready"],
        optional_mutations: &["create-git-tag"],
        rollback_notes: "Tagging is the only mutation; an unpushed tag is deletable locally before release.",
        rch_rules: "Cross-platform builds go through the release CI/dsr path, not the interactive pane.",
        stop_conditions: &[
            "a channel reports not-ready",
            "gauntlet finds a release blocker",
        ],
    },
    WorkflowMacro {
        id: "repair-derived-assets",
        title: "Repair stale or corrupt derived assets",
        intent_aliases: &["fix derived assets", "asset corruption", "rebuild index"],
        privacy_tier: "low",
        required_preflight_facts: &["db_present", "disk_headroom_ok"],
        steps: &[
            MacroStep {
                intent: "diagnose which derived assets are stale",
                command: "doctor.asset-truth-table",
                proof_gate: "assets-classified",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "plan the rebuild resource impact",
                command: "resource.what-if-rebuild",
                proof_gate: "rebuild-planned",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "rebuild only the stale assets",
                command: "doctor.rebuild-stale-assets",
                proof_gate: "assets-fresh",
                mutates: true,
                rch_rule: "offload-build",
            },
        ],
        proof_gates: &["assets-classified", "rebuild-planned", "assets-fresh"],
        optional_mutations: &["rebuild-stale-assets"],
        rollback_notes: "Rebuilds are additive and resumable; the prior asset is retained until the rebuild verifies.",
        rch_rules: "Honor disk headroom and concurrency caps from the resource plan before rebuilding.",
        stop_conditions: &["disk headroom is insufficient", "all assets report fresh"],
    },
    WorkflowMacro {
        id: "onboard-source",
        title: "Onboard a new agent-history source",
        intent_aliases: &["add a source", "onboard source", "first-run setup"],
        privacy_tier: "redacted",
        required_preflight_facts: &["sources_config_writable"],
        steps: &[
            MacroStep {
                intent: "preview the privacy exposure of the candidate source",
                command: "privacy.preview-exposure",
                proof_gate: "exposure-reviewed",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "register the source and run a dry sync",
                command: "sources.dry-sync",
                proof_gate: "dry-sync-ok",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "index the newly onboarded source",
                command: "index.incremental",
                proof_gate: "index-updated",
                mutates: true,
                rch_rule: "offload-build",
            },
        ],
        proof_gates: &["exposure-reviewed", "dry-sync-ok", "index-updated"],
        optional_mutations: &["register-source", "incremental-index"],
        rollback_notes: "Source registration is a config edit; remove the source entry to undo before indexing.",
        rch_rules: "Indexing of a large source should run off the interactive pane.",
        stop_conditions: &[
            "privacy exposure requires an opt-in the operator declines",
            "source path is unreadable",
        ],
    },
    WorkflowMacro {
        id: "export-encrypted-session",
        title: "Export an encrypted session for sharing",
        intent_aliases: &["export session", "encrypted export", "share a conversation"],
        privacy_tier: "sensitive",
        required_preflight_facts: &["session_selected", "export_key_available"],
        steps: &[
            MacroStep {
                intent: "preview what the export would include",
                command: "privacy.preview-exposure",
                proof_gate: "exposure-reviewed",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "confirm the encryption key and recipient policy",
                command: "export.confirm-key-policy",
                proof_gate: "key-policy-confirmed",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "produce the redaction-first encrypted export",
                command: "export.encrypted-html",
                proof_gate: "export-produced",
                mutates: true,
                rch_rule: "none",
            },
        ],
        proof_gates: &[
            "exposure-reviewed",
            "key-policy-confirmed",
            "export-produced",
        ],
        optional_mutations: &["write-encrypted-export"],
        rollback_notes: "The export is a new file; delete the output to undo. No source data is modified.",
        rch_rules: "Local-only; encryption never offloads.",
        stop_conditions: &[
            "the export key is unavailable",
            "privacy exposure is unacceptable for the recipient tier",
        ],
    },
    WorkflowMacro {
        id: "create-support-capsule",
        title: "Create a redacted support capsule",
        intent_aliases: &[
            "support bundle",
            "diagnostics capsule",
            "share a bug report",
        ],
        privacy_tier: "redacted",
        required_preflight_facts: &["db_present"],
        steps: &[
            MacroStep {
                intent: "preview the capsule's privacy exposure",
                command: "privacy.preview-exposure",
                proof_gate: "exposure-reviewed",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "gather redacted health and status evidence",
                command: "support.gather-evidence",
                proof_gate: "evidence-gathered",
                mutates: false,
                rch_rule: "none",
            },
            MacroStep {
                intent: "produce the redacted support capsule",
                command: "support.produce-capsule",
                proof_gate: "capsule-produced",
                mutates: true,
                rch_rule: "none",
            },
        ],
        proof_gates: &["exposure-reviewed", "evidence-gathered", "capsule-produced"],
        optional_mutations: &["write-support-capsule"],
        rollback_notes: "The capsule is a new bundle file; delete it to undo. Source data is untouched.",
        rch_rules: "Local-only; capsule generation never offloads.",
        stop_conditions: &[
            "required evidence is unavailable",
            "redaction would leave the capsule empty",
        ],
    },
];

/// Destructive tokens a macro recipe must never contain (advisory-only surface).
const FORBIDDEN_DESTRUCTIVE: &[&str] = &[
    "rm -rf",
    "git reset --hard",
    "git clean -f",
    "drop table",
    "drop database",
    "delete from",
    "mkfs",
    "> /dev/",
    ":(){",
];

/// Render the full registry (no preflight facts: every macro reports `unknown`
/// readiness, documenting the contract).
#[must_use]
pub fn render_workflow_macros_live() -> Value {
    render_payload("live", "live", None, None)
}

/// Render the registry from a checked-in swarm fixture source value.
///
/// The source may carry `{ "facts": { "<fact>": true, ... }, "macro": "<id>" }`
/// to evaluate preflight readiness and/or filter to one macro.
#[must_use]
pub fn render_workflow_macros_fixture(fixture_id: &str, source: Option<&Value>) -> Value {
    let facts = source.and_then(|value| value.get("facts"));
    let macro_filter = source
        .and_then(|value| value.get("macro"))
        .and_then(Value::as_str);
    render_payload(fixture_id, "fixture", facts, macro_filter)
}

fn render_payload(
    fixture_id: &str,
    source_kind: &str,
    facts: Option<&Value>,
    macro_filter: Option<&str>,
) -> Value {
    // Deterministic order: registry sorted by id.
    let mut selected: Vec<&WorkflowMacro> = MACROS
        .iter()
        .filter(|entry| macro_filter.is_none_or(|id| entry.id == id))
        .collect();
    selected.sort_by(|a, b| a.id.cmp(b.id));

    let rendered: Vec<Value> = selected
        .iter()
        .map(|entry| render_macro(entry, facts))
        .collect();

    // Registry self-validation: every macro must pass the recipe lint.
    let invalid: Vec<Value> = MACROS
        .iter()
        .filter_map(|entry| {
            let problems = validate_macro(entry);
            (!problems.is_empty()).then(|| json!({"macro": entry.id, "problems": problems}))
        })
        .collect();

    let ready_count = rendered
        .iter()
        .filter(|m| m.get("readiness").and_then(Value::as_str) == Some("ready"))
        .count();
    let blocked_count = rendered
        .iter()
        .filter(|m| m.get("readiness").and_then(Value::as_str) == Some("blocked"))
        .count();

    let status = if !invalid.is_empty() || blocked_count > 0 {
        "warning"
    } else {
        "ok"
    };
    let recommended_action = if !invalid.is_empty() {
        "fix-invalid-macros"
    } else if rendered.is_empty() {
        "unknown-macro-id"
    } else if blocked_count > 0 {
        "satisfy-preflight-facts"
    } else {
        "select-macro-to-run"
    };

    json!({
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "_meta": {
            "generated_at": Utc::now().to_rfc3339(),
            "source": source_kind,
            "fixture_id": fixture_id,
            "contract": "advisory workflow macro registry"
        },
        "summary": {
            "status": status,
            "macro_count": rendered.len(),
            "registry_size": MACROS.len(),
            "ready_count": ready_count,
            "blocked_count": blocked_count,
            "invalid_count": invalid.len(),
            "recommended_action": recommended_action
        },
        "macros": rendered,
        "invalid_macros": invalid,
        "mutation_contract": {
            "read_only": true,
            "apply_mode": false,
            "schedules_work": false,
            "mutates_files": false,
            "mutates_db": false,
            "touches_network": false
        },
        "guided_workflow": {
            "surface": "cass swarm macros --json",
            "bead_id": "coding_agent_session_search-guided-ops-repro-trust-5u82n.10",
            "apply_mode_available": false
        }
    })
}

fn render_macro(entry: &WorkflowMacro, facts: Option<&Value>) -> Value {
    let (readiness, missing) = readiness(entry, facts);
    json!({
        "id": entry.id,
        "title": entry.title,
        "intent_aliases": entry.intent_aliases,
        "privacy_tier": entry.privacy_tier,
        "required_preflight_facts": entry.required_preflight_facts,
        "readiness": readiness,
        "missing_facts": missing,
        "steps": entry.steps.iter().map(|step| json!({
            "intent": step.intent,
            "command": step.command,
            "proof_gate": step.proof_gate,
            "mutates": step.mutates,
            "rch_rule": step.rch_rule
        })).collect::<Vec<_>>(),
        "proof_gates": entry.proof_gates,
        "optional_mutations": entry.optional_mutations,
        "rollback_notes": entry.rollback_notes,
        "rch_rules": entry.rch_rules,
        "stop_conditions": entry.stop_conditions
    })
}

/// Evaluate readiness against supplied facts. Returns `(readiness, missing)`.
/// With no facts at all, readiness is `unknown` (advisory, not blocked).
fn readiness(entry: &WorkflowMacro, facts: Option<&Value>) -> (&'static str, Vec<String>) {
    let Some(facts) = facts.and_then(Value::as_object) else {
        return ("unknown", Vec::new());
    };
    let missing: Vec<String> = entry
        .required_preflight_facts
        .iter()
        .filter(|fact| facts.get(**fact).and_then(Value::as_bool) != Some(true))
        .map(|fact| (*fact).to_string())
        .collect();
    if missing.is_empty() {
        ("ready", missing)
    } else {
        ("blocked", missing)
    }
}

/// Lint a macro recipe. Returns the list of problems (empty == valid).
fn validate_macro(entry: &WorkflowMacro) -> Vec<String> {
    let mut problems = Vec::new();
    if entry.id.is_empty() {
        problems.push("empty id".to_string());
    }
    if entry.steps.is_empty() {
        problems.push("macro has no steps".to_string());
    }
    if entry.intent_aliases.is_empty() {
        problems.push("macro has no intent aliases".to_string());
    }
    if !matches!(entry.privacy_tier, "low" | "redacted" | "sensitive") {
        problems.push(format!("unknown privacy tier `{}`", entry.privacy_tier));
    }
    // Collect every command/intent/note string for the lint scans below.
    let mut texts: Vec<&str> = vec![entry.rollback_notes, entry.rch_rules];
    for step in entry.steps {
        texts.push(step.intent);
        texts.push(step.command);
        // A step's command must be a structured dotted intent, not a bare CLI
        // invocation an agent might copy-paste verbatim.
        if step.command.starts_with("cass ")
            || step.command.starts_with("bv ")
            || step.command == "cass"
            || step.command == "bv"
        {
            problems.push(format!(
                "step command `{}` is a bare cass/bv example",
                step.command
            ));
        }
    }
    for text in texts {
        let lower = text.to_ascii_lowercase();
        for token in FORBIDDEN_DESTRUCTIVE {
            if lower.contains(token) {
                problems.push(format!("recipe contains destructive token `{token}`"));
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_at_least_six_macros() {
        assert!(MACROS.len() >= 6, "registry must define >= 6 macros");
    }

    #[test]
    fn every_registered_macro_is_valid() {
        for entry in MACROS {
            let problems = validate_macro(entry);
            assert!(
                problems.is_empty(),
                "macro {} has problems: {problems:?}",
                entry.id
            );
        }
    }

    #[test]
    fn validator_rejects_destructive_and_bare_examples() {
        let bad = WorkflowMacro {
            id: "bad",
            title: "bad",
            intent_aliases: &["x"],
            privacy_tier: "low",
            required_preflight_facts: &[],
            steps: &[MacroStep {
                intent: "wipe everything",
                command: "cass index --force",
                proof_gate: "none",
                mutates: true,
                rch_rule: "none",
            }],
            proof_gates: &[],
            optional_mutations: &[],
            rollback_notes: "run rm -rf /tmp/x to clean up",
            rch_rules: "none",
            stop_conditions: &[],
        };
        let problems = validate_macro(&bad);
        assert!(problems.iter().any(|p| p.contains("bare cass/bv")));
        assert!(problems.iter().any(|p| p.contains("destructive token")));
    }

    #[test]
    fn no_registered_macro_emits_bare_cass_or_bv_or_destructive() {
        let out = render_workflow_macros_live();
        // Scope the lint to the macro recipes themselves (the envelope's
        // guided_workflow.surface legitimately names the `cass swarm macros` command).
        let text = serde_json::to_string(&out["macros"]).unwrap();
        // Structured dotted intents only — no runnable `cass `/`bv ` invocations.
        assert!(
            !text.contains("cass "),
            "rendered macros must not embed `cass ` examples"
        );
        assert!(
            !text.contains("bv "),
            "rendered macros must not embed `bv ` examples"
        );
        for token in ["rm -rf", "git reset --hard", "drop table"] {
            assert!(
                !text.to_ascii_lowercase().contains(token),
                "destructive token leaked: {token}"
            );
        }
        assert_eq!(out["summary"]["invalid_count"], json!(0));
    }

    #[test]
    fn deterministic_ordering_by_id() {
        let a = render_workflow_macros_live();
        let b = render_workflow_macros_live();
        assert_eq!(a["macros"], b["macros"]);
        let ids: Vec<&str> = a["macros"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "macros must be ordered by id");
    }

    #[test]
    fn healthy_scenario_is_ready() {
        let src = json!({
            "macro": "create-support-capsule",
            "facts": {"db_present": true}
        });
        let out = render_workflow_macros_fixture("macros-healthy", Some(&src));
        assert_eq!(out["summary"]["macro_count"], json!(1));
        assert_eq!(out["macros"][0]["readiness"], json!("ready"));
        assert_eq!(out["status"], json!("ok"));
    }

    #[test]
    fn blocked_scenario_lists_missing_facts() {
        let src = json!({
            "macro": "prepare-release",
            "facts": {"git_clean_or_known": true}
        });
        let out = render_workflow_macros_fixture("macros-blocked", Some(&src));
        assert_eq!(out["macros"][0]["readiness"], json!("blocked"));
        let missing = out["macros"][0]["missing_facts"].as_array().unwrap();
        assert!(missing.iter().any(|m| m == &json!("version_bumped")));
        assert_eq!(out["status"], json!("warning"));
    }

    #[test]
    fn unknown_macro_id_yields_empty_selection() {
        let src = json!({"macro": "does-not-exist"});
        let out = render_workflow_macros_fixture("macros-unknown", Some(&src));
        assert_eq!(out["summary"]["macro_count"], json!(0));
        assert_eq!(
            out["summary"]["recommended_action"],
            json!("unknown-macro-id")
        );
    }

    #[test]
    fn live_is_read_only_and_unknown_readiness() {
        let out = render_workflow_macros_live();
        assert_eq!(out["mutation_contract"]["read_only"], json!(true));
        assert_eq!(out["mutation_contract"]["apply_mode"], json!(false));
        assert!(out["summary"]["macro_count"].as_u64().unwrap() >= 6);
        assert_eq!(out["macros"][0]["readiness"], json!("unknown"));
    }
}
