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
use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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
    SearchCoverage,
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
        adapter: Adapter::SearchCoverage,
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
        Adapter::DoctorTruth => vec!["cass", "doctor", "check", "--data-dir", &data_dir, "--json"],
        Adapter::SearchCoverage => vec!["cass", "status", "--data-dir", &data_dir, "--json"],
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
    observation: Option<Value>,
}

fn evaluate_read_only_proof(
    command: AllowedCommand,
    proof_gate: &str,
    plan: &Value,
    request: &GuideRunRequest<'_>,
    privacy_accepted: bool,
    cost_accepted: bool,
    deadline: Instant,
) -> ProofResult {
    if request.source_kind != "fixture"
        && matches!(
            command.adapter,
            Adapter::Readiness
                | Adapter::DoctorTruth
                | Adapter::SearchCoverage
                | Adapter::SupportEvidence
        )
    {
        return evaluate_live_proof(command, request.data_dir, deadline);
    }
    if let Some(passed) = (request.source_kind == "fixture")
        .then(|| fixture_proof(request.fixture_context, proof_gate))
        .flatten()
    {
        return ProofResult {
            result: if passed { "passed" } else { "failed" },
            observation: None,
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
            observation: None,
            source: "preflight-facts",
            detail: "evaluated guide readiness from preflight facts".to_string(),
        },
        Adapter::DoctorTruth => ProofResult {
            result: if prerequisite_satisfied(plan, "db_present") {
                "passed"
            } else {
                "failed"
            },
            observation: None,
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
                observation: None,
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
                observation: None,
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
            observation: None,
            source: "preflight-facts",
            detail: "checked evidence source availability".to_string(),
        },
        Adapter::KeyPolicy => ProofResult {
            result: if prerequisite_satisfied(plan, "export_key_available") && privacy_accepted {
                "passed"
            } else {
                "needs-confirmation"
            },
            observation: None,
            source: "operator-and-preflight",
            detail: "requires an available key and exact privacy-tier acceptance".to_string(),
        },
        Adapter::SearchCoverage | Adapter::SupportBundle | Adapter::ObservationOnly => {
            ProofResult {
                result: "not-run",
                observation: None,
                source: "none",
                detail: "no automatic read-only proof adapter".to_string(),
            }
        }
    }
}

/// Whether a command may already have produced side effects is independent of
/// its exit status. In particular, a timeout after spawn is not a read-only run.
struct CommandFailure {
    started: bool,
    detail: &'static str,
}

impl CommandFailure {
    fn before_start(detail: &'static str) -> Self {
        Self {
            started: false,
            detail,
        }
    }

    fn after_start(detail: &'static str) -> Self {
        Self {
            started: true,
            detail,
        }
    }
}

/// Run the installed binary, never another `cass` selected by PATH. The shared
/// supervisor bounds both captured streams and terminates timed-out children.
fn run_bounded_cass(argv: &[String], timeout: Duration) -> Result<Output, CommandFailure> {
    let args = argv
        .get(1..)
        .filter(|args| !args.is_empty())
        .ok_or_else(|| CommandFailure::before_start("missing argv"))?;
    if argv.first().map(String::as_str) != Some("cass") || timeout.is_zero() {
        return Err(CommandFailure::before_start(
            "invalid command or exhausted command budget",
        ));
    }
    #[cfg(target_os = "linux")]
    let mut child = Command::new("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let mut child =
        Command::new(std::env::current_exe().map_err(|_| {
            CommandFailure::before_start("cannot resolve the running cass executable")
        })?);
    child
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    crate::sources::configure_child_process_group(&mut child);
    let child = child
        .spawn()
        .map_err(|_| CommandFailure::before_start("could not start cass command"))?;
    crate::sources::wait_for_child_output_with_limit(child, timeout, Some(2 * 1024 * 1024))
        .map_err(|_| CommandFailure::after_start("cass command failed to capture bounded output"))?
        .ok_or_else(|| CommandFailure::after_start("cass command deadline exceeded"))
}

fn unavailable_live_proof(detail: impl Into<String>) -> ProofResult {
    ProofResult {
        result: "not-run",
        source: "live-command",
        detail: detail.into(),
        observation: None,
    }
}

fn evaluate_live_proof(command: AllowedCommand, data_dir: &Path, deadline: Instant) -> ProofResult {
    let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
        return unavailable_live_proof("shared live proof deadline exhausted; no child started");
    };
    let Some(argv) = allowlisted_argv(command, data_dir) else {
        return unavailable_live_proof("no safe argv for live proof");
    };
    let output = match run_bounded_cass(&argv, timeout) {
        Ok(output) => output,
        Err(error) => return unavailable_live_proof(error.detail),
    };
    let payload = match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(payload) => payload,
        Err(_) => {
            return unavailable_live_proof("live proof returned malformed or non-JSON output");
        }
    };
    classify_live_proof(command.adapter, output.status.code(), &payload)
}

/// A diagnostic completing is distinct from the archive being healthy. A
/// degraded health report is useful support evidence, but cannot pass readiness.
/// Receipts contain only typed measurements: no paths, messages, or raw stderr.
fn classify_live_proof(adapter: Adapter, exit_code: Option<i32>, payload: &Value) -> ProofResult {
    if !matches!(exit_code, Some(0 | 1)) {
        return ProofResult {
            result: "failed",
            source: "live-command",
            detail: "live diagnostic did not complete (refusal, concurrency, or process failure)"
                .to_string(),
            observation: Some(json!({"exit_code": exit_code})),
        };
    }
    let Some(healthy) = payload.get("healthy").and_then(Value::as_bool) else {
        return unavailable_live_proof("live diagnostic lacks a typed healthy verdict");
    };
    if payload.get("error").is_some_and(|value| !value.is_null())
        || payload.get("err").is_some_and(|value| !value.is_null())
    {
        return unavailable_live_proof("live diagnostic returned an error envelope");
    }
    let checks = payload.get("checks").and_then(Value::as_array);
    let index = payload.get("index");
    let shape_valid = match adapter {
        Adapter::Readiness | Adapter::SupportEvidence => true,
        Adapter::DoctorTruth => checks.is_some_and(|checks| !checks.is_empty()),
        Adapter::SearchCoverage => index
            .and_then(|index| index.get("status"))
            .and_then(Value::as_str)
            .is_some(),
        _ => false,
    };
    if !shape_valid {
        return unavailable_live_proof("live diagnostic has an unsupported or incomplete schema");
    }
    let rebuilding = payload
        .pointer("/rebuild_progress/active")
        .and_then(Value::as_bool);
    let passed = adapter != Adapter::Readiness
        || (healthy && exit_code == Some(0) && rebuilding != Some(true));
    let observation = json!({
        "exit_code": exit_code,
        "healthy": healthy,
        "needs_rebuild": payload.get("needs_rebuild").and_then(Value::as_bool),
        "rebuild_active": rebuilding,
        "check_count": checks.map(Vec::len),
        "error_count": payload.get("errors").and_then(Value::as_array).map(Vec::len),
        "lexical": {
            "ready": index.and_then(|index| index.get("status")).and_then(Value::as_str)
                .map(|status| status == "ready"),
            "fresh": index.and_then(|index| index.get("fresh")).and_then(Value::as_bool),
            "hollow": index.and_then(|index| index.get("hollow")).and_then(Value::as_bool),
            "documents": index.and_then(|index| index.get("documents")).and_then(Value::as_u64),
            "live_documents": index.and_then(|index| index.get("live_documents")).and_then(Value::as_u64)
        }
    });
    ProofResult {
        result: if passed { "passed" } else { "failed" },
        source: "live-command",
        detail: match adapter {
            Adapter::Readiness if passed => "live health confirms search readiness",
            Adapter::Readiness => "live health is not ready; inspect health before mutating",
            Adapter::DoctorTruth => {
                "live doctor classified assets; this is not repair authorization"
            }
            Adapter::SearchCoverage => {
                "live status reported search coverage; unknown counts remain null"
            }
            _ => "live health evidence collected, including any degraded verdict",
        }
        .to_string(),
        observation: Some(observation),
    }
}

struct MutationResult {
    status: &'static str,
    proof_result: &'static str,
    detail: String,
    started: bool,
    observation: Option<Value>,
}

fn failed_mutation(started: bool, detail: impl Into<String>) -> MutationResult {
    MutationResult {
        status: "failed",
        proof_result: "failed",
        detail: detail.into(),
        started,
        observation: None,
    }
}

/// Check the artifact actually exists and retain a share-safe receipt. This is
/// a bounded manifest readability/fingerprint check, not an independent audit
/// of every bundled file or of the producer's redaction policy.
fn support_bundle_receipt(payload: &Value, data_dir: &Path) -> Result<Value, &'static str> {
    let root = data_dir
        .canonicalize()
        .map_err(|_| "support data directory unavailable")?;
    let bundle = payload
        .get("bundle_path")
        .and_then(Value::as_str)
        .ok_or("support command omitted bundle_path")?;
    let manifest = payload
        .get("manifest_path")
        .and_then(Value::as_str)
        .ok_or("support command omitted manifest_path")?;
    let bundle = Path::new(bundle)
        .canonicalize()
        .map_err(|_| "support bundle unavailable")?;
    let manifest_path = Path::new(manifest);
    let metadata =
        std::fs::symlink_metadata(manifest_path).map_err(|_| "support manifest unavailable")?;
    if !metadata.is_file() {
        return Err("support manifest must be a regular non-symlink file");
    }
    let manifest = manifest_path
        .canonicalize()
        .map_err(|_| "support manifest unavailable")?;
    if bundle == root
        || !bundle.starts_with(&root)
        || !bundle.is_dir()
        || !manifest.starts_with(&bundle)
    {
        return Err("support artifact escaped the selected data directory or bundle");
    }
    const LIMIT: u64 = 2 * 1024 * 1024;
    if metadata.len() > LIMIT {
        return Err("support manifest exceeds the receipt byte budget");
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&manifest)
        .map_err(|_| "support manifest could not be opened")?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "support manifest could not be read")?;
    if bytes.len() as u64 > LIMIT {
        return Err("support manifest exceeds the receipt byte budget");
    }
    let manifest_json: Value =
        serde_json::from_slice(&bytes).map_err(|_| "support manifest is not valid JSON")?;
    if !manifest_json.is_object() {
        return Err("support manifest must be a JSON object");
    }
    let relative_bundle = bundle
        .strip_prefix(&root)
        .map_err(|_| "support bundle is out of scope")?;
    let relative_manifest = manifest
        .strip_prefix(&root)
        .map_err(|_| "support manifest is out of scope")?;
    Ok(json!({
        "bundle_path": receipt_relative_path(relative_bundle),
        "manifest_path": receipt_relative_path(relative_manifest),
        "manifest_bytes": bytes.len(),
        "manifest_blake3": blake3::hash(&bytes).to_hex().to_string(),
        "manifest_readable": true,
        "contents_independently_verified": false
    }))
}

fn receipt_relative_path(path: &Path) -> String {
    let relative = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    format!("<data-dir>/{relative}")
}

fn classify_support_bundle(
    exit_code: Option<i32>,
    stdout: &[u8],
    data_dir: &Path,
) -> MutationResult {
    if exit_code != Some(0) {
        return failed_mutation(
            true,
            "support-bundle command did not exit successfully; partial files may remain",
        );
    }
    let payload: Value = match serde_json::from_slice(stdout) {
        Ok(payload) => payload,
        Err(_) => {
            return failed_mutation(
                true,
                "support-bundle command returned malformed JSON; partial files may remain",
            );
        }
    };
    let observation = match support_bundle_receipt(&payload, data_dir) {
        Ok(observation) => observation,
        Err(detail) => return failed_mutation(true, detail),
    };
    MutationResult {
        status: "executed",
        proof_result: "passed",
        detail:
            "support bundle produced; bounded manifest receipt recorded without private contents"
                .to_string(),
        started: true,
        observation: Some(observation),
    }
}

fn execute_support_bundle(argv: &[String], data_dir: &Path) -> MutationResult {
    match run_bounded_cass(argv, Duration::from_secs(120)) {
        Ok(output) => classify_support_bundle(output.status.code(), &output.stdout, data_dir),
        Err(error) => failed_mutation(error.started, error.detail),
    }
}

fn execute_mutation(command: AllowedCommand, argv: &[String], data_dir: &Path) -> MutationResult {
    match command.adapter {
        Adapter::SupportBundle => execute_support_bundle(argv, data_dir),
        _ => MutationResult {
            status: "adapter-unavailable",
            proof_result: "not-run",
            detail: "structured mutation has no closed, parameter-complete adapter".to_string(),
            started: false,
            observation: None,
        },
    }
}

fn mutation_contract(apply: bool, attempted: usize) -> Value {
    json!({
        "read_only": attempted == 0,
        "apply_mode": apply,
        "schedules_work": false,
        "mutates_files": attempted > 0,
        "mutates_db": false,
        "touches_network": false,
        "per_step_confirmation_required": true,
        "shell_evaluation": false
    })
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
    let fixture_context = (request.source_kind == "fixture")
        .then_some(request.fixture_context)
        .flatten();
    let triggered_stops = fixture_array(fixture_context, "triggered_stop_conditions")
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
    let fixture_declared_stops =
        fixture_context.is_some_and(|context| context.get("triggered_stop_conditions").is_some());
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
    let mut attempted_mutation_count = 0usize;
    let mut awaiting_confirmation = false;
    let mut blocked_step = false;

    let proof_deadline = Instant::now() + Duration::from_secs(15);
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
                proof_deadline,
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
            if let Some(observation) = proof.observation
                && let Some(entry) = transcript.last_mut().and_then(Value::as_object_mut)
            {
                entry.insert("observation".to_string(), observation);
            }
            continue;
        }

        let confirmed = confirmed_steps.contains(&order);
        let step_rch_ready = rch_rule != "offload-build" || request.allow_rch;
        let mut mutation_observation = None;
        let mut mutation_started = false;
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
            let mutation = execute_mutation(
                command,
                argv.as_deref().unwrap_or_default(),
                request.data_dir,
            );
            mutation_started = mutation.started;
            if mutation_started {
                attempted_mutation_count += 1;
            }
            mutation_observation = mutation.observation;
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
        if request.source_kind != "fixture"
            && let Some(entry) = transcript.last_mut().and_then(Value::as_object_mut)
        {
            entry.insert("mutation_started".to_string(), json!(mutation_started));
            if let Some(observation) = mutation_observation {
                entry.insert("observation".to_string(), observation);
            }
        }
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
        "deterministic_transcript": !request.apply || request.source_kind == "fixture",
        "shell_evaluation": false,
        "fixture_mutation_allowed": false,
        "confirmed_facts": request.confirmed_facts,
        "global_gates": global_gates,
        "automatic_read_only_step_count": automatic_read_only_count,
        "applied_mutation_count": applied_mutation_count,
        "attempted_mutation_count": attempted_mutation_count,
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
            mutation_contract(request.apply, attempted_mutation_count),
        );
    }
    plan
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::{Value, json};

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

    #[test]
    fn live_readiness_requires_a_healthy_successful_non_rebuilding_report() {
        use super::{Adapter, classify_live_proof};
        let ready = json!({"healthy": true, "rebuild_progress": {"active": false}});
        assert_eq!(
            classify_live_proof(Adapter::Readiness, Some(0), &ready).result,
            "passed"
        );
        for (code, payload) in [
            (Some(1), ready.clone()),
            (Some(0), json!({"healthy": false})),
            (
                Some(0),
                json!({"healthy": true, "rebuild_progress": {"active": true}}),
            ),
            (None, ready),
        ] {
            assert_ne!(
                classify_live_proof(Adapter::Readiness, code, &payload).result,
                "passed"
            );
        }
    }

    #[test]
    fn live_diagnostics_reject_error_envelopes_and_incomplete_schemas() {
        use super::{Adapter, classify_live_proof};
        for payload in [
            Value::Null,
            json!({}),
            json!({"healthy": "true"}),
            json!({"healthy": true, "error": {"message": "private"}}),
        ] {
            assert_ne!(
                classify_live_proof(Adapter::Readiness, Some(0), &payload).result,
                "passed"
            );
        }
        assert_ne!(
            classify_live_proof(
                Adapter::DoctorTruth,
                Some(0),
                &json!({"healthy": true, "checks": []})
            )
            .result,
            "passed"
        );
        assert_ne!(
            classify_live_proof(Adapter::SearchCoverage, Some(0), &json!({"healthy": true})).result,
            "passed"
        );
    }

    #[test]
    fn degraded_diagnostics_are_evidence_not_readiness() {
        use super::{Adapter, classify_live_proof};
        let payload = json!({"healthy": false, "needs_rebuild": true,
            "checks": [{"name": "lexical", "status": "warning"}],
            "index": {"status": "hollow", "hollow": true, "documents": 1}});
        for adapter in [
            Adapter::DoctorTruth,
            Adapter::SupportEvidence,
            Adapter::SearchCoverage,
        ] {
            assert_eq!(
                classify_live_proof(adapter, Some(1), &payload).result,
                "passed"
            );
        }
        assert_eq!(
            classify_live_proof(Adapter::Readiness, Some(1), &payload).result,
            "failed"
        );
    }

    #[test]
    fn live_receipts_project_measurements_without_private_content_or_invented_counts() {
        use super::{Adapter, classify_live_proof};
        let payload = json!({"healthy": true, "index": {"status": "ready", "path": "/private",
            "documents": 12, "live_documents": null}, "errors": ["secret diagnostic"],
            "source_path": "/secret", "content": "sensitive message"});
        let proof = classify_live_proof(Adapter::SearchCoverage, Some(0), &payload);
        let observation = proof.observation.expect("live receipt");
        assert_eq!(observation.pointer("/lexical/documents"), Some(&json!(12)));
        assert_eq!(
            observation.pointer("/lexical/live_documents"),
            Some(&Value::Null)
        );
        let text = observation.to_string();
        for private in ["private", "secret", "sensitive"] {
            assert!(!text.contains(private));
        }
    }

    #[test]
    fn live_proofs_ignore_fixture_overrides_and_expired_budgets_never_spawn() {
        use super::evaluate_read_only_proof;
        use std::time::Instant;
        let context = json!({"proof_results": {"readiness-ok": true}});
        let mut request = request(Path::new("/unread/missing/archive"));
        request.source_kind = "live";
        request.fixture_context = Some(&context);
        let command = allowed_command("search.readiness").expect("allowed proof");
        let proof = evaluate_read_only_proof(
            command,
            "readiness-ok",
            &json!({"readiness": "ready"}),
            &request,
            true,
            true,
            Instant::now(),
        );
        assert_eq!(proof.result, "not-run");
        assert_eq!(proof.source, "live-command");
        assert!(proof.detail.contains("deadline"));
    }

    #[test]
    fn fixture_proofs_remain_offline_even_without_observations() {
        use super::evaluate_read_only_proof;
        use std::time::Instant;
        let request = request(Path::new("/unread/missing/archive"));
        for id in [
            "search.readiness",
            "doctor.asset-truth-table",
            "diag.search-coverage",
            "support.gather-evidence",
        ] {
            let proof = evaluate_read_only_proof(
                allowed_command(id).expect("allowed proof"),
                "missing",
                &json!({}),
                &request,
                true,
                true,
                Instant::now(),
            );
            assert_ne!(proof.source, "live-command");
            assert!(proof.observation.is_none());
        }
    }

    #[test]
    fn coverage_and_doctor_argv_are_explicit_read_only_commands() {
        for id in ["doctor.asset-truth-table", "diag.search-coverage"] {
            let argv = allowlisted_argv(
                allowed_command(id).expect("allowed"),
                Path::new("/archive with spaces"),
            )
            .expect("argv");
            assert!(argv.iter().any(|arg| arg == "/archive with spaces"));
            assert!(argv.iter().any(|arg| arg == "--json"));
            assert!(
                !argv
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "--fix" | "--yes" | "repair" | "index"))
            );
        }
    }

    fn capsule_fixture() -> (tempfile::TempDir, Value) {
        let root = tempfile::tempdir().expect("capsule fixture");
        let bundle = root.path().join("doctor/support/capsule-1");
        std::fs::create_dir_all(&bundle).expect("bundle directory");
        let manifest = bundle.join("manifest.json");
        std::fs::write(&manifest, br#"{"private_content":"must not be echoed"}"#)
            .expect("manifest");
        let payload = json!({"bundle_path": bundle, "manifest_path": manifest});
        (root, payload)
    }

    #[test]
    fn capsule_success_requires_a_readable_in_scope_manifest_and_returns_a_receipt() {
        let (root, payload) = capsule_fixture();
        let result = super::classify_support_bundle(
            Some(0),
            &serde_json::to_vec(&payload).expect("JSON"),
            root.path(),
        );
        assert_eq!(result.status, "executed");
        assert!(result.started);
        let receipt = result.observation.expect("receipt");
        assert_eq!(receipt["manifest_readable"], true);
        assert_eq!(receipt["contents_independently_verified"], false);
        assert_eq!(
            receipt["bundle_path"],
            "<data-dir>/doctor/support/capsule-1"
        );
        assert_eq!(
            receipt["manifest_blake3"].as_str().expect("digest").len(),
            64
        );
        assert!(!receipt.to_string().contains("private_content"));
        assert!(!receipt.to_string().contains("must not be echoed"));
        assert!(
            !receipt
                .to_string()
                .contains(&root.path().display().to_string())
        );
    }

    #[test]
    fn capsule_exit_zero_without_artifacts_is_not_success() {
        let (root, mut payload) = capsule_fixture();
        payload["manifest_path"] = json!(root.path().join("missing.json"));
        for stdout in [
            b"not JSON".to_vec(),
            b"{}".to_vec(),
            serde_json::to_vec(&payload).expect("JSON"),
        ] {
            let result = super::classify_support_bundle(Some(0), &stdout, root.path());
            assert_eq!(result.status, "failed");
            assert!(result.started);
            assert!(result.observation.is_none());
        }
    }

    #[test]
    fn capsule_receipts_refuse_escaped_and_oversized_manifests() {
        let (root, mut payload) = capsule_fixture();
        let outside = tempfile::tempdir().expect("outside");
        let manifest = outside.path().join("manifest.json");
        std::fs::write(&manifest, b"{}").expect("outside manifest");
        payload["manifest_path"] = json!(manifest);
        assert!(super::support_bundle_receipt(&payload, root.path()).is_err());
        let manifest = root.path().join("doctor/support/capsule-1/large.json");
        std::fs::File::create(&manifest)
            .expect("large manifest")
            .set_len(2 * 1024 * 1024 + 1)
            .expect("large length");
        payload["manifest_path"] = json!(manifest);
        assert!(super::support_bundle_receipt(&payload, root.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn capsule_receipts_refuse_manifest_symlinks() {
        let (root, mut payload) = capsule_fixture();
        let target = root.path().join("doctor/support/capsule-1/manifest.json");
        let link = root.path().join("doctor/support/capsule-1/link.json");
        std::os::unix::fs::symlink(&target, &link).expect("manifest symlink");
        payload["manifest_path"] = json!(link);
        assert!(super::support_bundle_receipt(&payload, root.path()).is_err());
    }

    #[test]
    fn failed_started_mutations_never_claim_read_only_or_automatic_rollback() {
        let error = super::CommandFailure::after_start("timeout");
        let result = super::failed_mutation(error.started, error.detail);
        assert_eq!(result.status, "failed");
        let contract = super::mutation_contract(true, usize::from(result.started));
        assert_eq!(contract["read_only"], false);
        assert_eq!(contract["mutates_files"], true);
        assert_eq!(contract["mutates_db"], false);
        let not_started = super::CommandFailure::before_start("spawn failure");
        let contract = super::mutation_contract(true, usize::from(not_started.started));
        assert_eq!(contract["read_only"], true);
        assert_eq!(contract["mutates_files"], false);
    }

    #[test]
    fn malformed_bounded_commands_do_not_start_processes() {
        for argv in [
            Vec::new(),
            vec!["cass".to_string()],
            vec!["sh".to_string(), "health".to_string()],
        ] {
            match super::run_bounded_cass(&argv, std::time::Duration::ZERO) {
                Err(error) => assert!(!error.started),
                Ok(_) => panic!("invalid command must not start"),
            }
        }
    }

    #[test]
    fn live_stop_confirmation_cannot_be_supplied_by_fixture_context() {
        let context = json!({"triggered_stop_conditions": []});
        let mut request = request(Path::new("/not-touched"));
        request.apply = true;
        request.source_kind = "live";
        request.fixture_context = Some(&context);
        request.confirmed_steps = &[1];
        let plan = json!({"intent": {"recognized": true}, "readiness": "ready",
            "plan": {"privacy_tier": "low", "cost_risk": {"risk_level": "low"},
                "stop_conditions": ["required evidence unavailable"],
                "steps": [{"order": 1, "command": "support.produce-capsule",
                    "mutates": true, "proof_gate": "capsule-produced", "rch_rule": "none"}]}});
        let output = render_execution(plan, &request);
        assert_eq!(
            output.pointer("/execution/overall_status"),
            Some(&json!("blocked"))
        );
        assert_eq!(
            output.pointer("/execution/attempted_mutation_count"),
            Some(&json!(0))
        );
        assert_eq!(
            output.pointer("/mutation_contract/read_only"),
            Some(&json!(true))
        );
    }
}
