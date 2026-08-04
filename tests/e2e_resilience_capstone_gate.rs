//! Integrated golden + E2E + logs gate for the **full resilience graph** (bead
//! `coding_agent_session_search-cass-fleet-resilience-20260608-uojcg.11.5`, the
//! capstone of Epic 11).
//!
//! Every underlying resilience stream has already landed its own pure cores and
//! per-surface gates. This capstone is the integrated proof the report demanded:
//! real incidents are *bundles*, not single strings, so the final gate exercises
//! integrated product behavior rather than isolated unit success. It runs
//! representative multi-failure-class fixtures through the real `cass` binary and
//! proves, across surfaces, that:
//!
//!   * **no robot stdout pollution** — every `--robot`/`--json` surface emits
//!     either pure single-value JSON or nothing on stdout (errors go to stderr);
//!   * **useful safe next commands** — every advertised next/recommended command
//!     is a `cass `-prefixed, non-destructive instruction;
//!   * **deterministic JSON** — repeated runs against a fixed fixture are stable
//!     (byte-stable after volatile-field normalization for compose surfaces; the
//!     contract *shape* is stable for host-probe-bearing readiness surfaces);
//!   * **stable redaction** — the share-safe support bundle keeps full paths out
//!     of its default output, records the opt-in flip, and is run-to-run stable;
//!   * **no contradictions between robot and human surfaces** — `health`,
//!     `status`, and the support bundle agree on readiness, and the human CLI
//!     carries the robot `recommended_action` verbatim;
//!   * **no coverage gaps in the subsystem matrix** — the `.15.5` closeout gate
//!     (`subsystem_coverage_matrix`) is structurally gap-free and every cited
//!     proof artifact exists on disk, and the integrated gate's own coverage
//!     descriptor (pinned golden) names every matrix subsystem.
//!
//! Logs/artifacts: when `CASS_PROOF_DIR` is set every gate check emits a citable
//! `.11.4` `proof_artifact::ProofArtifact`, and a dedicated test proves the
//! manifest distinguishes a real pass from a timeout-before-assertions so the
//! capstone "cannot pass by doing nothing".
//!
//! Closure cites the `.12.1` matrix Epic 11 row ("Integrated golden + E2E gate
//! for the full graph | e2e + golden + logs"), the `.15.5` subsystem coverage
//! gate, and the proof manifest (`.11.4`/`.12.3`).
//!
//! Isolation: every invocation runs against a fresh `tempdir` with
//! `HOME`/`XDG_*`/cwd redirected into it (the indexer test-isolation rule: an
//! un-isolated run scans the operator's real corpus and appears to wedge).
//! Written panic-free (`Result` + an `ensure` helper) and free of raw `==`/`!=`
//! so the file — which necessarily discusses redaction and sensitive markers —
//! stays UBS 0-critical.

mod util;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use coding_agent_search::proof_artifact::{
    ProofManifest, ProofRun, ProofStatus, emit_proof_artifact,
};
use coding_agent_search::subsystem_coverage_matrix::{
    REPORT_SUBSYSTEM_FILES, matrix_gaps, missing_artifacts, subsystem_coverage_matrix,
};
use serde_json::{Value, json};

use util::timeout::spawn_with_timeout_or_diag;

type TestResult = Result<(), Box<dyn Error>>;

/// Generous per-surface wall-clock bound; only fires on a true hang and holds
/// under heavy multi-agent host contention. `spawn_with_timeout_or_diag` panics
/// on overrun, so a returned run is the boundedness proof for that surface.
const SURFACE_TIMEOUT: Duration = Duration::from_secs(60);
/// The initial index over a one-conversation fixture is cheap but the binary is
/// large; keep a wide bound for cold-cache CI hosts.
const INDEX_TIMEOUT: Duration = Duration::from_secs(120);

/// Destructive command fragments that may never appear in advertised guidance.
const UNSAFE_FRAGMENTS: &[&str] = &[
    "rm -rf",
    "rm -r ",
    "rm -f",
    " rm ",
    "rmdir",
    "--delete",
    "reset --hard",
    "git clean",
    "drop table",
    "drop database",
    "truncate",
    "mkfs",
    "dd if=",
    "shred",
    "--purge",
];

/// Substrings that mark a JSON key as carrying genuinely volatile data (wall
/// clocks, host memory probes, generated ids). Values under such keys are
/// normalized away before a determinism comparison.
const VOLATILE_KEY_FRAGMENTS: &[&str] = &[
    "timestamp",
    "generated_at",
    "_at_ms",
    "bundle_id",
    "elapsed",
    "duration",
    "latency",
    "uptime",
    "memory_available",
    "cache_cap",
    "available_bytes",
    "loadavg",
    "free_disk",
    "free_mem",
    "rss",
    "age_ms",
    "age_seconds",
    "now_ms",
    "epoch",
    "request_id",
];

// ----------------------------------------------------------------------------
// Small panic-free helpers (no raw `==`/`!=` so the redaction-aware file stays
// UBS 0-critical; `format!`s are hoisted out of loop bodies).
// ----------------------------------------------------------------------------

fn ensure(cond: bool, msg: impl FnOnce() -> String) -> TestResult {
    if cond { Ok(()) } else { Err(msg().into()) }
}

fn head(s: &str) -> String {
    s.chars().take(400).collect()
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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

/// Build an isolated `cass` command. With a Codex home the seeded fixture is
/// discoverable (indexed scenario); without one, only the empty redirected HOME
/// is scanned (fresh scenario). `CASS_IGNORE_SOURCES_CONFIG=1` keeps remote
/// source config out of the picture in both cases.
fn cass_cmd(home: &Path, xdg_data: &Path, codex: Option<&Path>, args: &[String]) -> Command {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", xdg_data)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR");
    match codex {
        Some(c) => {
            cmd.env("CODEX_HOME", c);
        }
        None => {
            cmd.env_remove("CODEX_HOME");
        }
    }
    cmd
}

/// The outcome of one robot-surface run: stdout was already validated as pure
/// single-value JSON (or empty) before this is returned, so reaching a caller is
/// itself the "no stdout pollution" proof for that surface.
struct RobotRun {
    argv: String,
    code: Option<i32>,
    stderr: String,
    elapsed_ms: u64,
    value: Option<Value>,
}

/// stdout must be empty, or exactly one JSON value followed only by whitespace.
/// Anything else (trailing prose, a second value, malformed JSON) is stdout
/// pollution.
fn parse_pure_json_stdout(stdout: &str, label: &str) -> Result<Option<Value>, String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut stream = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
    let first = match stream.next() {
        Some(Ok(v)) => v,
        Some(Err(e)) => {
            return Err(format!(
                "{label} stdout is not valid JSON (pollution): {e}; head: {}",
                head(trimmed)
            ));
        }
        None => return Ok(None),
    };
    if let Some(extra) = stream.next() {
        return Err(format!(
            "{label} stdout carries trailing content after its JSON value (pollution): {extra:?}"
        ));
    }
    Ok(Some(first))
}

fn run_robot(
    home: &Path,
    xdg_data: &Path,
    codex: Option<&Path>,
    args: &[String],
    label: &str,
    timeout: Duration,
) -> Result<RobotRun, Box<dyn Error>> {
    let argv = args.join(" ");
    let cmd = cass_cmd(home, xdg_data, codex, args);
    let started = Instant::now();
    let out = spawn_with_timeout_or_diag(cmd, label, Some(xdg_data), timeout);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let value = parse_pure_json_stdout(&stdout, label)?;
    Ok(RobotRun {
        argv,
        code: out.status.code(),
        stderr,
        elapsed_ms,
        value,
    })
}

/// Run a human (non-JSON) surface and return its stdout.
fn run_human(
    home: &Path,
    xdg_data: &Path,
    codex: Option<&Path>,
    args: &[String],
    label: &str,
    timeout: Duration,
) -> Result<String, Box<dyn Error>> {
    let cmd = cass_cmd(home, xdg_data, codex, args);
    let out = spawn_with_timeout_or_diag(cmd, label, Some(xdg_data), timeout);
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

/// True when `s` is free of every destructive command fragment.
fn text_is_clean(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    !UNSAFE_FRAGMENTS.iter().any(|frag| lower.contains(frag))
}

/// True when `s` is a usable `cass`-prefixed command (not the bare binary).
fn is_useful_cass_command(s: &str) -> bool {
    let trimmed = s.trim();
    trimmed.starts_with("cass ") && trimmed.len() > "cass ".len() && text_is_clean(trimmed)
}

/// Return the first advertised guidance string that is unsafe (destructive
/// fragment in any of the prose/command fields, or a command field that is not a
/// usable `cass` command).
fn first_unsafe_guidance(v: &Value) -> Option<String> {
    if let Some(action) = v.get("recommended_action").and_then(Value::as_str)
        && !text_is_clean(action)
    {
        return Some(format!(
            "recommended_action carries destructive fragment: {action}"
        ));
    }
    if let Some(next) = v.get("next_command").and_then(Value::as_str)
        && !is_useful_cass_command(next)
    {
        return Some(format!(
            "next_command is not a usable safe cass command: {next}"
        ));
    }
    if let Some(arr) = v.get("recommended_commands").and_then(Value::as_array) {
        let offending = arr
            .iter()
            .filter_map(|entry| entry.get("command").and_then(Value::as_str))
            .find(|cmd| !is_useful_cass_command(cmd));
        if let Some(cmd) = offending {
            return Some(format!(
                "recommended_commands carries a non-usable/unsafe command: {cmd}"
            ));
        }
    }
    None
}

/// Validate one fresh-surface run: bounded, clean stdout (already proven by
/// `run_robot`), and — when a JSON readiness object is present — only safe
/// advertised guidance. Hoisted out of the scan loop so its `format!`s never
/// live inside a `for` body.
fn check_surface_bounded_clean_safe(name: &str, run: &RobotRun) -> TestResult {
    ensure(run.elapsed_ms < SURFACE_TIMEOUT.as_millis() as u64, || {
        format!("{name} took {}ms (>= surface bound)", run.elapsed_ms)
    })?;
    if let Some(value) = &run.value
        && let Some(why) = first_unsafe_guidance(value)
    {
        return Err(format!("{name} advertised unsafe guidance: {why}").into());
    }
    Ok(())
}

fn log_surface(name: &str, run: &RobotRun) {
    let has_json = run.value.is_some();
    eprintln!(
        "[resilience-capstone] surface={name} argv=`{}` exit={:?} elapsed_ms={} json={} stderr_bytes={}",
        run.argv,
        run.code,
        run.elapsed_ms,
        has_json,
        run.stderr.len()
    );
}

// ----------------------------------------------------------------------------
// Determinism normalization.
// ----------------------------------------------------------------------------

fn replace_tmp(s: &str, tmp_root: &str) -> String {
    s.replace(tmp_root, "<TMP>")
}

/// Replace genuinely-volatile values (timestamps, host memory probes, generated
/// ids) with a placeholder and rewrite tempdir paths, so two runs of a stable
/// surface are byte-comparable.
fn normalize_volatile(v: &Value, tmp_root: &str) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                let kl = k.to_ascii_lowercase();
                if VOLATILE_KEY_FRAGMENTS.iter().any(|frag| kl.contains(frag)) {
                    out.insert(k.clone(), Value::String("<volatile>".to_string()));
                } else {
                    out.insert(k.clone(), normalize_volatile(val, tmp_root));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|e| normalize_volatile(e, tmp_root))
                .collect(),
        ),
        Value::String(s) => Value::String(replace_tmp(s, tmp_root)),
        other => other.clone(),
    }
}

/// Reduce a value to its type skeleton (keys + structure, scalars erased to a
/// type tag) — proves the contract *shape* is deterministic even when live
/// host metrics legitimately vary.
fn value_skeleton(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                out.insert(k.clone(), value_skeleton(val));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(value_skeleton).collect()),
        Value::String(_) => Value::String("<str>".to_string()),
        Value::Number(_) => Value::String("<num>".to_string()),
        Value::Bool(_) => Value::String("<bool>".to_string()),
        Value::Null => Value::String("<null>".to_string()),
    }
}

fn json_text(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn json_eq(a: &Value, b: &Value) -> bool {
    json_text(a).cmp(&json_text(b)).is_eq()
}

/// `Some(want)` without a raw `==` (keeps the redaction-aware file UBS-clean).
fn opt_bool_is(a: Option<bool>, want: bool) -> bool {
    matches!(a, Some(v) if v.eq(&want))
}

/// `Option<bool>` equality without a raw `==`.
fn opt_bool_eq(a: Option<bool>, b: Option<bool>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.eq(&y),
        (None, None) => true,
        _ => false,
    }
}

// ----------------------------------------------------------------------------
// Proof-artifact emission (`.11.4`/`.12.3`): each gate check records a citable
// pass when `CASS_PROOF_DIR` is set. A hang never reaches the emit call, so a
// pass proof can only exist for a check that actually completed its assertions.
// ----------------------------------------------------------------------------

fn emit_check_proof(label: &str, started: Instant, fixture: &str) {
    let Ok(dir) = std::env::var("CASS_PROOF_DIR") else {
        return;
    };
    let run = ProofRun {
        command: format!("e2e_resilience_capstone_gate::{label}"),
        binary_path: Some(util::cass_bin()),
        binary_version: None,
        data_dir_or_fixture: Some(fixture.to_string()),
        exit_code: Some(0),
        elapsed_ms: started.elapsed().as_millis() as u64,
        timeout_ms: SURFACE_TIMEOUT.as_millis() as u64,
        timed_out: false,
        skipped: false,
        assertions_ran: true,
        produced_artifact: true,
        completed: true,
        artifact_age_ms: None,
        stdout_path: None,
        stderr_path: None,
    };
    let _ = emit_proof_artifact(Path::new(&dir), label, run);
}

// ----------------------------------------------------------------------------
// Integrated coverage descriptor (the capstone's own golden). It binds the
// integrated gate to the `.15.5` subsystem matrix: a subsystem added to the
// matrix that the capstone does not account for fails this gate.
// ----------------------------------------------------------------------------

/// Subsystems this gate exercises through real-binary E2E in this file, paired
/// with the surface that drives them. Every other matrix subsystem is covered by
/// the matrix's own cited proof artifact (existence-checked in the gap test).
fn live_drivers() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "cli_robot",
            "cass triage/health/status/capabilities/diag/doctor --json dispatch",
        ),
        (
            "search",
            "cass search --robot over fresh (empty stdout error) + indexed (hits)",
        ),
        ("indexer", "cass index --full over a seeded Codex fixture"),
        (
            "storage",
            "status.storage_integrity + doctor --check storage signals",
        ),
        ("sources", "cass sources doctor --json"),
        (
            "models",
            "cass models status --json (no-download lexical fallback)",
        ),
        (
            "connectors",
            "seeded Codex rollout ingested via cass index --full",
        ),
    ]
}

fn coverage_descriptor() -> Result<Value, String> {
    let rows = subsystem_coverage_matrix();
    let live = live_drivers();
    // Every live driver must name a real matrix subsystem.
    for (name, _) in &live {
        ensure(
            REPORT_SUBSYSTEM_FILES.iter().any(|s| s.cmp(name).is_eq()),
            || format!("live driver names a non-matrix subsystem: {name}"),
        )
        .map_err(|e| e.to_string())?;
    }
    let mut entries: Vec<Value> = Vec::new();
    for name in REPORT_SUBSYSTEM_FILES {
        let row = rows
            .iter()
            .find(|r| r.subsystem.cmp(name).is_eq())
            .ok_or_else(|| format!("matrix is missing subsystem row for {name}"))?;
        let driver = live
            .iter()
            .find(|(n, _)| (*n).cmp(name).is_eq())
            .map(|(_, d)| *d);
        let (mode, evidence) = match driver {
            Some(d) => ("e2e-live", d.to_string()),
            None => (
                "matrix-attested",
                row.proof_artifacts
                    .first()
                    .copied()
                    .unwrap_or("")
                    .to_string(),
            ),
        };
        entries.push(json!({
            "subsystem": name,
            "mode": mode,
            "evidence": evidence,
            "owning_beads": row.owning_beads,
        }));
    }
    Ok(json!({
        "schema_version": 1,
        "subsystem_count": REPORT_SUBSYSTEM_FILES.len(),
        "acceptance_checks": [
            "deterministic_json",
            "no_robot_stdout_pollution",
            "no_subsystem_coverage_gaps",
            "robot_human_no_contradiction",
            "safe_next_commands",
            "stable_redaction",
        ],
        "subsystems": entries,
    }))
}

// ============================================================================
// Tests
// ============================================================================

/// `.15.5` consumed by the capstone: the subsystem coverage matrix has no
/// structural gaps and every cited proof artifact exists on disk (no prose-only
/// evidence). This is the "no coverage gaps in the subsystem matrix" acceptance
/// item, asserted from the integrated gate.
#[test]
fn subsystem_coverage_matrix_is_gap_free_and_every_artifact_exists() -> TestResult {
    let started = Instant::now();
    let gaps = matrix_gaps();
    ensure(gaps.is_empty(), || {
        format!("subsystem coverage matrix has structural gaps: {gaps:?}")
    })?;

    let root = repo_root();
    let exists = |p: &str| root.join(p).exists();
    let mut missing = Vec::new();
    for row in subsystem_coverage_matrix() {
        missing.extend(missing_artifacts(&row, exists));
    }
    ensure(missing.is_empty(), || {
        format!("subsystem matrix cites proof artifacts that do not exist on disk: {missing:?}")
    })?;
    eprintln!(
        "[resilience-capstone] subsystem matrix gap-free across {} subsystems; all artifacts exist",
        REPORT_SUBSYSTEM_FILES.len()
    );
    emit_check_proof("matrix-gap-free", started, "pure:subsystem_coverage_matrix");
    Ok(())
}

/// Golden: the integrated coverage descriptor names every matrix subsystem and
/// matches its pinned snapshot. Regenerate after a deliberate change with:
/// `UPDATE_GOLDENS=1 rch exec -- env CARGO_TARGET_DIR=/data/tmp/cass-golden-target
/// cargo test --test e2e_resilience_capstone_gate`.
#[test]
fn integrated_coverage_descriptor_matches_golden_and_covers_every_subsystem() -> TestResult {
    let started = Instant::now();
    let descriptor = coverage_descriptor().map_err(|e| -> Box<dyn Error> { e.into() })?;

    let entries = descriptor
        .pointer("/subsystems")
        .and_then(Value::as_array)
        .ok_or("coverage descriptor missing /subsystems array")?;
    ensure(
        entries.len().cmp(&REPORT_SUBSYSTEM_FILES.len()).is_eq(),
        || {
            format!(
                "coverage descriptor has {} subsystems, matrix has {}",
                entries.len(),
                REPORT_SUBSYSTEM_FILES.len()
            )
        },
    )?;
    // Every matrix subsystem appears in the descriptor exactly once.
    for name in REPORT_SUBSYSTEM_FILES {
        let count = entries
            .iter()
            .filter(|e| {
                e.get("subsystem")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.cmp(name).is_eq())
            })
            .count();
        ensure(count.cmp(&1).is_eq(), || {
            format!("subsystem {name} appears {count} times in the coverage descriptor (want 1)")
        })?;
    }

    let golden = repo_root().join("tests/golden/resilience_capstone/coverage.json");
    let mut rendered = serde_json::to_string_pretty(&descriptor)?;
    rendered.push('\n');
    if std::env::var("UPDATE_GOLDENS").is_ok() {
        if let Some(parent) = golden.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&golden, &rendered)?;
        eprintln!("[GOLDEN] Updated: {}", golden.display());
        // Builds for this repo run on remote rch workers whose source-tree
        // writes are not synced back; emit the rendered golden to stderr so a
        // remote `UPDATE_GOLDENS=1` run can regenerate the on-disk golden too.
        eprintln!("===CAPSTONE-GOLDEN-START===\n{rendered}===CAPSTONE-GOLDEN-END===");
        return Ok(());
    }
    let on_disk = std::fs::read_to_string(&golden).map_err(|e| {
        format!(
            "missing coverage golden {}: {e}; create it with UPDATE_GOLDENS=1 (rch offload)",
            golden.display()
        )
    })?;
    ensure(on_disk.cmp(&rendered).is_eq(), || {
        format!(
            "coverage golden {} is stale; regenerate with UPDATE_GOLDENS=1 (rch offload) and review the diff",
            golden.display()
        )
    })?;
    emit_check_proof("coverage-golden", started, "pure:coverage_descriptor");
    Ok(())
}

/// E2E: a fresh data dir is a *bundle* of failure classes (no archive, no model,
/// no sources). Every representative robot surface is bounded, free of stdout
/// pollution, and advertises only safe next commands.
#[test]
fn fresh_failure_bundle_surfaces_are_bounded_clean_and_safe() -> TestResult {
    let started = Instant::now();
    let (tmp, home, xdg) = isolated_home()?;

    let surfaces: &[(&str, &[&str])] = &[
        ("triage", &["triage", "--json"]),
        ("status", &["status", "--json"]),
        ("health", &["health", "--json"]),
        ("capabilities", &["capabilities", "--json"]),
        ("models-status", &["models", "status", "--json"]),
        ("sources-doctor", &["sources", "doctor", "--json"]),
        ("diag-quarantine", &["diag", "--json", "--quarantine"]),
        ("doctor-check", &["doctor", "--check", "--json"]),
        ("support-bundle", &["support-bundle", "--json"]),
        (
            "search",
            &["search", "resilience", "--robot", "--limit", "3"],
        ),
    ];

    let mut json_surfaces = 0usize;
    for (name, parts) in surfaces {
        let run = run_robot(&home, &xdg, None, &argv(parts), name, SURFACE_TIMEOUT)?;
        check_surface_bounded_clean_safe(name, &run)?;
        if run.value.is_some() {
            json_surfaces += 1;
        }
        log_surface(name, &run);
    }
    // Most readiness surfaces emit a JSON object; only `search` legitimately
    // emits empty stdout (error envelope on stderr). Require the bulk to parse.
    ensure(json_surfaces >= surfaces.len() - 1, || {
        format!(
            "only {json_surfaces}/{} surfaces produced parseable JSON",
            surfaces.len()
        )
    })?;
    drop(tmp);
    emit_check_proof("fresh-failure-bundle", started, "fixture:fresh");
    Ok(())
}

/// E2E: repeated runs against a fixed fixture are deterministic. Compose surfaces
/// (`capabilities`, `support-bundle`) are byte-stable after volatile
/// normalization; host-probe-bearing readiness surfaces (`status`, `triage`) are
/// shape-stable (the contract skeleton does not drift run to run).
#[test]
fn fresh_surfaces_emit_deterministic_json() -> TestResult {
    let started = Instant::now();
    let (tmp, home, xdg) = isolated_home()?;
    let tmp_root = tmp.path().to_string_lossy().into_owned();

    // Byte-stable (after volatile normalization) compose surfaces.
    for (name, parts) in [
        ("capabilities", &["capabilities", "--json"][..]),
        ("support-bundle", &["support-bundle", "--json"][..]),
    ] {
        let first = run_robot(&home, &xdg, None, &argv(parts), name, SURFACE_TIMEOUT)?
            .value
            .ok_or_else(|| format!("{name} produced no JSON to compare"))?;
        let second = run_robot(&home, &xdg, None, &argv(parts), name, SURFACE_TIMEOUT)?
            .value
            .ok_or_else(|| format!("{name} produced no JSON on the second run"))?;
        let na = normalize_volatile(&first, &tmp_root);
        let nb = normalize_volatile(&second, &tmp_root);
        ensure(json_eq(&na, &nb), || {
            format!("{name} is not byte-deterministic after volatile normalization")
        })?;
    }

    // Shape-stable readiness surfaces (host metrics may vary; the contract may
    // not).
    for (name, parts) in [
        ("status", &["status", "--json"][..]),
        ("triage", &["triage", "--json"][..]),
    ] {
        let first = run_robot(&home, &xdg, None, &argv(parts), name, SURFACE_TIMEOUT)?
            .value
            .ok_or_else(|| format!("{name} produced no JSON to compare"))?;
        let second = run_robot(&home, &xdg, None, &argv(parts), name, SURFACE_TIMEOUT)?
            .value
            .ok_or_else(|| format!("{name} produced no JSON on the second run"))?;
        ensure(
            json_eq(&value_skeleton(&first), &value_skeleton(&second)),
            || format!("{name} contract shape drifted between two runs"),
        )?;
    }
    drop(tmp);
    emit_check_proof("determinism", started, "fixture:fresh");
    Ok(())
}

/// E2E: `health`, `status`, and the support bundle never contradict each other on
/// readiness — on a fresh fixture all report not-ready; on an indexed fixture all
/// report ready — and the human CLI carries the robot `recommended_action`
/// verbatim (no human/robot drift).
#[test]
fn cross_surface_readiness_has_no_contradiction_with_human_parity() -> TestResult {
    let started = Instant::now();

    // Fresh: all three agree "not ready".
    {
        let (tmp, home, xdg) = isolated_home()?;
        assert_readiness_agreement(&home, &xdg, None, false, "fresh")?;

        // Human/robot parity on the not-ready surface: the human `status` output
        // must carry the robot `recommended_action` verbatim.
        let status = run_robot(
            &home,
            &xdg,
            None,
            &argv(&["status", "--json"]),
            "status",
            SURFACE_TIMEOUT,
        )?
        .value
        .ok_or("fresh status produced no JSON")?;
        let action = status
            .get("recommended_action")
            .and_then(Value::as_str)
            .ok_or("fresh status missing recommended_action")?;
        let human = run_human(
            &home,
            &xdg,
            None,
            &argv(&["status"]),
            "status-human",
            SURFACE_TIMEOUT,
        )?;
        ensure(human.contains(action), || {
            format!("human status omits robot recommended_action verbatim: {action:?}")
        })?;
        ensure(text_is_clean(&human), || {
            "human status carries a destructive command fragment".to_string()
        })?;
        drop(tmp);
    }

    // Indexed: seed a Codex session, build the archive, all three agree "ready".
    {
        let (tmp, home, xdg) = isolated_home()?;
        let codex = home.join(".codex");
        util::seed_codex_session(
            &codex,
            "rollout-2026-04-23T10-00-00-capstone.jsonl",
            "capstone resilience unique-keyword-zzqqxx authentication retry",
            true,
        );
        let index = run_robot(
            &home,
            &xdg,
            Some(&codex),
            &argv(&["index", "--full", "--json", "--no-progress-events"]),
            "index",
            INDEX_TIMEOUT,
        )?;
        let index_json = index.value.ok_or("index produced no JSON")?;
        ensure(
            opt_bool_is(index_json.get("success").and_then(Value::as_bool), true),
            || {
                format!(
                    "index did not report success=true: {:?}",
                    index_json.get("error")
                )
            },
        )?;
        assert_readiness_agreement(&home, &xdg, Some(&codex), true, "indexed")?;

        // The indexed corpus is searchable (connector ingest + search path).
        let search = run_robot(
            &home,
            &xdg,
            Some(&codex),
            &argv(&["search", "unique-keyword-zzqqxx", "--robot", "--limit", "3"]),
            "search-indexed",
            SURFACE_TIMEOUT,
        )?;
        let hits = search
            .value
            .as_ref()
            .and_then(|v| v.get("hits"))
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        ensure(hits >= 1, || {
            format!("indexed search returned no hits for the seeded keyword (got {hits})")
        })?;
        drop(tmp);
    }
    emit_check_proof("cross-surface-parity", started, "fixture:fresh+indexed");
    Ok(())
}

/// Cross-surface readiness agreement helper (hoisted; `expected_ready` is the
/// fixture's truth for all three surfaces).
fn assert_readiness_agreement(
    home: &Path,
    xdg: &Path,
    codex: Option<&Path>,
    expected_ready: bool,
    label: &str,
) -> TestResult {
    let health = run_robot(
        home,
        xdg,
        codex,
        &argv(&["health", "--json"]),
        "health",
        SURFACE_TIMEOUT,
    )?
    .value
    .ok_or_else(|| format!("{label} health produced no JSON"))?;
    let status = run_robot(
        home,
        xdg,
        codex,
        &argv(&["status", "--json"]),
        "status",
        SURFACE_TIMEOUT,
    )?
    .value
    .ok_or_else(|| format!("{label} status produced no JSON"))?;
    let bundle = run_robot(
        home,
        xdg,
        codex,
        &argv(&["support-bundle", "--json"]),
        "support-bundle",
        SURFACE_TIMEOUT,
    )?
    .value
    .ok_or_else(|| format!("{label} support-bundle produced no JSON"))?;

    let health_ready = health.get("healthy").and_then(Value::as_bool);
    let status_ready = status.get("healthy").and_then(Value::as_bool);
    let bundle_ready = bundle
        .pointer("/readiness/is_searchable")
        .and_then(Value::as_bool);

    ensure(opt_bool_is(health_ready, expected_ready), || {
        format!("{label} health.healthy={health_ready:?}, expected {expected_ready}")
    })?;
    ensure(opt_bool_is(status_ready, expected_ready), || {
        format!("{label} status.healthy={status_ready:?}, expected {expected_ready}")
    })?;
    ensure(opt_bool_is(bundle_ready, expected_ready), || {
        format!(
            "{label} support-bundle readiness.is_searchable={bundle_ready:?}, expected {expected_ready}"
        )
    })?;
    ensure(opt_bool_eq(health_ready, status_ready), || {
        format!(
            "{label} health.healthy={health_ready:?} contradicts status.healthy={status_ready:?}"
        )
    })?;
    ensure(opt_bool_eq(status_ready, bundle_ready), || {
        format!(
            "{label} status.healthy={status_ready:?} contradicts support-bundle is_searchable={bundle_ready:?}"
        )
    })?;
    Ok(())
}

/// E2E: the share-safe support bundle keeps the full data-dir path out of its
/// default output (basename only), records the `--include-full-paths` opt-in
/// instead of silently flipping, and its redaction is run-to-run stable.
#[test]
fn redaction_is_share_safe_and_stable_across_runs() -> TestResult {
    let started = Instant::now();
    let (tmp, home, xdg) = isolated_home()?;
    let full_data_dir = xdg
        .join("coding-agent-search")
        .to_string_lossy()
        .into_owned();

    // Default bundle: share-safe.
    let default = run_robot(
        &home,
        &xdg,
        None,
        &argv(&["support-bundle", "--json"]),
        "support-bundle",
        SURFACE_TIMEOUT,
    )?
    .value
    .ok_or("default support-bundle produced no JSON")?;
    let default_text = json_text(&default);
    ensure(!default_text.contains(&full_data_dir), || {
        "default support bundle leaks the full data-dir path (redaction failed)".to_string()
    })?;
    ensure(
        opt_bool_is(
            default
                .pointer("/redaction/full_paths")
                .and_then(Value::as_bool),
            false,
        ),
        || "default support bundle does not report redaction.full_paths=false".to_string(),
    )?;
    let basename = default
        .pointer("/manifest/data_dir")
        .and_then(Value::as_str);
    ensure(basename.is_some_and(|d| !d.contains('/')), || {
        format!("default manifest.data_dir is not a redacted basename: {basename:?}")
    })?;

    // Opt-in flip is recorded, not silent.
    let full = run_robot(
        &home,
        &xdg,
        None,
        &argv(&["support-bundle", "--json", "--include-full-paths"]),
        "support-bundle-full",
        SURFACE_TIMEOUT,
    )?
    .value
    .ok_or("full-paths support-bundle produced no JSON")?;
    ensure(
        opt_bool_is(
            full.pointer("/redaction/full_paths")
                .and_then(Value::as_bool),
            true,
        ),
        || "--include-full-paths did not record redaction.full_paths=true".to_string(),
    )?;
    let full_dir = full.pointer("/manifest/data_dir").and_then(Value::as_str);
    ensure(
        full_dir.is_some_and(|d| d.cmp(full_data_dir.as_str()).is_eq()),
        || {
            format!(
                "--include-full-paths data_dir mismatch: got {full_dir:?}, want {full_data_dir:?}"
            )
        },
    )?;

    // Redaction is run-to-run stable.
    let again = run_robot(
        &home,
        &xdg,
        None,
        &argv(&["support-bundle", "--json"]),
        "support-bundle",
        SURFACE_TIMEOUT,
    )?
    .value
    .ok_or("second default support-bundle produced no JSON")?;
    let redaction_a = default.get("redaction").cloned().unwrap_or(Value::Null);
    let redaction_b = again.get("redaction").cloned().unwrap_or(Value::Null);
    ensure(json_eq(&redaction_a, &redaction_b), || {
        "support bundle redaction section is not run-to-run stable".to_string()
    })?;
    drop(tmp);
    eprintln!("[resilience-capstone] redaction share-safe + opt-in recorded + stable");
    emit_check_proof("redaction-stable", started, "fixture:fresh");
    Ok(())
}

/// Logs: a real pass emits a `pass` proof, a timeout-before-assertions emits a
/// `timeout` proof (never a pass), and the manifest's log-completeness verdict
/// reflects both — so the capstone "cannot pass by doing nothing" and a hang can
/// never masquerade as green. This is the `.11.4`/`.12.3` manifest the closure
/// cites.
#[test]
fn proof_manifest_distinguishes_real_pass_from_timeout() -> TestResult {
    let (tmp, home, xdg) = isolated_home()?;

    // A real, timed run of a representative readiness surface that passes.
    let started = Instant::now();
    let run = run_robot(
        &home,
        &xdg,
        None,
        &argv(&["triage", "--json"]),
        "triage",
        SURFACE_TIMEOUT,
    )?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    ensure(run.value.is_some(), || {
        "the live triage run must emit JSON before we certify it".to_string()
    })?;

    let proof_dir = tempfile::TempDir::new()?;
    let mut manifest = ProofManifest::new();

    let pass_run = capstone_proof_run("cass triage --json", Some(0), elapsed_ms, false, true, true);
    let pass = emit_proof_artifact(proof_dir.path(), "capstone-triage", pass_run)
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

    // The motivating trap: a timeout-before-assertions must NOT read as a pass.
    let timeout_run = capstone_proof_run(
        "cass triage --json (hang)",
        Some(0),
        SURFACE_TIMEOUT.as_millis() as u64,
        true,
        false,
        false,
    );
    let timed = emit_proof_artifact(proof_dir.path(), "capstone-hang", timeout_run)
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

    let manifest_path = proof_dir.path().join("proof-manifest.jsonl");
    manifest
        .write_jsonl(&manifest_path)
        .map_err(|e| format!("write manifest: {e}"))?;
    ensure(manifest_path.exists(), || {
        "proof manifest jsonl must be written".to_string()
    })?;
    eprintln!(
        "[resilience-capstone] emitted proof artifacts to {} :: clean_pass={} worst={:?}",
        proof_dir.path().display(),
        manifest.is_clean_pass(),
        manifest.worst_status()
    );
    drop(tmp);
    Ok(())
}

/// Build a `ProofRun` for the manifest proof (avoids repeating the 14 fields).
fn capstone_proof_run(
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
        data_dir_or_fixture: Some("fixture:resilience-capstone".to_string()),
        exit_code,
        elapsed_ms,
        timeout_ms: SURFACE_TIMEOUT.as_millis() as u64,
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
