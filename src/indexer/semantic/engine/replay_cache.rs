//! Invocation-local canonical identity replay for repeated bounded backfills.
//!
//! This is a disposable optimization, not a coverage or serving authority. A
//! hit requires the existing descriptor-bound archive/WAL stamps, the exact
//! producer/input context, and an observation outside a database transaction.
//! Every pass still reads the candidate's identities; selected text is reloaded
//! and checked against this inventory before embedding. Nothing is persisted.

use super::*;
use std::mem::size_of;
use std::sync::Mutex;

#[cfg(test)]
mod tests;

const MAX_RETAINED_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct ReplayKey {
    archive: BackfillFilePair,
    embedder_id: String,
    vector_revision: String,
    model_revision: String,
    db_fingerprint: String,
}

impl ReplayKey {
    fn capture(
        indexer: &SemanticIndexer,
        storage: &FrankenStorage,
        plan: &SemanticBackfillStoragePlan,
    ) -> Result<Option<Self>> {
        let vector_revision = indexer.vector_space_revision()?;
        Ok(BackfillFilePair::archive(storage).map(|archive| Self {
            archive,
            embedder_id: indexer.embedder_id().into(),
            vector_revision: vector_revision.into(),
            model_revision: plan.model_revision.clone(),
            db_fingerprint: plan.db_fingerprint.clone(),
        }))
    }
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
        self.documents.iter().try_fold(
            size_of::<Self>().checked_mul(2)?,
            |bytes, document| {
                bytes.checked_add(size_of::<DocumentIdentity>())?
                    .checked_add(document.id.len())
            },
        )
    }
}

struct CachedReplay {
    key: ReplayKey,
    conversations: Vec<ConversationIdentity>,
    retained_bytes: usize,
}

/// One inventory per indexer, released with the indexer. The slot is taken
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

/// Keep the observation that authorized a hit through the final embedding
/// batch. A changed or unavailable stamp is never permission to publish from
/// an older cached inventory. The uncached path retains its existing behavior.
pub(super) struct ReplayProof(Option<BackfillFilePair>);

impl ReplayProof {
    pub(super) fn verify(&self, storage: &FrankenStorage) -> Result<()> {
        if let Some(expected) = &self.0 {
            anyhow::ensure!(
                BackfillFilePair::archive(storage).as_ref() == Some(expected),
                "canonical archive changed during cached semantic replay; retry backfill"
            );
        }
        Ok(())
    }
}

fn project_conversation(
    storage: &FrankenStorage,
    conversation_id: i64,
) -> Result<(ConversationIdentity, Vec<EmbeddingInput>)> {
    let (inputs, _) = packet_embedding_inputs_from_selected_canonical_messages(
        storage, &[conversation_id], |_| true,
    )?;
    let mut documents = Vec::new();
    let mut projected_inputs = Vec::new();
    for input in inputs {
        if let Some(id) = semantic_doc_id_for_input(&input) {
            documents.push(DocumentIdentity {
                id: id.into_boxed_str(),
                message_id: input.message_id,
                content_bytes: input.content.len(),
            });
            projected_inputs.push(input);
        }
    }
    Ok((ConversationIdentity {
        conversation_id,
        documents: documents.into_boxed_slice(),
    }, projected_inputs))
}

impl ReconcileSelection {
    #[allow(clippy::too_many_arguments)]
    fn visit(
        &mut self,
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
            anyhow::ensure!(self.current_ids.insert(document.id.to_string()),
                "canonical semantic backfill produced duplicate document {}", document.id);
            if !existing_ids.contains(document.id.as_ref()) {
                missing_count += 1;
                missing_messages.insert(document.message_id);
                missing_bytes = missing_bytes.saturating_add(
                    saturating_u64_from_usize(document.content_bytes),
                );
            }
        }
        // Identical to the existing whole-conversation selection policy,
        // including its first-conversation exception and later-small-item fit.
        let select = missing_count > 0
            && self.selected_conversations < plan.max_conversations.max(1)
            && (self.selected_conversations == 0
                || ((!caps.message_limited()
                    || self.selected_messages.saturating_add(missing_messages.len()) <= caps.max_messages)
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
                    let (observed, inputs) = project_conversation(storage, conversation.conversation_id)?;
                    anyhow::ensure!(observed.documents == conversation.documents,
                        "canonical semantic projection changed since cached replay; retry backfill");
                    self.loaded_selected_conversations += 1;
                    inputs
                }
            };
            anyhow::ensure!(inputs.len() == conversation.documents.len(),
                "canonical semantic projection lost its input binding");
            self.selected_conversations += 1;
            self.selected_messages = self.selected_messages.saturating_add(missing_messages.len());
            self.selected_bytes = self.selected_bytes.saturating_add(missing_bytes);
            for (input, document) in inputs.into_iter().zip(conversation.documents.iter()) {
                if !existing_ids.contains(document.id.as_ref()) {
                    let id = i64::try_from(input.message_id).unwrap_or(i64::MAX);
                    self.last_message_id = Some(self.last_message_id.map_or(id, |prior| prior.max(id)));
                    self.selected_ids.insert(document.id.to_string());
                    self.inputs.push(input);
                }
            }
        }
        sink.emit(SemanticProgressEvent::PacketReplayProgress, SemanticProgressFields {
            last_conversation_id: Some(conversation.conversation_id),
            rows_processed: Some(saturating_u64_from_usize(self.current_ids.len())),
            conversations_in_batch: Some(saturating_u64_from_usize(self.selected_conversations)),
            ..Default::default()
        });
        Ok(())
    }
}

impl CanonicalReplayCache {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn select(
        &self,
        indexer: &SemanticIndexer,
        storage: &FrankenStorage,
        plan: &SemanticBackfillStoragePlan,
        existing_ids: &HashSet<String>,
        caps: SemanticCheckpointCaps,
        sink: &SemanticProgressSink,
    ) -> Result<(ReconcileSelection, ReplayProof)> {
        let key = ReplayKey::capture(indexer, storage, plan)?;
        let budget = self.max_bytes.min(responsiveness::effective_inflight_byte_limit(self.max_bytes));
        // Poison only disables the optimization, never canonical reconciliation.
        let cached = self.slot.lock().ok().and_then(|mut slot| slot.take())
            .filter(|cached| budget > 0 && cached.retained_bytes <= budget
                && key.as_ref() == Some(&cached.key));
        let cache_hit = cached.is_some();
        let proof = ReplayProof(cached.as_ref().map(|cached| cached.key.archive.clone()));
        let mut selected = ReconcileSelection::default();
        let inventory = if let Some(cached) = cached {
            for conversation in &cached.conversations {
                selected.visit(storage, conversation, None, existing_ids, plan, caps, sink)?;
            }
            Some(cached)
        } else {
            let mut building = key.clone().filter(|_| budget > 0).map(|key| CachedReplay {
                key, conversations: Vec::new(), retained_bytes: 0,
            });
            let mut after_id = 0i64;
            loop {
                let ids: Vec<i64> = storage.raw().query_map_collect(
                    "SELECT id FROM conversations WHERE id > ?1 ORDER BY id LIMIT ?2",
                    &[ParamValue::from(after_id),
                      ParamValue::from(DEFAULT_SEMANTIC_RECONCILIATION_SCAN_CONVERSATIONS as i64)],
                    |row| row.get_typed(0),
                )?;
                if ids.is_empty() { break; }
                for id in ids {
                    anyhow::ensure!(id > after_id, "canonical semantic replay cursor did not advance");
                    let (conversation, inputs) = project_conversation(storage, id)?;
                    selected.replayed_conversations += 1;
                    selected.visit(storage, &conversation, Some(inputs), existing_ids, plan, caps, sink)?;
                    if let Some(cache) = building.as_mut() {
                        let new_bytes = conversation.retained_bytes()
                            .and_then(|bytes| cache.retained_bytes.checked_add(bytes));
                        if let Some(bytes) = new_bytes.filter(|bytes| *bytes <= budget) {
                            if cache.conversations.try_reserve(1).is_ok() {
                                cache.retained_bytes = bytes;
                                cache.conversations.push(conversation);
                            } else {
                                building = None;
                            }
                        } else {
                            // Do not retain a partial inventory or replay it again.
                            // The current uncached scan continues to completion.
                            building = None;
                        }
                    }
                    after_id = id;
                }
            }
            building
        };
        proof.verify(storage)?;
        let retained_bytes = inventory.as_ref().map_or(0, |cache| cache.retained_bytes);
        if let Some(inventory) = inventory
            && ReplayKey::capture(indexer, storage, plan)?.as_ref() == Some(&inventory.key)
            && let Ok(mut slot) = self.slot.lock()
        {
            *slot = Some(inventory);
        }
        tracing::debug!(cache_hit,
            canonical_conversations_replayed = selected.replayed_conversations,
            selected_conversations_reloaded = selected.loaded_selected_conversations,
            retained_identity_bytes = retained_bytes,
            "canonical semantic replay selection complete");
        Ok((selected, proof))
    }
}
