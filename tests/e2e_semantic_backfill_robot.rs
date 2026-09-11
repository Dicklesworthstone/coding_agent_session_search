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
