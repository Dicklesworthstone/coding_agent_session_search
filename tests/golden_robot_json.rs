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

#[test]
fn capabilities_json_matches_golden() {
    let test_home = tempfile::tempdir().expect("create temp home");
    let output = cass_cmd(test_home.path())
        .args(["capabilities", "--json"])
        .output()
        .expect("run cass capabilities --json");

    assert!(
        output.status.success(),
        "cass capabilities --json exited non-zero: status={:?}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    // Fail fast if the output is not valid JSON — a golden on invalid JSON
    // would lock in the failure.
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("capabilities --json emits valid JSON");
    // Re-serialize with stable ordering and 2-space indent so drift on
    // whitespace / key order is not shape drift.
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty-print JSON");
    let scrubbed = scrub_robot_json(&canonical, test_home.path());

    assert_golden("robot/capabilities.json.golden", &scrubbed);
}
