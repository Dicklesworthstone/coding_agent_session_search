//! Exercise the shipped CLI against real canonical storage, not injected input rows.
use super::*;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::quill_bridge::QuillCassIndex;
use coding_agent_search::storage::sqlite::FrankenStorage;
use frankensearch::quill::cass::CassDocument;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

fn seed(dir: &Path) -> Result<PathBuf> {
    let db = dir.join("agent_search.db");
    let storage = FrankenStorage::open(&db)?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let mut lexical =
        QuillCassIndex::open_or_create(&coding_agent_search::search::tantivy::index_dir(dir)?)?;
    for (ordinal, count) in [(1, 1), (2, 3), (3, 1)] {
        let source_path = PathBuf::from(format!("/fixture/external-{ordinal}.jsonl"));
        let messages: Vec<_> = (0..count)
            .map(|idx| Message {
                id: None,
                idx,
                role: MessageRole::User,
                author: None,
                created_at: Some(1_700_000_000_500),
                content: format!(
                    "private corpus conversation {ordinal} message {idx} compiler evidence"
                ),
                extra_json: json!({}),
                snippets: Vec::new(),
            })
            .collect();
        let conversation = Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some(format!("external-{ordinal}")),
            title: Some(format!("external fixture {ordinal}")),
            source_path: source_path.clone(),
            started_at: Some(1_700_000_000_000),
            ended_at: Some(1_700_000_001_000),
            approx_tokens: None,
            metadata_json: json!({}),
            messages,
            source_id: "local".into(),
            origin_host: None,
        };
        let inserted = storage.insert_conversation_tree(agent, None, &conversation)?;
        let documents: Vec<_> = conversation
            .messages
            .iter()
            .map(|message| CassDocument {
                agent: "codex".into(),
                workspace: None,
                workspace_original: None,
                source_path: source_path.display().to_string(),
                msg_idx: message.idx as u64,
                created_at: message.created_at,
                title: conversation.title.clone(),
                content: message.content.clone(),
                source_id: "local".into(),
                origin_kind: "local".into(),
                origin_host: None,
                conversation_id: Some(inserted.conversation_id),
            })
            .collect();
        lexical.add_cass_documents(&documents)?;
    }
    lexical.commit()?;
    Ok(db)
}

fn process(server: &Server, dir: &Path, db: &Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    // Never let inherited provider, model-directory, governor or test hooks
    // turn this loopback fixture into a request against a real service.
    for (name, _) in std::env::vars_os() {
        let key = name.to_string_lossy();
        if key.starts_with("CASS_") || key.starts_with("FRANKENSEARCH_") {
            command.env_remove(name);
        }
    }
    command
        .current_dir(dir)
        .args(["--color=never", "--db"])
        .arg(db)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("CASS_DATA_DIR", dir)
        .env("CASS_AUTO_REFRESH", "0")
        .env("CASS_INDEX_NO_PROGRESS_EVENTS", "1")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_MIN_STACK", "134217728")
        .env("CASS_EXTERNAL_EMBEDDINGS", "1")
        .env("CASS_EXTERNAL_EMBEDDING_URL", &server.url)
        .env("CASS_EXTERNAL_EMBEDDING_MODEL", "fixture-model")
        .env("CASS_EXTERNAL_EMBEDDING_DIMENSION", "384")
        .env("CASS_EXTERNAL_EMBEDDING_REVISION", "v1")
        .env("CASS_EXTERNAL_EMBEDDING_BATCH_SIZE", "2")
        .env("CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS", "2000");
    command
}

fn backfill(server: &Server, dir: &Path, db: &Path, batches: u32) -> Command {
    let mut command = process(server, dir, db);
    command
        .args([
            "models",
            "backfill",
            "--tier",
            "quality",
            "--batch-conversations",
            "1",
            "--json",
            "--max-batches",
        ])
        .arg(batches.to_string())
        .arg("--data-dir")
        .arg(dir);
    command
}

fn run(command: Command) -> Result<Output> {
    Ok(assert_cmd::Command::from_std(command)
        .timeout(Duration::from_secs(45))
        .output()?)
}

fn success(output: Output) -> Result<Value> {
    anyhow::ensure!(
        output.status.success(),
        "CLI failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn corpus(server: &Server) -> Vec<String> {
    server
        .take_inputs()
        .into_iter()
        .flatten()
        .filter(|text| text.starts_with("private corpus"))
        .collect()
}

fn signature(path: &Path) -> Result<BTreeMap<String, Vec<u32>>> {
    let index = VectorIndex::open_read_only(path)?;
    (0..index.record_count())
        .map(|row| {
            Ok((
                index.doc_id_at(row)?.to_owned(),
                index
                    .vector_at_f32(row)?
                    .into_iter()
                    .map(f32::to_bits)
                    .collect(),
            ))
        })
        .collect()
}

#[test]
fn external_cli_policy_builds_without_minilm_and_retains_one_provider() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = seed(dir.path())?;
    let server = Server::start(384)?;
    let mut command = backfill(&server, dir.path(), &db, 10);
    command.env("CASS_SEMANTIC_EMBEDDER", "external");
    let report = success(run(command)?)?;
    assert_eq!(report["published"], true);
    assert_eq!(report["batches_completed"], 3);
    assert_eq!(report["model_initializations"], 1);
    let manifest = SemanticManifest::load(dir.path())?.context("published manifest")?;
    let artifact = manifest.quality_tier.context("quality artifact")?;
    assert_eq!(
        artifact.embedder_id,
        server.config("fixture-model", 384, "v1")?.identity()
    );
    assert_eq!(artifact.model_revision, artifact.embedder_id);
    assert_eq!(artifact.doc_count, 5);
    assert!(manifest.fast_tier.is_none());
    assert!(!dir.path().join("models").exists());
    assert_eq!(corpus(&server).len(), 5);
    Ok(())
}

#[test]
fn external_cli_canonical_resume_rejects_partial_batches_and_preserves_durable_vectors()
-> Result<()> {
    for (fault, retries) in [
        (PARTIAL, 0),
        (SERVER_FAILURE, 0),
        (BAD_DIMENSION, 0),
        (PARTIAL, 2),
        (SERVER_FAILURE, 2),
        (BAD_DIMENSION, 2),
    ] {
        let dir = tempfile::tempdir()?;
        let db = seed(dir.path())?;
        let server = Server::start(384)?;
        let mut first = backfill(&server, dir.path(), &db, 1);
        first.args(["--embedder", "external"]);
        let first_report = success(run(first)?)?;
        let staging = PathBuf::from(
            first_report["index_path"]
                .as_str()
                .context("staging path")?,
        );
        let before = fs::read(&staging)?;
        let checkpoint = SemanticManifest::load(dir.path())?
            .unwrap()
            .checkpoint
            .unwrap();
        assert_eq!(checkpoint.docs_embedded, 1);
        server.take_inputs();
        // Four preflight requests (three probes at two rows), then one good
        // corpus request: fail the second half of a three-message conversation.
        server
            .state
            .good_requests_before_fault
            .store(5, Ordering::SeqCst);
        server.state.fault.store(fault, Ordering::SeqCst);
        let mut failing = backfill(&server, dir.path(), &db, 10);
        failing
            .args(["--embedder", "external"])
            .env("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", retries.to_string());
        let error = run(failing)?;
        assert!(!error.status.success());
        assert_eq!(
            corpus(&server).len(),
            3 + if fault == SERVER_FAILURE { retries } else { 0 },
            "only the one-input transient failure may be retried"
        );
        assert_eq!(fs::read(&staging)?, before);
        let retained = SemanticManifest::load(dir.path())?
            .unwrap()
            .checkpoint
            .unwrap();
        assert_eq!(
            serde_json::to_value(&retained)?,
            serde_json::to_value(&checkpoint)?
        );
        assert!(!vector_index_path(dir.path(), &retained.embedder_id).exists());
        server.state.fault.store(GOOD, Ordering::SeqCst);
        let mut resume = backfill(&server, dir.path(), &db, 10);
        resume.args(["--embedder", "external"]);
        let report = success(run(resume)?)?;
        assert_eq!(report["published"], true);
        assert_eq!(report["model_initializations"], 1);
        let sent = corpus(&server);
        assert_eq!(sent.len(), 4);
        assert!(sent.iter().all(|text| !text.contains("conversation 1")));
        let fresh = tempfile::tempdir()?;
        let fresh_db = seed(fresh.path())?;
        let mut independent = backfill(&server, fresh.path(), &fresh_db, 10);
        independent.args(["--embedder", "external"]);
        let fresh_report = success(run(independent)?)?;
        assert_eq!(
            signature(Path::new(report["index_path"].as_str().unwrap()))?,
            signature(Path::new(fresh_report["index_path"].as_str().unwrap()))?
        );
    }
    Ok(())
}

#[test]
fn external_cli_disabled_and_local_selection_never_send_text() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = seed(dir.path())?;
    let server = Server::start(384)?;
    let mut disabled = backfill(&server, dir.path(), &db, 1);
    disabled
        .args(["--embedder", "external"])
        .env("CASS_EXTERNAL_EMBEDDINGS", "0");
    let output = run(disabled)?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("external_disabled"));
    assert!(!SemanticManifest::path(dir.path()).exists());
    assert!(server.take_inputs().is_empty());
    // Even valid consent plus a URL does not select an external producer.
    let mut local = backfill(&server, dir.path(), &db, 10);
    local.args(["--embedder", "hash"]);
    assert_eq!(success(run(local)?)?["embedder_id"], "fnv1a-384");
    assert!(server.take_inputs().is_empty());
    Ok(())
}

#[test]
fn external_cli_scheduled_policy_obeys_pause_before_preflight() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = seed(dir.path())?;
    let server = Server::start(384)?;
    let mut paused = backfill(&server, dir.path(), &db, 2);
    paused
        .arg("--scheduled")
        .env("CASS_SEMANTIC_EMBEDDER", "external")
        .env("CASS_SEMANTIC_BACKFILL_FOREGROUND_ACTIVE", "1");
    let report = success(run(paused)?)?;
    assert_eq!(report["status"], "paused");
    assert_eq!(report["model_initializations"], 0);
    assert!(server.take_inputs().is_empty());
    let mut admitted = backfill(&server, dir.path(), &db, 10);
    admitted
        .arg("--scheduled")
        .env("CASS_SEMANTIC_EMBEDDER", "external")
        .env("CASS_SEMANTIC_BACKFILL_FORCE", "1");
    assert_eq!(success(run(admitted)?)?["published"], true);
    assert_eq!(corpus(&server).len(), 5);
    Ok(())
}

#[test]
fn external_cli_query_uses_matching_space_and_disabled_queries_send_nothing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = seed(dir.path())?;
    let server = Server::start(384)?;
    let mut build = backfill(&server, dir.path(), &db, 10);
    build.args(["--embedder", "external"]);
    success(run(build)?)?;
    server.take_inputs();
    let query = "private corpus conversation 1 message 0 compiler evidence";
    for policy in [false, true] {
        let mut search = process(&server, dir.path(), &db);
        search.args([
            "search",
            query,
            "--mode",
            "semantic",
            "--no-daemon",
            "--no-maintenance",
            "--json",
            "--timeout",
            "15000",
            "--limit",
            "5",
        ]);
        if policy {
            search.env("CASS_SEMANTIC_EMBEDDER", "external");
        } else {
            search.args(["--model", "external"]);
        }
        let report = success(run(search)?)?;
        assert!(!report["hits"].as_array().context("search hits")?.is_empty());
        assert_eq!(corpus(&server), vec![query.to_owned()]);
    }
    let mut disabled = process(&server, dir.path(), &db);
    disabled
        .args([
            "search",
            query,
            "--mode",
            "semantic",
            "--model",
            "external",
            "--no-maintenance",
            "--json",
        ])
        .env("CASS_EXTERNAL_EMBEDDINGS", "0");
    assert!(!run(disabled)?.status.success());
    assert!(server.take_inputs().is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn external_cli_signals_stop_after_inflight_request_and_resume_from_checkpoint() -> Result<()> {
    for (signal, expected_code) in [("-INT", 130), ("-TERM", 143)] {
        let dir = tempfile::tempdir()?;
        let db = seed(dir.path())?;
        let server = Server::start(384)?;
        let mut first = backfill(&server, dir.path(), &db, 1);
        first.args(["--embedder", "external"]);
        let report = success(run(first)?)?;
        let staged = PathBuf::from(report["index_path"].as_str().unwrap());
        let before = fs::read(&staged)?;
        let checkpoint = SemanticManifest::load(dir.path())?
            .unwrap()
            .checkpoint
            .unwrap();
        server.take_inputs();
        server.state.hold_corpus_reply.store(true, Ordering::SeqCst);
        let mut command = backfill(&server, dir.path(), &db, 10);
        command
            .args(["--embedder", "external"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let ready = Instant::now() + Duration::from_secs(20);
        while !server.state.corpus_waiting.load(Ordering::SeqCst) && Instant::now() < ready {
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("backfill exited before request: {status}");
            }
            thread::sleep(Duration::from_millis(5));
        }
        if !server.state.corpus_waiting.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("backfill did not reach the synchronized corpus request");
        }
        assert!(
            Command::new("kill")
                .args([signal, &child.id().to_string()])
                .status()?
                .success()
        );
        server
            .state
            .hold_corpus_reply
            .store(false, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait()?.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if child.try_wait()?.is_none() {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("cancelled CLI exceeded HTTP deadline and retained its worker");
        }
        let output = child.wait_with_output()?;
        assert_eq!(
            output.status.code(),
            Some(expected_code),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["status"], "cancelled");
        assert_eq!(report["batches_completed"], 0);
        assert_eq!(fs::read(&staged)?, before);
        assert_eq!(
            serde_json::to_value(
                SemanticManifest::load(dir.path())?
                    .unwrap()
                    .checkpoint
                    .unwrap()
            )?,
            serde_json::to_value(&checkpoint)?
        );
        assert_eq!(
            corpus(&server).len(),
            2,
            "no subsequent request after signal"
        );
        let mut resume = backfill(&server, dir.path(), &db, 10);
        resume.args(["--embedder", "external"]);
        assert_eq!(success(run(resume)?)?["published"], true);
        assert_eq!(corpus(&server).len(), 4);
    }
    Ok(())
}

fn scheduled_fast(server: &Server, dir: &Path, db: &Path, batches: u32) -> Command {
    let mut command = process(server, dir, db);
    command
        .args([
            "models",
            "backfill",
            "--tier",
            "fast",
            "--embedder",
            "hash",
            "--batch-conversations",
            "1",
            "--scheduled",
            "--json",
            "--max-batches",
        ])
        .arg(batches.to_string())
        .arg("--data-dir")
        .arg(dir)
        .env("CASS_SEMANTIC_BACKFILL_FORCE", "1")
        .env("CASS_SCHEDULE_PRESERVE_CHECKPOINT", "1");
    command
}

#[test]
fn external_cli_stale_scheduled_worker_cannot_erase_another_checkpoint() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = seed(dir.path())?;
    let server = Server::start(384)?;
    let mut first = backfill(&server, dir.path(), &db, 1);
    first.args(["--embedder", "external"]);
    let first_report = success(run(first)?)?;
    let staging = PathBuf::from(
        first_report["index_path"]
            .as_str()
            .context("staging path")?,
    );
    let manifest_path = SemanticManifest::path(dir.path());
    let manifest_before = fs::read(&manifest_path)?;
    let vectors_before = fs::read(&staging)?;
    let wal = frankensearch::index::wal_path_for(&staging);
    let wal_before = fs::read(&wal).ok();
    server.take_inputs();

    // This is the worker a stale fast-first schedule would have launched.
    let refused = run(scheduled_fast(&server, dir.path(), &db, 1))?;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("semantic_checkpoint_owned"));
    assert!(
        server.take_inputs().is_empty(),
        "a hash worker must send no HTTP"
    );
    assert_eq!(fs::read(&manifest_path)?, manifest_before);
    assert_eq!(fs::read(&staging)?, vectors_before);
    assert_eq!(fs::read(&wal).ok(), wal_before);
    assert!(!vector_index_path(dir.path(), "fnv1a-384").exists());

    // A same-tier worker whose endpoint revision changed is not the owner either.
    let mut changed = backfill(&server, dir.path(), &db, 1);
    changed
        .args(["--embedder", "external", "--scheduled"])
        .env("CASS_EXTERNAL_EMBEDDING_REVISION", "v2")
        .env("CASS_SEMANTIC_BACKFILL_FORCE", "1")
        .env("CASS_SCHEDULE_PRESERVE_CHECKPOINT", "1");
    let refused = run(changed)?;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("semantic_checkpoint_owned"));
    assert!(
        corpus(&server).is_empty(),
        "only fixed preflight probes may precede admission"
    );
    assert_eq!(fs::read(&manifest_path)?, manifest_before);
    assert_eq!(fs::read(&staging)?, vectors_before);
    assert_eq!(fs::read(&wal).ok(), wal_before);

    let mut resume = backfill(&server, dir.path(), &db, 10);
    resume
        .args(["--embedder", "external", "--scheduled"])
        .env("CASS_SEMANTIC_BACKFILL_FORCE", "1")
        .env("CASS_SCHEDULE_PRESERVE_CHECKPOINT", "1");
    assert_eq!(success(run(resume)?)?["published"], true);
    let resumed = corpus(&server);
    assert_eq!(resumed.len(), 4);
    assert!(resumed.iter().all(|text| !text.contains("conversation 1")));
    let quality = SemanticManifest::load(dir.path())?
        .unwrap()
        .quality_tier
        .unwrap();
    let signature_before = signature(&dir.path().join(&quality.index_path))?;
    assert_eq!(
        success(run(scheduled_fast(&server, dir.path(), &db, 10))?)?["published"],
        true
    );
    assert!(server.take_inputs().is_empty());
    assert_eq!(
        SemanticManifest::load(dir.path())?.unwrap().quality_tier,
        Some(quality.clone())
    );
    assert_eq!(
        signature(&dir.path().join(&quality.index_path))?,
        signature_before
    );
    Ok(())
}

#[cfg(unix)]
fn nightly(server: &Server, dir: &Path, db: &Path) -> Command {
    let mut command = process(server, dir, db);
    // A real nightly invokes connector discovery. Carry only the explicit
    // fixture settings, not agent-home overrides from the developer's shell.
    let environment: Vec<_> = command
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| (key.to_os_string(), value.to_os_string()))
        })
        .collect();
    command
        .env_clear()
        .envs(environment)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .args([
            "schedule",
            "run",
            "--job",
            "nightly",
            "--force",
            "--json",
            "--data-dir",
        ])
        .arg(dir)
        .env("CASS_SEMANTIC_EMBEDDER", "external")
        .env("CASS_SEMANTIC_BACKFILL_FORCE", "1")
        .env("CASS_SEMANTIC_MAX_MESSAGES_PER_CHECKPOINT", "1")
        .env("CASS_SCHEDULE_MAX_BACKFILL_BATCHES", "1");
    command
}

#[cfg(unix)]
#[test]
fn external_cli_nightly_resumes_quality_across_budget_boundaries_before_starting_fast()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = seed(dir.path())?;
    let server = Server::start(384)?;
    let mut first = backfill(&server, dir.path(), &db, 1);
    first.args(["--embedder", "external"]);
    success(run(first)?)?;
    let original = SemanticManifest::load(dir.path())?
        .unwrap()
        .checkpoint
        .unwrap();
    assert_eq!(original.docs_embedded, 1);
    server.take_inputs();

    for (night, corpus_count) in [(1, 3), (2, 1)] {
        let report = success(run(nightly(&server, dir.path(), &db))?)?;
        let workers: Vec<_> = report["steps"]
            .as_array()
            .context("nightly steps")?
            .iter()
            .filter(|step| {
                step["name"].as_str().is_some_and(|name| {
                    name.starts_with("semantic-backfill:fast:")
                        || name.starts_with("semantic-backfill:quality:")
                })
            })
            .collect();
        assert_eq!(
            workers.len(),
            1,
            "one worker in the shared nightly budget: {report}"
        );
        assert_eq!(
            workers[0]["result"]["tier"], "quality",
            "night {night}: {report}"
        );
        let sent = corpus(&server);
        assert_eq!(sent.len(), corpus_count, "night {night}");
        assert!(sent.iter().all(|text| !text.contains("conversation 1")));
        let manifest = SemanticManifest::load(dir.path())?.context("nightly manifest")?;
        assert!(
            manifest.fast_tier.is_none(),
            "fast cannot preempt unfinished quality work"
        );
        if night == 1 {
            let checkpoint = manifest.checkpoint.context("bounded quality continuation")?;
            assert_eq!(checkpoint.embedder_id, original.embedder_id);
            assert_eq!(checkpoint.tier, TierKind::Quality);
            assert_eq!(checkpoint.docs_embedded, 4);
        } else {
            assert!(manifest.checkpoint.is_none());
            let artifact = manifest.quality_tier.context("completed external quality")?;
            assert_eq!(artifact.doc_count, 5);
            assert!(artifact.ready);
        }
    }
    let fresh = tempfile::tempdir()?;
    let fresh_db = seed(fresh.path())?;
    let mut independent = backfill(&server, fresh.path(), &fresh_db, 10);
    independent.args(["--embedder", "external"]);
    let expected = success(run(independent)?)?;
    assert_eq!(
        signature(&vector_index_path(dir.path(), &original.embedder_id))?,
        signature(Path::new(expected["index_path"].as_str().unwrap()))?
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn external_cli_nightly_refuses_disabled_or_changed_resume_configuration_without_http()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let db = seed(dir.path())?;
    let server = Server::start(384)?;
    let mut first = backfill(&server, dir.path(), &db, 1);
    first.args(["--embedder", "external"]);
    let checkpoint = success(run(first)?)?;
    let staged = PathBuf::from(
        checkpoint["index_path"]
            .as_str()
            .context("staging path")?,
    );
    let manifest_before = fs::read(SemanticManifest::path(dir.path()))?;
    let vectors_before = fs::read(&staged)?;
    server.take_inputs();
    for (key, value) in [
        ("CASS_EXTERNAL_EMBEDDINGS", "0"),
        ("CASS_EXTERNAL_EMBEDDING_REVISION", "another-revision"),
    ] {
        let mut command = nightly(&server, dir.path(), &db);
        command.env(key, value);
        let output = run(command)?;
        assert!(!output.status.success());
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["ok"], false);
        let failure = report["steps"]
            .as_array()
            .context("nightly steps")?
            .iter()
            .find(|step| step["name"] == "semantic-backfill-plan")
            .context("a named checkpoint admission failure")?;
        assert_eq!(failure["result"]["semantic_workers_started"], false);
        assert!(
            server.take_inputs().is_empty(),
            "planning cannot send even probes"
        );
        assert_eq!(
            fs::read(SemanticManifest::path(dir.path()))?,
            manifest_before
        );
        assert_eq!(fs::read(&staged)?, vectors_before);
    }
    Ok(())
}
