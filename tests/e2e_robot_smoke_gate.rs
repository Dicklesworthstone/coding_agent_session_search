//! Real-binary smoke gate for critical robot surfaces.
//!
//! Bead `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.2.4`
//! (epic 2 — "Quiet bounded robot commands and archive-capable view").
//!
//! Why this exists
//! ---------------
//! The 2026-06-08 fleet/session report cites **pass-12**, where
//! `cass doctor --json` returned the *agent handbook* instead of doctor
//! output. That bug escaped every generated/golden/unit check because none
//! of them exercised **real dispatch** — they asserted against the doctor
//! emitter directly, never against `cass doctor --json` run as a real
//! process. A surface that dispatches to the wrong emitter is invisible to a
//! golden of the right emitter.
//!
//! This suite closes that hole: it invokes the **real `cass` binary**
//! (`CARGO_BIN_EXE_cass`, provided by cargo for integration tests) across
//! the critical robot surfaces and asserts, per surface:
//!   1. **Valid, *pure* JSON on stdout** — `serde_json::from_str` consumes
//!      the entire trimmed stdout. A diagnostic line leaking onto stdout
//!      breaks the parse (stdout=data / stderr=diagnostics hygiene, the
//!      epic-2 §60 family in `docs/RESILIENCE_TEST_MATRIX.md`).
//!   2. **Correct surface identity** — the parsed object carries that
//!      surface's distinctive top-level keys (e.g. doctor → `checks` +
//!      `doctor_command`; capabilities → `workflows` + `mistake_recoveries`).
//!      This is the direct pass-12 guard: a command that dispatched to the
//!      wrong surface fails the identity-key check.
//!   3. **Stable, kebab-case error kinds (on stderr)** — an errored robot
//!      command keeps stdout empty and emits the
//!      `{error:{code,kind,message,retryable}}` envelope on **stderr** (the
//!      stdout=data / stderr=diagnostics half of the hygiene contract), with a
//!      kebab `kind` from the documented set and the process exit code
//!      mirroring `error.code` (the README exit-code contract).
//!   4. **No bare TUI launch** — stdin is closed (`Stdio::null()`), the
//!      command completes within a bounded timeout (a TUI would block on
//!      input forever → timeout), and stdout carries no ANSI/alt-screen
//!      escape bytes.
//!   5. **Bounded completion** — every invocation runs through
//!      [`spawn_with_timeout_or_diag`], which converts a hang into a loud,
//!      structured `TIMEOUT DIAGNOSTIC` dump rather than a silent stall.
//!
//! When to run / interpreting results (see also `docs/PROOF_RECIPE.md` §7)
//! ----------------------------------------------------------------------
//! * **Routine**: runs under a normal `cargo test` (the surfaces are sub-second
//!   each against an isolated empty data dir). Target it directly with
//!   `cargo test --test e2e_robot_smoke_gate`.
//! * **PASS**: the process exited (not signal-killed) within the bound, stdout
//!   parsed as pure JSON, and the surface identity / error-kind assertions held.
//! * **FAIL (assertion)**: a surface dispatched wrong, leaked stdout, drifted an
//!   error kind, or mis-mirrored an exit code — the returned `Err` names the
//!   surface, the argv, and the offending payload head so a future agent can
//!   debug without rerunning. Every surface is evaluated and logged before the
//!   failure is returned, so one run shows all failing surfaces, not just the
//!   first.
//! * **TIMEOUT (≠ pass, ≠ ordinary fail)**: a surface exceeded the bound — the
//!   `TIMEOUT DIAGNOSTIC` block on stderr (phase, pid, elapsed, data-dir
//!   listing, stdout/stderr tails) distinguishes a hang from a clean failure.
//! * **CI proof artifacts**: set `E2E_LOG=1` to materialize the `.12.3`
//!   structured proof log / artifact manifest via [`PhaseTracker`].
//!
//! Isolation
//! ---------
//! Every invocation runs against a fresh `tempdir` with `HOME` / `XDG_*` /
//! cwd redirected into it and `CASS_IGNORE_SOURCES_CONFIG=1`, so the gate
//! never scans the operator's real session corpus (see the indexer
//! test-isolation note — an un-isolated run scans the real ~500k-session
//! archive and appears to wedge).

mod util;

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};

use util::e2e_log::{E2eError, PhaseTracker};
use util::timeout::spawn_with_timeout_or_diag;

/// Per-surface wall-clock bound. The surfaces are sub-second against an
/// empty isolated data dir; this generous bound only fires on a true hang
/// (e.g. an accidental bare-TUI launch blocking on stdin) and is enforced
/// even under heavy multi-agent host contention.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(60);

/// What a surface's robot payload must look like for the gate to pass.
enum Expect {
    /// A success object that must contain every one of these distinctive
    /// top-level keys (the surface-identity / dispatch-correctness proof).
    Keys(&'static [&'static str]),
    /// State-dependent surface: a success object with `keys`, **or** an
    /// error envelope whose `kind` is in `kinds`. Used where an empty data
    /// dir may legitimately yield either (e.g. `search` fail-open vs
    /// missing-index).
    KeysOrError {
        keys: &'static [&'static str],
        kinds: &'static [&'static str],
    },
    /// Must be an error envelope whose `kind` is in this stable set.
    Error(&'static [&'static str]),
}

struct SmokeSurface {
    name: &'static str,
    args: Vec<String>,
    expect: Expect,
}

/// Build the argv for a subcommand, appending the shared `--data-dir <dir>`
/// tail (placed after the subcommand, matching the existing e2e suites).
fn argv(base: &[&str], data_dir: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    if let Some(dir) = data_dir {
        v.push("--data-dir".to_string());
        v.push(dir.to_string());
    }
    v
}

/// The critical robot surfaces this gate covers. Signatures are pinned
/// against the golden robot JSON under `tests/golden/robot/`.
fn smoke_surfaces(data_dir: &str) -> Vec<SmokeSurface> {
    let dd = Some(data_dir);
    vec![
        // --- Static contract surfaces (state-independent) ---
        SmokeSurface {
            name: "api-version",
            args: argv(&["api-version", "--json"], None),
            expect: Expect::Keys(&["api_version", "contract_version", "crate_version"]),
        },
        SmokeSurface {
            name: "capabilities",
            args: argv(&["capabilities", "--json"], None),
            expect: Expect::Keys(&[
                "version",
                "workflows",
                "mistake_recoveries",
                "commands",
                "exit_codes",
            ]),
        },
        SmokeSurface {
            name: "introspect",
            args: argv(&["introspect", "--json"], None),
            expect: Expect::Keys(&[
                "api_version",
                "commands",
                "response_schemas",
                "global_flags",
            ]),
        },
        // --- Readiness / diagnostic surfaces (empty isolated data dir) ---
        SmokeSurface {
            name: "diag",
            args: argv(&["diag", "--json"], dd),
            expect: Expect::KeysOrError {
                keys: &["paths", "platform", "version"],
                kinds: &["missing-db", "missing-index"],
            },
        },
        SmokeSurface {
            name: "health",
            args: argv(&["health", "--json"], dd),
            expect: Expect::Keys(&["healthy", "health_level", "recommended_action"]),
        },
        SmokeSurface {
            name: "status",
            args: argv(&["status", "--json"], dd),
            expect: Expect::Keys(&["status", "healthy", "health_level", "recommended_action"]),
        },
        SmokeSurface {
            name: "doctor",
            args: argv(&["doctor", "--json"], dd),
            expect: Expect::Keys(&["checks", "auto_fix_applied", "doctor_command"]),
        },
        SmokeSurface {
            name: "triage",
            args: argv(&["triage", "--json"], dd),
            expect: Expect::Keys(&["recommended_commands"]),
        },
        // --- Query surfaces (uninitialized → documented error envelopes) ---
        SmokeSurface {
            name: "search",
            args: argv(
                &[
                    "search",
                    "cass smoke probe alpha",
                    "--robot",
                    "--limit",
                    "3",
                ],
                dd,
            ),
            expect: Expect::KeysOrError {
                keys: &["hits", "query", "total_matches"],
                kinds: &["missing-index", "missing-db"],
            },
        },
        SmokeSurface {
            name: "pack",
            args: argv(&["pack", "cass smoke probe alpha", "--robot"], dd),
            expect: Expect::Error(&["missing-index", "missing-db"]),
        },
        SmokeSurface {
            name: "stats",
            args: argv(&["stats", "--json"], dd),
            expect: Expect::Error(&["missing-db"]),
        },
        SmokeSurface {
            // `view` does not accept a trailing `--data-dir`; the isolated
            // HOME/XDG already redirects its default data dir into the
            // tempdir, and the nonexistent path triggers session-not-found
            // regardless of the (empty) archive.
            name: "view",
            args: argv(
                &[
                    "view",
                    "/nonexistent/cass-smoke-gate/session.jsonl",
                    "-n",
                    "1",
                    "--robot",
                ],
                None,
            ),
            // A missing *direct* path resolves to `file-not-found` (after the
            // archive-row fallback misses); an indexed-source miss yields
            // `session-not-found`. Accept either, plus a bare missing-db.
            expect: Expect::Error(&["file-not-found", "session-not-found", "missing-db"]),
        },
    ]
}

/// Build a `cass` command with the standard test-isolation environment so
/// the gate never reaches the operator's real corpus or config.
fn smoke_command(home: &Path, args: &[String]) -> Command {
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
//     .12.2 "debuggable without rerun" mandate. ---

fn has_escape(bytes: &[u8]) -> bool {
    bytes.contains(&0x1b)
}

/// A valid error-envelope `kind` is non-empty kebab-case: ascii lowercase /
/// digits / single internal hyphens.
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

fn check_success_keys(value: &Value, keys: &[&str], code: i32) -> Result<(), String> {
    let obj = value
        .as_object()
        .ok_or_else(|| format!("expected a JSON object, got: {}", compact(value)))?;
    if obj.contains_key("error") {
        return Err(format!(
            "expected a success surface but got an error envelope: {}",
            compact(value)
        ));
    }
    let missing: Vec<&str> = keys
        .iter()
        .copied()
        .filter(|k| !obj.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "success payload missing required surface-identity keys {missing:?}; present: {:?}. \
             This means the command dispatched to the WRONG surface (the pass-12 class: \
             a command returning some other surface's payload).",
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
    // The README exit-code contract: the process exit code mirrors error.code.
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
/// This is also the epic-2 stdout/stderr-hygiene assertion ("no stray stdout
/// on --robot; logs to stderr only").
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
///
/// Success payloads land on stdout; error envelopes land on stderr (stdout
/// empty). `KeysOrError` surfaces pick the branch by whether stdout carries
/// data.
fn evaluate_surface(surface: &SmokeSurface, out: &Output) -> Result<(), String> {
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
        Expect::Keys(keys) => {
            let value = parse_pure_stdout(stdout_trim, code)?;
            check_success_keys(&value, keys, code)
        }
        Expect::Error(kinds) => check_stderr_error_envelope(out, stdout_trim, kinds, code),
        Expect::KeysOrError { keys, kinds } => {
            if stdout_trim.is_empty() {
                check_stderr_error_envelope(out, stdout_trim, kinds, code)
            } else {
                let value = parse_pure_stdout(stdout_trim, code)?;
                if value.get("error").is_some() {
                    check_error_envelope(&value, kinds, code)
                } else {
                    check_success_keys(&value, keys, code)
                }
            }
        }
    }
}

/// Create an isolated `HOME`/data dir for a smoke invocation. The returned
/// `TempDir` must outlive the commands (RAII cleanup).
fn isolated_home() -> Result<(tempfile::TempDir, std::path::PathBuf), String> {
    let home = tempfile::tempdir().map_err(|e| format!("create tempdir: {e}"))?;
    let data_dir = home.path().join("cass-data");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("create isolated data dir: {e}"))?;
    Ok((home, data_dir))
}

const INCIDENT_PRIVATE_TEXT: &str = "sk_live_CASS_INCIDENT_E2E_ONLY cass health_class degraded database is locked ssh permission denied auth timeout";
const INCIDENT_REMOTE_PATH: &str = "/remote/private/session with ' quote.jsonl";
const INCIDENT_REMOTE_SOURCE: &str = "remote-proof";
const INCIDENT_REMOTE_HOST: &str = "proof-origin";

fn incident_message(content: &str, created_at: i64) -> Message {
    Message {
        id: None,
        idx: 0,
        role: MessageRole::User,
        author: Some("fixture".to_string()),
        created_at: Some(created_at),
        content: content.to_string(),
        extra_json: json!({"fixture": "analytics-incidents-e2e"}),
        snippets: Vec::new(),
    }
}

fn incident_conversation(
    external_id: &str,
    source_path: &str,
    source_id: &str,
    origin_host: Option<&str>,
    started_at: i64,
    content: &str,
) -> Conversation {
    Conversation {
        id: None,
        agent_slug: "codex".to_string(),
        workspace: None,
        external_id: Some(external_id.to_string()),
        title: Some(format!("incident proof {external_id}")),
        source_path: source_path.into(),
        started_at: Some(started_at),
        ended_at: Some(started_at + 1),
        approx_tokens: None,
        metadata_json: json!({"fixture": "analytics-incidents-e2e"}),
        messages: vec![incident_message(content, started_at)],
        source_id: source_id.to_string(),
        origin_host: origin_host.map(str::to_string),
    }
}

fn seed_incident_archive(db_path: &Path) -> Result<(), String> {
    let storage = FrankenStorage::open(db_path)
        .map_err(|error| format!("open FrankenStorage fixture: {error:#}"))?;
    let agent_id = storage
        .ensure_agent(&Agent {
            id: None,
            slug: "codex".to_string(),
            name: "Codex".to_string(),
            version: Some("e2e-fixture".to_string()),
            kind: AgentKind::Cli,
        })
        .map_err(|error| format!("seed incident agent: {error:#}"))?;

    // Insert the older local conversation first. Candidate discovery is
    // intentionally newest-archive-row-first (not timestamp-sorted), so the
    // remote conversation must receive the later canonical row id to make the
    // --max-messages=1 partial proof deterministic.
    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &incident_conversation(
                "incident-local-older",
                "/definitely/missing/local-incident.jsonl",
                "local",
                None,
                1_700_000_000_000,
                "cass index-ingest-out-of-memory quarantined_conversations=1",
            ),
        )
        .map_err(|error| format!("seed local incident conversation: {error:#}"))?;
    storage
        .insert_conversation_tree(
            agent_id,
            None,
            &incident_conversation(
                "incident-remote-newest",
                INCIDENT_REMOTE_PATH,
                INCIDENT_REMOTE_SOURCE,
                Some(INCIDENT_REMOTE_HOST),
                1_800_000_000_000,
                INCIDENT_PRIVATE_TEXT,
            ),
        )
        .map_err(|error| format!("seed remote incident conversation: {error:#}"))?;
    Ok(())
}

fn persist_incident_output(
    tracker: &PhaseTracker,
    label: &str,
    output: &Output,
) -> Result<(), String> {
    let artifact_dir = &tracker.artifacts().dir;
    std::fs::write(
        artifact_dir.join(format!("incidents-{label}.stdout.json")),
        &output.stdout,
    )
    .map_err(|error| format!("write {label} stdout artifact: {error}"))?;
    std::fs::write(
        artifact_dir.join(format!("incidents-{label}.stderr.log")),
        &output.stderr,
    )
    .map_err(|error| format!("write {label} stderr artifact: {error}"))?;
    Ok(())
}

fn parse_incident_success(label: &str, output: &Output) -> Result<Value, String> {
    if output.status.code() != Some(0) {
        return Err(format!(
            "analytics incidents {label} exited {:?}; stdout={} stderr={}",
            output.status.code(),
            head(&String::from_utf8_lossy(&output.stdout)),
            head(&String::from_utf8_lossy(&output.stderr))
        ));
    }
    if has_escape(&output.stdout) {
        return Err(format!(
            "analytics incidents {label} stdout contains an ANSI escape"
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|error| format!("analytics incidents {label} stdout is not UTF-8: {error}"))?;
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|error| {
        format!(
            "analytics incidents {label} stdout is not pure JSON: {error}; stdout head: {}",
            head(stdout)
        )
    })?;
    if value["command"] != "analytics/incidents" {
        return Err(format!(
            "analytics incidents {label} dispatched to the wrong surface: {}",
            compact(&value)
        ));
    }
    Ok(value)
}

/// Per-surface proof-log line. Kept out of the loop body so the live logging
/// allocates off the hot path.
fn log_surface_outcome(
    tracker: &PhaseTracker,
    surface: &SmokeSurface,
    exit: Option<i32>,
    result: &Result<(), String>,
) {
    match result {
        Ok(()) => tracker.verbose(&format!("OK surface={} exit={exit:?}", surface.name)),
        Err(why) => tracker.verbose(&format!("FAIL surface={} {why}", surface.name)),
    }
}

/// A single failure detail line (surface + reason + reproduction argv).
fn failure_detail(surface: &SmokeSurface, why: &str) -> String {
    format!(
        "[{}] {why} (argv: cass {})",
        surface.name,
        surface.args.join(" ")
    )
}

/// The comprehensive gate: every critical robot surface, one real-binary
/// invocation each, all checks applied. Returns `Err` (not a panic) so the
/// proof log records every surface's outcome before the test fails.
#[test]
fn critical_robot_surfaces_dispatch_with_pure_json_and_stable_kinds() -> Result<(), String> {
    let tracker = PhaseTracker::new(
        "e2e_robot_smoke_gate",
        "critical_robot_surfaces_dispatch_with_pure_json_and_stable_kinds",
    );
    let (home, data_dir) = isolated_home()?;
    let data_dir_str = data_dir
        .to_str()
        .ok_or_else(|| "data dir path is not valid UTF-8".to_string())?
        .to_string();

    let surfaces = smoke_surfaces(&data_dir_str);
    let total = surfaces.len();
    let mut failures: Vec<String> = Vec::new();

    for surface in &surfaces {
        let phase = tracker.start(surface.name, Some("real-binary robot surface"));
        let cmd = smoke_command(home.path(), &surface.args);
        // spawn_with_timeout_or_diag emits a TIMEOUT DIAGNOSTIC and aborts on
        // a hang — the explicit "timeout ≠ pass ≠ ordinary fail" signal.
        let out = spawn_with_timeout_or_diag(cmd, surface.name, Some(&data_dir), SMOKE_TIMEOUT);
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
        "{} of {total} critical robot surfaces failed the smoke gate:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
    tracker.fail(E2eError::new(summary.clone()));
    Err(summary)
}

/// Focused pass-12 regression: `cass doctor --json` must dispatch to the
/// doctor emitter, never the agent handbook (capabilities) surface.
#[test]
fn doctor_json_dispatches_to_doctor_not_the_agent_handbook() -> Result<(), String> {
    let (home, data_dir) = isolated_home()?;
    let args = argv(&["doctor", "--json"], data_dir.to_str());

    let cmd = smoke_command(home.path(), &args);
    let out = spawn_with_timeout_or_diag(cmd, "doctor-dispatch", Some(&data_dir), SMOKE_TIMEOUT);

    if has_escape(&out.stdout) {
        return Err(
            "doctor --json stdout carries an ANSI/TUI escape byte (possible bare-TUI launch)"
                .to_string(),
        );
    }
    let stdout = String::from_utf8(out.stdout)
        .map_err(|e| format!("doctor --json stdout not UTF-8: {e}"))?;
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "doctor --json stdout is not pure JSON: {e}; stdout head: {}",
            head(&stdout)
        )
    })?;
    let obj = value.as_object().ok_or_else(|| {
        format!(
            "doctor --json payload must be a JSON object, got: {}",
            compact(&value)
        )
    })?;

    // Positive: doctor's own identity keys must be present (no per-key loop
    // with an inline allocation — collect the missing set first).
    let required = ["checks", "auto_fix_applied", "doctor_command"];
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|k| !obj.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "doctor --json is missing its own identity keys {missing:?}; present: {:?}. \
             This is the pass-12 dispatch regression.",
            present_keys(&value)
        ));
    }

    // Negative: must NOT be the capabilities / agent-handbook surface. Those
    // two keys are capabilities-specific and absent from doctor output.
    if obj.contains_key("workflows") && obj.contains_key("mistake_recoveries") {
        return Err(
            "doctor --json returned the agent handbook / capabilities shape \
             (workflows + mistake_recoveries) instead of doctor output — the pass-12 dispatch bug."
                .to_string(),
        );
    }
    Ok(())
}

/// Real-binary proof for uojcg.10.3: the default incident report is useful but
/// private, provenance-preserving, directly actionable, and truthfully partial
/// when a caller-selected cap stops the archive scan.
#[test]
fn analytics_incidents_redacts_provenance_and_reports_bounded_partial_results() -> Result<(), String>
{
    let tracker = PhaseTracker::new(
        "e2e_robot_smoke_gate",
        "analytics_incidents_redacts_provenance_and_reports_bounded_partial_results",
    );

    let proof = (|| -> Result<(), String> {
        let (home, data_dir) = isolated_home()?;
        let db_path = data_dir.join("agent_search.db");
        let seed_phase = tracker.start(
            "incidents-seed",
            Some("seed a two-conversation canonical FrankenStorage archive"),
        );
        seed_incident_archive(&db_path)?;
        tracker.end("incidents-seed", None, seed_phase);

        let data_dir_str = data_dir
            .to_str()
            .ok_or_else(|| "incident proof data dir is not valid UTF-8".to_string())?;
        let complete_args = argv(
            &[
                "analytics",
                "incidents",
                "--json",
                "--limit",
                "10",
                "--max-sessions",
                "10",
                "--max-messages",
                "100",
                "--max-bytes",
                "1048576",
                "--budget-ms",
                "10000",
            ],
            Some(data_dir_str),
        );
        let complete_phase = tracker.start(
            "incidents-complete",
            Some("run the real binary through a completed bounded scan"),
        );
        let complete_output = spawn_with_timeout_or_diag(
            smoke_command(home.path(), &complete_args),
            "incidents-complete",
            Some(&data_dir),
            SMOKE_TIMEOUT,
        );
        tracker.end("incidents-complete", None, complete_phase);
        persist_incident_output(&tracker, "complete", &complete_output)?;
        let complete = parse_incident_success("complete", &complete_output)?;
        let complete_stdout = String::from_utf8_lossy(&complete_output.stdout);

        if complete_stdout.contains(INCIDENT_PRIVATE_TEXT)
            || complete_stdout.contains("sk_live_CASS_INCIDENT_E2E_ONLY")
        {
            return Err("completed incident report leaked raw private message text".to_string());
        }
        if complete["data"]["schema_version"] != 2
            || complete["data"]["count_scope"] != "all_matching_candidates"
            || complete["data"]["discovery"]["stop_reason"] != "completed"
            || complete["data"]["discovery"]["partial"] != false
            || complete["data"]["discovery"]["files_scanned"] != 2
            || complete["data"]["discovery"]["lines_scanned"] != 2
        {
            return Err(format!(
                "completed incident scan did not report complete two-row accounting: {}",
                compact(&complete["data"])
            ));
        }
        if complete["data"]["redaction"]["private_text_policy"] != "suppress_all"
            || complete["data"]["redaction"]["hash_strategy"] != "blake3_256_v1"
            || !complete["data"]["redaction"]["fields_suppressed"]
                .as_array()
                .is_some_and(|fields| fields.iter().any(|field| field == "raw_prompt_text"))
        {
            return Err(format!(
                "incident redaction manifest is incomplete: {}",
                compact(&complete["data"]["redaction"])
            ));
        }

        let sessions = complete["data"]["top_sessions"]
            .as_array()
            .ok_or_else(|| "completed incident report top_sessions is not an array".to_string())?;
        let remote = sessions
            .iter()
            .find(|session| session["session_id"].as_str() == Some("incident-remote-newest"))
            .ok_or_else(|| {
                format!(
                    "completed incident report omitted remote fixture: {}",
                    compact(&complete["data"]["top_sessions"])
                )
            })?;
        if remote["agent"] != "codex"
            || remote["host"] != INCIDENT_REMOTE_HOST
            || remote["source_id"] != INCIDENT_REMOTE_SOURCE
            || remote["origin_host"] != INCIDENT_REMOTE_HOST
            || remote["source_path"] != INCIDENT_REMOTE_PATH
            || remote["exists_state"] != "unknown"
            || remote["redaction_status"] != "redacted"
        {
            return Err(format!(
                "remote incident provenance/redaction drifted: {}",
                compact(remote)
            ));
        }
        let remote_conversation_id = remote["conversation_id"]
            .as_i64()
            .ok_or_else(|| "remote incident omitted canonical conversation_id".to_string())?;
        let canonical_db_path = std::fs::canonicalize(&db_path)
            .map_err(|error| format!("canonicalize incident database: {error}"))?
            .to_string_lossy()
            .into_owned();
        let expected_argv = json!([
            "cass",
            "--db",
            canonical_db_path,
            "view",
            INCIDENT_REMOTE_PATH,
            "--source",
            INCIDENT_REMOTE_SOURCE,
            "--conversation-id",
            remote_conversation_id.to_string(),
            "--json"
        ]);
        if remote["suggested_command"]["kind"] != "view"
            || remote["suggested_command"]["argv"] != expected_argv
        {
            return Err(format!(
                "incident follow-up action is not the exact safe argv contract: {}",
                compact(&remote["suggested_command"])
            ));
        }
        let suggested_argv = remote["suggested_command"]["argv"]
            .as_array()
            .ok_or_else(|| "incident suggested argv is not an array".to_string())?
            .iter()
            .map(|argument| {
                argument
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "incident suggested argv contains a non-string".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let followup_output = spawn_with_timeout_or_diag(
            smoke_command(home.path(), &suggested_argv[1..]),
            "incidents-suggested-view",
            Some(&data_dir),
            SMOKE_TIMEOUT,
        );
        persist_incident_output(&tracker, "suggested-view", &followup_output)?;
        if !followup_output.status.success() {
            return Err(format!(
                "incident suggested argv failed: status={:?} stderr={}",
                followup_output.status.code(),
                String::from_utf8_lossy(&followup_output.stderr)
            ));
        }
        let followup: Value = serde_json::from_slice(&followup_output.stdout)
            .map_err(|error| format!("incident suggested argv returned invalid JSON: {error}"))?;
        if !compact(&followup).contains("sk_live_CASS_INCIDENT_E2E_ONLY") {
            return Err("incident suggested argv did not open the exact archived row".to_string());
        }
        let evidence = remote["evidence_summaries"]
            .as_array()
            .ok_or_else(|| "remote incident evidence_summaries is not an array".to_string())?;
        let evidence_paths: Vec<&str> = evidence
            .iter()
            .flat_map(|summary| {
                summary["evidence_paths"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
            })
            .collect();
        let fingerprints: Vec<&str> = evidence
            .iter()
            .flat_map(|summary| {
                summary["content_fingerprints"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
            })
            .collect();
        if evidence_paths.is_empty()
            || evidence_paths
                .iter()
                .any(|path| *path != "session with ' quote.jsonl" || path.contains('/'))
            || fingerprints.is_empty()
            || fingerprints
                .iter()
                .any(|fingerprint| fingerprint.len() != 64)
        {
            return Err(format!(
                "incident evidence is not basename-redacted plus fingerprint-only: {}",
                compact(&remote["evidence_summaries"])
            ));
        }

        let partial_args = argv(
            &[
                "analytics",
                "incidents",
                "--json",
                "--limit",
                "10",
                "--max-sessions",
                "10",
                "--max-messages",
                "1",
                "--max-bytes",
                "1048576",
                "--budget-ms",
                "10000",
            ],
            Some(data_dir_str),
        );
        let partial_phase = tracker.start(
            "incidents-partial",
            Some("force a deterministic one-message partial scan through the real binary"),
        );
        let partial_output = spawn_with_timeout_or_diag(
            smoke_command(home.path(), &partial_args),
            "incidents-partial",
            Some(&data_dir),
            SMOKE_TIMEOUT,
        );
        tracker.end("incidents-partial", None, partial_phase);
        persist_incident_output(&tracker, "partial", &partial_output)?;
        let partial = parse_incident_success("partial", &partial_output)?;
        let partial_stdout = String::from_utf8_lossy(&partial_output.stdout);
        if partial_stdout.contains(INCIDENT_PRIVATE_TEXT)
            || partial_stdout.contains("sk_live_CASS_INCIDENT_E2E_ONLY")
        {
            return Err("partial incident report leaked raw private message text".to_string());
        }
        let discovery = &partial["data"]["discovery"];
        if partial["data"]["count_scope"] != "scanned_candidates_partial"
            || discovery["partial"] != true
            || discovery["timed_out"] != false
            || discovery["stop_reason"] != "lines-capped"
            || discovery["caps"]["max_lines"] != 1
            || discovery["lines_scanned"] != 1
            || discovery["files_scanned"] != 1
            || discovery["bytes_scanned"]
                .as_u64()
                .is_none_or(|bytes| bytes > 1_048_576)
        {
            return Err(format!(
                "one-message incident scan did not return truthful bounded partial metadata: {}",
                compact(discovery)
            ));
        }
        let partial_remote = partial["data"]["top_sessions"]
            .as_array()
            .and_then(|items| {
                items.iter().find(|session| {
                    session["session_id"].as_str() == Some("incident-remote-newest")
                })
            })
            .ok_or_else(|| {
                format!(
                    "partial incident report omitted the scanned newest session: {}",
                    compact(&partial["data"]["top_sessions"])
                )
            })?;
        if partial_remote["suggested_command"]["argv"] != expected_argv
            || partial_remote["redaction_status"] != "redacted"
        {
            return Err(format!(
                "partial incident result lost its safe action/redaction contract: {}",
                compact(partial_remote)
            ));
        }
        Ok(())
    })();

    match proof {
        Ok(()) => {
            tracker.complete();
            Ok(())
        }
        Err(error) => {
            tracker.fail(E2eError::new(error.clone()));
            Err(error)
        }
    }
}
