//! End-to-end test for cass#257 sub-fix 1: Progress JSONL sink for
//! quality semantic backfill.
//!
//! Seeds a tiny canonical DB, runs `cass models backfill` with the
//! `CASS_SEMANTIC_PROGRESS_JSONL` env var pointing at a temp file,
//! and verifies the file contains a reasonable sequence of events.
//!
//! We use the `hash` embedder rather than fastembed so the test does
//! not depend on a downloaded model. The 20 named transition events
//! and their carried fields are exercised by unit tests in
//! `src/indexer/semantic_progress.rs::tests`; this file is the
//! integration proof that the CLI path actually opens the sink and
//! emits the bracket events.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin_cmd;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn sample_agent() -> Agent {
    Agent {
        id: None,
        slug: "codex".to_string(),
        name: "Codex".to_string(),
        version: None,
        kind: AgentKind::Cli,
    }
}

fn sample_conversation(external_id: &str, content: &str) -> Conversation {
    Conversation {
        id: None,
        agent_slug: "codex".to_string(),
        workspace: None,
        external_id: Some(external_id.to_string()),
        title: Some(format!("semantic backfill {external_id}")),
        source_path: PathBuf::from(format!("/tmp/cass-e2e/{external_id}.jsonl")),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_001_000),
        approx_tokens: None,
        metadata_json: json!({"fixture": "cass-257-semantic-progress-jsonl"}),
        messages: vec![Message {
            id: None,
            idx: 0,
            role: MessageRole::User,
            author: None,
            created_at: Some(1_700_000_000_500),
            content: content.to_string(),
            extra_json: json!({}),
            snippets: Vec::new(),
        }],
        source_id: "local".to_string(),
        origin_host: None,
    }
}

fn seed_canonical_db(db_path: &Path) -> TestResult {
    let storage = FrankenStorage::open(db_path)?;
    let agent_id = storage.ensure_agent(&sample_agent())?;
    storage.insert_conversation_tree(
        agent_id,
        None,
        &sample_conversation(
            "cass-257-progress-first",
            "first semantic input for #257 jsonl test",
        ),
    )?;
    storage.insert_conversation_tree(
        agent_id,
        None,
        &sample_conversation(
            "cass-257-progress-second",
            "second semantic input for #257 jsonl test",
        ),
    )?;
    Ok(())
}

#[test]
fn semantic_backfill_emits_progress_jsonl_with_named_events_when_env_var_is_set() -> TestResult {
    let workdir = TempDir::new()?;
    let data_dir = workdir.path().join("data");
    fs::create_dir_all(&data_dir)?;
    let db_path = workdir.path().join("agent_search.db");
    let jsonl_path = workdir.path().join("progress.jsonl");

    seed_canonical_db(&db_path)?;

    // Drive the backfill to publication by sizing the batch ≥ the corpus.
    let output = cargo_bin_cmd!("cass")
        .args([
            "models",
            "backfill",
            "--tier",
            "fast",
            "--embedder",
            "hash",
            "--batch-conversations",
            "8",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("--db")
        .arg(&db_path)
        .arg("--json")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_PROGRESS_JSONL", &jsonl_path)
        .timeout(Duration::from_secs(60))
        .output()?;

    assert!(
        output.status.success(),
        "cass models backfill failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout)?;
    let outcome: Value = serde_json::from_str(stdout.trim())?;
    // Sanity-check the existing contract still holds: the run published
    // the fast tier and reports a non-error status.
    assert_eq!(
        outcome.get("status").and_then(Value::as_str),
        Some("published"),
        "outcome: {outcome:#?}"
    );

    let raw = fs::read_to_string(&jsonl_path)
        .map_err(|e| format!("read jsonl at {}: {e}", jsonl_path.display()))?;
    assert!(!raw.trim().is_empty(), "progress JSONL was empty");

    let mut events = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("line {idx} not valid JSON: {e} :: {line}"))?;
        // Every event must carry the schema, an event name, a phase,
        // a sub-phase, the tier and embedder ID, and an elapsed_ms.
        assert_eq!(
            value.get("schema").and_then(Value::as_str),
            Some("cass.semantic.progress.v1"),
            "line {idx} missing schema"
        );
        assert!(
            value.get("event").and_then(Value::as_str).is_some(),
            "line {idx}"
        );
        assert!(
            value.get("phase").and_then(Value::as_str).is_some(),
            "line {idx}"
        );
        assert!(
            value.get("sub_phase").and_then(Value::as_str).is_some(),
            "line {idx}"
        );
        assert!(
            value.get("tier").and_then(Value::as_str).is_some(),
            "line {idx}"
        );
        assert!(
            value.get("embedder_id").and_then(Value::as_str).is_some(),
            "line {idx}"
        );
        assert!(
            value.get("elapsed_ms").and_then(Value::as_u64).is_some(),
            "line {idx}"
        );
        let event = value
            .get("event")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        events.push(event);
    }

    // We don't pin the exact event sequence because event count
    // depends on internal batch sizing — but every required terminal
    // and bracket event must appear.
    let must_contain = [
        "selection_start",
        "selection_count_start",
        "selection_count_done",
        "selection_candidates_start",
        "selection_candidates_done",
        "selection_done",
        "packet_replay_start",
        "packet_replay_done",
        "embed_batch_start",
        "embed_batch_done",
        "staging_write_start",
        "staging_write_done",
        "publish_start",
        "publish_done",
        "complete",
    ];
    for expected in must_contain {
        assert!(
            events.iter().any(|event| event == expected),
            "expected event `{expected}` in progress JSONL; got events: {events:?}"
        );
    }

    // Order check: selection_start must come strictly before
    // embed_batch_start (otherwise we'd be embedding before selection).
    let selection_idx = events
        .iter()
        .position(|event| event == "selection_start")
        .unwrap();
    let embed_idx = events
        .iter()
        .position(|event| event == "embed_batch_start")
        .unwrap();
    assert!(
        selection_idx < embed_idx,
        "selection_start must precede embed_batch_start; got {events:?}"
    );
    let count_done_idx = events
        .iter()
        .position(|event| event == "selection_count_done")
        .unwrap();
    let candidates_start_idx = events
        .iter()
        .position(|event| event == "selection_candidates_start")
        .unwrap();
    let candidates_done_idx = events
        .iter()
        .position(|event| event == "selection_candidates_done")
        .unwrap();
    let selection_done_idx = events
        .iter()
        .position(|event| event == "selection_done")
        .unwrap();
    assert!(
        count_done_idx < candidates_start_idx
            && candidates_start_idx < candidates_done_idx
            && candidates_done_idx < selection_done_idx,
        "count and bounded-candidate telemetry must bracket the real SQL stages; got {events:?}"
    );
    // publish_done must come strictly before complete on a successful run.
    let publish_done_idx = events
        .iter()
        .position(|event| event == "publish_done")
        .unwrap();
    let complete_idx = events.iter().position(|event| event == "complete").unwrap();
    assert!(
        publish_done_idx < complete_idx,
        "publish_done must precede complete on a successful run; got {events:?}"
    );

    Ok(())
}

#[test]
fn semantic_backfill_progress_jsonl_silent_when_env_var_is_unset() -> TestResult {
    // Inverse contract: without `CASS_SEMANTIC_PROGRESS_JSONL` set, no
    // sink file should be touched. We don't have a way to assert
    // "nothing was written" globally, but we CAN check the run
    // succeeds and the absence of the env var doesn't break the path.
    let workdir = TempDir::new()?;
    let data_dir = workdir.path().join("data");
    fs::create_dir_all(&data_dir)?;
    let db_path = workdir.path().join("agent_search.db");
    let jsonl_path = workdir.path().join("should-not-exist.jsonl");

    seed_canonical_db(&db_path)?;

    let output = cargo_bin_cmd!("cass")
        .args([
            "models",
            "backfill",
            "--tier",
            "fast",
            "--embedder",
            "hash",
            "--batch-conversations",
            "8",
            "--data-dir",
        ])
        .arg(&data_dir)
        .arg("--db")
        .arg(&db_path)
        .arg("--json")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        // Deliberately do NOT set CASS_SEMANTIC_PROGRESS_JSONL.
        .env_remove("CASS_SEMANTIC_PROGRESS_JSONL")
        .timeout(Duration::from_secs(60))
        .output()?;

    assert!(
        output.status.success(),
        "cass models backfill failed without sink env var\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !jsonl_path.exists(),
        "sink file should not exist when CASS_SEMANTIC_PROGRESS_JSONL is unset, but found {}",
        jsonl_path.display()
    );

    Ok(())
}
