//! Bounded canonical identity replay for repeated backfill checkpoints.
//!
//! This is a disposable optimization, not a coverage or serving authority. A
//! hit requires the existing descriptor-bound archive/WAL stamps, the exact
//! producer/input context, and an observation outside a database transaction.
//! Every pass still reads the candidate's identities; selected text is reloaded
//! and checked against this inventory before embedding. A checksummed inventory
//! can survive process restarts while the exact archive and input contract stay
//! unchanged, including when it is too large to retain in memory. A complete
//! canonical mutation journal can refresh only changed conversations; missing
//! authority, unexplained file-stamp changes, or a producer change require
//! fresh replay. Journal deltas cover the canonical store's supported writers;
//! they never replace candidate hydration or publication correctness checks.

use super::*;
use crate::storage::sqlite::SemanticArchiveState;
use std::mem::size_of;
use std::sync::Mutex;

mod persist;

#[cfg(test)]
mod tests;

const MAX_RETAINED_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ReplayKey {
    archive: BackfillFilePair,
    #[serde(default)]
    journal: Option<SemanticArchiveState>,
    embedder_id: String,
    vector_revision: String,
    model_revision: String,
    db_fingerprint: String,
    schema_version: u32,
    chunking_version: u32,
    source_ingest_contract: String,
}

impl ReplayKey {
    fn capture(
        indexer: &SemanticIndexer,
        storage: &FrankenStorage,
        plan: &SemanticBackfillStoragePlan,
    ) -> Result<Option<Self>> {
        let vector_revision = indexer.vector_space_revision()?;
        let Some(archive) = BackfillFilePair::archive(storage) else {
            return Ok(None);
        };
        // Old archives and malformed optional journal metadata retain the
        // canonical replay path. Never create authority on a read-only probe.
        let journal = match storage.semantic_archive_state() {
            Ok(state) => state,
            Err(error) => {
                tracing::debug!(%error, "semantic replay journal unavailable");
                None
            }
        };
        anyhow::ensure!(
            BackfillFilePair::archive(storage).as_ref() == Some(&archive),
            "canonical archive changed while observing semantic replay; retry backfill"
        );
        Ok(Some(Self {
            archive,
            journal,
            embedder_id: indexer.embedder_id().into(),
            vector_revision: vector_revision.into(),
            model_revision: plan.model_revision.clone(),
            db_fingerprint: plan.db_fingerprint.clone(),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_version: CHUNKING_STRATEGY_VERSION,
            source_ingest_contract: env!("CASS_SOURCE_INGEST_CONTRACT").into(),
        }))
    }

    /// A journal is useful only inside the same physical archive incarnation
    /// and producer contract. Equal sequence with changed stamps is deliberately
    /// a miss: a raw SQL edit must not become an exact-cache hit.
    fn reuse_from(&self, previous: &Self, storage: &FrankenStorage) -> Result<Option<ReplayReuse>> {
        if self == previous {
            return Ok(Some(ReplayReuse::Exact));
        }
        if self.archive.path != previous.archive.path
            || self.archive.main.identity != previous.archive.main.identity
            || self.embedder_id != previous.embedder_id
            || self.vector_revision != previous.vector_revision
            || self.model_revision != previous.model_revision
            || self.schema_version != previous.schema_version
            || self.chunking_version != previous.chunking_version
            || self.source_ingest_contract != previous.source_ingest_contract
            || !semantic_backfill_identity_matches(&previous.db_fingerprint, &self.db_fingerprint)
        {
            return Ok(None);
        }
        let (Some(current), Some(prior)) = (&self.journal, &previous.journal) else {
            return Ok(None);
        };
        if current.archive_id != prior.archive_id || current.mutation_seq <= prior.mutation_seq {
            return Ok(None);
        }
        let Some(changes) = storage.semantic_conversation_changes_since(prior)? else {
            return Ok(None);
        };
        if !changes.complete || changes.state != *current || changes.conversation_ids.is_empty() {
            return Ok(None);
        }
        anyhow::ensure!(
            changes.conversation_ids.iter().all(|id| *id > 0)
                && changes
                    .conversation_ids
                    .windows(2)
                    .all(|pair| pair[0] < pair[1]),
            "semantic replay journal is not strictly ordered"
        );
        Ok(Some(ReplayReuse::Journal(changes.conversation_ids)))
    }
}

enum ReplayReuse {
    Exact,
    Journal(Vec<i64>),
}

#[derive(Debug, PartialEq, Eq)]
struct DocumentIdentity {
    id: Box<str>,
    message_id: u64,
    content_bytes: usize,
}

struct ConversationIdentity {
    conversation_id: i64,
    documents: Box<[DocumentIdentity]>,
}

impl ConversationIdentity {
    // Account for string payloads, identity records, and conservative outer
    // vector growth. This is a retained-payload budget, not an allocator/RSS
    // promise. The current conversation and output sets predate this cache.
    fn retained_bytes(&self) -> Option<usize> {
        self.documents
            .iter()
            .try_fold(size_of::<Self>().checked_mul(2)?, |bytes, document| {
                bytes
                    .checked_add(size_of::<DocumentIdentity>())?
                    .checked_add(document.id.len())
            })
    }
}

struct CachedReplay {
    key: ReplayKey,
    conversations: Vec<ConversationIdentity>,
    retained_bytes: usize,
}

enum ReplayInventory {
    Memory(std::vec::IntoIter<ConversationIdentity>),
    Disk(Box<persist::InventoryReader>),
}

impl ReplayInventory {
    fn next(&mut self, indexer: &SemanticIndexer) -> Result<Option<ConversationIdentity>> {
        indexer.check_external_cancelled()?;
        match self {
            Self::Memory(conversations) => Ok(conversations.next()),
            Self::Disk(reader) => reader.next_conversation(|| indexer.check_external_cancelled()),
        }
    }
}

fn retain_conversation(
    building: &mut Option<CachedReplay>,
    conversation: ConversationIdentity,
    budget: usize,
) {
    let Some(cache) = building.as_mut() else {
        return;
    };
    let bytes = conversation
        .retained_bytes()
        .and_then(|bytes| cache.retained_bytes.checked_add(bytes));
    let Some(bytes) = bytes.filter(|bytes| *bytes <= budget) else {
        // A partial memory inventory cannot be reused. The independent disk
        // writer can still retain the complete stream without this allocation.
        *building = None;
        return;
    };
    let reserved = if cache.conversations.is_empty() {
        cache.conversations.try_reserve_exact(1)
    } else {
        cache.conversations.try_reserve(1)
    };
    if reserved.is_err() {
        *building = None;
        return;
    }
    cache.retained_bytes = bytes;
    cache.conversations.push(conversation);
}

fn stage_inventory(
    indexer: &SemanticIndexer,
    data_dir: &Path,
    path: Option<&Path>,
    key: Option<&ReplayKey>,
    budget: usize,
) -> Result<Option<persist::InventoryWriter>> {
    let (Some(path), Some(key)) = (path, key) else {
        return Ok(None);
    };
    let staged = (|| -> Result<persist::InventoryWriter> {
        fs::create_dir_all(data_dir.join(VECTOR_INDEX_DIR))?;
        persist::InventoryWriter::create(path, key, budget, || indexer.check_external_cancelled())
    })();
    indexer.check_external_cancelled()?;
    match staged {
        Ok(writer) => Ok(Some(writer)),
        Err(error) => {
            tracing::debug!(%error, "semantic replay inventory could not be staged");
            Ok(None)
        }
    }
}

/// One memory inventory per indexer, released with the indexer. The slot is taken
/// before I/O: concurrent callers may do redundant work, but cannot share a
/// mutable scan or use a partially constructed inventory.
pub(super) struct CanonicalReplayCache {
    slot: Mutex<Option<CachedReplay>>,
    max_bytes: usize,
}

impl Default for CanonicalReplayCache {
    fn default() -> Self {
        Self {
            slot: Mutex::new(None),
            max_bytes: resolved_env_usize("CASS_SEMANTIC_REPLAY_CACHE_BYTES", MAX_RETAINED_BYTES)
                .min(MAX_RETAINED_BYTES),
        }
    }
}

#[derive(Default)]
pub(super) struct ReconcileSelection {
    pub(super) current_ids: HashSet<String>,
    pub(super) selected_ids: HashSet<String>,
    pub(super) inputs: Vec<EmbeddingInput>,
    pub(super) selected_conversations: usize,
    pub(super) covered_conversations: u64,
    pub(super) last_covered_conversation: i64,
    pub(super) last_message_id: Option<i64>,
    selected_messages: usize,
    selected_bytes: u64,
    replayed_conversations: usize,
    loaded_selected_conversations: usize,
}

/// Keep a stable archive observation through replay and the final embedding
/// batch, whether the inventory was cached or freshly read. A changed or
/// unavailable stamp cannot authorize publication from either inventory.
/// Platforms and explicit transactions without an observation retain the
/// existing uncached reconciliation path.
pub(super) struct ReplayProof(Option<BackfillFilePair>);

impl ReplayProof {
    pub(super) fn verify(&self, storage: &FrankenStorage) -> Result<()> {
        if let Some(expected) = &self.0 {
            anyhow::ensure!(
                BackfillFilePair::archive(storage).as_ref() == Some(expected),
                "canonical archive changed during semantic replay; retry backfill"
            );
        }
        Ok(())
    }
}

fn project_conversation(
    indexer: &SemanticIndexer,
    storage: &FrankenStorage,
    conversation_id: i64,
    retain_inputs: bool,
) -> Result<(ConversationIdentity, Vec<EmbeddingInput>)> {
    let mut documents = Vec::new();
    let mut projected_inputs = Vec::new();
    super::super::replay::visit_packet_embedding_inputs_for_conversation(
        storage,
        conversation_id,
        |input| {
            if let Some(id) = semantic_doc_id_for_input(&input) {
                documents.push(DocumentIdentity {
                    id: id.into_boxed_str(),
                    message_id: input.message_id,
                    content_bytes: input.content.len(),
                });
                if retain_inputs {
                    projected_inputs.push(input);
                }
            }
            Ok(())
        },
        || indexer.check_external_cancelled(),
    )?;
    Ok((
        ConversationIdentity {
            conversation_id,
            documents: documents.into_boxed_slice(),
        },
        projected_inputs,
    ))
}

impl ReconcileSelection {
    #[allow(clippy::too_many_arguments)]
    fn visit(
        &mut self,
        indexer: &SemanticIndexer,
        storage: &FrankenStorage,
        conversation: &ConversationIdentity,
        fresh_inputs: Option<Vec<EmbeddingInput>>,
        existing_ids: &HashSet<String>,
        plan: &SemanticBackfillStoragePlan,
        caps: SemanticCheckpointCaps,
        sink: &SemanticProgressSink,
    ) -> Result<()> {
        let mut missing_messages = HashSet::new();
        let mut missing_bytes = 0u64;
        let mut missing_count = 0usize;
        for document in conversation.documents.iter() {
            anyhow::ensure!(
                self.current_ids.insert(document.id.to_string()),
                "canonical semantic backfill produced duplicate document {}",
                document.id
            );
            if !existing_ids.contains(document.id.as_ref()) {
                missing_count += 1;
                missing_messages.insert(document.message_id);
                missing_bytes =
                    missing_bytes.saturating_add(saturating_u64_from_usize(document.content_bytes));
            }
        }
        // Identical to the existing whole-conversation selection policy,
        // including its first-conversation exception and later-small-item fit.
        let select = missing_count > 0
            && self.selected_conversations < plan.max_conversations.max(1)
            && (self.selected_conversations == 0
                || ((!caps.message_limited()
                    || self
                        .selected_messages
                        .saturating_add(missing_messages.len())
                        <= caps.max_messages)
                    && (!caps.byte_limited()
                        || self.selected_bytes.saturating_add(missing_bytes) <= caps.max_bytes)));
        if missing_count == 0 || select {
            self.covered_conversations = self.covered_conversations.saturating_add(1);
            self.last_covered_conversation = conversation.conversation_id;
        }
        if select {
            let inputs = match fresh_inputs {
                Some(inputs) => inputs,
                None => {
                    let (observed, inputs) =
                        project_conversation(indexer, storage, conversation.conversation_id, true)?;
                    anyhow::ensure!(
                        observed.documents == conversation.documents,
                        "canonical semantic projection changed since cached replay; retry backfill"
                    );
                    self.loaded_selected_conversations += 1;
                    inputs
                }
            };
            anyhow::ensure!(
                inputs.len() == conversation.documents.len(),
                "canonical semantic projection lost its input binding"
            );
            self.selected_conversations += 1;
            self.selected_messages = self
                .selected_messages
                .saturating_add(missing_messages.len());
            self.selected_bytes = self.selected_bytes.saturating_add(missing_bytes);
            for (input, document) in inputs.into_iter().zip(conversation.documents.iter()) {
                if !existing_ids.contains(document.id.as_ref()) {
                    let id = i64::try_from(input.message_id).unwrap_or(i64::MAX);
                    self.last_message_id =
                        Some(self.last_message_id.map_or(id, |prior| prior.max(id)));
                    self.selected_ids.insert(document.id.to_string());
                    self.inputs.push(input);
                }
            }
        }
        sink.emit(
            SemanticProgressEvent::PacketReplayProgress,
            SemanticProgressFields {
                last_conversation_id: Some(conversation.conversation_id),
                rows_processed: Some(saturating_u64_from_usize(self.current_ids.len())),
                conversations_in_batch: Some(saturating_u64_from_usize(
                    self.selected_conversations,
                )),
                ..Default::default()
            },
        );
        Ok(())
    }
}

impl CanonicalReplayCache {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn select(
        &self,
        indexer: &SemanticIndexer,
        storage: &FrankenStorage,
        data_dir: &Path,
        plan: &SemanticBackfillStoragePlan,
        existing_ids: &HashSet<String>,
        caps: SemanticCheckpointCaps,
        sink: &SemanticProgressSink,
    ) -> Result<(ReconcileSelection, ReplayProof)> {
        indexer.check_external_cancelled()?;
        let key = ReplayKey::capture(indexer, storage, plan)?;
        let budget = self
            .max_bytes
            .min(responsiveness::effective_inflight_byte_limit(
                self.max_bytes,
            ));
        // Poison only disables the optimization, never canonical reconciliation.
        let cached = self
            .slot
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .filter(|cached| budget > 0 && cached.retained_bytes <= budget);
        // A local path dependency can change canonicalization without changing
        // the recorded build inputs. Keep the in-memory optimization, but never
        // trust a previous process's inventory in such development builds.
        let cache_path = (budget > 0 && env!("CASS_SOURCE_INGEST_REUSE") == "true")
            .then(|| persistent_path(data_dir, plan.tier));
        let reuse = |previous: &ReplayKey| -> Option<ReplayReuse> {
            match key.as_ref()?.reuse_from(previous, storage) {
                Ok(reuse) => reuse,
                Err(error) => {
                    tracing::debug!(%error, "semantic replay journal rejected; reading canonical archive");
                    None
                }
            }
        };
        let mut source = cached.and_then(|cached| {
            reuse(&cached.key).map(|reuse| {
                (
                    ReplayInventory::Memory(cached.conversations.into_iter()),
                    reuse,
                )
            })
        });
        if source.is_none()
            && key.is_some()
            && let Some(path) = cache_path.as_ref()
        {
            match persist::InventoryReader::open_with_key(path, budget, || {
                indexer.check_external_cancelled()
            }) {
                Ok(Some((previous, reader))) => {
                    source = reuse(&previous)
                        .map(|reuse| (ReplayInventory::Disk(Box::new(reader)), reuse));
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(%error, "semantic replay inventory unavailable; reading canonical archive");
                }
            }
            // A cancellation reported by the optional cache reader must not
            // turn into an expensive uncached replay.
            indexer.check_external_cancelled()?;
        }
        let proof = ReplayProof(key.as_ref().map(|key| key.archive.clone()));
        let mut selected = ReconcileSelection::default();
        let new_memory_inventory = || {
            key.clone().filter(|_| budget > 0).map(|key| CachedReplay {
                key,
                conversations: Vec::new(),
                retained_bytes: 0,
            })
        };
        let mut inventory = new_memory_inventory();
        let mut cache_hit = false;
        let mut disk_hit = false;
        let mut journal_hit = false;
        let mut writer = None;
        let visit = |selected: &mut ReconcileSelection,
                     conversation: ConversationIdentity,
                     fresh_inputs: Option<Vec<EmbeddingInput>>,
                     inventory: &mut Option<CachedReplay>,
                     writer: &mut Option<persist::InventoryWriter>|
         -> Result<()> {
            selected.visit(
                indexer,
                storage,
                &conversation,
                fresh_inputs,
                existing_ids,
                plan,
                caps,
                sink,
            )?;
            if let Some(staged) = writer.as_mut()
                && let Err(error) =
                    staged.push(&conversation, || indexer.check_external_cancelled())
            {
                tracing::debug!(%error, "semantic replay inventory exceeded its bounds or could not be written");
                *writer = None;
                indexer.check_external_cancelled()?;
            }
            retain_conversation(inventory, conversation, budget);
            Ok(())
        };
        if let Some((mut source, reuse)) = source {
            let from_disk = matches!(&source, ReplayInventory::Disk(_));
            let (changed, from_journal) = match reuse {
                ReplayReuse::Exact => (Vec::new(), false),
                ReplayReuse::Journal(changed) => (changed, true),
            };
            if from_journal {
                writer = stage_inventory(
                    indexer,
                    data_dir,
                    cache_path.as_deref(),
                    key.as_ref(),
                    budget,
                )?;
            }
            let replayed = (|| -> Result<()> {
                let mut changed = changed.into_iter().peekable();
                let mut saved = source.next(indexer)?;
                while saved.is_some() || changed.peek().is_some() {
                    indexer.check_external_cancelled()?;
                    let use_changed = changed.peek().is_some_and(|id| {
                        saved.as_ref().is_none_or(|old| *id <= old.conversation_id)
                    });
                    let (conversation, inputs) = if use_changed {
                        let id = changed
                            .next()
                            .context("semantic replay lost its dirty cursor")?;
                        if saved.as_ref().is_some_and(|old| old.conversation_id == id) {
                            // Consume even discarded frames so a corrupt old
                            // inventory cannot authenticate a merged prefix.
                            saved = source.next(indexer)?;
                        }
                        let exists: i64 = storage.raw().query_row_map(
                            "SELECT EXISTS(SELECT 1 FROM conversations WHERE id = ?1)",
                            &[ParamValue::from(id)],
                            |row| row.get_typed(0),
                        )?;
                        if exists == 0 {
                            continue;
                        }
                        // A dirty parent may exceed caps or already be
                        // covered. Retain text only for the selected batch,
                        // just as on a full canonical replay.
                        let (conversation, _) = project_conversation(indexer, storage, id, false)?;
                        selected.replayed_conversations += 1;
                        (conversation, None)
                    } else {
                        let conversation = saved
                            .take()
                            .context("semantic replay lost its saved cursor")?;
                        saved = source.next(indexer)?;
                        (conversation, None)
                    };
                    visit(
                        &mut selected,
                        conversation,
                        inputs,
                        &mut inventory,
                        &mut writer,
                    )?;
                }
                // Disk records remain tentative until counts, checksum, and
                // EOF agree. No partially checked inputs escape this scope.
                proof.verify(storage)
            })();
            match replayed {
                Ok(()) => {
                    cache_hit = true;
                    disk_hit = from_disk;
                    journal_hit = from_journal;
                }
                Err(error) => {
                    indexer.check_external_cancelled()?;
                    proof.verify(storage)?;
                    tracing::debug!(%error, "semantic replay inventory rejected; discarding tentative selection");
                    selected = ReconcileSelection::default();
                    inventory = new_memory_inventory();
                    writer = None;
                }
            }
        }
        if !cache_hit {
            writer = stage_inventory(
                indexer,
                data_dir,
                cache_path.as_deref(),
                key.as_ref(),
                budget,
            )?;
            let mut after_id = None;
            loop {
                indexer.check_external_cancelled()?;
                // All signed SQLite rowids are valid canonical parents. A
                // sentinel of zero would discard nonpositive IDs; arithmetic
                // at either end of the rowid range can overflow.
                let limit = DEFAULT_SEMANTIC_RECONCILIATION_SCAN_CONVERSATIONS as i64;
                let (sql, params) = match after_id {
                    None => (
                        "SELECT id FROM conversations ORDER BY id LIMIT ?1",
                        vec![ParamValue::from(limit)],
                    ),
                    Some(id) => (
                        "SELECT id FROM conversations WHERE id > ?1 ORDER BY id LIMIT ?2",
                        vec![ParamValue::from(id), ParamValue::from(limit)],
                    ),
                };
                let ids: Vec<i64> = storage
                    .raw()
                    .query_map_collect(sql, &params, |row| row.get_typed(0))?;
                if ids.is_empty() {
                    break;
                }
                for id in ids {
                    indexer.check_external_cancelled()?;
                    anyhow::ensure!(
                        after_id.is_none_or(|previous| id > previous),
                        "canonical semantic replay cursor did not advance"
                    );
                    let (conversation, _) = project_conversation(indexer, storage, id, false)?;
                    selected.replayed_conversations += 1;
                    visit(
                        &mut selected,
                        conversation,
                        None,
                        &mut inventory,
                        &mut writer,
                    )?;
                    after_id = Some(id);
                }
            }
        }
        indexer.check_external_cancelled()?;
        proof.verify(storage)?;
        let retained_bytes = inventory.as_ref().map_or(0, |cache| cache.retained_bytes);
        if ReplayKey::capture(indexer, storage, plan)? == key {
            // A cold or journal pass streamed every frame to private scratch.
            // Install only after source verification; an exact hit writes none.
            if let Some(writer) = writer {
                if let Err(error) = writer.finish(|| indexer.check_external_cancelled()) {
                    tracing::debug!(%error, "semantic replay inventory was not persisted; memory reuse remains available");
                }
                indexer.check_external_cancelled()?;
            }
            if let Some(inventory) = inventory
                && let Ok(mut slot) = self.slot.lock()
            {
                *slot = Some(inventory);
            }
        }
        // Persisting a large inventory is optional I/O, but its time still
        // belongs to this selection. Refuse a concurrent write before handing
        // inputs to an expensive embedding batch.
        proof.verify(storage)?;
        tracing::debug!(
            cache_hit,
            disk_hit,
            journal_hit,
            canonical_conversations_replayed = selected.replayed_conversations,
            selected_conversations_reloaded = selected.loaded_selected_conversations,
            retained_identity_bytes = retained_bytes,
            "canonical semantic replay selection complete"
        );
        Ok((selected, proof))
    }
}

fn persistent_path(data_dir: &Path, tier: TierKind) -> PathBuf {
    // One replaceable cache per tier bounds disk growth. The serialized key,
    // not this pathname, binds the selected archive and producer identities.
    data_dir
        .join(VECTOR_INDEX_DIR)
        .join(format!(".canonical-replay-{}.bin", tier.as_str()))
}
