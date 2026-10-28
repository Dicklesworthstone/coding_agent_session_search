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
    pub db_path: Option<&'a Path>,
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
    LexicalRepair,
    SourcesDrySync,
    IncrementalIndex,
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
        adapter: Adapter::LexicalRepair,
    },
    AllowedCommand {
        id: "privacy.preview-exposure",
        mutation_class: "read-only-proof",
        adapter: Adapter::PrivacyPreview,
    },
    AllowedCommand {
        id: "sources.dry-sync",
        mutation_class: "read-only-proof",
        adapter: Adapter::SourcesDrySync,
    },
    AllowedCommand {
        id: "index.incremental",
        mutation_class: "index-mutation",
        adapter: Adapter::IncrementalIndex,
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

fn allowlisted_argv(
    command: AllowedCommand,
    data_dir: &Path,
    db_path: Option<&Path>,
) -> Option<Vec<String>> {
    let default_db = data_dir.join("agent_search.db");
    let db = db_path.unwrap_or(&default_db).to_str()?;
    let data_dir = data_dir.to_str()?;
    let argv = match command.adapter {
        Adapter::Readiness => vec![
            "cass",
            "health",
            "--data-dir",
            data_dir,
            "--db",
            db,
            "--json",
        ],
        Adapter::DoctorTruth => vec![
            "cass",
            "doctor",
            "check",
            "--data-dir",
            data_dir,
            "--db",
            db,
            "--json",
        ],
        Adapter::SearchCoverage => vec![
            "cass",
            "status",
            "--data-dir",
            data_dir,
            "--db",
            db,
            "--json",
        ],
        Adapter::ResourcePlan => vec![
            "cass",
            "swarm",
            "resource-plan",
            "--action",
            "full-index",
            "--json",
        ],
        Adapter::PrivacyPreview => vec!["cass", "swarm", "privacy-preview", "--json"],
        Adapter::SupportEvidence => vec![
            "cass",
            "health",
            "--data-dir",
            data_dir,
            "--db",
            db,
            "--json",
        ],
        Adapter::KeyPolicy => vec!["cass-internal", "confirm-key-policy"],
        Adapter::SupportBundle => vec![
            "cass",
            "doctor",
            "support-bundle",
            "--data-dir",
            data_dir,
            "--db",
            db,
            "--json",
        ],
        Adapter::LexicalRepair => vec![
            "cass",
            "index",
            "--force-rebuild",
            "--data-dir",
            data_dir,
            "--db",
            db,
            "--no-progress-events",
            "--json",
        ],
        Adapter::SourcesDrySync => vec![
            "cass",
            "sources",
            "sync",
            "--dry-run",
            "--no-index",
            "--json",
        ],
        Adapter::IncrementalIndex => vec![
            "cass",
            "index",
            "--data-dir",
            data_dir,
            "--db",
            db,
            "--no-progress-events",
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
    if request.source_kind != "fixture" && command.adapter == Adapter::SourcesDrySync {
        return evaluate_sources_dry_sync(command, request, deadline);
    }
    if request.source_kind != "fixture"
        && matches!(
            command.adapter,
            Adapter::Readiness
                | Adapter::DoctorTruth
                | Adapter::SearchCoverage
                | Adapter::SupportEvidence
        )
    {
        return evaluate_live_proof(command, request.data_dir, request.db_path, deadline);
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
        Adapter::SearchCoverage
        | Adapter::SupportBundle
        | Adapter::LexicalRepair
        | Adapter::SourcesDrySync
        | Adapter::IncrementalIndex
        | Adapter::ObservationOnly => ProofResult {
            result: "not-run",
            observation: None,
            source: "none",
            detail: "no automatic read-only proof adapter".to_string(),
        },
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
    run_bounded_cass_with_scope(argv, timeout, false, None)
}

fn run_bounded_canonical_repair(
    argv: &[String],
    timeout: Duration,
) -> Result<Output, CommandFailure> {
    run_bounded_cass_with_scope(argv, timeout, true, None)
}

fn run_bounded_cass_with_scope(
    argv: &[String],
    timeout: Duration,
    require_canonical_rebuild: bool,
    source_data_dir: Option<&Path>,
) -> Result<Output, CommandFailure> {
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
    if let Some(data_dir) = source_data_dir {
        child.env("CASS_DATA_DIR", data_dir);
    }
    if require_canonical_rebuild {
        child.env("CASS_INDEX_REQUIRE_CANONICAL_REBUILD", "1");
    }
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

fn sources_fingerprint() -> Result<String, &'static str> {
    if dotenvy::var("CASS_IGNORE_SOURCES_CONFIG").is_ok() {
        return Err("source onboarding cannot run while CASS_IGNORE_SOURCES_CONFIG is set");
    }
    let config = crate::sources::config::SourcesConfig::load()
        .map_err(|_| "configured sources could not be loaded and validated")?;
    fingerprint_sources(&config)
}

fn fingerprint_sources(
    config: &crate::sources::config::SourcesConfig,
) -> Result<String, &'static str> {
    let bytes = serde_json::to_vec(config)
        .map_err(|_| "configured source scope could not be identified")?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn failed_sources_proof(detail: impl Into<String>) -> ProofResult {
    ProofResult {
        result: "failed",
        source: "source-config",
        detail: detail.into(),
        observation: None,
    }
}

/// A dry sync proves a locally valid plan, never remote reachability, transfer
/// completion, or registration. In particular its path successes must be null.
fn classify_sources_dry_sync(exit_code: Option<i32>, payload: &Value) -> ProofResult {
    if exit_code != Some(0)
        || payload.get("status").and_then(Value::as_str) != Some("complete")
        || payload.get("dry_run").and_then(Value::as_bool) != Some(true)
        || payload.get("will_reindex").and_then(Value::as_bool) != Some(false)
        || [
            "sources_attempted",
            "sources_with_failures",
            "sources_fully_failed",
            "total_files",
            "total_bytes",
        ]
        .into_iter()
        .any(|key| payload.get(key).and_then(Value::as_u64) != Some(0))
        || ["error", "err"]
            .into_iter()
            .any(|key| payload.get(key).is_some_and(|v| !v.is_null()))
    {
        return failed_sources_proof(
            "dry sync requires configured remote sources and a complete, non-mutating preview",
        );
    }
    let Some(sources) = payload
        .get("sources")
        .and_then(Value::as_array)
        .filter(|s| !s.is_empty())
    else {
        return failed_sources_proof(
            "configure a remote source with cass sources add before onboarding",
        );
    };
    let mut path_count = 0usize;
    for source in sources {
        let Some(paths) = source
            .get("paths")
            .and_then(Value::as_array)
            .filter(|p| !p.is_empty())
        else {
            return failed_sources_proof("configured source has no previewable paths");
        };
        if source.get("status").and_then(Value::as_str) != Some("dry_run")
            || source.get("method").and_then(Value::as_str) != Some("not_run")
            || paths.iter().any(|path| {
                !path.get("success").is_some_and(Value::is_null)
                    || !path.get("error").is_some_and(Value::is_null)
                    || !path.get("failure_reason").is_some_and(Value::is_null)
                    || path.get("files").and_then(Value::as_u64) != Some(0)
                    || path.get("bytes").and_then(Value::as_u64) != Some(0)
            })
        {
            return failed_sources_proof(
                "source preview contains invalid paths or an executed transport",
            );
        }
        path_count = path_count.saturating_add(paths.len());
    }
    ProofResult {
        result: "passed",
        source: "live-command",
        detail: "configured-source sync plan validated locally; no source registered, SSH attempted or files transferred".into(),
        observation: Some(json!({
            "configured_remote_sources": sources.len(),
            "configured_paths": path_count,
            "transport_started": false,
            "sources_registered": false,
            "files_transferred": 0,
            "reindexed": false
        })),
    }
}

fn evaluate_sources_dry_sync(
    command: AllowedCommand,
    request: &GuideRunRequest<'_>,
    deadline: Instant,
) -> ProofResult {
    let before = match sources_fingerprint() {
        Ok(value) => value,
        Err(detail) => return failed_sources_proof(detail),
    };
    let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
        return unavailable_live_proof("source preview deadline exhausted; no child started");
    };
    let Some(argv) = allowlisted_argv(command, request.data_dir, request.db_path) else {
        return failed_sources_proof(
            "configured source preview scope cannot be represented by safe argv",
        );
    };
    let output = match run_bounded_cass_with_scope(&argv, timeout, false, Some(request.data_dir)) {
        Ok(output) => output,
        Err(error) => return failed_sources_proof(error.detail),
    };
    let payload = match serde_json::from_slice(&output.stdout) {
        Ok(payload) => payload,
        Err(_) => {
            return failed_sources_proof(
                "dry sync returned malformed output; preview is unverified",
            );
        }
    };
    if sources_fingerprint().as_ref() != Ok(&before) {
        return failed_sources_proof("configured sources changed during preview; rerun the guide");
    }
    let mut proof = classify_sources_dry_sync(output.status.code(), &payload);
    if let Some(observation) = proof.observation.as_mut().and_then(Value::as_object_mut) {
        observation.insert("source_config_blake3".into(), json!(before));
    }
    proof
}

fn evaluate_live_proof(
    command: AllowedCommand,
    data_dir: &Path,
    db_path: Option<&Path>,
    deadline: Instant,
) -> ProofResult {
    let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
        return unavailable_live_proof("shared live proof deadline exhausted; no child started");
    };
    let Some(argv) = allowlisted_argv(command, data_dir, db_path) else {
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
    // Doctor's exit 5 also represents a completed check with a failed asset
    // check. That is evidence for classification, never a readiness pass or
    // permission to repair. Require its exact read-only contract; generic
    // exit-5 errors and mutating doctor reports still fail closed.
    let classified_doctor_failure = adapter == Adapter::DoctorTruth
        && exit_code == Some(5)
        && payload.get("schema_version").and_then(Value::as_u64) == Some(2)
        && payload
            .get("doctor_contract_version")
            .and_then(Value::as_u64)
            == Some(1)
        && payload.get("status").and_then(Value::as_str) == Some("unhealthy")
        && payload.get("healthy").and_then(Value::as_bool) == Some(false)
        && payload
            .get("failures")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0)
        && payload.get("issues_fixed").and_then(Value::as_u64) == Some(0)
        && payload.get("auto_fix_applied").and_then(Value::as_bool) == Some(false)
        && payload
            .pointer("/doctor_command/execution_mode")
            .and_then(Value::as_str)
            == Some("read-only-check")
        && payload
            .pointer("/doctor_command/read_only")
            .and_then(Value::as_bool)
            == Some(true)
        && payload
            .pointer("/doctor_command/mutation_allowed")
            .and_then(Value::as_bool)
            == Some(false)
        && payload
            .get("checks")
            .and_then(Value::as_array)
            .is_some_and(|checks| {
                checks
                    .iter()
                    .any(|check| check.get("status").and_then(Value::as_str) == Some("fail"))
            });
    if !matches!(exit_code, Some(0 | 1)) && !classified_doctor_failure {
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

/// A lexical repair has a narrower postcondition than overall archive health:
/// optional semantic assets and source-scan freshness remain separate concerns.
/// Check the actual checkpoint and corpus binding, never only `healthy=true`.
fn lexical_matches_canonical(payload: &Value) -> bool {
    [
        "/index/exists",
        "/index/checkpoint/completed",
        "/index/checkpoint/db_matches",
        "/index/checkpoint/schema_matches",
        "/index/checkpoint/page_size_compatible",
        "/index/fingerprint/matches_current_db_fingerprint",
    ]
    .into_iter()
    .all(|path| payload.pointer(path).and_then(Value::as_bool) == Some(true))
        && [
            "/index/rebuilding",
            "/index/engine_incompatible",
            "/index/hollow",
            "/index/partial",
        ]
        .into_iter()
        .all(|path| payload.pointer(path).and_then(Value::as_bool) == Some(false))
        && payload
            .pointer("/index/manifest_source")
            .and_then(Value::as_str)
            == Some("current")
}

#[derive(Debug, PartialEq, Eq)]
struct LexicalRepairSnapshot {
    conversations: u64,
    messages: u64,
    canonical_fingerprint: String,
    lexical_ready: bool,
    source_fresh: Option<bool>,
}

fn lexical_repair_snapshot(
    exit_code: Option<i32>,
    payload: &Value,
    db_path: &Path,
) -> Result<LexicalRepairSnapshot, &'static str> {
    if !matches!(exit_code, Some(0 | 1))
        || payload.get("error").is_some_and(|value| !value.is_null())
        || payload.get("err").is_some_and(|value| !value.is_null())
    {
        return Err("lexical repair status probe did not complete successfully");
    }
    if payload.pointer("/database/path").and_then(Value::as_str) != db_path.to_str()
        || payload.pointer("/database/exists").and_then(Value::as_bool) != Some(true)
        || payload.pointer("/database/opened").and_then(Value::as_bool) != Some(true)
        || payload
            .pointer("/database/open_skipped")
            .and_then(Value::as_bool)
            != Some(false)
        || payload
            .pointer("/database/counts_skipped")
            .and_then(Value::as_bool)
            != Some(false)
        || payload
            .pointer("/database/integrity")
            .and_then(Value::as_str)
            != Some("ok")
    {
        return Err(
            "lexical repair requires a readable, classified canonical archive in the selected scope",
        );
    }
    if payload.pointer("/rebuild/active").and_then(Value::as_bool) != Some(false)
        || payload
            .pointer("/pending/watch_active")
            .and_then(Value::as_bool)
            != Some(false)
    {
        return Err("lexical repair is deferred while another index or watch operation is active");
    }
    let conversations = payload
        .pointer("/database/conversations")
        .and_then(Value::as_u64)
        .filter(|count| *count > 0)
        .ok_or("lexical repair requires a populated canonical archive; initialize it with cass index --full")?;
    let messages = payload
        .pointer("/database/messages")
        .and_then(Value::as_u64)
        .ok_or("lexical repair cannot verify the canonical message count")?;
    let canonical_fingerprint = payload
        .pointer("/index/fingerprint/current_db_fingerprint")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 1024)
        .ok_or("lexical repair cannot bind the canonical archive identity")?
        .to_string();
    Ok(LexicalRepairSnapshot {
        conversations,
        messages,
        canonical_fingerprint,
        lexical_ready: lexical_matches_canonical(payload),
        source_fresh: payload.pointer("/index/fresh").and_then(Value::as_bool),
    })
}

fn read_lexical_repair_snapshot(
    argv: &[String],
    db_path: &Path,
) -> Result<LexicalRepairSnapshot, &'static str> {
    let output = run_bounded_cass(argv, Duration::from_secs(15)).map_err(|error| error.detail)?;
    let payload = serde_json::from_slice(&output.stdout)
        .map_err(|_| "lexical repair status probe did not return valid JSON")?;
    lexical_repair_snapshot(output.status.code(), &payload, db_path)
}

fn lexical_repair_receipt(snapshot: &LexicalRepairSnapshot) -> Value {
    json!({
        "scope": "canonical-lexical",
        "conversations": snapshot.conversations,
        "messages": snapshot.messages,
        "canonical_fingerprint_blake3": blake3::hash(snapshot.canonical_fingerprint.as_bytes()).to_hex().to_string(),
        "lexical_ready": snapshot.lexical_ready,
        "source_scan_fresh": snapshot.source_fresh,
        "sources_scanned": false,
        "semantic_rebuilt": false,
        "canonical_rebuild_required": true
    })
}

fn classify_lexical_rebuild(exit_code: Option<i32>, stdout: &[u8]) -> Result<(), &'static str> {
    if exit_code != Some(0) {
        return Err(
            "lexical rebuild did not complete; inspect cass status before retrying, as resumable staging may remain",
        );
    }
    let payload: Value = serde_json::from_slice(stdout)
        .map_err(|_| "lexical rebuild returned malformed output; completion is unverified")?;
    if payload.get("error").is_some_and(|value| !value.is_null())
        || payload.get("err").is_some_and(|value| !value.is_null())
        || payload.get("success").and_then(Value::as_bool) != Some(true)
        || payload.get("force_rebuild").and_then(Value::as_bool) != Some(true)
        || payload.get("full").and_then(Value::as_bool) != Some(false)
        || payload
            .get("lexical_update_deferred")
            .and_then(Value::as_bool)
            != Some(false)
        || payload
            .pointer("/indexing_stats/scan_ms")
            .and_then(Value::as_u64)
            != Some(0)
        || payload
            .pointer("/indexing_stats/lexical_strategy_reason")
            .and_then(Value::as_str)
            != Some("force_rebuild_uses_readonly_authoritative_canonical_db_rebuild_only")
    {
        return Err("lexical rebuild did not attest a completed canonical-only repair");
    }
    Ok(())
}

fn verify_lexical_repair(
    before: &LexicalRepairSnapshot,
    after: &LexicalRepairSnapshot,
) -> MutationResult {
    if before.canonical_fingerprint != after.canonical_fingerprint
        || before.conversations != after.conversations
        || before.messages != after.messages
    {
        return failed_mutation(
            true,
            "canonical archive changed during lexical repair; the selected postcondition is unverified",
        );
    }
    if !after.lexical_ready {
        return failed_mutation(
            true,
            "lexical rebuild returned success but the fresh status probe did not verify a complete matching generation",
        );
    }
    MutationResult {
        status: "executed",
        proof_result: "passed",
        detail: "canonical lexical index rebuilt and independently checked against the same archive; source freshness and optional semantic assets are reported separately".to_string(),
        started: true,
        observation: Some(lexical_repair_receipt(after)),
    }
}

fn execute_lexical_repair(data_dir: &Path, db_path: Option<&Path>) -> MutationResult {
    let default_db = data_dir.join("agent_search.db");
    let Ok(db_path) = db_path.unwrap_or(&default_db).canonicalize() else {
        return failed_mutation(
            false,
            "lexical repair requires an existing canonical archive",
        );
    };
    let Ok(data_dir) = data_dir.canonicalize() else {
        return failed_mutation(false, "lexical repair data directory is unavailable");
    };
    let Some(status_command) = allowed_command("diag.search-coverage") else {
        return failed_mutation(false, "lexical repair status adapter is unavailable");
    };
    let Some(status_argv) = allowlisted_argv(status_command, &data_dir, Some(&db_path)) else {
        return failed_mutation(
            false,
            "lexical repair scope cannot be represented by safe argv",
        );
    };
    let before = match read_lexical_repair_snapshot(&status_argv, &db_path) {
        Ok(snapshot) => snapshot,
        Err(detail) => return failed_mutation(false, detail),
    };
    if before.lexical_ready {
        return MutationResult {
            status: "already-ready",
            proof_result: "passed",
            detail: "canonical lexical assets already match the archive; no rebuild was started"
                .to_string(),
            started: false,
            observation: Some(lexical_repair_receipt(&before)),
        };
    }
    let Some(repair_command) = allowed_command("doctor.rebuild-stale-assets") else {
        return failed_mutation(false, "lexical repair adapter is unavailable");
    };
    let Some(repair_argv) = allowlisted_argv(repair_command, &data_dir, Some(&db_path)) else {
        return failed_mutation(
            false,
            "lexical repair scope cannot be represented by safe argv",
        );
    };
    let output = match run_bounded_canonical_repair(&repair_argv, Duration::from_secs(120)) {
        Ok(output) => output,
        Err(error) => return failed_mutation(error.started, error.detail),
    };
    if let Err(detail) = classify_lexical_rebuild(output.status.code(), &output.stdout) {
        return failed_mutation(true, detail);
    }
    match read_lexical_repair_snapshot(&status_argv, &db_path) {
        Ok(after) => verify_lexical_repair(&before, &after),
        Err(detail) => failed_mutation(true, detail),
    }
}

/// Indexing may have changed canonical data even when its lexical update was
/// deferred. Require its actual completion receipt and a fresh scope-bound
/// status probe before reporting that the onboarding operation was verified.
fn classify_incremental_index(
    exit_code: Option<i32>,
    payload: &Value,
    data_dir: &Path,
    db_path: &Path,
) -> Result<Value, &'static str> {
    if exit_code != Some(0)
        || payload.get("success").and_then(Value::as_bool) != Some(true)
        || payload.get("full").and_then(Value::as_bool) != Some(false)
        || payload.get("force_rebuild").and_then(Value::as_bool) != Some(false)
        || payload
            .get("lexical_update_deferred")
            .and_then(Value::as_bool)
            != Some(false)
        || payload
            .get("quarantined_conversations")
            .and_then(Value::as_u64)
            != Some(0)
        || payload
            .pointer("/indexing_stats/scan_had_errors")
            .and_then(Value::as_bool)
            != Some(false)
        || payload
            .pointer("/indexing_stats/deferred_active_sources")
            .and_then(Value::as_u64)
            != Some(0)
        || payload.get("data_dir").and_then(Value::as_str) != data_dir.to_str()
        || payload.get("db_path").and_then(Value::as_str) != db_path.to_str()
        || ["error", "err"]
            .into_iter()
            .any(|key| payload.get(key).is_some_and(|v| !v.is_null()))
    {
        return Err(
            "incremental indexing did not attest a complete, non-deferred scan in the selected scope",
        );
    }
    let conversations = payload
        .get("conversations")
        .and_then(Value::as_u64)
        .ok_or("incremental indexing did not report conversation observations")?;
    let messages = payload
        .get("messages")
        .and_then(Value::as_u64)
        .ok_or("incremental indexing did not report message observations")?;
    Ok(json!({
        "exit_code": exit_code,
        "index_observations": {
            "conversations": conversations,
            "messages": messages
        },
        "source_scan_completed": true,
        "lexical_update_deferred": false,
        "semantic_rebuilt": false,
        "transport_started": false
    }))
}

fn verify_incremental_index(
    mut observation: Value,
    after: &LexicalRepairSnapshot,
    source_fingerprint: &str,
) -> MutationResult {
    if !after.lexical_ready || after.source_fresh != Some(true) {
        return failed_mutation(
            true,
            "fresh status did not verify the indexed corpus and complete lexical generation",
        );
    }
    // The public index receipt can contain this run's observations instead
    // of archive totals. Only the fresh selected-archive probe supplies those
    // totals; equating the two rejects legitimate incremental and no-op runs.
    observation["conversations"] = json!(after.conversations);
    observation["messages"] = json!(after.messages);
    observation["source_config_blake3"] = json!(source_fingerprint);
    observation["canonical_fingerprint_blake3"] = json!(
        blake3::hash(after.canonical_fingerprint.as_bytes())
            .to_hex()
            .to_string()
    );
    observation["lexical_ready"] = json!(true);
    observation["source_scan_fresh"] = json!(true);
    MutationResult {
        status: "executed",
        proof_result: "passed",
        detail: "configured-source incremental indexing completed and fresh canonical/lexical status matched; no remote transfer or semantic build was performed".into(),
        started: true,
        observation: Some(observation),
    }
}

fn execute_incremental_index(
    data_dir: &Path,
    db_path: Option<&Path>,
    source_preview_fingerprint: Option<&str>,
) -> MutationResult {
    let Some(expected) = source_preview_fingerprint else {
        return failed_mutation(
            false,
            "incremental indexing requires a verified configured-source preview",
        );
    };
    if sources_fingerprint().as_deref() != Ok(expected) {
        return failed_mutation(
            false,
            "configured sources changed after preview; no indexing started",
        );
    }
    let data_dir = match data_dir
        .canonicalize()
        .or_else(|_| std::path::absolute(data_dir))
    {
        Ok(path) => path,
        Err(_) => return failed_mutation(false, "index data directory cannot be resolved"),
    };
    let config = match crate::sources::config::SourcesConfig::load() {
        Ok(config) => config,
        Err(_) => return failed_mutation(false, "configured sources could not be revalidated"),
    };
    if fingerprint_sources(&config).as_deref() != Ok(expected) {
        return failed_mutation(
            false,
            "configured source scope changed; no indexing started",
        );
    }
    for source in config.remote_sources() {
        for path in &source.paths {
            let mirror = data_dir
                .join("remotes")
                .join(&source.name)
                .join("mirror")
                .join(crate::sources::sync::path_to_safe_dirname(path));
            if std::fs::read_dir(&mirror).is_err() {
                return failed_mutation(
                    false,
                    "a configured source mirror is unavailable; run cass sources sync before indexing",
                );
            }
        }
    }
    let default_db = data_dir.join("agent_search.db");
    let requested_db = db_path.unwrap_or(&default_db);
    let db_path = match requested_db
        .canonicalize()
        .or_else(|_| std::path::absolute(requested_db))
    {
        Ok(path) => path,
        Err(_) => return failed_mutation(false, "selected index archive cannot be resolved"),
    };
    let Some(command) = allowed_command("index.incremental") else {
        return failed_mutation(false, "incremental indexing adapter is unavailable");
    };
    let Some(argv) = allowlisted_argv(command, &data_dir, Some(&db_path)) else {
        return failed_mutation(
            false,
            "incremental indexing scope cannot be represented by safe argv",
        );
    };
    let output = match run_bounded_cass(&argv, Duration::from_secs(120)) {
        Ok(output) => output,
        Err(error) => return failed_mutation(error.started, error.detail),
    };
    let payload = match serde_json::from_slice(&output.stdout) {
        Ok(payload) => payload,
        Err(_) => {
            return failed_mutation(
                true,
                "incremental indexing returned malformed output; state is unverified",
            );
        }
    };
    let observation = match classify_incremental_index(
        output.status.code(),
        &payload,
        &data_dir,
        &db_path,
    ) {
        Ok(observation) => observation,
        Err(detail) => {
            let mut failed = failed_mutation(true, detail);
            failed.observation = Some(json!({
                "exit_code": output.status.code(),
                "index_busy": output.status.code() == Some(7),
                "lexical_update_deferred": payload.get("lexical_update_deferred").and_then(Value::as_bool),
                "scan_had_errors": payload.pointer("/indexing_stats/scan_had_errors").and_then(Value::as_bool),
                "deferred_active_sources": payload.pointer("/indexing_stats/deferred_active_sources").and_then(Value::as_u64),
                "quarantined_conversations": payload.get("quarantined_conversations").and_then(Value::as_u64),
                "state_may_have_changed": true
            }));
            return failed;
        }
    };
    if sources_fingerprint().as_deref() != Ok(expected) {
        return failed_mutation(
            true,
            "configured sources changed during indexing; completion is unverified",
        );
    }
    let Some(status) = allowed_command("diag.search-coverage")
        .and_then(|command| allowlisted_argv(command, &data_dir, Some(&db_path)))
    else {
        return failed_mutation(true, "post-index status scope is unavailable");
    };
    let after = match read_lexical_repair_snapshot(&status, &db_path) {
        Ok(snapshot) => snapshot,
        Err(detail) => return failed_mutation(true, detail),
    };
    verify_incremental_index(observation, &after, expected)
}

fn execute_mutation(
    command: AllowedCommand,
    argv: &[String],
    data_dir: &Path,
    db_path: Option<&Path>,
    source_preview_fingerprint: Option<&str>,
) -> MutationResult {
    match command.adapter {
        Adapter::SupportBundle => execute_support_bundle(argv, data_dir),
        Adapter::LexicalRepair => execute_lexical_repair(data_dir, db_path),
        Adapter::IncrementalIndex => {
            execute_incremental_index(data_dir, db_path, source_preview_fingerprint)
        }
        _ => MutationResult {
            status: "adapter-unavailable",
            proof_result: "not-run",
            detail: "structured mutation has no closed, parameter-complete adapter".to_string(),
            started: false,
            observation: None,
        },
    }
}

fn incremental_index_scope() -> Value {
    json!({
        "assets": "canonical-and-lexical",
        "source_scope": "configured-sources-and-local-discovery",
        "source_scan": true,
        "source_registration": false,
        "remote_sync": false,
        "requires_staged_mirrors": true,
        "semantic_rebuild": false,
        "timeout_seconds": 120
    })
}

fn mutation_contract(apply: bool, attempted: usize, attempted_db_mutation: bool) -> Value {
    json!({
        "read_only": attempted == 0,
        "apply_mode": apply,
        "schedules_work": false,
        "mutates_files": attempted > 0,
        "mutates_db": attempted_db_mutation,
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
    let mut attempted_db_mutation = false;
    let mut awaiting_confirmation = false;
    let mut blocked_step = false;
    let mut source_preview_fingerprint = None;

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
        let argv = allowlisted_argv(command, request.data_dir, request.db_path);
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
            if command.adapter == Adapter::LexicalRepair
                && let Some(entry) = transcript.last_mut().and_then(Value::as_object_mut)
            {
                entry.insert(
                    "execution_scope".to_string(),
                    json!({
                        "assets": "lexical",
                        "canonical_rebuild_required": true,
                        "source_scan": false,
                        "semantic_rebuild": false,
                        "timeout_seconds": 120
                    }),
                );
            }
            if command.adapter == Adapter::IncrementalIndex
                && let Some(entry) = transcript.last_mut().and_then(Value::as_object_mut)
            {
                entry.insert("execution_scope".into(), incremental_index_scope());
            }
            if command.adapter == Adapter::SourcesDrySync
                && let Some(entry) = transcript.last_mut().and_then(Value::as_object_mut)
            {
                entry.insert(
                    "environment".into(),
                    json!({"CASS_DATA_DIR": request.data_dir}),
                );
            }
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
            if command.adapter == Adapter::SourcesDrySync && proof.result == "passed" {
                source_preview_fingerprint = proof
                    .observation
                    .as_ref()
                    .and_then(|value| value.get("source_config_blake3"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
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
            if command.adapter == Adapter::SourcesDrySync
                && let Some(entry) = transcript.last_mut().and_then(Value::as_object_mut)
            {
                entry.insert(
                    "environment".into(),
                    json!({"CASS_DATA_DIR": request.data_dir}),
                );
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
                request.db_path,
                source_preview_fingerprint.as_deref(),
            );
            mutation_started = mutation.started;
            if mutation_started {
                attempted_mutation_count += 1;
                attempted_db_mutation |= matches!(
                    command.adapter,
                    Adapter::LexicalRepair | Adapter::IncrementalIndex
                );
            }
            mutation_observation = mutation.observation;
            if mutation.status == "executed" {
                applied_mutation_count += 1;
            } else if mutation.status != "already-ready" {
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
        if command.adapter == Adapter::LexicalRepair
            && let Some(entry) = transcript.last_mut().and_then(Value::as_object_mut)
        {
            entry.insert(
                "execution_scope".to_string(),
                json!({
                    "assets": "lexical",
                    "canonical_rebuild_required": true,
                    "source_scan": false,
                    "semantic_rebuild": false,
                    "timeout_seconds": 120
                }),
            );
        }
        if request.source_kind != "fixture"
            && let Some(entry) = transcript.last_mut().and_then(Value::as_object_mut)
        {
            entry.insert("mutation_started".to_string(), json!(mutation_started));
            if let Some(observation) = mutation_observation {
                entry.insert("observation".to_string(), observation);
            }
        }
        if command.adapter == Adapter::IncrementalIndex
            && let Some(entry) = transcript.last_mut().and_then(Value::as_object_mut)
        {
            entry.insert("execution_scope".into(), incremental_index_scope());
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
            mutation_contract(
                request.apply,
                attempted_mutation_count,
                attempted_db_mutation,
            ),
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
            db_path: None,
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
        let Some(argv) = allowlisted_argv(command, Path::new("/tmp/cass-guide-test"), None) else {
            return Err("search.readiness did not produce argv".into());
        };
        if argv.first().map(String::as_str) != Some("cass")
            || argv.iter().any(|token| has_shell_metacharacter(token))
        {
            return Err("allowlisted argv was not safely tokenized".into());
        }
        Ok(())
    }

    fn dry_sync_receipt() -> Value {
        json!({
            "status": "complete", "dry_run": true, "will_reindex": false,
            "sources_attempted": 0, "sources_with_failures": 0, "sources_fully_failed": 0,
            "total_files": 0, "total_bytes": 0,
            "sources": [{"status": "dry_run", "method": "not_run", "paths": [{
                "success": null, "error": null, "failure_reason": null, "files": 0, "bytes": 0
            }]}]
        })
    }

    #[test]
    fn dry_sync_requires_an_unexecuted_valid_path_plan() {
        let valid = dry_sync_receipt();
        let proof = super::classify_sources_dry_sync(Some(0), &valid);
        assert_eq!(proof.result, "passed");
        assert_eq!(
            proof.observation.expect("measurement")["transport_started"],
            false
        );
        for (pointer, value) in [
            ("/status", json!("no_sources")),
            ("/dry_run", json!(false)),
            ("/will_reindex", json!(true)),
            ("/sources_attempted", json!(1)),
            ("/sources", json!([])),
            ("/sources/0/status", json!("invalid_config")),
            ("/sources/0/method", json!("rsync")),
            ("/sources/0/paths", json!([])),
            ("/sources/0/paths/0/success", json!(true)),
            ("/sources/0/paths/0/error", json!("unreadable")),
            ("/sources/0/paths/0/failure_reason", json!("invalid_config")),
            ("/sources/0/paths/0/files", json!(1)),
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).expect("receipt field") = value;
            assert_eq!(
                super::classify_sources_dry_sync(Some(0), &invalid).result,
                "failed",
                "{pointer}"
            );
        }
        for code in [None, Some(1), Some(7)] {
            assert_eq!(
                super::classify_sources_dry_sync(code, &valid).result,
                "failed"
            );
        }
        for incomplete in [Value::Null, json!({}), json!({"status": "complete"})] {
            assert_eq!(
                super::classify_sources_dry_sync(Some(0), &incomplete).result,
                "failed"
            );
        }
    }

    #[test]
    fn source_binding_detects_path_host_and_exclusion_changes() {
        use crate::sources::config::{SourceDefinition, SourcesConfig};
        let mut source = SourceDefinition::ssh("example", "fixture.invalid");
        source.paths = vec!["~/.codex".into()];
        let config = SourcesConfig {
            sources: vec![source],
            ..Default::default()
        };
        let before = super::fingerprint_sources(&config).expect("preview identity");
        // Bind semantic configuration, so harmless TOML formatting is not drift.
        let round_trip: SourcesConfig =
            toml::from_str(&toml::to_string(&config).expect("TOML")).expect("same scope");
        assert_eq!(
            super::fingerprint_sources(&round_trip).expect("same identity"),
            before
        );
        let mut changed = config.clone();
        changed.sources[0].paths.push("~/.claude".into());
        assert_ne!(
            super::fingerprint_sources(&changed).expect("path change"),
            before
        );
        let mut changed = config.clone();
        changed.sources[0].host = Some("different.invalid".into());
        assert_ne!(
            super::fingerprint_sources(&changed).expect("host change"),
            before
        );
        let mut changed = config;
        changed.disabled_agents.push("codex".into());
        assert_ne!(
            super::fingerprint_sources(&changed).expect("exclusion change"),
            before
        );
    }

    #[test]
    fn incremental_receipt_requires_complete_scanning_in_the_selected_scope() {
        let root = tempfile::tempdir().expect("scope");
        let data = root.path().join("data");
        let db = root.path().join("selected.db");
        let valid = json!({
            "success": true, "full": false, "force_rebuild": false,
            "lexical_update_deferred": false, "quarantined_conversations": 0,
            "indexing_stats": {"scan_had_errors": false, "deferred_active_sources": 0},
            "data_dir": data, "db_path": db, "conversations": 5, "messages": 12
        });
        let classify =
            |code, payload: &Value| super::classify_incremental_index(code, payload, &data, &db);
        let measured = classify(Some(0), &valid).expect("complete scan");
        assert_eq!(measured["index_observations"]["conversations"], 5);
        for (pointer, value) in [
            ("/success", json!(false)),
            ("/full", json!(true)),
            ("/force_rebuild", json!(true)),
            ("/lexical_update_deferred", json!(true)),
            ("/quarantined_conversations", json!(1)),
            ("/indexing_stats/scan_had_errors", json!(true)),
            ("/indexing_stats/deferred_active_sources", json!(1)),
            ("/data_dir", json!("different")),
            ("/db_path", json!("different.db")),
            ("/messages", Value::Null),
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).expect("receipt field") = value;
            assert!(classify(Some(0), &invalid).is_err(), "{pointer}");
        }
        for code in [None, Some(1), Some(7)] {
            assert!(classify(code, &valid).is_err());
        }
        assert!(classify(Some(0), &json!({"success": true})).is_err());
        let missing_preview = super::execute_incremental_index(&data, Some(&db), None);
        assert!(!missing_preview.started);
        assert_eq!(missing_preview.status, "failed");
        assert!(!db.exists());
        assert!(!data.exists());
    }

    #[test]
    fn incremental_postconditions_distinguish_run_observations_from_archive_totals() {
        let mut after = super::LexicalRepairSnapshot {
            conversations: 2,
            messages: 4,
            canonical_fingerprint: "selected-corpus".into(),
            lexical_ready: true,
            source_fresh: Some(true),
        };
        for indexed in [0, 1] {
            let observation = json!({"index_observations": {
                "conversations": indexed, "messages": indexed * 2
            }});
            let result = super::verify_incremental_index(observation, &after, "source-scope");
            assert_eq!(result.status, "executed");
            let receipt = result.observation.expect("verified counts");
            assert_eq!(receipt["conversations"], 2);
            assert_eq!(receipt["messages"], 4);
            assert_eq!(receipt["index_observations"]["conversations"], indexed);
        }
        let observation = json!({"index_observations": {"conversations": 1, "messages": 2}});
        after.lexical_ready = false;
        assert_eq!(
            super::verify_incremental_index(observation.clone(), &after, "scope").status,
            "failed"
        );
        after.lexical_ready = true;
        for fresh in [None, Some(false)] {
            after.source_fresh = fresh;
            assert_eq!(
                super::verify_incremental_index(observation.clone(), &after, "scope").status,
                "failed"
            );
        }
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
    fn doctor_failed_check_is_classification_only_when_its_read_only_contract_is_complete() {
        use super::{Adapter, classify_live_proof};
        let complete = json!({
            "schema_version": 2, "doctor_contract_version": 1,
            "status": "unhealthy", "healthy": false, "failures": 1,
            "issues_fixed": 0, "auto_fix_applied": false,
            "doctor_command": {"execution_mode": "read-only-check", "read_only": true, "mutation_allowed": false},
            "checks": [{"name": "index", "status": "fail"}]
        });
        assert_eq!(
            classify_live_proof(Adapter::DoctorTruth, Some(5), &complete).result,
            "passed"
        );
        for adapter in [
            Adapter::Readiness,
            Adapter::SearchCoverage,
            Adapter::SupportEvidence,
        ] {
            assert_ne!(
                classify_live_proof(adapter, Some(5), &complete).result,
                "passed"
            );
        }
        for (pointer, value) in [
            ("/doctor_contract_version", Value::Null),
            ("/status", json!("error")),
            ("/healthy", json!(true)),
            ("/failures", json!(0)),
            ("/issues_fixed", json!(1)),
            ("/auto_fix_applied", json!(true)),
            ("/doctor_command/execution_mode", json!("repair")),
            ("/doctor_command/read_only", json!(false)),
            ("/doctor_command/mutation_allowed", json!(true)),
            ("/checks", json!([])),
        ] {
            let mut invalid = complete.clone();
            *invalid.pointer_mut(pointer).expect("doctor field") = value;
            assert_ne!(
                classify_live_proof(Adapter::DoctorTruth, Some(5), &invalid).result,
                "passed",
                "{pointer}"
            );
        }
        let mut error = complete;
        error["error"] = json!({"message": "failure"});
        assert_ne!(
            classify_live_proof(Adapter::DoctorTruth, Some(5), &error).result,
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
                None,
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
        let contract = super::mutation_contract(true, usize::from(result.started), false);
        assert_eq!(contract["read_only"], false);
        assert_eq!(contract["mutates_files"], true);
        assert_eq!(contract["mutates_db"], false);
        let not_started = super::CommandFailure::before_start("spawn failure");
        let contract = super::mutation_contract(true, usize::from(not_started.started), false);
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

    fn lexical_status_fixture() -> Value {
        json!({
            "healthy": false,
            "database": {
                "path": "/archive/custom.db",
                "exists": true,
                "opened": true,
                "open_skipped": false,
                "counts_skipped": false,
                "integrity": "ok",
                "conversations": 2,
                "messages": 7
            },
            "rebuild": {"active": false},
            "pending": {"watch_active": false},
            "index": {
                "exists": true,
                "fresh": false,
                "rebuilding": false,
                "engine_incompatible": false,
                "hollow": false,
                "partial": false,
                "manifest_source": "current",
                "checkpoint": {
                    "completed": true,
                    "db_matches": true,
                    "schema_matches": true,
                    "page_size_compatible": true
                },
                "fingerprint": {
                    "current_db_fingerprint": "content-v1:2:2:7",
                    "matches_current_db_fingerprint": true
                }
            }
        })
    }

    #[test]
    fn lexical_repair_preflight_requires_exact_scope_readable_counts_and_no_writer() {
        let db = Path::new("/archive/custom.db");
        let payload = lexical_status_fixture();
        let snapshot =
            super::lexical_repair_snapshot(Some(0), &payload, db).expect("classified archive");
        assert!(snapshot.lexical_ready);
        assert_eq!(snapshot.source_fresh, Some(false));
        for (pointer, value) in [
            ("/database/path", json!("/other/archive.db")),
            ("/database/exists", json!(false)),
            ("/database/opened", json!(false)),
            ("/database/open_skipped", json!(true)),
            ("/database/counts_skipped", json!(true)),
            ("/database/integrity", json!("corrupt")),
            ("/database/integrity", json!("unknown")),
            ("/database/conversations", json!(0)),
            ("/database/messages", Value::Null),
            ("/rebuild/active", json!(true)),
            ("/pending/watch_active", json!(true)),
            ("/index/fingerprint/current_db_fingerprint", Value::Null),
        ] {
            let mut changed = payload.clone();
            *changed.pointer_mut(pointer).expect("fixture pointer") = value;
            assert!(
                super::lexical_repair_snapshot(Some(0), &changed, db).is_err(),
                "unsafe preflight accepted {pointer}"
            );
        }
        assert!(super::lexical_repair_snapshot(Some(7), &payload, db).is_err());
        assert!(super::lexical_repair_snapshot(None, &payload, db).is_err());
        let mut error = payload;
        error["err"] = json!({"message": "private diagnostic"});
        assert!(super::lexical_repair_snapshot(Some(0), &error, db).is_err());
    }

    #[test]
    fn lexical_repair_verifies_checkpoint_and_corpus_instead_of_overall_health() {
        let db = Path::new("/archive/custom.db");
        let ready = lexical_status_fixture();
        let mut missing = ready.clone();
        missing["index"]["exists"] = json!(false);
        let before = super::lexical_repair_snapshot(Some(0), &missing, db).unwrap();
        let after = super::lexical_repair_snapshot(Some(0), &ready, db).unwrap();
        let result = super::verify_lexical_repair(&before, &after);
        assert_eq!(result.status, "executed");
        assert!(result.started);
        let receipt = result.observation.unwrap();
        assert_eq!(receipt["lexical_ready"], true);
        assert_eq!(receipt["source_scan_fresh"], false);
        assert_eq!(receipt["sources_scanned"], false);
        assert_eq!(receipt["semantic_rebuilt"], false);
        assert!(!receipt.to_string().contains("/archive"));
        for pointer in [
            "/index/exists",
            "/index/checkpoint/completed",
            "/index/checkpoint/db_matches",
            "/index/checkpoint/schema_matches",
            "/index/checkpoint/page_size_compatible",
            "/index/fingerprint/matches_current_db_fingerprint",
        ] {
            let mut broken = ready.clone();
            broken["healthy"] = json!(true);
            *broken.pointer_mut(pointer).unwrap() = json!(false);
            let broken = super::lexical_repair_snapshot(Some(0), &broken, db).unwrap();
            assert_eq!(
                super::verify_lexical_repair(&before, &broken).status,
                "failed"
            );
        }
        for pointer in [
            "/index/rebuilding",
            "/index/engine_incompatible",
            "/index/hollow",
            "/index/partial",
        ] {
            let mut broken = ready.clone();
            *broken.pointer_mut(pointer).unwrap() = json!(true);
            let broken = super::lexical_repair_snapshot(Some(0), &broken, db).unwrap();
            assert_eq!(
                super::verify_lexical_repair(&before, &broken).status,
                "failed"
            );
        }
        for source in [
            Value::Null,
            json!("previous_after_corrupt_current"),
            json!("previous_after_missing_current"),
        ] {
            let mut broken = ready.clone();
            broken["index"]["manifest_source"] = source;
            let broken = super::lexical_repair_snapshot(Some(0), &broken, db).unwrap();
            assert_eq!(
                super::verify_lexical_repair(&before, &broken).status,
                "failed"
            );
        }
        for (pointer, value) in [
            ("/database/conversations", json!(3)),
            ("/database/messages", json!(8)),
            (
                "/index/fingerprint/current_db_fingerprint",
                json!("content-v1:2:4:7"),
            ),
        ] {
            let mut drifted = ready.clone();
            *drifted.pointer_mut(pointer).unwrap() = value;
            let drifted = super::lexical_repair_snapshot(Some(0), &drifted, db).unwrap();
            assert_eq!(
                super::verify_lexical_repair(&before, &drifted).status,
                "failed"
            );
        }
    }

    #[test]
    fn lexical_rebuild_exit_zero_alone_cannot_certify_repair() {
        let complete = json!({
            "success": true,
            "force_rebuild": true,
            "full": false,
            "lexical_update_deferred": false,
            "indexing_stats": {
                "scan_ms": 0,
                "lexical_strategy_reason": "force_rebuild_uses_readonly_authoritative_canonical_db_rebuild_only"
            }
        });
        let bytes = serde_json::to_vec(&complete).unwrap();
        assert!(super::classify_lexical_rebuild(Some(0), &bytes).is_ok());
        for exit in [Some(1), Some(7), Some(130), None] {
            assert!(super::classify_lexical_rebuild(exit, &bytes).is_err());
        }
        for (pointer, value) in [
            ("/success", json!(false)),
            ("/force_rebuild", json!(false)),
            ("/full", json!(true)),
            ("/lexical_update_deferred", json!(true)),
            ("/indexing_stats/scan_ms", json!(1)),
            (
                "/indexing_stats/lexical_strategy_reason",
                json!("source_scan"),
            ),
        ] {
            let mut changed = complete.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            assert!(
                super::classify_lexical_rebuild(Some(0), &serde_json::to_vec(&changed).unwrap())
                    .is_err()
            );
        }
        assert!(super::classify_lexical_rebuild(Some(0), b"{}").is_err());
        assert!(super::classify_lexical_rebuild(Some(0), b"not JSON").is_err());
        let mut contradictory = complete;
        contradictory["err"] = json!({"message": "failed after publishing"});
        assert!(
            super::classify_lexical_rebuild(Some(0), &serde_json::to_vec(&contradictory).unwrap())
                .is_err()
        );
    }

    #[test]
    fn lexical_repair_preview_binds_database_and_preserves_apply_gates() {
        let plan = json!({"intent": {"recognized": true}, "readiness": "ready",
            "plan": {"privacy_tier": "low", "cost_risk": {"risk_level": "low"},
                "stop_conditions": [], "steps": [{"order": 3,
                    "command": "doctor.rebuild-stale-assets", "mutates": true,
                    "proof_gate": "lexical-ready", "rch_rule": "none"}]}});
        let db = Path::new("/custom archive/canonical.db");
        let mut request = request(Path::new("/archive with spaces"));
        request.db_path = Some(db);
        let preview = render_execution(plan.clone(), &request);
        let row = &preview["execution"]["transcript"][0];
        assert_eq!(row["result"], "planned");
        assert_eq!(row["execution_scope"]["source_scan"], false);
        assert_eq!(row["execution_scope"]["canonical_rebuild_required"], true);
        let argv = row["argv"].as_array().unwrap();
        assert!(
            argv.windows(2)
                .any(|pair| pair == [json!("--db"), json!("/custom archive/canonical.db")])
        );
        assert!(
            !argv
                .iter()
                .any(|arg| arg == "--full" || arg == "--semantic")
        );
        request.apply = true;
        let unconfirmed = render_execution(plan.clone(), &request);
        assert_eq!(
            unconfirmed["execution"]["transcript"][0]["result"],
            "awaiting-confirmation"
        );
        request.confirmed_steps = &[3];
        let fixture = render_execution(plan, &request);
        assert_eq!(
            fixture["execution"]["transcript"][0]["result"],
            "fixture-protected"
        );
        assert_eq!(fixture["execution"]["attempted_mutation_count"], 0);
        assert_eq!(fixture["mutation_contract"]["read_only"], true);
        let contract = super::mutation_contract(true, 1, true);
        assert_eq!(contract["mutates_db"], true);
        assert_eq!(contract["mutates_files"], true);
        assert_eq!(contract["touches_network"], false);
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
