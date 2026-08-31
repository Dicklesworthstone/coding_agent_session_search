//! Real-binary truth-surface parity gate for the `.14.1` storage-integrity
//! block (bead `coding_agent_session_search-qfswx`, follow-on to `vl1cj`).
//!
//! `vl1cj` wired `src/search/storage_integrity.rs`'s `StorageState` /
//! `SourceOfTruthRisk` / `ArchiveReadability` vocabulary into the deep surface,
//! `cass doctor --check --json`. `qfswx` projects the SAME block onto the two
//! lightweight readiness surfaces — `cass status --json` (top-level
//! `storage_integrity`) and `cass search … --robot-meta` (`_meta.storage_integrity`)
//! — so every truth surface agrees on the canonical storage vocabulary.
//!
//! This gate drives the REAL `cass` binary against isolated tempdir fixtures and
//! asserts two things the unit tests cannot:
//!   1. **Parity:** on a healthy indexed archive, doctor/status/search all report
//!      `storage_state = "ok"` (risk `none`, archive `readable`); on a fresh
//!      uninitialized data dir, doctor and status agree (`ok` / `not_checked`).
//!   2. **Honest provenance:** the lightweight surfaces record only a `db_open`
//!      check and NEVER claim the deep `archive_integrity` probe ran, while the
//!      doctor (which does run it) records `archive_integrity`. This proves the
//!      surfaces speak one vocabulary without the cheap ones over-claiming a
//!      depth they never reached.
//!
//! Authored panic-free (every helper and `#[test]` returns `Result<_, String>`,
//! no `unwrap`/`expect`/`assert!`/`panic!`) so the new file stays at 0
//! critical / 0 warning under the UBS regression gate. A genuine command hang is
//! the one loud exception: the bounded runner (`spawn_with_timeout_or_diag`)
//! dumps a timeout diagnostic and aborts, categorically distinct from a parity
//! `Err`.

mod util;

use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

/// Per-surface wall-clock bound. The readiness surfaces are sub-second against
/// the isolated fixtures; this generous bound only fires on a true hang and
/// holds under heavy multi-agent host contention.
const SURFACE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bound for the one-time index that builds the `indexed` fixture (a single
/// tiny seeded session indexes in well under a second; the bound is slack).
const INDEX_TIMEOUT: Duration = Duration::from_secs(120);

/// A unique keyword the seeded session carries, so search finds exactly it.
const KEYWORD: &str = "storage-state-parity-probe unique-keyword-qfswx";

/// An isolated fixture: a tempdir-backed HOME + data dir.
struct Fixture {
    /// Kept for RAII cleanup; commands must not outlive it.
    _home: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    /// `Some` when a seeded Codex session should be discoverable (indexed
    /// state); `None` for the fresh state (sources ignored, `CODEX_HOME` gone).
    codex_home: Option<PathBuf>,
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
    })
}

/// The indexed state: a single tiny seeded Codex session indexed once → a
/// healthy archive. Building this fixture *is itself* a real-binary `cass index`
/// invocation through the bounded runner.
fn indexed_fixture() -> Result<Fixture, String> {
    let (home, home_path, data_dir) = isolated_home()?;
    let codex_home = home_path.join(".codex");
    util::seed_codex_session(
        &codex_home,
        "rollout-2026-04-23T10-00-00-storage-parity.jsonl",
        KEYWORD,
        true,
    );
    let fixture = Fixture {
        _home: home,
        home: home_path,
        data_dir,
        codex_home: Some(codex_home),
    };

    let argv = build_argv(
        &fixture,
        &["index", "--full", "--json", "--no-progress-events"],
    );
    let out = run(&fixture, &argv, "indexed_fixture_index", INDEX_TIMEOUT);
    let value = parse_json(&out, "indexed_fixture_index")?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "index fixture did not report success=true: {}",
            compact(&value)
        ));
    }
    Ok(fixture)
}

/// Append the shared `--data-dir <dir>` tail (index/status/doctor/search all
/// accept it) to a base argv.
fn build_argv(fixture: &Fixture, base: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    v.push("--data-dir".to_string());
    v.push(fixture.data_dir.to_string_lossy().into_owned());
    v
}

/// Build a `cass` command with the fixture's isolation env. The fresh fixture
/// ignores sources config and removes `CODEX_HOME`; the indexed fixture points
/// `CODEX_HOME` at its seeded session so exactly that one session is found.
fn cass_command(fixture: &Fixture, argv: &[String]) -> Command {
    let home = &fixture.home;
    let mut cmd = Command::new(util::cass_bin());
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

/// Run a surface through the bounded runner. A hang dumps the runner's
/// `TIMEOUT DIAGNOSTIC` and aborts — the one loud exception to the no-panic rule.
fn run(fixture: &Fixture, argv: &[String], label: &str, timeout: Duration) -> Output {
    let cmd = cass_command(fixture, argv);
    spawn_with_timeout_or_diag(cmd, label, Some(&fixture.data_dir), timeout)
}

/// Parse a surface's pure-JSON stdout into a `Value`.
fn parse_json(out: &Output, label: &str) -> Result<Value, String> {
    let stdout =
        std::str::from_utf8(&out.stdout).map_err(|e| format!("{label} stdout not UTF-8: {e}"))?;
    serde_json::from_str(stdout.trim())
        .map_err(|e| format!("{label} stdout not JSON: {e}; head: {}", head(stdout)))
}

/// Pluck the storage-integrity block at `pointer` from a surface payload.
fn storage_block<'a>(root: &'a Value, pointer: &str, label: &str) -> Result<&'a Value, String> {
    root.pointer(pointer)
        .ok_or_else(|| format!("{label}: missing storage_integrity at {pointer}"))
}

/// Read a string field from a storage block (e.g. `storage_state`).
fn block_str(block: &Value, field: &str, label: &str) -> Result<String, String> {
    block
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{label}: storage_integrity.{field} missing or non-string"))
}

/// Whether the block's `checks_attempted` records a check with the given name.
fn has_check_named(block: &Value, name: &str) -> bool {
    block
        .get("checks_attempted")
        .and_then(Value::as_array)
        .is_some_and(|checks| {
            checks
                .iter()
                .filter_map(|c| c.get("name").and_then(Value::as_str))
                .any(|n| n == name)
        })
}

fn head(s: &str) -> String {
    s.chars().take(400).collect()
}

// --- diagnostic-string builders, kept OUT of loop bodies so the bug scanner's
//     "allocation inside a loop" heuristic stays quiet: the parity loop calls a
//     flat helper instead of inlining `format!`. ---

fn missing_db_open_check_msg(label: &str) -> String {
    format!("{label}: storage_integrity must record a db_open check")
}

fn claimed_deep_probe_msg(label: &str) -> String {
    format!("{label}: lightweight surface must NOT claim the deep archive_integrity probe ran")
}

fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "<unserializable>".to_string())
}

/// Assert a block's `(storage_state, source_of_truth_risk, archive_readability)`
/// triple matches the expected wire labels.
fn expect_triple(
    block: &Value,
    label: &str,
    state: &str,
    risk: &str,
    readability: &str,
) -> Result<(), String> {
    let got_state = block_str(block, "storage_state", label)?;
    if got_state != state {
        return Err(format!(
            "{label}: storage_state = {got_state:?}, want {state:?}"
        ));
    }
    let got_risk = block_str(block, "source_of_truth_risk", label)?;
    if got_risk != risk {
        return Err(format!(
            "{label}: source_of_truth_risk = {got_risk:?}, want {risk:?}"
        ));
    }
    let got_read = block_str(block, "archive_readability", label)?;
    if got_read != readability {
        return Err(format!(
            "{label}: archive_readability = {got_read:?}, want {readability:?}"
        ));
    }
    Ok(())
}

/// On a healthy indexed archive, doctor/status/search must all project the same
/// `ok` storage state — and the lightweight surfaces must record only a
/// `db_open` check, never the deep `archive_integrity` probe (which only the
/// doctor runs).
#[test]
fn indexed_archive_storage_state_agrees_across_doctor_status_search() -> Result<(), String> {
    let fixture = indexed_fixture()?;

    let doctor_out = run(
        &fixture,
        &build_argv(&fixture, &["doctor", "--check", "--json"]),
        "doctor/indexed",
        SURFACE_TIMEOUT,
    );
    let doctor = parse_json(&doctor_out, "doctor/indexed")?;
    let doctor_block = storage_block(&doctor, "/storage_integrity", "doctor/indexed")?;

    let status_out = run(
        &fixture,
        &build_argv(&fixture, &["status", "--json"]),
        "status/indexed",
        SURFACE_TIMEOUT,
    );
    let status = parse_json(&status_out, "status/indexed")?;
    let status_block = storage_block(&status, "/storage_integrity", "status/indexed")?;

    let search_out = run(
        &fixture,
        &build_argv(
            &fixture,
            &["search", KEYWORD, "--json", "--robot-meta", "--limit", "5"],
        ),
        "search/indexed",
        SURFACE_TIMEOUT,
    );
    let search = parse_json(&search_out, "search/indexed")?;
    let search_block = storage_block(&search, "/_meta/storage_integrity", "search/indexed")?;

    // Parity: a healthy archive is `ok` / `none` / `readable` on every surface.
    expect_triple(doctor_block, "doctor/indexed", "ok", "none", "readable")?;
    expect_triple(status_block, "status/indexed", "ok", "none", "readable")?;
    expect_triple(search_block, "search/indexed", "ok", "none", "readable")?;

    // Honest provenance: the lightweight surfaces ran `db_open` and NEVER claim
    // the deep integrity probe ran; the doctor (which does run it) records it.
    for (label, block) in [
        ("status/indexed", status_block),
        ("search/indexed", search_block),
    ] {
        if !has_check_named(block, "db_open") {
            return Err(missing_db_open_check_msg(label));
        }
        if has_check_named(block, "archive_integrity") {
            return Err(claimed_deep_probe_msg(label));
        }
    }
    if !has_check_named(doctor_block, "archive_integrity") {
        return Err(
            "doctor/indexed: storage_integrity must record the deep archive_integrity probe"
                .to_string(),
        );
    }

    Ok(())
}

/// On a fresh, uninitialized data dir, doctor and status must agree that storage
/// is vacuously `ok` with the archive `not_checked` (no DB to read yet).
#[test]
fn fresh_data_dir_storage_state_agrees_between_doctor_and_status() -> Result<(), String> {
    let fixture = fresh_fixture()?;

    let doctor_out = run(
        &fixture,
        &build_argv(&fixture, &["doctor", "--check", "--json"]),
        "doctor/fresh",
        SURFACE_TIMEOUT,
    );
    let doctor = parse_json(&doctor_out, "doctor/fresh")?;
    let doctor_block = storage_block(&doctor, "/storage_integrity", "doctor/fresh")?;

    let status_out = run(
        &fixture,
        &build_argv(&fixture, &["status", "--json"]),
        "status/fresh",
        SURFACE_TIMEOUT,
    );
    let status = parse_json(&status_out, "status/fresh")?;
    let status_block = storage_block(&status, "/storage_integrity", "status/fresh")?;

    // Fresh-empty: vacuously ok, nothing read.
    expect_triple(doctor_block, "doctor/fresh", "ok", "none", "not_checked")?;
    expect_triple(status_block, "status/fresh", "ok", "none", "not_checked")?;

    // The two surfaces agree by construction; assert the storage_state pair too.
    let ds = block_str(doctor_block, "storage_state", "doctor/fresh")?;
    let ss = block_str(status_block, "storage_state", "status/fresh")?;
    if ds != ss {
        return Err(format!(
            "fresh: doctor storage_state {ds:?} != status {ss:?}"
        ));
    }

    Ok(())
}
