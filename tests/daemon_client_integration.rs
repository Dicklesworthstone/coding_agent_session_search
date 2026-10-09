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
        request_finished: mpsc::Receiver<()>,
    },
    EmbedBatch {
        count: usize,
        resp: mpsc::Sender<Result<Vec<Vec<f32>>, DaemonError>>,
        request_finished: mpsc::Receiver<()>,
    },
    Rerank {
        count: usize,
        resp: mpsc::Sender<Result<Vec<f32>, DaemonError>>,
        request_finished: mpsc::Receiver<()>,
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
        request: impl FnOnce(mpsc::Receiver<()>) -> DaemonRequest,
        resp_rx: mpsc::Receiver<Result<T, DaemonError>>,
    ) -> Result<T, DaemonError> {
        let (_request_active, request_finished) = mpsc::channel();
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.tx.send(request(request_finished)).is_err() {
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
        self.send_request(
            |request_finished| DaemonRequest::Embed {
                resp: resp_tx,
                request_finished,
            },
            resp_rx,
        )
    }

    fn embed_batch(&self, texts: &[&str], _request_id: &str) -> Result<Vec<Vec<f32>>, DaemonError> {
        if !self.is_available() {
            return Err(DaemonError::Unavailable("daemon not available".to_string()));
        }
        let (resp_tx, resp_rx) = mpsc::channel();
        self.send_request(
            |request_finished| DaemonRequest::EmbedBatch {
                count: texts.len(),
                resp: resp_tx,
                request_finished,
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
            |request_finished| DaemonRequest::Rerank {
                count: documents.len(),
                resp: resp_tx,
                request_finished,
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
                    Ok(DaemonRequest::Embed {
                        resp,
                        request_finished,
                    }) => {
                        respond(mode_clone.as_ref(), resp, request_finished, vec![2.0; 4]);
                    }
                    Ok(DaemonRequest::EmbedBatch {
                        count,
                        resp,
                        request_finished,
                    }) => {
                        respond(
                            mode_clone.as_ref(),
                            resp,
                            request_finished,
                            vec![vec![2.0; 4]; count],
                        );
                    }
                    Ok(DaemonRequest::Rerank {
                        count,
                        resp,
                        request_finished,
                    }) => {
                        respond(
                            mode_clone.as_ref(),
                            resp,
                            request_finished,
                            vec![1.0; count],
                        );
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
    request_finished: mpsc::Receiver<()>,
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
            // Hold the response channel open until the caller's real timeout
            // finishes. A delayed success can win recv_timeout after the caller
            // is descheduled, even if it arrives beyond the nominal deadline.
            let _ = request_finished.recv();
            drop(resp);
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

#[cfg(unix)]
mod native_daemon_process {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    use coding_agent_search::daemon::{DaemonClientConfig, UdsDaemonClient};
    use coding_agent_search::search::fastembed_embedder::FastEmbedder;
    use coding_agent_search::search::model_download::{
        ModelManifest, compute_sha256, model_file_path,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    struct NativeDaemon {
        child: Child,
        stdout_path: PathBuf,
        stderr_path: PathBuf,
    }

    impl NativeDaemon {
        fn wait_ready(&mut self, config: &DaemonClientConfig) -> TestResult {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                if let Some(status) = self.child.try_wait()? {
                    return Err(format!("native daemon exited before readiness: {status}").into());
                }
                let probe = UdsDaemonClient::new(DaemonClientConfig {
                    request_timeout: Duration::from_secs(1),
                    ..config.clone()
                });
                if probe.connect().is_ok()
                    && let Ok(health) = probe.health()
                    && health.ready
                {
                    assert_eq!(
                        health.version,
                        coding_agent_search::daemon::protocol::PROTOCOL_VERSION
                    );
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err("native daemon did not become ready within 120 seconds".into());
                }
                thread::sleep(Duration::from_millis(100));
            }
        }

        fn shutdown(&mut self, client: &UdsDaemonClient) -> TestResult {
            client.shutdown()?;
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if let Some(status) = self.child.try_wait()? {
                    assert!(status.success(), "native daemon shutdown failed: {status}");
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err("native daemon did not shut down within 20 seconds".into());
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }

    impl Drop for NativeDaemon {
        fn drop(&mut self) {
            match self.child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Err(error) = self.child.kill() {
                        eprintln!("could not stop owned native daemon child: {error}");
                    }
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        match self.child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) if Instant::now() < deadline => {
                                thread::sleep(Duration::from_millis(50));
                            }
                            result => {
                                eprintln!("owned daemon child was not reaped: {result:?}");
                                break;
                            }
                        }
                    }
                }
                Err(error) => eprintln!("could not inspect owned native daemon child: {error}"),
            }
            for (label, path) in [("stdout", &self.stdout_path), ("stderr", &self.stderr_path)] {
                match fs::read_to_string(path) {
                    Ok(contents) => eprintln!("native daemon {label}:\n{contents}"),
                    Err(error) => eprintln!("could not read native daemon {label}: {error}"),
                }
            }
        }
    }

    fn assert_vector_bits(actual: &[f32], expected: &[f32], context: &str) {
        assert_eq!(actual.len(), 384, "{context}: native dimension");
        assert!(actual.iter().all(|value| value.is_finite()), "{context}");
        let actual_bits: Vec<_> = actual.iter().map(|value| value.to_bits()).collect();
        let expected_bits: Vec<_> = expected.iter().map(|value| value.to_bits()).collect();
        assert_eq!(actual_bits, expected_bits, "{context}: exact native output");
    }

    fn verify_supplied_bundle(source: &Path, manifest: &ModelManifest) -> TestResult {
        assert_eq!(
            manifest.files.len(),
            5,
            "the attested bundle has five files"
        );
        for file in &manifest.files {
            let path = model_file_path(source, file)
                .ok_or_else(|| format!("missing supplied native asset {}", file.name))?;
            assert_eq!(fs::metadata(&path)?.len(), file.size, "{}", file.name);
            assert_eq!(compute_sha256(&path)?, file.sha256, "{}", file.name);
        }
        Ok(())
    }

    fn run_native_daemon_case(model: &str, bundle_env: &str) -> TestResult {
        let source = PathBuf::from(dotenvy::var(bundle_env).map_err(|error| {
            format!("{bundle_env} must name an existing verified bundle; no downloads: {error}")
        })?)
        .canonicalize()?;
        let manifest = ModelManifest::for_embedder(model).ok_or("unknown native model")?;
        verify_supplied_bundle(&source, &manifest)?;

        // A short private root also fits macOS sockaddr_un, regardless of the
        // gate's TMPDIR. Both models use real supplied files, never acquisition.
        let temp = tempfile::Builder::new()
            .prefix("cass-native-daemon-")
            .tempdir_in("/tmp")?;
        let home = temp.path().join("home");
        let data = temp.path().join("data");
        fs::create_dir_all(&home)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(home.join(".env"))?;
        let managed = FastEmbedder::model_dir_for(&data, model).ok_or("model directory mapping")?;
        fs::create_dir_all(&managed)?;
        for file in &manifest.files {
            let original = model_file_path(&source, file).ok_or("verified asset disappeared")?;
            assert_eq!(
                fs::copy(original, managed.join(file.local_name()))?,
                file.size,
                "copy complete managed native asset {}",
                file.name
            );
        }
        verify_supplied_bundle(&managed, &manifest)?;

        let config = FastEmbedder::config_for(model).ok_or("native model configuration")?;
        let expected_id = config.embedder_id.clone();
        let wrong_id = if model == "minilm" {
            "multilingual-minilm-384"
        } else {
            "minilm-384"
        };
        let texts = [
            "A database transaction rolls back when validation fails.",
            "数据库事务失败时回滚。データベースの復旧を確認します。",
            "Unicode café λ: preserve source provenance and message order.",
            "A database transaction rolls back when validation fails.",
        ];
        let direct = FastEmbedder::load_with_config(&managed, config)?;
        assert!(direct.is_semantic());
        assert_eq!(direct.category(), ModelCategory::TransformerEmbedder);
        assert_eq!(direct.id(), expected_id);
        let expected_identity = direct.identity()?.clone();
        expected_identity.validate()?;
        let expected_vectors = direct.embed_batch_sync(&texts)?;
        assert_eq!(expected_vectors.len(), texts.len());
        for (index, text) in texts.iter().enumerate() {
            assert_vector_bits(
                &direct.embed_sync(text)?,
                &expected_vectors[index],
                &format!("direct single/batch {model} input {index}"),
            );
        }
        assert_ne!(expected_vectors[0], expected_vectors[1]);
        assert_vector_bits(
            &expected_vectors[0],
            &expected_vectors[3],
            "duplicate input",
        );
        // Do not retain a second native model while the child loads its own.
        drop(direct);

        let binary = PathBuf::from(assert_cmd::cargo::cargo_bin!("cass"));
        let binary_sha = compute_sha256(&binary)?;
        let socket = temp.path().join("daemon.sock");
        let stdout_path = temp.path().join("daemon.stdout");
        let stderr_path = temp.path().join("daemon.stderr");
        let mut command = Command::new(&binary);
        command.env_clear().current_dir(&home);
        for key in ["PATH", "SystemRoot", "WINDIR"] {
            if let Ok(value) = dotenvy::var(key) {
                command.env(key, value);
            }
        }
        command
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("CODEX_HOME", home.join(".codex"))
            .env("CASS_DATA_DIR", &data)
            .env("CASS_SEMANTIC_EMBEDDER", model)
            .env("CASS_DAEMON_INDEX_INTERVAL_SECS", "0")
            .env("CASS_AUTO_REFRESH", "0")
            .env("CASS_RESPONSIVENESS_DISABLE", "1")
            .env("TUI_HEADLESS", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("RUST_MIN_STACK", "134217728")
            .args(["daemon", "--socket"])
            .arg(&socket)
            .args(["--data-dir"])
            .arg(&data)
            .args(["--idle-timeout", "300", "--max-connections", "4"])
            .stdin(Stdio::null())
            .stdout(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&stdout_path)?,
            )
            .stderr(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&stderr_path)?,
            );
        let mut daemon = NativeDaemon {
            child: command.spawn()?,
            stdout_path,
            stderr_path,
        };
        let client_config = DaemonClientConfig {
            socket_path: socket,
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(60),
            auto_spawn: false,
            data_dir: Some(data.clone()),
            expected_embedder_id: Some(expected_id.clone()),
            ..Default::default()
        };
        daemon.wait_ready(&client_config)?;
        let client = Arc::new(UdsDaemonClient::new(client_config.clone()));
        client.connect()?;
        let (connection, verifier) = client.attestation_channel(&data)?;
        assert_eq!(connection.embedding_identity, expected_identity);
        assert_eq!(
            connection.model_category,
            ModelCategory::TransformerEmbedder
        );
        assert_eq!(
            connection.executable_fingerprint,
            frankensearch::daemon_executable_fingerprint(&hex::decode(&binary_sha)?)
        );
        assert!(connection.generation > 0);
        let transport: Arc<dyn DaemonClient> = client.clone();
        let retry = DaemonRetryConfig {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            jitter_pct: 0.0,
        };
        let verified = DaemonFallbackEmbedder::new_verified(
            transport.clone(),
            None,
            retry.clone(),
            connection.clone(),
            verifier,
        )?;
        assert_eq!(verified.trust_level(), DaemonTrustLevelV1::VerifiedRemote);
        assert_eq!(verified.identity()?, &expected_identity);
        assert_eq!(verified.connection_identity(), &connection);
        let remote_vectors = verified.embed_batch_sync(&texts)?;
        assert_eq!(remote_vectors.len(), texts.len());
        for (index, text) in texts.iter().enumerate() {
            assert_vector_bits(
                &remote_vectors[index],
                &expected_vectors[index],
                &format!("attested batch {model} input {index}"),
            );
            assert_vector_bits(
                &verified.embed_sync(text)?,
                &expected_vectors[index],
                &format!("attested single {model} input {index}"),
            );
        }

        // Reject a real same-width native response for the other model. The
        // daemon really computes the vector; only its incompatible identity is
        // refused. No fabricated response or local fallback participates.
        let wrong_client = UdsDaemonClient::new(DaemonClientConfig {
            expected_embedder_id: Some(wrong_id.to_owned()),
            ..client_config
        });
        wrong_client.connect()?;
        let error = wrong_client
            .embed(texts[0], "native-wrong-space")
            .expect_err("same dimension must not admit a different native model");
        assert!(matches!(error, DaemonError::InvalidInput(_)));
        assert!(error.to_string().contains(&format!("expected {wrong_id}")));
        assert!(
            error
                .to_string()
                .contains(&format!("received {expected_id}"))
        );
        drop(wrong_client);

        let (mut wrong_connection, verifier) = client.attestation_channel(&data)?;
        let first = if wrong_connection.executable_fingerprint.starts_with('0') {
            "1"
        } else {
            "0"
        };
        wrong_connection
            .executable_fingerprint
            .replace_range(..1, first);
        wrong_connection.validate()?;
        let error = DaemonFallbackEmbedder::new_verified(
            transport,
            None,
            retry,
            wrong_connection,
            verifier,
        )
        .err()
        .ok_or("changed executable identity was incorrectly authenticated")?;
        assert!(matches!(error, SearchError::UnverifiableRemoteSpace { .. }));
        assert_vector_bits(
            &verified.embed_sync(texts[2])?,
            &expected_vectors[2],
            "valid authenticated client remains usable after refusals",
        );
        drop(verified);
        daemon.shutdown(&client)?;
        verify_supplied_bundle(&source, &manifest)?;
        verify_supplied_bundle(&managed, &manifest)?;
        eprintln!(
            "native daemon acceptance model={model} manifest_revision={} binary_sha={binary_sha} \
             connection={} generation={} inputs={} exact_single_batch=true no_local_fallback=true",
            manifest.revision,
            connection.fingerprint(),
            connection.generation,
            texts.len()
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires CASS_NATIVE_REUSE_MODEL_DIR with the verified five-file MiniLM bundle"]
    fn gh467_native_daemon_attests_real_minilm_single_and_batch_vectors() -> TestResult {
        run_native_daemon_case("minilm", "CASS_NATIVE_REUSE_MODEL_DIR")
    }

    #[test]
    #[ignore = "requires CASS_NATIVE_MULTILINGUAL_MODEL_DIR with the separate verified multilingual bundle"]
    fn gh467_native_daemon_attests_real_multilingual_single_and_batch_vectors() -> TestResult {
        run_native_daemon_case("multilingual-minilm", "CASS_NATIVE_MULTILINGUAL_MODEL_DIR")
    }
}

#[cfg(unix)]
mod indexer_process_coexistence {
    use std::fs::{self, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use coding_agent_search::daemon::worker::EmbeddingJobConfig;
    use coding_agent_search::daemon::{DaemonClientConfig, UdsDaemonClient};
    use coding_agent_search::franken_sync::{Connection, compat::RowExt};
    use coding_agent_search::indexer::background_refresh;
    use coding_agent_search::search::vector_index::{VectorIndex, vector_index_path};
    use coding_agent_search::storage::sqlite::CURRENT_SCHEMA_VERSION;

    struct OwnedProcess {
        child: Child,
        stdout: PathBuf,
        stderr: PathBuf,
    }

    impl OwnedProcess {
        fn diagnostics(&self) -> String {
            format!(
                "stdout={}\nstderr={}",
                fs::read_to_string(&self.stdout).unwrap_or_default(),
                fs::read_to_string(&self.stderr).unwrap_or_default(),
            )
        }

        fn assert_running(&mut self) -> Result<()> {
            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "owned child exited early ({status}): {}",
                    self.diagnostics()
                );
            }
            Ok(())
        }

        fn wait(&mut self, timeout: Duration) -> Result<ExitStatus> {
            let deadline = Instant::now() + timeout;
            loop {
                if let Some(status) = self.child.try_wait()? {
                    return Ok(status);
                }
                if Instant::now() >= deadline {
                    bail!("owned child exceeded {timeout:?}: {}", self.diagnostics());
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        fn until(&mut self, condition: impl Fn() -> bool) -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(45);
            loop {
                self.assert_running()?;
                if condition() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!(
                        "owned child did not reach rendezvous: {}",
                        self.diagnostics()
                    );
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for OwnedProcess {
        fn drop(&mut self) {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            let _ = self.child.kill();
            if let Err(error) = self.wait(Duration::from_secs(5)) {
                eprintln!("could not reap owned coexistence child: {error:#}");
            }
        }
    }

    /// The periodic child belongs to the daemon's reaper, so this test cannot
    /// waitpid it directly. Keep its unique archive identity for bounded
    /// cleanup without ever signalling an unrelated process after PID reuse.
    struct PeriodicIndexGuard {
        data: PathBuf,
        db: PathBuf,
        release: PathBuf,
        finished: bool,
    }

    impl PeriodicIndexGuard {
        fn owned_pid(&self) -> Result<Option<u32>> {
            let Some(state) = background_refresh::load_state(&self.data) else {
                return Ok(None);
            };
            if state.last_reason != "daemon-periodic" || state.last_pid == 0 {
                return Ok(None);
            }
            let output = Command::new("/bin/ps")
                .args(["-ww", "-p", &state.last_pid.to_string(), "-o", "args="])
                .output()?;
            let command = String::from_utf8_lossy(&output.stdout);
            Ok((output.status.success()
                && command.contains(self.db.to_string_lossy().as_ref())
                && command.contains("--background")
                && command.contains("index"))
            .then_some(state.last_pid))
        }

        fn wait(&mut self, timeout: Duration) -> Result<()> {
            let deadline = Instant::now() + timeout;
            while self.owned_pid()?.is_some() {
                if Instant::now() >= deadline {
                    bail!(
                        "periodic index exceeded {timeout:?}: {}",
                        fs::read_to_string(background_refresh::log_path(&self.data))
                            .unwrap_or_default()
                    );
                }
                thread::sleep(Duration::from_millis(20));
            }
            self.finished = true;
            Ok(())
        }
    }

    impl Drop for PeriodicIndexGuard {
        fn drop(&mut self) {
            if self.finished {
                return;
            }
            let _ = fs::write(&self.release, b"release");
            if self.wait(Duration::from_secs(5)).is_err()
                && let Ok(Some(pid)) = self.owned_pid()
            {
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
                if let Err(error) = self.wait(Duration::from_secs(5)) {
                    eprintln!("could not drain owned periodic index: {error:#}");
                }
            }
        }
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        home: PathBuf,
        data: PathBuf,
        db: PathBuf,
        socket: PathBuf,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            // Fits macOS sockaddr_un even when the gate's TMPDIR is long.
            let temp = tempfile::Builder::new()
                .prefix("cass515-")
                .tempdir_in("/tmp")?;
            let home = temp.path().join("home");
            let data = temp.path().join("data");
            fs::create_dir_all(&home)?;
            fs::create_dir_all(&data)?;
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(home.join(".env"))?;
            // Deliberately outside data: archive ownership must not depend on
            // where a daemon or standalone indexer keeps its search assets.
            let db = temp.path().join("custom-archive.db");
            let socket = temp.path().join("daemon.sock");
            Ok(Self {
                _temp: temp,
                home,
                data,
                db,
                socket,
            })
        }

        fn command(&self) -> Command {
            let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
            command
                .env_clear()
                .current_dir(&self.home)
                .stdin(Stdio::null());
            for key in ["PATH", "SystemRoot", "WINDIR"] {
                if let Ok(value) = dotenvy::var(key) {
                    command.env(key, value);
                }
            }
            command
                .env("HOME", &self.home)
                .env("USERPROFILE", &self.home)
                .env("XDG_CONFIG_HOME", self.home.join(".config"))
                .env("XDG_DATA_HOME", self.home.join(".local/share"))
                .env("CLAUDE_CONFIG_DIR", self.home.join(".claude"))
                .env("CODEX_HOME", self.home.join(".codex"))
                .env("CASS_DATA_DIR", &self.data)
                .env("CASS_AUTO_REFRESH", "0")
                .env("CASS_DAEMON_INDEX_INTERVAL_SECS", "0")
                .env("CASS_RESPONSIVENESS_DISABLE", "1")
                .env("CASS_IGNORE_SOURCES_CONFIG", "1")
                .env("TUI_HEADLESS", "1")
                .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
                .env("RUST_MIN_STACK", "134217728")
                .env("RUST_LOG", "coding_agent_search::daemon::worker=debug");
            command
        }

        fn spawn(&self, label: &str, mut command: Command) -> Result<OwnedProcess> {
            let stdout = self.data.join(format!("{label}.stdout"));
            let stderr = self.data.join(format!("{label}.stderr"));
            command
                .stdout(
                    OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&stdout)?,
                )
                .stderr(
                    OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&stderr)?,
                );
            Ok(OwnedProcess {
                child: command.spawn()?,
                stdout,
                stderr,
            })
        }

        fn index_command(&self, extra: &[&str]) -> Command {
            self.index_command_for(&self.data, &self.db, extra)
        }

        fn index_command_for(&self, data: &Path, db: &Path, extra: &[&str]) -> Command {
            let mut command = self.command();
            command
                .args(["index", "--json", "--no-progress-events", "--data-dir"])
                .arg(data)
                .arg("--db")
                .arg(db)
                .args(extra);
            command
        }

        fn index(&self, label: &str, extra: &[&str]) -> Result<()> {
            let mut indexer = self.spawn(label, self.index_command(extra))?;
            let status = indexer.wait(Duration::from_secs(60))?;
            assert!(status.success(), "index {label}: {}", indexer.diagnostics());
            Ok(())
        }

        fn wait_index_owner(
            &self,
            process: &mut OwnedProcess,
            pid: u32,
            job_kind: &str,
            phase: Option<&str>,
        ) -> Result<()> {
            let expected_pid = pid.to_string();
            process
                .until(|| {
                    // The owner must rewrite this inode in place to retain its
                    // OS lock. A concurrent reader can see a truncated payload.
                    let Ok(metadata) = fs::read_to_string(self.data.join("index-run.lock")) else {
                        return false;
                    };
                    let value =
                        |prefix: &str| metadata.lines().find_map(|line| line.strip_prefix(prefix));
                    value("pid=") == Some(expected_pid.as_str())
                        && value("job_kind=") == Some(job_kind)
                        && value("phase=").is_some_and(|observed| {
                            !observed.is_empty() && phase.is_none_or(|phase| observed == phase)
                        })
                        && value("job_id=").is_some_and(|value| !value.is_empty())
                        && value("started_at_ms=").is_some_and(|value| value.parse::<i64>().is_ok())
                        && value("updated_at_ms=").is_some_and(|value| value.parse::<i64>().is_ok())
                })
                .with_context(|| {
                    format!(
                        "waiting for index-run owner pid={pid} job_kind={job_kind} phase={phase:?}"
                    )
                })
        }

        fn assert_index_busy(&self, label: &str) -> Result<()> {
            self.assert_index_command_busy(label, self.index_command(&[]))
        }

        fn assert_index_command_busy(&self, label: &str, command: Command) -> Result<()> {
            let mut indexer = self.spawn(label, command)?;
            assert_eq!(
                indexer.wait(Duration::from_secs(10))?.code(),
                Some(7),
                "expected index-busy for {label}: {}",
                indexer.diagnostics()
            );
            // Index emits its final structured result on stdout and marks the
            // error as already reported, so main must not emit it again.
            let result: serde_json::Value = serde_json::from_slice(&fs::read(&indexer.stdout)?)
                .with_context(|| format!("index-busy result: {}", indexer.diagnostics()))?;
            assert_eq!(result["success"], false, "{}", indexer.diagnostics());
            assert_eq!(result["code"], 7, "{}", indexer.diagnostics());
            assert_eq!(result["kind"], "index-busy", "{}", indexer.diagnostics());
            Ok(())
        }

        fn daemon(&self, pause_snapshot: bool) -> Result<(OwnedProcess, UdsDaemonClient)> {
            self.daemon_configured(|command| {
                if pause_snapshot {
                    command
                        .env(
                            "CASS_TEST_EMBEDDING_SNAPSHOT_READY",
                            self.data.join("embedding-ready.json"),
                        )
                        .env(
                            "CASS_TEST_EMBEDDING_SNAPSHOT_RELEASE",
                            self.data.join("embedding-release"),
                        );
                }
            })
        }

        fn daemon_configured(
            &self,
            configure: impl FnOnce(&mut Command),
        ) -> Result<(OwnedProcess, UdsDaemonClient)> {
            let mut command = self.command();
            command
                .args(["daemon", "--idle-timeout", "0", "--data-dir"])
                .arg(&self.data)
                .arg("--socket")
                .arg(&self.socket);
            configure(&mut command);
            let mut daemon = self.spawn("daemon", command)?;
            let client = UdsDaemonClient::new(DaemonClientConfig {
                socket_path: self.socket.clone(),
                data_dir: Some(self.data.clone()),
                auto_spawn: false,
                connect_timeout: Duration::from_secs(1),
                request_timeout: Duration::from_secs(2),
                ..Default::default()
            });
            daemon.until(|| client.connect().is_ok() && client.health().is_ok())?;
            Ok((daemon, client))
        }

        fn seed(&self, id: usize) -> Result<()> {
            let sessions = self.home.join(".codex/sessions/2026/10/08");
            fs::create_dir_all(&sessions)?;
            let content = format!("coexistenceprobe archive message number {id}");
            let lines = [
                serde_json::json!({"timestamp":"2026-10-08T12:00:00Z", "type":"session_meta",
                    "payload":{"id":format!("coexistence-{id}"), "cwd":self.home, "cli_version":"0.42.0"}}),
                serde_json::json!({"timestamp":"2026-10-08T12:00:01Z", "type":"response_item",
                    "payload":{"type":"message", "role":"user", "content":[{"type":"input_text", "text":content}]}}),
            ];
            let body = lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            fs::write(
                sessions.join(format!("rollout-coexistence-{id}.jsonl")),
                body,
            )?;
            Ok(())
        }

        fn assert_archive(&self, count: usize) -> Result<()> {
            let connection = Connection::open(self.db.to_string_lossy())?;
            let rows = connection.query("SELECT content FROM messages ORDER BY content")?;
            let actual = rows
                .iter()
                .map(|row| row.get_typed::<String>(0))
                .collect::<Result<Vec<_>, _>>()?;
            let mut expected = (0..count)
                .map(|id| format!("coexistenceprobe archive message number {id}"))
                .collect::<Vec<_>>();
            expected.sort();
            assert_eq!(
                actual, expected,
                "canonical messages must survive and never duplicate"
            );
            assert_eq!(
                connection
                    .query_row("SELECT MAX(version) FROM _schema_migrations")?
                    .get_typed::<i64>(0)?,
                CURRENT_SCHEMA_VERSION
            );
            for row in connection.query("PRAGMA integrity_check")? {
                assert_eq!(row.get_typed::<String>(0)?, "ok");
            }
            assert!(connection.query("PRAGMA foreign_key_check")?.is_empty());
            Ok(())
        }

        fn idempotency_cache_row_count(&self) -> Result<Option<i64>> {
            use coding_agent_search::franken_sync::compat::{OpenFlags, open_with_flags};

            let connection =
                open_with_flags(&self.db.to_string_lossy(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let tables = connection.query(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'idempotency_keys'",
            )?;
            let count = if tables.is_empty() {
                None
            } else {
                Some(
                    connection
                        .query_row("SELECT COUNT(*) FROM idempotency_keys")?
                        .get_typed::<i64>(0)?,
                )
            };
            connection.close_without_checkpoint()?;
            Ok(count)
        }

        fn prepare_v21_schema(&self) -> Result<()> {
            // V22 adds only this empty table. Remove the actual schema object
            // as well as its ledger entry so the next open must really migrate.
            let connection = Connection::open(self.db.to_string_lossy())?;
            connection.execute_batch(
                "DROP TABLE forgotten_sources; \
                 DELETE FROM _schema_migrations WHERE version = 22; \
                 UPDATE meta SET value = '21' WHERE key = 'schema_version';",
            )?;
            assert_eq!(
                connection
                    .query_row("SELECT MAX(version) FROM _schema_migrations")?
                    .get_typed::<i64>(0)?,
                21
            );
            Ok(())
        }

        fn job(&self) -> EmbeddingJobConfig {
            EmbeddingJobConfig {
                db_path: self.db.to_string_lossy().into_owned(),
                index_path: self.data.join("vectors").to_string_lossy().into_owned(),
                two_tier: false,
                fast_model: Some("hash".into()),
                quality_model: None,
            }
        }

        fn wait_job(
            &self,
            daemon: &mut OwnedProcess,
            client: &UdsDaemonClient,
            status: &str,
        ) -> Result<()> {
            daemon.until(|| {
                client
                    .embedding_job_status(&self.db.to_string_lossy())
                    .is_ok_and(|state| {
                        state
                            .jobs
                            .iter()
                            .max_by_key(|job| job.job_id)
                            .is_some_and(|job| job.status == status)
                    })
            })
        }

        fn assert_lock_released(&self) -> Result<()> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.data.join("index-run.lock"))?;
            file.try_lock()
                .context("completed operation still owns index-run.lock")?;
            file.unlock()?;
            Ok(())
        }

        fn shutdown(&self, daemon: &mut OwnedProcess, client: &UdsDaemonClient) -> Result<()> {
            client.shutdown()?;
            assert!(
                daemon.wait(Duration::from_secs(15))?.success(),
                "{}",
                daemon.diagnostics()
            );
            Ok(())
        }
    }

    #[test]
    fn gh515_resident_daemon_allows_fresh_schema_upgrade_and_repeated_indexing() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.seed(0)?;
        let (mut daemon, client) = fixture.daemon(false)?;
        fixture.index("fresh", &["--full"])?;
        fixture.assert_archive(1)?;
        fixture.prepare_v21_schema()?;
        fixture.seed(1)?;
        fixture.index("upgrade", &[])?;
        fixture.assert_archive(2)?;
        fixture.index("unchanged", &[])?;
        fixture.assert_archive(2)?;
        fixture.assert_lock_released()?;
        daemon.assert_running()?;
        fixture.shutdown(&mut daemon, &client)
    }

    #[test]
    fn gh515_periodic_daemon_index_serializes_standalone_and_releases_ownership() -> Result<()> {
        let mut fixture = Fixture::new()?;
        // Periodic daemon refresh explicitly targets the default archive.
        // The other coexistence regressions retain the external custom DB.
        fixture.db = fixture.data.join("agent_search.db");
        for id in 0..4 {
            fixture.seed(id)?;
        }
        fixture.index("periodic-initial", &["--full"])?;
        // Retain the first generation outside the live path. The next normal
        // periodic pass must perform a real canonical repair, whose commits
        // provide a deterministic rendezvous for the competing process.
        fs::rename(
            coding_agent_search::search::tantivy::expected_index_dir(&fixture.data),
            fixture.data.join("retained-initial-index"),
        )?;
        let sentinel = fixture.data.join("periodic-commit-window.json");
        let release = fixture.data.join("periodic-commit-release");
        let periodic = PeriodicIndexGuard {
            data: fixture.data.clone(),
            db: fixture.db.clone(),
            release: release.clone(),
            finished: false,
        };
        let (mut daemon, client) = fixture.daemon_configured(|command| {
            command
                .env("CASS_AUTO_REFRESH", "1")
                .env("CASS_DAEMON_INDEX_INTERVAL_SECS", "1")
                .env("CASS_AUTO_REFRESH_COOLDOWN_SECS", "300")
                .env(
                    "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SENTINEL",
                    &sentinel,
                )
                .env("CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMITS", "2")
                .env(
                    "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SLEEP_MS",
                    "60000",
                )
                .env(
                    "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_RELEASE",
                    &release,
                );
            for key in [
                "CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS",
                "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
                "CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS",
                "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
            ] {
                command.env(key, "1");
            }
        })?;
        // Drop the descendant guard before the daemon so its normal reaper
        // remains available even when a test assertion fails.
        let mut periodic = periodic;
        daemon.until(|| {
            fs::read(&sentinel)
                .ok()
                .is_some_and(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).is_ok())
        })?;
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(&sentinel)?)?;
        let state =
            background_refresh::load_state(&fixture.data).context("periodic spawn state")?;
        assert_eq!(state.last_reason, "daemon-periodic");
        assert_eq!(receipt["pid"].as_u64(), Some(u64::from(state.last_pid)));
        assert_ne!(state.last_pid, daemon.child.id());
        assert_eq!(periodic.owned_pid()?, Some(state.last_pid));
        assert!(
            receipt["committed_indexed_docs"].as_u64().unwrap_or(0)
                > receipt["checkpoint_indexed_docs"].as_u64().unwrap_or(0),
            "periodic work must reach a real committed prefix: {receipt}"
        );
        fixture.wait_index_owner(
            &mut daemon,
            state.last_pid,
            "lexical_refresh",
            Some("lexical:rebuild"),
        )?;
        fixture.assert_index_busy("while-periodic-index")?;
        fs::write(&release, b"release")?;
        periodic.wait(Duration::from_secs(45))?;
        fixture.assert_lock_released()?;
        assert!(fs::read(fixture.data.join("index-run.lock"))?.is_empty());
        fixture.assert_archive(4)?;
        let checkpoint: serde_json::Value = serde_json::from_slice(&fs::read(
            coding_agent_search::search::tantivy::expected_index_dir(&fixture.data)
                .join(".lexical-rebuild-state.json"),
        )?)?;
        assert_eq!(checkpoint["completed"], true, "{checkpoint}");
        fixture.index("after-periodic-index", &[])?;
        fixture.assert_archive(4)?;
        daemon.assert_running()?;
        assert_eq!(
            background_refresh::load_state(&fixture.data)
                .context("retained spawn state")?
                .last_pid,
            state.last_pid,
            "the cooldown must keep this test to one periodic child"
        );
        fixture.shutdown(&mut daemon, &client)
    }

    #[test]
    fn gh515_periodic_daemon_handoff_after_killed_foreground_index() -> Result<()> {
        use std::cell::Cell;
        use std::os::unix::process::ExitStatusExt;

        #[derive(Debug)]
        struct Owner {
            pid: u32,
            job_id: String,
            started_at_ms: i64,
            updated_at_ms: i64,
        }

        fn read_owner(data: &Path) -> Option<Owner> {
            let metadata = fs::read_to_string(data.join("index-run.lock")).ok()?;
            let value = |prefix: &str| metadata.lines().find_map(|line| line.strip_prefix(prefix));
            if value("job_kind=")? != "lexical_refresh" || value("phase=")? != "lexical:rebuild" {
                return None;
            }
            Some(Owner {
                pid: value("pid=")?.parse().ok()?,
                job_id: value("job_id=")?.to_string(),
                started_at_ms: value("started_at_ms=")?.parse().ok()?,
                updated_at_ms: value("updated_at_ms=")?.parse().ok()?,
            })
        }

        fn wait_owner(process: &mut OwnedProcess, data: &Path, pid: u32) -> Result<Owner> {
            // Heartbeats rewrite in place. Retry partial external reads rather
            // than treating their transient empty payload as an ownership loss.
            let observed = Cell::new(None);
            process.until(|| match read_owner(data) {
                Some(owner) if owner.pid == pid => {
                    observed.set(Some(owner));
                    true
                }
                _ => false,
            })?;
            observed.take().context("complete index owner metadata")
        }

        let mut fixture = Fixture::new()?;
        fixture.db = fixture.data.join("agent_search.db");
        for id in 0..4 {
            fixture.seed(id)?;
        }
        fixture.index("handoff-initial", &["--full"])?;

        let foreground_ready = fixture.data.join("foreground-commit-window.json");
        let mut command = fixture.index_command(&["--full", "--force-rebuild"]);
        command
            .env(
                "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SENTINEL",
                &foreground_ready,
            )
            .env("CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMITS", "2")
            .env(
                "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SLEEP_MS",
                "60000",
            );
        for key in [
            "CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS",
            "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
            "CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS",
            "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
        ] {
            command.env(key, "1");
        }
        let mut foreground = fixture.spawn("handoff-foreground", command)?;
        foreground.until(|| {
            fs::read(&foreground_ready)
                .ok()
                .is_some_and(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).is_ok())
        })?;
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(&foreground_ready)?)?;
        let foreground_pid = foreground.child.id();
        assert_eq!(receipt["pid"].as_u64(), Some(u64::from(foreground_pid)));
        assert!(
            receipt["committed_indexed_docs"].as_u64().unwrap_or(0)
                > receipt["checkpoint_indexed_docs"].as_u64().unwrap_or(0),
            "{receipt}"
        );
        let foreground_owner = wait_owner(&mut foreground, &fixture.data, foreground_pid)?;

        let periodic_ready = fixture.data.join("replacement-commit-window.json");
        let release = fixture.data.join("replacement-commit-release");
        let periodic = PeriodicIndexGuard {
            data: fixture.data.clone(),
            db: fixture.db.clone(),
            release: release.clone(),
            finished: false,
        };
        let (mut daemon, client) = fixture.daemon_configured(|command| {
            command
                .env("CASS_AUTO_REFRESH", "1")
                .env("CASS_DAEMON_INDEX_INTERVAL_SECS", "1")
                .env("CASS_AUTO_REFRESH_COOLDOWN_SECS", "300")
                .env("RUST_LOG", "coding_agent_search=debug")
                .env(
                    "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SENTINEL",
                    &periodic_ready,
                )
                .env("CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMITS", "1")
                .env(
                    "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SLEEP_MS",
                    "30000",
                )
                .env(
                    "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_RELEASE",
                    &release,
                );
            for key in [
                "CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS",
                "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
                "CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS",
                "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
            ] {
                command.env(key, "1");
            }
        })?;
        // Keep the daemon's reaper alive until its descendant has drained on
        // every exit, including an assertion failure during the handoff.
        let mut periodic = periodic;
        let daemon_log = daemon.stderr.clone();
        daemon.until(|| {
            fs::read_to_string(&daemon_log)
                .is_ok_and(|log| log.contains("auto-refresh skipped: index run already active"))
        })?;
        assert!(background_refresh::load_state(&fixture.data).is_none());
        assert_eq!(
            wait_owner(&mut daemon, &fixture.data, foreground_pid)?.job_id,
            foreground_owner.job_id,
            "a periodic attempt must not replace a live foreground owner"
        );

        foreground.child.kill()?;
        assert_eq!(
            foreground.wait(Duration::from_secs(5))?.signal(),
            Some(libc::SIGKILL),
            "{}",
            foreground.diagnostics()
        );
        // wait() has reaped the old process. A fresh busy record from this
        // point may legitimately belong to the daemon's next periodic child.
        daemon.until(|| {
            fs::read(&periodic_ready)
                .ok()
                .is_some_and(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).is_ok())
        })?;
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(&periodic_ready)?)?;
        let state = background_refresh::load_state(&fixture.data).context("replacement spawn")?;
        assert_eq!(state.last_reason, "daemon-periodic");
        assert_ne!(state.last_pid, foreground_pid);
        assert_ne!(state.last_pid, daemon.child.id());
        assert_eq!(receipt["pid"].as_u64(), Some(u64::from(state.last_pid)));
        assert_eq!(periodic.owned_pid()?, Some(state.last_pid));
        assert!(
            receipt["committed_indexed_docs"].as_u64().unwrap_or(0)
                > receipt["checkpoint_indexed_docs"].as_u64().unwrap_or(0),
            "{receipt}"
        );
        let replacement = wait_owner(&mut daemon, &fixture.data, state.last_pid)?;
        assert_ne!(replacement.job_id, foreground_owner.job_id);
        assert!(replacement.started_at_ms > foreground_owner.started_at_ms);
        fixture.assert_index_busy("handoff-replacement-active")?;
        daemon.until(|| {
            read_owner(&fixture.data).is_some_and(|owner| {
                owner.pid == replacement.pid
                    && owner.job_id == replacement.job_id
                    && owner.started_at_ms == replacement.started_at_ms
                    && owner.updated_at_ms > replacement.updated_at_ms
            })
        })?;

        fs::write(&release, b"release")?;
        periodic.wait(Duration::from_secs(45))?;
        fixture.assert_lock_released()?;
        assert!(fs::read(fixture.data.join("index-run.lock"))?.is_empty());
        fixture.index("handoff-recovered", &[])?;
        fixture.assert_archive(4)?;
        let checkpoint: serde_json::Value = serde_json::from_slice(&fs::read(
            coding_agent_search::search::tantivy::expected_index_dir(&fixture.data)
                .join(".lexical-rebuild-state.json"),
        )?)?;
        assert_eq!(checkpoint["completed"], true, "{checkpoint}");
        daemon.assert_running()?;
        assert_eq!(
            background_refresh::load_state(&fixture.data)
                .context("replacement cooldown state")?
                .last_pid,
            state.last_pid,
            "the cooldown must prevent a third periodic owner during recovery"
        );
        fixture.shutdown(&mut daemon, &client)
    }

    #[cfg(debug_assertions)]
    #[test]
    fn gh515_active_daemon_embedding_serializes_index_and_cancels_without_losing_retry()
    -> Result<()> {
        let fixture = Fixture::new()?;
        for id in 0..4 {
            fixture.seed(id)?;
        }
        fixture.index("initial", &["--full"])?;
        let (mut daemon, client) = fixture.daemon(true)?;
        client.submit_embedding_job(fixture.job())?;
        let ready = fixture.data.join("embedding-ready.json");
        daemon.until(|| {
            fs::read(&ready)
                .ok()
                .is_some_and(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).is_ok())
        })?;
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(&ready)?)?;
        let daemon_pid = daemon.child.id();
        assert_eq!(receipt["pid"].as_u64(), Some(u64::from(daemon_pid)));
        fixture.assert_index_busy("while-embedding")?;
        fixture.wait_index_owner(&mut daemon, daemon_pid, "semantic_rebuild", None)?;

        // A different search-assets directory must not admit another writer
        // to this archive. Exercise both the daemon's path and another spelling
        // of that same file while its real embedding snapshot remains open.
        let other_data = fixture.data.with_file_name("other-data");
        fs::create_dir_all(&other_data)?;
        let alias = fixture.db.with_file_name("archive-symlink.db");
        std::os::unix::fs::symlink(&fixture.db, &alias)?;
        assert_eq!(fs::canonicalize(&alias)?, fs::canonicalize(&fixture.db)?);
        let open_release = fixture.data.join("other-index-open-release");
        fs::write(&open_release, b"release")?;
        let bundle_bytes = || -> Result<Vec<Option<Vec<u8>>>> {
            [fixture.db.as_path(), alias.as_path()]
                .into_iter()
                .flat_map(|db| {
                    ["", "-wal", "-shm"].map(|suffix| {
                        let mut path = db.as_os_str().to_owned();
                        path.push(suffix);
                        match fs::read(Path::new(&path)) {
                            Ok(bytes) => Ok(Some(bytes)),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                            Err(error) => Err(error.into()),
                        }
                    })
                })
                .collect()
        };
        assert_eq!(fixture.idempotency_cache_row_count()?, None);
        let before = bundle_bytes()?;
        for (label, db) in [
            ("other-data-real-active", fixture.db.as_path()),
            ("other-data-symlink-active", alias.as_path()),
        ] {
            let opened = fixture.data.join(format!("{label}-opened.json"));
            let mut command =
                fixture.index_command_for(&other_data, db, &["--idempotency-key", label]);
            command
                .env("CASS_TEST_INDEX_STORAGE_OPEN_READY", &opened)
                .env("CASS_TEST_INDEX_STORAGE_OPEN_RELEASE", &open_release);
            fixture.assert_index_command_busy(label, command)?;
            assert!(
                !opened.exists(),
                "a contending writer reached storage preparation through {label}"
            );
            assert_eq!(fixture.idempotency_cache_row_count()?, None);
            assert_eq!(
                bundle_bytes()?,
                before,
                "pre-admission idempotency lookup changed the archive through {label}"
            );
            match fs::read(other_data.join("index-run.lock")) {
                Ok(metadata) => assert!(
                    metadata.is_empty(),
                    "rejected writer left an active record in its own data directory"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            daemon.assert_running()?;
        }

        assert_eq!(
            client.cancel_embedding_job(&fixture.db.to_string_lossy(), Some("hash"))?,
            1
        );
        fixture.wait_job(&mut daemon, &client, "cancelled")?;
        daemon.until(|| fixture.assert_lock_released().is_ok())?;
        assert!(!vector_index_path(Path::new(&fixture.job().index_path), "fnv1a-384").exists());
        fs::write(fixture.data.join("embedding-release"), b"release")?;
        client.submit_embedding_job(fixture.job())?;
        fixture.wait_job(&mut daemon, &client, "completed")?;
        daemon.until(|| fixture.assert_lock_released().is_ok())?;
        let vectors = VectorIndex::open(&vector_index_path(
            Path::new(&fixture.job().index_path),
            "fnv1a-384",
        ))?;
        assert_eq!(vectors.record_count(), 4);
        drop(vectors);
        fixture.index("after-embedding", &[])?;
        fixture.assert_archive(4)?;

        // Release must make progress possible through either data-directory
        // and archive-path spelling. Include a new message so the first retry
        // must perform canonical ingestion, then verify that replay is stable.
        fixture.seed(4)?;
        for (label, db) in [
            ("other-data-real-recovered", fixture.db.as_path()),
            ("other-data-symlink-recovered", alias.as_path()),
        ] {
            let command = fixture.index_command_for(&other_data, db, &["--idempotency-key", label]);
            let mut indexer = fixture.spawn(label, command)?;
            assert!(
                indexer.wait(Duration::from_secs(60))?.success(),
                "index {label}: {}",
                indexer.diagnostics()
            );
            fixture.assert_archive(5)?;
            assert!(fs::read(other_data.join("index-run.lock"))?.is_empty());
            daemon.assert_running()?;
        }
        assert_eq!(fixture.idempotency_cache_row_count()?, Some(2));
        fixture.shutdown(&mut daemon, &client)
    }

    #[cfg(debug_assertions)]
    #[test]
    fn gh515_daemon_defers_behind_index_and_recovers_after_forced_index_termination() -> Result<()>
    {
        let fixture = Fixture::new()?;
        for id in 0..4 {
            fixture.seed(id)?;
        }
        fixture.index("initial", &["--full"])?;
        let (mut daemon, client) = fixture.daemon(false)?;
        let sentinel = fixture.data.join("index-commit-window.json");
        let mut command = fixture.index_command(&["--full", "--force-rebuild"]);
        command
            .env(
                "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SENTINEL",
                &sentinel,
            )
            .env("CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMITS", "2")
            .env(
                "CASS_TEST_LEXICAL_REBUILD_KILL_AFTER_COMMIT_SLEEP_MS",
                "60000",
            );
        for key in [
            "CASS_TANTIVY_REBUILD_BATCH_FETCH_CONVERSATIONS",
            "CASS_TANTIVY_REBUILD_INITIAL_BATCH_FETCH_CONVERSATIONS",
            "CASS_TANTIVY_REBUILD_COMMIT_EVERY_CONVERSATIONS",
            "CASS_TANTIVY_REBUILD_INITIAL_COMMIT_EVERY_CONVERSATIONS",
        ] {
            command.env(key, "1");
        }
        let mut indexer = fixture.spawn("parked-index", command)?;
        indexer.until(|| {
            fs::read(&sentinel)
                .ok()
                .is_some_and(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).is_ok())
        })?;
        let receipt: serde_json::Value = serde_json::from_slice(&fs::read(&sentinel)?)?;
        assert!(
            receipt["committed_indexed_docs"].as_u64().unwrap_or(0)
                > receipt["checkpoint_indexed_docs"].as_u64().unwrap_or(0),
            "{receipt}"
        );
        client.submit_embedding_job(fixture.job())?;
        let daemon_log = daemon.stderr.clone();
        daemon.until(|| {
            fs::read_to_string(&daemon_log)
                .unwrap_or_default()
                .contains("Deferring embedding job behind active archive maintenance")
        })?;
        assert!(
            client
                .embedding_job_status(&fixture.db.to_string_lossy())?
                .jobs
                .is_empty(),
            "a deferred daemon must not create a running job or open a writer"
        );
        assert_eq!(
            client.cancel_embedding_job(&fixture.db.to_string_lossy(), None)?,
            1,
        );
        daemon.until(|| {
            fs::read_to_string(&daemon_log)
                .unwrap_or_default()
                .contains("Deferring embedding cancellation behind active archive maintenance")
        })?;
        // This new generation must run after the older cancellation's durable
        // cleanup, even though both are deferred behind the same index owner.
        client.submit_embedding_job(fixture.job())?;
        fixture.assert_index_busy("second-index")?;
        indexer.child.kill()?;
        assert!(!indexer.wait(Duration::from_secs(5))?.success());
        fixture.wait_job(&mut daemon, &client, "completed")?;
        daemon.until(|| fixture.assert_lock_released().is_ok())?;
        let idle_metadata = fs::read(fixture.data.join("index-run.lock"))?;
        assert!(
            idle_metadata.is_empty(),
            "a completed recovery must clear the active job record"
        );
        let heartbeat = || -> Option<u64> {
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(fixture.socket.with_extension("spawnlock")).ok()?)
                    .ok()?;
            value["heartbeat_unix_ms"].as_u64()
        };
        let previous = heartbeat().context("daemon heartbeat")?;
        daemon.until(|| heartbeat().is_some_and(|current| current > previous))?;
        assert_eq!(
            fs::read(fixture.data.join("index-run.lock"))?,
            idle_metadata,
            "the resident daemon must not recreate a finished archive job's heartbeat"
        );
        fixture.index("recovered", &[])?;
        fixture.assert_archive(4)?;
        let checkpoint: serde_json::Value = serde_json::from_slice(&fs::read(
            coding_agent_search::search::tantivy::expected_index_dir(&fixture.data)
                .join(".lexical-rebuild-state.json"),
        )?)?;
        assert_eq!(checkpoint["completed"], true, "{checkpoint}");
        fixture.shutdown(&mut daemon, &client)
    }

    #[cfg(debug_assertions)]
    #[test]
    fn gh515_sigterm_cancels_contended_migration_before_native_busy_timeout() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.seed(0)?;
        fixture.index("initial", &["--full"])?;
        fixture.prepare_v21_schema()?;
        let (mut daemon, client) = fixture.daemon(false)?;
        let ready = fixture.data.join("migration-open-ready.json");
        let release = fixture.data.join("migration-open-release");
        let mut command = fixture.index_command(&[]);
        command
            .env("CASS_TEST_INDEX_STORAGE_OPEN_READY", &ready)
            .env("CASS_TEST_INDEX_STORAGE_OPEN_RELEASE", &release);
        let mut indexer = fixture.spawn("contended-migration", command)?;
        let indexer_pid = indexer.child.id();
        indexer.until(|| {
            fs::read(&ready)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|receipt| {
                    receipt["pid"] == indexer_pid
                        && receipt["db_path"].as_str() == fixture.db.to_str()
                })
        })?;
        // Hold a real independent SQLite writer, without the CASS admission
        // lock, only after the child's raw engine constructor has returned.
        // The pending CASS migration must await this transaction, which stays
        // held until the signalled index has cooperatively drained.
        let blocker = Connection::open(fixture.db.to_string_lossy())?;
        blocker.execute("PRAGMA fsqlite.concurrent_mode = OFF")?;
        blocker.execute("BEGIN IMMEDIATE")?;
        blocker.execute("INSERT INTO meta(key, value) VALUES('gh515_uncommitted', 'rollback')")?;
        fs::write(&release, b"continue into CASS migration")?;
        fixture.wait_index_owner(
            &mut indexer,
            indexer_pid,
            "lexical_refresh",
            Some("watch_startup:open_storage"),
        )?;
        thread::sleep(Duration::from_millis(100));
        indexer.assert_running()?;
        fixture.wait_index_owner(
            &mut indexer,
            indexer_pid,
            "lexical_refresh",
            Some("watch_startup:open_storage"),
        )?;
        let shutdown_started = Instant::now();
        assert!(
            Command::new("/bin/kill")
                .args(["-TERM", &indexer.child.id().to_string()])
                .status()?
                .success()
        );
        // Storage uses a 5-second native busy timeout. Cancellation must finish
        // while our writer is still held and before that timeout expires.
        let status = indexer.wait(Duration::from_secs(3))?;
        let shutdown_elapsed = shutdown_started.elapsed();
        assert!(
            shutdown_elapsed < Duration::from_secs(3),
            "cooperative index cancellation exceeded 3 seconds ({shutdown_elapsed:?}): {}",
            indexer.diagnostics()
        );
        assert_eq!(status.code(), Some(143), "{}", indexer.diagnostics());
        fixture.assert_lock_released()?;
        assert!(
            fs::read(fixture.data.join("index-run.lock"))?.is_empty(),
            "cooperative cancellation must drain the owner and clear active metadata: {}",
            indexer.diagnostics()
        );
        assert_eq!(
            blocker
                .query_row("SELECT value FROM meta WHERE key = 'gh515_uncommitted'")?
                .get_typed::<String>(0)?,
            "rollback"
        );
        blocker.execute("ROLLBACK")?;
        blocker.close()?;
        daemon.assert_running()?;
        fixture.index("after-sigterm", &[])?;
        fixture.assert_archive(1)?;
        let recovered = Connection::open(fixture.db.to_string_lossy())?;
        assert_eq!(
            recovered
                .query_row("SELECT COUNT(*) FROM meta WHERE key = 'gh515_uncommitted'")?
                .get_typed::<i64>(0)?,
            0
        );
        recovered.close()?;
        fixture.shutdown(&mut daemon, &client)
    }

    #[test]
    fn gh515_sigterm_bounds_contended_engine_open_and_recovers_with_resident_daemon() -> Result<()>
    {
        let fixture = Fixture::new()?;
        fixture.seed(0)?;
        fixture.index("initial", &["--full"])?;
        fixture.prepare_v21_schema()?;
        let (mut daemon, client) = fixture.daemon(false)?;
        // This writer is acquired before the child's raw engine constructor.
        // Its bootstrap retry precedes CASS's cancellable migration SQL, so
        // the existing five-second forced shutdown must bound this stage.
        let blocker = Connection::open(fixture.db.to_string_lossy())?;
        blocker.execute("PRAGMA fsqlite.concurrent_mode = OFF")?;
        blocker.execute("BEGIN IMMEDIATE")?;
        blocker.execute(
            "INSERT INTO meta(key, value) VALUES('gh515_bootstrap_uncommitted', 'rollback')",
        )?;
        let mut indexer = fixture.spawn("contended-engine-open", fixture.index_command(&[]))?;
        let indexer_pid = indexer.child.id();
        fixture.wait_index_owner(
            &mut indexer,
            indexer_pid,
            "lexical_refresh",
            Some("watch_startup:open_storage"),
        )?;
        thread::sleep(Duration::from_millis(100));
        indexer.assert_running()?;
        fixture.wait_index_owner(
            &mut indexer,
            indexer_pid,
            "lexical_refresh",
            Some("watch_startup:open_storage"),
        )?;
        let shutdown_started = Instant::now();
        assert!(
            Command::new("/bin/kill")
                .args(["-TERM", &indexer_pid.to_string()])
                .status()?
                .success()
        );
        let status = indexer.wait(Duration::from_secs(8))?;
        let shutdown_elapsed = shutdown_started.elapsed();
        assert!(
            shutdown_elapsed < Duration::from_secs(8),
            "engine bootstrap exceeded the forced shutdown bound ({shutdown_elapsed:?}): {}",
            indexer.diagnostics()
        );
        assert_eq!(status.code(), Some(143), "{}", indexer.diagnostics());
        let stderr = fs::read_to_string(&indexer.stderr)?;
        let error = stderr
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|event| event["success"] == false && event["code"] == 143)
            .with_context(|| format!("forced shutdown envelope: {}", indexer.diagnostics()))?;
        assert_eq!(error["kind"], "index", "{error}");
        assert_eq!(error["retryable"], true, "{error}");
        assert!(
            error["error"]
                .as_str()
                .is_some_and(|message| message.contains("did not stop within 5 seconds")),
            "{error}"
        );
        assert!(
            error["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("WAL")),
            "{error}"
        );
        assert!(
            fs::read_to_string(&indexer.stdout)?
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .all(|event| event["success"] != true),
            "forced shutdown must not report a successful index: {}",
            indexer.diagnostics()
        );
        fixture.assert_lock_released()?;
        let exited_metadata = fs::read(fixture.data.join("index-run.lock"))?;
        let heartbeat = || -> Option<u64> {
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(fixture.socket.with_extension("spawnlock")).ok()?)
                    .ok()?;
            value["heartbeat_unix_ms"].as_u64()
        };
        let previous = heartbeat().context("daemon heartbeat")?;
        daemon.until(|| heartbeat().is_some_and(|current| current > previous))?;
        assert_eq!(
            fs::read(fixture.data.join("index-run.lock"))?,
            exited_metadata,
            "a live daemon must not heartbeat the forcibly exited index owner"
        );
        assert_eq!(
            blocker
                .query_row("SELECT value FROM meta WHERE key = 'gh515_bootstrap_uncommitted'")?
                .get_typed::<String>(0)?,
            "rollback"
        );
        blocker.execute("ROLLBACK")?;
        blocker.close()?;
        daemon.assert_running()?;
        fixture.index("after-forced-bootstrap-shutdown", &[])?;
        fixture.assert_archive(1)?;
        let recovered = Connection::open(fixture.db.to_string_lossy())?;
        assert_eq!(
            recovered
                .query_row("SELECT COUNT(*) FROM meta WHERE key = 'gh515_bootstrap_uncommitted'")?
                .get_typed::<i64>(0)?,
            0
        );
        recovered.close()?;
        fixture.assert_lock_released()?;
        assert!(
            fs::read(fixture.data.join("index-run.lock"))?.is_empty(),
            "successful recovery must clear the previous owner's active metadata"
        );
        daemon.assert_running()?;
        fixture.shutdown(&mut daemon, &client)
    }
}
