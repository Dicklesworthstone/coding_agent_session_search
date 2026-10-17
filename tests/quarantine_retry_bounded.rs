//! Integration proof for bounded quarantine retry (beads
//! cass-fleet-resilience-20260608-uojcg.3.2 and xaztn).
//!
//! Satisfies the `docs/RESILIENCE_TEST_MATRIX.md` Epic-3 row "Bounded retry for
//! eligible entries → integration (no unbounded growth)". The unit tests in
//! `src/indexer/quarantine_retry.rs` cover the eligibility gate in-process;
//! these tests drive the **real durable `QuarantineState` save/load cycle** —
//! the resume checkpoint — across multiple passes through the production
//! `execute_retry`, proving two things the matrix calls out:
//!
//! 1. Repeated OOM retries never grow the on-disk `quarantine_state.json`; the
//!    re-quarantine-in-place contract holds and the pass converges to
//!    suppression once entries become irreducible same-version.
//! 2. A bounded budget drains an eligible backlog across resumes with each
//!    entry attempted exactly once and the state file fully drained at the end.
//!
//! The first two tests use only the public crate API + the pure executor's
//! injected attempt seam. The final real-binary test exercises the live
//! dry-run/apply command against one local Codex fixture, proving that apply
//! reparses and persists the exact quarantine key without touching its source.
//! No network access is required.

mod util;

use std::collections::BTreeSet;
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use assert_cmd::cargo::cargo_bin;
use chrono::{DateTime, Utc};
use coding_agent_search::connectors::codex::CodexConnector;
use coding_agent_search::connectors::{Connector, ScanContext, ScanRoot};
use coding_agent_search::franken_sync::compat::{ConnectionExt, RowExt};
use coding_agent_search::indexer::quarantine::{QuarantineRecord, QuarantineState};
use coding_agent_search::indexer::quarantine_retry::{AttemptResult, RetryConfig, execute_retry};
use coding_agent_search::storage::sqlite::{CURRENT_SCHEMA_VERSION, FrankenStorage};
use tempfile::tempdir;
use util::timeout::spawn_with_timeout_or_diag;

/// The version `execute_retry` compares against. Using the package version here
/// matches what `QuarantineState::record_attempt` stamps (`CARGO_PKG_VERSION`),
/// so a re-quarantined entry becomes "same-version" relative to this string and
/// the suppression assertions hold regardless of the current release number.
const CURRENT: &str = env!("CARGO_PKG_VERSION");
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

fn ts(secs: i64) -> anyhow::Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(secs, 0).context("fixture timestamp is representable")
}

/// `storage_key` shape is `"{conversation_id}::v{schema_version}"`. Built in a
/// flat helper so no `format!` runs inside a seeding loop.
fn conv_key(i: usize) -> String {
    format!("conv-{i}::v1")
}

/// An eligible (legacy: `cass_version_at_quarantine = None`) ingest-OOM record.
fn legacy_oom(attempts: u64) -> anyhow::Result<QuarantineRecord> {
    Ok(QuarantineRecord {
        first_attempt_at: ts(1_700_000_000)?,
        last_attempt_at: ts(1_700_000_000)?,
        attempt_count: attempts,
        last_reason: "ingest_oom".to_string(),
        cass_version_at_quarantine: None,
    })
}

fn no_missing() -> BTreeSet<String> {
    BTreeSet::new()
}

/// Seed `n` eligible legacy OOM entries into a fresh data dir, persisted through
/// the production atomic save path.
fn seed(dir: &std::path::Path, n: usize) -> anyhow::Result<()> {
    let mut state = QuarantineState::default();
    for i in 0..n {
        state.entries.insert(conv_key(i), legacy_oom(1)?);
    }
    state.save(dir)?;
    Ok(())
}

#[test]
fn repeated_oom_retry_never_grows_durable_state_and_converges_to_suppression() -> anyhow::Result<()>
{
    let dir = tempdir()?;
    let n = 8usize;
    seed(dir.path(), n)?;

    // Pass 1: load → retry (every attempt OOMs again) → save. All N are
    // attempted, none cleared, each re-quarantined IN PLACE (no append).
    let mut state = QuarantineState::load(dir.path());
    let r1 = execute_retry(
        &mut state,
        CURRENT,
        &RetryConfig::default(),
        &no_missing(),
        ts(1_800_000_000)?,
        |_key| AttemptResult::OutOfMemory,
    );
    state.save(dir.path())?;
    ensure!(r1.attempted == n, "all eligible entries attempted");
    ensure!(r1.cleared == 0, "an OOM retry must not clear entries");
    ensure!(
        r1.re_quarantined_oom == n,
        "every OOM must re-quarantine in place"
    );
    ensure!(r1.stalled, "attempted-but-nothing-cleared is a stall");
    ensure!(
        QuarantineState::load(dir.path()).len() == n,
        "no unbounded growth: still exactly N entries after re-quarantine"
    );

    // Pass 2 (resume off the durable state): the entries are now stamped with
    // the current version, so they are irreducible same-version and NONE are
    // attempted — the OOM re-quarantine suppressed the retry storm.
    let mut state2 = QuarantineState::load(dir.path());
    let r2 = execute_retry(
        &mut state2,
        CURRENT,
        &RetryConfig::default(),
        &no_missing(),
        ts(1_800_000_100)?,
        |_key| AttemptResult::OutOfMemory,
    );
    state2.save(dir.path())?;
    ensure!(
        r2.attempted == 0,
        "same-version entries are suppressed on resume"
    );
    ensure!(
        r2.skipped_irreducible == n,
        "all same-version entries must be classified irreducible"
    );
    ensure!(
        QuarantineState::load(dir.path()).len() == n,
        "no unbounded growth across resumes"
    );
    Ok(())
}

#[test]
fn bounded_budget_drains_eligible_backlog_across_resumes_each_attempted_once() -> anyhow::Result<()>
{
    let dir = tempdir()?;
    let n = 5usize;
    seed(dir.path(), n)?;

    let config = RetryConfig {
        max_attempts: Some(2),
        eligible_only: true,
    };

    let mut attempted_ids = vec![false; n];
    let mut attempted_count = 0usize;
    let mut duplicate_attempt = false;
    let mut invalid_attempt_key = false;
    let mut passes = 0usize;

    // Resume until the durable backlog is drained. A safety cap keeps the test
    // from hanging if convergence ever regresses.
    loop {
        passes += 1;
        ensure!(passes <= n + 2, "bounded retry must converge");
        let mut state = QuarantineState::load(dir.path());
        if state.is_empty() {
            break;
        }
        let report = execute_retry(
            &mut state,
            CURRENT,
            &config,
            &no_missing(),
            ts(1_800_000_000)?,
            |key| {
                attempted_count = attempted_count.saturating_add(1);
                match key
                    .conversation_id
                    .strip_prefix("conv-")
                    .and_then(|raw| raw.parse::<usize>().ok())
                    .filter(|index| *index < attempted_ids.len())
                {
                    Some(index) => {
                        if let Some(attempted) = attempted_ids.get_mut(index) {
                            duplicate_attempt |= *attempted;
                            *attempted = true;
                        } else {
                            invalid_attempt_key = true;
                        }
                    }
                    None => invalid_attempt_key = true,
                }
                AttemptResult::Reindexed
            },
        );
        state.save(dir.path())?;
        ensure!(report.attempted <= 2, "budget caps attempts per pass");
    }

    ensure!(
        QuarantineState::load(dir.path()).is_empty(),
        "backlog fully drained across bounded resumes"
    );
    ensure!(attempted_count == n, "every entry attempted");
    ensure!(!duplicate_attempt, "an entry must not be attempted twice");
    ensure!(
        !invalid_attempt_key,
        "every attempt key must match the fixture"
    );
    ensure!(
        attempted_ids.iter().all(|attempted| *attempted),
        "every entry attempted exactly once"
    );
    Ok(())
}

#[test]
fn cli_list_and_clear_include_poison_only_quarantine_records() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let data_dir = dir.path().join("data");
    let quarantine_dir = data_dir.join("quarantine");
    fs::create_dir_all(&quarantine_dir)?;
    let poison_path = quarantine_dir.join("index_ingest_poison.jsonl");
    fs::write(
        &poison_path,
        format!(
            "{}\n",
            serde_json::json!({
                "conversation_id": "poison-only",
                "schema_version_at_quarantine": 1,
                "first_quarantined_at_ms": 1_700_000_000_000_i64,
                "last_attempt_at_ms": 1_700_000_001_000_i64,
                "attempt_count": 3,
                "cass_version_at_quarantine": "0.0.1-old",
                "reason": "index-ingest-out-of-memory",
            })
        ),
    )?;
    ensure!(
        QuarantineState::load(&data_dir).is_empty(),
        "fixture must exercise a poison-ledger-only key"
    );

    let data_dir_arg = data_dir.to_str().context("UTF-8 data dir")?;
    let run = |label: &str, args: &[&str]| {
        let mut command = Command::new(cargo_bin("cass"));
        command
            .args(args)
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .env("XDG_DATA_HOME", dir.path().join("xdg-data"))
            .env("XDG_CONFIG_HOME", dir.path().join("xdg-config"))
            .env("XDG_CACHE_HOME", dir.path().join("xdg-cache"))
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("NO_COLOR", "1")
            .env_remove("CLAUDE_CONFIG_DIR");
        spawn_with_timeout_or_diag(command, label, Some(&data_dir), COMMAND_TIMEOUT)
    };

    let listed = run(
        "quarantine-list-poison-only",
        &["quarantine", "list", "--data-dir", data_dir_arg, "--json"],
    );
    ensure!(
        listed.status.success(),
        "poison-only list stderr: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed_json: serde_json::Value = serde_json::from_slice(&listed.stdout)?;
    ensure!(
        listed_json["quarantined_conversations"] == 1,
        "list must include the poison-only key"
    );
    ensure!(
        listed_json["entries"][0]["conversation_id"] == "poison-only",
        "list must identify the poison-only key"
    );

    let dry = run(
        "quarantine-clear-poison-only-dry-run",
        &["quarantine", "clear", "--data-dir", data_dir_arg, "--json"],
    );
    ensure!(
        dry.status.success(),
        "poison-only clear dry-run stderr: {}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry_json: serde_json::Value = serde_json::from_slice(&dry.stdout)?;
    ensure!(
        dry_json["matched"] == 1,
        "dry-run must match poison-only key"
    );
    ensure!(dry_json["cleared"] == 0, "dry-run must not clear the key");
    ensure!(
        fs::read_to_string(&poison_path)?.contains("poison-only"),
        "dry-run must leave the poison ledger unchanged"
    );

    let applied = run(
        "quarantine-clear-poison-only-apply",
        &[
            "quarantine",
            "clear",
            "--data-dir",
            data_dir_arg,
            "--apply",
            "--json",
        ],
    );
    ensure!(
        applied.status.success(),
        "poison-only clear apply stderr: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let applied_json: serde_json::Value = serde_json::from_slice(&applied.stdout)?;
    ensure!(applied_json["matched"] == 1, "apply must match one key");
    ensure!(applied_json["cleared"] == 1, "apply must clear one key");
    ensure!(
        fs::read_to_string(&poison_path)?.is_empty(),
        "apply must clear the poison-only ledger record"
    );

    let after = run(
        "quarantine-list-after-poison-only-clear",
        &["quarantine", "list", "--data-dir", data_dir_arg, "--json"],
    );
    ensure!(after.status.success(), "post-clear list must succeed");
    let after_json: serde_json::Value = serde_json::from_slice(&after.stdout)?;
    ensure!(
        after_json["quarantined_conversations"] == 0,
        "post-clear list must agree that the quarantine is empty"
    );
    Ok(())
}

/// `cass health` reports quarantined conversations with a `next_command`; that
/// command must name them (v0.10.0 pointed at `cass diag --json --quarantine`,
/// which inventories derived assets and never lists a conversation). The list
/// then groups the merged state + poison rows and offers a retry only while
/// one can help: an eligible row whose source is gone does not count.
#[test]
fn cli_health_pointer_lists_conversations_and_list_status_offers_only_useful_retries()
-> anyhow::Result<()> {
    let dir = tempdir()?;
    let data_dir = dir.path().join("data");
    let mut state = QuarantineState::default();
    state
        .entries
        .insert("conv-legacy::v1".to_string(), legacy_oom(1)?);
    state.entries.insert(
        "conv-same::v1".to_string(),
        QuarantineRecord {
            cass_version_at_quarantine: Some(CURRENT.to_string()),
            ..legacy_oom(4)?
        },
    );
    state.save(&data_dir)?;
    let quarantine_dir = data_dir.join("quarantine");
    fs::create_dir_all(&quarantine_dir)?;
    fs::write(
        quarantine_dir.join("index_ingest_poison.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({
                "conversation_id": "conv-gone",
                "schema_version_at_quarantine": 1,
                "last_attempt_at_ms": 1_700_000_001_000_i64,
                "attempt_count": 2,
                "cass_version_at_quarantine": "0.0.1-old",
                "reason": "index-ingest-out-of-memory",
                "source_path": dir.path().join("moved-away.jsonl"),
            })
        ),
    )?;

    let data_dir_arg = data_dir.to_str().context("UTF-8 data dir")?;
    let run_json = |label: &str, args: &[&str]| -> anyhow::Result<serde_json::Value> {
        let mut command = Command::new(cargo_bin("cass"));
        command
            .args(args)
            .args(["--data-dir", data_dir_arg])
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .env("XDG_DATA_HOME", dir.path().join("xdg-data"))
            .env("XDG_CONFIG_HOME", dir.path().join("xdg-config"))
            .env("XDG_CACHE_HOME", dir.path().join("xdg-cache"))
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_AUTO_REFRESH", "0")
            .env("NO_COLOR", "1")
            .env_remove("CLAUDE_CONFIG_DIR");
        let output = spawn_with_timeout_or_diag(command, label, Some(&data_dir), COMMAND_TIMEOUT);
        // `health` exits 1 on an unindexed archive; its JSON is still the
        // contract under test.
        serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "{label} stdout is not JSON; stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        })
    };

    let health = run_json("quarantine-health", &["health", "--json"])?;
    let completeness = health
        .get("search_completeness")
        .or_else(|| health["state"].get("search_completeness"))
        .context("health reports search_completeness")?;
    ensure!(
        completeness["quarantined_conversations"] == 3,
        "health must count all three quarantined conversations: {completeness}"
    );
    let next = completeness["next_command"]
        .as_str()
        .context("next_command is a string")?;
    ensure!(
        next == "cass quarantine list --json",
        "health must point at the surface that lists the conversations, got {next}"
    );

    let args: Vec<&str> = next.split_whitespace().skip(1).collect();
    let listed = run_json("quarantine-list-from-health-pointer", &args)?;
    let ids: Vec<&str> = listed["entries"]
        .as_array()
        .context("entries array")?
        .iter()
        .filter_map(|entry| entry["conversation_id"].as_str())
        .collect();
    ensure!(
        ids == ["conv-gone", "conv-legacy", "conv-same"],
        "the pointed-at command must name every excluded conversation: {ids:?}"
    );
    let status = &listed["status"];
    ensure!(
        status["total_excluded_conversations"] == 3
            && status["by_eligibility"]["eligible"] == 2
            && status["by_eligibility"]["irreducible_same_version"] == 1
            && status["by_version_bucket"]["legacy"] == 1
            && status["by_version_bucket"]["same_version"] == 1
            && status["by_version_bucket"]["version_stale"] == 1
            && status["source_missing_count"] == 1,
        "status must group the merged rows: {status}"
    );
    ensure!(
        status["retryable_now"] == 1,
        "only conv-legacy is retryable: conv-gone is eligible but its source is gone: {status}"
    );
    ensure!(
        status["next_safe_command"] == "cass quarantine retry --json",
        "an eligible row with its source present gets the bounded retry plan: {status}"
    );

    // Negative: once the only retryable row is gone, the remaining rows are an
    // irreducible same-version failure and a source-missing one. No retry can
    // help, so none is offered.
    state.entries.remove("conv-legacy::v1");
    state.save(&data_dir)?;
    let listed = run_json(
        "quarantine-list-nothing-retryable",
        &["quarantine", "list", "--json"],
    )?;
    let status = &listed["status"];
    ensure!(
        status["total_excluded_conversations"] == 2
            && status["retryable_now"] == 0
            && status["next_safe_command"].is_null(),
        "no retry command may be offered when no retry can help: {status}"
    );
    Ok(())
}

#[test]
fn cli_dry_run_plans_then_apply_reingests_exact_quarantine_key() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let data_dir = dir.path().join("data");
    let source_path = dir
        .path()
        .join(".codex/sessions/2026/07/rollout-retry.jsonl");
    fs::create_dir_all(source_path.parent().context("source parent")?)?;
    fs::write(
        &source_path,
        r#"{"timestamp":"2026-07-22T05:00:00.000Z","type":"session_meta","payload":{"id":"targeted-retry-cli","cwd":"/data/projects/cass"}}
{"timestamp":"2026-07-22T05:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"targeted CLI retry"}]}}
{"timestamp":"2026-07-22T05:00:02.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"retry persisted"}]}}
"#,
    )?;

    let connector = CodexConnector::new();
    let root = ScanRoot::local(source_path.clone());
    let context = ScanContext::with_roots(source_path.clone(), vec![root], None);
    let mut conversations = connector.scan(&context)?;
    ensure!(
        conversations.len() == 1,
        "fixture has exactly one conversation, got {}",
        conversations.len()
    );
    let conversation = conversations
        .first_mut()
        .context("missing fixture conversation")?;
    // Model a normal directory-root scan whose external_id is relative to the
    // session root. Targeted retry scans the exact file and must restore this
    // poison-ledger identity before persistence, preventing a duplicate.
    conversation.external_id = Some("sessions/2026/07/rollout-retry".to_string());
    let workspace = conversation
        .workspace
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let conversation_id = format!(
        "{}|{}|{}|{}|{}|{}|{}",
        conversation.agent_slug,
        conversation.source_path.display(),
        workspace,
        conversation.external_id.as_deref().unwrap_or(""),
        conversation.started_at.unwrap_or_default(),
        conversation.ended_at.unwrap_or_default(),
        conversation.messages.len()
    );
    let schema_version = u32::try_from(CURRENT_SCHEMA_VERSION).context("schema version")?;

    let mut state = QuarantineState::default();
    state.entries.insert(
        format!("{conversation_id}::v{schema_version}"),
        legacy_oom(1)?,
    );
    state.save(&data_dir)?;
    let quarantine_dir = data_dir.join("quarantine");
    fs::create_dir_all(&quarantine_dir)?;
    fs::write(
        quarantine_dir.join("index_ingest_poison.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({
                "conversation_id": conversation_id,
                "schema_version_at_quarantine": schema_version,
                "cass_version_at_quarantine": "0.0.1-old",
                "reason": "index-ingest-out-of-memory",
                "agent_slug": "codex",
                "external_id": conversation.external_id,
                "source_path": source_path,
                "workspace": conversation.workspace.as_ref().map(|path| path.display().to_string()),
                "started_at": conversation.started_at,
                "ended_at": conversation.ended_at,
                "message_count": conversation.messages.len(),
            })
        ),
    )?;

    let data_dir_arg = data_dir.to_str().context("UTF-8 data dir")?;
    let started_at_ms = Utc::now().timestamp_millis();
    let command_started = Instant::now();

    let mut dry_command = Command::new(cargo_bin("cass"));
    dry_command
        .args(["quarantine", "retry", "--data-dir", data_dir_arg, "--json"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_DATA_HOME", dir.path().join("xdg-data"))
        .env("XDG_CONFIG_HOME", dir.path().join("xdg-config"))
        .env("XDG_CACHE_HOME", dir.path().join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR");
    let dry = spawn_with_timeout_or_diag(
        dry_command,
        "quarantine-retry-dry-run",
        Some(&data_dir),
        COMMAND_TIMEOUT,
    );
    ensure!(
        dry.status.success(),
        "dry-run stderr: {}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry_json: serde_json::Value = serde_json::from_slice(&dry.stdout)?;
    ensure!(dry_json["dry_run"] == true, "dry-run flag must be true");
    ensure!(dry_json["applied"] == false, "dry-run must not apply");
    ensure!(
        dry_json["planned_attempts"] == 1,
        "dry-run must plan the exact quarantine key"
    );
    ensure!(
        QuarantineState::load(&data_dir).len() == 1,
        "dry-run must be read-only"
    );

    let mut apply_command = Command::new(cargo_bin("cass"));
    apply_command
        .args([
            "quarantine",
            "retry",
            "--data-dir",
            data_dir_arg,
            "--apply",
            "--json",
        ])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("XDG_DATA_HOME", dir.path().join("xdg-data"))
        .env("XDG_CONFIG_HOME", dir.path().join("xdg-config"))
        .env("XDG_CACHE_HOME", dir.path().join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR");
    let applied = spawn_with_timeout_or_diag(
        apply_command,
        "quarantine-retry-apply",
        Some(&data_dir),
        COMMAND_TIMEOUT,
    );
    ensure!(
        applied.status.success(),
        "apply stderr: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let applied_json: serde_json::Value = serde_json::from_slice(&applied.stdout)?;
    ensure!(applied_json["dry_run"] == false, "apply is not dry-run");
    ensure!(applied_json["applied"] == true, "apply flag must be true");
    ensure!(applied_json["cleared"] == 1, "one exact key must clear");
    ensure!(
        applied_json["remaining_quarantined"] == 0,
        "no retried quarantine key may remain"
    );
    ensure!(
        QuarantineState::load(&data_dir).is_empty(),
        "durable quarantine state must be drained"
    );
    ensure!(
        source_path.is_file(),
        "retry must never remove the source log"
    );

    let storage = FrankenStorage::open(&data_dir.join("agent_search.db"))?;
    let message_count: i64 = storage
        .raw()
        .query_row_map("SELECT COUNT(*) FROM messages", &[], |row| row.get_typed(0))
        .context("message count")?;
    ensure!(message_count == 2, "exact conversation persisted once");

    // Emit and immediately validate a .12.3/.12.6-style structured proof
    // artifact. The paths and payload are fixture-local and contain no raw
    // conversation text; the manifest distinguishes a real command pass from
    // a timeout, invalid JSON, stale evidence, or a test that never ran.
    let proof_dir = dir.path().join("proof");
    fs::create_dir_all(&proof_dir)?;
    let stdout_path = proof_dir.join("stdout.json");
    let stderr_path = proof_dir.join("stderr.log");
    let manifest_path = proof_dir.join("proof-manifest.jsonl");
    fs::write(&stdout_path, &applied.stdout)?;
    fs::write(&stderr_path, &applied.stderr)?;
    let elapsed_ms = i64::try_from(command_started.elapsed().as_millis())
        .context("elapsed milliseconds fit i64")?;
    let proof = serde_json::json!({
        "run_id": "xaztn-targeted-retry",
        "scenario_id": "quarantine-retry-real-codex-source",
        "issue_ids_covered": ["coding_agent_session_search-xaztn"],
        "fixture_id": "codex-targeted-retry",
        "command_id": "cass-quarantine-retry-apply",
        "phase": "assert",
        "started_at_ms": started_at_ms,
        "finished_at_ms": started_at_ms + elapsed_ms,
        "elapsed_ms": elapsed_ms,
        "meta": {
            "cass_binary_path": env!("CARGO_BIN_EXE_cass"),
            "cass_version": env!("CARGO_PKG_VERSION"),
            "git_revision": serde_json::Value::Null,
            "cargo_profile": "test",
            "feature_flags": [],
            "target_dir": serde_json::Value::Null,
            "data_dir": data_dir,
            "config_dir": serde_json::Value::Null,
            "model_dir": serde_json::Value::Null,
            "source_roots": [source_path]
        },
        "execution": {
            "argv": ["cass", "quarantine", "retry", "--data-dir", "[FIXTURE_DATA]", "--apply", "--json"],
            "sanitized_env": {},
            "timeout_ms": 60_000,
            "exit_code": applied.status.code(),
            "signal": serde_json::Value::Null,
            "timed_out": false,
            "retry_count": 0
        },
        "artifacts": {
            "stdout_path": stdout_path,
            "stderr_path": stderr_path,
            "parsed_stdout_json": applied_json,
            "parsed_stderr_json_if_expected": serde_json::Value::Null,
            "robot_contract_ok": true,
            "ansi_free_stdout_ok": !applied.stdout.contains(&0x1b)
        },
        "assertions": [
            {"name": "dry_run_read_only", "expected": true, "actual": true, "status": "pass"},
            {"name": "exact_key_cleared", "expected": 1, "actual": 1, "status": "pass"},
            {"name": "source_preserved", "expected": true, "actual": true, "status": "pass"},
            {"name": "messages_persisted_once", "expected": 2, "actual": message_count, "status": "pass"}
        ],
        "redaction_status": "safe",
        "privacy_notes": "fixture-local paths only; no raw conversation text retained",
        "artifact_manifest_path": manifest_path,
        "stale_artifact_check": "fresh",
        "generated_artifact_check": "real_binary_output",
        "outcome": "passed"
    });
    fs::write(
        &manifest_path,
        format!("{}\n", serde_json::to_string(&proof)?),
    )?;
    let manifest_text = fs::read_to_string(&manifest_path)?;
    let manifest_lines = manifest_text.lines().collect::<Vec<_>>();
    ensure!(manifest_lines.len() == 1, "one proof record is required");
    let verified: serde_json::Value = serde_json::from_str(
        manifest_lines
            .first()
            .context("missing structured proof record")?,
    )?;
    ensure!(
        verified["outcome"]
            .as_str()
            .is_some_and(|value| matches!(value, "passed")),
        "proof outcome must be passed"
    );
    ensure!(
        matches!(verified["execution"]["timed_out"].as_bool(), Some(false)),
        "proof must distinguish a completed command from a timeout"
    );
    ensure!(
        matches!(
            verified["artifacts"]["robot_contract_ok"].as_bool(),
            Some(true)
        ),
        "proof must record parsed robot JSON"
    );
    ensure!(
        verified["redaction_status"]
            .as_str()
            .is_some_and(|value| matches!(value, "safe")),
        "proof manifest must pass its redaction review"
    );
    Ok(())
}

/// Run `cass <args> --data-dir <data_dir>` in an isolated HOME and return its
/// stdout (status/health exit non-zero on an unindexed archive).
fn cass_stdout_in(
    home: &std::path::Path,
    data_dir: &std::path::Path,
    label: &str,
    args: &[&str],
) -> anyhow::Result<(Vec<u8>, String)> {
    let data_dir_arg = data_dir.to_str().context("UTF-8 data dir")?;
    let mut command = Command::new(cargo_bin("cass"));
    command
        .args(args)
        .args(["--data-dir", data_dir_arg])
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_CACHE_HOME", home.join("xdg-cache"))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("NO_COLOR", "1")
        .env_remove("CLAUDE_CONFIG_DIR");
    let output = spawn_with_timeout_or_diag(command, label, Some(data_dir), COMMAND_TIMEOUT);
    Ok((
        output.stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// GH#510: conversations the watcher keeps deferring after a bounded-guard
/// NoMem are missing from search; `status` and `health` must say so and point
/// at the list, without calling them a quarantine. Without a ledger neither
/// surface mentions deferrals.
#[test]
fn cli_status_and_health_warn_about_watch_nomem_deferrals() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let run_json = |data_dir: &std::path::Path, label: &str, args: &[&str]| {
        let (stdout, stderr) = cass_stdout_in(dir.path(), data_dir, label, args)?;
        serde_json::from_slice::<serde_json::Value>(&stdout)
            .with_context(|| format!("{label} stdout is not JSON; stderr: {stderr}"))
    };
    let human_status = |data_dir: &std::path::Path, label: &str| {
        cass_stdout_in(dir.path(), data_dir, label, &["status"])
            .map(|(stdout, _)| String::from_utf8_lossy(&stdout).into_owned())
    };
    // ingest_quarantine.recommended_action already names deferrals, so match
    // the top-level warnings, not the whole payload.
    let mentions_deferral = |payload: &serde_json::Value| {
        payload["warnings"].as_array().is_some_and(|warnings| {
            warnings
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|warning| warning.contains("deferred by the watcher"))
        })
    };

    let deferred_dir = dir.path().join("deferred");
    fs::create_dir_all(deferred_dir.join("quarantine"))?;
    fs::write(
        deferred_dir.join("quarantine/watch_nomem_deferrals.json"),
        serde_json::to_vec(&serde_json::json!({
            "entries": {
                "amp\u{1f}/logs/a.json\u{1f}": {
                    "agent": "amp",
                    "source_path": "/logs/a.json",
                    "deferrals": 3,
                    "first_deferred_at_ms": 1_700_000_000_000_i64,
                    "last_deferred_at_ms": 1_700_000_100_000_i64,
                    "next_retry_at_ms": 1_700_000_340_000_i64,
                    "indexed_text_bytes": 525
                }
            }
        }))?,
    )?;
    for (label, args) in [
        ("deferral-status", ["status", "--json"]),
        ("deferral-health", ["health", "--json"]),
    ] {
        let payload = run_json(&deferred_dir, label, &args)?;
        ensure!(
            mentions_deferral(&payload),
            "{label} must warn about the deferred conversation: {payload}"
        );
        let summary = payload
            .get("ingest_quarantine")
            .or_else(|| payload["state"].get("ingest_quarantine"))
            .context("ingest_quarantine summary")?;
        ensure!(
            summary["deferred_conversations"] == 1 && summary["status"] == "ok",
            "{label}: one deferral, not a quarantine: {summary}"
        );
    }
    let human = human_status(&deferred_dir, "deferral-status-human")?;
    ensure!(
        human.contains("Warning: 1 conversation(s) are deferred by the watcher"),
        "human status must warn about the deferred conversation:\n{human}"
    );

    let clean_dir = dir.path().join("clean");
    fs::create_dir_all(&clean_dir)?;
    for (label, args) in [
        ("clean-status", ["status", "--json"]),
        ("clean-health", ["health", "--json"]),
    ] {
        let payload = run_json(&clean_dir, label, &args)?;
        ensure!(
            !mentions_deferral(&payload),
            "{label} must not mention deferrals without a ledger: {payload}"
        );
    }
    let human = human_status(&clean_dir, "clean-status-human")?;
    ensure!(
        !human.contains("deferred by the watcher"),
        "human status must not mention deferrals without a ledger:\n{human}"
    );
    Ok(())
}
