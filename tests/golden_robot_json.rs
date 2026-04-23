//! Golden-file regression tests for cass robot-mode JSON outputs.
//!
//! Bead `u9osp`: cass ships a robot/LLM discovery surface via
//! `cass capabilities --json`, `cass robot-docs --json`, `cass health --json`,
//! and `cass models status --json`. These payloads are the contract every
//! downstream agent consumes — a single renamed field or moved key silently
//! breaks every consumer without failing any existing test.
//!
//! This file freezes the **shape** of those payloads against scrubbed golden
//! files under `tests/golden/robot/`. Scrubbing rules live in
//! [`scrub_robot_json`] below; see `tests/golden/robot/PROVENANCE.md` for
//! regeneration procedure.
//!
//! ## Regenerating a golden
//!
//! ```bash
//! UPDATE_GOLDENS=1 cargo test --test golden_robot_json
//! git diff tests/golden/        # review EVERY change
//! git add tests/golden/
//! git commit -m "Update robot-mode goldens: <reason>"
//! ```
//!
//! Any diff between `actual` and golden is either a bug or an intentional
//! schema change that requires human review before it ships.

use assert_cmd::Command;
use std::path::PathBuf;

/// Build a `cass` binary invocation with the env knobs required for
/// deterministic test output (no update check, no ambient data-dir surprise).
fn cass_cmd(test_home: &std::path::Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        // Pin data dir so the test never touches the user's real cache.
        .env("XDG_DATA_HOME", test_home)
        .env("HOME", test_home)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd
}

/// Strip non-deterministic values from a robot-mode JSON payload so the
/// golden captures *shape* rather than ephemeral facts.
///
/// - `crate_version` → `"[VERSION]"` so the test survives cargo version bumps
/// - ISO timestamps → `"[TIMESTAMP]"`
/// - Absolute paths under the test `HOME` → `"[PATH]"`
/// - UUID-ish tokens → `"[UUID]"`
fn scrub_robot_json(input: &str, test_home: &std::path::Path) -> String {
    let mut out = input.to_string();

    // 1. `crate_version` field in capabilities output. Match the exact JSON
    //    key so we don't inadvertently touch version strings inside features.
    let crate_version_re = regex::Regex::new(r#""crate_version"\s*:\s*"[^"]*""#).unwrap();
    out = crate_version_re
        .replace_all(&out, r#""crate_version": "[VERSION]""#)
        .to_string();

    // 2. ISO-8601 timestamps (match with optional fractional seconds / tz).
    let ts_re =
        regex::Regex::new(r#"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})?"#)
            .unwrap();
    out = ts_re.replace_all(&out, "[TIMESTAMP]").to_string();

    // 3. Absolute paths rooted at the isolated test HOME. Anything else is
    //    either a constant relative path or a configured mount — both are
    //    shape-relevant and stay in the golden.
    let home_str = test_home.display().to_string();
    if !home_str.is_empty() {
        out = out.replace(&home_str, "[TEST_HOME]");
    }

    // 4. UUIDs.
    let uuid_re =
        regex::Regex::new(r#"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"#)
            .unwrap();
    out = uuid_re.replace_all(&out, "[UUID]").to_string();

    // 5. latency_ms (health --json) — wall-clock duration that varies run to
    // run and by host. Keep the field in the golden to prove the shape but
    // scrub the value so drift on it doesn't fail the contract test.
    let latency_re = regex::Regex::new(r#""latency_ms"\s*:\s*\d+"#).unwrap();
    out = latency_re
        .replace_all(&out, r#""latency_ms": "[LATENCY_MS]""#)
        .to_string();

    // 6. Live-sampled kernel metrics in health --json (load average per
    // core and PSI CPU pressure). These float values change between runs
    // based on whatever else is happening on the box. Scrub to placeholders
    // so the golden locks the shape without chasing host noise.
    for key in ["load_per_core", "psi_cpu_some_avg10"] {
        let re = regex::Regex::new(&format!(
            r#""{key}"\s*:\s*(-?\d+(\.\d+)?([eE][+-]?\d+)?|null)"#
        ))
        .unwrap();
        out = re
            .replace_all(&out, format!(r#""{key}": "[LIVE_METRIC]""#).as_str())
            .to_string();
    }

    out
}

/// Compare `actual` against the golden at `tests/golden/<name>`. Writes /
/// overwrites the golden when `UPDATE_GOLDENS=1` is set in the env.
fn assert_golden(name: &str, actual: &str) {
    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name);

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        std::fs::create_dir_all(golden_path.parent().unwrap()).expect("create golden parent dir");
        std::fs::write(&golden_path, actual).expect("write golden file");
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|err| {
        panic!(
            "Golden file missing or unreadable: {}\n{err}\n\n\
             Run with UPDATE_GOLDENS=1 to create it, then review and commit:\n\
             \tUPDATE_GOLDENS=1 cargo test --test golden_robot_json\n\
             \tgit diff tests/golden/\n\
             \tgit add tests/golden/",
            golden_path.display(),
        )
    });

    if actual != expected {
        // Dump actual next to golden for easy diffing.
        let actual_path = golden_path.with_extension("actual");
        std::fs::write(&actual_path, actual).expect("write .actual file");
        panic!(
            "GOLDEN MISMATCH: {name}\n\n\
             Expected: {}\n\
             Actual:   {}\n\n\
             diff the two files to see the drift, then either:\n\
             \t- fix the code if this was unintentional, or\n\
             \t- regenerate: UPDATE_GOLDENS=1 cargo test --test golden_robot_json \\\n\
             \t              && git diff tests/golden/ && git add tests/golden/",
            golden_path.display(),
            actual_path.display(),
        );
    }
}

/// Capture stdout of `cass <args>` in the isolated test home and return
/// the scrubbed canonical-JSON form (keys-sorted by serde_json's default
/// `BTreeMap` insertion preservation, pretty-printed, dynamic values
/// scrubbed). Returns the parsed-then-reserialized string so the golden
/// survives whitespace drift.
///
/// `expect_status` selects the exit-code contract: `ExitOk` for commands
/// that must succeed (capabilities, models status), `ExitAny` for
/// commands that legitimately exit non-zero when reporting a problem
/// (health, which exits 1 when the DB / index is not initialised — that
/// non-zero status *is* part of the contract and we freeze its JSON).
fn capture_robot_json(
    test_home: &std::path::Path,
    args: &[&str],
    expect_status: ExpectStatus,
) -> String {
    let output = cass_cmd(test_home)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("run cass {args:?}: {err}"));
    if matches!(expect_status, ExpectStatus::ExitOk) {
        assert!(
            output.status.success(),
            "cass {args:?} exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("cass {args:?} stdout is not JSON: {err}\nstdout:\n{stdout}"));
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty-print JSON");
    scrub_robot_json(&canonical, test_home)
}

#[derive(Clone, Copy)]
enum ExpectStatus {
    ExitOk,
    ExitAny,
}

#[test]
fn capabilities_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let scrubbed = capture_robot_json(
        test_home.path(),
        &["capabilities", "--json"],
        ExpectStatus::ExitOk,
    );
    assert_golden("robot/capabilities.json.golden", &scrubbed);
}

#[test]
fn models_status_json_matches_golden() {
    // `cass models status --json` reads XDG_DATA_HOME for the model cache
    // directory. In our isolated test home the cache is always empty, so
    // the output is deterministic: state=not_installed across every field.
    // Absolute paths inside the payload (`model_dir`, `files[].actual_path`)
    // get scrubbed by `scrub_robot_json` → `[TEST_HOME]` prefix.
    let test_home = tempfile::tempdir().expect("create temp home");
    let scrubbed = capture_robot_json(
        test_home.path(),
        &["models", "status", "--json"],
        ExpectStatus::ExitOk,
    );
    assert_golden("robot/models_status.json.golden", &scrubbed);
}

#[test]
fn health_json_matches_golden() {
    // `cass health --json` reports readiness for an isolated empty HOME:
    // status=not_initialized, healthy=false, db.exists=false,
    // state.index.status=missing, state.semantic.availability=...
    // All paths scrub to [TEST_HOME], latency_ms scrubs to [LATENCY_MS].
    // The golden freezes the full readiness contract (ibuuh.9 scope):
    // top-level status/healthy/initialized/errors/recommended_action
    // plus the per-subsystem state.* nested blocks.
    let test_home = tempfile::tempdir().expect("create temp home");
    // `cass health` exits 1 when reporting an unhealthy / uninitialised
    // state — that non-zero exit is part of the contract and the golden
    // below freezes the JSON body that accompanies it. ExitAny lets the
    // capture proceed regardless of status.
    let scrubbed = capture_robot_json(
        test_home.path(),
        &["health", "--json"],
        ExpectStatus::ExitAny,
    );
    assert_golden("robot/health.json.golden", &scrubbed);
}

#[test]
fn diag_json_matches_golden() {
    // `cass diag --json` is the artifact-inventory surface that
    // ibuuh.36's verification matrix wants frozen alongside manifest
    // snapshots and golden-query digests: version, platform, paths,
    // database counts, index presence, and per-connector detection. Under
    // an isolated empty HOME every field is deterministic (no connectors
    // detected, database/index absent, paths scrub to [TEST_HOME]).
    // Freezing this makes drift on any connector-detection or path-layout
    // field fail in CI instead of silently misreporting to operators.
    let test_home = tempfile::tempdir().expect("create temp home");
    let scrubbed = capture_robot_json(test_home.path(), &["diag", "--json"], ExpectStatus::ExitOk);
    assert_golden("robot/diag.json.golden", &scrubbed);
}

#[test]
fn api_version_json_matches_golden() {
    // `cass api-version --json` is the smallest LLM contract surface —
    // three fields (crate_version, api_version, contract_version) that
    // together tell an agent "am I talking to a compatible cass build".
    // A silent bump of api_version or contract_version without a
    // coordinated client update breaks every downstream agent. Freezing
    // here catches the drift at commit time.
    let test_home = tempfile::tempdir().expect("create temp home");
    let scrubbed = capture_robot_json(
        test_home.path(),
        &["api-version", "--json"],
        ExpectStatus::ExitOk,
    );
    assert_golden("robot/api_version.json.golden", &scrubbed);
}

#[test]
fn introspect_json_matches_golden() {
    // `cass introspect --json` is the full API schema surface — every
    // subcommand, its flags, positional args, and response-schema
    // references. Agents that bind to cass programmatically use this
    // to generate typed clients; silent drift breaks every downstream
    // client.
    //
    // Was #[ignore]'d when first captured (HashMap-based schema registry
    // emitted non-deterministic key order — filed as bead
    // coding_agent_session_search-8sl73). The underlying HashMap was
    // swapped for BTreeMap in the same commit that re-enabled this test;
    // byte-identical output is now verified across independent runs.
    let test_home = tempfile::tempdir().expect("create temp home");
    let scrubbed = capture_robot_json(
        test_home.path(),
        &["introspect", "--json"],
        ExpectStatus::ExitOk,
    );
    assert_golden("robot/introspect.json.golden", &scrubbed);
}
