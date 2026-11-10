//! Real-binary gate for the hardened model-acquisition contract surfaced on
//! `cass models status` / `cass models verify` (bead
//! `coding_agent_session_search-0qxwg`, follow-on to
//! `cass-fleet-resilience-20260608-uojcg.5.5`).
//!
//! The pure contract (src/search/model_acquisition.rs) is now projected into
//! both robot surfaces as a `model_acquisition` block. These tests drive the
//! test-built `cass` against a fully isolated HOME through the bounded shared
//! E2E runner and check, end
//! to end:
//!   1. status + verify emit the report fields (state / runtime / source /
//!      cost_class / skipped_network_reason / next_command / model identity),
//!      and stay lexical-fail-open;
//!   2. neither status nor verify auto-acquires a model — the typed
//!      `skipped_network_reason` is present (the README "cass never
//!      auto-downloads" contract, made machine-checkable) AND no model file
//!      lands on disk;
//!   3. a run emits a redaction-safe `.12.3`-schema proof-log artifact carrying
//!      the model identity, elapsed_ms, the skipped-network reason, and the
//!      artifact paths.
//!
//! These report/cache assertions do not independently count network attempts.
//!
//! Tests return diagnostic errors and retain actual runner artifacts. Scanner
//! results are established by running UBS; this module makes no clean-scan claim.

use assert_cmd::cargo::cargo_bin;
use coding_agent_search::e2e_runner::{
    ArtifactRunSpec, RunEvent, RunExpectation, RunMode, RunSpec, ScenarioArtifactEntry,
    ScenarioArtifactManifest, run_with_artifacts,
};
use coding_agent_search::proof_artifact::{ProofArtifact, ProofStatus};
use coding_agent_search::search::policy::MAX_MODEL_SIZE_MB;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Run the real `cass` binary with a private cache, bounded execution and
/// retained runner artifacts. Returns `(entry, stdout, stderr)` so callers can
/// preserve the actual execution facts when emitting another proof format.
fn run_isolated_cass(
    home: &std::path::Path,
    args: &[&str],
) -> Result<(ScenarioArtifactEntry, String, String), String> {
    let phase = args
        .get(1)
        .copied()
        .ok_or_else(|| "model command is missing its subcommand".to_string())?;
    let is_status = matches!(phase, "status");
    let env_overrides = BTreeMap::from([
        ("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT".into(), "1".into()),
        ("CASS_IGNORE_SOURCES_CONFIG".into(), "1".into()),
        (
            "CASS_DATA_DIR".into(),
            home.join("cass-data").display().to_string(),
        ),
        ("FRANKENSEARCH_MODEL_DIR".into(), String::new()),
        ("CASS_SEMANTIC_MODE".into(), "hybrid_preferred".into()),
        ("CASS_SEMANTIC_DOWNLOAD_POLICY".into(), "opt_in".into()),
        ("CASS_SEMANTIC_EMBEDDER".into(), "minilm".into()),
        (
            "CASS_SEMANTIC_MAX_MODEL_SIZE_MB".into(),
            MAX_MODEL_SIZE_MB.to_string(),
        ),
        ("HOME".into(), home.display().to_string()),
        (
            "XDG_DATA_HOME".into(),
            home.join(".local/share").display().to_string(),
        ),
        (
            "XDG_CONFIG_HOME".into(),
            home.join(".config").display().to_string(),
        ),
        (
            "XDG_CACHE_HOME".into(),
            home.join(".cache").display().to_string(),
        ),
    ]);
    let spec = RunSpec {
        binary_path: cargo_bin("cass").display().to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        timeout: Duration::from_secs(30),
        env_overrides,
        cwd: home.to_path_buf(),
        fixture_id: Some("isolated-empty-model-cache".into()),
        require_path: Some(home.to_path_buf()),
        phase: format!("models-{phase}"),
        mode: RunMode::Quick,
    };
    let expectation = RunExpectation {
        expect_json: true,
        assertions: vec![
            (
                "empty-cache-acquisition-report".into(),
                Box::new(move |stdout, _stderr| {
                    serde_json::from_str::<Value>(stdout)
                        .map_err(|error| error.to_string())
                        .and_then(|payload| {
                            if is_status {
                                minilm_acquisition(&payload)
                            } else {
                                payload.get("model_acquisition").cloned().ok_or_else(|| {
                                    "verify JSON missing model_acquisition".to_string()
                                })
                            }
                        })
                        .and_then(|acquisition| {
                            check_no_auto_download(&acquisition)?;
                            check_model_identity(&acquisition).map(|_| ())
                        })
                        .is_ok()
                }),
            ),
            (
                "private-model-cache-remains-absent".into(),
                Box::new(move |stdout, _stderr| {
                    serde_json::from_str::<Value>(stdout)
                        .map_err(|error| error.to_string())
                        .and_then(|payload| {
                            if is_status {
                                minilm_model_dir(&payload, home)
                            } else {
                                payload
                                    .get("model_dir")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                                    .ok_or_else(|| "verify JSON missing model_dir".to_string())
                            }
                        })
                        .is_ok_and(|model_dir| {
                            let path = std::path::Path::new(&model_dir);
                            path.starts_with(home.join("cass-data")) && !path.exists()
                        })
                }),
            ),
        ],
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("runner clock: {error}"))?
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let artifact_root = home.join("runner-artifacts");
    let entry = run_with_artifacts(
        &spec,
        &expectation,
        &ArtifactRunSpec {
            artifact_root: &artifact_root,
            run_id: "model-acquisition-no-download",
            scenario_id: "empty-cache",
            command_id: phase,
            issue_ids: &["coding_agent_session_search-0qxwg"],
            privacy_note: "Synthetic private model cache; no archived conversation content.",
            suite_started_ms: now_ms,
            accepted_exit_codes: &[0],
            forbidden_stream_markers: &[],
            extra_artifact_paths: &[],
        },
        now_ms,
    )
    .map_err(|error| format!("bounded cass {args:?}: {error}"))?;
    let stdout = std::fs::read_to_string(&entry.stdout_path)
        .map_err(|error| format!("read runner stdout: {error}"))?;
    let stderr = std::fs::read_to_string(&entry.stderr_path)
        .map_err(|error| format!("read runner stderr: {error}"))?;
    if !entry.outcome.is_success()
        || !matches!(entry.proof_status, ProofStatus::Pass)
        || !entry.assertions_ran
        || entry.generated_only
        || !entry.redaction_safe
        || !entry.fresh_for_suite
    {
        return Err(format!(
            "bounded cass {args:?} failed: outcome={:?}, proof={:?}, event={}; stdout: {stdout}; stderr: {stderr}",
            entry.outcome, entry.proof_status, entry.event_path
        ));
    }
    Ok((entry, stdout, stderr))
}

/// Extract `models[registry_name=="minilm"].model_acquisition` from a
/// `cass models status --json` payload.
fn minilm_acquisition(status: &Value) -> Result<Value, String> {
    status
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models.iter().find(|m| {
                matches!(
                    m.get("registry_name").and_then(Value::as_str),
                    Some("minilm")
                )
            })
        })
        .and_then(|m| m.get("model_acquisition").cloned())
        .ok_or_else(|| format!("status JSON missing models[minilm].model_acquisition: {status}"))
}

/// `models[registry_name=="minilm"].model_dir` from a status payload.
fn minilm_model_dir(status: &Value, home: &std::path::Path) -> Result<String, String> {
    let model_dir = status
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models.iter().find(|m| {
                matches!(
                    m.get("registry_name").and_then(Value::as_str),
                    Some("minilm")
                )
            })
        })
        .and_then(|m| m.get("model_dir").and_then(Value::as_str))
        .map(str::to_string)
        .ok_or_else(|| format!("status JSON missing models[minilm].model_dir: {status}"))?;
    if !std::path::Path::new(&model_dir).starts_with(home.join("cass-data")) {
        return Err(format!(
            "model cache must stay under the isolated data directory: {model_dir}"
        ));
    }
    Ok(model_dir)
}

/// Assert that an acquisition block for an *empty* cache proves cass did not
/// auto-download. On a semantic-enabled, AVX2-capable host (the default build)
/// the model classifies `absent` with the typed `explicit_install_required`
/// reason and lexical fail-open. On a `-baseline`/non-AVX2 host the runtime is a
/// hard block, which legitimately has no skipped-download story — still a
/// no-download outcome, accepted.
fn check_no_auto_download(acq: &Value) -> Result<(), String> {
    let runtime = acq
        .get("runtime")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let state = acq.get("state").and_then(Value::as_str).unwrap_or_default();
    let skipped = acq.get("skipped_network_reason").and_then(Value::as_str);

    if matches!(runtime, "baseline_no_semantic" | "incompatible_cpu") {
        if skipped.is_some() {
            return Err(format!(
                "host-block runtime {runtime} must not carry a skipped_network_reason; acq: {acq}"
            ));
        }
        return Ok(());
    }

    if !matches!(state, "absent") {
        return Err(format!(
            "empty cache on a capable host must classify as absent; got state={state}; acq: {acq}"
        ));
    }
    if !matches!(skipped, Some("explicit_install_required")) {
        return Err(format!(
            "an absent model must prove no auto-download via \
             skipped_network_reason=explicit_install_required; got {skipped:?}; acq: {acq}"
        ));
    }
    if !matches!(
        acq.get("fallback_mode").and_then(Value::as_str),
        Some("lexical")
    ) {
        return Err(format!(
            "an absent semantic model must fail open to lexical; acq: {acq}"
        ));
    }
    // Model-name-agnostic: status reports `minilm`, verify reports
    // `all-minilm-l6-v2`; both must point at the explicit install command.
    let next_cmd = acq
        .get("next_command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let parsed =
        <coding_agent_search::Cli as clap::Parser>::try_parse_from(next_cmd.split_whitespace())
            .map_err(|error| {
                format!("next_command is not accepted by the CLI: {error}; acq: {acq}")
            })?;
    if !matches!(
        parsed.command,
        Some(coding_agent_search::Commands::Models(coding_agent_search::ModelsCommand::Install {
            model,
            from_file: None,
            mirror: None,
            ..
        })) if matches!(model.as_str(), "minilm" | "all-minilm-l6-v2")
    ) {
        return Err(format!(
            "an absent model's next_command must be the explicit install; got {next_cmd:?}; acq: {acq}"
        ));
    }
    Ok(())
}

/// Assert the acquisition block carries a deterministic model identity
/// (`<revision>:<digest>`); returns the marker token for proof-log enrichment.
fn check_model_identity(acq: &Value) -> Result<String, String> {
    let fp = acq
        .get("fingerprint")
        .ok_or_else(|| format!("acquisition block missing fingerprint: {acq}"))?;
    let revision = fp
        .get("revision")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("fingerprint missing revision: {fp}"))?;
    let token = fp
        .get("marker_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("fingerprint missing marker_token: {fp}"))?;
    if !token.starts_with(&format!("{revision}:")) {
        return Err(format!(
            "marker_token must be <revision>:<digest>; got {token}"
        ));
    }
    Ok(token.to_string())
}

#[test]
fn models_install_dry_run_reports_a_plan_without_acquiring_files() -> Result<(), String> {
    use coding_agent_search::search::model_download::ModelManifest;

    let manifest = ModelManifest::minilm_v2();
    for (source_kind, extra) in [
        ("registry", Vec::<&str>::new()),
        (
            "mirror",
            vec!["--mirror", "https://mirror.example.invalid/models"],
        ),
        (
            "from_file",
            vec!["--from-file", "unavailable-airgap-source"],
        ),
    ] {
        let tmp = tempfile::tempdir().map_err(|error| format!("tempdir: {error}"))?;
        let mut args = vec![
            "models",
            "install",
            "--model",
            "minilm",
            "--dry-run",
            "--json",
        ];
        args.extend(extra);
        let (_, stdout, _) = run_isolated_cass(tmp.path(), &args)?;
        let plan: Value = serde_json::from_str(&stdout)
            .map_err(|error| format!("parse installation preview: {error}"))?;
        if !matches!(plan.get("dry_run").and_then(Value::as_bool), Some(true))
            || !matches!(plan.get("status").and_then(Value::as_str), Some("planned"))
            || plan.get("source_kind").and_then(Value::as_str) != Some(source_kind)
            || plan.get("manifest_bytes").and_then(Value::as_u64) != Some(manifest.total_size())
        {
            return Err(format!("incorrect {source_kind} installation plan: {plan}"));
        }
        let expected_download_bytes = if matches!(source_kind, "from_file") {
            0
        } else {
            manifest.total_size()
        };
        let expected_action = if matches!(source_kind, "from_file") {
            "install_from_file"
        } else {
            "download_and_install"
        };
        if plan.get("action").and_then(Value::as_str) != Some(expected_action) {
            return Err(format!(
                "incorrect {source_kind} installation action: {plan}"
            ));
        }
        if plan.get("expected_download_bytes").and_then(Value::as_u64)
            != Some(expected_download_bytes)
        {
            return Err(format!(
                "incorrect {source_kind} expected download size: {plan}"
            ));
        }
        let model_dir = plan
            .get("model_dir")
            .and_then(Value::as_str)
            .ok_or("installation preview is missing model_dir")?;
        let files = plan
            .get("files")
            .and_then(Value::as_array)
            .ok_or("installation preview is missing manifest files")?;
        if files.len() != manifest.files.len() {
            return Err(format!(
                "installation preview omitted manifest files: {plan}"
            ));
        }
        for (file, expected) in files.iter().zip(&manifest.files) {
            let destination = std::path::Path::new(model_dir).join(expected.local_name());
            if file.get("local_name").and_then(Value::as_str) != Some(expected.local_name())
                || file.get("size_bytes").and_then(Value::as_u64) != Some(expected.size)
                || file.get("expected_sha256").and_then(Value::as_str)
                    != Some(expected.sha256.as_str())
                || file.get("destination").and_then(Value::as_str) != destination.to_str()
            {
                return Err(format!("incorrect installation file plan: {file}"));
            }
        }
        if tmp.path().join("cass-data").exists() {
            return Err("installation preview created its destination directory".to_string());
        }
    }
    Ok(())
}

#[test]
fn status_and_verify_emit_acquisition_block_and_prove_no_auto_download() -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;

    // Pin the data directory and policy and neutralize the runtime model-dir
    // override: inherited environment or .env values must not redirect or
    // disable this empty cache. Explicit values also prevent dotenv refills.
    let (entry, stdout, stderr) = run_isolated_cass(tmp.path(), &["models", "status", "--json"])?;
    let code = entry.actual_exit_code;
    if !matches!(code, Some(0)) {
        return Err(format!(
            "models status --json exit {code:?}; stderr: {stderr}"
        ));
    }
    let status: Value = serde_json::from_str(&stdout)
        .map_err(|e| format!("models status stdout is not JSON: {e}; stdout: {stdout}"))?;

    let acq = minilm_acquisition(&status)?;
    check_no_auto_download(&acq)?;
    check_model_identity(&acq)?;

    // The active embedder is mirrored to the top level — its block is present
    // too, so a consumer reading the flattened shape sees the contract.
    if status.get("model_acquisition").is_none() {
        return Err(format!(
            "status must surface a top-level active model_acquisition block: {status}"
        ));
    }

    // No model file may have appeared — the filesystem proof that status did
    // not download anything.
    let model_dir = minilm_model_dir(&status, tmp.path())?;
    if std::path::Path::new(&model_dir)
        .join("model.safetensors")
        .exists()
    {
        return Err(format!(
            "status must not download model.safetensors; one appeared under {model_dir}"
        ));
    }

    // --- cass models verify --json (same isolated data directory and policy) ---
    let (verify_entry, vstdout, vstderr) =
        run_isolated_cass(tmp.path(), &["models", "verify", "--json"])?;
    let vcode = verify_entry.actual_exit_code;
    if !matches!(vcode, Some(0)) {
        return Err(format!(
            "models verify --json exit {vcode:?}; stderr: {vstderr}"
        ));
    }
    let verify: Value = serde_json::from_str(&vstdout)
        .map_err(|e| format!("models verify stdout is not JSON: {e}; stdout: {vstdout}"))?;
    let verify_dir = verify
        .get("model_dir")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("verify --json must expose its model directory: {verify}"))?;
    if !std::path::Path::new(verify_dir).starts_with(tmp.path().join("cass-data")) {
        return Err(format!(
            "verify must stay under the isolated data directory: {verify_dir}"
        ));
    }
    let vacq = verify
        .get("model_acquisition")
        .cloned()
        .ok_or_else(|| format!("verify --json must emit model_acquisition: {verify}"))?;
    check_no_auto_download(&vacq)?;
    check_model_identity(&vacq)?;
    // The new block joins — it does not replace — the legacy cache_lifecycle.
    if verify.get("cache_lifecycle").is_none() {
        return Err(format!(
            "verify must keep cache_lifecycle alongside model_acquisition: {verify}"
        ));
    }
    Ok(())
}

/// Secret-bearing env key fragments that must never appear in a retained proof
/// log's `sanitized_env` (mirrors src/search/proof_log.rs SECRET markers).
const PROOF_SECRET_MARKERS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "APIKEY",
    "CREDENTIAL",
    "PRIVATE_KEY",
    "SESSION",
];

#[test]
fn no_download_proof_emits_redaction_safe_proof_log_artifact() -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let artifact_dir = tmp.path().join("proof");
    std::fs::create_dir_all(&artifact_dir).map_err(|e| format!("create artifact dir: {e}"))?;

    // The helper pins the model cache and semantic policy to this fixture.
    let (entry, stdout, stderr) = run_isolated_cass(tmp.path(), &["models", "status", "--json"])?;
    let code = entry.actual_exit_code;
    if !matches!(code, Some(0)) {
        return Err(format!(
            "models status --json exit {code:?}; stderr: {stderr}"
        ));
    }
    let status: Value = serde_json::from_str(&stdout)
        .map_err(|e| format!("models status stdout is not JSON: {e}; stdout: {stdout}"))?;
    let acq = minilm_acquisition(&status)?;
    check_no_auto_download(&acq)?;
    let identity = check_model_identity(&acq)?;
    let skipped_reason = acq
        .get("skipped_network_reason")
        .and_then(Value::as_str)
        .unwrap_or("host_blocked")
        .to_string();
    let model_dir = minilm_model_dir(&status, tmp.path())?;
    let event: RunEvent = serde_json::from_slice(
        &std::fs::read(&entry.event_path).map_err(|e| format!("read runner event: {e}"))?,
    )
    .map_err(|e| format!("parse runner event: {e}"))?;
    let proof: ProofArtifact = serde_json::from_slice(
        &std::fs::read(&entry.proof_path).map_err(|e| format!("read runner proof: {e}"))?,
    )
    .map_err(|e| format!("parse runner proof: {e}"))?;
    let binary_version = event
        .binary_version
        .as_deref()
        .ok_or("runner event is missing the observed binary version")?;

    // Persist the real stdout/stderr as redaction-safe artifacts under a temp
    // dir (no $HOME leakage), then emit one `.12.3`-schema proof-log record.
    let stdout_path = artifact_dir.join("models_status.stdout.json");
    let stderr_path = artifact_dir.join("models_status.stderr.log");
    std::fs::write(&stdout_path, &stdout).map_err(|e| format!("write stdout artifact: {e}"))?;
    std::fs::write(&stderr_path, &stderr).map_err(|e| format!("write stderr artifact: {e}"))?;
    let stdout_path_str = stdout_path
        .to_str()
        .ok_or("stdout artifact path not UTF-8")?;
    let stderr_path_str = stderr_path
        .to_str()
        .ok_or("stderr artifact path not UTF-8")?;

    let outcome = if matches!(code, Some(0)) {
        "passed"
    } else {
        "failed"
    };
    let record = json!({
        "run_id": "models-no-download-proof",
        "scenario_id": "model_acquisition_no_auto_download",
        "issue_ids_covered": ["coding_agent_session_search-0qxwg"],
        "command_id": "models_status_json",
        "phase": "verify",
        "started_at_ms": event.start_ms,
        "finished_at_ms": event.end_ms,
        "elapsed_ms": event.elapsed_ms,
        "meta": {
            "cass_binary_path": event.binary_path,
            "cass_version": binary_version,
            "cargo_profile": "test",
            "target_dir": std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string()),
            "data_dir": tmp.path().join("cass-data").display().to_string(),
            "config_dir": tmp.path().join(".config").display().to_string(),
            "model_dir": model_dir,
        },
        "execution": {
            "argv": event.command_line,
            "sanitized_env": event.env_overrides,
            "timeout_ms": proof.run.timeout_ms,
            "exit_code": event.exit_code,
            "signal": event.signal,
            "timed_out": event.timed_out,
            "retry_count": 0,
        },
        "artifacts": {
            "stdout_path": stdout_path_str,
            "stderr_path": stderr_path_str,
            "robot_contract_ok": true,
            "ansi_free_stdout_ok": !stdout.contains('\u{1b}'),
        },
        // Bead-named enrichments carried alongside the .12.3 record.
        "model_fingerprint": identity,
        "skipped_network_reason": skipped_reason,
        "outcome": outcome,
    });

    let log_path = artifact_dir.join("acquisition-proof.jsonl");
    let line =
        serde_json::to_string(&record).map_err(|e| format!("serialize proof record: {e}"))?;
    std::fs::write(&log_path, format!("{line}\n")).map_err(|e| format!("write proof log: {e}"))?;

    // Read it back and prove the wire form satisfies the .12.3 contract.
    let jsonl = std::fs::read_to_string(&log_path)
        .map_err(|e| format!("read proof log {}: {e}", log_path.display()))?;
    let parsed: Value = serde_json::from_str(jsonl.trim())
        .map_err(|e| format!("proof-log line is not JSON: {e}; line: {jsonl}"))?;

    let required = [
        "run_id",
        "scenario_id",
        "command_id",
        "phase",
        "elapsed_ms",
        "meta",
        "execution",
        "artifacts",
        "outcome",
    ];
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|k| parsed.get(*k).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            ".12.3 proof record missing required fields {missing:?}: {parsed}"
        ));
    }

    if !matches!(
        parsed.get("outcome").and_then(Value::as_str),
        Some("passed")
    ) {
        return Err(format!(
            "a clean no-download run must record outcome=passed: {parsed}"
        ));
    }
    if parsed
        .get("model_fingerprint")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(format!(
            "proof record must carry the model identity: {parsed}"
        ));
    }
    if parsed
        .get("skipped_network_reason")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(format!(
            "proof record must carry the skipped_network_reason: {parsed}"
        ));
    }
    if parsed.get("elapsed_ms").and_then(Value::as_i64).is_none() {
        return Err(format!(
            "proof record must carry numeric elapsed_ms: {parsed}"
        ));
    }

    // Feed the serialized log's timestamps through the real runner consumer.
    // A fabricated epoch-zero record must fail against its actual event and
    // this run's suite boundary, even when its JSON shape looks valid.
    let mut recorded_entry = entry.clone();
    recorded_entry.started_at_ms = parsed
        .get("started_at_ms")
        .and_then(Value::as_u64)
        .ok_or("proof record is missing its actual start timestamp")?;
    recorded_entry.finished_at_ms = parsed
        .get("finished_at_ms")
        .and_then(Value::as_u64)
        .ok_or("proof record is missing its actual finish timestamp")?;
    let mut manifest = ScenarioArtifactManifest::new(
        entry.run_id.clone(),
        RunMode::Quick,
        entry.started_at_ms,
        vec![entry.scenario_id.clone()],
        vec![format!("{}/{}", entry.scenario_id, entry.command_id)],
        1,
    );
    manifest.record(recorded_entry);
    let errors = manifest.validation_errors();
    if !errors.is_empty() {
        return Err(format!(
            "proof log does not describe its actual run: {errors:?}"
        ));
    }
    if !matches!(
        parsed
            .get("execution")
            .and_then(|e| e.get("timeout_ms"))
            .and_then(Value::as_u64),
        Some(30_000)
    ) {
        return Err(format!(
            "proof log must preserve the actual 30s command limit: {parsed}"
        ));
    }

    // Redaction guard: no sanitized_env key may carry a secret marker.
    let env_obj = parsed
        .get("execution")
        .and_then(|e| e.get("sanitized_env"))
        .and_then(Value::as_object)
        .ok_or_else(|| format!("proof record missing execution.sanitized_env object: {parsed}"))?;
    let leaked: Vec<String> = env_obj
        .keys()
        .filter(|k| {
            let up = k.to_ascii_uppercase();
            PROOF_SECRET_MARKERS.iter().any(|m| up.contains(m))
        })
        .cloned()
        .collect();
    if !leaked.is_empty() {
        return Err(format!(
            "proof log retained secret-bearing env keys: {leaked:?}"
        ));
    }

    // Artifact paths must stay inside the temp artifact dir (no home leakage).
    let artifact_root = artifact_dir.to_str().ok_or("artifact dir path not UTF-8")?;
    let unsafe_paths: Vec<&str> = ["stdout_path", "stderr_path"]
        .into_iter()
        .filter(|key| {
            !parsed
                .get("artifacts")
                .and_then(|a| a.get(*key))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .starts_with(artifact_root)
        })
        .collect();
    if !unsafe_paths.is_empty() {
        return Err(format!(
            "artifact paths must be redaction-safe temp paths; offending keys: {unsafe_paths:?}"
        ));
    }
    Ok(())
}
