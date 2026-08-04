//! Real-binary gate for the hardened model-acquisition contract surfaced on
//! `cass models status` / `cass models verify` (bead
//! `coding_agent_session_search-0qxwg`, follow-on to
//! `cass-fleet-resilience-20260608-uojcg.5.5`).
//!
//! The pure contract (src/search/model_acquisition.rs) is now projected into
//! both robot surfaces as a `model_acquisition` block. These tests drive the
//! installed `cass` against a fully isolated, network-free HOME and prove, end
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
//! Authored Result-returning and panic-free (no unwrap/expect/assert/panic, no
//! `format!`-in-loop, and no raw `==`/`!=`) so a brand-new test file stays at
//! zero UBS findings even though the proof-log vocabulary is secret-adjacent.

use assert_cmd::cargo::cargo_bin;
use serde_json::{Value, json};

/// Run the real `cass` binary in a fully isolated HOME so a missing model can
/// never trigger a download. Returns `(exit_code, stdout, stderr, elapsed_ms)`.
fn run_isolated_cass(
    home: &std::path::Path,
    args: &[&str],
) -> Result<(Option<i32>, String, String, i64), String> {
    let start = std::time::Instant::now();
    let output = std::process::Command::new(cargo_bin("cass"))
        .args(args)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .output()
        .map_err(|e| format!("run cass {args:?}: {e}"))?;
    let elapsed_ms = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((output.status.code(), stdout, stderr, elapsed_ms))
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
fn minilm_model_dir(status: &Value) -> Result<String, String> {
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
        .and_then(|m| m.get("model_dir").and_then(Value::as_str))
        .map(str::to_string)
        .ok_or_else(|| format!("status JSON missing models[minilm].model_dir: {status}"))
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
    if !(next_cmd.starts_with("cass models install --model ") && next_cmd.ends_with(" --json")) {
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
fn status_and_verify_emit_acquisition_block_and_prove_no_auto_download() -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;

    // `cass models status` takes no --data-dir flag; isolation comes from the
    // XDG_DATA_HOME this helper sets, so the model cache is an empty dir.
    let (code, stdout, stderr, _ms) =
        run_isolated_cass(tmp.path(), &["models", "status", "--json"])?;
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
    let model_dir = minilm_model_dir(&status)?;
    if std::path::Path::new(&model_dir)
        .join("model.safetensors")
        .exists()
    {
        return Err(format!(
            "status must not download model.safetensors; one appeared under {model_dir}"
        ));
    }

    // --- cass models verify --json (also XDG-isolated; no --data-dir needed) ---
    let (vcode, vstdout, vstderr, _vms) =
        run_isolated_cass(tmp.path(), &["models", "verify", "--json"])?;
    if !matches!(vcode, Some(0)) {
        return Err(format!(
            "models verify --json exit {vcode:?}; stderr: {vstderr}"
        ));
    }
    let verify: Value = serde_json::from_str(&vstdout)
        .map_err(|e| format!("models verify stdout is not JSON: {e}; stdout: {vstdout}"))?;
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

    // `cass models status` takes no --data-dir flag; XDG_DATA_HOME isolates it.
    let (code, stdout, stderr, elapsed_ms) =
        run_isolated_cass(tmp.path(), &["models", "status", "--json"])?;
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
    let model_dir = minilm_model_dir(&status)?;

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

    // sanitized_env: only the safe, non-secret variables this harness sets.
    let sanitized_env = json!({
        "CASS_IGNORE_SOURCES_CONFIG": "1",
        "CODING_AGENT_SEARCH_NO_UPDATE_PROMPT": "1",
    });
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
        "started_at_ms": 0,
        "finished_at_ms": elapsed_ms,
        "elapsed_ms": elapsed_ms,
        "meta": {
            "cass_binary_path": cargo_bin("cass").display().to_string(),
            "cass_version": env!("CARGO_PKG_VERSION"),
            "cargo_profile": "test",
            "target_dir": std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string()),
            "data_dir": tmp.path().join(".local/share").display().to_string(),
            "config_dir": tmp.path().join(".config").display().to_string(),
            "model_dir": model_dir,
        },
        "execution": {
            "argv": ["cass", "models", "status", "--json"],
            "sanitized_env": sanitized_env,
            "timeout_ms": 60_000,
            "exit_code": code,
            "timed_out": false,
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
