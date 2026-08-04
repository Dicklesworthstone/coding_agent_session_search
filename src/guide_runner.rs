//! Gated execution policy for `cass guide --apply`.
//!
//! Workflow macros deliberately store structured command identifiers rather
//! than shell snippets. This module preserves that trust boundary: identifiers
//! are resolved through a closed argv allowlist, argv is always tokenized, and
//! no shell is involved. Read-only proof adapters may run automatically;
//! mutating adapters additionally require every global gate plus an explicit
//! confirmation for the exact step number. Fixture runs are deterministic and
//! permanently non-mutating.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

pub const SCHEMA_VERSION: &str = "cass.guide.execution.v1";
pub const BEAD_ID: &str = "coding_agent_session_search-guided-ops-repro-trust-5u82n.17";

/// Operator grants supplied to the gated runner.
pub struct GuideRunRequest<'a> {
    pub apply: bool,
    pub confirmed_steps: &'a [usize],
    pub confirmed_facts: &'a [String],
    pub accepted_privacy_tier: Option<&'a str>,
    pub accepted_cost_risk: Option<&'a str>,
    pub allow_rch: bool,
    pub stop_conditions_clear: bool,
    pub source_kind: &'a str,
    pub fixture_context: Option<&'a Value>,
    pub data_dir: &'a Path,
}

#[derive(Clone, Copy)]
struct AllowedCommand {
    id: &'static str,
    mutation_class: &'static str,
    adapter: Adapter,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Adapter {
    Readiness,
    DoctorTruth,
    ResourcePlan,
    PrivacyPreview,
    SupportEvidence,
    KeyPolicy,
    SupportBundle,
    ObservationOnly,
}

const ALLOWLIST: &[AllowedCommand] = &[
    AllowedCommand {
        id: "search.readiness",
        mutation_class: "read-only-proof",
        adapter: Adapter::Readiness,
    },
    AllowedCommand {
        id: "search.two-tier-explain",
        mutation_class: "read-only-proof",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "diag.search-coverage",
        mutation_class: "read-only-proof",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "diag.ci-first-failure",
        mutation_class: "read-only-proof",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "verify.reproduce-gate",
        mutation_class: "read-only-proof",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "verify.rerun-gate",
        mutation_class: "source-mutation",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "verify.release-gauntlet",
        mutation_class: "read-only-proof",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "release.verify-channels",
        mutation_class: "read-only-proof",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "release.changelog-review",
        mutation_class: "read-only-proof",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "doctor.asset-truth-table",
        mutation_class: "read-only-proof",
        adapter: Adapter::DoctorTruth,
    },
    AllowedCommand {
        id: "resource.what-if-rebuild",
        mutation_class: "read-only-proof",
        adapter: Adapter::ResourcePlan,
    },
    AllowedCommand {
        id: "doctor.rebuild-stale-assets",
        mutation_class: "derived-asset-mutation",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "privacy.preview-exposure",
        mutation_class: "read-only-proof",
        adapter: Adapter::PrivacyPreview,
    },
    AllowedCommand {
        id: "sources.dry-sync",
        mutation_class: "read-only-proof",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "index.incremental",
        mutation_class: "index-mutation",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "export.confirm-key-policy",
        mutation_class: "read-only-proof",
        adapter: Adapter::KeyPolicy,
    },
    AllowedCommand {
        id: "export.encrypted-html",
        mutation_class: "derived-artifact-mutation",
        adapter: Adapter::ObservationOnly,
    },
    AllowedCommand {
        id: "support.gather-evidence",
        mutation_class: "read-only-proof",
        adapter: Adapter::SupportEvidence,
    },
    AllowedCommand {
        id: "support.produce-capsule",
        mutation_class: "derived-artifact-mutation",
        adapter: Adapter::SupportBundle,
    },
];

fn allowed_command(id: &str) -> Option<AllowedCommand> {
    ALLOWLIST.iter().copied().find(|entry| entry.id == id)
}

fn is_mutating(class: &str) -> bool {
    class != "read-only-proof"
}

fn has_shell_metacharacter(token: &str) -> bool {
    token.chars().any(|ch| {
        matches!(
            ch,
            ';' | '|' | '&' | '>' | '<' | '`' | '$' | '\n' | '\r' | '\0'
        )
    })
}

fn allowlisted_argv(command: AllowedCommand, data_dir: &Path) -> Option<Vec<String>> {
    let data_dir = data_dir.display().to_string();
    let argv = match command.adapter {
        Adapter::Readiness => vec!["cass", "health", "--data-dir", &data_dir, "--json"],
        Adapter::DoctorTruth => vec!["cass", "doctor", "--data-dir", &data_dir, "--json"],
        Adapter::ResourcePlan => vec![
            "cass",
            "swarm",
            "resource-plan",
            "--action",
            "full-index",
            "--json",
        ],
        Adapter::PrivacyPreview => vec!["cass", "swarm", "privacy-preview", "--json"],
        Adapter::SupportEvidence => vec!["cass", "health", "--data-dir", &data_dir, "--json"],
        Adapter::KeyPolicy => vec!["cass-internal", "confirm-key-policy"],
        Adapter::SupportBundle => vec![
            "cass",
            "doctor",
            "support-bundle",
            "--data-dir",
            &data_dir,
            "--json",
        ],
        Adapter::ObservationOnly => return None,
    };
    let tokens = argv.into_iter().map(str::to_string).collect::<Vec<_>>();
    if tokens.iter().any(|token| has_shell_metacharacter(token)) {
        return None;
    }
    Some(tokens)
}

fn gate(name: &str, result: &str, detail: impl Into<String>) -> Value {
    json!({ "gate": name, "result": result, "detail": detail.into() })
}

fn exact_acceptance(expected: &str, actual: Option<&str>) -> bool {
    actual.is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn fixture_array<'a>(context: Option<&'a Value>, key: &str) -> Option<&'a [Value]> {
    context?.get(key)?.as_array().map(Vec::as_slice)
}

fn fixture_proof(context: Option<&Value>, proof_gate: &str) -> Option<bool> {
    context?.get("proof_results")?.get(proof_gate)?.as_bool()
}

fn prerequisite_satisfied(plan: &Value, fact: &str) -> bool {
    plan.pointer("/plan/prerequisites")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("fact").and_then(Value::as_str) == Some(fact)
                    && item.get("status").and_then(Value::as_str) == Some("satisfied")
            })
        })
}

fn steps_have_command(plan: &Value, command: &str) -> bool {
    plan.pointer("/plan/steps")
        .and_then(Value::as_array)
        .is_some_and(|steps| {
            steps
                .iter()
                .any(|step| step.get("command").and_then(Value::as_str) == Some(command))
        })
}

fn resource_readiness(plan: &Value, request: &GuideRunRequest<'_>) -> Option<String> {
    let action = plan
        .pointer("/plan/cost_risk/resource_action")
        .and_then(Value::as_str)?;
    let source = request
        .fixture_context
        .and_then(|context| context.get("resource_plan"));
    let payload = if request.source_kind == "fixture" {
        crate::resource_plan::render_resource_plan_fixture("guide-apply", source, Some(action))
    } else {
        crate::resource_plan::render_resource_plan_live(Some(action))
    };
    payload
        .pointer("/summary/readiness")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn privacy_readiness(request: &GuideRunRequest<'_>) -> String {
    let source = request
        .fixture_context
        .and_then(|context| context.get("privacy_preview"));
    let payload = if request.source_kind == "fixture" {
        crate::privacy_exposure::render_privacy_exposure_fixture("guide-apply", source)
    } else {
        crate::privacy_exposure::render_privacy_exposure_live()
    };
    payload
        .pointer("/summary/readiness")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

struct ProofResult {
    result: &'static str,
    source: &'static str,
    detail: String,
}

fn evaluate_read_only_proof(
    command: AllowedCommand,
    proof_gate: &str,
    plan: &Value,
    request: &GuideRunRequest<'_>,
    privacy_accepted: bool,
    cost_accepted: bool,
) -> ProofResult {
    if let Some(passed) = fixture_proof(request.fixture_context, proof_gate) {
        return ProofResult {
            result: if passed { "passed" } else { "failed" },
            source: "fixture-observation",
            detail: "deterministic fixture proof observation".to_string(),
        };
    }
    match command.adapter {
        Adapter::Readiness => ProofResult {
            result: if plan.get("readiness").and_then(Value::as_str) == Some("ready") {
                "passed"
            } else {
                "failed"
            },
            source: "preflight-facts",
            detail: "evaluated guide readiness from preflight facts".to_string(),
        },
        Adapter::DoctorTruth => ProofResult {
            result: if prerequisite_satisfied(plan, "db_present") {
                "passed"
            } else {
                "failed"
            },
            source: "preflight-facts",
            detail: "evaluated canonical database presence before derived-asset diagnosis"
                .to_string(),
        },
        Adapter::ResourcePlan => {
            let readiness = resource_readiness(plan, request).unwrap_or_else(|| "unknown".into());
            ProofResult {
                result: if readiness == "ready" || (readiness == "review-required" && cost_accepted)
                {
                    "passed"
                } else if readiness == "blocked" {
                    "failed"
                } else {
                    "needs-confirmation"
                },
                source: "resource-what-if",
                detail: format!("resource plan readiness: {readiness}"),
            }
        }
        Adapter::PrivacyPreview => {
            let readiness = privacy_readiness(request);
            ProofResult {
                result: if readiness == "opt-in-required" {
                    "failed"
                } else if readiness == "ready" || privacy_accepted {
                    "passed"
                } else {
                    "needs-confirmation"
                },
                source: "privacy-preview",
                detail: format!("privacy preview readiness: {readiness}"),
            }
        }
        Adapter::SupportEvidence => ProofResult {
            result: if prerequisite_satisfied(plan, "db_present") {
                "passed"
            } else {
                "failed"
            },
            source: "preflight-facts",
            detail: "checked evidence source availability".to_string(),
        },
        Adapter::KeyPolicy => ProofResult {
            result: if prerequisite_satisfied(plan, "export_key_available") && privacy_accepted {
                "passed"
            } else {
                "needs-confirmation"
            },
            source: "operator-and-preflight",
            detail: "requires an available key and exact privacy-tier acceptance".to_string(),
        },
        Adapter::SupportBundle | Adapter::ObservationOnly => ProofResult {
            result: "not-run",
            source: "none",
            detail: "no automatic read-only proof adapter".to_string(),
        },
    }
}

struct MutationResult {
    status: &'static str,
    proof_result: &'static str,
    detail: String,
}

fn execute_support_bundle(argv: &[String]) -> MutationResult {
    // argv[0] is the display binary. All remaining tokens came from the closed
    // allowlist above; no shell parses or expands them.
    let args = argv.get(1..).unwrap_or_default();
    #[cfg(target_os = "linux")]
    let result = Command::new("/proc/self/exe").args(args).output();
    #[cfg(not(target_os = "linux"))]
    let result = Command::new("cass").args(args).output();
    match result {
        Ok(output) if output.status.success() => MutationResult {
            status: "executed",
            proof_result: "passed",
            detail: "allowlisted support-bundle adapter exited successfully".to_string(),
        },
        Ok(output) => MutationResult {
            status: "failed",
            proof_result: "failed",
            detail: format!(
                "allowlisted support-bundle adapter exited with {}",
                output.status.code().unwrap_or(-1)
            ),
        },
        Err(error) => MutationResult {
            status: "failed",
            proof_result: "failed",
            detail: format!("could not start allowlisted support-bundle adapter: {error}"),
        },
    }
}

fn execute_mutation(command: AllowedCommand, argv: &[String]) -> MutationResult {
    match command.adapter {
        Adapter::SupportBundle => execute_support_bundle(argv),
        _ => MutationResult {
            status: "adapter-unavailable",
            proof_result: "not-run",
            detail: "structured mutation has no closed, parameter-complete adapter".to_string(),
        },
    }
}

/// Attach a deterministic dry-run/apply transcript to a recognized guide plan.
#[must_use]
pub fn render_execution(mut plan: Value, request: &GuideRunRequest<'_>) -> Value {
    let recognized = plan
        .pointer("/intent/recognized")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !recognized {
        return plan;
    }

    let confirmed_steps = request
        .confirmed_steps
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let readiness = plan
        .get("readiness")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let privacy_tier = plan
        .pointer("/plan/privacy_tier")
        .and_then(Value::as_str)
        .unwrap_or("low");
    let cost_risk = plan
        .pointer("/plan/cost_risk/risk_level")
        .and_then(Value::as_str)
        .unwrap_or("low");
    let privacy_accepted =
        privacy_tier == "low" || exact_acceptance(privacy_tier, request.accepted_privacy_tier);
    let privacy_preview_required = steps_have_command(&plan, "privacy.preview-exposure");
    let privacy_state = privacy_preview_required.then(|| privacy_readiness(request));
    let privacy_blocked = privacy_state.as_deref() == Some("opt-in-required");
    let resource = resource_readiness(&plan, request);
    let cost_accepted =
        cost_risk == "low" || exact_acceptance(cost_risk, request.accepted_cost_risk);
    let resource_blocked = resource.as_deref() == Some("blocked");

    let declared_stops = plan
        .pointer("/plan/stop_conditions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let triggered_stops = fixture_array(request.fixture_context, "triggered_stop_conditions")
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .filter(|candidate| {
            declared_stops
                .iter()
                .filter_map(Value::as_str)
                .any(|s| s == *candidate)
        })
        .map(str::to_string)
        .collect::<Vec<_>>();
    let fixture_declared_stops = request
        .fixture_context
        .is_some_and(|context| context.get("triggered_stop_conditions").is_some());
    let stops_clear = triggered_stops.is_empty()
        && (request.stop_conditions_clear || fixture_declared_stops || declared_stops.is_empty());

    let steps = plan
        .pointer("/plan/steps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let offload_mutation = steps.iter().any(|step| {
        step.get("mutates").and_then(Value::as_bool) == Some(true)
            && step.get("rch_rule").and_then(Value::as_str) == Some("offload-build")
    });

    let mut global_gates = vec![
        gate(
            "readiness",
            if readiness == "ready" {
                "passed"
            } else {
                "blocked"
            },
            format!("guide readiness is {readiness}"),
        ),
        gate(
            "stop-conditions",
            if !triggered_stops.is_empty() {
                "blocked"
            } else if stops_clear {
                "passed"
            } else {
                "needs-confirmation"
            },
            if triggered_stops.is_empty() {
                "no triggered stop condition was observed".to_string()
            } else {
                format!("triggered: {}", triggered_stops.join(" | "))
            },
        ),
        gate(
            "forbidden-shortcuts",
            "passed",
            "closed argv allowlist; shell evaluation disabled",
        ),
        gate(
            "privacy-tier",
            if privacy_blocked {
                "blocked"
            } else if privacy_accepted {
                "passed"
            } else {
                "needs-confirmation"
            },
            format!(
                "declared tier: {privacy_tier}; preview readiness: {}; acceptance must match exactly",
                privacy_state.as_deref().unwrap_or("not-applicable")
            ),
        ),
        gate(
            "cost-risk",
            if resource_blocked {
                "blocked"
            } else if cost_accepted {
                "passed"
            } else {
                "needs-confirmation"
            },
            format!(
                "declared risk: {cost_risk}; resource readiness: {}",
                resource.as_deref().unwrap_or("not-applicable")
            ),
        ),
        gate(
            "rch",
            if !offload_mutation || request.allow_rch {
                "passed"
            } else {
                "needs-confirmation"
            },
            if offload_mutation {
                "mutating offload step requires --allow-rch"
            } else {
                "no mutating offload step"
            },
        ),
    ];
    let mutating_steps = steps
        .iter()
        .filter(|step| step.get("mutates").and_then(Value::as_bool) == Some(true))
        .filter_map(|step| step.get("order").and_then(Value::as_u64))
        .filter_map(|order| usize::try_from(order).ok())
        .collect::<BTreeSet<_>>();
    let invalid_confirmations = confirmed_steps
        .iter()
        .copied()
        .filter(|step| !mutating_steps.contains(step))
        .collect::<Vec<_>>();
    if !invalid_confirmations.is_empty() {
        global_gates.push(gate(
            "step-confirmations",
            "blocked",
            format!("unknown step confirmations: {invalid_confirmations:?}"),
        ));
    }

    let global_mutation_ready = readiness == "ready"
        && stops_clear
        && triggered_stops.is_empty()
        && privacy_accepted
        && !privacy_blocked
        && cost_accepted
        && !resource_blocked
        && (!offload_mutation || request.allow_rch)
        && invalid_confirmations.is_empty();
    let mut prior_proofs_passed = true;
    let mut transcript = Vec::with_capacity(steps.len());
    let mut automatic_read_only_count = 0usize;
    let mut applied_mutation_count = 0usize;
    let mut awaiting_confirmation = false;
    let mut blocked_step = false;

    for step in &steps {
        let order = step
            .get("order")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0);
        let structured = step.get("command").and_then(Value::as_str).unwrap_or("");
        let proof_gate = step
            .get("proof_gate")
            .and_then(Value::as_str)
            .unwrap_or("unspecified");
        let declared_mutates = step
            .get("mutates")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let rch_rule = step
            .get("rch_rule")
            .and_then(Value::as_str)
            .unwrap_or("none");
        let Some(command) = allowed_command(structured) else {
            transcript.push(json!({
                "step": order,
                "structured_command": structured,
                "argv": Value::Null,
                "argv_tokenized": true,
                "allowlist_result": "rejected",
                "mutation_class": if declared_mutates { "unknown-mutation" } else { "unknown-read-only" },
                "confirmation": { "required": declared_mutates, "provided": confirmed_steps.contains(&order) },
                "rch": { "rule": rch_rule, "result": "blocked" },
                "proof_gate": { "name": proof_gate, "result": "not-run", "source": "none" },
                "result": "blocked",
                "detail": "structured command is not in the closed guide-runner allowlist"
            }));
            blocked_step = true;
            prior_proofs_passed = false;
            continue;
        };
        let class_mutates = is_mutating(command.mutation_class);
        let allowlist_consistent = class_mutates == declared_mutates;
        let argv = allowlisted_argv(command, request.data_dir);
        if !allowlist_consistent {
            transcript.push(json!({
                "step": order,
                "structured_command": structured,
                "argv": argv,
                "argv_tokenized": true,
                "allowlist_result": "rejected",
                "mutation_class": command.mutation_class,
                "confirmation": { "required": declared_mutates, "provided": confirmed_steps.contains(&order) },
                "rch": { "rule": rch_rule, "result": "blocked" },
                "proof_gate": { "name": proof_gate, "result": "not-run", "source": "none" },
                "result": "blocked",
                "detail": "macro mutation bit disagrees with the closed allowlist classification"
            }));
            blocked_step = true;
            prior_proofs_passed = false;
            continue;
        }

        if !request.apply {
            transcript.push(json!({
                "step": order,
                "structured_command": structured,
                "argv": argv,
                "argv_tokenized": true,
                "allowlist_result": "allowed",
                "mutation_class": command.mutation_class,
                "confirmation": { "required": declared_mutates, "provided": false },
                "rch": { "rule": rch_rule, "result": "not-run" },
                "proof_gate": { "name": proof_gate, "result": "not-run", "source": "dry-run" },
                "result": "planned",
                "detail": "dry-run: no step executed"
            }));
            continue;
        }

        if !declared_mutates {
            let proof = evaluate_read_only_proof(
                command,
                proof_gate,
                &plan,
                request,
                privacy_accepted,
                cost_accepted,
            );
            let result = if proof.result == "passed" {
                automatic_read_only_count += 1;
                "proof-passed"
            } else if proof.result == "failed" {
                blocked_step = true;
                "proof-failed"
            } else {
                awaiting_confirmation = true;
                "proof-unavailable"
            };
            prior_proofs_passed &= proof.result == "passed";
            transcript.push(json!({
                "step": order,
                "structured_command": structured,
                "argv": argv,
                "argv_tokenized": true,
                "allowlist_result": "allowed",
                "mutation_class": command.mutation_class,
                "confirmation": { "required": false, "provided": false },
                "rch": { "rule": rch_rule, "result": if rch_rule == "offload-build" && !request.allow_rch { "not-run" } else { "passed" } },
                "proof_gate": { "name": proof_gate, "result": proof.result, "source": proof.source },
                "result": result,
                "detail": proof.detail
            }));
            continue;
        }

        let confirmed = confirmed_steps.contains(&order);
        let step_rch_ready = rch_rule != "offload-build" || request.allow_rch;
        let (result, proof_result, detail) = if !global_mutation_ready {
            blocked_step = true;
            (
                "blocked",
                "not-run",
                "one or more readiness/privacy/cost/rch/stop-condition gates did not pass".into(),
            )
        } else if !prior_proofs_passed {
            blocked_step = true;
            (
                "blocked",
                "not-run",
                "a preceding proof gate did not pass".into(),
            )
        } else if !confirmed {
            awaiting_confirmation = true;
            (
                "awaiting-confirmation",
                "not-run",
                "mutation requires --confirm-step for this exact step".into(),
            )
        } else if request.source_kind == "fixture" {
            blocked_step = true;
            (
                "fixture-protected",
                "not-run",
                "fixture apply is permanently non-mutating".into(),
            )
        } else if argv.is_none() {
            blocked_step = true;
            (
                "adapter-unavailable",
                "not-run",
                "no parameter-complete argv adapter is available".into(),
            )
        } else {
            let mutation = execute_mutation(command, argv.as_deref().unwrap_or_default());
            if mutation.status == "executed" {
                applied_mutation_count += 1;
            } else {
                blocked_step = true;
            }
            (mutation.status, mutation.proof_result, mutation.detail)
        };
        prior_proofs_passed &= proof_result == "passed";
        transcript.push(json!({
            "step": order,
            "structured_command": structured,
            "argv": argv,
            "argv_tokenized": true,
            "allowlist_result": "allowed",
            "mutation_class": command.mutation_class,
            "confirmation": { "required": true, "provided": confirmed },
            "rch": { "rule": rch_rule, "result": if step_rch_ready { "passed" } else { "needs-confirmation" } },
            "proof_gate": { "name": proof_gate, "result": proof_result, "source": "mutation-adapter" },
            "result": result,
            "detail": detail
        }));
    }

    let overall_status = if !request.apply {
        "dry-run"
    } else if blocked_step {
        "blocked"
    } else if awaiting_confirmation {
        "awaiting-confirmation"
    } else {
        "completed"
    };
    let execution = json!({
        "schema_version": SCHEMA_VERSION,
        "bead_id": BEAD_ID,
        "mode": if request.apply { "apply" } else { "dry-run" },
        "overall_status": overall_status,
        "deterministic_transcript": true,
        "shell_evaluation": false,
        "fixture_mutation_allowed": false,
        "confirmed_facts": request.confirmed_facts,
        "global_gates": global_gates,
        "automatic_read_only_step_count": automatic_read_only_count,
        "applied_mutation_count": applied_mutation_count,
        "transcript": transcript
    });
    if let Some(map) = plan.as_object_mut() {
        map.insert("execution".to_string(), execution);
        if request.apply {
            map.insert(
                "status".to_string(),
                json!(if overall_status == "completed" {
                    "ok"
                } else {
                    "warning"
                }),
            );
        }
        map.insert(
            "recommended_action".to_string(),
            json!(match overall_status {
                "dry-run" => "review-plan-or-rerun-with-apply",
                "awaiting-confirmation" => "provide-required-explicit-confirmations",
                "blocked" => "resolve-execution-gates-before-retrying",
                _ => "review-execution-transcript",
            }),
        );
        map.insert(
            "mutation_contract".to_string(),
            json!({
                "read_only": applied_mutation_count == 0,
                "apply_mode": request.apply,
                "schedules_work": false,
                "mutates_files": applied_mutation_count > 0,
                "mutates_db": false,
                "touches_network": false,
                "per_step_confirmation_required": true,
                "shell_evaluation": false
            }),
        );
    }
    plan
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::{
        GuideRunRequest, allowed_command, allowlisted_argv, has_shell_metacharacter,
        render_execution,
    };

    fn request<'a>(data_dir: &'a Path) -> GuideRunRequest<'a> {
        GuideRunRequest {
            apply: false,
            confirmed_steps: &[],
            confirmed_facts: &[],
            accepted_privacy_tier: None,
            accepted_cost_risk: None,
            allow_rch: false,
            stop_conditions_clear: false,
            source_kind: "fixture",
            fixture_context: None,
            data_dir,
        }
    }

    #[test]
    fn allowlist_rejects_shell_metacharacters_and_unknown_ids() -> Result<(), String> {
        if allowed_command("health;touch /tmp/nope").is_some()
            || allowed_command("sh").is_some()
            || !has_shell_metacharacter("$(touch-nope)")
            || !has_shell_metacharacter("health|tee")
        {
            return Err("closed allowlist accepted an unsafe command shape".into());
        }
        Ok(())
    }

    #[test]
    fn allowlisted_argv_is_tokenized_and_shell_free() -> Result<(), String> {
        let Some(command) = allowed_command("search.readiness") else {
            return Err("search.readiness missing from allowlist".into());
        };
        let Some(argv) = allowlisted_argv(command, Path::new("/tmp/cass-guide-test")) else {
            return Err("search.readiness did not produce argv".into());
        };
        if argv.first().map(String::as_str) != Some("cass")
            || argv.iter().any(|token| has_shell_metacharacter(token))
        {
            return Err("allowlisted argv was not safely tokenized".into());
        }
        Ok(())
    }

    #[test]
    fn dry_run_never_executes_steps() -> Result<(), String> {
        let facts = json!({"db_present": true});
        let plan = crate::guide_planner::render_guide_plan(
            "support-capsule",
            Some(&facts),
            "fixture",
            "test",
        );
        let output = render_execution(plan, &request(Path::new("/tmp/cass-guide-test")));
        let transcript_is_planned = output
            .pointer("/execution/transcript")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|steps| {
                steps.iter().all(|step| {
                    step.get("result").and_then(serde_json::Value::as_str) == Some("planned")
                })
            });
        if output.pointer("/execution/mode") != Some(&json!("dry-run"))
            || output.pointer("/execution/applied_mutation_count") != Some(&json!(0))
            || !transcript_is_planned
        {
            return Err("dry-run transcript reported execution".into());
        }
        Ok(())
    }
}
