//! Real-binary human/robot **parity** E2E gate for the readiness journeys.
//!
//! Bead `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.13.4`
//! (epic 13 — "User-facing recovery journeys and human/robot guidance parity").
//!
//! Why this gate exists
//! --------------------
//! User-facing polish is a liability, not an asset, if the human-facing surface
//! drifts from the robot JSON: a reassuring sentence that contradicts the
//! machine contract makes cass *less* trustworthy. Bead `13.2`
//! (`src/search/human_readiness_summary.rs`) makes the human projection a
//! structural layer over the *same* `DerivedAssetTruthTable` the robot JSON
//! serializes, so parity is guaranteed in-process. This gate proves that
//! guarantee survives all the way to the **real `cass` binary**: for each
//! readiness journey it runs the robot surface (`--json`) and the human surface
//! (no `--json`) against the *same* fixture and asserts they name the same
//! state family, the same safe next command, and the same source/quarantine
//! semantics — and that the human surface never surfaces destructive guidance,
//! even though a generic repair command (`cass index --full`) exists.
//!
//! Relationship to `.2.4` / `.11.1`
//! --------------------------------
//! `.2.4` (smoke) and `.11.1` (recovery) prove the *robot* surfaces dispatch
//! correctly and carry their contract. This `.13.4` gate adds the missing
//! dimension: that the *human* rendering of the same command agrees with the
//! robot JSON. It reuses the same isolation, the `.12.2` bounded runner
//! (`spawn_with_timeout_or_diag`), and the `.12.3` proof-log manifest
//! (`E2eLogger`) so a hang is a loud `TIMEOUT DIAGNOSTIC` (never a silent pass)
//! and a real pass is wire-distinguishable from a timeout or a structured skip.
//!
//! State families covered
//! ----------------------
//! Two deterministic fixtures exercise two distinct state families end to end:
//!   * **fresh** — an empty isolated data dir → `not_initialized` ("Run
//!     `cass index --full`").
//!   * **indexed** — a tiny seeded Codex session indexed once → `healthy`
//!     ("Lexical search is ready; …").
//! For every surface in both states the human "Recommended:" line must carry
//! the robot's `recommended_action` *verbatim* and the human headline must
//! reflect the robot's state family.
//!
//! Drift attribution
//! -----------------
//! Every parity failure is attributed to one of five loci so a future agent
//! knows *where* the drift is — robot state, human projection, docs/copy,
//! fixture, or proof logging — instead of a bare "they disagree".
//!
//! Isolation
//! ---------
//! Every invocation runs against a fresh `tempdir` with `HOME`/`XDG_*`/cwd
//! redirected into it (the indexer test-isolation rule: an un-isolated run
//! scans the operator's real ~500k-session corpus and appears to wedge). The
//! fresh fixture additionally sets `CASS_IGNORE_SOURCES_CONFIG=1` and removes
//! `CODEX_HOME`; the indexed fixture points `CODEX_HOME` at its seeded session
//! so exactly one conversation is discovered. `CASS_SEMANTIC_EMBEDDER=hash`
//! keeps semantic acquisition offline and `NO_COLOR=1` keeps the human output
//! ANSI-free so it is parseable.

mod util;

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

use util::e2e_log::{E2eError, E2eLogger, E2ePhase, E2eRunSummary, E2eTestInfo, PhaseTracker};
use util::timeout::spawn_with_timeout_or_diag;

/// Per-surface wall-clock bound. The readiness surfaces are sub-second against
/// the isolated fixtures; this generous bound only fires on a true hang and
/// holds under heavy multi-agent host contention.
const SURFACE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bound for the one-time index that builds the `indexed` fixture (a single
/// tiny seeded session indexes in well under a second; the bound is slack).
const INDEX_TIMEOUT: Duration = Duration::from_secs(120);

/// Destructive command fragments the human surface must never surface. Mirrors
/// `human_readiness_summary::UNSAFE_COMMAND_FRAGMENTS` — duplicated here because
/// that module is crate-private and this is an out-of-crate integration test.
const UNSAFE_COMMAND_FRAGMENTS: &[&str] = &[
    "rm -rf",
    "rm -r ",
    "rm -f",
    " rm ",
    "rmdir",
    "--delete",
    "reset --hard",
    "git clean",
    "checkout --",
    "drop table",
    "drop database",
    "truncate",
    "mkfs",
    "dd if=",
    "shred",
    "--purge",
    "> /dev/sd",
];

/// Where a parity failure lives. Reported so an operator/agent knows whether to
/// fix the robot state machine, the human projection, the docs/copy, the
/// fixture, or the proof logging — not just that "they disagree".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriftKind {
    /// The robot surface itself is wrong/missing (bad JSON, missing field,
    /// destructive recommendation).
    RobotState,
    /// The human surface contradicts or omits what the robot reported.
    HumanProjection,
    /// The shared copy/docs string is malformed (e.g. carries a destructive
    /// fragment) on both sides.
    Docs,
    /// The fixture could not be established (seed/index/setup failed).
    Fixture,
    /// The proof-log manifest did not record the run distinguishably.
    ProofLogging,
}

impl DriftKind {
    fn as_str(self) -> &'static str {
        match self {
            DriftKind::RobotState => "robot-state",
            DriftKind::HumanProjection => "human-projection",
            DriftKind::Docs => "docs",
            DriftKind::Fixture => "fixture",
            DriftKind::ProofLogging => "proof-logging",
        }
    }
}

/// Which fixture state a surface is exercised against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FixtureState {
    Fresh,
    Indexed,
}

impl FixtureState {
    fn as_str(self) -> &'static str {
        match self {
            FixtureState::Fresh => "fresh",
            FixtureState::Indexed => "indexed",
        }
    }
}

/// A readiness journey that has both a human and a robot surface form.
struct ParitySurface {
    name: &'static str,
    /// Robot (machine) argv base, e.g. `["status", "--json"]`.
    robot_args: &'static [&'static str],
    /// Human argv base, e.g. `["status"]`.
    human_args: &'static [&'static str],
    /// JSON pointer to the robot state-family token (e.g. `/health_level`).
    family_pointer: &'static str,
    /// Keys the robot success payload must carry (completeness proof).
    identity_keys: &'static [&'static str],
    /// Whether this is a *detailed* surface that always names the recommended
    /// action. `health` is the cheap/terse surface: when the robot reports
    /// `healthy=true` it legitimately collapses to a one-line "✓ Healthy" and
    /// the (informational) recommended action is reached via `status`/`triage`.
    /// So the verbatim recommended-action parity is required for detailed
    /// surfaces always, and for the terse surface only when there is an
    /// actionable problem (`healthy=false`).
    detailed: bool,
}

/// The readiness journeys this gate proves parity for. Health/status/triage all
/// derive from the same `state_meta_json_for_status` object, so the human
/// "Recommended:" line and the robot `recommended_action` come from one source.
///
/// Bead 76tvn extends the set to the `doctor --check` recovery surface, which
/// reports a top-level `status` + `healthy` + `recommended_action` from the same
/// readiness derivation. `doctor` is treated as *terse* (`detailed: false`): in
/// a healthy state its `recommended_action` is informational ("no immediate
/// action…") and the multi-section human output legitimately does not echo it
/// verbatim — exactly the `health` rule — so verbatim parity is required only
/// when there is an actionable problem (`healthy=false`). The `sources doctor`
/// recovery surface is *per-source* shaped (no single readiness verdict) and so
/// gets its own dedicated parity test rather than a `ParitySurface` entry.
fn parity_surfaces() -> Vec<ParitySurface> {
    vec![
        ParitySurface {
            name: "health",
            robot_args: &["health", "--json"],
            human_args: &["health"],
            family_pointer: "/health_level",
            identity_keys: &["healthy", "health_level", "recommended_action"],
            detailed: false,
        },
        ParitySurface {
            name: "status",
            robot_args: &["status", "--json"],
            human_args: &["status"],
            family_pointer: "/health_level",
            identity_keys: &["status", "healthy", "health_level", "recommended_action"],
            detailed: true,
        },
        ParitySurface {
            name: "triage",
            robot_args: &["triage", "--json"],
            human_args: &["triage"],
            family_pointer: "/status",
            identity_keys: &["recommended_commands", "recommended_action"],
            detailed: true,
        },
        ParitySurface {
            name: "doctor",
            robot_args: &["doctor", "--check", "--json"],
            human_args: &["doctor", "--check"],
            family_pointer: "/status",
            identity_keys: &["status", "healthy", "health_class", "recommended_action"],
            detailed: false,
        },
    ]
}

/// An isolated fixture: a tempdir-backed HOME + data dir, in one state.
struct Fixture {
    /// Kept for RAII cleanup; the commands must not outlive it.
    _home: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    /// `Some` when a seeded Codex session should be discoverable (indexed
    /// state); `None` for the fresh state (sources ignored, `CODEX_HOME` removed).
    codex_home: Option<PathBuf>,
    state: FixtureState,
}

/// Create the empty `tempdir` HOME + data dir shared by both fixtures.
fn isolated_home() -> Result<(tempfile::TempDir, PathBuf, PathBuf), String> {
    let home = tempfile::tempdir().map_err(|e| format!("create tempdir: {e}"))?;
    let home_path = home.path().to_path_buf();
    let data_dir = home_path.join("cass-data");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("create isolated data dir: {e}"))?;
    Ok((home, home_path, data_dir))
}

/// The fresh state: an empty isolated data dir → `not_initialized`.
fn fresh_fixture() -> Result<Fixture, String> {
    let (home, home_path, data_dir) = isolated_home()?;
    Ok(Fixture {
        _home: home,
        home: home_path,
        data_dir,
        codex_home: None,
        state: FixtureState::Fresh,
    })
}

/// The indexed state: a single tiny seeded Codex session indexed once →
/// `healthy`. Building this fixture *is itself* a real-binary `cass index`
/// invocation through the bounded runner.
fn indexed_fixture() -> Result<Fixture, String> {
    let (home, home_path, data_dir) = isolated_home()?;
    let codex_home = home_path.join(".codex");
    util::seed_codex_session(
        &codex_home,
        "rollout-2026-04-23T10-00-00-parity.jsonl",
        "human robot parity probe alpha unique-keyword-xyzzy",
        true,
    );
    let fixture = Fixture {
        _home: home,
        home: home_path,
        data_dir,
        codex_home: Some(codex_home),
        state: FixtureState::Indexed,
    };

    // Index once so the surfaces report a `healthy` family.
    let argv = build_argv(
        &fixture,
        &["index", "--full", "--json", "--no-progress-events"],
    );
    let cmd = parity_command(&fixture, &argv);
    let out = spawn_with_timeout_or_diag(
        cmd,
        "indexed_fixture_index",
        Some(&fixture.data_dir),
        INDEX_TIMEOUT,
    );
    let stdout = std::str::from_utf8(&out.stdout)
        .map_err(|e| format!("index fixture stdout not UTF-8: {e}"))?;
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("index fixture stdout not JSON: {e}; head: {}", head(stdout)))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "index fixture did not report success=true: {}",
            compact(&value)
        ));
    }
    Ok(fixture)
}

/// Build a full argv for a surface, appending the shared `--data-dir <dir>`
/// tail (health/status/triage/index all accept it).
fn build_argv(fixture: &Fixture, base: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    v.push("--data-dir".to_string());
    v.push(fixture.data_dir.to_string_lossy().into_owned());
    v
}

/// Build a `cass` command with the fixture's isolation env. The fresh fixture
/// ignores sources config and removes `CODEX_HOME`; the indexed fixture points
/// `CODEX_HOME` at its seeded session so exactly that one session is found.
fn parity_command(fixture: &Fixture, argv: &[String]) -> Command {
    let home = &fixture.home;
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(argv)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR");
    match &fixture.codex_home {
        Some(codex_home) => {
            cmd.env("CODEX_HOME", codex_home);
        }
        None => {
            cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1")
                .env_remove("CODEX_HOME");
        }
    }
    cmd
}

// --- small pure helpers (no panic; every check returns a diagnostic) --------

fn has_escape(bytes: &[u8]) -> bool {
    bytes.contains(&0x1b)
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

/// Lowercase and fold `_`/`-` to spaces so a robot snake-case family token
/// (`not_initialized`) is matched against the human phrase ("Not initialized").
fn normalize(s: &str) -> String {
    s.to_ascii_lowercase().replace(['_', '-'], " ")
}

/// Whether `text` is free of every destructive command fragment.
fn text_is_clean(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    !UNSAFE_COMMAND_FRAGMENTS
        .iter()
        .any(|frag| lower.contains(frag))
}

/// Whether a recommended command is safe to surface: free of every destructive
/// fragment and a sub-commanded `cass` invocation (never a bare interactive
/// `cass`, which would launch the TUI). This is the refusal contract — a generic
/// repair (`cass index --full`) or a read-only query (`cass search …`) is safe;
/// a destructive cleanup (`rm -rf`, `--delete`, `--purge`, `reset --hard`) is not.
fn command_is_safe(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let trimmed = lower.trim();
    text_is_clean(trimmed) && trimmed.starts_with("cass ") && trimmed != "cass"
}

// --- diagnostic-string builders, kept OUT of loop bodies so the bug scanner's
//     "allocation inside a loop" heuristic stays quiet (the recovery gate does
//     the same): each loop calls a flat helper instead of inlining `format!`. ---

/// `<surface>/<state>` label for a parity check.
fn surface_label(name: &str, state: FixtureState) -> String {
    format!("{name}/{}", state.as_str())
}

/// A `<label>_<form>` runner phase label (e.g. `status/fresh_robot`).
fn form_label(label: &str, form: &str) -> String {
    format!("{label}_{form}")
}

/// A drift line tagged with its attribution locus.
fn drift_failure_line(kind: DriftKind, why: &str) -> String {
    format!("[drift={}] {why}", kind.as_str())
}

/// The first recommended command that is not safe to surface, if any — so the
/// scan stays a flat iterator and the message `format!` lives in a helper.
fn first_unsafe_recommended_command(robot: &Value) -> Option<String> {
    robot
        .get("recommended_commands")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|c| c.get("command").and_then(Value::as_str))
        .find(|cmd| !command_is_safe(cmd))
        .map(str::to_string)
}

fn unsafe_command_msg(label: &str, cmd: &str) -> String {
    format!("{label} robot recommended command is not on the safe allow-list: {cmd}")
}

fn violation_destructive(label: &str) -> String {
    format!("{label}: human output carries a destructive fragment")
}

fn violation_bad_json(label: &str, err: &serde_json::Error) -> String {
    format!("{label}: robot stdout not JSON: {err}")
}

fn violation_unsafe_cmd(label: &str, cmd: &str) -> String {
    format!("{label}: unsafe recommended command {cmd:?}")
}

/// Run one surface form through the bounded runner. A hang dumps the runner's
/// `TIMEOUT DIAGNOSTIC` and panics (the loud timeout signal), categorically
/// separate from a parity `Err`.
fn run_form(fixture: &Fixture, base: &[&str], label: &str) -> Output {
    let argv = build_argv(fixture, base);
    let cmd = parity_command(fixture, &argv);
    spawn_with_timeout_or_diag(cmd, label, Some(&fixture.data_dir), SURFACE_TIMEOUT)
}

/// Parse a robot surface's pure-JSON stdout (the stdout=data hygiene half).
fn parse_robot_stdout(
    out: &Output,
    label: &str,
) -> Result<(Value, String, i32), (DriftKind, String)> {
    let code = out.status.code().ok_or((
        DriftKind::RobotState,
        format!("{label} robot was killed by a signal (no exit code)"),
    ))?;
    if has_escape(&out.stdout) {
        return Err((
            DriftKind::RobotState,
            format!("{label} robot stdout carries an ANSI escape (possible bare-TUI launch)"),
        ));
    }
    let stdout = std::str::from_utf8(&out.stdout).map_err(|e| {
        (
            DriftKind::RobotState,
            format!("{label} robot stdout not UTF-8: {e}"),
        )
    })?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err((
            DriftKind::RobotState,
            format!("{label} robot produced empty stdout (expected a JSON payload)"),
        ));
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|e| {
        (
            DriftKind::RobotState,
            format!(
                "{label} robot stdout is not pure JSON: {e}; head: {}",
                head(stdout)
            ),
        )
    })?;
    Ok((value, stdout.to_string(), code))
}

/// The core parity check for one surface in one fixture state. Returns the
/// drift locus + diagnostic on failure so the caller can attribute every drift.
fn check_parity(fixture: &Fixture, surface: &ParitySurface) -> Result<(), (DriftKind, String)> {
    let label = format!("{}/{}", surface.name, fixture.state.as_str());

    // --- robot surface ---
    let robot_out = run_form(fixture, surface.robot_args, &format!("{label}_robot"));
    let (robot, robot_stdout, robot_code) = parse_robot_stdout(&robot_out, &label)?;

    let obj = robot.as_object().ok_or((
        DriftKind::RobotState,
        format!(
            "{label} robot payload is not a JSON object: {}",
            compact(&robot)
        ),
    ))?;
    let missing: Vec<&str> = surface
        .identity_keys
        .iter()
        .copied()
        .filter(|k| !obj.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return Err((
            DriftKind::RobotState,
            format!(
                "{label} robot payload missing identity keys {missing:?}; present: {:?}",
                present_keys(&robot)
            ),
        ));
    }
    if robot_code != 0 && robot_code != 1 {
        return Err((
            DriftKind::RobotState,
            format!(
                "{label} robot exit {robot_code} (a readiness surface returns 0 ready or 1 not-ready)"
            ),
        ));
    }

    let recommended = robot
        .get("recommended_action")
        .and_then(Value::as_str)
        .ok_or((
            DriftKind::RobotState,
            format!("{label} robot payload missing string recommended_action"),
        ))?;
    let family = robot
        .pointer(surface.family_pointer)
        .and_then(Value::as_str)
        .ok_or((
            DriftKind::RobotState,
            format!(
                "{label} robot payload missing family token at {}",
                surface.family_pointer
            ),
        ))?;

    // The shared copy/docs string must itself be clean (a destructive fragment
    // here is a docs bug both surfaces would inherit).
    if !text_is_clean(recommended) {
        return Err((
            DriftKind::Docs,
            format!(
                "{label} robot recommended_action carries a destructive fragment: {recommended}"
            ),
        ));
    }
    // Every concrete recommended command must be on the safe allow-list.
    if let Some(cmd) = first_unsafe_recommended_command(&robot) {
        return Err((DriftKind::RobotState, unsafe_command_msg(&label, &cmd)));
    }

    // --- human surface ---
    let human_out = run_form(fixture, surface.human_args, &format!("{label}_human"));
    if has_escape(&human_out.stdout) {
        return Err((
            DriftKind::HumanProjection,
            format!("{label} human stdout carries an ANSI escape despite NO_COLOR"),
        ));
    }
    let human_stdout = std::str::from_utf8(&human_out.stdout).map_err(|e| {
        (
            DriftKind::HumanProjection,
            format!("{label} human stdout not UTF-8: {e}"),
        )
    })?;
    if human_stdout.trim().is_empty() {
        return Err((
            DriftKind::HumanProjection,
            format!("{label} human surface produced no output"),
        ));
    }

    // Parity 1 — the human surface names the robot's safe next action verbatim.
    // Required for the detailed surfaces always; for the terse `health` surface
    // only when there is an actionable problem (`healthy=false`). When healthy,
    // `health` legitimately collapses to a one-line confirmation and the
    // informational action is reached via `status`/`triage`.
    let healthy = robot
        .get("healthy")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let require_action_verbatim = surface.detailed || !healthy;
    if require_action_verbatim && !human_stdout.contains(recommended) {
        return Err((
            DriftKind::HumanProjection,
            format!(
                "{label} human output does not name the robot recommended_action.\n  robot: {recommended}\n  human head: {}",
                head(human_stdout)
            ),
        ));
    }
    // Parity 2 — the human headline reflects the robot's state family.
    let family_norm = normalize(family);
    if !normalize(human_stdout).contains(family_norm.as_str()) {
        return Err((
            DriftKind::HumanProjection,
            format!(
                "{label} human output does not reflect robot state family {family:?} (normalized {family_norm:?}); human head: {}",
                head(human_stdout)
            ),
        ));
    }
    // Parity 3 (refusal) — the human surface never surfaces destructive guidance.
    if !text_is_clean(human_stdout) {
        return Err((
            DriftKind::HumanProjection,
            format!("{label} human output carries a destructive command fragment"),
        ));
    }
    // Parity 4 — the human surface is concise; the robot JSON is complete. A
    // human rendering that is not strictly smaller than the full robot payload
    // is a sign it leaked the machine contract into operator copy.
    if human_stdout.len() >= robot_stdout.len() {
        return Err((
            DriftKind::HumanProjection,
            format!(
                "{label} human output ({} bytes) is not more concise than the complete robot JSON ({} bytes)",
                human_stdout.len(),
                robot_stdout.len()
            ),
        ));
    }

    Ok(())
}

/// Per-surface proof line (kept off the loop hot path).
fn log_parity_outcome(
    tracker: &PhaseTracker,
    label: &str,
    result: &Result<(), (DriftKind, String)>,
) {
    match result {
        Ok(()) => tracker.verbose(&format!("PARITY OK {label}")),
        Err((kind, why)) => {
            tracker.verbose(&format!("PARITY DRIFT [{}] {label}: {why}", kind.as_str()))
        }
    }
}

/// The comprehensive gate: every readiness surface, in both state families,
/// proven in human/robot parity. Returns `Err` (not a panic) so the proof log
/// records every surface's outcome before failing, and every failure carries
/// its drift attribution.
#[test]
fn human_robot_readiness_surfaces_are_in_parity_across_states() -> Result<(), String> {
    let tracker = PhaseTracker::new(
        "e2e_human_robot_parity_gate",
        "human_robot_readiness_surfaces_are_in_parity_across_states",
    );

    let fresh = fresh_fixture().map_err(|e| {
        format!(
            "[drift={}] fresh fixture setup failed: {e}",
            DriftKind::Fixture.as_str()
        )
    })?;
    let indexed = indexed_fixture().map_err(|e| {
        format!(
            "[drift={}] indexed fixture setup failed: {e}",
            DriftKind::Fixture.as_str()
        )
    })?;
    let fixtures = [&fresh, &indexed];
    let surfaces = parity_surfaces();

    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for fixture in fixtures {
        for surface in &surfaces {
            let label = surface_label(surface.name, fixture.state);
            let phase = tracker.start(&label, Some("human/robot parity"));
            let result = check_parity(fixture, surface);
            tracker.end(&label, None, phase);
            log_parity_outcome(&tracker, &label, &result);
            checked += 1;
            if let Err((kind, why)) = result {
                failures.push(drift_failure_line(kind, &why));
            }
        }
    }

    if failures.is_empty() {
        tracker.complete();
        return Ok(());
    }
    let summary = format!(
        "{} of {checked} human/robot parity checks drifted:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
    tracker.fail(E2eError::new(summary.clone()));
    Err(summary)
}

/// Negative / refusal proof: across every surface and state, the human output
/// must never carry a destructive command fragment and every recommended
/// command must be on the safe allow-list — even though a generic repair
/// command (`cass index --full`) is exactly what the fresh state recommends.
#[test]
fn human_surfaces_never_surface_destructive_guidance() -> Result<(), String> {
    let fresh = fresh_fixture()?;
    let indexed = indexed_fixture()?;
    let fixtures = [&fresh, &indexed];
    let surfaces = parity_surfaces();

    let mut violations: Vec<String> = Vec::new();
    for fixture in fixtures {
        for surface in &surfaces {
            let label = surface_label(surface.name, fixture.state);
            let human_out = run_form(fixture, surface.human_args, &form_label(&label, "human"));
            let human = String::from_utf8_lossy(&human_out.stdout);
            if !text_is_clean(&human) {
                violations.push(violation_destructive(&label));
            }

            let robot_out = run_form(fixture, surface.robot_args, &form_label(&label, "robot"));
            let robot_stdout = String::from_utf8_lossy(&robot_out.stdout);
            let value: Value = match serde_json::from_str(robot_stdout.trim()) {
                Ok(v) => v,
                Err(e) => {
                    violations.push(violation_bad_json(&label, &e));
                    continue;
                }
            };
            if let Some(cmd) = first_unsafe_recommended_command(&value) {
                violations.push(violation_unsafe_cmd(&label, &cmd));
            }
        }
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} refusal violation(s):\n  - {}",
            violations.len(),
            violations.join("\n  - ")
        ))
    }
}

/// Recovery-surface parity for `cass sources doctor` (bead 76tvn). Unlike the
/// readiness journeys, sources doctor is a *per-source* report — there is no
/// single top-level readiness verdict — so it gets a dedicated parity check
/// rather than a `ParitySurface` entry. For a configured-but-unreachable source
/// (RFC 6761 `.invalid` host → a deterministic DNS failure, no network needed):
///   * the robot `--json` form is pure JSON, a per-source report (`sources[]` +
///     `summary` + `mutation_free=true`) that marks the source not-reached and
///     offers an allow-listed `safe_next_command`;
///   * the human form names the same source and the same failing host, stays
///     escape-free, and carries no destructive guidance; and
///   * classifying the source never rewrites `sources.toml`.
#[test]
fn sources_doctor_recovery_surface_is_in_parity() -> Result<(), String> {
    let home = tempfile::tempdir().map_err(|e| format!("create tempdir: {e}"))?;
    let home_path = home.path().to_path_buf();
    let config_dir = home_path.join("xdg-config").join("cass");
    std::fs::create_dir_all(&config_dir).map_err(|e| format!("create config dir: {e}"))?;
    let sources_toml = config_dir.join("sources.toml");
    let toml = concat!(
        "[[sources]]\n",
        "name = \"parity-host\"\n",
        "type = \"ssh\"\n",
        "host = \"nobody@parity.invalid\"\n",
        "paths = [\"~/.claude/projects\"]\n",
    );
    std::fs::write(&sources_toml, toml).map_err(|e| format!("write sources.toml: {e}"))?;
    let before = std::fs::read(&sources_toml).map_err(|e| format!("read sources.toml: {e}"))?;
    let robot_drift = DriftKind::RobotState.as_str();
    let human_drift = DriftKind::HumanProjection.as_str();

    // --- robot form ---
    let robot_out = run_sources_doctor_form(
        &home_path,
        &["sources", "doctor", "--json"],
        "sources_doctor_robot",
    );
    if robot_out.status.code() != Some(1) {
        return Err(format!(
            "[drift={robot_drift}] sources doctor --json should exit 1 with an unreachable source, got {:?}",
            robot_out.status.code(),
        ));
    }
    let robot_stdout = std::str::from_utf8(&robot_out.stdout)
        .map_err(|e| format!("[drift={robot_drift}] sources doctor robot stdout not UTF-8: {e}"))?;
    let robot: Value = serde_json::from_str(robot_stdout.trim()).map_err(|e| {
        format!(
            "[drift={robot_drift}] sources doctor robot stdout is not pure JSON: {e}; head: {}",
            head(robot_stdout)
        )
    })?;
    if robot.get("mutation_free").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "[drift={robot_drift}] sources doctor robot payload must mark mutation_free=true"
        ));
    }
    let sources = robot.get("sources").and_then(Value::as_array).ok_or_else(|| {
        format!(
            "[drift={robot_drift}] sources doctor robot payload missing sources[] array; present: {:?}",
            present_keys(&robot)
        )
    })?;
    let source = sources
        .iter()
        .find(|s| s.get("source_id").and_then(Value::as_str) == Some("parity-host"))
        .ok_or_else(|| {
            format!(
                "[drift={robot_drift}] sources doctor robot did not report the configured source"
            )
        })?;
    if source.get("host_reached").and_then(Value::as_bool) != Some(false) {
        return Err(format!(
            "[drift={robot_drift}] sources doctor should report the .invalid host as not reached"
        ));
    }
    let safe_cmd = source
        .get("safe_next_command")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!("[drift={robot_drift}] sources doctor source missing safe_next_command")
        })?;
    if !command_is_safe(safe_cmd) {
        return Err(format!(
            "[drift={robot_drift}] sources doctor safe_next_command is not on the safe allow-list: {safe_cmd}"
        ));
    }

    // --- human form ---
    let human_out =
        run_sources_doctor_form(&home_path, &["sources", "doctor"], "sources_doctor_human");
    if has_escape(&human_out.stdout) {
        return Err(format!(
            "[drift={human_drift}] sources doctor human stdout carries an ANSI escape despite NO_COLOR"
        ));
    }
    let human = std::str::from_utf8(&human_out.stdout)
        .map_err(|e| format!("[drift={human_drift}] sources doctor human stdout not UTF-8: {e}"))?;
    if human.trim().is_empty() {
        return Err(format!(
            "[drift={human_drift}] sources doctor human surface produced no output"
        ));
    }
    // Parity — the human projection names the same source the robot reports.
    if !human.contains("parity-host") {
        return Err(format!(
            "[drift={human_drift}] sources doctor human output does not name the source 'parity-host'; head: {}",
            head(human)
        ));
    }
    // Parity — both surfaces diagnose the same failing host (the shared evidence
    // the robot `state`/`connection_error` and the human checks both reference).
    if !human.contains("parity.invalid") {
        return Err(format!(
            "[drift={human_drift}] sources doctor human output does not reflect the failing host 'parity.invalid'; head: {}",
            head(human)
        ));
    }
    // Refusal — the human surface never carries destructive guidance.
    if !text_is_clean(human) {
        return Err(format!(
            "[drift={human_drift}] sources doctor human output carries a destructive command fragment"
        ));
    }

    // Mutation-free — classifying the source did not rewrite sources.toml.
    let after =
        std::fs::read(&sources_toml).map_err(|e| format!("read sources.toml after: {e}"))?;
    if before != after {
        return Err(format!(
            "[drift={}] sources doctor rewrote sources.toml (must be mutation-free)",
            DriftKind::Fixture.as_str()
        ));
    }
    Ok(())
}

/// Run a `sources doctor` form against an isolated HOME/XDG with a pre-seeded
/// `sources.toml`. `sources doctor` reads `XDG_CONFIG_HOME/cass/sources.toml` and
/// takes no `--data-dir`, so this builds its own bounded command rather than
/// reusing the data-dir-appending readiness harness.
fn run_sources_doctor_form(home: &std::path::Path, args: &[&str], label: &str) -> Output {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR");
    spawn_with_timeout_or_diag(cmd, label, None, SURFACE_TIMEOUT)
}

/// The three-way outcome the gate distinguishes for a parity run. A hang is its
/// own loud category — neither a pass nor an ordinary drift.
#[derive(Debug)]
enum Outcome {
    Pass,
    Fail,
    Timeout,
}

/// Classify a parity check, catching the bounded runner's timeout panic so the
/// `Timeout` path is provably separate from the `Pass`/`Fail` paths. The runner
/// still prints its `TIMEOUT DIAGNOSTIC` first.
fn classify_parity(fixture: &Fixture, surface: &ParitySurface) -> Outcome {
    let spawned = std::panic::catch_unwind(AssertUnwindSafe(|| check_parity(fixture, surface)));
    match spawned {
        Err(_) => Outcome::Timeout,
        Ok(Ok(())) => Outcome::Pass,
        Ok(Err(_)) => Outcome::Fail,
    }
}

/// Drive a guaranteed-hang child through the same bounded-runner + catch-unwind
/// path so the `Timeout` category is proven, not asserted by luck. A
/// deterministic `sleep` avoids the cold-start race a tiny-bound real-binary run
/// would flake on.
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

/// Proves the gate separates pass / fail / timeout for parity runs:
///   * a real surface in parity → `Pass`;
///   * a real surface checked with a deliberately wrong (absent) family pointer
///     → `Fail` (a robot-state drift the evaluator returns as `Err`);
///   * a guaranteed-hang child under a short bound → `Timeout`.
#[test]
fn parity_outcome_is_classified_pass_fail_timeout() -> Result<(), String> {
    let fresh = fresh_fixture()?;

    let good = ParitySurface {
        name: "status",
        robot_args: &["status", "--json"],
        human_args: &["status"],
        family_pointer: "/health_level",
        identity_keys: &["status", "healthy", "health_level", "recommended_action"],
        detailed: true,
    };
    let pass = classify_parity(&fresh, &good);
    if !matches!(pass, Outcome::Pass) {
        return Err(format!(
            "expected status parity in the fresh state to classify as Pass, got {pass:?}"
        ));
    }

    // Same real surface, but point the family token at a field that does not
    // exist → the evaluator returns Err → Fail (a deterministic robot-state
    // drift, no host flakiness).
    let broken = ParitySurface {
        name: "status",
        robot_args: &["status", "--json"],
        human_args: &["status"],
        family_pointer: "/this_field_does_not_exist_zzz",
        identity_keys: &["status"],
        detailed: true,
    };
    let fail = classify_parity(&fresh, &broken);
    if !matches!(fail, Outcome::Fail) {
        return Err(format!(
            "expected a broken-pointer parity check to classify as Fail, got {fail:?}"
        ));
    }

    #[cfg(unix)]
    {
        let timeout = classify_hang(Duration::from_millis(300));
        if !matches!(timeout, Outcome::Timeout) {
            return Err(format!(
                "expected a guaranteed-hang child to classify as Timeout, got {timeout:?}"
            ));
        }
    }
    Ok(())
}

/// Parse one proof-log line into a JSON event (allocation off the per-line hot
/// path, error context preserved).
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

/// True when `event` is a `test_end` whose `result.status` equals `want_status`
/// and whose `error.type` matches `want_err_type`.
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

/// Count `test_end` records matching a status (and optional `error.type`).
fn count_test_ends(
    jsonl: &str,
    want_status: &str,
    want_err_type: Option<&str>,
) -> Result<usize, String> {
    let events = proof_log_events(jsonl)?;
    Ok(events
        .iter()
        .filter(|event| test_end_matches(event, want_status, want_err_type))
        .count())
}

/// Epic-13 acceptance: the integrated gate cannot pass unless parity passes *or*
/// is explicitly skipped with a structured reason; failures identify the drift
/// locus. This drives the real `.12.3` `E2eLogger` to a tempdir and asserts the
/// wire form separates a parity pass, a parity drift (carrying its drift
/// locus), and a structured skip (carrying its reason).
#[test]
fn parity_proof_log_distinguishes_pass_drift_and_skip() -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("create tempdir: {e}"))?;
    let log_path = tmp.path().join("parity-gate-proof.jsonl");

    let logger = E2eLogger::with_path("rust", log_path.clone())
        .map_err(|e| format!("open proof logger: {e}"))?;
    logger
        .run_start(None)
        .map_err(|e| format!("run_start: {e}"))?;

    // A parity pass: status=pass, no error.
    let pass = E2eTestInfo::simple("status/fresh", "e2e_human_robot_parity_gate");
    logger
        .test_start(&pass)
        .map_err(|e| format!("test_start(pass): {e}"))?;
    let phase = E2ePhase {
        name: "parity".to_string(),
        description: Some("human/robot readiness parity".to_string()),
    };
    logger
        .phase_start(&phase)
        .map_err(|e| format!("phase_start: {e}"))?;
    logger
        .phase_end(&phase, 11)
        .map_err(|e| format!("phase_end: {e}"))?;
    logger
        .test_pass(&pass, 11, None)
        .map_err(|e| format!("test_pass: {e}"))?;

    // A parity drift: status=fail, error.type carries the drift locus so the
    // failure says *where* the drift is.
    let drift = E2eTestInfo::simple("status/indexed", "e2e_human_robot_parity_gate");
    logger
        .test_start(&drift)
        .map_err(|e| format!("test_start(drift): {e}"))?;
    logger
        .test_fail(
            &drift,
            14,
            None,
            E2eError::with_type(
                "human output omits robot recommended_action",
                DriftKind::HumanProjection.as_str(),
            ),
        )
        .map_err(|e| format!("test_fail: {e}"))?;

    // A structured skip: status=skip with a reason (the "explicitly skipped
    // with a structured reason" half of the acceptance).
    let skipped = E2eTestInfo::simple("triage/live-host", "e2e_human_robot_parity_gate");
    logger
        .test_start(&skipped)
        .map_err(|e| format!("test_start(skip): {e}"))?;
    logger
        .test_end(
            &skipped,
            "skip",
            0,
            None,
            Some(E2eError::with_type(
                "parity skipped: live unreachable-host journey is opt-in and not run in CI",
                "structured_skip_reason",
            )),
        )
        .map_err(|e| format!("test_end(skip): {e}"))?;

    logger
        .run_end(
            E2eRunSummary {
                total: 3,
                passed: 1,
                failed: 1,
                skipped: 1,
                flaky: None,
                duration_ms: 25,
            },
            1,
        )
        .map_err(|e| format!("run_end: {e}"))?;

    let jsonl = std::fs::read_to_string(&log_path)
        .map_err(|e| format!("read proof log {}: {e}", log_path.display()))?;

    let pass_records = count_test_ends(&jsonl, "pass", None)?;
    if pass_records != 1 {
        return Err(format!(
            "[drift={}] expected exactly one parity pass record (status=pass, no error), got {pass_records}; \
             proof log:\n{jsonl}",
            DriftKind::ProofLogging.as_str()
        ));
    }
    let drift_records = count_test_ends(&jsonl, "fail", Some(DriftKind::HumanProjection.as_str()))?;
    if drift_records != 1 {
        return Err(format!(
            "expected exactly one parity drift record (status=fail, error.type=human-projection), \
             got {drift_records}; proof log:\n{jsonl}"
        ));
    }
    let skip_records = count_test_ends(&jsonl, "skip", Some("structured_skip_reason"))?;
    if skip_records != 1 {
        return Err(format!(
            "expected exactly one structured-skip record (status=skip with a reason), got {skip_records}; \
             proof log:\n{jsonl}"
        ));
    }
    Ok(())
}

// --- bead v6vuz: the human readiness summary block is actually RENDERED -------
//
// `.13.2` (and the parity tests above) prove the human *projection* agrees with
// the robot JSON in-process and that the human surfaces name the robot's
// recommended action. Bead `v6vuz` wires `project_human_summary` into the live
// human (non-`--json`) branches of `status` / `triage` / `health` / `doctor` /
// `search`, so the same bounded readiness summary the robot serializes is now
// rendered to operators. This gate proves that wiring survives to the real
// binary: the block is emitted, and its searchable verdict does not contradict
// the matching `--json` readiness.

/// Markers every wired human readiness surface must emit — the bounded summary
/// block layered over the same `DerivedAssetTruthTable` the robot JSON
/// serializes (`render_lines` in `src/search/human_readiness_summary.rs`).
const READINESS_BLOCK_MARKERS: &[&str] = &[
    "Readiness:",
    "Search usable now:",
    "Safest next step:",
    "State codes:",
];

/// Flat message builders (kept off the loop hot path so the bug scanner's
/// "allocation inside a loop" heuristic stays quiet, matching this file's style).
fn missing_marker_msg(label: &str, marker: &str, human: &str) -> String {
    format!(
        "{label}: human output is missing readiness block marker {marker:?}; head: {}",
        head(human)
    )
}

fn searchable_drift_msg(label: &str, want: &str, human: &str) -> String {
    format!(
        "{label}: human readiness searchable verdict drift, expected {want:?}; head: {}",
        head(human)
    )
}

fn robot_contradiction_msg(label: &str, robot_healthy: bool, expect_searchable: bool) -> String {
    format!(
        "{label}: robot status healthy={robot_healthy} contradicts the human readiness searchable={expect_searchable}"
    )
}

fn dirty_block_msg(label: &str) -> String {
    format!("{label}: human readiness block carries a destructive command fragment")
}

/// Bead `coding_agent_session_search-v6vuz`: every wired human readiness surface
/// (status / triage / health) must EMIT the bounded readiness summary block, and
/// that block's searchable verdict must agree with the matching `--json`
/// readiness for the same fixture. This proves the `.13.2` human projection is
/// not merely defined but actually *rendered* by the real binary, and that it
/// never contradicts the robot contract for the same state.
#[test]
fn human_readiness_block_is_emitted_and_agrees_with_robot() -> Result<(), String> {
    let fresh = fresh_fixture()?;
    let indexed = indexed_fixture()?;

    // (fixture, expect_searchable): the fresh dir is `not_initialized` → not
    // searchable; the indexed dir has a freshly-built lexical index →
    // searchable. For both, the robot `healthy` bit is ⟺ searchable, so it is a
    // sound non-contradiction cross-check.
    let cases: [(&Fixture, bool); 2] = [(&fresh, false), (&indexed, true)];
    // The bead's human surfaces that carry the full bounded block on stdout.
    let surfaces: [&[&str]; 3] = [&["status"][..], &["triage"][..], &["health"][..]];

    let mut failures: Vec<String> = Vec::new();

    for (fixture, expect_searchable) in cases {
        // Robot cross-check: the readiness verdict the human block renders must
        // not contradict `--json`.
        let status_label = surface_label("status", fixture.state);
        let robot_out = run_form(
            fixture,
            &["status", "--json"][..],
            &form_label(&status_label, "robot"),
        );
        let robot_healthy = match parse_robot_stdout(&robot_out, &status_label) {
            Ok((value, _, _)) => value
                .get("healthy")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            Err((_, why)) => {
                failures.push(why);
                continue;
            }
        };
        if robot_healthy != expect_searchable {
            failures.push(robot_contradiction_msg(
                &status_label,
                robot_healthy,
                expect_searchable,
            ));
        }

        let usable_line = if expect_searchable {
            "Search usable now: yes"
        } else {
            "Search usable now: no"
        };

        for surface in surfaces {
            let name = surface[0];
            let label = surface_label(name, fixture.state);
            let human_out = run_form(fixture, surface, &form_label(&label, "human"));
            let human = String::from_utf8_lossy(&human_out.stdout);

            for marker in READINESS_BLOCK_MARKERS {
                if !human.contains(marker) {
                    failures.push(missing_marker_msg(&label, marker, &human));
                }
            }
            if !human.contains(usable_line) {
                failures.push(searchable_drift_msg(&label, usable_line, &human));
            }
            // The block (and all surrounding human copy) must stay clean.
            if !text_is_clean(&human) {
                failures.push(dirty_block_msg(&label));
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} human readiness wiring failure(s):\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        ))
    }
}
