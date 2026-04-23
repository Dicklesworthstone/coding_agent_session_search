use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use assert_cmd::cargo::cargo_bin_cmd;
use coding_agent_search::default_data_dir;
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

fn run_robot_scheduled_backfill_paused(data_dir: &Path, db_path: &Path) -> TestResult<Value> {
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
            "--scheduled",
            "--data-dir",
        ])
        .arg(data_dir)
        .arg("--db")
        .arg(db_path)
        .arg("--json")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_SEMANTIC_BACKFILL_FOREGROUND_ACTIVE", "1")
        .timeout(Duration::from_secs(20))
        .output()?;

    if !output.status.success() {
        return Err(format!(
            "cass scheduled models backfill failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let stdout = String::from_utf8(output.stdout)?;
    Ok(serde_json::from_str(stdout.trim())?)
}

#[derive(Debug, Clone)]
struct LiveBootstrapHarnessConfig {
    data_dir: PathBuf,
    db_path: PathBuf,
    artifact_root: PathBuf,
    query: String,
    min_hits: usize,
    limit: usize,
    tier: String,
    embedder: String,
    batch_conversations: usize,
    max_backfill_runs: usize,
    timeout: Duration,
    run_backfill: bool,
}

#[derive(Debug)]
struct LiveRobotArtifact {
    label: String,
    args: Vec<String>,
    exit_code: i32,
    duration_ms: u64,
    stdout: String,
    stderr: String,
    stdout_json: Option<Value>,
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn live_rollout_run_label() -> String {
    format!("run-{}-pid{}", now_ms(), std::process::id())
}

fn resolve_live_bootstrap_paths(
    data_dir_override: Option<PathBuf>,
    db_override: Option<PathBuf>,
    artifact_base_override: Option<PathBuf>,
    run_label: &str,
) -> (PathBuf, PathBuf, PathBuf) {
    let data_dir = data_dir_override.unwrap_or_else(default_data_dir);
    let db_path = db_override.unwrap_or_else(|| data_dir.join("agent_search.db"));
    let artifact_root = artifact_base_override
        .unwrap_or_else(|| data_dir.join("test-artifacts").join("ibuuh.11-live"))
        .join(run_label);
    (data_dir, db_path, artifact_root)
}

impl LiveBootstrapHarnessConfig {
    fn from_env() -> Self {
        let run_label = live_rollout_run_label();
        let (data_dir, db_path, artifact_root) = resolve_live_bootstrap_paths(
            std::env::var_os("CASS_TEST_LIVE_DATA_DIR").map(PathBuf::from),
            std::env::var_os("CASS_TEST_LIVE_DB").map(PathBuf::from),
            std::env::var_os("CASS_TEST_LIVE_ARTIFACT_DIR").map(PathBuf::from),
            &run_label,
        );

        Self {
            data_dir,
            db_path,
            artifact_root,
            query: std::env::var("CASS_TEST_LIVE_QUERY")
                .unwrap_or_else(|_| "authentication".to_string()),
            min_hits: env_usize("CASS_TEST_LIVE_MIN_HITS", 1).max(1),
            limit: env_usize("CASS_TEST_LIVE_LIMIT", 5).max(1),
            tier: std::env::var("CASS_TEST_LIVE_TIER").unwrap_or_else(|_| "fast".to_string()),
            embedder: std::env::var("CASS_TEST_LIVE_EMBEDDER")
                .unwrap_or_else(|_| "hash".to_string()),
            batch_conversations: env_usize("CASS_TEST_LIVE_BATCH_CONVERSATIONS", 64).max(1),
            max_backfill_runs: env_usize("CASS_TEST_LIVE_MAX_BACKFILL_RUNS", 3).max(1),
            timeout: Duration::from_secs(env_u64("CASS_TEST_LIVE_TIMEOUT_SECS", 300).max(30)),
            run_backfill: !env_truthy("CASS_TEST_LIVE_SKIP_BACKFILL"),
        }
    }

    fn manifest_json(&self) -> Value {
        json!({
            "data_dir": self.data_dir,
            "db_path": self.db_path,
            "artifact_root": self.artifact_root,
            "query": self.query,
            "min_hits": self.min_hits,
            "limit": self.limit,
            "tier": self.tier,
            "embedder": self.embedder,
            "batch_conversations": self.batch_conversations,
            "max_backfill_runs": self.max_backfill_runs,
            "timeout_secs": self.timeout.as_secs(),
            "run_backfill": self.run_backfill,
            "commands": [
                "cass health --json --data-dir <data_dir>",
                "cass status --json --data-dir <data_dir>",
                "cass models status --json --data-dir <data_dir>",
                "cass search <query> --json --robot-meta --limit <limit> --data-dir <data_dir>",
                "cass models backfill --tier <tier> --embedder <embedder> --batch-conversations <n> --json --data-dir <data_dir> --db <db_path>"
            ]
        })
    }
}

fn write_live_json_artifact(path: &Path, payload: &Value) -> TestResult {
    let body = serde_json::to_vec_pretty(payload)?;
    fs::write(path, body)?;
    Ok(())
}

fn run_live_robot_capture(
    config: &LiveBootstrapHarnessConfig,
    step_index: usize,
    label: &str,
    args: Vec<String>,
    allowed_exit_codes: &[i32],
) -> TestResult<LiveRobotArtifact> {
    let mut command = cargo_bin_cmd!("cass");
    command
        .args(args.iter().map(String::as_str))
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .timeout(config.timeout);

    let started_at_ms = now_ms();
    let output = command.output()?;
    let finished_at_ms = now_ms();
    let duration_ms = finished_at_ms.saturating_sub(started_at_ms);
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    let stdout_json = serde_json::from_str(stdout.trim()).ok();

    let artifact = LiveRobotArtifact {
        label: label.to_string(),
        args: args.clone(),
        exit_code,
        duration_ms,
        stdout,
        stderr,
        stdout_json,
    };

    write_live_json_artifact(
        &config
            .artifact_root
            .join(format!("{step_index:02}-{label}.json")),
        &json!({
            "label": artifact.label,
            "command": artifact.args,
            "exit_code": artifact.exit_code,
            "duration_ms": artifact.duration_ms,
            "stdout": artifact.stdout,
            "stderr": artifact.stderr,
            "stdout_json": artifact.stdout_json,
        }),
    )?;

    if !allowed_exit_codes.contains(&exit_code) {
        return Err(format!(
            "cass {} failed with exit code {exit_code}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            artifact.stdout,
            artifact.stderr
        )
        .into());
    }

    Ok(artifact)
}

fn assert_default_hybrid_contract(
    artifact: &LiveRobotArtifact,
    min_hits: usize,
) -> TestResult<Value> {
    let payload = artifact
        .stdout_json
        .as_ref()
        .ok_or("search output should be valid JSON")?;
    let meta = payload
        .get("_meta")
        .and_then(Value::as_object)
        .ok_or("search --robot-meta output should include _meta")?;
    let hits = payload
        .get("hits")
        .and_then(Value::as_array)
        .ok_or("search output should include hits array")?;

    if hits.len() < min_hits {
        return Err(format!(
            "live canonical query {:?} returned {} hits, expected at least {}; set CASS_TEST_LIVE_QUERY to a known-good term",
            artifact.args.get(1).cloned().unwrap_or_default(),
            hits.len(),
            min_hits
        )
        .into());
    }

    if meta.get("requested_search_mode").and_then(Value::as_str) != Some("hybrid") {
        return Err("default search intent should request hybrid mode".into());
    }
    if meta.get("mode_defaulted").and_then(Value::as_bool) != Some(true) {
        return Err("default search intent should report mode_defaulted=true".into());
    }

    match meta.get("search_mode").and_then(Value::as_str) {
        Some("hybrid") => {}
        Some("lexical") => {
            if meta.get("fallback_tier").and_then(Value::as_str) != Some("lexical") {
                return Err("lexical fail-open should surface fallback_tier=lexical".into());
            }
            if meta.get("semantic_refinement").and_then(Value::as_bool) != Some(false) {
                return Err("lexical fail-open should report semantic_refinement=false".into());
            }
        }
        Some(other) => {
            return Err(format!("unexpected realized search mode {other}").into());
        }
        None => return Err("search output missing realized search_mode".into()),
    }

    Ok(payload.clone())
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

#[test]
fn robot_models_backfill_scheduled_yields_to_foreground_pressure() -> TestResult {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("cass-data");
    let db_path = temp.path().join("agent_search.db");
    seed_canonical_db(&db_path)?;

    let paused = run_robot_scheduled_backfill_paused(&data_dir, &db_path)?;
    assert_eq!(paused["status"], "paused");
    assert_eq!(
        paused["next_step"],
        "foreground pressure is present; retry after the idle delay"
    );
    assert_eq!(paused["tier"], "fast");
    assert_eq!(paused["embedder_id"], "hash");
    assert_eq!(paused["batch_conversations_limit"], 8);
    assert_eq!(paused["scheduler"]["state"], "paused");
    assert_eq!(paused["scheduler"]["reason"], "foreground_pressure");
    assert_eq!(paused["scheduler"]["foreground_pressure"], true);
    assert_eq!(paused["scheduler"]["scheduled_batch_conversations"], 0);
    assert!(
        paused["scheduler"]["next_eligible_after_ms"]
            .as_u64()
            .is_some_and(|delay| delay > 0)
    );
    assert!(
        !SemanticManifest::path(&data_dir).exists(),
        "paused scheduled backfill should not touch semantic manifests"
    );

    Ok(())
}

#[test]
fn live_bootstrap_paths_default_under_standard_data_dir() {
    let data_dir = PathBuf::from("/tmp/cass-live");
    let (resolved_data_dir, resolved_db_path, artifact_root) =
        resolve_live_bootstrap_paths(Some(data_dir.clone()), None, None, "run-123");

    assert_eq!(resolved_data_dir, data_dir);
    assert_eq!(resolved_db_path, data_dir.join("agent_search.db"));
    assert_eq!(
        artifact_root,
        data_dir
            .join("test-artifacts")
            .join("ibuuh.11-live")
            .join("run-123")
    );
}

#[test]
#[ignore = "live canonical rollout harness; run explicitly with CASS_TEST_LIVE_CANONICAL_BOOTSTRAP=1"]
fn live_canonical_bootstrap_captures_repeatable_robot_artifacts() -> TestResult {
    if !env_truthy("CASS_TEST_LIVE_CANONICAL_BOOTSTRAP") {
        return Err(
            "set CASS_TEST_LIVE_CANONICAL_BOOTSTRAP=1 before running this ignored live rollout harness"
                .into(),
        );
    }

    let config = LiveBootstrapHarnessConfig::from_env();
    fs::create_dir_all(&config.artifact_root)?;
    write_live_json_artifact(
        &config.artifact_root.join("00-config.json"),
        &config.manifest_json(),
    )?;

    let before_health = run_live_robot_capture(
        &config,
        1,
        "health-before",
        vec![
            "health".to_string(),
            "--json".to_string(),
            "--data-dir".to_string(),
            config.data_dir.display().to_string(),
        ],
        &[0, 1],
    )?;
    let before_status = run_live_robot_capture(
        &config,
        2,
        "status-before",
        vec![
            "status".to_string(),
            "--json".to_string(),
            "--data-dir".to_string(),
            config.data_dir.display().to_string(),
        ],
        &[0],
    )?;
    let before_models = run_live_robot_capture(
        &config,
        3,
        "models-status-before",
        vec![
            "models".to_string(),
            "status".to_string(),
            "--json".to_string(),
            "--data-dir".to_string(),
            config.data_dir.display().to_string(),
        ],
        &[0],
    )?;
    let before_search = run_live_robot_capture(
        &config,
        4,
        "search-before",
        vec![
            "search".to_string(),
            config.query.clone(),
            "--json".to_string(),
            "--robot-meta".to_string(),
            "--limit".to_string(),
            config.limit.to_string(),
            "--data-dir".to_string(),
            config.data_dir.display().to_string(),
        ],
        &[0],
    )?;
    let before_search_payload = assert_default_hybrid_contract(&before_search, config.min_hits)?;

    let mut backfill_statuses = Vec::new();
    if config.run_backfill {
        for run in 0..config.max_backfill_runs {
            let label = format!("models-backfill-{:02}", run + 1);
            let artifact = run_live_robot_capture(
                &config,
                5 + run,
                &label,
                vec![
                    "models".to_string(),
                    "backfill".to_string(),
                    "--tier".to_string(),
                    config.tier.clone(),
                    "--embedder".to_string(),
                    config.embedder.clone(),
                    "--batch-conversations".to_string(),
                    config.batch_conversations.to_string(),
                    "--data-dir".to_string(),
                    config.data_dir.display().to_string(),
                    "--db".to_string(),
                    config.db_path.display().to_string(),
                    "--json".to_string(),
                ],
                &[0],
            )?;
            let payload = artifact
                .stdout_json
                .as_ref()
                .ok_or("models backfill output should be JSON")?;
            let status = payload
                .get("status")
                .and_then(Value::as_str)
                .ok_or("models backfill output missing status")?;
            backfill_statuses.push(status.to_string());
            if matches!(status, "published" | "ready") {
                break;
            }
        }
    }

    let after_models = run_live_robot_capture(
        &config,
        20,
        "models-status-after",
        vec![
            "models".to_string(),
            "status".to_string(),
            "--json".to_string(),
            "--data-dir".to_string(),
            config.data_dir.display().to_string(),
        ],
        &[0],
    )?;
    let after_search = run_live_robot_capture(
        &config,
        21,
        "search-after",
        vec![
            "search".to_string(),
            config.query.clone(),
            "--json".to_string(),
            "--robot-meta".to_string(),
            "--limit".to_string(),
            config.limit.to_string(),
            "--data-dir".to_string(),
            config.data_dir.display().to_string(),
        ],
        &[0],
    )?;
    let after_search_payload = assert_default_hybrid_contract(&after_search, config.min_hits)?;

    write_live_json_artifact(
        &config.artifact_root.join("summary.json"),
        &json!({
            "data_dir": config.data_dir,
            "db_path": config.db_path,
            "artifact_root": config.artifact_root,
            "before": {
                "health_exit_code": before_health.exit_code,
                "status_exit_code": before_status.exit_code,
                "models_status_exit_code": before_models.exit_code,
                "search_duration_ms": before_search.duration_ms,
                "search_mode_meta": before_search_payload.get("_meta"),
                "hits": before_search_payload.get("hits").and_then(Value::as_array).map(|hits| hits.len()),
            },
            "backfill_statuses": backfill_statuses,
            "after": {
                "models_status_exit_code": after_models.exit_code,
                "search_duration_ms": after_search.duration_ms,
                "search_mode_meta": after_search_payload.get("_meta"),
                "hits": after_search_payload.get("hits").and_then(Value::as_array).map(|hits| hits.len()),
            }
        }),
    )?;

    Ok(())
}
