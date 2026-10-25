//! Real-binary gate for bead `coding_agent_session_search-b4uax` (gh#382):
//! an established archive whose lexical generation was written by the legacy
//! Tantivy engine (`meta.json`, no Quill `MANIFEST`) must be reported by
//! `cass status --json` as `legacy_engine` / `engine_incompatible`, with the
//! readiness class demanding a rebuild — never as `ready`/fresh.
//!
//! The legacy shape is produced deterministically from a real index: build a
//! Quill generation, then rename its `MANIFEST` to `meta.json` (rename only;
//! nothing is deleted).
//!
//! Bead `coding_agent_session_search-mknmt`: a corrupt current `MANIFEST` is
//! read through `MANIFEST.prev`; status, health and search robot-meta must say
//! so, and `cass index` must rebuild the newest publish.

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

// mknmt: a corrupt current MANIFEST is read through MANIFEST.prev, so search
// silently served the generation before the newest publish while health and
// status called the index ready.
const SECOND_KEYWORD: &str = "manifest-slot-gate-keyword-qwer";
const CORRUPT_SOURCE: &str = "previous_after_corrupt_current";

fn json_run(fixture: &Fixture, label: &str, args: &[&str]) -> Result<(Output, Value), String> {
    let out = run(fixture, label, args, INDEX_TIMEOUT);
    let value = serde_json::from_str(text(&out.stdout).trim()).map_err(|e| {
        format!(
            "{label}: stdout not JSON: {e}; stderr: {}",
            text(&out.stderr)
        )
    })?;
    Ok((out, value))
}

fn index_ok(fixture: &Fixture, label: &str, args: &[&str]) -> Result<(), String> {
    let (_, value) = json_run(fixture, label, args)?;
    if value.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(format!("{label}: index did not succeed: {value}"))
    }
}

/// Lexical hits for `keyword`, plus the search's robot-meta.
fn search_hits(fixture: &Fixture, label: &str, keyword: &str) -> Result<(usize, Value), String> {
    let (_, value) = json_run(
        fixture,
        label,
        &[
            "search",
            keyword,
            "--json",
            "--robot-meta",
            "--mode",
            "lexical",
            "--limit",
            "5",
        ],
    )?;
    let hits = value
        .get("hits")
        .and_then(Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| format!("{label}: no hits array: {value}"))?;
    Ok((hits, value))
}

fn corrupt_current_manifest(data_dir: &Path) -> Result<(), String> {
    let manifest = quill_generation_dir(data_dir)?.join("MANIFEST");
    let mut bytes = std::fs::read(&manifest).map_err(|e| format!("read MANIFEST: {e}"))?;
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    std::fs::write(&manifest, &bytes).map_err(|e| format!("corrupt MANIFEST: {e}"))
}

/// The newest publish is served again: current slot, not stale for it, and
/// the second session's keyword is found.
fn expect_repaired(fixture: &Fixture, label: &str) -> Result<(), String> {
    let status = status_json(fixture, &format!("{label}_status"))?;
    if str_at(&status, "/index/manifest_source") != Some("current") {
        return Err(format!("{label}: manifest_source after repair: {status}"));
    }
    if str_at(&status, "/index/reason").is_some_and(|reason| reason.contains("MANIFEST")) {
        return Err(format!(
            "{label}: corrupt-MANIFEST reason survived repair: {status}"
        ));
    }
    let (hits, _) = search_hits(fixture, &format!("{label}_search"), SECOND_KEYWORD)?;
    if hits == 0 {
        return Err(format!("{label}: the newest publish is still not served"));
    }
    Ok(())
}

fn check_corrupt_current_manifest() -> Result<(), String> {
    let fixture = indexed_fixture()?;
    // A second publish after the full build, so MANIFEST.prev holds a real
    // generation (the first session only), not the empty genesis.
    util::seed_codex_session(
        &fixture.codex_home,
        "rollout-2026-04-23T11-00-00-second.jsonl",
        SECOND_KEYWORD,
        true,
    );
    index_ok(
        &fixture,
        "slot_incremental",
        &["index", "--json", "--no-progress-events"],
    )?;
    let control = status_json(&fixture, "slot_status_current")?;
    if str_at(&control, "/index/manifest_source") != Some("current") {
        return Err(format!("intact MANIFEST must read as current: {control}"));
    }
    let (hits, _) = search_hits(&fixture, "slot_search_current", SECOND_KEYWORD)?;
    if hits == 0 {
        return Err("the second session must be searchable before corruption".to_string());
    }

    corrupt_current_manifest(&fixture.data_dir)?;
    let mut failures = Vec::new();
    let status = status_json(&fixture, "slot_status_corrupt")?;
    if str_at(&status, "/index/manifest_source") != Some(CORRUPT_SOURCE) {
        failures.push(format!(
            "status index.manifest_source: {:?}",
            status.pointer("/index/manifest_source")
        ));
    }
    if str_at(&status, "/index/status") != Some("stale")
        || bool_at(&status, "/index/fresh") != Some(false)
    {
        failures.push(format!(
            "a generation one publish behind is stale, not ready: status={:?} fresh={:?}",
            str_at(&status, "/index/status"),
            bool_at(&status, "/index/fresh")
        ));
    }
    let reason = str_at(&status, "/index/reason").unwrap_or("");
    if !reason.contains("MANIFEST.prev") || !reason.contains("corrupt") {
        failures.push(format!("status reason must name the slot: {reason:?}"));
    }
    let recommended = str_at(&status, "/recommended_action").unwrap_or("");
    if !recommended.contains("cass index") || !recommended.contains("MANIFEST.prev") {
        failures.push(format!("status recommended_action: {recommended:?}"));
    }
    let (_, health) = json_run(&fixture, "slot_health_corrupt", &["health", "--json"])?;
    if str_at(&health, "/state/index/manifest_source") != Some(CORRUPT_SOURCE) {
        failures.push(format!(
            "health state.index.manifest_source: {:?}",
            health.pointer("/state/index/manifest_source")
        ));
    }
    if bool_at(&health, "/healthy") != Some(false) {
        failures.push("health must not call a corrupt MANIFEST healthy".to_string());
    }
    let errors = health
        .get("errors")
        .and_then(Value::as_array)
        .map(|errors| {
            errors
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default();
    if !errors.contains("MANIFEST corrupt") {
        failures.push(format!(
            "health errors must name the corrupt MANIFEST: {errors:?}"
        ));
    }
    // What readers serve: the first session, not the newest publish.
    let (first_hits, _) = search_hits(&fixture, "slot_search_first", KEYWORD)?;
    let (second_hits, meta) = search_hits(&fixture, "slot_search_second", SECOND_KEYWORD)?;
    if first_hits == 0 || second_hits != 0 {
        failures.push(format!(
            "readers should serve MANIFEST.prev: first={first_hits} second={second_hits}"
        ));
    }
    if str_at(&meta, "/_meta/index_freshness/manifest_source") != Some(CORRUPT_SOURCE) {
        failures.push(format!(
            "search robot-meta index_freshness.manifest_source: {:?}",
            meta.pointer("/_meta/index_freshness/manifest_source")
        ));
    }
    if !failures.is_empty() {
        return Err(failures.join("\n"));
    }

    // The advised command repairs it: a plain incremental run rebuilds the
    // generation from the canonical database even though nothing changed.
    index_ok(
        &fixture,
        "slot_repair_incremental",
        &["index", "--json", "--no-progress-events"],
    )?;
    expect_repaired(&fixture, "slot_after_index")?;

    // So does a full run.
    corrupt_current_manifest(&fixture.data_dir)?;
    index_ok(
        &fixture,
        "slot_repair_full",
        &["index", "--full", "--json", "--no-progress-events"],
    )?;
    expect_repaired(&fixture, "slot_after_full")
}

#[test]
fn corrupt_current_manifest_is_reported_and_rebuilt_by_cass_index() -> Result<(), String> {
    check_corrupt_current_manifest()
}
