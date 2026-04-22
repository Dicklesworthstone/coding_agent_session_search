use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin_cmd;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::semantic_manifest::SemanticManifest;
use coding_agent_search::storage::sqlite::FrankenStorage;
use serde_json::{Value, json};

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
        metadata_json: json!({"fixture": "semantic-backfill-robot"}),
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
        &sample_conversation("first", "first robot semantic backfill message"),
    )?;
    storage.insert_conversation_tree(
        agent_id,
        None,
        &sample_conversation("second", "second robot semantic backfill message"),
    )?;
    Ok(())
}

fn run_robot_backfill(data_dir: &Path, db_path: &Path) -> TestResult<Value> {
    let output = cargo_bin_cmd!("cass")
        .args([
            "models",
            "backfill",
            "--tier",
            "fast",
            "--embedder",
            "hash",
            "--batch-conversations",
            "1",
            "--data-dir",
        ])
        .arg(data_dir)
        .arg("--db")
        .arg(db_path)
        .arg("--json")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .timeout(Duration::from_secs(20))
        .output()?;

    if !output.status.success() {
        return Err(format!(
            "cass models backfill failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let stdout = String::from_utf8(output.stdout)?;
    Ok(serde_json::from_str(stdout.trim())?)
}

#[test]
fn robot_models_backfill_checkpoints_then_publishes_fast_tier() -> TestResult {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("cass-data");
    let db_path = temp.path().join("agent_search.db");
    seed_canonical_db(&db_path)?;

    let first = run_robot_backfill(&data_dir, &db_path)?;
    assert_eq!(first["status"], "checkpointed");
    assert_eq!(
        first["next_step"],
        "rerun the same command to continue the resumable backfill"
    );
    assert_eq!(first["tier"], "fast");
    assert_eq!(first["embedder_id"], "fnv1a-384");
    assert_eq!(first["batch_conversations_limit"], 1);
    assert_eq!(first["embedded_docs"], 1);
    assert_eq!(first["conversations_processed"], 1);
    assert_eq!(first["total_conversations"], 2);
    assert_eq!(first["checkpoint_saved"], true);
    assert_eq!(first["published"], false);
    assert_eq!(first["backlog"]["total_conversations"], 2);
    assert_eq!(first["backlog"]["fast_tier_processed"], 0);
    assert!(
        Path::new(
            first["manifest_path"]
                .as_str()
                .ok_or("manifest_path should be a string")?
        )
        .is_file()
    );
    assert!(
        Path::new(
            first["index_path"]
                .as_str()
                .ok_or("staged index_path should be a string")?
        )
        .is_file()
    );

    let second = run_robot_backfill(&data_dir, &db_path)?;
    assert_eq!(second["status"], "published");
    assert_eq!(second["next_step"], "semantic tier is ready");
    assert_eq!(second["tier"], "fast");
    assert_eq!(second["embedder_id"], "fnv1a-384");
    assert_eq!(second["embedded_docs"], 1);
    assert_eq!(second["conversations_processed"], 2);
    assert_eq!(second["total_conversations"], 2);
    assert_eq!(second["checkpoint_saved"], false);
    assert_eq!(second["published"], true);
    assert_eq!(second["backlog"]["fast_tier_processed"], 2);
    assert!(
        Path::new(
            second["index_path"]
                .as_str()
                .ok_or("published index_path should be a string")?
        )
        .is_file()
    );

    let manifest = SemanticManifest::load(&data_dir)?.ok_or("semantic manifest should exist")?;
    assert!(manifest.checkpoint.is_none());
    assert_eq!(
        manifest.fast_tier.as_ref().map(|artifact| (
            artifact.ready,
            artifact.conversation_count,
            artifact.doc_count
        )),
        Some((true, 2, 2))
    );
    assert_eq!(manifest.backlog.total_conversations, 2);
    assert_eq!(manifest.backlog.fast_tier_processed, 2);

    Ok(())
}
