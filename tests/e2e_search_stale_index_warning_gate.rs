//! Real-binary gate for bead `coding_agent_session_search-i6tyy` (GH #375
//! tail note, GH #353): a search served from a lexical index older than the
//! stale threshold must SAY so — inline in `--robot-meta` (`_meta._warning`)
//! and as a one-line stderr note in human output — because zero hits on a
//! stale index is otherwise indistinguishable from "that history does not
//! exist".
//!
//! Before i6tyy the robot warning existed but the human note could never
//! fire: index freshness was a `--robot-meta`-only probe, so human search had
//! nothing to warn from. This gate pins both surfaces against the real
//! binary and proves the human probe is mutation-free (the searchable index
//! manifest and the canonical DB are byte-identical before/after).
//!
//! Staleness is induced deterministically: the strict-read (no DB open)
//! freshness path derives `last_indexed_at` from the searchable index's
//! publication manifest mtime, so ageing that mtime past the default
//! threshold (1800s) is exactly the on-disk signature of an old index.

mod util;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

const SURFACE_TIMEOUT: Duration = Duration::from_secs(60);
const INDEX_TIMEOUT: Duration = Duration::from_secs(120);
const KEYWORD: &str = "stale-index-warning-probe-qwertzuiop";
/// Comfortably past `DEFAULT_STALE_THRESHOLD_SECS` (1800).
const AGE: Duration = Duration::from_secs(4 * 3600);

struct Fixture {
    _home: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    codex_home: PathBuf,
}

fn cass(fixture: &Fixture, args: &[&str]) -> Command {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(args)
        .arg("--data-dir")
        .arg(&fixture.data_dir)
        .current_dir(&fixture.home)
        .env("HOME", &fixture.home)
        .env("XDG_DATA_HOME", fixture.home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", fixture.home.join("xdg-config"))
        .env("XDG_CACHE_HOME", fixture.home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("CODEX_HOME", &fixture.codex_home)
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

fn run(fixture: &Fixture, label: &str, args: &[&str], timeout: Duration) -> Output {
    spawn_with_timeout_or_diag(cass(fixture, args), label, Some(&fixture.data_dir), timeout)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn indexed_fixture() -> Result<Fixture, String> {
    let home = tempfile::tempdir().map_err(|e| format!("create tempdir: {e}"))?;
    let home_path = home.path().to_path_buf();
    let data_dir = home_path.join("cass-data");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("create data dir: {e}"))?;
    let codex_home = home_path.join(".codex");
    util::seed_codex_session(
        &codex_home,
        "rollout-2026-04-23T10-00-00-stale.jsonl",
        KEYWORD,
        true,
    );
    let fixture = Fixture {
        _home: home,
        home: home_path,
        data_dir,
        codex_home,
    };
    let out = run(
        &fixture,
        "stale_gate_index",
        &["index", "--full", "--json", "--no-progress-events"],
        INDEX_TIMEOUT,
    );
    let value: Value = serde_json::from_str(text(&out.stdout).trim())
        .map_err(|e| format!("index stdout not JSON: {e}; stderr: {}", text(&out.stderr)))?;
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(format!("index did not succeed: {value}"));
    }
    Ok(fixture)
}

/// The searchable index's publication authority (Quill `MANIFEST`, or the
/// legacy `meta.json`), whose mtime is the strict-read `last_indexed_at`.
fn index_manifest(data_dir: &Path) -> Result<PathBuf, String> {
    // Layout is `index/<schema-version>/MANIFEST`; scan one level down.
    let index = data_dir.join("index");
    let mut roots = vec![index.clone()];
    if let Ok(entries) = std::fs::read_dir(&index) {
        roots.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir()),
        );
    }
    for root in roots {
        for candidate in [root.join("MANIFEST"), root.join("meta.json")] {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "no searchable index manifest under {}",
        index.display()
    ))
}

fn age_manifest(path: &Path) -> Result<(), String> {
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .map_err(|e| format!("open {} for mtime: {e}", path.display()))?;
    let old = SystemTime::now()
        .checked_sub(AGE)
        .ok_or_else(|| "clock underflow".to_string())?;
    file.set_modified(old)
        .map_err(|e| format!("set mtime on {}: {e}", path.display()))
}

fn snapshot(path: &Path) -> Result<(Vec<u8>, SystemTime), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("mtime {}: {e}", path.display()))?;
    Ok((bytes, mtime))
}

fn stale_line(stderr: &str) -> Option<&str> {
    stderr
        .lines()
        .find(|line| line.starts_with("Warning: Index may be stale"))
}

fn check() -> Result<(), String> {
    let fixture = indexed_fixture()?;
    let manifest = index_manifest(&fixture.data_dir)?;
    let db_path = fixture.data_dir.join("agent_search.db");

    // Fresh index: hits, and NO stale note on either surface.
    let fresh = run(
        &fixture,
        "human_fresh",
        &["search", KEYWORD],
        SURFACE_TIMEOUT,
    );
    let fresh_out = text(&fresh.stdout);
    let fresh_err = text(&fresh.stderr);
    if !fresh_out.contains(KEYWORD) {
        return Err(format!(
            "fresh human search lost its hit; stdout: {fresh_out}"
        ));
    }
    if let Some(line) = stale_line(&fresh_err) {
        return Err(format!("fresh index must not warn stale: {line}"));
    }

    // Age the published index past the stale threshold.
    age_manifest(&manifest)?;
    let manifest_before = snapshot(&manifest)?;
    let db_before = snapshot(&db_path)?;

    // Human surface: one-line stderr note; stdout still carries the hit.
    let human = run(
        &fixture,
        "human_stale",
        &["search", KEYWORD],
        SURFACE_TIMEOUT,
    );
    let human_out = text(&human.stdout);
    let human_err = text(&human.stderr);
    if !human_out.contains(KEYWORD) {
        return Err(format!(
            "stale human search lost its hit; stdout: {human_out}"
        ));
    }
    let human_line = stale_line(&human_err)
        .ok_or_else(|| format!("human search did not warn stale; stderr: {human_err}"))?;
    if human_out.contains("Index may be stale") {
        return Err("stale note leaked onto stdout (must stay stderr-only)".to_string());
    }
    if !human_line.contains("cass index") {
        return Err(format!(
            "stale note must name the refresh command: {human_line}"
        ));
    }
    // The human probe is mutation-free: manifest bytes+mtime and DB bytes are
    // untouched by the human search (snapshot taken before it ran).
    let manifest_after = snapshot(&manifest)?;
    let db_after = snapshot(&db_path)?;
    if manifest_after.0.cmp(&manifest_before.0).is_ne() || manifest_after.1 != manifest_before.1 {
        return Err("human search rewrote the index manifest".to_string());
    }
    if db_after.0.cmp(&db_before.0).is_ne() {
        return Err("human search rewrote the canonical DB".to_string());
    }

    // Robot surface: `_meta._warning` carries the identical text.
    let robot = run(
        &fixture,
        "robot_stale",
        &[
            "search",
            KEYWORD,
            "--json",
            "--robot-meta",
            "--no-maintenance",
        ],
        SURFACE_TIMEOUT,
    );
    let robot_json: Value = serde_json::from_str(text(&robot.stdout).trim()).map_err(|e| {
        format!(
            "robot stdout not JSON: {e}; stderr: {}",
            text(&robot.stderr)
        )
    })?;
    // Robot envelope carries the warning at top level `_warning` and mirrors
    // it under `_meta.state._warning`.
    let robot_warning = robot_json
        .get("_warning")
        .or_else(|| {
            robot_json
                .get("_meta")
                .and_then(|m| m.get("state"))
                .and_then(|s| s.get("_warning"))
        })
        .and_then(Value::as_str)
        .ok_or_else(|| format!("robot _warning missing: {robot_json}"))?;
    let expected_human = format!("Warning: {robot_warning}");
    if human_line.cmp(expected_human.as_str()).is_ne() {
        return Err(format!(
            "human/robot stale text drifted:\n  human: {human_line}\n  robot: {expected_human}"
        ));
    }
    let robot_stale = robot_json
        .get("_meta")
        .and_then(|m| m.get("index_freshness"))
        .and_then(|f| f.get("stale"))
        .and_then(Value::as_bool);
    if robot_stale.is_none_or(|stale| !stale) {
        return Err(format!(
            "robot index_freshness.stale must be true: {robot_json}"
        ));
    }

    Ok(())
}

#[test]
fn human_and_robot_search_warn_when_lexical_index_is_stale() -> Result<(), String> {
    check()
}
