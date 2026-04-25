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
use std::path::{Path, PathBuf};

mod util;
use util::EnvGuard;

struct StatsFixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    codex_home: PathBuf,
    data_dir: PathBuf,
}

impl StatsFixture {
    fn new() -> Self {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        let codex_home = home.join(".codex");
        let data_dir = home.join("cass_data");
        fs::create_dir_all(&data_dir).unwrap();

        Self {
            _tmp: tmp,
            home,
            codex_home,
            data_dir,
        }
    }

    fn env_guards(&self) -> (EnvGuard, EnvGuard) {
        (
            EnvGuard::set("HOME", self.home.to_string_lossy().as_ref()),
            EnvGuard::set("CODEX_HOME", self.codex_home.to_string_lossy().as_ref()),
        )
    }

    fn index_full(&self) {
        cargo_bin_cmd!("cass")
            .args(["index", "--full", "--data-dir"])
            .arg(&self.data_dir)
            .env("CODEX_HOME", &self.codex_home)
            .env("HOME", &self.home)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .assert()
            .success();
    }

    fn stats_json(&self, by_source: bool) -> serde_json::Value {
        let mut args: Vec<&str> = vec!["stats", "--json"];
        if by_source {
            args.push("--by-source");
        }
        args.push("--data-dir");
        let data_dir_str = self.data_dir.to_str().expect("utf8 data dir");
        args.push(data_dir_str);

        let output = cargo_bin_cmd!("cass")
            .args(&args)
            .env("HOME", &self.home)
            .env("CODEX_HOME", &self.codex_home)
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
        serde_json::from_slice(&output.stdout).expect("cass stats --json output is valid JSON")
    }

    fn seed(&self, day: &str, rollout: &str, content: &str, ts_millis: u64) {
        let date_path = format!("2024/11/{day}");
        let filename = format!("rollout-{rollout}.jsonl");
        write_codex_session(&self.codex_home, &date_path, &filename, content, ts_millis);
    }
}

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

/// `coding_agent_session_search-5v5b4`: pin the metamorphic relation
/// `total_messages == sum(by_source[*].messages)` AND
/// `total_conversations == sum(by_source[*].conversations)`. Even
/// with a single source_id (the only kind reachable without remote
/// sync), the two SQL paths MUST agree — divergence indicates a
/// regression in the by_source aggregator or the total counter.
#[test]
fn mr_stats_total_equals_sum_of_by_source() {
    let fixture = StatsFixture::new();
    let _guards = fixture.env_guards();

    // Seed three distinct Codex sessions across different dates so
    // the row count is unambiguously > 1. Each session contributes 2
    // messages (one user + one assistant) and 1 conversation, so the
    // expected aggregate is 3 conversations / 6 messages.
    fixture.seed("20", "1", "first session content", 1_732_118_400_000);
    fixture.seed("21", "2", "second session content", 1_732_204_800_000);
    fixture.seed("22", "3", "third session content", 1_732_291_200_000);

    fixture.index_full();

    // Capture both stats variants.
    let total = fixture.stats_json(false);
    let breakdown = fixture.stats_json(true);

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

/// `coding_agent_session_search-pdg22` item (3): empty data-dir
/// produces zero counters across every aggregate field. Without
/// this pin, a regression that emits null/missing/sentinel values
/// for the empty case would silently break agent harnesses
/// expecting the documented contract.
#[test]
fn mr_stats_empty_data_dir_produces_zero_counters_or_structured_error() {
    let fixture = StatsFixture::new();
    fs::create_dir_all(&fixture.codex_home).unwrap();

    let _guards = fixture.env_guards();

    let output = cargo_bin_cmd!("cass")
        .args(["stats", "--json", "--data-dir"])
        .arg(&fixture.data_dir)
        .env("HOME", &fixture.home)
        .env("CODEX_HOME", &fixture.codex_home)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .output()
        .expect("run cass stats on empty dir");

    if !output.status.success() {
        // Acceptable: empty data dir errors out with the missing-db
        // envelope (the q931h-pinned shape). Verify the envelope is
        // structured JSON on STDOUT (per hd89i contract).
        let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
            .expect("error envelope MUST still be valid JSON on stdout (per hd89i)");
        assert!(
            parsed.get("error").is_some(),
            "non-success stats output MUST emit a structured error envelope; got: {parsed:#}"
        );
        return;
    }

    // Success path on empty: every counter MUST be zero.
    let total: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stats --json on empty dir emits valid JSON");
    assert_eq!(
        total["conversations"].as_i64(),
        Some(0),
        "empty data dir MUST report 0 conversations; got: {total:#}"
    );
    assert_eq!(
        total["messages"].as_i64(),
        Some(0),
        "empty data dir MUST report 0 messages; got: {total:#}"
    );
    if let Some(by_agent) = total["by_agent"].as_array() {
        assert!(
            by_agent.is_empty(),
            "empty data dir MUST emit empty by_agent array; got: {by_agent:#?}"
        );
    }
}

/// `coding_agent_session_search-pdg22` item (2): date_range invariant.
/// When stats reports a date_range with both oldest + newest present,
/// oldest MUST be ≤ newest. A regression that swaps the two (or uses
/// MIN where MAX was intended) would produce a nonsensical "future
/// is older than past" envelope. ISO-8601 strings sort
/// lexicographically by chronology, so string comparison is valid.
#[test]
fn mr_stats_date_range_oldest_lte_newest() {
    let fixture = StatsFixture::new();
    let _guards = fixture.env_guards();

    fixture.seed("20", "1", "first", 1_732_118_400_000);
    fixture.seed("22", "2", "second", 1_732_291_200_000);

    fixture.index_full();

    let total = fixture.stats_json(false);
    let date_range = &total["date_range"];

    let oldest_str = date_range["oldest"].as_str();
    let newest_str = date_range["newest"].as_str();
    match (oldest_str, newest_str) {
        (Some(oldest), Some(newest)) => {
            assert!(
                oldest <= newest,
                "metamorphic invariant violated: date_range.oldest ({oldest}) > newest ({newest}). \
                 payload: {date_range:#}"
            );
        }
        (None, None) => {
            // Empty corpus — degenerate case, allowed.
        }
        (oldest, newest) => panic!(
            "date_range MUST have both oldest + newest present OR both absent; \
             got oldest={oldest:?}, newest={newest:?}; payload: {date_range:#}"
        ),
    }
}
