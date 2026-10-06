//! Real FSVI/checkpoint tests over a loopback OpenAI-compatible fake.
//! No local model assets, GPU, cloud credentials or environment mutation.
use anyhow::{Context, Result};
use coding_agent_search::indexer::semantic::{
    EmbeddingInput, SemanticBackfillBatchPlan, SemanticIndexer, expected_vector_space_revision,
};
use coding_agent_search::search::external_embedder::{
    CancelCheck, EXTERNAL_VECTOR_SPACE_REVISION, ExternalEmbeddingConfig,
};
use coding_agent_search::search::semantic_manifest::{SemanticManifest, TierKind};
use coding_agent_search::search::vector_index::vector_index_path;
use frankensearch::index::VectorIndex;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[path = "external_backfill_runtime/cli.rs"]
mod cli;

const GOOD: usize = 0;
const BAD_DIMENSION: usize = 1;
const PARTIAL: usize = 2;
const SERVER_FAILURE: usize = 3;
const CANCEL_AFTER_RESPONSE: usize = 4;

struct State {
    fault: AtomicUsize,
    dimension: AtomicUsize,
    good_requests_before_fault: AtomicUsize,
    cancel: Arc<AtomicBool>,
    stopped: AtomicBool,
    hold_corpus_reply: AtomicBool,
    corpus_waiting: AtomicBool,
    inputs: Mutex<Vec<Vec<String>>>,
}

struct Server {
    url: String,
    state: Arc<State>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl Server {
    fn start(dimension: usize) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}/v1/embeddings", listener.local_addr()?);
        let state = Arc::new(State {
            fault: AtomicUsize::new(GOOD),
            dimension: AtomicUsize::new(dimension),
            good_requests_before_fault: AtomicUsize::new(0),
            cancel: Arc::new(AtomicBool::new(false)),
            stopped: AtomicBool::new(false),
            hold_corpus_reply: AtomicBool::new(false),
            corpus_waiting: AtomicBool::new(false),
            inputs: Mutex::new(Vec::new()),
        });
        let shared = Arc::clone(&state);
        let worker = thread::spawn(move || {
            while !shared.stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &shared)?,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        });
        Ok(Self {
            url,
            state,
            worker: Some(worker),
        })
    }

    fn config(
        &self,
        model: &str,
        dimension: usize,
        revision: &str,
    ) -> Result<ExternalEmbeddingConfig> {
        ExternalEmbeddingConfig::from_lookup(|name| match name {
            "CASS_EXTERNAL_EMBEDDINGS" => Some("1".into()),
            "CASS_EXTERNAL_EMBEDDING_URL" => Some(self.url.clone()),
            "CASS_EXTERNAL_EMBEDDING_MODEL" => Some(model.into()),
            "CASS_EXTERNAL_EMBEDDING_DIMENSION" => Some(dimension.to_string()),
            "CASS_EXTERNAL_EMBEDDING_REVISION" => Some(revision.into()),
            "CASS_EXTERNAL_EMBEDDING_BATCH_SIZE" => Some("2".into()),
            "CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS" => Some("2000".into()),
            _ => None,
        })?
        .context("explicit test consent must produce a configuration")
    }

    fn cancel_check(&self) -> CancelCheck {
        let flag = Arc::clone(&self.state.cancel);
        Arc::new(move || flag.load(Ordering::SeqCst))
    }

    fn indexer(&self) -> Result<SemanticIndexer> {
        SemanticIndexer::with_external_config(
            self.config(
                "fixture-model",
                self.state.dimension.load(Ordering::SeqCst),
                "v1",
            )?,
            self.cancel_check(),
        )
    }

    fn take_inputs(&self) -> Vec<Vec<String>> {
        std::mem::take(&mut *self.state.inputs.lock().expect("fake inputs"))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result
                    .expect("fake server thread panicked")
                    .expect("fake server I/O failed");
            }
        }
    }
}

fn serve(mut stream: TcpStream, state: &State) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    assert_eq!(line.trim_end(), "POST /v1/embeddings HTTP/1.1");
    let mut length = None;
    let mut header_bytes = line.len();
    loop {
        line.clear();
        anyhow::ensure!(reader.read_line(&mut line)? > 0, "truncated fake request");
        header_bytes += line.len();
        anyhow::ensure!(header_bytes <= 16 * 1024, "fake request headers too large");
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("request content length")?;
    anyhow::ensure!(length <= 262_144, "provider exceeded request byte bound");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    let request: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(request["encoding_format"], "float");
    let inputs: Vec<String> = serde_json::from_value(request["input"].clone())?;
    assert!(
        !inputs.is_empty() && inputs.len() <= 2,
        "provider exceeded configured row bound"
    );
    state
        .inputs
        .lock()
        .expect("fake inputs")
        .push(inputs.clone());
    if inputs
        .iter()
        .any(|input| input.starts_with("private corpus"))
        && state.hold_corpus_reply.load(Ordering::SeqCst)
    {
        state.corpus_waiting.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while state.hold_corpus_reply.load(Ordering::SeqCst)
            && !state.stopped.load(Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(2));
        }
    }
    let configured_fault = state.fault.load(Ordering::SeqCst);
    let fault = if configured_fault != GOOD
        && state.good_requests_before_fault.load(Ordering::SeqCst) > 0
    {
        state
            .good_requests_before_fault
            .fetch_sub(1, Ordering::SeqCst);
        GOOD
    } else {
        configured_fault
    };
    let dimension = state.dimension.load(Ordering::SeqCst);
    let mut data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let mut vector = vec![0.0_f32; dimension];
            let coordinate = input
                .bytes()
                .fold(0_usize, |sum, byte| sum.wrapping_add(usize::from(byte)))
                % dimension;
            vector[coordinate] = 1.0;
            if fault == BAD_DIMENSION {
                vector.push(0.0);
            }
            json!({"index": index, "embedding": vector})
        })
        .collect();
    if fault == PARTIAL {
        data.pop();
    }
    // A reversed wire order must still yield the input order in persisted FSVI rows.
    data.reverse();
    let status = if fault == SERVER_FAILURE {
        "503 Service Unavailable"
    } else {
        "200 OK"
    };
    let body = serde_json::to_vec(&json!({"model": request["model"], "data": data}))?;
    if fault == CANCEL_AFTER_RESPONSE {
        state.cancel.store(true, Ordering::SeqCst);
    }
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

fn plan(offset: i64, complete: bool) -> SemanticBackfillBatchPlan {
    SemanticBackfillBatchPlan {
        tier: TierKind::Quality,
        db_fingerprint: "external-fixture-canonical-db-v1".into(),
        model_revision: EXTERNAL_VECTOR_SPACE_REVISION.into(),
        total_conversations: 2,
        conversations_in_batch: 1,
        last_offset: offset,
        cursor_exhausted: complete,
    }
}

#[test]
fn external_backfill_partial_failures_preserve_checkpoint_and_resume_after_restart() -> Result<()> {
    for (fault, expected) in [
        (BAD_DIMENSION, "external_dimension_mismatch"),
        (PARTIAL, "external_partial_response"),
        (SERVER_FAILURE, "external_http_503"),
    ] {
        let temp = tempfile::tempdir()?;
        let server = Server::start(384)?;
        let indexer = server.indexer()?;
        let id = indexer.embedder_id().to_owned();
        assert_ne!(id, "minilm-384");
        assert_eq!(
            expected_vector_space_revision(&id),
            Some(EXTERNAL_VECTOR_SPACE_REVISION)
        );
        let mut manifest = SemanticManifest::default();
        let first = indexer.run_backfill_batch(
            &[EmbeddingInput::new(
                1,
                "already durable: ownership compiler checkpoint",
            )],
            temp.path(),
            &mut manifest,
            plan(1, false),
        )?;
        assert!(first.checkpoint_saved && !first.published);
        let manifest_before = fs::read(SemanticManifest::path(temp.path()))?;
        let staging_before = fs::read(&first.index_path)?;
        server.take_inputs();
        server
            .state
            .good_requests_before_fault
            .store(1, Ordering::SeqCst);
        server.state.fault.store(fault, Ordering::SeqCst);
        // Exceed the HTTP row bound; a failed call must not publish a prefix.
        let tail = [
            EmbeddingInput::new(2, "not durable yet: endpoint tail first"),
            EmbeddingInput::new(3, "not durable yet: endpoint tail second"),
            EmbeddingInput::new(4, "not durable yet: endpoint tail third"),
        ];
        let error = indexer
            .run_backfill_batch(&tail, temp.path(), &mut manifest, plan(2, true))
            .expect_err("injected endpoint failure must fail the checkpoint batch");
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert_eq!(
            fs::read(SemanticManifest::path(temp.path()))?,
            manifest_before
        );
        assert_eq!(fs::read(&first.index_path)?, staging_before);
        assert!(!vector_index_path(temp.path(), &id).exists());
        assert_eq!(manifest.checkpoint.as_ref().unwrap().last_offset, 1);
        assert_eq!(
            server.take_inputs().len(),
            2,
            "failure must follow one successful HTTP sub-batch"
        );
        drop(indexer);

        server.state.fault.store(GOOD, Ordering::SeqCst);
        let restarted = server.indexer()?;
        let mut resumed = SemanticManifest::load(temp.path())?.context("durable checkpoint")?;
        assert_eq!(resumed.checkpoint.as_ref().unwrap().last_offset, 1);
        server.take_inputs();
        let outcome =
            restarted.run_backfill_batch(&tail, temp.path(), &mut resumed, plan(2, true))?;
        assert!(outcome.published && !outcome.checkpoint_saved);
        let index = VectorIndex::open_read_only(&outcome.index_path)?;
        assert_eq!(index.embedder_id(), id);
        assert_eq!(index.embedder_revision(), EXTERNAL_VECTOR_SPACE_REVISION);
        assert_eq!(index.dimension(), 384);
        assert_eq!(index.record_count(), 4);
        let requests = server.take_inputs();
        assert!(requests.iter().all(|batch| batch.len() <= 2));
        assert!(
            requests
                .iter()
                .flatten()
                .all(|text| !text.contains("already durable"))
        );
        // Compare against an independently built, uninterrupted generation.
        // Its HTTP boundaries differ (2+2 rather than 1 then 2+1), so swapped
        // response indices cannot pass merely by reproducing the same mistake.
        let fresh_dir = tempfile::tempdir()?;
        let mut fresh_manifest = SemanticManifest::default();
        let mut all = vec![EmbeddingInput::new(
            1,
            "already durable: ownership compiler checkpoint",
        )];
        all.extend(tail.iter().cloned());
        let fresh = restarted.run_backfill_batch(
            &all,
            fresh_dir.path(),
            &mut fresh_manifest,
            plan(2, true),
        )?;
        let fresh_index = VectorIndex::open_read_only(&fresh.index_path)?;
        let signature =
            |index: &VectorIndex| -> Result<std::collections::BTreeMap<String, Vec<u32>>> {
                (0..index.record_count())
                    .map(|record| {
                        Ok((
                            index.doc_id_at(record)?.to_owned(),
                            index
                                .vector_at_f32(record)?
                                .into_iter()
                                .map(f32::to_bits)
                                .collect(),
                        ))
                    })
                    .collect()
            };
        assert_eq!(signature(&index)?, signature(&fresh_index)?);
        // Durable one-hot output must remain finite and normalized.
        for record in 0..index.record_count() {
            let vector = index.vector_at_f32(record)?;
            assert_eq!(vector.iter().filter(|&&value| value == 1.0).count(), 1);
            assert_eq!(vector.iter().map(|value| value * value).sum::<f32>(), 1.0);
        }
    }
    Ok(())
}

#[test]
fn external_backfill_cancellation_preserves_checkpoint_and_local_vectors() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let local = SemanticIndexer::new("hash", None)?;
    let local_rows =
        local.embed_messages(&[EmbeddingInput::new(99, "local-only corpus remains private")])?;
    drop(local.build_and_save_index(local_rows, temp.path())?);
    let local_path = vector_index_path(temp.path(), local.embedder_id());
    let local_before = fs::read(&local_path)?;
    let server = Server::start(384)?;
    let indexer = server.indexer()?;
    let mut manifest = SemanticManifest::default();
    let first = indexer.run_backfill_batch(
        &[EmbeddingInput::new(1, "checkpoint before cancellation")],
        temp.path(),
        &mut manifest,
        plan(1, false),
    )?;
    let checkpoint_before = fs::read(SemanticManifest::path(temp.path()))?;
    let staging_before = fs::read(&first.index_path)?;
    server.take_inputs();
    server
        .state
        .fault
        .store(CANCEL_AFTER_RESPONSE, Ordering::SeqCst);
    let error = indexer
        .run_backfill_batch(
            &[
                EmbeddingInput::new(2, "in-flight cancellation first"),
                EmbeddingInput::new(3, "in-flight cancellation second"),
                EmbeddingInput::new(4, "must never send this later batch"),
            ],
            temp.path(),
            &mut manifest,
            plan(2, true),
        )
        .expect_err("cancelled output must not be published");
    assert!(format!("{error:#}").contains("external_cancelled"));
    assert_eq!(server.take_inputs().len(), 1);
    assert_eq!(
        fs::read(SemanticManifest::path(temp.path()))?,
        checkpoint_before
    );
    assert_eq!(fs::read(&first.index_path)?, staging_before);
    assert_eq!(fs::read(local_path)?, local_before);
    assert_eq!(manifest.checkpoint.as_ref().unwrap().last_offset, 1);
    Ok(())
}

#[test]
fn external_preflight_refuses_before_any_checkpoint_or_index_can_be_written() -> Result<()> {
    let server = Server::start(384)?;
    server.state.fault.store(BAD_DIMENSION, Ordering::SeqCst);
    let error = match server.indexer() {
        Ok(_) => anyhow::bail!("malformed preflight admitted a runtime indexer"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("external_dimension_mismatch"));
    assert!(
        server
            .take_inputs()
            .iter()
            .flatten()
            .all(|text| !text.contains("private archive"))
    );
    server.state.fault.store(GOOD, Ordering::SeqCst);
    server.state.cancel.store(true, Ordering::SeqCst);
    assert!(server.indexer().is_err());
    assert!(
        server.take_inputs().is_empty(),
        "pre-cancelled construction made an HTTP request"
    );
    Ok(())
}

#[test]
fn disabled_external_configuration_cannot_construct_an_indexer_or_send_text() -> Result<()> {
    let server = Server::start(384)?;
    let mut seen = Vec::new();
    let config = ExternalEmbeddingConfig::from_lookup(|key| {
        seen.push(key.to_owned());
        match key {
            "CASS_EXTERNAL_EMBEDDINGS" => Some("0".into()),
            "CASS_EXTERNAL_EMBEDDING_URL" => Some(server.url.clone()),
            _ => None,
        }
    })?;
    assert!(config.is_none());
    assert_eq!(seen, ["CASS_EXTERNAL_EMBEDDINGS"]);
    let local = SemanticIndexer::new("hash", None)?;
    assert_eq!(
        local
            .embed_messages(&[EmbeddingInput::new(1, "private archive text")])?
            .len(),
        1
    );
    assert!(server.take_inputs().is_empty());
    Ok(())
}

#[test]
fn external_model_dimension_revision_switches_never_resume_other_spaces() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let server = Server::start(384)?;
    let original = server.indexer()?;
    let mut manifest = SemanticManifest::default();
    let first = original.run_backfill_batch(
        &[EmbeddingInput::new(1, "retained first provider checkpoint")],
        temp.path(),
        &mut manifest,
        plan(1, false),
    )?;
    let original_id = original.embedder_id().to_owned();
    let retained = fs::read(&first.index_path)?;
    let original_manifest = fs::read(SemanticManifest::path(temp.path()))?;
    // Independently reload the same checkpoint into isolated directories. Each
    // new provider must start its own space even at the same 384 dimensions.
    for (model, dimension, revision) in [
        ("other-model", 384, "v1"),
        ("fixture-model", 256, "v1"),
        ("fixture-model", 384, "v2"),
    ] {
        let other_dir = tempfile::tempdir()?;
        let manifest_path = SemanticManifest::path(other_dir.path());
        fs::create_dir_all(manifest_path.parent().context("manifest parent")?)?;
        fs::write(&manifest_path, &original_manifest)?;
        let relative_staging = first.index_path.strip_prefix(temp.path())?;
        fs::copy(&first.index_path, other_dir.path().join(relative_staging))?;
        server.state.dimension.store(dimension, Ordering::SeqCst);
        let other = SemanticIndexer::with_external_config(
            server.config(model, dimension, revision)?,
            server.cancel_check(),
        )?;
        assert_ne!(other.embedder_id(), original_id);
        let mut old_checkpoint = SemanticManifest::load(other_dir.path())?.unwrap();
        let result = other.run_backfill_batch(
            &[EmbeddingInput::new(2, "replacement provider only")],
            other_dir.path(),
            &mut old_checkpoint,
            plan(1, false),
        )?;
        assert_eq!(
            result.conversations_processed, 1,
            "foreign cursor was reused"
        );
        let checkpoint = old_checkpoint
            .checkpoint
            .as_ref()
            .context("new-space checkpoint")?;
        assert_eq!(checkpoint.docs_embedded, 1);
        assert_eq!(checkpoint.embedder_id, other.embedder_id());
        let reader = VectorIndex::open_read_only(&result.index_path)?;
        assert_eq!(reader.record_count(), 1);
        assert_eq!(reader.embedder_id(), other.embedder_id());
        assert_eq!(reader.dimension(), dimension);
        assert_eq!(fs::read(&first.index_path)?, retained);
    }
    Ok(())
}

#[test]
fn external_artifact_identity_is_not_an_instruction_to_contact_a_provider() -> Result<()> {
    let server = Server::start(384)?;
    let identity = server.config("fixture-model", 384, "v1")?.identity();
    assert!(SemanticIndexer::new(&identity, None).is_err());
    assert!(server.take_inputs().is_empty());
    assert_eq!(
        expected_vector_space_revision("external-v1-384-not-a-digest"),
        None
    );
    assert_ne!(
        expected_vector_space_revision("minilm-384"),
        Some(EXTERNAL_VECTOR_SPACE_REVISION)
    );
    Ok(())
}

#[test]
fn external_cancelled_empty_batch_preserves_checkpoint_without_http() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let server = Server::start(384)?;
    let indexer = server.indexer()?;
    let mut manifest = SemanticManifest::default();
    let first = indexer.run_backfill_batch(
        &[EmbeddingInput::new(1, "retained prefix")],
        dir.path(),
        &mut manifest,
        plan(1, false),
    )?;
    let staged = fs::read(&first.index_path)?;
    let ledger = fs::read(SemanticManifest::path(dir.path()))?;
    server.take_inputs();
    server.state.cancel.store(true, Ordering::SeqCst);
    let error = indexer
        .run_backfill_batch(&[], dir.path(), &mut manifest, plan(2, true))
        .expect_err("empty batches must not bypass cancellation and publish");
    assert!(error.to_string().contains("external_cancelled"));
    assert!(server.take_inputs().is_empty());
    assert_eq!(fs::read(&first.index_path)?, staged);
    assert_eq!(fs::read(SemanticManifest::path(dir.path()))?, ledger);
    assert!(manifest.quality_tier.is_none());
    assert!(!vector_index_path(dir.path(), indexer.embedder_id()).exists());
    Ok(())
}

#[test]
fn external_cancelled_empty_archive_does_not_publish_and_local_batches_still_drain() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let server = Server::start(384)?;
    let indexer = server.indexer()?;
    server.take_inputs();
    server.state.cancel.store(true, Ordering::SeqCst);
    assert!(indexer.embed_messages(&[]).is_err());
    let mut manifest = SemanticManifest::default();
    assert!(
        indexer
            .run_backfill_batch(&[], dir.path(), &mut manifest, plan(2, true))
            .is_err()
    );
    assert!(server.take_inputs().is_empty());
    assert!(manifest.checkpoint.is_none());
    assert!(manifest.quality_tier.is_none());
    assert!(!SemanticManifest::path(dir.path()).exists());
    let local = SemanticIndexer::new_with_cancel(
        "hash",
        None,
        Arc::new(|| panic!("local inference must retain its drain-before-cancellation contract")),
    )?;
    assert!(local.embed_messages(&[])?.is_empty());
    assert_eq!(
        local
            .embed_messages(&[EmbeddingInput::new(1, "local batch")])?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn external_cancelled_canonical_scan_keeps_vectors_and_resumes_exactly() -> Result<()> {
    use coding_agent_search::indexer::semantic::SemanticBackfillStoragePlan;
    use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
    use coding_agent_search::storage::sqlite::FrankenStorage;
    use std::collections::BTreeMap;
    use std::path::Path;

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
    let dir = tempfile::tempdir()?;
    let storage = FrankenStorage::open(&dir.path().join("agent_search.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    for ordinal in 0..32 {
        storage.insert_conversation_tree(
            agent,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some(format!("cancel-scan-{ordinal}")),
                title: None,
                source_path: format!("/fixture/cancel-scan-{ordinal}.jsonl").into(),
                started_at: Some(1),
                ended_at: Some(2),
                approx_tokens: None,
                metadata_json: json!({}),
                source_id: "local".into(),
                origin_host: None,
                messages: vec![Message {
                    id: None,
                    idx: 0,
                    role: MessageRole::User,
                    author: None,
                    created_at: Some(1),
                    content: format!("private canonical conversation {ordinal}"),
                    extra_json: json!({}),
                    snippets: Vec::new(),
                }],
            },
        )?;
    }
    let server = Server::start(384)?;
    let armed = Arc::new(AtomicBool::new(false));
    let polls = Arc::new(AtomicUsize::new(0));
    let check: CancelCheck = {
        let armed = armed.clone();
        let polls = polls.clone();
        Arc::new(move || armed.load(Ordering::SeqCst) && polls.fetch_add(1, Ordering::SeqCst) >= 19)
    };
    let indexer =
        SemanticIndexer::with_external_config(server.config("fixture-model", 384, "v1")?, check)?;
    let storage_plan = |max_conversations| SemanticBackfillStoragePlan {
        tier: TierKind::Quality,
        db_fingerprint: "cancel-scan-fixture".into(),
        model_revision: indexer.embedder_id().to_owned(),
        max_conversations,
    };
    let mut manifest = SemanticManifest::default();
    let first =
        indexer.run_backfill_from_storage(&storage, dir.path(), &mut manifest, storage_plan(1))?;
    assert!(first.checkpoint_saved && !first.published);
    let checkpoint = serde_json::to_value(manifest.checkpoint.as_ref().unwrap())?;
    let staged = fs::read(&first.index_path)?;
    server.take_inputs();
    armed.store(true, Ordering::SeqCst);
    let error = indexer
        .run_backfill_from_storage(&storage, dir.path(), &mut manifest, storage_plan(1))
        .expect_err("count scanning must cooperate before corpus requests");
    assert!(
        format!("{error:#}").contains("canonical scan interrupted"),
        "{error:#}"
    );
    assert_eq!(polls.load(Ordering::SeqCst), 20);
    assert!(server.take_inputs().is_empty());
    assert_eq!(fs::read(&first.index_path)?, staged);
    let mut restored = SemanticManifest::load(dir.path())?.context("durable manifest")?;
    assert_eq!(
        serde_json::to_value(restored.checkpoint.as_ref().unwrap())?,
        checkpoint
    );
    armed.store(false, Ordering::SeqCst);
    let resumed =
        indexer.run_backfill_from_storage(&storage, dir.path(), &mut restored, storage_plan(64))?;
    assert!(resumed.published);
    assert_eq!(resumed.embedded_docs, 31);
    let sent = server.take_inputs().concat();
    assert_eq!(sent.len(), 31);
    assert!(
        !sent
            .iter()
            .any(|text| text == "private canonical conversation 0")
    );
    let independent = tempfile::tempdir()?;
    let fresh = indexer.run_backfill_from_storage(
        &storage,
        independent.path(),
        &mut SemanticManifest::default(),
        storage_plan(64),
    )?;
    assert!(fresh.published);
    assert_eq!(
        signature(&resumed.index_path)?,
        signature(&fresh.index_path)?
    );
    Ok(())
}
