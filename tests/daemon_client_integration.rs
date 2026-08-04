use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use coding_agent_search::search::daemon_client::{
    DaemonClient, DaemonError, DaemonFallbackEmbedder, DaemonFallbackReranker, DaemonRetryConfig,
};
use coding_agent_search::search::embedder::{Embedder, EmbedderResult};
use coding_agent_search::search::reranker::{
    RerankDocument, RerankScore, Reranker, RerankerResult, rerank_texts,
};
use frankensearch::{AssumedDaemonClient, DaemonTrustLevelV1, ModelCategory, SearchError};
use parking_lot::Mutex;

#[cfg(unix)]
#[test]
fn issue_347_uds_client_rejects_same_width_wrong_model() {
    use coding_agent_search::daemon::protocol::{
        EmbedResponse, FramedMessage, HealthStatus, PROTOCOL_VERSION, Request, Response,
        decode_message, encode_message,
    };
    use coding_agent_search::daemon::{DaemonClientConfig, UdsDaemonClient};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    let temp = tempfile::tempdir().expect("tempdir");
    let socket = temp.path().join("semantic.sock");
    let listener = UnixListener::bind(&socket).expect("bind daemon fixture socket");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept client");
        for _ in 0..2 {
            let mut len = [0_u8; 4];
            stream.read_exact(&mut len).expect("read request length");
            let mut payload = vec![0_u8; u32::from_be_bytes(len) as usize];
            stream
                .read_exact(&mut payload)
                .expect("read request payload");
            let request: FramedMessage<Request> = decode_message(&payload).expect("decode request");
            let response = match request.payload {
                Request::Health => Response::Health(HealthStatus {
                    uptime_secs: 1,
                    version: PROTOCOL_VERSION,
                    ready: true,
                    memory_bytes: 0,
                }),
                Request::Embed { texts, .. } => Response::Embed(EmbedResponse {
                    embeddings: vec![vec![0.25; 384]; texts.len()],
                    model: "hash-384".to_string(),
                    elapsed_ms: 1,
                }),
                _ => Response::Health(HealthStatus {
                    uptime_secs: 1,
                    version: PROTOCOL_VERSION,
                    ready: false,
                    memory_bytes: 0,
                }),
            };
            stream
                .write_all(
                    &encode_message(&FramedMessage::new(request.request_id, response))
                        .expect("encode response"),
                )
                .expect("write response");
        }
    });

    let client = UdsDaemonClient::new(DaemonClientConfig {
        socket_path: socket,
        auto_spawn: false,
        expected_embedder_id: Some("minilm-384".to_string()),
        ..Default::default()
    });
    client.connect().expect("connect to fixture daemon");
    assert!(client.is_available(), "fixture health must be ready");
    let error = client
        .embed("daemon contract probe", "issue-347")
        .expect_err("a hash response must be rejected for the MiniLM index");
    assert!(matches!(error, DaemonError::InvalidInput(_)));
    assert!(error.to_string().contains("expected minilm-384"));
    assert!(error.to_string().contains("received hash-384"));
    server.join().expect("daemon fixture thread");
}

#[derive(Clone, Copy)]
enum DaemonMode {
    Ok,
    Drop,
    Timeout,
}

enum DaemonRequest {
    Embed {
        resp: mpsc::Sender<Result<Vec<f32>, DaemonError>>,
    },
    EmbedBatch {
        count: usize,
        resp: mpsc::Sender<Result<Vec<Vec<f32>>, DaemonError>>,
    },
    Rerank {
        count: usize,
        resp: mpsc::Sender<Result<Vec<f32>, DaemonError>>,
    },
    Shutdown,
}

struct ChannelDaemonClient {
    id: String,
    available: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    tx: mpsc::Sender<DaemonRequest>,
    timeout: Duration,
}

impl ChannelDaemonClient {
    fn send_request<T>(
        &self,
        request: DaemonRequest,
        resp_rx: mpsc::Receiver<Result<T, DaemonError>>,
    ) -> Result<T, DaemonError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.tx.send(request).is_err() {
            return Err(DaemonError::Unavailable(
                "daemon channel closed".to_string(),
            ));
        }
        match resp_rx.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(DaemonError::Timeout("daemon response timeout".to_string()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(DaemonError::Unavailable(
                "daemon channel closed".to_string(),
            )),
        }
    }
}

impl DaemonClient for ChannelDaemonClient {
    fn id(&self) -> &str {
        &self.id
    }

    fn is_available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }

    fn embed(&self, _text: &str, _request_id: &str) -> Result<Vec<f32>, DaemonError> {
        if !self.is_available() {
            return Err(DaemonError::Unavailable("daemon not available".to_string()));
        }
        let (resp_tx, resp_rx) = mpsc::channel();
        self.send_request(DaemonRequest::Embed { resp: resp_tx }, resp_rx)
    }

    fn embed_batch(&self, texts: &[&str], _request_id: &str) -> Result<Vec<Vec<f32>>, DaemonError> {
        if !self.is_available() {
            return Err(DaemonError::Unavailable("daemon not available".to_string()));
        }
        let (resp_tx, resp_rx) = mpsc::channel();
        self.send_request(
            DaemonRequest::EmbedBatch {
                count: texts.len(),
                resp: resp_tx,
            },
            resp_rx,
        )
    }

    fn rerank(
        &self,
        _query: &str,
        documents: &[&str],
        _request_id: &str,
    ) -> Result<Vec<f32>, DaemonError> {
        if !self.is_available() {
            return Err(DaemonError::Unavailable("daemon not available".to_string()));
        }
        let (resp_tx, resp_rx) = mpsc::channel();
        self.send_request(
            DaemonRequest::Rerank {
                count: documents.len(),
                resp: resp_tx,
            },
            resp_rx,
        )
    }
}

struct DaemonHarness {
    client: Arc<ChannelDaemonClient>,
    _mode: Arc<Mutex<DaemonMode>>,
    available: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    tx: mpsc::Sender<DaemonRequest>,
    handle: Option<thread::JoinHandle<()>>,
}

impl DaemonHarness {
    fn new(mode: DaemonMode) -> Self {
        let (tx, rx) = mpsc::channel();
        let mode = Arc::new(Mutex::new(mode));
        let available = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicUsize::new(0));
        let mode_clone = Arc::clone(&mode);
        let handle = thread::spawn(move || {
            loop {
                match rx.recv() {
                    Ok(DaemonRequest::Shutdown) | Err(_) => break,
                    Ok(DaemonRequest::Embed { resp }) => {
                        respond(mode_clone.as_ref(), resp, vec![2.0; 4]);
                    }
                    Ok(DaemonRequest::EmbedBatch { count, resp }) => {
                        respond(mode_clone.as_ref(), resp, vec![vec![2.0; 4]; count]);
                    }
                    Ok(DaemonRequest::Rerank { count, resp }) => {
                        respond(mode_clone.as_ref(), resp, vec![1.0; count]);
                    }
                }
            }
        });

        let client = Arc::new(ChannelDaemonClient {
            id: "channel-daemon".to_string(),
            available: Arc::clone(&available),
            calls: Arc::clone(&calls),
            tx: tx.clone(),
            timeout: Duration::from_millis(25),
        });

        Self {
            client,
            _mode: mode,
            available,
            calls,
            tx,
            handle: Some(handle),
        }
    }

    fn client(&self) -> Arc<ChannelDaemonClient> {
        Arc::clone(&self.client)
    }

    fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Relaxed);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        let _ = self.tx.send(DaemonRequest::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn respond<T: Send + 'static>(
    mode: &Mutex<DaemonMode>,
    resp: mpsc::Sender<Result<T, DaemonError>>,
    ok_value: T,
) {
    match *mode.lock() {
        DaemonMode::Ok => {
            let _ = resp.send(Ok(ok_value));
        }
        DaemonMode::Drop => {
            // Simulate a crash by dropping the response channel.
        }
        DaemonMode::Timeout => {
            // Exceed ChannelDaemonClient's 25 ms response budget so the caller
            // observes the real timeout path before this late response arrives.
            thread::sleep(Duration::from_millis(50));
            let _ = resp.send(Ok(ok_value));
        }
    }
}

struct StaticEmbedder {
    dim: usize,
    value: f32,
}

impl Embedder for StaticEmbedder {
    fn embed_sync(&self, _text: &str) -> EmbedderResult<Vec<f32>> {
        Ok(vec![self.value; self.dim])
    }

    fn embed_batch_sync(&self, texts: &[&str]) -> EmbedderResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![self.value; self.dim]).collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }

    fn id(&self) -> &str {
        "static-embedder"
    }

    fn is_semantic(&self) -> bool {
        true
    }

    fn category(&self) -> ModelCategory {
        ModelCategory::StaticEmbedder
    }
}

struct StaticReranker {
    value: f32,
}

impl Reranker for StaticReranker {
    fn rerank_sync(
        &self,
        _query: &str,
        documents: &[RerankDocument],
    ) -> RerankerResult<Vec<RerankScore>> {
        Ok(documents
            .iter()
            .enumerate()
            .map(|(i, doc)| RerankScore {
                doc_id: doc.doc_id.clone(),
                score: self.value,
                original_rank: i,
                raw_logit: None,
            })
            .collect())
    }

    fn id(&self) -> &str {
        "static-reranker"
    }

    fn model_name(&self) -> &str {
        "static-reranker"
    }

    fn is_available(&self) -> bool {
        true
    }
}

#[test]
fn daemon_integration_embed_and_rerank() {
    let harness = DaemonHarness::new(DaemonMode::Ok);
    let daemon = harness.client();

    let fallback = Arc::new(StaticEmbedder { dim: 4, value: 1.0 });
    let cfg = DaemonRetryConfig {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
        jitter_pct: 0.0,
    };

    let error = match DaemonFallbackEmbedder::new(daemon.clone(), fallback, cfg.clone()) {
        Err(error) => error,
        Ok(_) => unreachable!("legacy raw daemon composition must fail closed"),
    };
    assert!(matches!(error, SearchError::UnverifiableRemoteSpace { .. }));
    assert_eq!(
        harness.calls(),
        0,
        "constructor rejection must happen before any unverified daemon request"
    );

    let reranker_fallback = Arc::new(StaticReranker { value: 0.5 });
    let reranker = DaemonFallbackReranker::new(daemon, Some(reranker_fallback), cfg);
    let scores = rerank_texts(&reranker, "q", &["a", "b"]).unwrap();
    assert_eq!(scores, vec![1.0, 1.0]);

    assert_eq!(harness.calls(), 1);
}

#[test]
fn raw_daemon_is_available_only_through_explicit_transient_assumed_mode() {
    let harness = DaemonHarness::new(DaemonMode::Ok);
    let daemon = harness.client();

    let assumed = AssumedDaemonClient::new(daemon);
    let batch = assumed
        .embed_transient("transient exploration only")
        .expect("raw daemon remains available in explicit assumed mode");
    assert_eq!(batch.trust_level(), DaemonTrustLevelV1::AssumedRemote);
    assert_eq!(batch.vectors(), &[vec![2.0; 4]]);
    assert_eq!(harness.calls(), 1);

    let rendered = format!("{assumed:?} {batch:?}");
    assert!(rendered.contains("AssumedRemote"));
    assert!(rendered.contains("<redacted>"));
    assert!(
        !rendered.contains("[2.0, 2.0, 2.0, 2.0]"),
        "transient vectors must stay redacted from diagnostics"
    );
}

#[test]
fn failed_raw_daemon_never_becomes_an_index_compatible_fallback_embedder() {
    let harness = DaemonHarness::new(DaemonMode::Drop);
    let daemon = harness.client();

    let fallback = Arc::new(StaticEmbedder { dim: 4, value: 1.0 });
    let error =
        match DaemonFallbackEmbedder::new(daemon.clone(), fallback, DaemonRetryConfig::default()) {
            Err(error) => error,
            Ok(_) => unreachable!("unattested daemon must not implement indexed embedding"),
        };
    assert!(matches!(error, SearchError::UnverifiableRemoteSpace { .. }));
    assert_eq!(harness.calls(), 0);

    let assumed = AssumedDaemonClient::new(daemon);
    assert!(
        assumed.embed_transient("raw failure").is_err(),
        "assumed mode must expose daemon failure rather than relabel local vectors"
    );
    assert_eq!(harness.calls(), 1);
    harness.set_available(false);
    assert!(assumed.embed_transient("unavailable raw failure").is_err());
    assert_eq!(
        harness.calls(),
        1,
        "known-unavailable raw daemon must fail before a transport request"
    );
}

#[test]
fn daemon_reranker_crash_falls_back_without_losing_results() {
    let harness = DaemonHarness::new(DaemonMode::Drop);
    let daemon = harness.client();
    let fallback = Arc::new(StaticReranker { value: 0.5 });
    let cfg = DaemonRetryConfig {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
        jitter_pct: 0.0,
    };

    let reranker = DaemonFallbackReranker::new(daemon, Some(fallback), cfg);
    let first = rerank_texts(&reranker, "q", &["doc"]).expect("fallback rerank after daemon crash");
    assert_eq!(first, vec![0.5]);
    assert_eq!(harness.calls(), 1);

    harness.set_available(false);
    let second =
        rerank_texts(&reranker, "q", &["doc"]).expect("fallback rerank while daemon unavailable");
    assert_eq!(second, vec![0.5]);
    assert_eq!(
        harness.calls(),
        1,
        "known-unavailable daemon must not receive another transport request"
    );
}

#[test]
fn daemon_reranker_timeout_backoff_with_jitter_retries_after_window() {
    let harness = DaemonHarness::new(DaemonMode::Timeout);
    let daemon = harness.client();
    let fallback = Arc::new(StaticReranker { value: 0.5 });
    let cfg = DaemonRetryConfig {
        max_attempts: 1,
        base_delay: Duration::from_millis(20),
        max_delay: Duration::from_millis(50),
        jitter_pct: 0.5,
    };

    let reranker = DaemonFallbackReranker::new(daemon, Some(fallback), cfg.clone());
    assert_eq!(
        rerank_texts(&reranker, "q", &["first"]).expect("first fallback rerank"),
        vec![0.5]
    );
    let calls_after_first = harness.calls();

    assert_eq!(
        rerank_texts(&reranker, "q", &["second"]).expect("backoff fallback rerank"),
        vec![0.5]
    );
    let calls_after_second = harness.calls();
    assert_eq!(
        calls_after_first, calls_after_second,
        "backoff window must suppress immediate retries"
    );

    let max_jitter_ms = (cfg.base_delay.as_millis() as f64 * (1.0 + cfg.jitter_pct)).ceil();
    std::thread::sleep(Duration::from_millis(max_jitter_ms as u64 + 10));

    assert_eq!(
        rerank_texts(&reranker, "q", &["third"]).expect("post-backoff fallback rerank"),
        vec![0.5]
    );
    assert!(
        harness.calls() > calls_after_second,
        "daemon must be retried after the bounded backoff window"
    );
}
