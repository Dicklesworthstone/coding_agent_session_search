//! Golden-file tests for `cass robot-docs <topic>` plain-text output.
//!
//! Bead `3pjoy` (u9osp follow-up): the LLM-facing `robot-docs` surface
//! emits bounded plain text per topic. Some topics (`exit-codes`, `env`,
//! `schemas`) are host-independent. Others (`paths`) embed the resolved
//! data-dir, so we pin `XDG_DATA_HOME` / `HOME` and then scrub the test
//! home prefix to `[TEST_HOME]` before comparison.
//!
//! ## Regenerate
//!
//! ```bash
//! UPDATE_GOLDENS=1 cargo test --test golden_robot_docs
//! git diff tests/golden/robot_docs/
//! ```

use assert_cmd::Command;
use std::path::{Path, PathBuf};

/// Build a `cass` invocation with knobs pinned for deterministic text.
fn cass_cmd(test_home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("XDG_DATA_HOME", test_home)
        .env("HOME", test_home)
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("NO_COLOR", "1");
    cmd
}

/// Scrub host-specific bits. Today that's only the test-home path — the
/// remaining fields (`exit-codes`, `env`, `schemas`) are host-independent
/// constants emitted by the topic generator.
fn scrub_robot_docs(input: &str, test_home: &Path) -> String {
    let home_str = test_home.display().to_string();
    if home_str.is_empty() {
        input.to_string()
    } else {
        input.replace(&home_str, "[TEST_HOME]")
    }
}

/// `assert_golden` mirrors the helper in `tests/golden_robot_json.rs`:
/// `UPDATE_GOLDENS=1` regenerates the file; otherwise diff against it.
fn assert_golden(name: &str, actual: &str) {
    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name);

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        std::fs::create_dir_all(golden_path.parent().unwrap()).expect("mkdir goldens");
        std::fs::write(&golden_path, actual).expect("write golden");
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|err| {
        panic!(
            "Golden missing: {}\n{err}\n\n\
             UPDATE_GOLDENS=1 cargo test --test golden_robot_docs\n\
             git diff tests/golden/ && git add tests/golden/",
            golden_path.display(),
        )
    });

    if actual != expected {
        let actual_path = golden_path.with_extension("actual");
        std::fs::write(&actual_path, actual).expect("write .actual");
        panic!(
            "GOLDEN MISMATCH: {name}\nExpected: {}\nActual:   {}",
            golden_path.display(),
            actual_path.display(),
        );
    }
}

fn capture_docs(topic: &str) -> String {
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = cass_cmd(test_home.path())
        .args(["robot-docs", topic])
        .output()
        .unwrap_or_else(|err| panic!("run cass robot-docs {topic}: {err}"));
    assert!(
        out.status.success(),
        "cass robot-docs {topic} exited non-zero: {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    scrub_robot_docs(&stdout, test_home.path())
}

#[test]
fn robot_docs_exit_codes_matches_golden() {
    assert_golden(
        "robot_docs/exit-codes.txt.golden",
        &capture_docs("exit-codes"),
    );
}

#[test]
fn robot_docs_env_matches_golden() {
    assert_golden("robot_docs/env.txt.golden", &capture_docs("env"));
}

#[test]
fn robot_docs_paths_matches_golden() {
    assert_golden("robot_docs/paths.txt.golden", &capture_docs("paths"));
}

#[test]
fn robot_docs_schemas_matches_golden() {
    assert_golden("robot_docs/schemas.txt.golden", &capture_docs("schemas"));
}

// `coding_agent_session_search-5fiqq`: pre-fix, only 4 of the 11
// `RobotTopic` enum variants (src/lib.rs:1552) had frozen goldens —
// exit-codes, env, paths, schemas. The other 7 surfaces (commands,
// guide, examples, contracts, wrap, sources, analytics) emit bounded
// plain text via `print_robot_docs` / `render_*_docs` helpers in
// src/lib.rs but were unfrozen, so silent reword/reorder/drop
// regressions on any of those surfaces would slip through CI.
//
// These 7 tests close that gap by freezing every remaining topic.
// Together with the original 4 they pin the full LLM-facing
// `robot-docs <topic>` contract (11/11) so any drift on a
// machine-readable agent surface fails loudly.

#[test]
fn robot_docs_commands_matches_golden() {
    assert_golden("robot_docs/commands.txt.golden", &capture_docs("commands"));
}

#[test]
fn robot_docs_guide_matches_golden() {
    assert_golden("robot_docs/guide.txt.golden", &capture_docs("guide"));
}

#[test]
fn robot_docs_examples_matches_golden() {
    assert_golden("robot_docs/examples.txt.golden", &capture_docs("examples"));
}

#[test]
fn robot_docs_contracts_matches_golden() {
    assert_golden(
        "robot_docs/contracts.txt.golden",
        &capture_docs("contracts"),
    );
}

#[test]
fn robot_docs_wrap_matches_golden() {
    assert_golden("robot_docs/wrap.txt.golden", &capture_docs("wrap"));
}

#[test]
fn robot_docs_sources_matches_golden() {
    assert_golden("robot_docs/sources.txt.golden", &capture_docs("sources"));
}

#[test]
fn robot_docs_analytics_matches_golden() {
    assert_golden(
        "robot_docs/analytics.txt.golden",
        &capture_docs("analytics"),
    );
}

/// Capture plain-text `--robot-help` output in an isolated home. The
/// robot-help string is the top-level "start here" contract surface
/// agents read on first contact; keeping it stable is load-bearing.
fn capture_robot_help() -> String {
    let test_home = tempfile::tempdir().expect("tempdir");
    let out = cass_cmd(test_home.path())
        .args(["--robot-help"])
        .output()
        .unwrap_or_else(|err| panic!("run cass --robot-help: {err}"));
    assert!(
        out.status.success(),
        "cass --robot-help exited non-zero: {:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    scrub_robot_docs(&stdout, test_home.path())
}

#[test]
fn robot_help_matches_golden() {
    // ibuuh.36 verification-matrix row: --robot-help is the top-level
    // LLM onboarding surface. Topics, subcommand list, exit codes, and
    // example invocations are all printed here as a bounded static
    // block. Silent removal of any line breaks agent workflows — freeze
    // the contract so drift fails CI loudly.
    assert_golden("robot_docs/robot_help.txt.golden", &capture_robot_help());
}
