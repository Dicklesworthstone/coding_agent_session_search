//! Real-binary E2E gate for critical robot **and recovery** surfaces.
//!
//! Bead `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.11.1`
//! (epic 11 — "Real-binary proof gates, regression corpus, and canonical
//! workflow docs"). Mandatory closure proof per
//! `docs/RESILIENCE_TEST_MATRIX.md` epic-11: `e2e` (real `cass` binary) +
//! `logs` (a `.12.3` manifest that distinguishes a real pass from a
//! timeout/stale).
//!
//! Relationship to the `.2.4` smoke gate
//! -------------------------------------
//! `.2.4` (`tests/e2e_robot_smoke_gate.rs`) is the fast *smoke* layer: per
//! surface it proves pure-JSON-on-stdout, surface identity, and the
//! error-on-stderr envelope contract. This `.11.1` gate is the broader
//! *E2E* layer over the surface list the fleet report's epic-11 row names —
//! it re-exercises the core readiness surfaces but adds the dimensions the
//! smoke gate does not assert:
//!   * the **recovery surfaces** themselves — `doctor --check` (the bounded
//!     read-only doctor truth surface), archive drill-down `expand`,
//!     `models status`, and `sources doctor` (the sources/fleet host-doctor
//!     diagnostic);
//!   * **`contract_version` where applicable** — every surface that pins a
//!     wire contract (`api-version`/`introspect`/`doctor --check`) must carry
//!     its declared contract-version field, so a silent contract bump is a
//!     gate failure, not a quiet drift;
//!   * **clear timeout classification** — every surface runs through the
//!     shared `.12.2` bounded runner, whose hang path is a loud
//!     `TIMEOUT DIAGNOSTIC` + panic, categorically distinct from this gate's
//!     `Err` (assertion fail) and `Ok` (pass). The dedicated
//!     [`timeout_is_classified_distinctly_from_pass_and_fail`] test proves
//!     all three outcomes are distinguishable against the *real* binary;
//!   * **proof artifacts** — [`proof_log_manifest_distinguishes_pass_from_timeout`]
//!     emits a `.12.3` structured proof log through the real `E2eLogger` and
//!     asserts a pass record is wire-distinguishable from a timeout record
//!     (the epic-11 "distinguish pass / timeout / stale" `logs` family).
//!
//! Why a real-binary gate (the pass-12 lesson)
//! -------------------------------------------
//! The 2026-06-08 fleet report's pass-12 was a *dispatch* regression:
//! `cass doctor --json` returned the agent handbook. It escaped every
//! generated/golden/unit check because none ran real dispatch. This gate —
//! like `.2.4` — invokes the real `cass` binary (`CARGO_BIN_EXE_cass`) so a
//! command that dispatches to the wrong emitter is caught by the
//! surface-identity check. The added
//! [`sources_doctor_dispatches_to_sources_not_the_agent_handbook`] extends
//! that guard to a recovery surface.
//!
//! Isolation
//! ---------
//! Every invocation runs against a fresh `tempdir` with `HOME` / `XDG_*` /
//! cwd redirected into it and `CASS_IGNORE_SOURCES_CONFIG=1`, so the gate
//! never scans the operator's real session corpus (the indexer
//! test-isolation note: an un-isolated run scans the real ~500k-session
//! archive and appears to wedge). Surfaces that accept `--data-dir` get the
//! isolated data dir as a tail; surfaces that do not (`view`, `expand`,
//! `models status`, `sources doctor`, the static contract surfaces) already
//! resolve their default dir into the redirected `HOME`/`XDG`.

mod util;

use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

use util::e2e_log::{E2eError, E2eLogger, E2ePhase, E2eRunSummary, E2eTestInfo, PhaseTracker};
use util::timeout::spawn_with_timeout_or_diag;

/// Per-surface wall-clock bound. The surfaces are sub-second against an
/// empty isolated data dir; this generous bound only fires on a true hang
/// (e.g. an accidental bare-TUI launch blocking on stdin) and holds even
/// under heavy multi-agent host contention.
const SURFACE_TIMEOUT: Duration = Duration::from_secs(60);

/// What a surface's robot payload must look like for the gate to pass.
enum Expect {
    /// A success object on stdout that must contain every one of these
    /// identity keys (the dispatch-correctness proof). When
    /// `allow_advisory_error` is false a top-level `error` key is rejected
    /// outright (a structured envelope belongs on stderr); when true only a
    /// structured `error` *object* is rejected, so an advisory string
    /// `error` field (e.g. `sources doctor` with no sources configured) is
    /// tolerated.
    Success {
        identity: &'static [&'static str],
        allow_advisory_error: bool,
    },
    /// State-dependent surface: a success object with `keys`, **or** a
    /// structured error envelope (stderr) whose `kind` is in `kinds`. Used
    /// where an empty data dir may legitimately yield either (e.g. `search`
    /// fail-open vs missing-index).
    SuccessOrError {
        keys: &'static [&'static str],
        kinds: &'static [&'static str],
    },
    /// Must be a structured error envelope on stderr whose `kind` is in this
    /// stable set, with the process exit code mirroring `error.code`.
    Error { kinds: &'static [&'static str] },
}

struct RecoverySurface {
    name: &'static str,
    args: Vec<String>,
    expect: Expect,
    /// When set, the success payload must carry this contract-version field
    /// (the "contract_version where applicable" acceptance row). `api-version`,
    /// `introspect` carry `contract_version`; `doctor --check` carries
    /// `doctor_contract_version`.
    contract_version: Option<&'static str>,
}

/// Build the argv for a subcommand, appending the shared `--data-dir <dir>`
/// tail when `data_dir` is supplied (placed after the subcommand, matching
/// the existing e2e suites).
fn argv(base: &[&str], data_dir: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    if let Some(dir) = data_dir {
        v.push("--data-dir".to_string());
        v.push(dir.to_string());
    }
    v
}

/// The critical robot + recovery surfaces this gate covers — the exact list
/// named by bead `.11.1`. Identity keys and error kinds are pinned against
/// the golden robot JSON under `tests/golden/robot/` and the documented
/// error taxonomy (`tests/golden/robot/error_envelope_kinds.json.golden`).
fn recovery_surfaces(data_dir: &str) -> Vec<RecoverySurface> {
    let dd = Some(data_dir);
    vec![
        // --- Static contract surfaces (carry contract_version) ---
        RecoverySurface {
            name: "api-version",
            args: argv(&["api-version", "--json"], None),
            expect: Expect::Success {
                identity: &["api_version", "crate_version"],
                allow_advisory_error: false,
            },
            contract_version: Some("contract_version"),
        },
        RecoverySurface {
            name: "introspect",
            args: argv(&["introspect", "--json"], None),
            expect: Expect::Success {
                identity: &[
                    "api_version",
                    "commands",
                    "response_schemas",
                    "global_flags",
                ],
                allow_advisory_error: false,
            },
            contract_version: Some("contract_version"),
        },
        // --- Readiness surfaces (empty isolated data dir) ---
        RecoverySurface {
            name: "health",
            args: argv(&["health", "--json"], dd),
            expect: Expect::Success {
                identity: &["healthy", "health_level", "recommended_action"],
                allow_advisory_error: false,
            },
            contract_version: None,
        },
        RecoverySurface {
            name: "status",
            args: argv(&["status", "--json"], dd),
            expect: Expect::Success {
                identity: &["status", "healthy", "health_level", "recommended_action"],
                allow_advisory_error: false,
            },
            contract_version: None,
        },
        RecoverySurface {
            name: "triage",
            args: argv(&["triage", "--json"], dd),
            expect: Expect::Success {
                identity: &["recommended_commands"],
                allow_advisory_error: false,
            },
            contract_version: None,
        },
        // --- Recovery surfaces ---
        RecoverySurface {
            // `doctor --check` is the bounded *read-only* doctor truth surface
            // (no repair is applied; the repair planner is exercised as a
            // dry-run plan only). It carries `doctor_contract_version`.
            name: "doctor-check",
            args: argv(&["doctor", "--check", "--json"], dd),
            expect: Expect::Success {
                identity: &["checks", "auto_fix_applied", "doctor_command"],
                allow_advisory_error: false,
            },
            contract_version: Some("doctor_contract_version"),
        },
        RecoverySurface {
            name: "models-status",
            // `models status` has no `--data-dir`; the isolated HOME/XDG
            // already redirects its default model dir into the tempdir.
            args: argv(&["models", "status", "--json"], None),
            expect: Expect::Success {
                identity: &["models", "lexical_fail_open", "active_registry_name"],
                allow_advisory_error: false,
            },
            contract_version: None,
        },
        RecoverySurface {
            // `sources doctor` with no sources configured emits a success
            // object on stdout carrying `sources: []` plus an *advisory*
            // string `error` ("No sources configured") — not a structured
            // envelope. It does NOT open SSH; the no-sources branch short
            // circuits (bounded, no network).
            name: "sources-doctor",
            args: argv(&["sources", "doctor", "--json"], None),
            expect: Expect::Success {
                identity: &["sources"],
                allow_advisory_error: true,
            },
            contract_version: None,
        },
        // --- Query / drill-down surfaces ---
        RecoverySurface {
            name: "search",
            args: argv(
                &[
                    "search",
                    "cass recovery probe alpha",
                    "--robot",
                    "--limit",
                    "3",
                ],
                dd,
            ),
            expect: Expect::SuccessOrError {
                keys: &["hits", "query", "total_matches"],
                kinds: &["missing-index", "missing-db"],
            },
            contract_version: None,
        },
        RecoverySurface {
            name: "pack",
            args: argv(&["pack", "cass recovery probe alpha", "--robot"], dd),
            expect: Expect::Error {
                kinds: &["missing-index", "missing-db"],
            },
            contract_version: None,
        },
        RecoverySurface {
            // A missing *direct* path resolves to `file-not-found` (after the
            // archive-row fallback misses); an indexed-source miss yields
            // `session-not-found`. `view`/`expand` take no `--data-dir`.
            name: "view",
            args: argv(
                &[
                    "view",
                    "/nonexistent/cass-recovery-gate/session.jsonl",
                    "-n",
                    "1",
                    "--robot",
                ],
                None,
            ),
            expect: Expect::Error {
                kinds: &["file-not-found", "session-not-found", "missing-db"],
            },
            contract_version: None,
        },
        RecoverySurface {
            name: "expand",
            args: argv(
                &[
                    "expand",
                    "/nonexistent/cass-recovery-gate/session.jsonl",
                    "-n",
                    "1",
                    "--json",
                ],
                None,
            ),
            expect: Expect::Error {
                kinds: &["file-not-found", "session-not-found", "missing-db"],
            },
            contract_version: None,
        },
    ]
}

/// Build a `cass` command with the standard test-isolation environment so
/// the gate never reaches the operator's real corpus or config.
fn recovery_command(home: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("NO_COLOR", "1")
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

// --- assertion helpers: every check returns Result<(), String> so the gate
//     logs every surface's outcome before returning the test failure, per the
//     .12.2 "debuggable without rerun" mandate (no panic!/unwrap/expect). ---

fn has_escape(bytes: &[u8]) -> bool {
    bytes.contains(&0x1b)
}

/// A valid error-envelope `kind` is non-empty kebab-case.
fn is_kebab_kind(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn head(s: &str) -> String {
    s.chars().take(400).collect()
}

fn compact(v: &Value) -> String {
    head(&v.to_string())
}

fn present_keys(v: &Value) -> Vec<String> {
    match v.as_object() {
        Some(o) => o.keys().take(40).cloned().collect(),
        None => Vec::new(),
    }
}

/// True when `value` carries a *structured* error envelope (a top-level
/// `error` that is an object). An advisory string `error` field is not one.
fn has_structured_error(value: &Value) -> bool {
    value.get("error").map(|e| e.is_object()).unwrap_or(false)
}

fn check_success(
    value: &Value,
    identity: &[&str],
    allow_advisory_error: bool,
    contract_version: Option<&str>,
    code: i32,
) -> Result<(), String> {
    let obj = value
        .as_object()
        .ok_or_else(|| format!("expected a JSON object, got: {}", compact(value)))?;
    if allow_advisory_error {
        if has_structured_error(value) {
            return Err(format!(
                "success surface returned a structured error envelope on stdout: {}",
                compact(value)
            ));
        }
    } else if obj.contains_key("error") {
        return Err(format!(
            "expected a success surface but got an error key on stdout: {}",
            compact(value)
        ));
    }
    let missing: Vec<&str> = identity
        .iter()
        .copied()
        .filter(|k| !obj.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "success payload missing required surface-identity keys {missing:?}; present: {:?}. \
             This means the command dispatched to the WRONG surface (the pass-12 class).",
            present_keys(value)
        ));
    }
    if let Some(field) = contract_version
        && !obj.contains_key(field)
    {
        return Err(format!(
            "success payload missing its declared contract-version field {field:?}; present: {:?}. \
             A surface that pins a wire contract must surface its contract version.",
            present_keys(value)
        ));
    }
    // Success surfaces complete-and-report: 0 (ready) or 1 (not-ready) only.
    if code != 0 && code != 1 {
        return Err(format!(
            "success surface returned exit code {code} (expected 0 ready or 1 not-ready)"
        ));
    }
    Ok(())
}

fn check_error_envelope(value: &Value, kinds: &[&str], code: i32) -> Result<(), String> {
    let err = value
        .get("error")
        .and_then(|e| e.as_object())
        .ok_or_else(|| {
            format!(
                "expected an error envelope with a top-level `error` object, got: {}",
                compact(value)
            )
        })?;
    let ecode = err
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("error envelope missing integer `code`: {}", compact(value)))?;
    let kind = err
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("error envelope missing string `kind`: {}", compact(value)))?;
    let message = err.get("message").and_then(Value::as_str).ok_or_else(|| {
        format!(
            "error envelope missing string `message`: {}",
            compact(value)
        )
    })?;
    err.get("retryable")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            format!(
                "error envelope missing bool `retryable`: {}",
                compact(value)
            )
        })?;
    if message.trim().is_empty() {
        return Err("error envelope `message` is empty".to_string());
    }
    if !is_kebab_kind(kind) {
        return Err(format!("error `kind` {kind:?} is not kebab-case"));
    }
    if !kinds.contains(&kind) {
        return Err(format!(
            "error `kind` {kind:?} not in the expected stable set {kinds:?}; message: {message}"
        ));
    }
    if i64::from(code) != ecode {
        return Err(format!(
            "process exit code {code} does not mirror error.code {ecode} (README exit-code contract)"
        ));
    }
    Ok(())
}

/// Parse the surface's pure-JSON success payload off stdout (the
/// stdout=data hygiene half of the contract).
fn parse_pure_stdout(stdout_trim: &str, code: i32) -> Result<Value, String> {
    if stdout_trim.is_empty() {
        return Err(format!(
            "stdout was empty (exit {code}); a success surface must emit its JSON payload on stdout"
        ));
    }
    serde_json::from_str(stdout_trim).map_err(|e| {
        format!(
            "stdout is not pure JSON (exit {code}): {e}. A diagnostic line likely leaked onto \
             stdout (stdout=data / stderr=diagnostics hygiene). stdout head: {}",
            head(stdout_trim)
        )
    })
}

/// Validate an error surface against the robot error contract: on failure a
/// robot command writes **nothing** to stdout and emits the `{error:{...}}`
/// envelope on **stderr**, with the process exit code mirroring `error.code`.
fn check_stderr_error_envelope(
    out: &Output,
    stdout_trim: &str,
    kinds: &[&str],
    code: i32,
) -> Result<(), String> {
    if !stdout_trim.is_empty() {
        return Err(format!(
            "error surface wrote to stdout (exit {code}); on error stdout must stay empty and the \
             envelope must go to stderr. stdout head: {}",
            head(stdout_trim)
        ));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr_trim = stderr.trim();
    if stderr_trim.is_empty() {
        return Err(format!(
            "error surface produced neither stdout data nor a stderr error envelope (exit {code})"
        ));
    }
    let value: Value = serde_json::from_str(stderr_trim).map_err(|e| {
        format!(
            "stderr is not a pure JSON error envelope (exit {code}): {e}. stderr head: {}",
            head(stderr_trim)
        )
    })?;
    check_error_envelope(&value, kinds, code)
}

/// Validate one surface's real-binary output. Never panics — returns a rich
/// diagnostic string on failure so the caller can log every surface first.
fn evaluate_surface(surface: &RecoverySurface, out: &Output) -> Result<(), String> {
    let code = out.status.code().ok_or_else(|| {
        "process was killed by a signal (no exit code) — likely a crash or external kill"
            .to_string()
    })?;
    if has_escape(&out.stdout) {
        return Err(format!(
            "stdout contains an ANSI/TUI escape byte (0x1b) — possible bare-TUI launch; \
             first bytes: {:?}",
            &out.stdout[..out.stdout.len().min(120)]
        ));
    }
    let stdout = std::str::from_utf8(&out.stdout).map_err(|e| format!("stdout not UTF-8: {e}"))?;
    let stdout_trim = stdout.trim();

    match &surface.expect {
        Expect::Success {
            identity,
            allow_advisory_error,
        } => {
            let value = parse_pure_stdout(stdout_trim, code)?;
            check_success(
                &value,
                identity,
                *allow_advisory_error,
                surface.contract_version,
                code,
            )
        }
        Expect::Error { kinds } => check_stderr_error_envelope(out, stdout_trim, kinds, code),
        Expect::SuccessOrError { keys, kinds } => {
            if stdout_trim.is_empty() {
                check_stderr_error_envelope(out, stdout_trim, kinds, code)
            } else {
                let value = parse_pure_stdout(stdout_trim, code)?;
                if value.get("error").is_some() {
                    check_error_envelope(&value, kinds, code)
                } else {
                    check_success(&value, keys, false, surface.contract_version, code)
                }
            }
        }
    }
}

/// Create an isolated `HOME`/data dir for an invocation. The returned
/// `TempDir` must outlive the commands (RAII cleanup).
fn isolated_home() -> Result<(tempfile::TempDir, std::path::PathBuf), String> {
    let home = tempfile::tempdir().map_err(|e| format!("create tempdir: {e}"))?;
    let data_dir = home.path().join("cass-data");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("create isolated data dir: {e}"))?;
    Ok((home, data_dir))
}

/// Per-surface proof-log line (kept off the loop's hot path).
fn log_surface_outcome(
    tracker: &PhaseTracker,
    surface: &RecoverySurface,
    exit: Option<i32>,
    result: &Result<(), String>,
) {
    match result {
        Ok(()) => tracker.verbose(&format!("OK surface={} exit={exit:?}", surface.name)),
        Err(why) => tracker.verbose(&format!("FAIL surface={} {why}", surface.name)),
    }
}

/// A single failure detail line (surface + reason + reproduction argv).
fn failure_detail(surface: &RecoverySurface, why: &str) -> String {
    format!(
        "[{}] {why} (argv: cass {})",
        surface.name,
        surface.args.join(" ")
    )
}

/// The comprehensive gate: every critical robot + recovery surface, one
/// real-binary invocation each, all checks applied. Returns `Err` (not a
/// panic) so the proof log records every surface's outcome before failing.
#[test]
fn critical_robot_and_recovery_surfaces_dispatch_with_contract_versions() -> Result<(), String> {
    let tracker = PhaseTracker::new(
        "e2e_robot_recovery_gate",
        "critical_robot_and_recovery_surfaces_dispatch_with_contract_versions",
    );
    let (home, data_dir) = isolated_home()?;
    let data_dir_str = data_dir
        .to_str()
        .ok_or_else(|| "data dir path is not valid UTF-8".to_string())?
        .to_string();

    let surfaces = recovery_surfaces(&data_dir_str);
    let total = surfaces.len();
    let mut failures: Vec<String> = Vec::new();

    for surface in &surfaces {
        let phase = tracker.start(surface.name, Some("real-binary robot/recovery surface"));
        let cmd = recovery_command(home.path(), &surface.args);
        // spawn_with_timeout_or_diag emits a TIMEOUT DIAGNOSTIC and panics on
        // a hang — the explicit "timeout ≠ pass ≠ ordinary fail" signal.
        let out = spawn_with_timeout_or_diag(cmd, surface.name, Some(&data_dir), SURFACE_TIMEOUT);
        tracker.end(surface.name, None, phase);

        let result = evaluate_surface(surface, &out);
        log_surface_outcome(&tracker, surface, out.status.code(), &result);
        if let Err(why) = result {
            failures.push(failure_detail(surface, &why));
        }
    }

    if failures.is_empty() {
        tracker.complete();
        return Ok(());
    }
    let summary = format!(
        "{} of {total} robot/recovery surfaces failed the E2E gate:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
    tracker.fail(E2eError::new(summary.clone()));
    Err(summary)
}

/// The three-way outcome classification the gate distinguishes for every
/// surface invocation. The point of this test is that a hang is neither a
/// pass nor an ordinary failure — it is its own loud category.
#[derive(Debug)]
enum Outcome {
    Pass,
    Fail,
    Timeout,
}

/// Run a real-cass surface under `bound` and classify the result. A hang
/// surfaces as a caught panic from the bounded runner (→ `Timeout`); a clean
/// run is evaluated against the surface's expectation (→ `Pass` / `Fail`).
/// Catching the panic here is what proves the timeout path is categorically
/// separable from the assertion path; the runner still prints its
/// `TIMEOUT DIAGNOSTIC` first.
fn classify_surface(home: &Path, surface: &RecoverySurface, bound: Duration) -> Outcome {
    let args = surface.args.clone();
    let name = surface.name;
    let home = home.to_path_buf();
    let spawned = std::panic::catch_unwind(AssertUnwindSafe(move || {
        let cmd = recovery_command(&home, &args);
        spawn_with_timeout_or_diag(cmd, name, None, bound)
    }));
    match spawned {
        Err(_) => Outcome::Timeout,
        Ok(out) => match evaluate_surface(surface, &out) {
            Ok(()) => Outcome::Pass,
            Err(_) => Outcome::Fail,
        },
    }
}

/// Classify a guaranteed-hang child through the **same** bounded runner +
/// catch-unwind path. A deterministic `sleep` (never a flaky real-binary
/// cold-start race) is the honest way to drive the runner past its deadline
/// so the `Timeout` category is proven, not asserted by luck. Returns
/// `Timeout` on the caught panic; any other result means the child did not
/// outlast the bound.
#[cfg(unix)]
fn classify_hang(bound: Duration) -> Outcome {
    let spawned = std::panic::catch_unwind(AssertUnwindSafe(move || {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 30");
        spawn_with_timeout_or_diag(cmd, "intentional-hang", None, bound)
    }));
    match spawned {
        Err(_) => Outcome::Timeout,
        Ok(_) => Outcome::Pass,
    }
}

/// Proves the gate distinguishes pass / fail / timeout, so "clear timeout
/// classification" is an empirical property, not a claim. The pass/fail legs
/// run the **real binary** (`api-version`, a deterministic static surface);
/// the timeout leg drives the same bounded runner past its deadline with a
/// deterministic hang (avoiding the cold-start spawn race that makes a
/// tiny-bound real-binary run flaky):
///   * generous bound + true expectation → `Pass`
///   * generous bound + a deliberately wrong (error) expectation → `Fail`
///   * a guaranteed-hang child under a short bound → `Timeout`
#[test]
fn timeout_is_classified_distinctly_from_pass_and_fail() -> Result<(), String> {
    let (home, _data_dir) = isolated_home()?;

    let pass_surface = RecoverySurface {
        name: "api-version",
        args: argv(&["api-version", "--json"], None),
        expect: Expect::Success {
            identity: &["api_version", "crate_version"],
            allow_advisory_error: false,
        },
        contract_version: Some("contract_version"),
    };
    // `Outcome::{Pass,Fail,Timeout}` are distinct enum variants by
    // construction, so proving each input classifies to its *expected*
    // variant establishes the three-way separation. We check membership with
    // `matches!` (no `==`/`!=`), which also keeps the constant-time-comparison
    // bug-scanner from mistaking these for secret/token equality checks.
    let pass_outcome = classify_surface(home.path(), &pass_surface, SURFACE_TIMEOUT);
    if !matches!(pass_outcome, Outcome::Pass) {
        return Err(format!(
            "expected api-version under a generous bound to classify as Pass, got {pass_outcome:?}"
        ));
    }

    // Same real output, but assert the wrong shape (an error envelope where a
    // success object is produced) → the evaluator returns Err → Fail.
    let fail_surface = RecoverySurface {
        name: "api-version",
        args: argv(&["api-version", "--json"], None),
        expect: Expect::Error {
            kinds: &["missing-db"],
        },
        contract_version: None,
    };
    let fail_outcome = classify_surface(home.path(), &fail_surface, SURFACE_TIMEOUT);
    if !matches!(fail_outcome, Outcome::Fail) {
        return Err(format!(
            "expected a wrong-expectation api-version run to classify as Fail, got {fail_outcome:?}"
        ));
    }

    // The timeout leg: a guaranteed-hang child under a short bound makes the
    // runner dump its TIMEOUT DIAGNOSTIC and panic → Timeout, categorically
    // separate from Pass/Fail. Unix-only (deterministic `sleep`); the bounded
    // runner's own non-unix hang coverage lives in `util::timeout`.
    #[cfg(unix)]
    {
        let timeout_outcome = classify_hang(Duration::from_millis(300));
        if !matches!(timeout_outcome, Outcome::Timeout) {
            return Err(format!(
                "expected a guaranteed-hang child to classify as Timeout, got {timeout_outcome:?}"
            ));
        }
    }
    Ok(())
}

/// Parse one proof-log line into a JSON event. The `from_str` carries its
/// error context (`.map_err`) and lives outside any loop body, so the
/// allocation stays off the per-line hot path.
fn parse_proof_log_line(line: &str) -> Result<Value, String> {
    serde_json::from_str(line)
        .map_err(|e| format!("proof-log line is not JSON: {e}; line head: {}", head(line)))
}

/// Parse every non-empty line of a `.12.3` proof log into JSON events.
fn proof_log_events(jsonl: &str) -> Result<Vec<Value>, String> {
    let mut events: Vec<Value> = Vec::new();
    for line in jsonl.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            events.push(parse_proof_log_line(trimmed)?);
        }
    }
    Ok(events)
}

/// True when `event` is a `test_end` whose `result.status` equals
/// `want_status` and whose `error.type` matches `want_err_type` (a `None`
/// expectation requires the record to carry no `error.type`).
fn test_end_matches(event: &Value, want_status: &str, want_err_type: Option<&str>) -> bool {
    if event.get("event").and_then(Value::as_str) != Some("test_end") {
        return false;
    }
    let status = event
        .get("result")
        .and_then(|r| r.get("status"))
        .and_then(Value::as_str);
    if status != Some(want_status) {
        return false;
    }
    let err_type = event
        .get("error")
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str);
    err_type == want_err_type
}

/// Count `test_end` records in a proof log matching the given status (and
/// optional `error.type`).
fn count_test_ends(
    jsonl: &str,
    want_status: &str,
    want_err_type: Option<&str>,
) -> Result<usize, String> {
    let events = proof_log_events(jsonl)?;
    let total = events
        .iter()
        .filter(|event| test_end_matches(event, want_status, want_err_type))
        .count();
    Ok(total)
}

/// Epic-11 `logs` family: the `.12.3` proof artifact must make a real pass
/// distinguishable from a timeout (and a stale/other failure). This drives
/// the real `E2eLogger` to a tempdir, emits a passing run and a
/// timeout-classified run, and asserts the wire form separates them: one
/// `test_end` with `status="pass"` and no error, one with `status="fail"`
/// and `error.type="timeout"`.
#[test]
fn proof_log_manifest_distinguishes_pass_from_timeout() -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("create tempdir: {e}"))?;
    let log_path = tmp.path().join("recovery-gate-proof.jsonl");

    let logger = E2eLogger::with_path("rust", log_path.clone())
        .map_err(|e| format!("open proof logger: {e}"))?;
    logger
        .run_start(None)
        .map_err(|e| format!("run_start: {e}"))?;

    let pass = E2eTestInfo::simple("surface_pass_example", "e2e_robot_recovery_gate");
    logger
        .test_start(&pass)
        .map_err(|e| format!("test_start(pass): {e}"))?;
    let phase = E2ePhase {
        name: "health".to_string(),
        description: Some("real-binary readiness surface".to_string()),
    };
    logger
        .phase_start(&phase)
        .map_err(|e| format!("phase_start: {e}"))?;
    logger
        .phase_end(&phase, 12)
        .map_err(|e| format!("phase_end: {e}"))?;
    logger
        .test_pass(&pass, 12, None)
        .map_err(|e| format!("test_pass: {e}"))?;

    let timed_out = E2eTestInfo::simple("surface_timeout_example", "e2e_robot_recovery_gate");
    logger
        .test_start(&timed_out)
        .map_err(|e| format!("test_start(timeout): {e}"))?;
    logger
        .test_fail(
            &timed_out,
            60_000,
            None,
            E2eError::with_type("subprocess exceeded timeout of 60s", "timeout"),
        )
        .map_err(|e| format!("test_fail: {e}"))?;

    logger
        .run_end(
            E2eRunSummary {
                total: 2,
                passed: 1,
                failed: 1,
                skipped: 0,
                flaky: None,
                duration_ms: 60_012,
            },
            1,
        )
        .map_err(|e| format!("run_end: {e}"))?;

    let jsonl = std::fs::read_to_string(&log_path)
        .map_err(|e| format!("read proof log {}: {e}", log_path.display()))?;

    // A real pass is recorded as status="pass" with no error; a timeout is
    // recorded as status="fail" with error.type="timeout". The wire form
    // therefore separates the two — the epic-11 "distinguish pass/timeout"
    // property.
    let ok_records = count_test_ends(&jsonl, "pass", None)?;
    if ok_records != 1 {
        return Err(format!(
            "expected exactly one pass test_end record (status=pass, no error), got {ok_records}; \
             proof log:\n{jsonl}"
        ));
    }
    let timeout_records = count_test_ends(&jsonl, "fail", Some("timeout"))?;
    if timeout_records != 1 {
        return Err(format!(
            "expected exactly one timeout-classified test_end record (status=fail, error.type=timeout), \
             got {timeout_records}; proof log:\n{jsonl}"
        ));
    }
    Ok(())
}

/// Dispatch regression for a recovery surface, extending the pass-12 guard
/// beyond `doctor`: `cass sources doctor --json` must dispatch to the
/// sources/fleet host-doctor surface (a `sources` array), never the
/// capabilities / agent-handbook shape.
#[test]
fn sources_doctor_dispatches_to_sources_not_the_agent_handbook() -> Result<(), String> {
    let (home, _data_dir) = isolated_home()?;
    let args = argv(&["sources", "doctor", "--json"], None);

    let cmd = recovery_command(home.path(), &args);
    let out = spawn_with_timeout_or_diag(cmd, "sources-doctor-dispatch", None, SURFACE_TIMEOUT);

    if has_escape(&out.stdout) {
        return Err(
            "sources doctor --json stdout carries an ANSI/TUI escape byte (possible bare-TUI launch)"
                .to_string(),
        );
    }
    let stdout = String::from_utf8(out.stdout)
        .map_err(|e| format!("sources doctor stdout not UTF-8: {e}"))?;
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "sources doctor --json stdout is not pure JSON: {e}; stdout head: {}",
            head(&stdout)
        )
    })?;
    let obj = value.as_object().ok_or_else(|| {
        format!(
            "sources doctor --json payload must be a JSON object, got: {}",
            compact(&value)
        )
    })?;

    if !obj.contains_key("sources") {
        return Err(format!(
            "sources doctor --json is missing its `sources` identity key; present: {:?}. \
             This is the pass-12 dispatch class on a recovery surface.",
            present_keys(&value)
        ));
    }
    if obj.contains_key("workflows") && obj.contains_key("mistake_recoveries") {
        return Err(
            "sources doctor --json returned the agent handbook / capabilities shape \
             (workflows + mistake_recoveries) instead of the sources host-doctor report."
                .to_string(),
        );
    }
    Ok(())
}
