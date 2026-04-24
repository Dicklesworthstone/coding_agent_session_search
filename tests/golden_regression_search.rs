//! Regression goldens for real `cass` binary search flows.
//!
//! These snapshots intentionally build a fresh temp HOME, run `cass index`
//! into a real tempdir database, then snapshot `cass search --json` output.
//! Regenerate with:
//!   UPDATE_GOLDENS=1 cargo test --test golden_regression_search

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn cass_cmd(test_home: &Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", test_home)
        .env("XDG_DATA_HOME", test_home.join(".local/share"))
        .env("XDG_CONFIG_HOME", test_home.join(".config"))
        .current_dir(test_home);
    cmd
}

fn seed_claude_code_fixture(test_home: &Path) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("claude_code_real")
        .join("projects")
        .join("-test-project")
        .join("agent-test123.jsonl");
    let dst = test_home
        .join(".claude")
        .join("projects")
        .join("-test-project")
        .join("agent-test123.jsonl");
    fs::create_dir_all(dst.parent().expect("fixture destination parent"))
        .expect("create fixture destination");
    fs::copy(&src, &dst).expect("copy claude_code fixture");
    dst
}

fn scrub_temp_home(input: &str, test_home: &Path) -> String {
    input.replace(&test_home.display().to_string(), "[TEST_HOME]")
}

fn assert_golden(name: &str, actual: &str) {
    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("regression")
        .join(name);

    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        fs::create_dir_all(golden_path.parent().expect("golden parent"))
            .expect("create golden parent");
        fs::write(&golden_path, actual).expect("write golden");
        eprintln!("[GOLDEN] Updated: {}", golden_path.display());
        return;
    }

    let expected = fs::read_to_string(&golden_path).unwrap_or_else(|err| {
        panic!(
            "Golden file missing: {}\n{err}\n\n\
             Run: UPDATE_GOLDENS=1 cargo test --test golden_regression_search",
            golden_path.display()
        )
    });

    if actual != expected {
        let actual_path = golden_path.with_extension("actual");
        fs::write(&actual_path, actual).expect("write actual golden output");
        panic!(
            "GOLDEN MISMATCH: {name}\nExpected: {}\nActual: {}\n",
            golden_path.display(),
            actual_path.display()
        );
    }
}

#[test]
fn indexed_claude_code_positive_search_matches_golden() {
    let root = tempfile::tempdir().expect("create temp root");
    let test_home = root.path().join("home");
    let data_dir = root.path().join("cass-data");
    fs::create_dir_all(&test_home).expect("create temp home");
    fs::create_dir_all(&data_dir).expect("create temp data dir");
    seed_claude_code_fixture(&test_home);

    let index_output = cass_cmd(&test_home)
        .args([
            "index",
            "--full",
            "--json",
            "--no-progress-events",
            "--data-dir",
            data_dir.to_str().expect("utf8 data dir"),
        ])
        .output()
        .expect("run cass index");
    assert!(
        index_output.status.success(),
        "cass index failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        index_output.status,
        String::from_utf8_lossy(&index_output.stdout),
        String::from_utf8_lossy(&index_output.stderr)
    );
    let index_json: Value = serde_json::from_slice(&index_output.stdout).expect("valid index JSON");
    assert_eq!(index_json["conversations"].as_u64(), Some(1));
    assert_eq!(index_json["messages"].as_u64(), Some(2));

    let search_output = cass_cmd(&test_home)
        .args([
            "search",
            "matrix",
            "--json",
            "--fields",
            "minimal",
            "--limit",
            "3",
            "--data-dir",
            data_dir.to_str().expect("utf8 data dir"),
        ])
        .output()
        .expect("run cass search");
    assert!(
        search_output.status.success(),
        "cass search failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        search_output.status,
        String::from_utf8_lossy(&search_output.stdout),
        String::from_utf8_lossy(&search_output.stderr)
    );

    let parsed: Value = serde_json::from_slice(&search_output.stdout).expect("valid search JSON");
    assert_eq!(parsed["count"].as_u64(), Some(2));
    let canonical = serde_json::to_string_pretty(&parsed).expect("pretty search JSON");
    let scrubbed = scrub_temp_home(&canonical, &test_home);
    assert_golden("claude_indexed_search_matrix.json", &scrubbed);
}
