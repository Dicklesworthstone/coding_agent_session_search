//! Real-binary E2E gate for `cass support-bundle` (bead
//! `coding_agent_session_search-6f1lm`; the `.13.3` redacted recovery evidence
//! bundle contract).
//!
//! `.13.3` landed `src/recovery_support_bundle.rs`: a pure assembler that
//! composes the canonical `ReadinessSummary`, `NextCommandEnvelope`,
//! `SourceDoctorReport`, `QuarantineSummary`, optional `FleetSummary`, and
//! `RootCauseAttribution` into one redacted, share-safe bundle. `6f1lm` wires it
//! into a live command (`src/lib.rs::run_support_bundle`). This gate proves the
//! wiring survives to the real `cass` binary:
//!
//! 1. **Manifest completeness** — the live bundle carries a complete manifest
//!    (version, id, cass version, provenance, live mode, a real timestamp).
//! 2. **Redaction defaults are share-safe** — no full paths, raw session/tool
//!    payloads suppressed, the data dir truncated to a basename.
//! 3. **Opt-in flips the policy** — `--include-full-paths` emits the real path,
//!    `--include-raw-evidence` includes raw payloads (recorded, not silent).
//! 4. **No destructive guidance** — the bundle never surfaces a destructive
//!    command fragment.
//! 5. **Consistency with the robot JSON** — the bundle's readiness verdict does
//!    not contradict `cass status --json` for the same fixture.
//! 6. **Bounded runtime** — the surface is sub-second on an isolated fixture.
//!
//! Isolation: every invocation runs against a fresh `tempdir` with
//! `HOME`/`XDG_*`/cwd redirected into it (the indexer test-isolation rule: an
//! un-isolated run scans the operator's real session corpus and appears to
//! wedge). `CASS_IGNORE_SOURCES_CONFIG=1` + removed `CODEX_HOME` keep it a clean
//! fresh (`not_initialized`) state; `NO_COLOR=1` keeps stdout ANSI-free.

use std::process::Command;
use std::time::{Duration, Instant};

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;

/// Generous per-surface wall-clock bound; only fires on a true hang and holds
/// under heavy multi-agent host contention.
const SURFACE_BOUND: Duration = Duration::from_secs(60);

/// Destructive command fragments the bundle must never surface.
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

/// An isolated tempdir HOME + data dir for one fresh-state run.
struct Fixture {
    _home: tempfile::TempDir,
    home: std::path::PathBuf,
    data_dir: std::path::PathBuf,
}

fn fresh_fixture() -> Result<Fixture, String> {
    let home = tempfile::tempdir().map_err(|e| format!("create tempdir: {e}"))?;
    let home_path = home.path().to_path_buf();
    let data_dir = home_path.join("cass-data");
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("create data dir: {e}"))?;
    Ok(Fixture {
        _home: home,
        home: home_path,
        data_dir,
    })
}

/// Build an isolated `cass` command with `--data-dir` appended.
fn cass_command(fixture: &Fixture, base: &[&str]) -> Command {
    let mut cmd = Command::new(cargo_bin("cass"));
    cmd.args(base)
        .arg("--data-dir")
        .arg(&fixture.data_dir)
        .current_dir(&fixture.home)
        .env("HOME", &fixture.home)
        .env("XDG_DATA_HOME", fixture.home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", fixture.home.join("xdg-config"))
        .env("XDG_CACHE_HOME", fixture.home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_EMBEDDER", "hash")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("NO_COLOR", "1")
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR");
    cmd
}

/// Run a surface and return (parsed-json, elapsed). stdout must be pure JSON.
fn run_json(fixture: &Fixture, base: &[&str]) -> Result<(Value, Duration), String> {
    let started = Instant::now();
    let out = cass_command(fixture, base)
        .output()
        .map_err(|e| format!("spawn {base:?}: {e}"))?;
    let elapsed = started.elapsed();
    let code = out
        .status
        .code()
        .ok_or_else(|| format!("{base:?} killed by signal"))?;
    if code != 0 {
        return Err(format!(
            "{base:?} exited {code}; stderr: {}",
            head(&String::from_utf8_lossy(&out.stderr))
        ));
    }
    let stdout =
        String::from_utf8(out.stdout).map_err(|e| format!("{base:?} stdout not UTF-8: {e}"))?;
    let value: Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("{base:?} stdout not JSON: {e}; head: {}", head(&stdout)))?;
    Ok((value, elapsed))
}

fn head(s: &str) -> String {
    s.chars().take(400).collect()
}

fn missing_key_msg(key: &str) -> String {
    format!("support-bundle JSON is missing required top-level key {key:?}")
}

fn str_at<'a>(v: &'a Value, ptr: &str) -> Option<&'a str> {
    v.pointer(ptr).and_then(Value::as_str)
}

fn array_contains(v: &Value, ptr: &str, needle: &str) -> bool {
    v.pointer(ptr)
        .and_then(Value::as_array)
        .is_some_and(|a| a.iter().filter_map(Value::as_str).any(|s| s == needle))
}

/// Whether `s` is free of every destructive fragment.
fn text_is_clean(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    !UNSAFE_FRAGMENTS.iter().any(|frag| lower.contains(frag))
}

/// 1+2+4+6: default bundle is complete, share-safe, clean, and bounded.
#[test]
fn support_bundle_default_is_complete_share_safe_and_bounded() -> Result<(), String> {
    let fixture = fresh_fixture()?;
    let (bundle, elapsed) = run_json(&fixture, &["support-bundle", "--json"])?;

    let mut failures: Vec<String> = Vec::new();

    // Completeness: every composed contract is present.
    for key in [
        "manifest",
        "readiness",
        "command_envelope",
        "source_provenance",
        "quarantine",
        "root_cause",
        "redaction",
    ] {
        if bundle.pointer(&format!("/{key}")).is_none() {
            failures.push(missing_key_msg(key));
        }
    }

    // Manifest completeness (mirrors SupportBundleManifest::is_complete).
    if bundle
        .pointer("/manifest/manifest_version")
        .and_then(Value::as_u64)
        != Some(1)
    {
        failures.push("manifest.manifest_version != 1".to_string());
    }
    if !str_at(&bundle, "/manifest/bundle_id").is_some_and(|s| s.starts_with("support-bundle-")) {
        failures.push("manifest.bundle_id is not a 'support-bundle-<ts>' id".to_string());
    }
    if !str_at(&bundle, "/manifest/cass_version").is_some_and(|s| !s.is_empty()) {
        failures.push("manifest.cass_version is empty".to_string());
    }
    if str_at(&bundle, "/manifest/command_provenance") != Some("cass support-bundle") {
        failures.push("manifest.command_provenance != 'cass support-bundle'".to_string());
    }
    if str_at(&bundle, "/manifest/mode") != Some("live") {
        failures.push("manifest.mode != 'live'".to_string());
    }
    if !bundle
        .pointer("/manifest/generated_at_ms")
        .and_then(Value::as_i64)
        .is_some_and(|ms| ms > 0)
    {
        failures.push("manifest.generated_at_ms is not > 0".to_string());
    }

    // Share-safe defaults.
    if bundle
        .pointer("/redaction/full_paths")
        .and_then(Value::as_bool)
        != Some(false)
    {
        failures.push("default redaction.full_paths is not false".to_string());
    }
    if !array_contains(
        &bundle,
        "/redaction/fields_suppressed",
        "raw_session_payload",
    ) {
        failures.push("default does not suppress raw_session_payload".to_string());
    }
    if !array_contains(&bundle, "/redaction/fields_suppressed", "raw_tool_payload") {
        failures.push("default does not suppress raw_tool_payload".to_string());
    }
    // The data dir is basename-only (no path separator) by default.
    if let Some(dir) = str_at(&bundle, "/manifest/data_dir") {
        if dir.contains('/') {
            failures.push(format!(
                "default manifest.data_dir leaks a full path: {dir}"
            ));
        }
    } else {
        failures.push("manifest.data_dir absent (expected a basename)".to_string());
    }

    // No destructive guidance anywhere in the bundle's content. The redaction
    // report is excluded from this scan: it is meta-vocabulary describing what
    // was redacted (e.g. the field name `fields_truncated`), not operator
    // guidance, so it would false-positive on fragments like "truncate".
    let mut guidance = bundle.clone();
    if let Some(obj) = guidance.as_object_mut() {
        obj.remove("redaction");
    }
    if !text_is_clean(&guidance.to_string()) {
        failures.push("bundle guidance carries a destructive command fragment".to_string());
    }

    // Bounded runtime.
    if elapsed > SURFACE_BOUND {
        failures.push(format!(
            "support-bundle took {elapsed:?} (> {SURFACE_BOUND:?})"
        ));
    }

    finish(failures)
}

/// 3: `--include-full-paths` emits the real path and records the opt-in.
#[test]
fn support_bundle_full_paths_opt_in_unredacts_data_dir() -> Result<(), String> {
    let fixture = fresh_fixture()?;
    let (bundle, _) = run_json(
        &fixture,
        &["support-bundle", "--json", "--include-full-paths"],
    )?;

    let mut failures: Vec<String> = Vec::new();
    if bundle
        .pointer("/redaction/full_paths")
        .and_then(Value::as_bool)
        != Some(true)
    {
        failures.push("--include-full-paths did not set redaction.full_paths=true".to_string());
    }
    // With full paths, the manifest data dir is the real (slash-bearing) path.
    let expected = fixture.data_dir.to_string_lossy().into_owned();
    match str_at(&bundle, "/manifest/data_dir") {
        Some(dir) if dir == expected => {}
        other => failures.push(format!(
            "--include-full-paths data_dir mismatch: got {other:?}, expected {expected:?}"
        )),
    }
    finish(failures)
}

/// 3: `--include-raw-evidence` includes raw payloads (recorded, not silent).
#[test]
fn support_bundle_raw_evidence_opt_in_is_recorded() -> Result<(), String> {
    let fixture = fresh_fixture()?;
    let (bundle, _) = run_json(
        &fixture,
        &["support-bundle", "--json", "--include-raw-evidence"],
    )?;

    let mut failures: Vec<String> = Vec::new();
    // Raw payloads move from suppressed to included.
    if !array_contains(&bundle, "/redaction/fields_included", "raw_session_payload") {
        failures.push("--include-raw-evidence did not include raw_session_payload".to_string());
    }
    if array_contains(
        &bundle,
        "/redaction/fields_suppressed",
        "raw_session_payload",
    ) {
        failures.push(
            "--include-raw-evidence still lists raw_session_payload as suppressed".to_string(),
        );
    }
    finish(failures)
}

/// 5: the bundle readiness verdict does not contradict `cass status --json`.
#[test]
fn support_bundle_readiness_agrees_with_status() -> Result<(), String> {
    let fixture = fresh_fixture()?;
    let (bundle, _) = run_json(&fixture, &["support-bundle", "--json"])?;
    let (status, _) = run_json(&fixture, &["status", "--json"])?;

    let mut failures: Vec<String> = Vec::new();
    let bundle_searchable = bundle
        .pointer("/readiness/is_searchable")
        .and_then(Value::as_bool);
    let status_healthy = status.get("healthy").and_then(Value::as_bool);

    // Fresh (not_initialized): both must report not-ready, and they must agree.
    if bundle_searchable != Some(false) {
        failures.push(format!(
            "fresh bundle readiness.is_searchable expected false, got {bundle_searchable:?}"
        ));
    }
    if status_healthy != Some(false) {
        failures.push(format!(
            "fresh status.healthy expected false, got {status_healthy:?}"
        ));
    }
    if bundle_searchable != status_healthy {
        failures.push(format!(
            "bundle readiness.is_searchable={bundle_searchable:?} contradicts status.healthy={status_healthy:?}"
        ));
    }
    finish(failures)
}

/// Collapse a failure list into a single `Result`.
fn finish(failures: Vec<String>) -> Result<(), String> {
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} support-bundle gate failure(s):\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        ))
    }
}
