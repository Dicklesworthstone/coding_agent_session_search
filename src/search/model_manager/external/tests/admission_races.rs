//! A valid public-input preflight must not renew an obsolete archive admission.
//! Interleave real ledger/storage writes with the first loopback HTTP request;
//! no sleep determines whether the mutation happens before the readiness check.
use super::*;
use crate::model::types::{Agent, AgentKind, Conversation, Message, MessageRole};
use crate::search::semantic_manifest::BuildCheckpoint;
use anyhow::{Context as _, Result as AnyResult};
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const PRIVATE_QUERY: &str = "private query must wait for final admission";
const PUBLIC_PROBES: [&str; 3] = [
    "cass external embedding probe: the quick brown fox",
    "fn add(left: i32, right: i32) -> i32 { left + right }",
    "Semantic search: café, 日本語, and a different sentence.",
];
type Mutation = Box<dyn FnOnce() -> AnyResult<()> + Send>;

struct Endpoint {
    config: ExternalEmbeddingConfig,
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    mutation: Arc<Mutex<Option<Mutation>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<AnyResult<()>>>,
}

impl Endpoint {
    fn start() -> AnyResult<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}/v1/embeddings", listener.local_addr()?);
        let config = ExternalEmbeddingConfig::from_lookup(|key| match key {
            "CASS_EXTERNAL_EMBEDDINGS" => Some("1".into()),
            "CASS_EXTERNAL_EMBEDDING_URL" => Some(url.clone()),
            "CASS_EXTERNAL_EMBEDDING_MODEL" => Some("fixture".into()),
            "CASS_EXTERNAL_EMBEDDING_DIMENSION" => Some("384".into()),
            "CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS" => Some("10000".into()),
            _ => None,
        })?
        .context("explicit fixture consent")?;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mutation = Arc::new(Mutex::new(None::<Mutation>));
        let stopped = Arc::new(AtomicBool::new(false));
        let (recorded, action, stop) = (
            Arc::clone(&seen),
            Arc::clone(&mutation),
            Arc::clone(&stopped),
        );
        let worker = thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &recorded, &action)?,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        });
        Ok(Self {
            config,
            seen,
            mutation,
            stopped,
            worker: Some(worker),
        })
    }

    fn arm(&self, action: impl FnOnce() -> AnyResult<()> + Send + 'static) {
        let mut mutation = self.mutation.lock().unwrap();
        assert!(
            mutation.is_none(),
            "one mutation per first preflight request"
        );
        *mutation = Some(Box::new(action));
    }

    fn assert_only_public_preflight(&self) {
        let seen = self.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "both normal preflight rounds must finish");
        for batch in seen.iter() {
            assert_eq!(batch, &PUBLIC_PROBES.map(str::to_owned).to_vec());
        }
        assert!(self.mutation.lock().unwrap().is_none(), "mutation ran");
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result
                    .expect("endpoint panicked")
                    .expect("endpoint I/O failed");
            }
        }
    }
}

fn serve(
    mut stream: TcpStream,
    seen: &Mutex<Vec<Vec<String>>>,
    mutation: &Mutex<Option<Mutation>>,
) -> AnyResult<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    anyhow::ensure!(reader.read_line(&mut line)? > 0, "missing HTTP request");
    anyhow::ensure!(line.trim_end() == "POST /v1/embeddings HTTP/1.1");
    let mut header_bytes = line.len();
    let mut length = None;
    loop {
        line.clear();
        anyhow::ensure!(reader.read_line(&mut line)? > 0, "truncated HTTP header");
        header_bytes += line.len();
        anyhow::ensure!(header_bytes <= 16 * 1024, "unbounded fixture headers");
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("missing content length")?;
    anyhow::ensure!(length <= 262_144, "unbounded fixture body");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    let request: Value = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(request["model"] == "fixture");
    anyhow::ensure!(request["encoding_format"] == "float");
    let inputs: Vec<String> = serde_json::from_value(request["input"].clone())?;
    anyhow::ensure!(!inputs.is_empty() && inputs.len() <= 64);
    seen.lock().unwrap().push(inputs.clone());
    let action = mutation.lock().unwrap().take();
    if let Some(action) = action {
        action().context("interleaved archive/publication mutation failed")?;
    }
    let data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let mut vector = vec![0.0_f32; 384];
            let coordinate = text.bytes().fold(0usize, |n, b| (n + usize::from(b)) % 384);
            vector[coordinate] = 1.0;
            json!({"index": index, "embedding": vector})
        })
        .collect();
    let body = serde_json::to_vec(&json!({"model": "fixture", "data": data}))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

struct Fixture {
    // Stop/join the server before removing its temporary archive.
    endpoint: Endpoint,
    dir: tempfile::TempDir,
    db: PathBuf,
    record: ArtifactRecord,
}

fn add_conversation(storage: &FrankenStorage, ordinal: u32) -> AnyResult<()> {
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codex".into(),
        name: "Codex".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    storage.insert_conversation_tree(
        agent,
        None,
        &Conversation {
            id: None,
            agent_slug: "codex".into(),
            workspace: None,
            external_id: Some(format!("admission-{ordinal}")),
            title: None,
            source_path: format!("/fixture/admission-{ordinal}.jsonl").into(),
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
                content: format!("private original admission corpus {ordinal}"),
                extra_json: json!({}),
                snippets: Vec::new(),
            }],
        },
    )?;
    Ok(())
}

impl Fixture {
    fn new() -> AnyResult<Self> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("agent_search.db");
        let storage = FrankenStorage::open(&db)?;
        add_conversation(&storage, 1)?;
        storage.complete_semantic_identity_rebuild(SemanticIdentityTier::Quality)?;
        let endpoint = Endpoint::start()?;
        let config = &endpoint.config;
        let path = vector_index_path(dir.path(), &config.identity());
        fs::create_dir_all(path.parent().context("vector directory")?)?;
        let mut writer = VectorIndex::create_with_revision(
            &path,
            &config.identity(),
            crate::search::external_embedder::EXTERNAL_VECTOR_SPACE_REVISION,
            config.dimension(),
            Quantization::F16,
        )?;
        let doc = crate::search::vector_index::SemanticDocId {
            message_id: 1,
            chunk_idx: 0,
            agent_id: 1,
            workspace_id: 0,
            source_id: 0,
            role: ROLE_USER,
            created_at_ms: 1,
            content_hash: Some(crate::search::canonicalize::content_hash(
                "private original admission corpus 1",
            )),
        };
        let mut vector = vec![0.0_f32; config.dimension()];
        vector[0] = 1.0;
        writer.write_record(&doc.to_doc_id_string(), &vector)?;
        writer.finish()?;
        let mut record = publication(dir.path(), config);
        record.db_fingerprint = crate::indexer::lexical_storage_fingerprint_for_storage(&storage)?;
        record.size_bytes = fs::metadata(&path)?.len();
        let mut manifest = SemanticManifest {
            quality_tier: Some(record.clone()),
            ..SemanticManifest::default()
        };
        manifest.save(dir.path())?;
        drop(storage);
        Ok(Self {
            endpoint,
            dir,
            db,
            record,
        })
    }

    fn load(&self, strict: bool) -> SemanticSetup {
        load_with_config(
            self.dir.path(),
            &self.db,
            strict,
            self.endpoint.config.clone(),
            Arc::new(|| false),
        )
    }

    fn mutate_manifest(
        &self,
        mutate: impl FnOnce(&mut SemanticManifest) + Send + 'static,
    ) -> Arc<Mutex<Option<Vec<u8>>>> {
        let root = self.dir.path().to_path_buf();
        let captured = Arc::new(Mutex::new(None));
        let saved = Arc::clone(&captured);
        self.endpoint.arm(move || {
            let mut manifest = SemanticManifest::load(&root)?.context("existing publication")?;
            mutate(&mut manifest);
            manifest.save(&root)?;
            *saved.lock().unwrap() = Some(fs::read(SemanticManifest::path(&root))?);
            Ok(())
        });
        captured
    }

    fn assert_manifest_retained(&self, captured: &Mutex<Option<Vec<u8>>>) -> AnyResult<()> {
        let captured = captured.lock().unwrap();
        assert_eq!(
            &fs::read(SemanticManifest::path(self.dir.path()))?,
            captured.as_ref().context("mutation must have executed")?
        );
        Ok(())
    }
}

fn checkpoint(record: &ArtifactRecord, tier: TierKind, id: String) -> BuildCheckpoint {
    BuildCheckpoint {
        tier,
        embedder_id: id,
        last_offset: 1,
        docs_embedded: 1,
        conversations_processed: 1,
        total_conversations: 2,
        db_fingerprint: record.db_fingerprint.clone(),
        schema_version: SEMANTIC_SCHEMA_VERSION,
        chunking_version: CHUNKING_STRATEGY_VERSION,
        saved_at_ms: 3,
        last_message_id: Some(1),
        cursor_exhausted: false,
    }
}

fn assert_stale(setup: &SemanticSetup, reason_fragment: &str) {
    assert!(
        setup.context.is_none(),
        "stale admission exposed a query provider"
    );
    match &setup.availability {
        SemanticAvailability::IndexStale { reason, .. } => {
            assert!(reason.contains(reason_fragment), "{reason}");
        }
        other => panic!("unexpected failure (not the tested race): {other:?}"),
    }
}

#[test]
fn unchanged_preflight_retains_the_reader_without_extra_http() -> AnyResult<()> {
    for strict in [false, true] {
        let fixture = Fixture::new()?;
        let path = vector_index_path(fixture.dir.path(), &fixture.endpoint.config.identity());
        let vectors = fs::read(&path)?;
        let manifest = fs::read(SemanticManifest::path(fixture.dir.path()))?;
        let setup = fixture.load(strict);
        assert!(matches!(
            setup.availability,
            SemanticAvailability::Ready { .. }
        ));
        fixture.endpoint.assert_only_public_preflight();
        let context = setup
            .context
            .context("valid admission must expose a query context")?;
        assert_eq!(context.artifacts[0].index().record_count(), 1);
        assert_eq!(context.embedder.embed_sync(PRIVATE_QUERY)?.len(), 384);
        let seen = fixture.endpoint.seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "two preflights then the authorized query");
        assert_eq!(seen[2], vec![PRIVATE_QUERY.to_owned()]);
        assert_eq!(fs::read(&path)?, vectors);
        assert_eq!(
            fs::read(SemanticManifest::path(fixture.dir.path()))?,
            manifest
        );
    }
    Ok(())
}

#[test]
fn backfill_started_during_preflight_cannot_become_ready() -> AnyResult<()> {
    let fixture = Fixture::new()?;
    let record = fixture.record.clone();
    let saved = fixture.mutate_manifest(move |manifest| {
        manifest.checkpoint = Some(checkpoint(
            &record,
            TierKind::Quality,
            record.embedder_id.clone(),
        ));
    });
    let vectors = fs::read(fixture.dir.path().join(&fixture.record.index_path))?;
    assert_stale(&fixture.load(true), "incomplete");
    fixture.endpoint.assert_only_public_preflight();
    fixture.assert_manifest_retained(&saved)?;
    assert_eq!(
        fs::read(fixture.dir.path().join(&fixture.record.index_path))?,
        vectors
    );
    Ok(())
}

#[test]
fn replaced_or_revoked_publication_during_preflight_is_refused() -> AnyResult<()> {
    for revoked in [false, true] {
        let fixture = Fixture::new()?;
        let saved = fixture.mutate_manifest(move |manifest| {
            let record = manifest.quality_tier.as_mut().unwrap();
            if revoked {
                record.ready = false;
            } else {
                // Still independently valid, but not the admitted publication.
                record.completed_at_ms += 1;
            }
        });
        assert_stale(
            &fixture.load(true),
            if revoked {
                "artifact identity"
            } else {
                "external_admission_changed"
            },
        );
        fixture.endpoint.assert_only_public_preflight();
        fixture.assert_manifest_retained(&saved)?;
        if !revoked {
            // Retrying may admit the newly published record, but the first
            // call must not silently replace its old admission with it.
            let retry = fixture.load(true);
            assert!(matches!(
                retry.availability,
                SemanticAvailability::Ready { .. }
            ));
            assert!(retry.context.is_some());
            assert_eq!(fixture.endpoint.seen.lock().unwrap().len(), 4);
            fixture.assert_manifest_retained(&saved)?;
        }
    }
    Ok(())
}

#[test]
fn canonical_append_during_preflight_is_seen_on_a_fresh_connection() -> AnyResult<()> {
    let fixture = Fixture::new()?;
    let db = fixture.db.clone();
    fixture.endpoint.arm(move || {
        let storage = FrankenStorage::open(&db)?;
        add_conversation(&storage, 2)
    });
    let before = fs::read(SemanticManifest::path(fixture.dir.path()))?;
    assert_stale(&fixture.load(true), "canonical archive changed");
    fixture.endpoint.assert_only_public_preflight();
    let current = FrankenStorage::open_strict_readonly(&fixture.db)?;
    assert_ne!(
        crate::indexer::lexical_storage_fingerprint_for_storage(&current)?,
        fixture.record.db_fingerprint
    );
    assert_eq!(
        fs::read(SemanticManifest::path(fixture.dir.path()))?,
        before
    );
    Ok(())
}

#[test]
fn canonical_identity_invalidation_during_preflight_is_not_ignored() -> AnyResult<()> {
    let fixture = Fixture::new()?;
    let db = fixture.db.clone();
    fixture.endpoint.arm(move || {
        FrankenStorage::open(&db)?
            .mark_semantic_identity_rebuild_required(SemanticIdentityTier::Quality)
    });
    assert_stale(&fixture.load(true), "canonical archive changed");
    fixture.endpoint.assert_only_public_preflight();
    let current = FrankenStorage::open_strict_readonly(&fixture.db)?;
    assert!(current.semantic_identity_rebuild_required(SemanticIdentityTier::Quality)?);
    Ok(())
}

#[test]
fn corrupt_ledger_during_preflight_is_never_repaired_by_a_query() -> AnyResult<()> {
    let fixture = Fixture::new()?;
    let path = SemanticManifest::path(fixture.dir.path());
    let changed = path.clone();
    fixture.endpoint.arm(move || {
        fs::write(changed, b"{truncated concurrent publication")?;
        Ok(())
    });
    let setup = fixture.load(true);
    assert!(setup.context.is_none());
    match setup.availability {
        SemanticAvailability::LoadFailed { context } => {
            assert!(context.contains("external semantic manifest"), "{context}");
        }
        other => panic!("unexpected failure: {other:?}"),
    }
    fixture.endpoint.assert_only_public_preflight();
    assert_eq!(fs::read(path)?, b"{truncated concurrent publication");
    Ok(())
}

#[test]
fn selected_generation_during_preflight_still_requires_its_owner() -> AnyResult<()> {
    let fixture = Fixture::new()?;
    let pointer = SemanticCurrentPointerV1::path(fixture.dir.path());
    let changed = pointer.clone();
    fixture.endpoint.arm(move || {
        fs::write(changed, b"requires the immutable-generation reader")?;
        Ok(())
    });
    let setup = fixture.load(true);
    assert!(setup.context.is_none());
    match setup.availability {
        SemanticAvailability::LoadFailed { context } => {
            assert_eq!(
                context,
                SemanticProgressiveUnavailableReason::OwnerBackedReaderRequired.code()
            );
        }
        other => panic!("unexpected failure: {other:?}"),
    }
    fixture.endpoint.assert_only_public_preflight();
    assert_eq!(
        fs::read(pointer)?,
        b"requires the immutable-generation reader"
    );
    Ok(())
}

#[test]
fn unrelated_fast_checkpoint_does_not_revoke_current_quality() -> AnyResult<()> {
    let fixture = Fixture::new()?;
    let record = fixture.record.clone();
    let saved = fixture.mutate_manifest(move |manifest| {
        manifest.checkpoint = Some(checkpoint(&record, TierKind::Fast, "fnv1a-384".into()));
    });
    let setup = fixture.load(true);
    assert!(matches!(
        setup.availability,
        SemanticAvailability::Ready { .. }
    ));
    assert!(setup.context.is_some());
    fixture.endpoint.assert_only_public_preflight();
    fixture.assert_manifest_retained(&saved)?;
    Ok(())
}
