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
use frankensearch::ModelCategory;
use parking_lot::Mutex;

#[derive(Clone, Copy)]
enum DaemonMode {
    Ok,
    Drop,
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

    let embedder = DaemonFallbackEmbedder::new(daemon.clone(), fallback, cfg.clone());
    let embed = embedder.embed_sync("hello").unwrap();
    assert_eq!(embed[0], 2.0);

    let reranker_fallback = Arc::new(StaticReranker { value: 0.5 });
    let reranker = DaemonFallbackReranker::new(daemon, Some(reranker_fallback), cfg);
    let scores = rerank_texts(&reranker, "q", &["a", "b"]).unwrap();
    assert_eq!(scores, vec![1.0, 1.0]);

    assert_eq!(harness.calls(), 2);
}

#[test]
fn daemon_integration_crash_falls_back() {
    let harness = DaemonHarness::new(DaemonMode::Drop);
    let daemon = harness.client();

    let fallback = Arc::new(StaticEmbedder { dim: 4, value: 1.0 });
    let cfg = DaemonRetryConfig {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(5),
        jitter_pct: 0.0,
    };

    let embedder = DaemonFallbackEmbedder::new(daemon.clone(), fallback, cfg.clone());
    let first = embedder.embed_sync("hello").unwrap();
    assert_eq!(first[0], 1.0);
    assert_eq!(harness.calls(), 1);

    harness.set_available(false);
    let second = embedder.embed_sync("hello again").unwrap();
    assert_eq!(second[0], 1.0);
    assert_eq!(harness.calls(), 1);

    let reranker_fallback = Arc::new(StaticReranker { value: 0.5 });
    let reranker = DaemonFallbackReranker::new(daemon, Some(reranker_fallback), cfg);
    let scores = rerank_texts(&reranker, "q", &["doc"]).unwrap();
    assert_eq!(scores, vec![0.5]);
}

#[test]
fn daemon_integration_timeout_backoff_with_jitter() {
    let harness = DaemonHarness::new(DaemonMode::Drop);
    let daemon = harness.client();

    let fallback = Arc::new(StaticEmbedder { dim: 4, value: 1.0 });
    let cfg = DaemonRetryConfig {
        max_attempts: 1,
        base_delay: Duration::from_millis(20),
        max_delay: Duration::from_millis(50),
        jitter_pct: 0.5,
    };

    let embedder = DaemonFallbackEmbedder::new(daemon.clone(), fallback, cfg.clone());
    let _ = embedder.embed_sync("first").unwrap();
    let calls_after_first = harness.calls();

    let _ = embedder.embed_sync("second").unwrap();
    let calls_after_second = harness.calls();
    assert_eq!(calls_after_first, calls_after_second);

    let max_jitter_ms = (cfg.base_delay.as_millis() as f64 * (1.0 + cfg.jitter_pct)).ceil();
    std::thread::sleep(Duration::from_millis(max_jitter_ms as u64 + 10));

    let _ = embedder.embed_sync("third").unwrap();
    let calls_after_third = harness.calls();
    assert!(calls_after_third > calls_after_second);
}
