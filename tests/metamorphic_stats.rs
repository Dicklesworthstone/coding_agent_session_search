//! Metamorphic regression tests for `cass stats`.
//!
//! `coding_agent_session_search-5v5b4`: the by_source aggregator and
//! the top-level total counter in src/lib.rs:10155+ are computed via
//! SEPARATE SQL paths through src/storage/sqlite.rs. Existing
//! `tests/e2e_filters.rs::stats_by_source_grouping` only verifies
//! that `cass stats --by-source --json` emits a non-empty `by_source`
//! array, NOT that `total == sum(by_source[*])`. A regression that
//! double-counts in one path or drops a source would leave the
//! existing test green while violating the invariant operators rely
//! on.
//!
//! The MR archetype is **Inclusive (Pattern 5)**: T(stats_query) =
//! stats_query + `--by-source`. Relation: `total == sum(by_source[*])`
//! per metric. Even with a single source_id (the only kind reachable
//! without a remote sync setup), this catches divergence between the
//! two SQL paths — they MUST emit consistent counts for the same
//! underlying canonical DB.

use assert_cmd::cargo::cargo_bin_cmd;
use std::fs;
use std::path::Path;

mod util;
use util::EnvGuard;

/// Creates a Codex session JSONL file mirroring the helper used by
/// tests/e2e_filters.rs::make_codex_session_at. Inlined here so this
/// test crate is self-contained and not coupled to fixture changes
/// elsewhere.
fn write_codex_session(
    codex_home: &Path,
    date_path: &str,
    filename: &str,
    content: &str,
    ts_millis: u64,
) {
    let sessions = codex_home.join(format!("sessions/{date_path}"));
    fs::create_dir_all(&sessions).unwrap();
    let file = sessions.join(filename);
    let sample = format!(
        r#"{{"type": "event_msg", "timestamp": {ts_millis}, "payload": {{"type": "user_message", "message": "{content}"}}}}
{{"type": "response_item", "timestamp": {}, "payload": {{"role": "assistant", "content": "{content}_response"}}}}"#,
        ts_millis + 1000
    );
    fs::write(file, sample).unwrap();
}

fn capture_stats_json(home: &Path, codex_home: &Path, data_dir: &Path, by_source: bool) -> serde_json::Value {
    let mut args: Vec<&str> = vec!["stats", "--json"];
    if by_source {
        args.push("--by-source");
    }
    args.push("--data-dir");
    let data_dir_str = data_dir.to_str().expect("utf8 data dir");
    args.push(data_dir_str);

    let output = cargo_bin_cmd!("cass")
        .args(&args)
        .env("HOME", home)
        .env("CODEX_HOME", codex_home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass stats");
    assert!(
        output.status.success(),
        "cass stats {args:?} exited non-zero: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout)
        .expect("cass stats --json output is valid JSON")
}

/// `coding_agent_session_search-5v5b4`: pin the metamorphic relation
/// `total_messages == sum(by_source[*].messages)` AND
/// `total_conversations == sum(by_source[*].conversations)`. Even
/// with a single source_id (the only kind reachable without remote
/// sync), the two SQL paths MUST agree — divergence indicates a
/// regression in the by_source aggregator or the total counter.
#[test]
fn mr_stats_total_equals_sum_of_by_source() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path();
    let codex_home = home.join(".codex");
    let data_dir = home.join("cass_data");
    fs::create_dir_all(&data_dir).unwrap();

    let _guard_home = EnvGuard::set("HOME", home.to_string_lossy());
    let _guard_codex = EnvGuard::set("CODEX_HOME", codex_home.to_string_lossy());

    // Seed three distinct Codex sessions across different dates so
    // the row count is unambiguously > 1. Each session contributes 2
    // messages (one user + one assistant) and 1 conversation, so the
    // expected aggregate is 3 conversations / 6 messages.
    write_codex_session(
        &codex_home,
        "2024/11/20",
        "rollout-1.jsonl",
        "first session content",
        1_732_118_400_000,
    );
    write_codex_session(
        &codex_home,
        "2024/11/21",
        "rollout-2.jsonl",
        "second session content",
        1_732_204_800_000,
    );
    write_codex_session(
        &codex_home,
        "2024/11/22",
        "rollout-3.jsonl",
        "third session content",
        1_732_291_200_000,
    );

    cargo_bin_cmd!("cass")
        .args(["index", "--full", "--data-dir"])
        .arg(&data_dir)
        .env("CODEX_HOME", &codex_home)
        .env("HOME", home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .assert()
        .success();

    // Capture both stats variants.
    let total = capture_stats_json(home, &codex_home, &data_dir, false);
    let breakdown = capture_stats_json(home, &codex_home, &data_dir, true);

    let total_conversations = total["conversations"]
        .as_i64()
        .expect("total.conversations is integer");
    let total_messages = total["messages"]
        .as_i64()
        .expect("total.messages is integer");

    // Sanity: the seed produced > 0 rows so the invariant is non-vacuous.
    assert!(
        total_conversations >= 3,
        "expected at least 3 conversations from 3 seeded sessions; got {total_conversations}; \
         total payload: {total:#}"
    );
    assert!(
        total_messages >= 6,
        "expected at least 6 messages (3 sessions × 2 messages); got {total_messages}; \
         total payload: {total:#}"
    );

    let by_source = breakdown["by_source"]
        .as_array()
        .expect("--by-source emits by_source array");
    assert!(
        !by_source.is_empty(),
        "by_source must be non-empty when sessions are indexed; payload: {breakdown:#}"
    );

    let summed_conversations: i64 = by_source
        .iter()
        .map(|entry| {
            entry["conversations"]
                .as_i64()
                .expect("by_source[i].conversations is integer")
        })
        .sum();
    let summed_messages: i64 = by_source
        .iter()
        .map(|entry| {
            entry["messages"]
                .as_i64()
                .expect("by_source[i].messages is integer")
        })
        .sum();

    assert_eq!(
        total_conversations, summed_conversations,
        "metamorphic invariant violated: total.conversations ({total_conversations}) != \
         sum(by_source[*].conversations) ({summed_conversations}). \
         total: {total:#}\nby_source: {by_source:#?}"
    );
    assert_eq!(
        total_messages, summed_messages,
        "metamorphic invariant violated: total.messages ({total_messages}) != \
         sum(by_source[*].messages) ({summed_messages}). \
         total: {total:#}\nby_source: {by_source:#?}"
    );
}
