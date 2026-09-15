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

#[cfg(unix)]
mod native_daemon_process {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    use coding_agent_search::daemon::{DaemonClientConfig, UdsDaemonClient};
    use coding_agent_search::search::fastembed_embedder::FastEmbedder;
    use coding_agent_search::search::model_download::{ModelManifest, compute_sha256, model_file_path};

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
        assert_eq!(manifest.files.len(), 5, "the attested bundle has five files");
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
        assert_vector_bits(&expected_vectors[0], &expected_vectors[3], "duplicate input");
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
        assert_eq!(connection.model_category, ModelCategory::TransformerEmbedder);
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
        assert!(error.to_string().contains(&format!("received {expected_id}")));
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
