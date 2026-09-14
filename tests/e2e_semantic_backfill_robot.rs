use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use assert_cmd::cargo::cargo_bin_cmd;
use coding_agent_search::default_data_dir;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::semantic_manifest::SemanticManifest;
use coding_agent_search::search::vector_index::{
    SemanticDocId, VectorIndex, parse_semantic_doc_id, vector_index_path,
};
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

fn seed_zero_doc_first_canonical_db(db_path: &Path) -> TestResult {
    let storage = FrankenStorage::open(db_path)?;
    let agent_id = storage.ensure_agent(&sample_agent())?;
    storage.insert_conversation_tree(agent_id, None, &sample_conversation("empty-first", ""))?;
    storage.insert_conversation_tree(
        agent_id,
        None,
        &sample_conversation("nonempty-second", "second robot semantic backfill message"),
    )?;
    Ok(())
}

fn seed_distinct_backfill_db(
    db_path: &Path,
    contents: [&str; 2],
) -> TestResult<Vec<SemanticDocId>> {
    let storage = FrankenStorage::open(db_path)?;
    let mut expected = Vec::new();
    for (ordinal, slug, source_id, role) in [
        (0, "codex", "local", MessageRole::User),
        (1, "claude_code", "work-laptop", MessageRole::Agent),
    ] {
        let mut agent = sample_agent();
        agent.slug = slug.into();
        agent.name = slug.into();
        let agent_id = storage.ensure_agent(&agent)?;
        let workspace = PathBuf::from(format!("/tmp/cass-lifetime/workspace-{ordinal}"));
        let workspace_id = storage.ensure_workspace(&workspace, None)?;
        let mut conversation = sample_conversation(
            &format!("lifetime-{ordinal}"),
            contents[usize::try_from(ordinal)?],
        );
        conversation.agent_slug = slug.into();
        conversation.workspace = Some(workspace);
        conversation.source_id = source_id.into();
        conversation.messages[0].role = role;
        conversation.messages[0].created_at = Some(1_700_000_000_500 + ordinal);
        let outcome =
            storage.insert_conversation_tree(agent_id, Some(workspace_id), &conversation)?;
        let messages = storage.fetch_messages(outcome.conversation_id)?;
        assert_eq!(messages.len(), 1);
        expected.push(SemanticDocId {
            message_id: messages[0]
                .id
                .ok_or("persisted message missing id")?
                .try_into()?,
            chunk_idx: 0,
            agent_id: agent_id.try_into()?,
            workspace_id: workspace_id.try_into()?,
            source_id: crc32fast::hash(source_id.as_bytes()),
            role: ordinal.try_into()?,
            created_at_ms: 1_700_000_000_500 + ordinal,
            content_hash: Some(coding_agent_search::search::canonicalize::content_hash(
                &conversation.messages[0].content,
            )),
        });
    }
    Ok(expected)
}

fn ordered_backfill_vectors(data_dir: &Path) -> TestResult<Vec<(SemanticDocId, Vec<u32>)>> {
    let index = VectorIndex::open(&vector_index_path(data_dir, "fnv1a-384"))?;
    assert_eq!(index.embedder_id(), "fnv1a-384");
    assert_eq!(index.dimension(), 384);
    assert_eq!(
        index.embedder_revision(),
        coding_agent_search::indexer::semantic::HASH_VECTOR_SPACE_REVISION
    );
    let mut records = Vec::new();
    for ordinal in 0..index.record_count() {
        assert!(!index.is_deleted(ordinal));
        let metadata = parse_semantic_doc_id(index.doc_id_at(ordinal)?)
            .ok_or("published vector has an invalid semantic identity")?;
        let vector = index.vector_at_f32(ordinal)?;
        assert_eq!(vector.len(), 384);
        assert!(vector.iter().all(|value| value.is_finite()));
        records.push((metadata, vector.into_iter().map(f32::to_bits).collect()));
    }
    Ok(records)
}

fn robot_backfill_process(data_dir: &Path, db_path: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command
        .arg("--db")
        .arg(db_path)
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
        .arg("--json")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_MIN_STACK", "134217728");
    command
}

fn robot_backfill_command(data_dir: &Path, db_path: &Path) -> assert_cmd::Command {
    let mut command = assert_cmd::Command::from_std(robot_backfill_process(data_dir, db_path));
    command.timeout(Duration::from_secs(20));
    command
}

fn run_robot_backfill(data_dir: &Path, db_path: &Path) -> TestResult<Value> {
    let output = robot_backfill_command(data_dir, db_path).output()?;

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

fn run_robot_backfill_batches(
    data_dir: &Path,
    db_path: &Path,
    max_batches: u32,
) -> TestResult<Value> {
    let output = robot_backfill_command(data_dir, db_path)
        .arg("--max-batches")
        .arg(max_batches.to_string())
        .output()?;
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn assert_backfill_busy(output: &std::process::Output) -> TestResult {
    assert_eq!(
        output.status.code(),
        Some(7),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "busy errors must not emit success data"
    );
    let envelope: Value = serde_json::from_slice(&output.stderr).map_err(|error| {
        format!(
            "invalid busy error JSON: {error}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    assert_eq!(envelope["error"]["code"], 7);
    assert_eq!(envelope["error"]["kind"], "index-busy");
    assert_eq!(envelope["error"]["retryable"], true);
    Ok(())
}

fn canonical_bundle_snapshot(db_path: &Path) -> TestResult<Vec<(PathBuf, Vec<u8>, SystemTime)>> {
    let mut files = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_os_string();
        name.push(suffix);
        let path = PathBuf::from(name);
        if path.is_file() {
            files.push((
                path.clone(),
                fs::read(&path)?,
                fs::metadata(&path)?.modified()?,
            ));
        }
    }
    Ok(files)
}

fn vector_files_snapshot(
    data_dir: &Path,
) -> TestResult<std::collections::BTreeMap<PathBuf, Vec<u8>>> {
    let root = data_dir.join("vector_index");
    let mut files = std::collections::BTreeMap::new();
    if root.exists() {
        for entry in walkdir::WalkDir::new(&root) {
            let entry = entry?;
            if entry.file_type().is_file() {
                files.insert(entry.path().to_path_buf(), fs::read(entry.path())?);
            }
        }
    }
    Ok(files)
}

#[test]
fn robot_models_backfill_respects_index_lock_before_canonical_or_vector_writes() -> TestResult {
    use fs2::FileExt;

    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("cass-data");
    let db_path = temp.path().join("agent_search.db");
    seed_canonical_db(&db_path)?;
    fs::create_dir_all(&data_dir)?;
    // Exercise both an empty vector store and a real populated resumable
    // checkpoint. The losing child must neither create nor mutate assets.
    for expected_after_release in ["checkpointed", "published"] {
        let canonical_before = canonical_bundle_snapshot(&db_path)?;
        let vectors_before = vector_files_snapshot(&data_dir)?;
        let vector_dir_existed = data_dir.join("vector_index").exists();
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data_dir.join("index-run.lock"))?;
        lock.lock_exclusive()?;
        let rejected = robot_backfill_command(&data_dir, &db_path).output()?;
        assert_backfill_busy(&rejected)?;
        assert_eq!(canonical_bundle_snapshot(&db_path)?, canonical_before);
        assert_eq!(vector_files_snapshot(&data_dir)?, vectors_before);
        assert_eq!(data_dir.join("vector_index").exists(), vector_dir_existed);
        FileExt::unlock(&lock)?;

        let resumed = run_robot_backfill(&data_dir, &db_path)?;
        assert_eq!(resumed["status"], expected_after_release);
        assert_eq!(resumed["embedded_docs"], 1);
    }
    let manifest = SemanticManifest::load(&data_dir)?.ok_or("missing published manifest")?;
    assert!(manifest.checkpoint.is_none());
    assert_eq!(
        manifest.fast_tier.as_ref().map(|tier| tier.doc_count),
        Some(2)
    );
    Ok(())
}

#[cfg(unix)]
struct BackfillChild(std::process::Child);

#[cfg(unix)]
impl Drop for BackfillChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn wait_for_backfill_owner_heartbeat(
    child: &mut std::process::Child,
    lock_path: &Path,
) -> TestResult {
    use std::time::Instant;

    let timestamp = |metadata: &str, key: &str| -> TestResult<i64> {
        Ok(metadata
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .ok_or("missing lock timestamp")?
            .parse()?)
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut initial = None;
    loop {
        let metadata = fs::read_to_string(lock_path).unwrap_or_default();
        if metadata.contains("job_kind=semantic_rebuild")
            && metadata
                .lines()
                .any(|line| line == format!("pid={}", child.id()))
        {
            let progress = timestamp(&metadata, "last_progress_at_ms=")?;
            let updated = timestamp(&metadata, "updated_at_ms=")?;
            if let Some((initial_progress, initial_updated)) = initial {
                assert_eq!(
                    progress, initial_progress,
                    "heartbeat fabricated backfill progress"
                );
                if updated > initial_updated {
                    return Ok(());
                }
            } else {
                initial = Some((progress, updated));
            }
        }
        assert!(
            child.try_wait()?.is_none(),
            "backfill exited before taking the lock"
        );
        assert!(
            Instant::now() < deadline,
            "backfill never produced an owner heartbeat"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn drain_progress_pipe(reader: &mut fs::File, progress: &mut Vec<u8>) -> std::io::Result<()> {
    use std::io::Read;

    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => progress.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
#[test]
fn robot_models_backfills_exclude_each_other_until_the_owner_finishes() -> TestResult {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    use std::process::Stdio;
    use std::time::Instant;

    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("cass-data");
    let db_path = temp.path().join("agent_search.db");
    seed_canonical_db(&db_path)?;
    let fifo = temp.path().join("backfill-progress.fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()?
            .success()
    );
    // Opening the real JSONL sink waits for its reader. This holds an actual
    // backfill at a deterministic boundary, without an artificial sleep hook.
    let mut command = robot_backfill_process(&data_dir, &db_path);
    command
        .env("CASS_SEMANTIC_PROGRESS_JSONL", &fifo)
        .env("CASS_INDEX_RUN_LOCK_HEARTBEAT_EVERY_MS", "20");
    let mut owner = BackfillChild(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let lock_path = data_dir.join("index-run.lock");
    wait_for_backfill_owner_heartbeat(&mut owner.0, &lock_path)?;
    let rejected = robot_backfill_command(&data_dir, &db_path).output()?;
    assert_backfill_busy(&rejected)?;
    assert!(
        owner.0.try_wait()?.is_none(),
        "the owner must still hold its lock"
    );

    // Release the actual sink and let this same owner checkpoint normally.
    // RDWR prevents the test-side open from waiting if the owner fails first.
    let mut reader = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)?;
    let mut progress = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        drain_progress_pipe(&mut reader, &mut progress)?;
        if let Some(status) = owner.0.try_wait()? {
            drain_progress_pipe(&mut reader, &mut progress)?;
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "backfill did not finish after sink release"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    owner
        .0
        .stdout
        .take()
        .ok_or("missing stdout")?
        .read_to_string(&mut stdout)?;
    owner
        .0
        .stderr
        .take()
        .ok_or("missing stderr")?
        .read_to_string(&mut stderr)?;
    assert!(status.success(), "stdout: {stdout}\nstderr: {stderr}");
    let first: Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(first["status"], "checkpointed");
    // The first event proves the owner reached the actual semantic pipeline.
    let progress = String::from_utf8(progress)?;
    let event: Value = serde_json::from_str(progress.lines().next().ok_or("no semantic events")?)?;
    assert_eq!(event["phase"], "selection");
    assert_eq!(
        run_robot_backfill(&data_dir, &db_path)?["status"],
        "published"
    );
    Ok(())
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
fn robot_models_backfill_keeps_archive_and_assets_scoped_to_path_overrides() -> TestResult {
    let temp = tempfile::tempdir()?;
    let ambient = temp.path().join("ambient");
    fs::create_dir_all(&ambient)?;
    let ambient_db = ambient.join("agent_search.db");
    {
        let storage = FrankenStorage::open(&ambient_db)?;
        let agent_id = storage.ensure_agent(&sample_agent())?;
        storage.insert_conversation_tree(
            agent_id,
            None,
            &sample_conversation("ambient", "this archive was not selected"),
        )?;
    }
    let ambient_before = fs::read(&ambient_db)?;

    for (name, explicit_data_dir, explicit_db) in [
        ("data-dir-only", true, false),
        ("db-only", false, true),
        ("split-layout", true, true),
    ] {
        let root = temp.path().join(name);
        let data_dir = root.join("assets");
        let db_path = if explicit_db {
            root.join("archive").join("selected.db")
        } else {
            data_dir.join("agent_search.db")
        };
        let db_parent = db_path.parent().ok_or("fixture DB needs a parent")?;
        fs::create_dir_all(db_parent)?;
        seed_canonical_db(&db_path)?;
        let expected_assets = if explicit_data_dir {
            data_dir.as_path()
        } else {
            db_parent
        };

        let mut command = cargo_bin_cmd!("cass");
        command.args([
            "models",
            "backfill",
            "--tier",
            "fast",
            "--embedder",
            "hash",
            "--json",
        ]);
        if explicit_data_dir {
            command.arg("--data-dir").arg(&data_dir);
        }
        if explicit_db {
            command.arg("--db").arg(&db_path);
        }
        let output = command
            .env("CASS_DATA_DIR", &ambient)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .timeout(Duration::from_secs(20))
            .output()?;
        assert!(
            output.status.success(),
            "{name}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["status"], "published", "{name}: {report}");
        assert_eq!(report["total_conversations"], 2, "{name}: {report}");
        assert_eq!(report["embedded_docs"], 2, "{name}: {report}");
        let manifest = SemanticManifest::load(expected_assets)?
            .ok_or("selected data directory should contain the published manifest")?;
        assert_eq!(
            manifest
                .fast_tier
                .as_ref()
                .map(|tier| (tier.ready, tier.doc_count)),
            Some((true, 2)),
            "{name}"
        );
        assert!(SemanticManifest::load(&ambient)?.is_none(), "{name}");
        assert_eq!(fs::read(&ambient_db)?, ambient_before, "{name}");
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh458_robot_unchanged_maintenance_skips_packet_replay_and_publication() -> TestResult {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("cass-data");
    let db_path = temp.path().join("agent_search.db");
    seed_canonical_db(&db_path)?;
    assert_eq!(
        run_robot_backfill(&data_dir, &db_path)?["status"],
        "checkpointed"
    );
    assert_eq!(
        run_robot_backfill(&data_dir, &db_path)?["status"],
        "published"
    );
    let before = vector_files_snapshot(&data_dir)?;
    let progress = temp.path().join("unchanged-progress.jsonl");
    let output = robot_backfill_command(&data_dir, &db_path)
        .env("CASS_SEMANTIC_PROGRESS_JSONL", &progress)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&output)?;
    assert_eq!(report["status"], "unchanged", "{report}");
    assert_eq!(report["unchanged"], true);
    assert_eq!(report["embedded_docs"], 0);
    assert_eq!(report["published"], true);
    assert_eq!(vector_files_snapshot(&data_dir)?, before);
    let events: Vec<Value> = fs::read_to_string(&progress)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        events.len(),
        1,
        "a no-op must not replay packets or republish: {events:?}"
    );
    assert_eq!(events[0]["event"], "complete");

    // A changed early row has the same conversation count and maximum IDs.
    // It must miss the cache and actually replace the old document identity.
    {
        let storage = FrankenStorage::open(&db_path)?;
        storage.raw().execute("UPDATE messages SET content = 'updated early semantic document' WHERE conversation_id = 1")?;
    }
    let changed = run_robot_backfill(&data_dir, &db_path)?;
    assert_eq!(changed["status"], "published");
    assert_eq!(changed["unchanged"], false);
    assert_eq!(changed["embedded_docs"], 1);
    assert_ne!(vector_files_snapshot(&data_dir)?, before);
    Ok(())
}

#[test]
fn gh467_robot_quality_backfill_resolves_native_aliases_before_loading_models() -> TestResult {
    use fs2::FileExt;

    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("cass-data");
    let db_path = temp.path().join("agent_search.db");
    seed_canonical_db(&db_path)?;
    let before = canonical_bundle_snapshot(&db_path)?;
    let multilingual = "paraphrase-multilingual-MiniLM-L12-v2";

    let run = |name: &str| {
        cargo_bin_cmd!("cass")
            .current_dir(temp.path())
            .arg("--db")
            .arg(&db_path)
            .args([
                "models",
                "backfill",
                "--tier",
                "quality",
                "--embedder",
                name,
                "--batch-conversations",
                "1",
                "--data-dir",
            ])
            .arg(&data_dir)
            .arg("--json")
            .env("HOME", temp.path())
            .env("XDG_CONFIG_HOME", temp.path().join("config"))
            .env("CASS_SEMANTIC_EMBEDDER", "minilm")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env_remove("FRANKENSEARCH_MODEL_DIR")
            .timeout(Duration::from_secs(20))
            .output()
    };

    fs::create_dir_all(&data_dir)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data_dir.join("index-run.lock"))?;
    lock.lock_exclusive()?;
    assert_backfill_busy(&run("minilm")?)?;
    assert!(
        canonical_bundle_snapshot(&db_path)? == before,
        "busy-index refusal must preserve database bundle bytes and timestamps"
    );
    FileExt::unlock(&lock)?;

    for (name, model_directory) in [
        ("minilm", "all-MiniLM-L6-v2"),
        ("fastembed", "all-MiniLM-L6-v2"),
        ("multilingual-minilm", multilingual),
        ("multilingual-minilm-384", multilingual),
        ("paraphrase-multilingual-minilm-l12-v2", multilingual),
    ] {
        let output = run(name)?;
        assert_eq!(output.status.code(), Some(20), "{name}: {output:?}");
        assert!(output.stdout.is_empty(), "{name}: {output:?}");
        let report: Value = serde_json::from_slice(&output.stderr)?;
        assert_eq!(report["error"]["kind"], "model", "{name}: {report}");
        assert_eq!(report["error"]["retryable"], true, "{name}: {report}");
        let message = report["error"]["message"].as_str().unwrap_or_default();
        let selected_directory = data_dir.join("models").join(model_directory);
        assert!(
            message.contains("model directory not found"),
            "{name}: {report}"
        );
        assert!(
            message.contains(selected_directory.to_string_lossy().as_ref()),
            "{name}: {report}"
        );
        assert!(!message.contains("unknown embedder"), "{name}: {report}");
        assert!(
            !data_dir.join("models").exists(),
            "{name}: no automatic acquisition"
        );
        assert!(SemanticManifest::load(&data_dir)?.is_none(), "{name}");
        assert!(
            canonical_bundle_snapshot(&db_path)? == before,
            "{name}: unavailable model must preserve database bundle bytes and timestamps"
        );
    }

    Ok(())
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
fn gh471_robot_backfill_retains_one_model_and_matches_restarted_batches() -> TestResult {
    let temp = tempfile::tempdir()?;
    let mut published = Vec::new();
    for (name, retained) in [("single-process", true), ("restarted", false)] {
        let root = temp.path().join(name);
        fs::create_dir_all(&root)?;
        let data_dir = root.join("data");
        let db_path = root.join("archive.db");
        let expected = seed_distinct_backfill_db(
            &db_path,
            ["duplicate canonical evidence retains distinct source provenance"; 2],
        )?;
        let report = if retained {
            let report = run_robot_backfill_batches(&data_dir, &db_path, 2)?;
            assert_eq!(report["batches_attempted"], 2, "{report}");
            assert_eq!(report["batches_completed"], 2, "{report}");
            assert_eq!(report["model_initializations"], 1, "{report}");
            report
        } else {
            let first = run_robot_backfill_batches(&data_dir, &db_path, 1)?;
            assert_eq!(first["status"], "checkpointed", "{first}");
            assert_eq!(first["batches_attempted"], 1);
            assert_eq!(first["batches_completed"], 1);
            assert_eq!(first["model_initializations"], 1);
            let manifest = SemanticManifest::load(&data_dir)?.ok_or("missing checkpoint")?;
            let checkpoint = manifest.checkpoint.ok_or("first batch must be durable")?;
            assert_eq!(checkpoint.conversations_processed, 1);
            assert_eq!(checkpoint.docs_embedded, 1);
            let second = run_robot_backfill(&data_dir, &db_path)?;
            assert_eq!(second["batches_attempted"], 1);
            assert_eq!(second["batches_completed"], 1);
            assert_eq!(second["model_initializations"], 1);
            second
        };
        assert_eq!(report["status"], "published", "{report}");
        assert_eq!(report["conversations_processed"], 2);
        assert_eq!(
            report["embedded_docs"], 1,
            "last batch fields stay top-level"
        );
        let vectors = ordered_backfill_vectors(&data_dir)?;
        let mut published_metadata: Vec<_> = vectors.iter().map(|(id, _)| *id).collect();
        published_metadata.sort_by_key(|id| id.message_id);
        assert_eq!(published_metadata, expected);
        assert_eq!(vectors.len(), 2);
        assert_eq!(
            vectors[0].1, vectors[1].1,
            "equal content keeps both source rows"
        );
        let manifest = SemanticManifest::load(&data_dir)?.ok_or("missing publication")?;
        assert!(manifest.checkpoint.is_none());
        let tier = manifest.fast_tier.ok_or("missing fast tier")?;
        assert!(tier.ready);
        assert_eq!((tier.doc_count, tier.conversation_count), (2, 2));
        published.push(vectors);

        let before = vector_files_snapshot(&data_dir)?;
        let unchanged = run_robot_backfill_batches(&data_dir, &db_path, 4)?;
        assert_eq!(unchanged["status"], "unchanged", "{unchanged}");
        assert_eq!(unchanged["batches_attempted"], 1);
        assert_eq!(unchanged["batches_completed"], 1);
        assert_eq!(unchanged["model_initializations"], 1);
        assert_eq!(vector_files_snapshot(&data_dir)?, before);
    }
    assert_eq!(
        published[0], published[1],
        "complete ordered vector and metadata parity"
    );
    Ok(())
}

#[test]
fn gh471_robot_backfill_zero_doc_batch_continues_with_retained_model() -> TestResult {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("data");
    let db_path = temp.path().join("archive.db");
    seed_zero_doc_first_canonical_db(&db_path)?;
    let report = run_robot_backfill_batches(&data_dir, &db_path, 2)?;
    assert_eq!(report["status"], "published", "{report}");
    assert_eq!(report["batches_attempted"], 2);
    assert_eq!(report["batches_completed"], 2);
    assert_eq!(report["model_initializations"], 1);
    assert_eq!(report["conversations_processed"], 2);
    assert_eq!(report["embedded_docs"], 1);
    let vectors = ordered_backfill_vectors(&data_dir)?;
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].0.message_id, 2);
    let manifest = SemanticManifest::load(&data_dir)?.ok_or("missing publication")?;
    assert!(manifest.checkpoint.is_none());
    let tier = manifest.fast_tier.ok_or("missing fast tier")?;
    assert_eq!((tier.doc_count, tier.conversation_count), (1, 2));
    Ok(())
}

mod gh471_native_worker {
    use super::*;
    use coding_agent_search::search::fastembed_embedder::{
        FastEmbedder, MINILM_VECTOR_SPACE_REVISION,
    };
    use coding_agent_search::search::model_download::{ModelManifest, model_file_path};

    const LOADED: &str = "native frankentorch MiniLM embedder loaded (mean-pool + L2)";

    fn copy_model(data_dir: &Path) -> TestResult {
        let supplied =
            PathBuf::from(dotenvy::var("CASS_NATIVE_REUSE_MODEL_DIR")?).canonicalize()?;
        let destination = FastEmbedder::default_model_dir(data_dir);
        fs::create_dir_all(&destination)?;
        let manifest = ModelManifest::minilm_v2();
        assert_eq!(manifest.files.len(), 5);
        for file in &manifest.files {
            let source = model_file_path(&supplied, file)
                .ok_or_else(|| format!("missing supplied model file: {}", file.name))?;
            assert_eq!(
                fs::copy(source, destination.join(file.local_name()))?,
                file.size
            );
        }
        // The actual child loader must verify hashes and executing kernels.
        // This copy never invokes model acquisition or alters the supplied bundle.
        Ok(())
    }

    fn child(home: &Path) -> TestResult<std::process::Command> {
        fs::create_dir_all(home)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(home.join(".env"))?;
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command.env_clear().current_dir(home);
        for key in ["PATH", "SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("TUI_HEADLESS", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_RESPONSIVENESS_DISABLE", "1")
            .env("RUST_MIN_STACK", "134217728")
            .env(
                "CASS_TRACE_FILTER",
                "warn,frankensearch_rerank::native_embedder=info",
            );
        Ok(command)
    }

    fn jsonl(path: &Path) -> TestResult<Vec<Value>> {
        let text = fs::read_to_string(path)?;
        let mut rows = Vec::new();
        for (ordinal, line) in text.lines().enumerate() {
            let row: Value = serde_json::from_str(line).map_err(|error| {
                format!("{}:{}: {error}; record={line}", path.display(), ordinal + 1)
            })?;
            assert_ne!(row["event"], "trace_truncated", "{row}");
            assert_ne!(row["fields"]["event"], "trace_truncated", "{row}");
            rows.push(row);
        }
        assert!(
            !rows.is_empty(),
            "empty actual artifact: {}",
            path.display()
        );
        Ok(rows)
    }

    fn run(
        root: &Path,
        data_dir: &Path,
        db_path: &Path,
        name: &str,
        batches: u32,
    ) -> TestResult<(Value, Value, Vec<Value>)> {
        let trace = root.join(format!("{name}-trace.jsonl"));
        let progress = root.join(format!("{name}-progress.jsonl"));
        let mut command = child(&root.join(format!("{name}-home")))?;
        command
            .arg("--db")
            .arg(db_path)
            .args([
                "models",
                "backfill",
                "--tier",
                "quality",
                "--embedder",
                "minilm",
            ])
            .args(["--batch-conversations", "1", "--max-batches"])
            .arg(batches.to_string())
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--json")
            .env("CASS_DATA_DIR", data_dir)
            .env("CASS_TRACE_FILE", &trace)
            .env("CASS_TRACE_TEST_ID", name)
            .env("CASS_SEMANTIC_PROGRESS_JSONL", &progress);
        let output = assert_cmd::Command::from_std(command)
            .timeout(Duration::from_secs(1200))
            .output()?;
        assert!(
            output.status.success(),
            "{name}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            format!(
                "{name}: {error}; stdout={}; stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })?;
        assert_eq!(report["batches_attempted"], batches, "{report}");
        assert_eq!(report["batches_completed"], batches, "{report}");
        assert_eq!(report["model_initializations"], 1, "{report}");
        assert_eq!(report["embedder_id"], "minilm-384", "{report}");
        let traces = jsonl(&trace)?;
        let constructors: Vec<_> = traces
            .iter()
            .filter(|row| {
                row["target"] == "frankensearch_rerank::native_embedder"
                    && row["fields"]["message"] == LOADED
            })
            .collect();
        assert_eq!(constructors.len(), 1, "actual constructors: {traces:?}");
        let constructor = (*constructors[0]).clone();
        assert_eq!(constructor["fields"]["dimension"], 384, "{constructor}");
        for field in ["manifest", "identity"] {
            assert!(
                constructor["fields"][field]
                    .as_str()
                    .is_some_and(|value| !value.is_empty() && value != "[REDACTED]"),
                "{constructor}"
            );
        }
        let events = jsonl(&progress)?;
        assert!(
            events
                .iter()
                .all(|row| row["schema"] == "cass.semantic.progress.v1")
        );
        assert!(events.iter().all(|row| row["tier"] == "quality"));
        assert!(events.iter().all(|row| row["embedder_id"] == "minilm-384"));
        let durable: Vec<_> = events
            .iter()
            .filter(|row| {
                matches!(
                    row["event"].as_str(),
                    Some("checkpoint_save_done" | "publish_done")
                )
            })
            .cloned()
            .collect();
        assert_eq!(durable.len(), usize::try_from(batches)?);
        eprintln!(
            "gh471_native_worker_evidence={}",
            json!({
                "name": name, "report": report, "constructor": constructor,
                "durable_boundaries": durable,
                "stderr": String::from_utf8_lossy(&output.stderr)
            })
        );
        Ok((report, constructor, durable))
    }

    fn vectors(data_dir: &Path) -> TestResult<Vec<(SemanticDocId, Vec<u32>)>> {
        let index = VectorIndex::open(&vector_index_path(data_dir, "minilm-384"))?;
        assert_eq!(index.embedder_id(), "minilm-384");
        assert_eq!(index.dimension(), 384);
        assert_eq!(index.embedder_revision(), MINILM_VECTOR_SPACE_REVISION);
        let mut records = Vec::new();
        for ordinal in 0..index.record_count() {
            assert!(!index.is_deleted(ordinal));
            let metadata = parse_semantic_doc_id(index.doc_id_at(ordinal)?)
                .ok_or("invalid native semantic identity")?;
            let vector = index.vector_at_f32(ordinal)?;
            assert_eq!(vector.len(), 384);
            assert!(vector.iter().all(|value| value.is_finite()));
            records.push((metadata, vector.into_iter().map(f32::to_bits).collect()));
        }
        Ok(records)
    }

    #[test]
    #[ignore = "requires CASS_NATIVE_REUSE_MODEL_DIR with an attested five-file MiniLM bundle; never downloads"]
    fn gh471_quality_worker_one_constructor_two_durable_batches_matches_restarts() -> TestResult {
        let temp = tempfile::tempdir()?;
        let mut outputs = Vec::new();
        let mut producer_identities = Vec::new();
        let mut all_boundaries = Vec::new();
        for (name, retained) in [("retained", true), ("restarted", false)] {
            let root = temp.path().join(name);
            fs::create_dir_all(&root)?;
            let data_dir = root.join("data");
            let db_path = root.join("archive.db");
            let expected = seed_distinct_backfill_db(
                &db_path,
                [
                    "The database transaction recovered from a durable checkpoint.",
                    "Unicode source evidence preserves café and 東京 in another workspace.",
                ],
            )?;
            copy_model(&data_dir)?;
            let mut boundaries = Vec::new();
            let batch_limits: &[u32] = if retained { &[2] } else { &[1, 1] };
            for (ordinal, &limit) in batch_limits.iter().enumerate() {
                let (report, constructor, events) = run(
                    &root,
                    &data_dir,
                    &db_path,
                    &format!("worker-{ordinal}"),
                    limit,
                )?;
                producer_identities.push(constructor["fields"]["identity"].clone());
                boundaries.extend(events);
                let manifest = SemanticManifest::load(&data_dir)?
                    .ok_or("native worker omitted durable manifest")?;
                if !retained && ordinal == 0 {
                    assert_eq!(report["status"], "checkpointed", "{report}");
                    let checkpoint = manifest.checkpoint.ok_or("missing first checkpoint")?;
                    assert_eq!(
                        (checkpoint.conversations_processed, checkpoint.docs_embedded),
                        (1, 1)
                    );
                    assert_eq!(
                        checkpoint.last_message_id,
                        Some(expected[0].message_id.try_into()?)
                    );
                    assert!(manifest.quality_tier.is_none());
                } else {
                    assert_eq!(report["status"], "published", "{report}");
                    assert_eq!(report["conversations_processed"], 2, "{report}");
                    assert!(manifest.checkpoint.is_none());
                    let tier = manifest.quality_tier.ok_or("missing quality publication")?;
                    assert!(tier.ready);
                    assert_eq!(tier.embedder_id, "minilm-384");
                    assert_eq!((tier.doc_count, tier.conversation_count), (2, 2));
                }
            }
            assert_eq!(boundaries.len(), 2);
            assert_eq!(boundaries[0]["event"], "checkpoint_save_done");
            assert_eq!(boundaries[1]["event"], "publish_done");
            for (ordinal, event) in boundaries.iter().enumerate() {
                assert_eq!(event["rows_processed"], ordinal + 1, "{event}");
                assert_eq!(event["rows_total"], 2, "{event}");
                assert_eq!(
                    event["last_message_id"], expected[ordinal].message_id,
                    "{event}"
                );
            }
            all_boundaries.push(
                boundaries
                    .iter()
                    .map(|event| {
                        json!({
                            "event": event["event"],
                            "rows_processed": event["rows_processed"],
                            "last_conversation_id": event["last_conversation_id"],
                            "last_message_id": event["last_message_id"]
                        })
                    })
                    .collect::<Vec<_>>(),
            );
            let records = vectors(&data_dir)?;
            // FSVI orders by document hash. Check source membership separately
            // and retain physical order for the final worker/restart comparison.
            let mut published_metadata: Vec<_> = records.iter().map(|(id, _)| *id).collect();
            published_metadata.sort_by_key(|id| id.message_id);
            assert_eq!(published_metadata, expected);
            assert_eq!(records.len(), 2);
            assert_ne!(
                records[0].1, records[1].1,
                "different source content must remain distinct"
            );
            outputs.push(records);
        }
        assert_eq!(
            producer_identities.len(),
            3,
            "one retained plus two restarted loads"
        );
        assert!(
            producer_identities
                .iter()
                .all(|identity| identity == &producer_identities[0])
        );
        assert_eq!(
            all_boundaries[0], all_boundaries[1],
            "same durable source cursor sequence"
        );
        assert_eq!(
            outputs[0], outputs[1],
            "full ordered metadata and f32-bit equality"
        );
        Ok(())
    }
}

#[test]
fn gh471_robot_backfill_rejects_invalid_batch_limits_without_mutation() -> TestResult {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("data");
    let db_path = temp.path().join("archive.db");
    seed_canonical_db(&db_path)?;
    let before = canonical_bundle_snapshot(&db_path)?;
    for invalid in ["0", "-1", "4294967296", "not-a-number"] {
        let output = robot_backfill_command(&data_dir, &db_path)
            .arg(format!("--max-batches={invalid}"))
            .output()?;
        assert_eq!(
            output.status.code(),
            Some(2),
            "{invalid}: stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.is_empty(),
            "invalid limits cannot report success"
        );
        assert_eq!(canonical_bundle_snapshot(&db_path)?, before);
        assert!(!SemanticManifest::path(&data_dir).exists());
        assert!(!data_dir.join("vector_index").exists());
    }
    Ok(())
}

#[test]
fn gh471_robot_backfill_later_storage_failure_preserves_checkpoint_and_retries() -> TestResult {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("data");
    let db_path = temp.path().join("archive.db");
    seed_canonical_db(&db_path)?;
    {
        let storage = FrankenStorage::open(&db_path)?;
        // SQLite accepts this dynamic type, but the real canonical Message
        // hydration requires Option<i64>. Only the second selected batch fails.
        storage.raw().execute(
            "UPDATE messages SET created_at = 'invalid-timestamp' WHERE conversation_id = 2",
        )?;
    }
    let output = robot_backfill_command(&data_dir, &db_path)
        .args(["--max-batches", "2"])
        .output()?;
    assert_eq!(
        output.status.code(),
        Some(5),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let failed: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(failed["error"]["code"], 5);
    assert_eq!(failed["error"]["kind"], "semantic-backfill");
    assert_eq!(failed["batches_attempted"], 2);
    assert_eq!(failed["batches_completed"], 1);
    assert_eq!(failed["model_initializations"], 1);
    let prior = &failed["last_completed_batch"];
    assert_eq!(prior["status"], "checkpointed");
    assert_eq!(prior["checkpoint_saved"], true);
    assert_eq!(prior["conversations_processed"], 1);
    let manifest = SemanticManifest::load(&data_dir)?.ok_or("lost first checkpoint")?;
    let checkpoint = manifest.checkpoint.ok_or("lost durable checkpoint")?;
    assert_eq!(
        (checkpoint.docs_embedded, checkpoint.conversations_processed),
        (1, 1)
    );
    assert!(!vector_index_path(&data_dir, "fnv1a-384").is_file());
    {
        let storage = FrankenStorage::open(&db_path)?;
        storage
            .raw()
            .execute("UPDATE messages SET created_at = 1700000000500 WHERE conversation_id = 2")?;
    }
    let resumed = run_robot_backfill_batches(&data_dir, &db_path, 2)?;
    assert_eq!(resumed["status"], "published", "{resumed}");
    assert_eq!(resumed["batches_attempted"], 1);
    assert_eq!(resumed["batches_completed"], 1);
    assert_eq!(resumed["model_initializations"], 1);
    assert_eq!(
        resumed["embedded_docs"], 1,
        "already durable source must be reused"
    );
    let control = temp.path().join("control");
    fs::create_dir_all(&control)?;
    let control_db = control.join("archive.db");
    let control_data = control.join("data");
    seed_canonical_db(&control_db)?;
    assert_eq!(
        run_robot_backfill(&control_data, &control_db)?["status"],
        "checkpointed"
    );
    assert_eq!(
        run_robot_backfill(&control_data, &control_db)?["status"],
        "published"
    );
    assert_eq!(
        ordered_backfill_vectors(&data_dir)?,
        ordered_backfill_vectors(&control_data)?
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn gh471_robot_backfill_cancel_finishes_current_checkpoint_and_resumes() -> TestResult {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    use std::process::Stdio;
    use std::time::Instant;

    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("data");
    let db_path = temp.path().join("archive.db");
    seed_canonical_db(&db_path)?;
    let fifo = temp.path().join("cancel-progress.fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()?
            .success()
    );
    let mut command = robot_backfill_process(&data_dir, &db_path);
    command
        .args(["--max-batches", "2"])
        .env("CASS_SEMANTIC_PROGRESS_JSONL", &fifo)
        .env("CASS_INDEX_RUN_LOCK_HEARTBEAT_EVERY_MS", "20");
    let mut owner = BackfillChild(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    wait_for_backfill_owner_heartbeat(&mut owner.0, &data_dir.join("index-run.lock"))?;
    // The real sink's open blocks inside the first admitted batch. Request
    // cancellation there, then release it: this batch must checkpoint, and
    // the retained worker must stop before admitting the second batch.
    assert!(
        std::process::Command::new("kill")
            .arg("-INT")
            .arg(owner.0.id().to_string())
            .status()?
            .success()
    );
    let mut reader = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)?;
    let mut progress = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        drain_progress_pipe(&mut reader, &mut progress)?;
        if let Some(status) = owner.0.try_wait()? {
            drain_progress_pipe(&mut reader, &mut progress)?;
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "cancelled backfill did not stop after its durable batch"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    owner
        .0
        .stdout
        .take()
        .ok_or("missing stdout")?
        .read_to_string(&mut stdout)?;
    owner
        .0
        .stderr
        .take()
        .ok_or("missing stderr")?
        .read_to_string(&mut stderr)?;
    assert_eq!(
        status.code(),
        Some(130),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let cancelled: Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
    assert_eq!(cancelled["error"]["code"], 130);
    assert_eq!(cancelled["error"]["kind"], "semantic-backfill");
    assert_eq!(cancelled["error"]["retryable"], true);
    assert_eq!(cancelled["batches_attempted"], 1);
    assert_eq!(cancelled["batches_completed"], 1);
    assert_eq!(cancelled["model_initializations"], 1);
    let prior = &cancelled["last_completed_batch"];
    assert_eq!(prior["status"], "checkpointed");
    assert_eq!(prior["checkpoint_saved"], true);
    assert_eq!(prior["conversations_processed"], 1);
    let manifest = SemanticManifest::load(&data_dir)?.ok_or("cancel lost manifest")?;
    let checkpoint = manifest
        .checkpoint
        .ok_or("cancel lost durable checkpoint")?;
    assert_eq!(checkpoint.conversations_processed, 1);
    assert_eq!(checkpoint.docs_embedded, 1);
    assert!(!vector_index_path(&data_dir, "fnv1a-384").is_file());
    let progress = String::from_utf8(progress)?;
    let events: Vec<Value> = progress
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "checkpoint_save_done")
            .count(),
        1,
        "the cancelled process must save exactly its admitted batch: {events:?}"
    );
    assert!(events.iter().all(|event| event["event"] != "publish_done"));
    let resumed = run_robot_backfill_batches(&data_dir, &db_path, 2)?;
    assert_eq!(resumed["status"], "published", "{resumed}");
    assert_eq!(resumed["batches_attempted"], 1);
    assert_eq!(resumed["batches_completed"], 1);
    assert_eq!(resumed["model_initializations"], 1);
    assert_eq!(resumed["embedded_docs"], 1);
    let records = ordered_backfill_vectors(&data_dir)?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].0.message_id, 1);
    assert_eq!(records[1].0.message_id, 2);
    assert!(
        SemanticManifest::load(&data_dir)?
            .ok_or("resume lost manifest")?
            .checkpoint
            .is_none()
    );
    Ok(())
}

#[test]
fn robot_models_backfill_zero_doc_batch_still_reports_checkpointed() -> TestResult {
    let temp = tempfile::tempdir()?;
    let data_dir = temp.path().join("cass-data");
    let db_path = temp.path().join("agent_search.db");
    seed_zero_doc_first_canonical_db(&db_path)?;

    let first = run_robot_backfill(&data_dir, &db_path)?;
    assert_eq!(first["status"], "checkpointed");
    assert_eq!(
        first["next_step"],
        "rerun the same command to continue the resumable backfill"
    );
    assert_eq!(first["embedded_docs"], 0);
    assert_eq!(first["conversations_processed"], 1);
    assert_eq!(first["total_conversations"], 2);
    assert_eq!(first["checkpoint_saved"], true);
    assert_eq!(first["published"], false);
    let progress_pct = first
        .get("progress_pct")
        .and_then(Value::as_f64)
        .ok_or("progress_pct should be numeric")?;
    assert!((progress_pct - 50.0).abs() < f64::EPSILON);

    let manifest = SemanticManifest::load(&data_dir)?.ok_or("semantic manifest should exist")?;
    let checkpoint = manifest.checkpoint.ok_or("checkpoint should remain")?;
    assert_eq!(checkpoint.docs_embedded, 0);
    assert_eq!(checkpoint.conversations_processed, 1);
    assert!(!checkpoint.cursor_exhausted);

    let second = run_robot_backfill(&data_dir, &db_path)?;
    assert_eq!(second["status"], "published");
    assert_eq!(second["next_step"], "semantic tier is ready");
    assert_eq!(second["embedded_docs"], 1);
    assert_eq!(second["conversations_processed"], 2);
    assert_eq!(second["total_conversations"], 2);
    assert_eq!(second["checkpoint_saved"], false);
    assert_eq!(second["published"], true);

    let manifest = SemanticManifest::load(&data_dir)?.ok_or("semantic manifest should exist")?;
    assert!(manifest.checkpoint.is_none());
    assert_eq!(
        manifest.fast_tier.as_ref().map(|artifact| (
            artifact.ready,
            artifact.conversation_count,
            artifact.doc_count
        )),
        Some((true, 2, 1))
    );

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
    assert_eq!(paused["batches_attempted"], 1);
    assert_eq!(paused["batches_completed"], 0);
    assert_eq!(paused["model_initializations"], 0);
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
