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
    #[cfg(unix)]
    fn covered() -> AnyResult<Self> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("agent_search.db");
        let storage = FrankenStorage::open(&db)?;
        add_conversation(&storage, 1)?;
        add_conversation(&storage, 2)?;
        let endpoint = Endpoint::start()?;
        let manifest = publish_external(&storage, dir.path(), &endpoint)?;
        let record = manifest
            .quality_tier
            .context("published external quality")?;
        let certificate = manifest
            .coverage
            .get(&endpoint.config.identity())
            .context("external producer did not certify canonical coverage")?;
        assert!(certificate.matches_artifact(&record));
        assert!(record.index_path.starts_with("vector_index/served/"));
        endpoint.seen.lock().unwrap().clear();
        drop(storage);
        Ok(Self {
            endpoint,
            dir,
            db,
            record,
        })
    }

    #[cfg(unix)]
    fn client(&self) -> AnyResult<crate::search::query::SearchClient> {
        use crate::search::query::{SearchClient, SearchClientOptions};
        let setup = self.load(true);
        let context = setup
            .context
            .with_context(|| format!("external admission: {:?}", setup.availability))?;
        let client = SearchClient::open_semantic(
            &self.db,
            SearchClientOptions {
                enable_reload: false,
                enable_warm: false,
                strict_read_only: true,
            },
        )?
        .context("canonical fixture archive")?;
        client.set_semantic_artifacts_context(
            context.embedder,
            context.artifacts,
            context.quality_artifact,
            context.filter_maps,
            context.roles,
        )?;
        Ok(client)
    }

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

#[cfg(unix)]
fn publish_external(
    storage: &FrankenStorage,
    root: &Path,
    endpoint: &Endpoint,
) -> AnyResult<SemanticManifest> {
    use crate::indexer::semantic::{SemanticBackfillStoragePlan, SemanticIndexer};
    let indexer =
        SemanticIndexer::with_external_config(endpoint.config.clone(), Arc::new(|| false))?;
    let mut manifest = SemanticManifest::load_or_default(root)?;
    let outcome = indexer.run_backfill_from_storage(
        storage,
        root,
        &mut manifest,
        SemanticBackfillStoragePlan {
            tier: TierKind::Quality,
            db_fingerprint: crate::indexer::lexical_storage_fingerprint_for_storage(storage)?,
            model_revision: endpoint.config.identity(),
            max_conversations: 64,
        },
    )?;
    assert!(outcome.published || outcome.unchanged);
    Ok(manifest)
}

#[cfg(unix)]
fn search_external(
    client: &crate::search::query::SearchClient,
) -> AnyResult<(
    Vec<crate::search::query::SearchHit>,
    SemanticCoverageSnapshot,
)> {
    use crate::search::query::{FieldMask, SearchFilters, SemanticTierMode};
    let (hits, stats, coverage) = client.search_semantic_with_tier_and_coverage(
        "private original admission corpus 1",
        SearchFilters::default(),
        10,
        0,
        FieldMask::FULL,
        false,
        SemanticTierMode::Single,
    )?;
    assert!(stats.is_none(), "external serving keeps exact retrieval");
    Ok((hits, coverage.context("external coverage receipt")?))
}

#[cfg(unix)]
#[test]
fn certified_external_search_survives_append_and_adopts_completed_backfill() -> AnyResult<()> {
    let fixture = Fixture::covered()?;
    let retained = fixture.client()?;
    let (initial, complete) = search_external(&retained)?;
    assert_eq!(initial.len(), 2);
    assert_eq!(complete.status, "complete");
    let storage = FrankenStorage::open(&fixture.db)?;
    add_conversation(&storage, 3)?;
    drop(storage);

    // Both a warm reader and a fresh admission keep the same verified subset.
    // The new message has no vector in that generation and is not advertised.
    for client in [&retained, &fixture.client()?] {
        let (hits, partial) = search_external(client)?;
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| !hit.content.ends_with("corpus 3")));
        assert_eq!(partial.status, "partial");
        assert_eq!(partial.generation, complete.generation);
        assert!(partial.current_max_message_id > complete.current_max_message_id);
        assert!(partial.current_mutation_seq > complete.current_mutation_seq);
    }

    let storage = FrankenStorage::open(&fixture.db)?;
    publish_external(&storage, fixture.dir.path(), &fixture.endpoint)?;
    drop(storage);
    let (current, newer) = search_external(&fixture.client()?)?;
    assert_eq!(current.len(), 3);
    assert_eq!(newer.status, "complete");
    assert_ne!(newer.generation, complete.generation);
    let (old, old_receipt) = search_external(&retained)?;
    assert_eq!(old.len(), 2, "publication cannot replace a retained reader");
    assert_eq!(old_receipt.generation, complete.generation);
    Ok(())
}

#[cfg(unix)]
#[test]
fn certified_external_preflight_still_refuses_a_concurrent_archive_change() -> AnyResult<()> {
    let fixture = Fixture::covered()?;
    let db = fixture.db.clone();
    fixture.endpoint.arm(move || {
        let storage = FrankenStorage::open(&db)?;
        add_conversation(&storage, 3)
    });
    assert_stale(&fixture.load(true), "canonical archive changed");
    fixture.endpoint.assert_only_public_preflight();
    // A new admission may explicitly accept the now-partial covered generation.
    let setup = fixture.load(true);
    let context = setup
        .context
        .context("retry should admit covered history")?;
    assert_eq!(
        context.artifacts[0].coverage_snapshot().unwrap().status,
        "partial"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn certified_external_revisions_are_excluded_and_forget_fences_retained_readers() -> AnyResult<()> {
    use crate::storage::sqlite::ForgetConversationsFilter;
    let fixture = Fixture::covered()?;
    let storage = FrankenStorage::open(&fixture.db)?;
    let agent = storage.ensure_agent(&Agent {
        id: None,
        slug: "codebuff".into(),
        name: "Codebuff".into(),
        version: None,
        kind: AgentKind::Cli,
    })?;
    let mut conversation = Conversation {
        id: None,
        agent_slug: "codebuff".into(),
        workspace: None,
        external_id: Some("external-revision".into()),
        title: None,
        source_path: "/fixture/external-revision.json".into(),
        started_at: Some(1),
        ended_at: Some(2),
        approx_tokens: None,
        metadata_json: json!({}),
        source_id: "local".into(),
        origin_host: None,
        messages: vec![Message {
            id: None,
            idx: 0,
            role: MessageRole::Agent,
            author: None,
            created_at: Some(1),
            content: "private original admission corpus revision".into(),
            extra_json: json!({
                "id": "native-0", "codebuff_message_id": "native-0",
                "variant": "ai", "isComplete": false,
                "blocks": [{"type": "text", "content": "private original admission corpus revision"}]
            }),
            snippets: Vec::new(),
        }],
    };
    storage.insert_conversation_tree_with_analytics(agent, None, &conversation, false)?;
    publish_external(&storage, fixture.dir.path(), &fixture.endpoint)?;
    drop(storage);
    let retained = fixture.client()?;
    assert_eq!(search_external(&retained)?.0.len(), 3);

    conversation.messages[0].content = "a changed message has no published embedding".into();
    conversation.messages[0].extra_json["isComplete"] = json!(true);
    conversation.messages[0].extra_json["blocks"][0]["content"] =
        json!(conversation.messages[0].content);
    let storage = FrankenStorage::open(&fixture.db)?;
    let revised =
        storage.insert_conversation_tree_with_analytics(agent, None, &conversation, false)?;
    assert_eq!(revised.updated_indices, vec![0]);
    drop(storage);
    for client in [&retained, &fixture.client()?] {
        let (hits, coverage) = search_external(client)?;
        assert_eq!(
            hits.len(),
            2,
            "the old vector cannot return rewritten content"
        );
        assert!(hits.iter().all(|hit| hit.agent != "codebuff"));
        assert!(coverage.excluded_candidates >= 1);
        assert_eq!(coverage.status, "partial");
    }

    let storage = FrankenStorage::open(&fixture.db)?;
    storage.forget_conversations(
        &ForgetConversationsFilter {
            source_paths: &["/fixture/admission-1.jsonl".to_owned()],
            ..Default::default()
        },
        false,
    )?;
    assert!(storage.forget_derived_rebuild_pending()?);
    drop(storage);
    let error = search_external(&retained).unwrap_err();
    assert!(
        format!("{error:#}").contains("forget-repair-pending"),
        "covered serving must retain the canonical deletion privacy fence: {error:#}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn certified_external_checkpoint_keeps_the_completed_owner_available() -> AnyResult<()> {
    let fixture = Fixture::covered()?;
    let record = fixture.record.clone();
    let saved = fixture.mutate_manifest(move |manifest| {
        manifest.checkpoint = Some(checkpoint(
            &record,
            TierKind::Quality,
            record.embedder_id.clone(),
        ));
    });
    let setup = fixture.load(true);
    let context = setup
        .context
        .context("covered generation must remain available")?;
    assert_eq!(context.artifacts[0].index().record_count(), 2);
    assert!(context.artifacts[0].coverage_certificate().is_some());
    fixture.endpoint.assert_only_public_preflight();
    fixture.assert_manifest_retained(&saved)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn certified_external_publication_replacement_during_preflight_is_refused() -> AnyResult<()> {
    let fixture = Fixture::covered()?;
    let id = fixture.endpoint.config.identity();
    let saved = fixture.mutate_manifest(move |manifest| {
        manifest.quality_tier.as_mut().unwrap().completed_at_ms += 1;
        manifest.coverage.get_mut(&id).unwrap().published_at_ms += 1;
    });
    assert_stale(&fixture.load(true), "external_admission_changed");
    fixture.endpoint.assert_only_public_preflight();
    fixture.assert_manifest_retained(&saved)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn certified_external_damaged_proof_is_refused_before_http() -> AnyResult<()> {
    let fixture = Fixture::covered()?;
    let manifest = SemanticManifest::load_or_default(fixture.dir.path())?;
    let certificate = manifest
        .coverage
        .get(&fixture.endpoint.config.identity())
        .context("coverage certificate")?;
    let changed = fixture.dir.path().join(&certificate.artifact_path);
    let mut damaged = fs::read(&changed)?;
    *damaged.last_mut().context("nonempty vector artifact")? ^= 1;
    fs::write(&changed, &damaged)?;
    let setup = fixture.load(true);
    assert!(setup.context.is_none());
    match setup.availability {
        SemanticAvailability::LoadFailed { context } => {
            assert!(context.contains("artifact digest mismatch"), "{context}");
        }
        other => panic!("wrong damaged-file refusal: {other:?}"),
    }
    assert!(fixture.endpoint.seen.lock().unwrap().is_empty());
    assert_eq!(
        fs::read(changed)?,
        damaged,
        "query must not repair artifacts"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn unchanged_legacy_external_backfill_upgrades_through_canonical_reconciliation() -> AnyResult<()> {
    let fixture = Fixture::covered()?;
    let id = fixture.endpoint.config.identity();
    let mut manifest = SemanticManifest::load_or_default(fixture.dir.path())?;
    let builder_path = vector_index_path(fixture.dir.path(), &id);
    let record = manifest.quality_tier.as_mut().context("quality artifact")?;
    record.index_path = builder_path
        .strip_prefix(fixture.dir.path())?
        .to_string_lossy()
        .into_owned();
    record.size_bytes = fs::metadata(&builder_path)?.len();
    manifest.coverage.remove(&id);
    manifest.save(fixture.dir.path())?;
    let storage = FrankenStorage::open(&fixture.db)?;
    let upgraded = publish_external(&storage, fixture.dir.path(), &fixture.endpoint)?;
    let certificate = upgraded
        .coverage
        .get(&id)
        .context("legacy certificate upgrade")?;
    assert!(certificate.matches_artifact(upgraded.quality_tier.as_ref().unwrap()));
    // Same canonical documents retain their vectors; publication must not send
    // the unchanged corpus to the provider again merely to attach provenance.
    fixture.endpoint.assert_only_public_preflight();
    add_conversation(&storage, 3)?;
    drop(storage);
    let (hits, coverage) = search_external(&fixture.client()?)?;
    assert_eq!(hits.len(), 2);
    assert_eq!(coverage.status, "partial");
    Ok(())
}

#[test]
fn legacy_external_append_still_requires_certified_republication() -> AnyResult<()> {
    let fixture = Fixture::new()?;
    let storage = FrankenStorage::open(&fixture.db)?;
    add_conversation(&storage, 2)?;
    drop(storage);
    assert_stale(&fixture.load(true), "canonical archive changed");
    assert!(fixture.endpoint.seen.lock().unwrap().is_empty());
    Ok(())
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
