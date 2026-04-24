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

    // Bead 2hh1s: the `fallback_reason` field is the agent-diagnostic
    // string populated by `SearchModeMeta::fall_back_to_lexical` in
    // src/lib.rs. It must be present (not null) and non-empty on every
    // fail-open path, otherwise agents consuming --robot-meta cannot tell
    // WHY the planner demoted. The exact prefix depends on which branch
    // fired (rejected, unavailable, hybrid execution unavailable, or
    // semantic assets unavailable) — all of those are acceptable.
    let fallback_reason = meta
        .get("fallback_reason")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!(
                "--robot-meta must populate `_meta.fallback_reason` on fail-open; meta: {meta:?}"
            )
        });
    assert!(
        !fallback_reason.is_empty(),
        "fallback_reason must be a non-empty diagnostic string; got: {fallback_reason:?}"
    );
    assert!(
        fallback_reason.contains("semantic") || fallback_reason.contains("hybrid"),
        "fallback_reason should describe why the planner demoted (expected 'semantic'/'hybrid' \
         in the reason string); got: {fallback_reason:?}"
    );
}

// Bead coding_agent_session_search-jogco (child of ibuuh.10, scenario C:
// default-hybrid result quality in lexical-only state).
//
// The sibling test above pins the `_meta` truthfulness on the fail-open
// path but never looks at the actual result set. ibuuh.10's AC calls
// for "default-hybrid result quality across lexical-only, fast-tier,
// and full-hybrid states" — this test covers the LEXICAL-ONLY slice
// (no semantic model installed, which is the default cass install).
//
// Claim pinned: when semantic assets are absent, the default-hybrid
// planner is expected to fail open to lexical AND produce exactly the
// same hit list — same source_path+line_number keys in the same order
// — as an explicit `--mode lexical` search. If a future refactor made
// the default path silently rank differently, drop hits, or run a
// reranker that lexical-mode doesn't, users see a quality regression
// that pure _meta tests don't catch.
fn hit_keys(hits: &[Value]) -> Vec<(String, i64)> {
    // Fail loud on null/missing source_path or line_number instead of
    // defaulting to "" / -1. A silently-defaulted hit would make two
    // modes look equivalent even when both are emitting malformed
    // hits — hollowing out the equivalence guarantee this helper
    // exists to enforce.
    hits.iter()
        .map(|h| {
            let path = h
                .get("source_path")
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    panic!(
                        "hit must have a non-null source_path string; \
                         got hit: {h}"
                    )
                })
                .to_string();
            let line = h
                .get("line_number")
                .and_then(Value::as_i64)
                .unwrap_or_else(|| {
                    panic!(
                        "hit must have a non-null integer line_number; \
                         got hit: {h}"
                    )
                });
            (path, line)
        })
        .collect()
}

#[test]
fn default_hybrid_hit_list_equals_explicit_lexical_when_semantic_absent() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    // Seed three rollouts so the corpus is large enough to give the
    // planner real ranking work. Filenames start with `rollout-` per
    // franken_agent_detection::CodexConnector::is_rollout_file (line
    // ~77). Multiple conversations also sidesteps the single-conv
    // shard-plan bug tracked in bead rx1ex.
    for idx in 1..=3 {
        let name = format!("rollout-equiv-{idx:02}.jsonl");
        seed_codex_session(&codex_home, &name, "equivprobe");
    }

    let mut index = cass_cmd(home);
    index
        .args(["index", "--full", "--json", "--data-dir"])
        .arg(&data_dir);
    let index_output = index.output().expect("run cass index --full");
    assert!(
        index_output.status.success(),
        "cass index --full must succeed on the seeded corpus. stdout: {} stderr: {}",
        String::from_utf8_lossy(&index_output.stdout),
        String::from_utf8_lossy(&index_output.stderr)
    );

    // Search in DEFAULT mode (hybrid-preferred per AGENTS.md but
    // failing open to lexical since no semantic model is installed).
    let mut default_search = cass_cmd(home);
    default_search
        .args([
            "search",
            "equivprobe",
            "--json",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir);
    let default_out = default_search.output().expect("run default search");
    assert!(
        default_out.status.success(),
        "default-mode search must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&default_out.stdout),
        String::from_utf8_lossy(&default_out.stderr)
    );
    let default_json: Value = serde_json::from_slice(&default_out.stdout)
        .unwrap_or_else(|err| panic!("default search JSON parse failed: {err}"));
    let default_hits = default_json
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Search with EXPLICIT --mode lexical on the same corpus.
    let mut lexical_search = cass_cmd(home);
    lexical_search
        .args([
            "search",
            "equivprobe",
            "--json",
            "--mode",
            "lexical",
            "--limit",
            "10",
            "--data-dir",
        ])
        .arg(&data_dir);
    let lexical_out = lexical_search.output().expect("run lexical search");
    assert!(
        lexical_out.status.success(),
        "--mode lexical search must succeed. stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&lexical_out.stdout),
        String::from_utf8_lossy(&lexical_out.stderr)
    );
    let lexical_json: Value = serde_json::from_slice(&lexical_out.stdout)
        .unwrap_or_else(|err| panic!("lexical search JSON parse failed: {err}"));
    let lexical_hits = lexical_json
        .get("hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Guard: there really should be hits for the seeded keyword. A
    // zero-hit corpus would make the equivalence trivially true and
    // hide real regressions.
    assert!(
        !default_hits.is_empty(),
        "default search must return >=1 hit for the seeded keyword; \
         payload: {default_json}"
    );

    // The actual contract pin: same hits in the same order.
    let default_keys = hit_keys(&default_hits);
    let lexical_keys = hit_keys(&lexical_hits);
    assert_eq!(
        default_keys, lexical_keys,
        "default-mode hit list must equal --mode lexical hit list when \
         semantic assets are absent.\ndefault: {default_keys:?}\nlexical: {lexical_keys:?}"
    );

    // Hit counts must also match — guards against a regression where
    // the planner silently truncates or expands one of the paths.
    assert_eq!(
        default_json.get("count").and_then(Value::as_u64),
        lexical_json.get("count").and_then(Value::as_u64),
        "default and lexical `count` must match in lexical-only state. \
         default: {default_json}\nlexical: {lexical_json}"
    );
    assert_eq!(
        default_json.get("total_matches").and_then(Value::as_u64),
        lexical_json.get("total_matches").and_then(Value::as_u64),
        "default and lexical `total_matches` must match in lexical-only state. \
         default: {default_json}\nlexical: {lexical_json}"
    );
}
