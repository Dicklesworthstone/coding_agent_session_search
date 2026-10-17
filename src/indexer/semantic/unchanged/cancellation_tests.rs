//! Cancellation must not turn a read-only proof into new serving authority.
use super::*;
use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use crate::search::external_embedder::{CancelCheck, ExternalEmbeddingConfig};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

struct Endpoint {
    url: String,
    requests: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl Endpoint {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}/v1/embeddings", listener.local_addr()?);
        let requests = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&requests);
        let stop = Arc::clone(&stopped);
        let worker = thread::spawn(move || -> Result<()> {
            while !stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
                // The listener polls; accepted request streams use bounded blocking I/O.
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line)?;
                ensure!(line.starts_with("POST /v1/embeddings "));
                let mut length = None;
                loop {
                    line.clear();
                    ensure!(reader.read_line(&mut line)? > 0, "truncated HTTP header");
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("content-length")
                    {
                        length = Some(value.trim().parse::<usize>()?);
                    }
                }
                let length = length.context("missing request length")?;
                ensure!(length <= 1024 * 1024, "unbounded fixture request");
                let mut body = vec![0; length];
                reader.read_exact(&mut body)?;
                let request: Value = serde_json::from_slice(&body)?;
                seen.fetch_add(1, Ordering::SeqCst);
                let data: Vec<_> = request["input"]
                    .as_array()
                    .context("input array")?
                    .iter()
                    .enumerate()
                    .map(|(index, text)| {
                        let mut vector = vec![0.0_f32; 8];
                        let slot = text
                            .as_str()
                            .unwrap()
                            .bytes()
                            .fold(0usize, |n, b| (n + usize::from(b)) % 8);
                        vector[slot] = 1.0;
                        json!({"index": index, "embedding": vector})
                    })
                    .collect();
                let body = serde_json::to_vec(&json!({
                    "model": request["model"], "data": data,
                }))?;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )?;
                stream.write_all(&body)?;
            }
            Ok(())
        });
        Ok(Self {
            url,
            requests,
            stopped,
            worker: Some(worker),
        })
    }

    fn config(&self) -> Result<ExternalEmbeddingConfig> {
        ExternalEmbeddingConfig::from_lookup(|key| match key {
            "CASS_EXTERNAL_EMBEDDINGS" => Some("1".into()),
            "CASS_EXTERNAL_EMBEDDING_URL" => Some(self.url.clone()),
            "CASS_EXTERNAL_EMBEDDING_MODEL" => Some("proof-fixture".into()),
            "CASS_EXTERNAL_EMBEDDING_DIMENSION" => Some("8".into()),
            "CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS" => Some("2000".into()),
            _ => None,
        })?
        .context("explicit fixture configuration")
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result
                    .expect("fixture thread panicked")
                    .expect("fixture server failed");
            }
        }
    }
}

#[test]
fn cancelled_read_only_proof_preserves_publication_and_cannot_refresh_receipt() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let data = temp.path();
    let storage = FrankenStorage::open(&data.join("agent_search.db"))?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    for ordinal in 0..4 {
        storage.insert_conversation_tree(
            agent,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some(format!("proof-{ordinal}")),
                title: None,
                source_path: format!("/fixture/proof-{ordinal}.jsonl").into(),
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
                    content: format!("private proof corpus {ordinal}"),
                    extra_json: json!({}),
                    snippets: Vec::new(),
                }],
            },
        )?;
    }
    let endpoint = Endpoint::start()?;
    let polls = Arc::new(AtomicUsize::new(0));
    let cancel_at = Arc::new(AtomicUsize::new(usize::MAX));
    let check: CancelCheck = {
        let polls = Arc::clone(&polls);
        let cancel_at = Arc::clone(&cancel_at);
        Arc::new(move || {
            polls.fetch_add(1, Ordering::SeqCst) + 1 >= cancel_at.load(Ordering::SeqCst)
        })
    };
    let engine = engine::SemanticIndexer::with_external_config(endpoint.config()?, check)?;
    let plan = SemanticBackfillStoragePlan {
        tier: TierKind::Quality,
        db_fingerprint: crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?,
        model_revision: engine.embedder_id().into(),
        max_conversations: 16,
    };
    let mut manifest = SemanticManifest::default();
    let built = engine.run_backfill_from_storage(&storage, data, &mut manifest, plan.clone())?;
    assert!(built.published);
    let receipt = data
        .join(crate::search::vector_index::VECTOR_INDEX_DIR)
        .join(format!(
            ".completed-backfill-quality-{}.json",
            engine.embedder_id(),
        ));
    // Corrupt only the disposable skip hint: the complete generation stays valid.
    fs::write(&receipt, b"incomplete skip receipt")?;
    let vectors_before = fs::read(&built.index_path)?;
    let manifest_before = fs::read(SemanticManifest::path(data))?;
    let sent_before = endpoint.requests.load(Ordering::SeqCst);
    let sink = SemanticProgressSink::disabled();
    // Cancel in the vector loop, in canonical replay, and at the final proof
    // checkpoint. A no-op must not hide cancellation or refresh its skip hint.
    for (threshold, final_checkpoint) in [(3, false), (7, false), (usize::MAX, true)] {
        polls.store(0, Ordering::SeqCst);
        cancel_at.store(threshold, Ordering::SeqCst);
        let error = prove_unchanged(&engine, &storage, data, &manifest, &plan, &sink, || {
            if final_checkpoint {
                cancel_at.store(0, Ordering::SeqCst);
            }
            Ok(())
        })
        .expect_err("a cancelled proof must not announce completion");
        assert!(
            format!("{error:#}").contains("external_cancelled"),
            "{error:#}"
        );
        assert_eq!(fs::read(&built.index_path)?, vectors_before);
        assert_eq!(fs::read(SemanticManifest::path(data))?, manifest_before);
        assert_eq!(fs::read(&receipt)?, b"incomplete skip receipt");
        assert_eq!(endpoint.requests.load(Ordering::SeqCst), sent_before);
    }
    cancel_at.store(usize::MAX, Ordering::SeqCst);
    let outcome = prove_unchanged(&engine, &storage, data, &manifest, &plan, &sink, || Ok(()))?
        .context("uncancelled proof must retain the completed generation")?;
    assert!(outcome.unchanged && outcome.published);
    assert_eq!(outcome.embedded_docs, 0);
    assert_eq!(fs::read(&built.index_path)?, vectors_before);
    assert_eq!(fs::read(SemanticManifest::path(data))?, manifest_before);
    assert_eq!(endpoint.requests.load(Ordering::SeqCst), sent_before);
    assert!(
        engine
            .completed_backfill_fingerprint(
                &storage,
                data,
                &manifest,
                plan.tier,
                &plan.model_revision,
            )?
            .is_some()
    );
    Ok(())
}
