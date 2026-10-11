//! External backfill -> retained JSON/MCP retrieval, using the real binary.
//! Only a synthetic loopback HTTP server receives text. The tests never change
//! the parent's environment, consult provider history, or download a model.

use anyhow::{Context, Result, ensure};
use coding_agent_search::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use coding_agent_search::search::external_embedder::ExternalEmbeddingConfig;
use coding_agent_search::search::semantic_manifest::SemanticManifest;
use coding_agent_search::search::tantivy::{TantivyIndex, expected_index_dir};
use coding_agent_search::storage::sqlite::FrankenStorage;
use frankensearch::quill::cass::CassDocument;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(180);
const MODEL: &str = "service-test-model";
const KEY: &str = "synthetic-service-key-not-a-credential";

struct Endpoint {
    url: String,
    stop: Arc<AtomicBool>,
    refuse: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<Value>>>,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Endpoint {
    fn new() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}/v1/embeddings", listener.local_addr()?);
        let stop = Arc::new(AtomicBool::new(false));
        let refuse = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let failure = Arc::new(Mutex::new(None));
        let worker_stop = Arc::clone(&stop);
        let worker_refuse = Arc::clone(&refuse);
        let worker_seen = Arc::clone(&seen);
        let worker_failure = Arc::clone(&failure);
        let thread = thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Err(error) = respond(stream, &worker_seen, &worker_refuse) {
                            *worker_failure.lock().unwrap() = Some(format!("{error:#}"));
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => {
                        *worker_failure.lock().unwrap() = Some(error.to_string());
                        break;
                    }
                }
            }
        });
        Ok(Self {
            url,
            stop,
            refuse,
            seen,
            failure,
            thread: Some(thread),
        })
    }

    fn count(&self) -> usize {
        assert!(
            self.failure.lock().unwrap().is_none(),
            "fixture server failed"
        );
        self.seen.lock().unwrap().len()
    }

    fn identity(&self) -> Result<String> {
        Ok(ExternalEmbeddingConfig::from_lookup(|key| match key {
            "CASS_EXTERNAL_EMBEDDINGS" => Some("1".into()),
            "CASS_EXTERNAL_EMBEDDING_URL" => Some(self.url.clone()),
            "CASS_EXTERNAL_EMBEDDING_MODEL" => Some(MODEL.into()),
            "CASS_EXTERNAL_EMBEDDING_DIMENSION" => Some("384".into()),
            "CASS_EXTERNAL_EMBEDDING_REVISION" => Some("fixture-v1".into()),
            _ => None,
        })?
        .context("fixture consent missing")?
        .identity())
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn respond(mut stream: TcpStream, seen: &Mutex<Vec<Value>>, refuse: &AtomicBool) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    ensure!(
        first.trim_end() == "POST /v1/embeddings HTTP/1.1",
        "unexpected request"
    );
    let mut length = None;
    let mut authenticated = false;
    let mut header_bytes = first.len();
    loop {
        let mut line = String::new();
        ensure!(reader.read_line(&mut line)? > 0, "truncated headers");
        header_bytes += line.len();
        ensure!(header_bytes < 16 * 1024, "fixture header budget exceeded");
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                ensure!(length.is_none(), "duplicate length");
                length = Some(value.trim().parse::<usize>()?);
            }
            if name.eq_ignore_ascii_case("authorization") {
                authenticated = value.trim() == format!("Bearer {KEY}");
            }
        }
    }
    ensure!(authenticated, "missing fixture authentication");
    let length = length.context("missing content length")?;
    ensure!(length <= 1024 * 1024, "fixture request budget exceeded");
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let request: Value = serde_json::from_slice(&body)?;
    ensure!(request["model"] == MODEL, "provider model changed");
    let input = request["input"]
        .as_array()
        .context("input is not an array")?;
    let mut data = Vec::new();
    for (index, text) in input.iter().enumerate() {
        let text = text.as_str().context("input is not text")?;
        let mut vector = vec![0.0_f32; 384];
        vector[usize::from(!text.contains("externalneedle"))] = 1.0;
        data.push(json!({"index": index, "embedding": vector}));
    }
    {
        let mut records = seen.lock().unwrap();
        ensure!(records.len() < 64, "unbounded fixture request loop");
        records.push(request);
    }
    let (status, body) = if refuse.load(Ordering::SeqCst) {
        (
            "401 Unauthorized",
            json!({"error": "PRIVATE-ENDPOINT-BODY-MUST-NOT-ESCAPE"}),
        )
    } else {
        ("200 OK", json!({"model": MODEL, "data": data}))
    };
    let body = serde_json::to_vec(&body)?;
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

struct Process(Child);
impl Process {
    fn wait(&mut self) -> Result<ExitStatus> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
            ensure!(Instant::now() < deadline, "test child exceeded its deadline");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn command(root: &Path, data: &Path, endpoint: &Endpoint, consent: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
    // Keep only OS/runtime necessities, not parent CASS policy, fixture hooks,
    // tracing destinations, credentials, dotenv contents, or provider roots.
    command.env_clear().envs(std::env::vars_os().filter(|(key, _)| {
        [
            "PATH",
            "SYSTEMROOT",
            "WINDIR",
            "TEMP",
            "TMP",
            "TMPDIR",
            "RUST_MIN_STACK",
            "LD_LIBRARY_PATH",
            "DYLD_LIBRARY_PATH",
        ]
        .iter()
        .any(|allowed| key.to_string_lossy().eq_ignore_ascii_case(allowed))
    }));
    command
        .current_dir(root)
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("local-data"))
        .env("CASS_DATA_DIR", data)
        .env("CASS_AUTO_REFRESH", "0")
        // Policy and endpoint configuration alone must not select a service provider.
        .env("CASS_SEMANTIC_EMBEDDER", "external")
        .env("CASS_EXTERNAL_EMBEDDINGS", if consent { "1" } else { "0" })
        .env("CASS_EXTERNAL_EMBEDDING_URL", &endpoint.url)
        .env("CASS_EXTERNAL_EMBEDDING_MODEL", MODEL)
        .env("CASS_EXTERNAL_EMBEDDING_DIMENSION", "384")
        .env("CASS_EXTERNAL_EMBEDDING_REVISION", "fixture-v1")
        .env("CASS_EXTERNAL_EMBEDDING_API_KEY", KEY)
        .env("CASS_EXTERNAL_EMBEDDING_BATCH_SIZE", "64")
        .env("CASS_EXTERNAL_EMBEDDING_MAX_REQUEST_BYTES", "262144")
        .env("CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS", "5000")
        .env("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", "0");
    command
}

struct Worker {
    process: Process,
    stdin: ChildStdin,
    replies: mpsc::Receiver<std::io::Result<String>>,
}

impl Worker {
    fn start(
        root: &Path,
        data: &Path,
        endpoint: &Endpoint,
        selected: bool,
        consent: bool,
        mcp: bool,
    ) -> Result<Self> {
        let mut command = command(root, data, endpoint, consent);
        command
            .args(["serve", "--stdio", "--data-dir"])
            .arg(data)
            .arg("--db")
            .arg(data.join("agent_search.db"))
            .args([
                "--request-timeout-ms",
                "120000",
                "--max-resident-mib",
                "4096",
            ]);
        if selected {
            command.args(["--semantic-embedder", "external"]);
        }
        if mcp {
            command.arg("--mcp");
        }
        let mut process = Process(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?,
        );
        let stdin = process.0.stdin.take().context("child stdin missing")?;
        let stdout = process.0.stdout.take().context("child stdout missing")?;
        let (send, replies) = mpsc::channel();
        // The guarded child owns the pipe. Killing/waiting it on every exit
        // closes stdout and releases this finite reader even after an assertion.
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            process,
            stdin,
            replies,
        })
    }

    fn notify(&mut self, request: Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, &request)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn request(&mut self, request: Value) -> Result<Value> {
        let id = request["id"].clone();
        self.notify(request)?;
        let line = self
            .replies
            .recv_timeout(WAIT)
            .context("missing child reply")??;
        let reply: Value = serde_json::from_str(&line)?;
        ensure!(reply["id"] == id, "uncorrelated reply: {reply}");
        ensure!(
            !line.contains(KEY) && !line.contains("PRIVATE-ENDPOINT-BODY"),
            "private error payload escaped"
        );
        Ok(reply)
    }

    fn stop(mut self) -> Result<()> {
        let reply = self.request(json!({"op": "shutdown", "id": 999}))?;
        ensure!(reply["ok"] == true, "shutdown failed: {reply}");
        ensure!(self.process.wait()?.success(), "service exit failed");
        Ok(())
    }
}

fn semantic_request(id: u64, query: &str) -> Value {
    json!({"op": "semantic_search", "id": id, "query": query, "mode": "semantic", "limit": 1})
}

struct Fixture {
    root: tempfile::TempDir,
    data: PathBuf,
    conversation: i64,
    source: PathBuf,
}

impl Fixture {
    fn new(endpoint: &Endpoint) -> Result<Self> {
        let root = tempfile::tempdir()?;
        // Stop dotenv's upward search at this fresh fixture; never read or
        // overwrite a shared host's .env file.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.path().join(".env"))?;
        let data = root.path().join("data");
        fs::create_dir(&data)?;
        let database = data.join("agent_search.db");
        let storage = FrankenStorage::open(&database)?;
        let agent = storage.ensure_agent(&Agent {
            id: None,
            slug: "codex".into(),
            name: "Codex".into(),
            version: None,
            kind: AgentKind::Cli,
        })?;
        let source = root.path().join("source-must-never-be-opened.jsonl");
        let outcome = storage.insert_conversation_tree(
            agent,
            None,
            &Conversation {
                id: None,
                agent_slug: "codex".into(),
                workspace: None,
                external_id: Some("external-service-fixture".into()),
                title: Some("Synthetic service history".into()),
                source_path: source.clone(),
                started_at: None,
                ended_at: None,
                approx_tokens: None,
                metadata_json: json!({}),
                source_id: "local".into(),
                origin_host: None,
                messages: [
                    (7, "externalneedle retained provider retrieval"),
                    (12, "banana orchard unrelated"),
                ]
                .into_iter()
                .map(|(idx, content)| Message {
                    id: None,
                    idx,
                    role: MessageRole::Agent,
                    author: None,
                    created_at: None,
                    content: content.into(),
                    extra_json: json!({}),
                    snippets: Vec::new(),
                })
                .collect(),
            },
        )?;
        drop(storage);
        let mut lexical = TantivyIndex::open_or_create(&expected_index_dir(&data))?;
        lexical.add_prebuilt_documents_slice(&[CassDocument {
            agent: "codex".into(),
            workspace: None,
            workspace_original: None,
            source_path: source.to_string_lossy().into_owned(),
            msg_idx: 7,
            created_at: None,
            title: Some("Synthetic service history".into()),
            content: "localfallback".into(),
            source_id: "local".into(),
            origin_kind: "local".into(),
            origin_host: None,
            conversation_id: Some(outcome.conversation_id),
        }])?;
        lexical.commit()?;
        drop(lexical);
        // Use the real producer instead of hand-assembling its fingerprint,
        // passage IDs, ownership metadata or current publication contract.
        let stdout = root.path().join("backfill.stdout");
        let stderr = root.path().join("backfill.stderr");
        let mut command = command(root.path(), &data, endpoint, true);
        command
            .arg("--db")
            .arg(&database)
            .args([
                "models",
                "backfill",
                "--tier",
                "quality",
                "--embedder",
                "external",
                "--batch-conversations",
                "4",
                "--max-batches",
                "4",
                "--json",
                "--data-dir",
            ])
            .arg(&data)
            .stdin(Stdio::null())
            .stdout(File::create(&stdout)?)
            .stderr(File::create(&stderr)?);
        let status = Process(command.spawn()?).wait()?;
        ensure!(
            status.success(),
            "backfill failed: {}",
            fs::read_to_string(stderr)?
        );
        let manifest = SemanticManifest::load(&data)?.context("backfill manifest missing")?;
        let quality = manifest.quality_tier.context("quality artifact missing")?;
        ensure!(
            quality.ready && quality.doc_count >= 2,
            "backfill did not publish the fixture"
        );
        ensure!(
            quality.embedder_id == endpoint.identity()?,
            "backfill selected another provider"
        );
        ensure!(
            manifest.checkpoint.is_none(),
            "backfill left a partial generation"
        );
        Ok(Self {
            root,
            data,
            conversation: outcome.conversation_id,
            source,
        })
    }

    fn worker(
        &self,
        endpoint: &Endpoint,
        selected: bool,
        consent: bool,
        mcp: bool,
    ) -> Result<Worker> {
        Worker::start(
            self.root.path(),
            &self.data,
            endpoint,
            selected,
            consent,
            mcp,
        )
    }
}

#[test]
fn external_service_reuses_real_backfill_and_unloads_without_local_inference() -> Result<()> {
    let endpoint = Endpoint::new()?;
    let fixture = Fixture::new(&endpoint)?;
    let database_before = fs::read(fixture.data.join("agent_search.db"))?;
    let manifest_before = fs::read(SemanticManifest::path(&fixture.data))?;
    let mut worker = fixture.worker(&endpoint, true, true, false)?;
    let base = endpoint.count();
    let status = worker.request(json!({"op": "status", "id": 1}))?;
    assert_eq!(status["result"]["semantic"]["loaded"], false);
    assert_eq!(status["result"]["semantic"]["load_attempts"], 0);
    let invalid = worker.request(json!({
        "op": "semantic_search", "id": 2, "query": " ", "mode": "semantic"
    }))?;
    assert_eq!(invalid["error"]["kind"], "invalid_request");
    let lexical = worker.request(json!({
        "op": "search", "id": 3, "query": "localfallback", "limit": 1
    }))?;
    assert_eq!(lexical["result"]["count"], 1);
    assert_eq!(
        endpoint.count(),
        base,
        "passive and lexical paths must never disclose queries"
    );
    for id in 4..=5 {
        let reply = worker.request(semantic_request(id, &format!("externalneedle request {id}")))?;
        assert_eq!(reply["ok"], true, "{reply}");
        let result = &reply["result"];
        assert_eq!(result["realized_mode"], "semantic");
        assert_eq!(result["hits"][0]["conversation_id"], fixture.conversation);
        assert_eq!(result["hits"][0]["message_index"], 8);
        assert_eq!(result["semantic_reused"], id == 5);
        assert_eq!(result["snapshot"]["semantic"]["successful_loads"], 1);
        assert_eq!(
            result["snapshot"]["semantic"]["embedder_id"],
            endpoint.identity()?
        );
        assert_eq!(result["snapshot"]["semantic"]["local_model_loaded"], false);
        assert!(result["snapshot"]["semantic"]["neural"].is_null());
        assert_eq!(result["ann"]["used"], false);
    }
    assert_eq!(
        endpoint.count(),
        base + 4,
        "two public probe rounds, then exactly two queries"
    );
    {
        let sent = endpoint.seen.lock().unwrap();
        let requests = &sent[base..];
        assert_eq!(requests[0]["input"], requests[1]["input"]);
        assert_eq!(requests[0]["input"].as_array().unwrap().len(), 3);
        assert_eq!(requests[2]["input"], json!(["externalneedle request 4"]));
        assert_eq!(requests[3]["input"], json!(["externalneedle request 5"]));
        assert!(
            requests.iter().all(|request| {
                !request.to_string().contains("retained provider retrieval")
                    && !request.to_string().contains("banana orchard unrelated")
            }),
            "search must not resend canonical message bodies to the endpoint"
        );
    }
    let unloaded = worker.request(json!({"op": "unload", "id": 6}))?;
    assert_eq!(unloaded["result"]["semantic"]["loaded"], false);
    assert!(unloaded["result"]["semantic"]["embedder_id"].is_null());
    assert_eq!(endpoint.count(), base + 4);
    let reloaded = worker.request(semantic_request(7, "externalneedle after unload"))?;
    assert_eq!(reloaded["ok"], true, "{reloaded}");
    assert_eq!(reloaded["result"]["semantic_reused"], false);
    assert_eq!(
        reloaded["result"]["snapshot"]["semantic"]["successful_loads"],
        2
    );
    assert_eq!(
        endpoint.count(),
        base + 7,
        "unload requires a fresh admitted provider"
    );
    worker.stop()?;
    assert!(!fixture.source.exists());
    assert_eq!(
        database_before,
        fs::read(fixture.data.join("agent_search.db"))?
    );
    assert_eq!(
        manifest_before,
        fs::read(SemanticManifest::path(&fixture.data))?
    );
    Ok(())
}

#[test]
fn external_service_configuration_never_replaces_explicit_selection_or_consent() -> Result<()> {
    let endpoint = Endpoint::new()?;
    let fixture = Fixture::new(&endpoint)?;
    for (selected, consent, expected) in [
        (false, true, "semantic_disabled"),
        (true, false, "semantic_unavailable"),
    ] {
        let base = endpoint.count();
        let mut worker = fixture.worker(&endpoint, selected, consent, false)?;
        let reply = worker.request(semantic_request(1, "private query must stay local"))?;
        assert_eq!(reply["error"]["kind"], expected, "{reply}");
        assert_eq!(endpoint.count(), base);
        let lexical = worker.request(json!({
            "op": "search", "id": 2, "query": "localfallback", "limit": 1
        }))?;
        assert_eq!(lexical["result"]["count"], 1);
        assert_eq!(endpoint.count(), base);
        worker.stop()?;
    }
    Ok(())
}

#[test]
fn external_service_refuses_changed_publication_before_sending_another_query() -> Result<()> {
    let endpoint = Endpoint::new()?;
    let fixture = Fixture::new(&endpoint)?;
    let mut worker = fixture.worker(&endpoint, true, true, false)?;
    assert_eq!(
        worker.request(semantic_request(1, "externalneedle original"))?["ok"],
        true
    );
    let base = endpoint.count();
    let path = SemanticManifest::path(&fixture.data);
    let original = fs::read(&path)?;
    // Change a guarded file without making its JSON invalid. No wall-clock
    // sleep or filesystem timestamp granularity is needed: length changes.
    let mut changed = original.clone();
    changed.extend_from_slice(b"\n ");
    fs::write(&path, &changed)?;
    let refused = worker.request(semantic_request(2, "private query after publication changed"))?;
    assert_eq!(
        refused["error"]["kind"],
        "semantic_reload_required",
        "{refused}"
    );
    assert_eq!(endpoint.count(), base);
    let status = worker.request(json!({"op": "status", "id": 3}))?;
    assert_eq!(status["result"]["semantic"]["loaded"], false);
    assert!(status["result"]["semantic"]["embedder_id"].is_null());
    assert_eq!(status["result"]["semantic"]["requires_reload"], true);
    worker.request(json!({"op": "unload", "id": 4}))?;
    let mut manifest: Value = serde_json::from_slice(&original)?;
    manifest["quality_tier"]["ready"] = json!(false);
    fs::write(&path, serde_json::to_vec(&manifest)?)?;
    let refused = worker.request(semantic_request(5, "private query for unfinished publication"))?;
    assert_eq!(refused["error"]["kind"], "semantic_unavailable", "{refused}");
    assert_eq!(
        endpoint.count(),
        base,
        "invalid publication must precede even public preflight"
    );
    worker.stop()?;
    Ok(())
}

#[test]
fn external_service_endpoint_failure_stays_private_and_hybrid_fallback_is_explicit() -> Result<()> {
    let endpoint = Endpoint::new()?;
    let fixture = Fixture::new(&endpoint)?;
    let mut worker = fixture.worker(&endpoint, true, true, false)?;
    assert_eq!(
        worker.request(semantic_request(1, "externalneedle establish provider"))?["ok"],
        true
    );
    endpoint.refuse.store(true, Ordering::SeqCst);
    let failed = worker.request(semantic_request(2, "externalneedle rejected request"))?;
    assert_eq!(failed["ok"], false, "{failed}");
    assert!(failed.get("result").is_none_or(Value::is_null));
    let fallback = worker.request(json!({"op": "semantic_search", "id": 3,
        "mode": "hybrid", "query": "localfallback", "limit": 1}))?;
    assert_eq!(fallback["ok"], true, "{fallback}");
    assert_eq!(fallback["result"]["realized_mode"], "lexical");
    assert!(
        fallback["result"]["semantic_fallback_reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty())
    );
    assert_eq!(fallback["result"]["count"], 1);
    worker.stop()?;
    Ok(())
}

#[test]
fn external_mcp_queries_share_the_same_retained_provider() -> Result<()> {
    let endpoint = Endpoint::new()?;
    let fixture = Fixture::new(&endpoint)?;
    let mut worker = fixture.worker(&endpoint, true, true, true)?;
    let base = endpoint.count();
    let initialized = worker.request(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "external-fixture", "version": "1"}}}))?;
    assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");
    assert!(
        initialized["result"]["instructions"]
            .as_str()
            .context("missing instructions")?
            .contains("sends its query outside this process")
    );
    worker.notify(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
    let catalog = worker.request(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))?;
    assert!(
        catalog["result"]["tools"]
            .as_array()
            .context("missing catalog")?
            .iter()
            .any(|tool| tool["name"] == "cass_semantic_search")
    );
    for tool in catalog["result"]["tools"].as_array().unwrap() {
        assert_eq!(
            tool["annotations"]["openWorldHint"],
            tool["name"] == "cass_semantic_search"
        );
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            tool["name"] != "cass_semantic_search"
        );
    }
    assert_eq!(
        endpoint.count(),
        base,
        "handshake/catalog must not send probes"
    );
    for id in 3..=4 {
        let reply = worker.request(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "cass_semantic_search", "arguments": {
                "query": format!("externalneedle mcp request {id}"), "mode": "semantic", "limit": 1}}}))?;
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        let result = &reply["result"]["structuredContent"];
        assert_eq!(result["hits"][0]["conversation_id"], fixture.conversation);
        assert_eq!(result["semantic_reused"], id == 4);
        assert_eq!(result["snapshot"]["semantic"]["successful_loads"], 1);
    }
    assert_eq!(endpoint.count(), base + 4);
    // MCP owns EOF shutdown rather than the CASS protocol's shutdown frame.
    drop(worker.stdin);
    ensure!(worker.process.wait()?.success(), "MCP EOF shutdown failed");
    Ok(())
}
