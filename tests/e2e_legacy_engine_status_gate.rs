//! Real-binary gate for bead `coding_agent_session_search-b4uax` (gh#382):
//! an established archive whose lexical generation was written by the legacy
//! Tantivy engine (`meta.json`, no Quill `MANIFEST`) must be reported by
//! `cass status --json` as `legacy_engine` / `engine_incompatible`, with the
//! readiness class demanding a rebuild — never as `ready`/fresh.
//!
//! The legacy shape is produced deterministically from a real index: build a
//! Quill generation, then rename its `MANIFEST` to `meta.json` (rename only;
//! nothing is deleted).

mod util;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

const INDEX_TIMEOUT: Duration = Duration::from_secs(120);
const SURFACE_TIMEOUT: Duration = Duration::from_secs(60);
const KEYWORD: &str = "legacy-engine-gate-keyword-vbnm";

struct Fixture {
    _home: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    codex_home: PathBuf,
}

fn run(fixture: &Fixture, label: &str, args: &[&str], timeout: Duration) -> Output {
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
    spawn_with_timeout_or_diag(cmd, label, Some(&fixture.data_dir), timeout)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn indexed_fixture() -> Result<Fixture, String> {
    let home = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let home_path = home.path().to_path_buf();
    let data_dir = home_path.join("cass-data");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("data dir: {e}"))?;
    let codex_home = home_path.join(".codex");
    util::seed_codex_session(
        &codex_home,
        "rollout-2026-04-23T10-00-00-legacy.jsonl",
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
        "legacy_gate_index",
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

/// The Quill generation directory (`index/<schema>/`) holding `MANIFEST`.
fn quill_generation_dir(data_dir: &Path) -> Result<PathBuf, String> {
    let index = data_dir.join("index");
    let entries =
        std::fs::read_dir(&index).map_err(|e| format!("read {}: {e}", index.display()))?;
    entries
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.join("MANIFEST").is_file())
        .ok_or_else(|| {
            format!(
                "no Quill generation with MANIFEST under {}",
                index.display()
            )
        })
}

fn status_json(fixture: &Fixture, label: &str) -> Result<Value, String> {
    let out = run(fixture, label, &["status", "--json"], SURFACE_TIMEOUT);
    serde_json::from_str(text(&out.stdout).trim()).map_err(|e| {
        format!(
            "{label}: status stdout not JSON: {e}; stderr: {}",
            text(&out.stderr)
        )
    })
}

fn str_at<'a>(v: &'a Value, ptr: &str) -> Option<&'a str> {
    v.pointer(ptr).and_then(Value::as_str)
}

fn bool_at(v: &Value, ptr: &str) -> Option<bool> {
    v.pointer(ptr).and_then(Value::as_bool)
}

fn check() -> Result<(), String> {
    let fixture = indexed_fixture()?;

    // Control: the fresh Quill generation is not engine-incompatible.
    let healthy = status_json(&fixture, "status_quill")?;
    if bool_at(&healthy, "/index/engine_incompatible") != Some(false) {
        return Err(format!(
            "fresh Quill generation must not be engine_incompatible: {healthy}"
        ));
    }
    if str_at(&healthy, "/index/status") == Some("legacy_engine") {
        return Err("fresh Quill generation reported legacy_engine".to_string());
    }

    // Turn the generation into the legacy Tantivy shape: rename (not delete)
    // MANIFEST to meta.json.
    let generation = quill_generation_dir(&fixture.data_dir)?;
    std::fs::rename(generation.join("MANIFEST"), generation.join("meta.json"))
        .map_err(|e| format!("rename MANIFEST -> meta.json: {e}"))?;

    let legacy = status_json(&fixture, "status_legacy")?;
    let mut failures = Vec::new();
    if bool_at(&legacy, "/index/exists") != Some(true) {
        failures
            .push("legacy generation must still count as existing (flip = REBUILD)".to_string());
    }
    if bool_at(&legacy, "/index/engine_incompatible") != Some(true) {
        failures.push(format!(
            "engine_incompatible not reported: {:?}",
            legacy.pointer("/index/engine_incompatible")
        ));
    }
    if str_at(&legacy, "/index/status") != Some("legacy_engine") {
        failures.push(format!(
            "index.status should be legacy_engine, got {:?}",
            str_at(&legacy, "/index/status")
        ));
    }
    if bool_at(&legacy, "/index/fresh") != Some(false) {
        failures.push("legacy generation must not be reported fresh".to_string());
    }
    let reason = str_at(&legacy, "/index/reason").unwrap_or("");
    if !reason.contains("cass index --full") || !reason.contains("Tantivy") {
        failures.push(format!(
            "reason must name the rebuild command and engine: {reason:?}"
        ));
    }
    // Readiness: search is not usable; the recommendation is a rebuild now.
    // `status` speaks the human sentence; it must name the full rebuild and
    // the engine flip, never the incremental "refresh" advice.
    let recommended = str_at(&legacy, "/recommended_action").unwrap_or("");
    if !recommended.contains("cass index --full")
        || !recommended.contains("Tantivy")
        || recommended.contains("to refresh the index")
    {
        failures.push(format!(
            "recommended_action should name the full rebuild for the engine flip, got {recommended:?}"
        ));
    }
    let commands = legacy
        .pointer("/recommended_commands")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    entry
                        .as_str()
                        .or_else(|| entry.get("command").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default();
    if !commands.contains("cass index --full") {
        failures.push(format!(
            "recommended_commands should name the full rebuild, got {commands:?}"
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

#[test]
fn status_reports_legacy_tantivy_generation_as_engine_incompatible() -> Result<(), String> {
    check()
}
