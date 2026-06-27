//! INV-cass-20 — `cass index --json` idempotency contract.
//!
//! Indexing is the data-ingestion engine for cass. A regression where
//! every `cass index --json` invocation does a full rebuild — instead
//! of detecting that the corpus is unchanged and taking the incremental
//! fast path — would silently burn CI budget, multiply local-dev wait
//! time, and (in production) thrash the lexical-publish atomic-swap
//! pipeline on every wake.
//!
//! The planner emits two `indexing_stats.lexical_strategy` values that
//! distinguish the two paths verbatim:
//!
//! - first index against a fresh data-dir:
//!   `"inline_rebuild_from_scan"`
//!   reason: `"lexical_index_needs_rebuild_so_scan_results_repopulate_tantivy_directly"`
//! - second + later index against the same corpus:
//!   `"incremental_inline"`
//!   reason: `"incremental_scan_applies_inline_lexical_updates_only_for_new_messages"`
//!
//! This file pins three invariants:
//!
//! 1. Two consecutive `cass index --json` invocations against the same
//!    stable corpus produce stable `conversations` and `messages`
//!    total counts. No double-counting, no under-counting.
//! 2. The first run's `lexical_strategy` is `"inline_rebuild_from_scan"`
//!    (the rebuild fast-path).
//! 3. The second run's `lexical_strategy` is `"incremental_inline"`
//!    (the no-op fast-path). A regression where every run does a
//!    full rebuild would emit `"inline_rebuild_from_scan"` twice and
//!    fail this assertion immediately.
//!
//! Verified by copying the `tests/fixtures/aider/.aider.chat.history.md`
//! file into an isolated HOME, pointing cwd into that directory so the
//! aider connector discovers the single fixture session, and indexing
//! against a fresh data-dir twice in a row.

use std::cmp::Ordering;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

fn test_error(message: impl Into<String>) -> Box<dyn Error> {
    std::io::Error::other(message.into()).into()
}

fn ensure(condition: bool, message: impl Into<String>) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(test_error(message))
    }
}

/// Install the aider fixture under a fresh project subdirectory so the
/// aider connector running from cwd discovers exactly one session. Returns
/// `(project_dir, data_dir)`. The temp dir's lifetime is tied to the
/// caller, so this returns a `TempDir` guard separately.
fn install_aider_fixture_project() -> Result<(TempDir, PathBuf, PathBuf), Box<dyn Error>> {
    let tmp = TempDir::new()?;
    let project_dir = tmp.path().join("aider_project");
    let data_dir = tmp.path().join("cass_data");
    fs::create_dir_all(&project_dir)?;
    fs::create_dir_all(&data_dir)?;
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("aider")
        .join(".aider.chat.history.md");
    let dst = project_dir.join(".aider.chat.history.md");
    fs::copy(&src, &dst)?;
    Ok((tmp, project_dir, data_dir))
}

/// Run `cass index --json --no-progress-events --data-dir <data_dir>`
/// from `current_dir = <project_dir>` so the aider connector picks up
/// the fixture as the only session source. Returns the parsed JSON
/// envelope and asserts exit 0.
fn run_index_in(project_dir: &Path, data_dir: &Path, home: &Path) -> Result<Value, Box<dyn Error>> {
    let output = Command::cargo_bin("cass")?
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .current_dir(project_dir)
        .args(["--color=never", "index", "--json", "--no-progress-events"])
        .args(["--data-dir", data_dir.to_str().ok_or("non-utf8 path")?])
        .output()?;
    let code = output
        .status
        .code()
        .ok_or_else(|| test_error("cass index killed by signal"))?;
    if !matches!(code.cmp(&0), Ordering::Equal) {
        return Err(test_error(format!(
            "cass index --json exited {code}; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let parsed: Value = serde_json::from_slice(&output.stdout)?;
    Ok(parsed)
}

fn lexical_strategy(envelope: &Value) -> Result<&str, Box<dyn Error>> {
    envelope
        .pointer("/indexing_stats/lexical_strategy")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            test_error(format!(
                "envelope missing indexing_stats.lexical_strategy: {envelope}"
            ))
        })
}

fn conversation_count(envelope: &Value) -> Result<i64, Box<dyn Error>> {
    envelope
        .get("conversations")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            test_error(format!(
                "envelope missing integer `conversations`: {envelope}"
            ))
        })
}

fn message_count(envelope: &Value) -> Result<i64, Box<dyn Error>> {
    envelope
        .get("messages")
        .and_then(Value::as_i64)
        .ok_or_else(|| test_error(format!("envelope missing integer `messages`: {envelope}")))
}

#[test]
fn first_index_uses_inline_rebuild_strategy_against_fresh_data_dir() -> TestResult {
    let (tmp, project, data) = install_aider_fixture_project()?;
    let envelope = run_index_in(&project, &data, tmp.path())?;
    let strategy = lexical_strategy(&envelope)?;
    ensure(
        strategy == "inline_rebuild_from_scan",
        format!(
            "first index against a fresh data-dir should use \
             `inline_rebuild_from_scan`; got {strategy:?}.\n\
             envelope.indexing_stats: {}",
            envelope.pointer("/indexing_stats").unwrap_or(&Value::Null)
        ),
    )?;
    // Also sanity: the aider fixture should produce at least one conversation,
    // otherwise the rest of the test setup is silently broken.
    let n_conv = conversation_count(&envelope)?;
    ensure(
        !matches!(n_conv.cmp(&0), Ordering::Less | Ordering::Equal),
        format!("first index should find >=1 aider conversation; got {n_conv}"),
    )?;
    Ok(())
}

#[test]
fn second_index_uses_incremental_inline_strategy_proving_idempotency() -> TestResult {
    let (tmp, project, data) = install_aider_fixture_project()?;
    // First run primes the corpus.
    let _envelope1 = run_index_in(&project, &data, tmp.path())?;
    // Second run is the assertion target.
    let envelope2 = run_index_in(&project, &data, tmp.path())?;
    let strategy = lexical_strategy(&envelope2)?;
    ensure(
        strategy == "incremental_inline",
        format!(
            "second index against the same corpus should use the no-op \
             `incremental_inline` strategy (proving idempotency); \
             got {strategy:?}.\n\
             A regression here means every cass index invocation is doing a \
             full rebuild — burning CI budget and thrashing the atomic-swap pipeline.\n\
             envelope.indexing_stats: {}",
            envelope2.pointer("/indexing_stats").unwrap_or(&Value::Null)
        ),
    )?;
    Ok(())
}

#[test]
fn consecutive_indexes_produce_stable_conversation_and_message_totals() -> TestResult {
    let (tmp, project, data) = install_aider_fixture_project()?;
    let envelope1 = run_index_in(&project, &data, tmp.path())?;
    let envelope2 = run_index_in(&project, &data, tmp.path())?;

    let conv1 = conversation_count(&envelope1)?;
    let conv2 = conversation_count(&envelope2)?;
    let msg1 = message_count(&envelope1)?;
    let msg2 = message_count(&envelope2)?;

    ensure(
        matches!(conv1.cmp(&conv2), Ordering::Equal),
        format!(
            "conversation counts drifted across consecutive idempotent runs: \
             first={conv1}, second={conv2}. Either the planner is double-counting \
             or the source is being re-discovered as new on every run."
        ),
    )?;
    ensure(
        matches!(msg1.cmp(&msg2), Ordering::Equal),
        format!(
            "message counts drifted across consecutive idempotent runs: \
             first={msg1}, second={msg2}."
        ),
    )?;
    Ok(())
}
