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
//!   2. status and verify report the explicit-install decision through
//!      `skipped_network_reason`, and no model file lands on disk;
//!   3. a run emits a redaction-safe `.12.3`-schema proof-log artifact carrying
//!      the model identity, elapsed_ms, the skipped-network reason, and the
//!      artifact paths.
//!
//! The report/cache assertions alone do not count network attempts. The Linux
//! syscall-observer case below traces the real process tree and calibrates its
//! detector with an explicit install attempt against a closed loopback port.
//!
//! Tests return diagnostic errors and retain actual runner artifacts. Scanner
//! results are established by running UBS; this module makes no clean-scan claim.

use assert_cmd::cargo::cargo_bin;
use coding_agent_search::e2e_runner::{
    ArtifactRunSpec, RunEvent, RunExpectation, RunMode, RunOutcome, RunSpec, ScenarioArtifactEntry,
    ScenarioArtifactManifest, run_with_artifacts,
};
use coding_agent_search::proof_artifact::{ProofArtifact, ProofStatus};
use coding_agent_search::search::policy::MAX_MODEL_SIZE_MB;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Keep the private HOME and actual runner evidence after success or failure.
/// Use the OS temporary root so dotenv cannot find the repository's .env by
/// walking up from the isolated command's working directory.
fn isolated_home_with_retained_artifacts() -> Result<tempfile::TempDir, String> {
    let home = tempfile::Builder::new()
        .prefix("cass-model-acquisition-")
        .disable_cleanup(true)
        .tempdir()
        .map_err(|error| format!("create isolated model HOME: {error}"))?;
    eprintln!("Model acquisition artifacts: {}", home.path().display());
    Ok(home)
}

#[test]
fn unexecuted_json_proof_events_never_claim_parse_success() -> Result<(), String> {
    let home = isolated_home_with_retained_artifacts()?;
    let artifact_root = home.path().join("runner-short-circuit-artifacts");
    for (require_fixture, expect_json, command_id) in [
        (true, true, "missing-fixture-json"),
        (false, true, "spawn-failure-json"),
        (true, false, "missing-fixture-text"),
        (false, false, "spawn-failure-text"),
    ] {
        let mut spec = isolated_cass_spec(home.path(), &[], "runner-short-circuit");
        spec.binary_path = home.path().join("missing-executable").display().to_string();
        spec.require_path = require_fixture.then(|| home.path().join("missing-fixture"));
        let entry = run_with_artifacts(
            &spec,
            &RunExpectation {
                expect_json,
                assertions: vec![("must_not_run".to_string(), Box::new(|_, _| false))],
            },
            &ArtifactRunSpec {
                artifact_root: &artifact_root,
                run_id: "runner-short-circuit",
                scenario_id: "unexecuted-command",
                command_id,
                issue_ids: &["coding_agent_session_search-0qxwg"],
                privacy_note: "Real missing fixture and OS spawn failures; no model capability proof.",
                suite_started_ms: 0,
                accepted_exit_codes: &[0],
                forbidden_stream_markers: &[],
                extra_artifact_paths: &[],
            },
            0,
        )
        .map_err(|error| format!("write {command_id} artifacts: {error}"))?;
        let event: RunEvent = serde_json::from_slice(
            &std::fs::read(&entry.event_path)
                .map_err(|error| format!("read {command_id} event: {error}"))?,
        )
        .map_err(|error| format!("decode {command_id} event: {error}"))?;
        let expected_failure = if require_fixture {
            matches!(event.outcome, RunOutcome::MissingFixture { .. })
        } else {
            matches!(event.outcome, RunOutcome::CommandFailure { exit_code: -1 })
        };
        let expected_parsed_json = !expect_json;
        if !expected_failure
            || event.parsed_json_ok != expected_parsed_json
            || event.stdout_len != 0
            || !event.assertion_results.is_empty()
            || entry.outcome.is_success()
            || matches!(entry.proof_status, ProofStatus::Pass)
        {
            return Err(format!("untruthful {command_id} proof event: {event:?}"));
        }
    }
    Ok(())
}

fn decode_model_output(stdout: &str, format: &str) -> Result<Value, String> {
    if matches!(format, "toon") {
        if serde_json::from_str::<Value>(stdout).is_ok() {
            return Err("TOON was requested, but the command returned JSON".to_string());
        }
        return toon::try_decode(stdout, None)
            .map(Value::from)
            .map_err(|error| format!("decode model TOON output: {error}"));
    }
    if matches!(format, "jsonl" | "compact" | "sessions") && stdout.lines().count() != 1 {
        return Err(format!("{format} must contain exactly one JSON record"));
    }
    serde_json::from_str(stdout).map_err(|error| format!("decode model {format} output: {error}"))
}

/// Share the private source roots and model policy across direct and traced
/// real-binary commands. Explicit install remains permitted by this policy.
fn isolated_cass_spec(home: &std::path::Path, args: &[&str], phase: &str) -> RunSpec {
    let env_overrides = BTreeMap::from([
        ("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT".into(), "1".into()),
        ("CASS_IGNORE_SOURCES_CONFIG".into(), "1".into()),
        ("CASS_AUTO_REFRESH".into(), "0".into()),
        (
            "CODEX_HOME".into(),
            home.join(".codex").display().to_string(),
        ),
        (
            "CLAUDE_CONFIG_DIR".into(),
            home.join(".claude").display().to_string(),
        ),
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
    RunSpec {
        binary_path: cargo_bin("cass").display().to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        timeout: Duration::from_secs(30),
        env_overrides,
        cwd: home.to_path_buf(),
        fixture_id: Some("isolated-empty-model-cache".into()),
        require_path: Some(home.to_path_buf()),
        phase: phase.to_string(),
        mode: RunMode::Quick,
    }
}

/// Run the real `cass` binary with a private cache, bounded execution and
/// retained runner artifacts. Returns `(entry, stdout, stderr)` so callers can
/// preserve the actual execution facts when emitting another proof format.
/// A prepared empty model directory must stay empty; other calls require it
/// to remain absent.
fn run_isolated_cass(
    home: &std::path::Path,
    args: &[&str],
    empty_model_dir_exists: bool,
) -> Result<(ScenarioArtifactEntry, String, String), String> {
    let phase = args
        .get(1)
        .copied()
        .ok_or_else(|| "model command is missing its subcommand".to_string())?;
    let is_status = matches!(phase, "status");
    let format = args
        .windows(2)
        .find(|pair| matches!(pair[0], "--robot-format"))
        .map(|pair| pair[1])
        .unwrap_or("json");
    let spec = isolated_cass_spec(home, args, &format!("models-{phase}"));
    let expectation = RunExpectation {
        expect_json: !matches!(format, "toon"),
        assertions: vec![
            (
                "empty-cache-acquisition-report".into(),
                Box::new(move |stdout, _stderr| {
                    decode_model_output(stdout, format)
                        .and_then(|payload| {
                            if is_status {
                                minilm_acquisition(&payload)
                            } else {
                                payload.get("model_acquisition").cloned().ok_or_else(|| {
                                    "model output missing model_acquisition".to_string()
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
                "private-model-cache-remains-unchanged".into(),
                Box::new(move |stdout, _stderr| {
                    decode_model_output(stdout, format)
                        .and_then(|payload| {
                            if is_status {
                                minilm_model_dir(&payload, home)
                            } else {
                                payload
                                    .get("model_dir")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                                    .ok_or_else(|| "model output missing model_dir".to_string())
                            }
                        })
                        .is_ok_and(|model_dir| {
                            let path = std::path::Path::new(&model_dir);
                            path.starts_with(home.join("cass-data"))
                                && if empty_model_dir_exists {
                                    path.is_dir()
                                        && std::fs::read_dir(path)
                                            .is_ok_and(|mut entries| entries.next().is_none())
                                } else {
                                    !path.exists()
                                }
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

/// Check the reported acquisition decision for an empty cache. A capable
/// runtime reports `absent`, `explicit_install_required` and lexical fallback.
/// The contract also recognizes legacy runtime hard-block states. These fields
/// do not independently observe network attempts.
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
            "an absent model must report its acquisition decision via \
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
        let tmp = isolated_home_with_retained_artifacts()?;
        let mut args = vec![
            "models",
            "install",
            "--model",
            "minilm",
            "--dry-run",
            "--json",
        ];
        args.extend(extra);
        let (_, stdout, _) = run_isolated_cass(tmp.path(), &args, false)?;
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
fn models_inspection_respects_structured_output_formats() -> Result<(), String> {
    use coding_agent_search::search::fastembed_embedder::FastEmbedder;
    use coding_agent_search::search::model_download::ModelManifest;

    for format in ["json", "jsonl", "compact", "sessions", "toon"] {
        for (phase, empty_model_dir_exists) in
            [("status", false), ("verify", false), ("verify", true)]
        {
            let tmp = isolated_home_with_retained_artifacts()?;
            if empty_model_dir_exists {
                let model_dir =
                    FastEmbedder::model_dir_for(&tmp.path().join("cass-data"), "minilm")
                        .ok_or("missing registered MiniLM directory")?;
                std::fs::create_dir_all(&model_dir)
                    .map_err(|error| format!("prepare empty model directory: {error}"))?;
            }
            let (_, stdout, _) = run_isolated_cass(
                tmp.path(),
                &["models", phase, "--robot-format", format],
                empty_model_dir_exists,
            )?;
            let payload = decode_model_output(&stdout, format)?;
            if matches!(phase, "verify") {
                if payload.get("all_valid").and_then(Value::as_bool) != Some(false) {
                    return Err(format!(
                        "empty-cache verification reported valid files: {payload}"
                    ));
                }
                if empty_model_dir_exists {
                    let files = payload
                        .get("files")
                        .and_then(Value::as_array)
                        .ok_or("existing model directory skipped per-file verification")?;
                    if files.len() != ModelManifest::minilm_v2().files.len()
                        || files.iter().any(|file| {
                            file.get("exists").and_then(Value::as_bool) != Some(false)
                                || file.get("valid").and_then(Value::as_bool) != Some(false)
                        })
                    {
                        return Err(format!("incomplete empty-cache verification: {payload}"));
                    }
                } else if payload.get("error").and_then(Value::as_str)
                    != Some("model directory does not exist")
                {
                    return Err(format!(
                        "missing-directory verification lost its diagnostic: {payload}"
                    ));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn status_and_verify_emit_acquisition_block_and_prove_no_auto_download() -> Result<(), String> {
    let tmp = isolated_home_with_retained_artifacts()?;

    // Pin the data directory and policy and neutralize the runtime model-dir
    // override: inherited environment or .env values must not redirect or
    // disable this empty cache. Explicit values also prevent dotenv refills.
    let (entry, stdout, stderr) =
        run_isolated_cass(tmp.path(), &["models", "status", "--json"], false)?;
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
        run_isolated_cass(tmp.path(), &["models", "verify", "--json"], false)?;
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

/// This Linux acceptance case requires /usr/bin/strace and permission to trace
/// its own children. Missing or denied tracing fails; it is never a zero-run pass.
/// The explicit-install control must produce an observed INET connect attempt.
#[cfg(target_os = "linux")]
#[test]
fn model_commands_have_no_network_syscalls_on_linux() -> Result<(), String> {
    use coding_agent_search::search::model_download::compute_sha256;
    use std::io::Read;
    use std::path::PathBuf;

    let now_ms = || -> Result<u64, String> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis()
            .try_into()
            .map_err(|_| "observer timestamp exceeds u64".to_string())
    };
    let suite_started_ms = now_ms()?;
    let home = isolated_home_with_retained_artifacts()?;
    let (identity_entry, status_stdout, _) =
        run_isolated_cass(home.path(), &["models", "status", "--json"], false)?;
    let identity: RunEvent = serde_json::from_slice(
        &std::fs::read(&identity_entry.event_path)
            .map_err(|e| format!("read CASS identity: {e}"))?,
    )
    .map_err(|e| format!("decode CASS identity: {e}"))?;
    if identity.binary_version.as_deref() != Some(env!("CARGO_PKG_VERSION"))
        || identity
            .binary_hash
            .as_ref()
            .is_none_or(|hash| hash.len() != 64)
    {
        return Err("network observer requires the actual test-built CASS identity".into());
    }
    let subject = PathBuf::from(&identity.binary_path);
    let subject_hash = compute_sha256(&subject).map_err(|e| format!("hash traced CASS: {e}"))?;
    let status: Value = serde_json::from_str(&status_stdout).map_err(|e| e.to_string())?;
    let model_dir = PathBuf::from(minilm_model_dir(&status, home.path())?);
    let expected_fingerprint = check_model_identity(&minilm_acquisition(&status)?)?;
    let artifacts = home.path().join("network-observer");
    std::fs::create_dir_all(&artifacts).map_err(|e| e.to_string())?;

    // Ingest a real provider fixture through CASS, then demand a real lexical
    // hit. An empty/missing archive cannot satisfy the no-download search case.
    let source = home
        .path()
        .join(".claude/projects/network-proof/session.jsonl");
    std::fs::create_dir_all(source.parent().ok_or("fixture source has no parent")?)
        .map_err(|e| e.to_string())?;
    std::fs::write(
        &source,
        include_str!("fixtures/claude_code_real/projects/-test-project/agent-test123.jsonl"),
    )
    .map_err(|e| e.to_string())?;
    let source_arg = source.to_str().ok_or("fixture source is not UTF-8")?;
    let seed = run_with_artifacts(
        &isolated_cass_spec(home.path(), &["index", "--watch-once", source_arg, "--json"], "seed"),
        &RunExpectation::default(),
        &ArtifactRunSpec {
            artifact_root: &artifacts,
            run_id: "linux-network-observer",
            scenario_id: "private-corpus",
            command_id: "seed",
            issue_ids: &["coding_agent_session_search-0qxwg"],
            privacy_note: "Fixture setup only; the traced search must return an actual fixture hit.",
            suite_started_ms,
            accepted_exit_codes: &[0],
            forbidden_stream_markers: &[],
            extra_artifact_paths: &[],
        },
        now_ms()?,
    )
    .map_err(|e| format!("seed private archive: {e}"))?;
    if !seed.outcome.is_success() {
        return Err(format!(
            "CASS fixture index failed; inspect {}",
            seed.event_path
        ));
    }

    let observe = |args: &[&str], label: &str, network_expected: bool| -> Result<Value, String> {
        let trace_path = artifacts.join(format!("{label}.network.log"));
        if trace_path.exists() {
            return Err(format!("refusing to reuse trace {}", trace_path.display()));
        }
        let mut spec = isolated_cass_spec(home.path(), args, label);
        if spec.binary_path != identity.binary_path {
            return Err("traced CASS path differs from its identity event".into());
        }
        spec.binary_path = "/usr/bin/strace".into();
        spec.args = vec![
            "-f".into(),
            "-s".into(),
            "256".into(),
            "-e".into(),
            "trace=%network".into(),
            "-o".into(),
            trace_path.display().to_string(),
            "--".into(),
            identity.binary_path.clone(),
        ];
        spec.args.extend(args.iter().map(|arg| (*arg).to_string()));
        let expected = RunExpectation {
            expect_json: !network_expected,
            assertions: vec![(
                "observed-process-tree-network-policy".into(),
                Box::new(|_, _| {
                    let mut trace = String::new();
                    let read = std::fs::File::open(&trace_path)
                        .and_then(|file| file.take(1024 * 1024 + 1).read_to_string(&mut trace));
                    read.is_ok()
                        && trace.len() <= 1024 * 1024
                        && trace.contains("+++ exited with")
                        && if network_expected {
                            trace
                                .lines()
                                .any(|line| line.contains("connect(") && line.contains("AF_INET"))
                        } else {
                            // Reject every network syscall, including Unix
                            // resolver IPC that could delegate DNS elsewhere.
                            !trace.contains('(')
                        }
                }),
            )],
        };
        let extra = [
            trace_path.clone(),
            PathBuf::from(&identity_entry.event_path),
        ];
        let entry = run_with_artifacts(
            &spec,
            &expected,
            &ArtifactRunSpec {
                artifact_root: &artifacts,
                run_id: "linux-network-observer",
                scenario_id: "private-corpus",
                command_id: label,
                issue_ids: &["coding_agent_session_search-0qxwg"],
                privacy_note: "Actual strace wrapper and CASS child; subject identity is cited separately. The failed explicit install calibrates the detector, not installation success.",
                suite_started_ms,
                accepted_exit_codes: if network_expected { &[23] } else { &[0] },
                forbidden_stream_markers: &[],
                extra_artifact_paths: &extra,
            },
            now_ms()?,
        )
        .map_err(|e| format!("observe {label}: {e}"))?;
        if !entry.outcome.is_success()
            || !entry.assertions_ran
            || entry.generated_only
            || !entry.redaction_safe
            || !entry.fresh_for_suite
            || entry.proof_status != ProofStatus::Pass
        {
            return Err(format!(
                "network observation {label} failed; inspect {} and {}",
                entry.event_path,
                trace_path.display()
            ));
        }
        if network_expected {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&std::fs::read(&entry.stdout_path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("decode observed {label}: {e}"))
    };

    let status = observe(&["models", "status", "--json"], "status", false)?;
    let acquisition = minilm_acquisition(&status)?;
    check_no_auto_download(&acquisition)?;
    if check_model_identity(&acquisition)? != expected_fingerprint {
        return Err("traced status model fingerprint changed".into());
    }
    let verify = observe(&["models", "verify", "--json"], "verify", false)?;
    let acquisition = verify
        .get("model_acquisition")
        .ok_or("verify lacks acquisition report")?;
    check_no_auto_download(acquisition)?;
    if check_model_identity(acquisition)? != expected_fingerprint {
        return Err("traced verify model fingerprint changed".into());
    }
    let plan = observe(
        &["models", "install", "--dry-run", "--json"],
        "preview",
        false,
    )?;
    if plan.get("status").and_then(Value::as_str) != Some("planned") {
        return Err(format!(
            "observed preview did not produce an install plan: {plan}"
        ));
    }
    let search = observe(
        &[
            "search",
            "smartedgar",
            "--json",
            "--limit",
            "1",
            "--fields",
            "content",
            "--no-daemon",
        ],
        "search",
        false,
    )?;
    if !search
        .get("hits")
        .and_then(Value::as_array)
        .is_some_and(|hits| {
            hits.iter().any(|hit| {
                hit.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("smartedgar"))
            })
        })
        || model_dir.exists()
    {
        return Err(format!(
            "expected a real lexical hit with no acquired model: {search}"
        ));
    }

    let socket = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    let port = socket.local_addr().map_err(|e| e.to_string())?.port();
    drop(socket);
    let mirror = format!("http://127.0.0.1:{port}");
    observe(
        &[
            "models", "install", "--model", "minilm", "--yes", "--mirror", &mirror,
        ],
        "install-control",
        true,
    )?;
    if compute_sha256(&subject).map_err(|e| e.to_string())? != subject_hash {
        return Err("CASS executable changed during network observation".into());
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
    let tmp = isolated_home_with_retained_artifacts()?;
    let artifact_dir = tmp.path().join("proof");
    std::fs::create_dir_all(&artifact_dir).map_err(|e| format!("create artifact dir: {e}"))?;

    // The helper pins the model cache and semantic policy to this fixture.
    let (entry, stdout, stderr) =
        run_isolated_cass(tmp.path(), &["models", "status", "--json"], false)?;
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
    if binary_version != env!("CARGO_PKG_VERSION") {
        return Err(format!(
            "observed model CLI version does not match the test build: {binary_version}"
        ));
    }

    // Persist the real stdout/stderr under the retained private HOME, then
    // emit one `.12.3`-schema proof-log record.
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

    // Artifact paths must stay inside the private artifact dir.
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
            "artifact paths must be redaction-safe private paths; offending keys: {unsafe_paths:?}"
        ));
    }

    // Scope exit used to erase the evidence while every assertion still
    // passed. Read the actual files after releasing the temporary handle.
    drop(tmp);
    for (path, expected) in [
        (&log_path, jsonl.as_str()),
        (&stdout_path, stdout.as_str()),
        (&stderr_path, stderr.as_str()),
    ] {
        let retained = std::fs::read_to_string(path)
            .map_err(|error| format!("read retained artifact {}: {error}", path.display()))?;
        if retained != expected {
            return Err(format!("retained artifact changed: {}", path.display()));
        }
    }
    Ok(())
}
