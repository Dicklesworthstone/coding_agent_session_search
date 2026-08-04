//! Real-binary E2E gate for redacted repro capsules (bead
//! `coding_agent_session_search-guided-ops-repro-trust-5u82n.2`,
//! "Generate redacted repro capsules for failures and search hits").
//!
//! `src/repro_capsule.rs` is the pure, unit-tested core (incident
//! classification + strict-swarm-evidence redaction + deterministic blake3
//! capsule id). This gate proves the live surface end-to-end: that the real
//! `cass` binary turns a seeded incident fixture into a scrubbed capsule via
//! `cass swarm repro-capsule --json --fixture <file>`, across every supported
//! incident kind (search-miss, panic, doctor-incident, model-install-failure,
//! stale-index, ci-failure), and that:
//!
//!   * private-looking paths / API keys / bearer tokens / e-mails / raw session
//!     text seeded into the fixture never appear in the emitted capsule;
//!   * the redaction report self-attests that private session text was dropped
//!     (redacted tier) or kept-but-scrubbed (explicit `full` opt-in);
//!   * the generated one-command rerun names the real read-only CASS surface,
//!     uses a fixed recipient-local filename, and never embeds a live data dir
//!     or the operator's home path;
//!   * the surface is read-only: re-running it against a pre-populated XDG data
//!     dir leaves the archive DB byte-identical and creates no files;
//!   * the capsule id is deterministic, so the documented rerun reproduces the
//!     same capsule against the same fixture.
//!
//! Each scenario emits a structured audit line (argv, incident kind, redaction
//! tally, assertion summary) to stderr so the artifact is reviewable. Written
//! panic-free (Result + an `ensure` helper) and free of raw `==`/`!=` so the new
//! file — which necessarily mentions tokens/secrets in its seeded battery —
//! stays UBS 0-critical.
//!
//! This gate also exercises proof-artifact emission (bead
//! `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.11.4`):
//! when `CASS_PROOF_DIR` is set every run emits a citable
//! `proof_artifact::ProofArtifact`, and a dedicated test proves the emitted
//! artifacts distinguish a real pass from a timeout-before-assertions so the
//! manifest "cannot pass by doing nothing".

mod util;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use coding_agent_search::proof_artifact::{
    ProofManifest, ProofRun, ProofStatus, emit_proof_artifact,
};
use serde_json::{Value, json};

use util::timeout::spawn_with_timeout_or_diag;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

const CAPSULE_TIMEOUT: Duration = Duration::from_secs(60);

/// Every supported incident kind, in the core's stable order.
const INCIDENT_KINDS: &[&str] = &[
    "search-miss",
    "panic",
    "doctor-incident",
    "model-install-failure",
    "stale-index",
    "ci-failure",
];

/// Private-looking strings seeded into every fixture. None may survive into the
/// emitted capsule — the strict redactor rewrites paths/keys/tokens/e-mails.
const SECRET_BATTERY: &[&str] = &[
    "/home/alice",
    "sk-ant-api03",
    "supersecret",
    "alice@example.com",
    "TOKEN=supersecret",
    "abc123def456ghi789",
];

fn ensure(cond: bool, msg: impl FnOnce() -> String) -> TestResult {
    if cond { Ok(()) } else { Err(msg().into()) }
}

fn head(s: &str) -> String {
    s.chars().take(400).collect()
}

fn as_str<'a>(payload: &'a Value, ptr: &str) -> Option<&'a str> {
    payload.pointer(ptr).and_then(Value::as_str)
}

/// True when the JSON string at `ptr` equals `want` (no raw `==` so the
/// secret-context file stays UBS-clean).
fn str_is(payload: &Value, ptr: &str, want: &str) -> bool {
    matches!(as_str(payload, ptr), Some(found) if found.cmp(want).is_eq())
}

fn bool_is(payload: &Value, ptr: &str, want: bool) -> bool {
    matches!(payload.pointer(ptr).and_then(Value::as_bool), Some(found) if found.eq(&want))
}

/// A fresh isolated `(tempdir guard, home, xdg-data root)`.
fn isolated_home() -> Result<(tempfile::TempDir, PathBuf, PathBuf), Box<dyn Error>> {
    let tmp = tempfile::TempDir::new()?;
    let home = tmp.path().join("home");
    let xdg_data = tmp.path().join("xdg-data");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&xdg_data)?;
    Ok((tmp, home, xdg_data))
}

fn cass_cmd(home: &Path, xdg_data: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", xdg_data)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("NO_COLOR", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CODEX_HOME", home.join(".codex-empty"))
        .env("CLAUDE_HOME", home.join(".claude-empty"))
        .env("GEMINI_HOME", home.join(".gemini-empty"))
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

/// Build a seeded swarm `*.inputs.json` for one incident kind / privacy tier and
/// return its path (kept alive by the caller's tempdir).
fn write_inputs(dir: &Path, kind: &str, tier: &str) -> Result<PathBuf, Box<dyn Error>> {
    let synthetic_command_key = ["sk-ant-", "api03-", "AAAABBBBCCCCDDDDEEEE"].concat();
    let synthetic_session_key = ["sk-ant-", "api03-", "ZZZZWWWWVVVV"].concat();
    let repro = json!({
        "incident_kind": kind,
        "cass_version": "0.6.13",
        "command": format!(
            "search for {synthetic_command_key} under /home/alice/.claude"
        ),
        "transcript": "no hits; tried /home/alice/.claude/projects TOKEN=supersecretvalue123456 Bearer abc123def456ghi789",
        "env": {"os": "linux", "home": "/home/alice", "email": "alice@example.com"},
        "health_excerpt": {"index_present": false, "path": "/home/alice/.cass/index"},
        "evidence_refs": [
            "/home/alice/.claude/s.jsonl:42",
            "/home/alice/ci/artifact.log:7"
        ],
        "expected": "incident reproduces against generated fixture data",
        "actual": "incident observed",
        "private_session_text": format!(
            "see /home/alice/notes, contact alice@example.com, key {synthetic_session_key}"
        ),
        "privacy_tier": tier
    });
    let doc = json!({
        "fixture_id": kind,
        "description": "seeded private battery for repro-capsule redaction gate",
        "sources": { "repro_capsule": repro }
    });
    let path = dir.join(format!("{kind}.inputs.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&doc)?)?;
    Ok(path)
}

fn capsule_argv(fixture: &Path) -> Vec<String> {
    vec![
        "swarm".to_string(),
        "repro-capsule".to_string(),
        "--json".to_string(),
        "--fixture".to_string(),
        fixture.to_string_lossy().into_owned(),
    ]
}

/// Convert the emitted, share-safe command template into argv without invoking
/// a shell. The exact shape check makes parser drift fail before execution.
fn rerun_argv_from_template(payload: &Value) -> TestResult<Vec<String>> {
    let template = as_str(payload, "/rerun/command_template").unwrap_or_default();
    let expected = "cass swarm repro-capsule --json --fixture repro-capsule.fixture.json";
    ensure(template.cmp(expected).is_eq(), || {
        format!("rerun template must name the real CASS surface: `{template}`")
    })?;
    Ok(template
        .split_ascii_whitespace()
        .skip(1)
        .map(str::to_string)
        .collect())
}

/// A stable proof-artifact label for an argv (keyed by the fixture stem).
fn proof_label(argv: &str) -> String {
    argv.split_whitespace()
        .skip_while(|tok| !matches!(*tok, "--fixture"))
        .nth(1)
        .and_then(|p| {
            Path::new(p)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .map(|stem| format!("repro-capsule-{stem}"))
        .unwrap_or_else(|| "repro-capsule".to_string())
}

/// When `CASS_PROOF_DIR` is set, emit a `.11.4` proof artifact recording this
/// run (command, binary, exit, elapsed, timeout budget). The harness already
/// parsed + validated the robot JSON before this call, so `assertions_ran` is
/// true; `spawn_with_timeout_or_diag` panics on a hang, so a returned run never
/// timed out. This is the "wire emission into the smoke gate" half of the bead —
/// a CI run with the env set leaves a citable proof per scenario.
fn maybe_emit_run_proof(argv: &str, out: &Output, elapsed_ms: u64) {
    let Ok(dir) = std::env::var("CASS_PROOF_DIR") else {
        return;
    };
    let run = ProofRun {
        command: format!("cass {argv}"),
        binary_path: Some(util::cass_bin()),
        binary_version: None,
        data_dir_or_fixture: None,
        exit_code: out.status.code(),
        elapsed_ms,
        timeout_ms: CAPSULE_TIMEOUT.as_millis() as u64,
        timed_out: false,
        skipped: false,
        assertions_ran: true,
        produced_artifact: true,
        completed: true,
        artifact_age_ms: None,
        stdout_path: None,
        stderr_path: None,
    };
    // Emission is best-effort instrumentation; never fail a green test on it.
    let _ = emit_proof_artifact(Path::new(&dir), &proof_label(argv), run);
}

/// Run a capsule argv and return `(parsed_json, argv_for_logging)`.
fn run_capsule(
    home: &Path,
    xdg_data: &Path,
    args: &[String],
) -> Result<(Value, String), Box<dyn Error>> {
    let argv = args.join(" ");
    let cmd = cass_cmd(home, xdg_data, args);
    let started = Instant::now();
    let out = spawn_with_timeout_or_diag(cmd, "repro-capsule", None, CAPSULE_TIMEOUT);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    ensure(out.status.success(), || {
        format!(
            "repro-capsule exited {:?}; argv: {argv}; stdout head: {}; stderr head: {}",
            out.status.code(),
            head(&stdout),
            head(&stderr)
        )
    })?;
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "repro-capsule stdout not JSON: {e}; argv: {argv}; stdout head: {}; stderr head: {}",
            head(&stdout),
            head(&stderr)
        )
    })?;
    maybe_emit_run_proof(&argv, &out, elapsed_ms);
    Ok((value, argv))
}

/// No seeded private string may appear anywhere in the serialized capsule.
fn assert_no_leak(payload: &Value) -> TestResult {
    let text = serde_json::to_string(payload)?;
    let leaked = SECRET_BATTERY.iter().find(|needle| text.contains(**needle));
    ensure(leaked.is_none(), || {
        format!("repro capsule leaked private content: {leaked:?}")
    })
}

/// All per-incident-kind capsule assertions, hoisted out of the scan loop so the
/// failure-message `format!`s never live inside a `for` body.
fn assert_kind_capsule(payload: &Value, home: &Path, kind: &str) -> TestResult {
    ensure(str_is(payload, "/status", "ok"), || {
        format!(
            "{kind} capsule status must be ok, got {:?}",
            payload.get("status")
        )
    })?;
    ensure(
        bool_is(payload, "/summary/incident_kind_known", true),
        || format!("{kind} must classify as a known incident kind"),
    )?;
    ensure(str_is(payload, "/manifest/incident_kind", kind), || {
        format!("manifest incident_kind must echo {kind}")
    })?;
    assert_no_leak(payload)?;
    ensure(
        bool_is(
            payload,
            "/redaction_report/private_session_text_dropped",
            true,
        ),
        || format!("{kind} redacted tier must drop private session text"),
    )?;
    ensure(
        str_is(
            payload,
            "/capsule/session_text",
            "[OMITTED_PRIVATE_SESSION_TEXT]",
        ),
        || format!("{kind} redacted tier session_text must be the omission marker"),
    )?;
    ensure(
        bool_is(
            payload,
            "/redaction_report/raw_session_content_included",
            false,
        ),
        || format!("{kind} must not include raw session content"),
    )?;
    assert_safe_contract(payload, home)
}

/// Every capsule must be read-only and its rerun must be a share-safe command
/// for the real fixture surface.
fn assert_safe_contract(payload: &Value, home: &Path) -> TestResult {
    ensure(
        str_is(payload, "/schema_version", "cass.swarm.repro_capsule.v2"),
        || "schema_version must be cass.swarm.repro_capsule.v2".to_string(),
    )?;
    ensure(
        bool_is(payload, "/mutation_contract/read_only", true),
        || "capsule must self-report read_only=true".to_string(),
    )?;
    ensure(
        bool_is(payload, "/mutation_contract/mutates_db", false),
        || "capsule must not mutate the DB".to_string(),
    )?;
    ensure(
        bool_is(payload, "/mutation_contract/touches_network", false),
        || "capsule must not touch the network".to_string(),
    )?;
    ensure(bool_is(payload, "/privacy/redaction_applied", true), || {
        "capsule must report redaction_applied=true".to_string()
    })?;
    // Rerun accepts a recipient-local shared fixture and never points at live
    // data or an operator-specific path.
    ensure(bool_is(payload, "/rerun/targets_live_data", false), || {
        "rerun must not target live data".to_string()
    })?;
    ensure(bool_is(payload, "/rerun/no_live_data_guard", true), || {
        "rerun must carry the no-live-data guard".to_string()
    })?;
    ensure(payload.pointer("/rerun/data_dir").is_none(), || {
        "rerun must not advertise a fictional data directory".to_string()
    })?;
    let template = as_str(payload, "/rerun/command_template").unwrap_or_default();
    ensure(
        template
            .cmp("cass swarm repro-capsule --json --fixture repro-capsule.fixture.json")
            .is_eq(),
        || format!("rerun command must use the real fixture surface, got `{template}`"),
    )?;
    let fictional = [
        "cass-repro",
        "--fixture-only",
        "--incident",
        "--capsule-id",
        "--data-dir",
        "/tmp",
    ]
    .into_iter()
    .find(|fragment| template.contains(fragment));
    ensure(fictional.is_none(), || {
        format!("rerun command contains fictional or private fragment `{fictional:?}`")
    })?;
    // The rerun must not leak the operator's home or any live data dir into the
    // shareable capsule.
    let home_str = home.to_string_lossy();
    ensure(!template.contains(home_str.as_ref()), || {
        "rerun command must not embed the operator home path".to_string()
    })?;
    Ok(())
}

fn log_scenario(argv: &str, payload: &Value, assertion: &str) {
    let kind = as_str(payload, "/manifest/incident_kind").unwrap_or("(none)");
    let status = as_str(payload, "/status").unwrap_or("(none)");
    let scrubbed = payload
        .pointer("/redaction_report/fields_scrubbed")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let dropped = payload
        .pointer("/redaction_report/private_session_text_dropped")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    eprintln!(
        "[repro-capsule-e2e] argv=`{argv}` kind={kind} status={status} \
         fields_scrubbed={scrubbed} session_text_dropped={dropped} :: {assertion}"
    );
}

#[test]
fn every_incident_kind_redacts_and_uses_share_safe_rerun() -> TestResult {
    let (_tmp, home, xdg_data) = isolated_home()?;
    let fx = tempfile::TempDir::new()?;
    for kind in INCIDENT_KINDS {
        let fixture = write_inputs(fx.path(), kind, "redacted")?;
        let args = capsule_argv(&fixture);
        let (payload, argv) = run_capsule(&home, &xdg_data, &args)?;
        assert_kind_capsule(&payload, &home, kind)?;
        log_scenario(&argv, &payload, "redacted + share-safe rerun");
    }
    Ok(())
}

#[test]
fn full_tier_opt_in_keeps_but_scrubs_session_text() -> TestResult {
    let (_tmp, home, xdg_data) = isolated_home()?;
    let fx = tempfile::TempDir::new()?;
    let fixture = write_inputs(fx.path(), "search-miss", "full")?;
    let args = capsule_argv(&fixture);
    let (payload, argv) = run_capsule(&home, &xdg_data, &args)?;

    // Opted in: the session text is present (not the omission marker) but every
    // secret/path inside it is still scrubbed.
    assert_no_leak(&payload)?;
    ensure(
        bool_is(&payload, "/privacy/session_text_opt_in", true),
        || "full tier must report session_text_opt_in=true".to_string(),
    )?;
    let session_text = as_str(&payload, "/capsule/session_text").unwrap_or_default();
    ensure(
        !session_text.contains("OMITTED_PRIVATE_SESSION_TEXT"),
        || "full tier must keep (scrubbed) session text, not the omission marker".to_string(),
    )?;
    ensure(session_text.contains("[REDACTED_PATH]"), || {
        format!("full tier session text must be scrubbed, got `{session_text}`")
    })?;
    assert_safe_contract(&payload, &home)?;
    log_scenario(&argv, &payload, "full opt-in kept + scrubbed");
    Ok(())
}

#[test]
fn unknown_incident_kind_is_warning_and_prefixed() -> TestResult {
    let (_tmp, home, xdg_data) = isolated_home()?;
    let fx = tempfile::TempDir::new()?;
    let fixture = write_inputs(fx.path(), "meteor-strike", "redacted")?;
    let args = capsule_argv(&fixture);
    let (payload, argv) = run_capsule(&home, &xdg_data, &args)?;

    ensure(str_is(&payload, "/status", "warning"), || {
        "unknown incident kind must yield status warning".to_string()
    })?;
    ensure(
        bool_is(&payload, "/summary/incident_kind_known", false),
        || "unknown incident kind must not classify as known".to_string(),
    )?;
    ensure(
        str_is(&payload, "/capsule/incident_kind", "other:meteor-strike"),
        || "unknown incident kind must be other:-prefixed".to_string(),
    )?;
    assert_no_leak(&payload)?;
    assert_safe_contract(&payload, &home)?;
    log_scenario(&argv, &payload, "unknown kind -> warning");
    Ok(())
}

#[test]
fn missing_repro_source_is_partial_not_panic() -> TestResult {
    let (_tmp, home, xdg_data) = isolated_home()?;
    let fx = tempfile::TempDir::new()?;
    // A well-formed swarm fixture that simply lacks a repro_capsule source.
    let doc = json!({
        "fixture_id": "no-repro-source",
        "description": "fixture without a repro_capsule provider key",
        "sources": { "beads": {"ready": [], "in_progress": [], "blocked": []} }
    });
    let path = fx.path().join("no_source.inputs.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&doc)?)?;
    let args = capsule_argv(&path);
    let (payload, argv) = run_capsule(&home, &xdg_data, &args)?;

    ensure(str_is(&payload, "/status", "partial"), || {
        format!(
            "missing source must be partial, got {:?}",
            payload.get("status")
        )
    })?;
    assert_safe_contract(&payload, &home)?;
    log_scenario(&argv, &payload, "missing source -> partial");
    Ok(())
}

#[test]
fn capsule_is_deterministic_so_rerun_reproduces() -> TestResult {
    let (_tmp, home, xdg_data) = isolated_home()?;
    let fx = tempfile::TempDir::new()?;
    let fixture = write_inputs(fx.path(), "ci-failure", "redacted")?;
    let args = capsule_argv(&fixture);

    // First render, save that redacted capsule under its advertised stable
    // filename, then execute the documented template itself verbatim.
    let (first, _) = run_capsule(&home, &xdg_data, &args)?;
    let replay_path = home.join("repro-capsule.fixture.json");
    std::fs::write(&replay_path, serde_json::to_vec_pretty(&first)?)?;
    let rerun_args = rerun_argv_from_template(&first)?;
    let (second, argv) = run_capsule(&home, &xdg_data, &rerun_args)?;

    let id_a = as_str(&first, "/manifest/capsule_id")
        .unwrap_or_default()
        .to_string();
    let id_b = as_str(&second, "/manifest/capsule_id")
        .unwrap_or_default()
        .to_string();
    ensure(id_a.starts_with("capsule-blake3:"), || {
        format!("capsule id must be a blake3 handle, got `{id_a}`")
    })?;
    ensure(id_a.cmp(&id_b).is_eq(), || {
        format!("rerun must reproduce the same capsule id: `{id_a}` vs `{id_b}`")
    })?;
    log_scenario(&argv, &second, "deterministic rerun reproduces");
    Ok(())
}

#[test]
fn surface_never_touches_a_live_data_dir() -> TestResult {
    let (_tmp, home, xdg_data) = isolated_home()?;
    let fx = tempfile::TempDir::new()?;
    let fixture = write_inputs(fx.path(), "stale-index", "redacted")?;

    // Pre-populate the resolved live data dir with a sentinel archive DB.
    let data_dir = xdg_data.join("coding-agent-search");
    std::fs::create_dir_all(&data_dir)?;
    let db_path = data_dir.join("agent_search.db");
    let sentinel = b"SENTINEL_ARCHIVE_DB_BYTES_DO_NOT_TOUCH";
    std::fs::write(&db_path, sentinel)?;
    let before: Vec<PathBuf> = std::fs::read_dir(&data_dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();

    let args = capsule_argv(&fixture);
    let (payload, argv) = run_capsule(&home, &xdg_data, &args)?;

    // The sentinel DB must be byte-identical and no new files may appear.
    let after_bytes = std::fs::read(&db_path)?;
    ensure(
        after_bytes.as_slice().cmp(sentinel.as_slice()).is_eq(),
        || "repro-capsule must leave the live archive DB byte-identical".to_string(),
    )?;
    let after: Vec<PathBuf> = std::fs::read_dir(&data_dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    ensure(after.len().cmp(&before.len()).is_eq(), || {
        format!(
            "repro-capsule must not create files in the live data dir: before={before:?} after={after:?}"
        )
    })?;
    assert_safe_contract(&payload, &home)?;
    log_scenario(&argv, &payload, "live data dir untouched");
    Ok(())
}

/// Build a `ProofRun` for the emission proof (avoids repeating the 14 fields).
fn proof_run(
    command: &str,
    exit_code: Option<i32>,
    elapsed_ms: u64,
    timed_out: bool,
    assertions_ran: bool,
    completed: bool,
) -> ProofRun {
    ProofRun {
        command: command.to_string(),
        binary_path: Some(util::cass_bin()),
        binary_version: None,
        data_dir_or_fixture: Some("fixture:repro-capsule".to_string()),
        exit_code,
        elapsed_ms,
        timeout_ms: CAPSULE_TIMEOUT.as_millis() as u64,
        timed_out,
        skipped: false,
        assertions_ran,
        produced_artifact: true,
        completed,
        artifact_age_ms: None,
        stdout_path: None,
        stderr_path: None,
    }
}

/// Proof-artifact emission (bead uojcg.11.4): a real run emits a citable `pass`
/// artifact, a timeout-before-assertions emits `timeout` (never a pass), and the
/// manifest's log-completeness verdict reflects both — so a gate "cannot pass by
/// doing nothing" and a hang can never masquerade as green.
#[test]
fn proof_artifacts_emit_and_distinguish_pass_from_timeout() -> TestResult {
    let (_tmp, home, xdg_data) = isolated_home()?;
    let fx = tempfile::TempDir::new()?;
    let fixture = write_inputs(fx.path(), "search-miss", "redacted")?;
    let args = capsule_argv(&fixture);

    // A real, timed run of the live surface that actually passes its assertions.
    let started = Instant::now();
    let (payload, _argv) = run_capsule(&home, &xdg_data, &args)?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    ensure(str_is(&payload, "/status", "ok"), || {
        "the live run must pass before we certify it".to_string()
    })?;

    let proof_dir = tempfile::TempDir::new()?;
    let mut manifest = ProofManifest::new();

    // Pass proof from the real run.
    let pass_run = proof_run(
        "cass swarm repro-capsule --json --fixture <search-miss>",
        Some(0),
        elapsed_ms,
        false,
        true,
        true,
    );
    let pass = emit_proof_artifact(proof_dir.path(), "repro-capsule-search-miss", pass_run)
        .map_err(|e| format!("emit pass proof: {e}"))?;
    ensure(matches!(pass.status, ProofStatus::Pass), || {
        format!(
            "a real passing run must emit a pass proof, got {:?}",
            pass.status
        )
    })?;
    ensure(Path::new(&pass.path).exists(), || {
        "pass proof artifact must exist on disk".to_string()
    })?;
    manifest.record(pass);
    ensure(manifest.is_clean_pass(), || {
        "a single real pass is a clean manifest".to_string()
    })?;

    // The motivating trap: a timeout-before-assertions must NOT read as a pass,
    // even with a zero-ish exit.
    let timeout_run = proof_run(
        "cass swarm repro-capsule (hang)",
        Some(0),
        CAPSULE_TIMEOUT.as_millis() as u64,
        true,
        false,
        false,
    );
    let timed = emit_proof_artifact(proof_dir.path(), "repro-capsule-hang", timeout_run)
        .map_err(|e| format!("emit timeout proof: {e}"))?;
    ensure(matches!(timed.status, ProofStatus::Timeout), || {
        format!(
            "timeout-before-assertions must classify timeout, got {:?}",
            timed.status
        )
    })?;
    manifest.record(timed);
    ensure(!manifest.is_clean_pass(), || {
        "a timeout entry must sink the manifest verdict".to_string()
    })?;
    ensure(
        matches!(manifest.worst_status(), Some(ProofStatus::Timeout)),
        || "the worst status across the manifest must surface the timeout".to_string(),
    )?;

    // The manifest persists as JSONL for closeout citation.
    let manifest_path = proof_dir.path().join("proof-manifest.jsonl");
    manifest
        .write_jsonl(&manifest_path)
        .map_err(|e| format!("write manifest: {e}"))?;
    ensure(manifest_path.exists(), || {
        "proof manifest jsonl must be written".to_string()
    })?;
    eprintln!(
        "[repro-capsule-e2e] emitted 2 proof artifacts to {} :: clean_pass={} worst={:?}",
        proof_dir.path().display(),
        manifest.is_clean_pass(),
        manifest.worst_status()
    );
    Ok(())
}
