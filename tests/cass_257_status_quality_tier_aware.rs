//! Test for cass#257 sub-fix 3: `cass status` reports the quality
//! tier as published independently of the progressive/hybrid stack.
//!
//! Strategy:
//! 1. Seed a tiny canonical DB.
//! 2. Run `cass models backfill --tier quality --embedder hash`
//!    to publish the quality vector index without ever building
//!    the fast tier (so `progressive_ready` stays false).
//! 3. Snapshot `cass status --json` and assert the new additive
//!    fields are present and have the right values.

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
        title: Some(format!("cass-257 sub-fix 3 {external_id}")),
        source_path: PathBuf::from(format!("/tmp/cass-e2e/{external_id}.jsonl")),
        started_at: Some(1_700_000_000_000),
        ended_at: Some(1_700_000_001_000),
        approx_tokens: None,
        metadata_json: json!({"fixture": "cass-257-status-quality-tier"}),
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

fn seed(db_path: &Path) -> TestResult {
    let storage = FrankenStorage::open(db_path)?;
    let agent_id = storage.ensure_agent(&sample_agent())?;
    storage.insert_conversation_tree(
        agent_id,
        None,
        &sample_conversation("q-only-one", "one quality-only message"),
    )?;
    storage.insert_conversation_tree(
        agent_id,
        None,
        &sample_conversation("q-only-two", "two quality-only message"),
    )?;
    Ok(())
}

fn run_status_json(data_dir: &Path, db_path: &Path) -> TestResult<Value> {
    let output = cargo_bin_cmd!("cass")
        .args(["status", "--data-dir"])
        .arg(data_dir)
        .arg("--db")
        .arg(db_path)
        .args(["--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .timeout(Duration::from_secs(45))
        .output()?;
    assert!(
        output.status.success(),
        "cass status failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    Ok(serde_json::from_str(stdout.trim())?)
}

#[test]
fn cass_status_reports_quality_tier_published_and_semantic_only_search_available() -> TestResult {
    let workdir = TempDir::new()?;
    let data_dir = workdir.path().join("data");
    fs::create_dir_all(&data_dir)?;
    let db_path = workdir.path().join("agent_search.db");
    seed(&db_path)?;

    // Build the quality tier with the `hash` embedder so we don't
    // depend on a downloaded model. Important: do NOT build the fast
    // tier here; we want progressive_ready to remain false to
    // demonstrate the sub-fix-3 quality-tier-only path.
    let backfill = cargo_bin_cmd!("cass")
        .args([
            "models",
            "backfill",
            "--tier",
            "quality",
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
        .timeout(Duration::from_secs(60))
        .output()?;
    assert!(
        backfill.status.success(),
        "cass models backfill (quality) failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&backfill.stdout),
        String::from_utf8_lossy(&backfill.stderr)
    );
    let backfill_outcome: Value =
        serde_json::from_str(String::from_utf8_lossy(&backfill.stdout).trim())?;
    assert_eq!(
        backfill_outcome.get("status").and_then(Value::as_str),
        Some("published"),
        "expected quality-tier publication; got {backfill_outcome:#?}"
    );

    let status = run_status_json(&data_dir, &db_path)?;
    let semantic = status
        .get("semantic")
        .expect("status.semantic must exist on the status JSON");

    // Sub-fix-3 contract: the additive fields are present.
    let quality_tier_published = semantic
        .get("quality_tier_published")
        .and_then(Value::as_bool)
        .expect("semantic.quality_tier_published must exist on the status JSON");
    let semantic_only_search_available = semantic
        .get("semantic_only_search_available")
        .and_then(Value::as_bool)
        .expect("semantic.semantic_only_search_available must exist on the status JSON");

    assert!(
        quality_tier_published,
        "expected quality_tier_published=true after `cass models backfill --tier quality` publishes; full semantic block: {semantic:#?}"
    );
    assert!(
        semantic_only_search_available,
        "expected semantic_only_search_available=true while the quality tier is queryable; full semantic block: {semantic:#?}"
    );

    // Backwards-compatible: existing fields keep their meaning. The
    // quality_tier sub-object should agree, and progressive_ready may
    // be either — it's not constrained by sub-fix 3.
    let quality_tier = semantic
        .get("quality_tier")
        .expect("status.semantic.quality_tier must exist");
    assert_eq!(
        quality_tier.get("ready").and_then(Value::as_bool),
        Some(true),
        "quality_tier.ready=true must agree with quality_tier_published"
    );

    Ok(())
}
