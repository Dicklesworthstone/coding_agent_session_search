//! Real-binary gate for bead `coding_agent_session_search-uaulb` (gh#408):
//! `cass models build-hnsw` builds / verifies the HNSW accelerator for an
//! already-published semantic artifact without opening the archive or
//! embedding, records it in the semantic manifest, and `--check` never
//! writes.

mod util;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

use util::timeout::spawn_with_timeout_or_diag;

const INDEX_TIMEOUT: Duration = Duration::from_secs(180);
const MODELS_TIMEOUT: Duration = Duration::from_secs(120);
const KEYWORD: &str = "build-hnsw-gate-keyword-qwer";

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
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("RUST_LOG");
    spawn_with_timeout_or_diag(cmd, label, Some(&fixture.data_dir), timeout)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn json_stdout(out: &Output, label: &str) -> Result<Value, String> {
    serde_json::from_str(text(&out.stdout).trim()).map_err(|e| {
        format!(
            "{label}: stdout not JSON: {e}; stdout: {}; stderr: {}",
            text(&out.stdout),
            text(&out.stderr)
        )
    })
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn semantic_fixture() -> Result<Fixture, String> {
    let home = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let home_path = home.path().to_path_buf();
    let data_dir = home_path.join("cass-data");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("data dir: {e}"))?;
    let codex_home = home_path.join(".codex");
    util::seed_codex_session(
        &codex_home,
        "rollout-2026-04-23T10-00-00-hnsw.jsonl",
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
        "build_hnsw_gate_index",
        &[
            "index",
            "--full",
            "--semantic",
            "--embedder",
            "hash",
            "--json",
            "--no-progress-events",
        ],
        INDEX_TIMEOUT,
    );
    let value = json_stdout(&out, "index")?;
    if value
        .get("success")
        .and_then(Value::as_bool)
        .is_none_or(|ok| !ok)
    {
        return Err(format!("semantic index did not succeed: {value}"));
    }
    Ok(fixture)
}

fn manifest_hnsw(data_dir: &Path) -> Result<Option<Value>, String> {
    let path = data_dir.join("vector_index").join("semantic_manifest.json");
    let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let manifest: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse manifest: {e}"))?;
    Ok(manifest.get("hnsw").filter(|v| !v.is_null()).cloned())
}

fn build_hnsw(fixture: &Fixture, label: &str, extra: &[&str]) -> Result<(Output, Value), String> {
    let mut args = vec!["models", "build-hnsw", "--json"];
    args.extend_from_slice(extra);
    let out = run(fixture, label, &args, MODELS_TIMEOUT);
    let value = json_stdout(&out, label)?;
    Ok((out, value))
}

fn check() -> Result<(), String> {
    let fixture = semantic_fixture()?;
    let mut failures = Vec::new();

    // 1. `--check` on a fresh artifact: no accelerator yet, nothing written.
    let (out, before) = build_hnsw(&fixture, "build_hnsw_check_missing", &["--check"])?;
    if !out.status.success() {
        failures.push(format!("--check exited non-zero: {}", text(&out.stderr)));
    }
    if str_at(&before, "state_before").cmp("missing").is_ne()
        || str_at(&before, "action").cmp("check_only").is_ne()
        || before
            .get("current")
            .and_then(Value::as_bool)
            .is_none_or(|c| c)
    {
        failures.push(format!(
            "check on a fresh artifact should report missing: {before}"
        ));
    }
    if str_at(&before, "next_command")
        .cmp("cass models build-hnsw")
        .is_ne()
    {
        failures.push(format!("check should name the build command: {before}"));
    }
    if manifest_hnsw(&fixture.data_dir)?.is_some()
        || before
            .get("manifest_recorded")
            .and_then(Value::as_bool)
            .is_none_or(|r| r)
    {
        failures.push("--check must not record an accelerator in the manifest".to_string());
    }
    let hnsw_path = fixture.data_dir.join(str_at(&before, "hnsw_path"));
    if hnsw_path.exists() {
        failures.push(format!("--check must not write {}", hnsw_path.display()));
    }
    let archive_before = std::fs::read(fixture.data_dir.join("agent_search.db"))
        .map_err(|e| format!("read archive: {e}"))?;

    // 2. Build: the accelerator appears, loads back as native, is recorded.
    let (out, built) = build_hnsw(&fixture, "build_hnsw_build", &[])?;
    if !out.status.success() {
        failures.push(format!("build exited non-zero: {}", text(&out.stderr)));
    }
    if str_at(&built, "action").cmp("rebuilt").is_ne()
        || str_at(&built, "state_after").cmp("native_valid").is_ne()
        || built
            .get("manifest_published")
            .and_then(Value::as_bool)
            .is_none_or(|p| !p)
        || built
            .get("current")
            .and_then(Value::as_bool)
            .is_none_or(|c| !c)
        || built
            .get("manifest_recorded")
            .and_then(Value::as_bool)
            .is_none_or(|r| !r)
    {
        failures.push(format!(
            "build should rebuild to native_valid and publish: {built}"
        ));
    }
    if built
        .get("vector_count")
        .and_then(Value::as_u64)
        .is_none_or(|n| n < 1)
    {
        failures.push(format!("published artifact should carry vectors: {built}"));
    }
    if !hnsw_path.is_file() {
        failures.push(format!(
            "accelerator metadata missing at {}",
            hnsw_path.display()
        ));
    }
    match manifest_hnsw(&fixture.data_dir)? {
        Some(record) => {
            if record
                .get("ready")
                .and_then(Value::as_bool)
                .is_none_or(|r| !r)
                || str_at(&record, "embedder_id")
                    .cmp(str_at(&built, "embedder_id"))
                    .is_ne()
                || str_at(&record, "base_tier")
                    .cmp(str_at(&built, "tier"))
                    .is_ne()
            {
                failures.push(format!(
                    "manifest hnsw record disagrees with the report: {record}"
                ));
            }
        }
        None => failures.push("build must record the accelerator in the manifest".to_string()),
    }
    let archive_after = std::fs::read(fixture.data_dir.join("agent_search.db"))
        .map_err(|e| format!("read archive: {e}"))?;
    if archive_after.cmp(&archive_before).is_ne() {
        failures.push("build-hnsw must never modify the canonical archive".to_string());
    }

    // 3. Re-run without --force: current graph is kept, nothing rebuilt.
    let (_, again) = build_hnsw(&fixture, "build_hnsw_again", &[])?;
    if str_at(&again, "action").cmp("unchanged").is_ne()
        || str_at(&again, "state_before").cmp("native_valid").is_ne()
    {
        failures.push(format!(
            "second build should be a no-op on a current graph: {again}"
        ));
    }

    // 4. `--check` now proves the native graph; `--force` rebuilds anyway.
    let (_, verified) = build_hnsw(&fixture, "build_hnsw_check_valid", &["--check"])?;
    if str_at(&verified, "state_before")
        .cmp("native_valid")
        .is_ne()
        || verified
            .get("current")
            .and_then(Value::as_bool)
            .is_none_or(|c| !c)
    {
        failures.push(format!(
            "check after build should be native_valid: {verified}"
        ));
    }
    let (_, forced) = build_hnsw(&fixture, "build_hnsw_force", &["--force"])?;
    if str_at(&forced, "action").cmp("rebuilt").is_ne()
        || str_at(&forced, "state_after").cmp("native_valid").is_ne()
    {
        failures.push(format!("--force should rebuild to native_valid: {forced}"));
    }

    // 5. An unpublished tier is refused with the index-missing envelope.
    let out = run(
        &fixture,
        "build_hnsw_quality_missing",
        &["models", "build-hnsw", "--json", "--tier", "quality"],
        MODELS_TIMEOUT,
    );
    if out.status.success() {
        failures.push("build-hnsw on an unpublished tier must fail".to_string());
    }
    let combined = format!("{}{}", text(&out.stdout), text(&out.stderr));
    if !combined.contains("no published quality semantic artifact") {
        failures.push(format!(
            "unpublished tier error should name the tier: {combined}"
        ));
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

#[test]
fn models_build_hnsw_builds_verifies_and_records_accelerator() -> Result<(), String> {
    check()
}
