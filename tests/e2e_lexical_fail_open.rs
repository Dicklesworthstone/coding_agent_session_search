//! Bead coding_agent_session_search-0a8y3 (child of ibuuh.10):
//! E2E regression that the "explicit `--mode hybrid` fails open to
//! lexical when semantic assets are absent" contract from commit
//! 86c88d0b holds on a freshly-built corpus.
//!
//! The sibling test
//! `tests/cli_robot.rs::search_robot_meta_reports_explicit_hybrid_fail_open`
//! exercises the same contract against the committed
//! `tests/fixtures/search_demo_data` snapshot. This test complements
//! that coverage by:
//!   - Building the canonical DB AND the lexical index fresh from
//!     seeded Codex sessions (so a schema or pipeline regression
//!     that only affects fresh-build corpora is caught here).
//!   - Isolating HOME / XDG_DATA_HOME / XDG_CONFIG_HOME / CODEX_HOME
//!     to a tempdir so the test doesn't pollute or read the user's
//!     real session corpus.
//!   - Setting CASS_IGNORE_SOURCES_CONFIG=1 so the indexer doesn't
//!     pick up the operator's real `~/.config/cass/sources.toml`.

use assert_cmd::Command;
use serde_json::{Value, json};
use std::fs;
use tempfile::TempDir;

fn cass_cmd(temp_home: &std::path::Path) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1");
    cmd.env("CASS_IGNORE_SOURCES_CONFIG", "1");
    cmd.env("HOME", temp_home);
    cmd.env("XDG_DATA_HOME", temp_home.join(".local/share"));
    cmd.env("XDG_CONFIG_HOME", temp_home.join(".config"));
    cmd.env("CODEX_HOME", temp_home.join(".codex"));
    cmd
}

fn iso_ts(ts_ms: u64) -> String {
    let ts_ms_i64 = i64::try_from(ts_ms).unwrap_or(i64::MAX);
    chrono::DateTime::from_timestamp_millis(ts_ms_i64)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339()
}

fn seed_codex_session(codex_home: &std::path::Path, filename: &str, keyword: &str) {
    let sessions = codex_home.join("sessions/2026/04/23");
    fs::create_dir_all(&sessions).unwrap();
    let ts = 1_714_000_000_000_u64;
    let workspace = codex_home.to_string_lossy().into_owned();
    let lines = [
        json!({
            "timestamp": iso_ts(ts),
            "type": "session_meta",
            "payload": { "id": filename, "cwd": workspace, "cli_version": "0.42.0" },
        }),
        json!({
            "timestamp": iso_ts(ts + 1_000),
            "type": "response_item",
            "payload": {
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": keyword }],
            },
        }),
        json!({
            "timestamp": iso_ts(ts + 2_000),
            "type": "response_item",
            "payload": {
                "type": "message", "role": "assistant",
                "content": [{ "type": "text", "text": format!("{keyword} response") }],
            },
        }),
    ];
    let mut body = String::new();
    for line in lines {
        body.push_str(&serde_json::to_string(&line).unwrap());
        body.push('\n');
    }
    fs::write(sessions.join(filename), body).unwrap();
}

#[test]
fn explicit_hybrid_mode_fails_open_to_lexical_when_semantic_assets_missing() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Seed one Codex session with a single-word keyword (no underscores
    // to stay clear of tokenizer split behavior downstream).
    seed_codex_session(&codex_home, "failopen-fixture-01.jsonl", "failopenprobe");

    // Build canonical DB + lexical index from the freshly seeded
    // session. No `--semantic` flag: the semantic tier is deliberately
    // absent so the fail-open path activates below.
    let mut index = cass_cmd(home);
    index
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let index_output = index.output().expect("run cass index --full");
    assert!(
        index_output.status.success(),
        "cass index --full must succeed on a fresh seeded corpus. stdout: {} stderr: {}",
        String::from_utf8_lossy(&index_output.stdout),
        String::from_utf8_lossy(&index_output.stderr)
    );

    // Request hybrid search explicitly. With no semantic assets, the
    // 86c88d0b contract says cass fails open to lexical rather than
    // erroring out, and the robot meta reports every realized-tier
    // field so observability stays truthful.
    let mut search = cass_cmd(home);
    search
        .args([
            "search",
            "failopenprobe",
            "--json",
            "--robot-meta",
            "--mode",
            "hybrid",
            "--limit",
            "5",
            "--data-dir",
        ])
        .arg(&data_dir);
    let search_output = search.output().expect("run cass search --mode hybrid");
    let search_stdout = String::from_utf8_lossy(&search_output.stdout);
    let search_stderr = String::from_utf8_lossy(&search_output.stderr);
    assert!(
        search_output.status.success(),
        "cass search --mode hybrid must fail open, not error, when semantic \
         assets are absent.\nstdout: {search_stdout}\nstderr: {search_stderr}"
    );

    let payload: Value = serde_json::from_str(search_stdout.trim()).unwrap_or_else(|err| {
        panic!("cass search --json output is not valid JSON: {err}\nstdout: {search_stdout}")
    });
    let meta = payload
        .get("_meta")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("--robot-meta must populate `_meta`; payload: {payload}"));

    assert_eq!(
        meta.get("requested_search_mode").and_then(Value::as_str),
        Some("hybrid"),
        "explicit --mode hybrid must be preserved as the requested intent"
    );
    assert_eq!(
        meta.get("search_mode").and_then(Value::as_str),
        Some("lexical"),
        "realized tier must be lexical when semantic assets are missing"
    );
    assert_eq!(
        meta.get("mode_defaulted").and_then(Value::as_bool),
        Some(false),
        "the user explicitly passed --mode hybrid; mode_defaulted must be false"
    );
    assert_eq!(
        meta.get("fallback_tier").and_then(Value::as_str),
        Some("lexical"),
        "robot meta must name the fail-open tier so agents can diagnose degraded results"
    );
    assert_eq!(
        meta.get("semantic_refinement").and_then(Value::as_bool),
        Some(false),
        "no semantic pass happened, so semantic_refinement must be false"
    );
}
