//! Durable lexical recovery across ordinary canonical archive growth (GH #496).
//!
//! A saved cursor is not evidence that an old conversation is unchanged. Pin
//! the candidate's native committed snapshot, stream the canonical archive in
//! bounded batches, and compare every retained document's complete projection.
//! Only missing identities are indexed. Every old live document must be seen
//! exactly once; an edit, deletion or identity move refuses this candidate.
//! The expensive index work survives another interruption, while the next
//! attempt repeats the bounded audit instead of trusting a stale cursor.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;
#[cfg(unix)]
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::lexical_reconcile::canary::exact::{ContentMatch, PublishedSnapshot};
use super::lexical_reconcile::snapshot::CanonicalSnapshot;
use super::{
    IndexingProgress, LexicalRebuildConversationPacket, LexicalRebuildEquivalenceAccumulator,
    LexicalRebuildExecutionMode, LexicalRebuildOutcome, LexicalRebuildPipelineRuntimeSnapshot,
    LexicalRebuildState,
};
use crate::connectors::source_dependencies::observation::file_observation;
use crate::model::types::Message;
use crate::search::tantivy::{SCHEMA_HASH, TantivyIndex};
use crate::sources::provenance::SourceKind;
use crate::storage::sqlite::{FrankenStorage, LexicalRebuildConversationRow};

const BINDING_VERSION: u32 = 1;
const MAX_BATCH_MESSAGES: usize = 16_384;
const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;
const MAX_COMPLETED_ARTIFACT_BYTES: u64 = 1024 * 1024;

/// Separate stable archive identity from its changing DB/WAL write epoch.
/// Budgets may change on retry; projection semantics may not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    version: u32,
    archive_identity: Value,
    schema_hash: String,
    packet_version: u32,
    content_cap_bytes: usize,
}

impl Binding {
    pub(super) fn capture(db_path: &Path) -> Result<Option<Self>> {
        let path = crate::normalize_path_identity(db_path);
        let metadata = fs::metadata(&path).with_context(|| {
            format!(
                "reading lexical recovery archive identity {}",
                path.display()
            )
        })?;
        ensure!(
            metadata.is_file(),
            "lexical recovery archive is not a regular file"
        );
        #[cfg(unix)]
        let archive_identity = {
            use std::os::unix::fs::MetadataExt;
            let created = metadata
                .created()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_nanos().to_string());
            Some(serde_json::json!({
                "platform": "unix", "device": metadata.dev(), "inode": metadata.ino(),
                "created_ns": created,
            }))
        };
        #[cfg(windows)]
        let archive_identity = file_observation(&path).and_then(|observation| {
            let native = observation.get("windows")?;
            Some(serde_json::json!({
                "platform": "windows",
                "volume_serial": native.get("volume_serial")?.as_u64()?,
                "file_id": native.get("file_id")?.as_str()?,
                "creation_time_100ns": native.get("creation_time_100ns")?.as_str()?,
            }))
        });
        #[cfg(not(any(unix, windows)))]
        let archive_identity: Option<Value> = None;
        Ok(archive_identity.map(|archive_identity| Self {
            version: BINDING_VERSION,
            archive_identity,
            schema_hash: SCHEMA_HASH.to_string(),
            packet_version: super::LEXICAL_REBUILD_PACKET_VERSION,
            content_cap_bytes: crate::storage::sqlite::lexical_max_message_content_bytes()
                .min(i32::MAX as usize),
        }))
    }

    pub(super) fn matches(&self, db_path: &Path) -> bool {
        Self::capture(db_path).ok().flatten().as_ref() == Some(self)
    }
}

/// Missing evidence never authorizes a cursor. Explicit v3 staged candidates
/// from older versions may be adopted only by the same complete native audit.
/// Their prior file lineage is unknown; the audit proves equivalence to the
/// current archive before that identity is bound for subsequent retries.
pub(super) fn eligible(state: &LexicalRebuildState, db_path: &Path) -> bool {
    checked_eligible(state, db_path).unwrap_or(false)
}

/// Mutation-capable admission must distinguish incompatible evidence from an
/// unavailable source. The conservative boolean wrapper is only for preflight;
/// treating a temporary stat failure as incompatibility would quarantine a
/// perfectly reusable candidate before the archive could be restored.
pub(super) fn checked_eligible(state: &LexicalRebuildState, db_path: &Path) -> Result<bool> {
    if !state.is_incomplete()
        || state.version != super::LEXICAL_REBUILD_STATE_VERSION
        || state.schema_hash != SCHEMA_HASH
        || !super::lexical_rebuild_page_size_is_compatible(state.page_size)
        || !matches!(
            state.effective_execution_mode(),
            LexicalRebuildExecutionMode::StagedSingleIndex
                | LexicalRebuildExecutionMode::StagedVerifiedReplay
        )
        || (state.resume_binding.is_none()
            && state.execution_mode != Some(LexicalRebuildExecutionMode::StagedSingleIndex))
    {
        return Ok(false);
    }
    // Establish source availability before comparing normalized paths. A
    // missing or unreadable symlink target can otherwise appear to change
    // archive identity.
    let current_binding = Binding::capture(db_path)?
        .context("stable archive identity is unavailable; candidate retained")?;
    Ok(
        crate::stored_path_identity_matches(&state.db.db_path, db_path)
            && state
                .resume_binding
                .as_ref()
                .is_none_or(|saved| saved == &current_binding),
    )
}

/// Only this class permits the caller to quarantine and rebuild once. I/O
/// errors and a moving canonical epoch retain the candidate for another try.
#[derive(Debug, thiserror::Error)]
#[error("lexical recovery candidate is incompatible: {reason}")]
pub(super) struct Incompatible {
    reason: String,
}

fn incompatible(reason: impl Into<String>) -> anyhow::Error {
    Incompatible {
        reason: reason.into(),
    }
    .into()
}

fn candidate_failure(error: anyhow::Error, context: &str) -> anyhow::Error {
    // Missing/truncated/corrupt durable evidence cannot authorize reuse. A
    // permission, lock, device or allocation failure says nothing about that
    // evidence and must leave it available for another attempt.
    if super::error_is_out_of_memory(&error)
        || error.chain().any(|cause| {
            cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                !matches!(
                    io.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::InvalidData
                        | std::io::ErrorKind::UnexpectedEof
                )
            })
        })
    {
        return error.context(format!("{context}; candidate retained"));
    }
    incompatible(format!("{context}: {error:#}"))
}

#[derive(Debug, thiserror::Error)]
#[error("canonical archive changed during lexical recovery; durable candidate retained for retry")]
pub(super) struct SourceChanged;

/// Physical write evidence brackets the source snapshot and publication.
/// Exclude SHM: ordinary readers legitimately update that coordination file.
/// Completed checkpoints persist this evidence to refuse reuse after even a
/// same-count edit. It never authorizes skipping the next incomplete audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArchiveEpoch {
    database: Value,
    wal: Value,
}

impl ArchiveEpoch {
    pub(super) fn capture(db_path: &Path) -> Result<Self> {
        let db_path = crate::normalize_path_identity(db_path);
        let database = epoch_file_observation(&db_path)
            .context("cannot establish canonical archive write epoch; candidate retained")?;
        ensure!(
            database.get("absent").and_then(Value::as_bool) != Some(true),
            "canonical archive is missing; candidate retained"
        );
        let wal = epoch_file_observation(&crate::storage::sqlite::database_sidecar_path(
            &db_path, "-wal",
        ))
        .context("cannot establish canonical WAL write epoch; candidate retained")?;
        Ok(Self { database, wal })
    }

    pub(super) fn verify(&self, db_path: &Path) -> Result<()> {
        if *self != Self::capture(db_path)? {
            return Err(SourceChanged.into());
        }
        Ok(())
    }
}

fn epoch_file_observation(path: &Path) -> Option<Value> {
    if path.to_str().is_some() {
        return file_observation(path);
    }
    // The shared source-ledger helper needs a JSON path string and refuses
    // non-Unicode names. Epoch evidence is always checked against the same
    // caller's archive path, so its Unix fallback needs only native metadata.
    // Never collapse distinct names through a lossy string conversion.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match fs::metadata(path) {
            Ok(metadata) => Some(serde_json::json!({
                "size": metadata.len(),
                "mtime_ns": metadata.modified().ok()?.duration_since(UNIX_EPOCH)
                    .ok()?.as_nanos().to_string(),
                "device": metadata.dev(),
                "inode": metadata.ino(),
                "ctime": [metadata.ctime(), metadata.ctime_nsec()],
            })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Some(serde_json::json!({"absent": true}))
            }
            Err(_) => None,
        }
    }
    #[cfg(not(unix))]
    None
}

/// Deterministic in-process fault/visibility seams, also useful to tests that
/// mutate canonical storage without sharing the indexer's own write lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(super) enum Event {
    CanonicalMessage {
        conversation_id: i64,
        message_idx: i64,
    },
    CommittedBatch {
        indexed_docs: usize,
    },
    BeforePublication,
}

pub(super) fn run(
    db_path: &Path,
    data_dir: &Path,
    index_path: &Path,
    state: LexicalRebuildState,
    progress: Option<Arc<IndexingProgress>>,
    progress_bump: Option<Arc<AtomicI64>>,
) -> Result<LexicalRebuildOutcome> {
    run_inner(
        db_path,
        data_dir,
        index_path,
        state,
        progress,
        progress_bump,
        &mut |_| Ok(()),
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn run_with_observer(
    db_path: &Path,
    data_dir: &Path,
    index_path: &Path,
    state: LexicalRebuildState,
    progress: Option<Arc<IndexingProgress>>,
    progress_bump: Option<Arc<AtomicI64>>,
    mut observer: impl FnMut(Event) -> Result<()>,
) -> Result<LexicalRebuildOutcome> {
    run_inner(
        db_path,
        data_dir,
        index_path,
        state,
        progress,
        progress_bump,
        &mut observer,
    )
}

/// Reuse completed work only through unchanged source/native evidence or a
/// full read-only equivalence audit. Bookkeeping and WAL relocation may change
/// a physical epoch without changing indexed content: audit that content,
/// never renew an epoch across an unverified write interval.
pub(super) fn revalidate_completed(
    db_path: &Path,
    index_path: &Path,
    state: LexicalRebuildState,
    progress: Option<Arc<IndexingProgress>>,
    progress_bump: Option<Arc<AtomicI64>>,
) -> Result<Option<LexicalRebuildOutcome>> {
    completed_audit_result(revalidate_completed_inner(
        db_path,
        index_path,
        state,
        progress,
        progress_bump,
        &mut |_| Ok(()),
    ))
}

#[cfg(test)]
pub(super) fn revalidate_completed_with_observer(
    db_path: &Path,
    index_path: &Path,
    state: LexicalRebuildState,
    progress: Option<Arc<IndexingProgress>>,
    progress_bump: Option<Arc<AtomicI64>>,
    mut observer: impl FnMut(Event) -> Result<()>,
) -> Result<Option<LexicalRebuildOutcome>> {
    completed_audit_result(revalidate_completed_inner(
        db_path,
        index_path,
        state,
        progress,
        progress_bump,
        &mut observer,
    ))
}

fn completed_audit_result(
    result: Result<Option<LexicalRebuildOutcome>>,
) -> Result<Option<LexicalRebuildOutcome>> {
    match result {
        Err(error) if error.is::<Incompatible>() => {
            tracing::info!(reason = %format!("{error:#}"),
                "completed lexical generation differs from canonical content; fresh rebuild required");
            Ok(None)
        }
        other => other,
    }
}

/// These small sidecars must agree before a completed checkpoint can use the
/// unchanged-source shortcut. Missing, malformed or oversized sidecars ask for
/// a full audit, whose verified evidence repairs the pair before completion.
/// Do not let a corrupt artifact turn admission into an unbounded allocation
/// or a blocking special-file read.
fn read_completed_artifact<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspecting completed lexical artifact {}", path.display())
            });
        }
    };
    if !metadata.is_file() || metadata.len() > MAX_COMPLETED_ARTIFACT_BYTES {
        return Ok(None);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("opening completed lexical artifact {}", path.display()));
        }
    };
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(MAX_COMPLETED_ARTIFACT_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading completed lexical artifact {}", path.display()))?;
    if bytes.len() as u64 > MAX_COMPLETED_ARTIFACT_BYTES {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&bytes).ok())
}

fn completed_artifacts_match(index_path: &Path, state: &LexicalRebuildState) -> Result<bool> {
    use super::lexical_generation::{
        LEXICAL_GENERATION_MANIFEST_VERSION, LexicalGenerationBuildState,
        LexicalGenerationManifest, LexicalGenerationPublishState,
    };

    let Some(manifest): Option<LexicalGenerationManifest> =
        read_completed_artifact(&super::lexical_generation::manifest_path(index_path))?
    else {
        return Ok(false);
    };
    let Some(evidence): Option<super::LexicalRebuildEquivalenceEvidence> = read_completed_artifact(
        &super::lexical_rebuild_equivalence_evidence_path(index_path),
    )?
    else {
        return Ok(false);
    };
    Ok(
        manifest.manifest_version == LEXICAL_GENERATION_MANIFEST_VERSION
            && manifest.build_state == LexicalGenerationBuildState::Validated
            && manifest.publish_state == LexicalGenerationPublishState::Published
            && !manifest.generation_id.is_empty()
            && !manifest.attempt_id.is_empty()
            && manifest.source_db_fingerprint == state.db.storage_fingerprint
            && manifest.conversation_count == state.db.total_conversations as u64
            && manifest.message_count == state.db.total_messages as u64
            && manifest.indexed_doc_count == state.indexed_docs as u64
            && evidence.document_count == state.indexed_docs as u64
            && !evidence.manifest_fingerprint.is_empty()
            && manifest.equivalence_manifest_fingerprint.as_ref()
                == Some(&evidence.manifest_fingerprint),
    )
}

fn revalidate_completed_inner(
    db_path: &Path,
    index_path: &Path,
    state: LexicalRebuildState,
    progress: Option<Arc<IndexingProgress>>,
    progress_bump: Option<Arc<AtomicI64>>,
    observer: &mut dyn FnMut(Event) -> Result<()>,
) -> Result<Option<LexicalRebuildOutcome>> {
    if !super::lexical_state_can_certify_publication(&state)
        || !crate::stored_path_identity_matches(&state.db.db_path, db_path)
    {
        return Ok(None);
    }
    let epoch = ArchiveEpoch::capture(db_path)?;
    let Some(binding) = Binding::capture(db_path)? else {
        return Ok(None);
    };
    if state
        .resume_binding
        .as_ref()
        .is_some_and(|saved| saved != &binding)
    {
        return Ok(None);
    }
    crate::search::tantivy::validate_searchable_index_contract(index_path)
        .map_err(|error| candidate_failure(error, "completed lexical contract is unreadable"))?;
    if crate::search::tantivy::searchable_index_live_docs(index_path).is_some_and(|live| {
        live.manifest_source
            == crate::search::quill_bridge::QuillManifestSource::PreviousAfterCorruptCurrent
    }) {
        return Ok(None);
    }
    let native_fingerprint = super::index_meta_fingerprint(index_path)?;
    if native_fingerprint.is_none() {
        return Ok(None);
    }
    let published = PublishedSnapshot::open(index_path)
        .map_err(|error| candidate_failure(error, "completed lexical generation is unreadable"))?;
    let original_docs = usize::try_from(published.doc_count())
        .context("completed lexical document count exceeds usize")?;
    epoch.verify(db_path)?;
    ensure!(
        super::index_meta_fingerprint(index_path)? == native_fingerprint,
        "native lexical generation changed during completed admission; existing evidence retained"
    );
    let checkpoint_before = state.clone();
    if state.resume_binding.as_ref() == Some(&binding)
        && state.completed_source_epoch.as_ref() == Some(&epoch)
        && state.committed_meta_fingerprint == native_fingerprint
        && state.indexed_docs == original_docs
        && completed_artifacts_match(index_path, &state)?
    {
        ensure!(
            super::load_lexical_rebuild_state(index_path)?.as_ref() == Some(&checkpoint_before),
            "lexical checkpoint changed during completed admission; newer evidence retained"
        );
        (observer)(Event::BeforePublication)?;
        ensure!(
            super::index_meta_fingerprint(index_path)? == native_fingerprint,
            "native lexical generation changed during completed admission; existing evidence retained"
        );
        if !binding.matches(db_path) {
            return Err(SourceChanged.into());
        }
        // Sidecar parsing and checkpoint admission happen after the initial
        // epoch check. A raw writer during those reads must invalidate even
        // this unchanged-generation shortcut; never capture a newer epoch.
        epoch.verify(db_path)?;
        finish_audit_progress(progress.as_ref(), state.processed_conversations);
        return Ok(Some(LexicalRebuildOutcome {
            observed_conversations: state.processed_conversations,
            indexed_docs: original_docs,
            observed_messages: Some(state.db.total_messages),
            exact_checkpoint_persisted: false,
            equivalence: None,
        }));
    }

    let storage = FrankenStorage::open_strict_readonly(db_path)
        .context("opening canonical archive for completed lexical verification")?;
    let snapshot = CanonicalSnapshot::begin(storage.raw())?;
    let total_conversations = super::count_total_conversations_exact(&storage)?;
    let total_messages = super::count_total_messages_exact(&storage)?;
    let db_state = super::lexical_rebuild_db_state_with_exact_totals(
        &storage,
        db_path,
        total_conversations,
        total_messages,
    )?;
    let descriptor_identity = storage.raw().file_identity()?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&File::open(db_path)?)?;
    ensure!(
        descriptor_identity.is_some() && descriptor_identity == path_identity,
        "canonical archive pathname changed while admitting completed verification"
    );
    epoch.verify(db_path)?;
    if !binding.matches(db_path) {
        return Err(SourceChanged.into());
    }
    let limits = ReplayLimits::capture();
    if let Some(progress) = &progress {
        progress.phase.store(2, Ordering::Relaxed);
        progress.is_rebuilding.store(true, Ordering::Relaxed);
        progress.current.store(0, Ordering::Relaxed);
        progress.total.store(total_conversations, Ordering::Relaxed);
        progress.total_is_final.store(true, Ordering::Relaxed);
    }
    tracing::info!(
        original_docs,
        total_conversations,
        total_messages,
        "verifying completed lexical content after source epoch or publication evidence changed"
    );
    let mut replay = Replay {
        live: index_path,
        candidate: index_path,
        state,
        index: None,
        published,
        original_docs,
        retained_docs: 0,
        inserted_docs: 0,
        pending_docs: 0,
        pending_bytes: 0,
        pending_conversations: 0,
        pending_last_conversation_id: None,
        commit_max_messages: limits.messages,
        commit_max_bytes: limits.bytes,
        commit_max_conversations: limits.conversations,
        expected_docs: 0,
        observed_messages: 0,
        processed_conversations: 0,
        last_conversation_id: None,
        equivalence: LexicalRebuildEquivalenceAccumulator::new(),
        progress,
        progress_bump,
        observer,
        runtime: LexicalRebuildPipelineRuntimeSnapshot::default(),
    };
    replay.scan(&storage, limits.page_size)?;
    let indexed_docs = replay.verify_coverage(total_conversations, total_messages)?;
    snapshot.release()?;
    storage.close_without_checkpoint()?;
    (replay.observer)(Event::BeforePublication)?;
    epoch.verify(db_path)?;
    if !binding.matches(db_path) {
        return Err(SourceChanged.into());
    }
    ensure!(
        super::index_meta_fingerprint(index_path)? == native_fingerprint,
        "native lexical generation changed during completed verification; existing evidence retained"
    );
    ensure!(
        super::load_lexical_rebuild_state(index_path)?.as_ref() == Some(&checkpoint_before),
        "lexical checkpoint changed during completed verification; newer evidence retained"
    );
    let evidence = replay.equivalence.finalize();
    ensure!(
        evidence.document_count == indexed_docs as u64,
        "completed lexical verification evidence omits canonical documents"
    );
    // GH457: generation metadata and its paired evidence must be durable
    // before a completed checkpoint certifies them. This also repairs legacy
    // missing sidecars and refreshes counts after verified inline growth,
    // without rewriting any native index artifact or inventing build timings.
    let manifest = super::persist_lexical_rebuild_generation_artifacts(
        index_path,
        &db_state.storage_fingerprint,
        total_conversations,
        total_conversations,
        total_messages,
        indexed_docs,
        &evidence,
    )?;
    epoch.verify(db_path)?;
    ensure!(
        super::index_meta_fingerprint(index_path)? == native_fingerprint,
        "native lexical generation changed while persisting completed verification artifacts"
    );
    ensure!(
        super::load_lexical_rebuild_state(index_path)?.as_ref() == Some(&checkpoint_before),
        "lexical checkpoint changed while persisting completed verification artifacts"
    );
    let state = &mut replay.state;
    state.db = db_state;
    state.resume_binding = Some(binding);
    state.completed_source_epoch = Some(epoch.clone());
    state.processed_conversations = total_conversations;
    state.committed_offset = i64::try_from(total_conversations)
        .context("completed lexical verification cursor overflow")?;
    state.committed_conversation_id = replay.last_conversation_id;
    state.indexed_docs = indexed_docs;
    state.mark_completed(native_fingerprint.clone());
    // Keep the epoch captured BEFORE the audit. A writer arriving after the
    // final check invalidates this old epoch; it cannot be silently absorbed
    // by capturing a newer epoch after an unverified interval.
    super::persist_lexical_rebuild_state(index_path, state)?;
    super::persist_lexical_publication_receipt(index_path, state)?;
    epoch.verify(db_path)?;
    ensure!(
        super::index_meta_fingerprint(index_path)? == native_fingerprint,
        "native lexical generation changed while persisting completed verification"
    );
    super::log_lexical_generation_manifest_published(&manifest, &evidence);
    finish_audit_progress(replay.progress.as_ref(), total_conversations);
    Ok(Some(LexicalRebuildOutcome {
        observed_conversations: total_conversations,
        indexed_docs,
        observed_messages: Some(total_messages),
        exact_checkpoint_persisted: true,
        equivalence: Some(evidence),
    }))
}

fn finish_audit_progress(progress: Option<&Arc<IndexingProgress>>, total_conversations: usize) {
    if let Some(progress) = progress {
        progress
            .current
            .store(total_conversations, Ordering::Relaxed);
        progress.total.store(total_conversations, Ordering::Relaxed);
        progress.total_is_final.store(true, Ordering::Relaxed);
        progress.phase.store(0, Ordering::Relaxed);
        progress.is_rebuilding.store(false, Ordering::Relaxed);
    }
}

struct Replay<'a> {
    live: &'a Path,
    candidate: &'a Path,
    state: LexicalRebuildState,
    /// None is a read-only equivalence audit of the already published index.
    index: Option<TantivyIndex>,
    published: PublishedSnapshot,
    original_docs: usize,
    retained_docs: usize,
    inserted_docs: usize,
    pending_docs: usize,
    pending_bytes: usize,
    pending_conversations: usize,
    pending_last_conversation_id: Option<i64>,
    commit_max_messages: usize,
    commit_max_bytes: usize,
    commit_max_conversations: usize,
    expected_docs: usize,
    observed_messages: usize,
    processed_conversations: usize,
    last_conversation_id: Option<i64>,
    equivalence: LexicalRebuildEquivalenceAccumulator,
    progress: Option<Arc<IndexingProgress>>,
    progress_bump: Option<Arc<AtomicI64>>,
    observer: &'a mut dyn FnMut(Event) -> Result<()>,
    runtime: LexicalRebuildPipelineRuntimeSnapshot,
}

struct ReplayLimits {
    bytes: usize,
    messages: usize,
    conversations: usize,
    page_size: i64,
}

impl ReplayLimits {
    fn capture() -> Self {
        let settings = super::lexical_rebuild_pipeline_settings_snapshot();
        let (startup_bytes, steady_bytes) =
            super::lexical_rebuild_page_fetch_message_bytes(&settings);
        Self {
            bytes: startup_bytes
                .min(steady_bytes)
                .min(settings.pipeline_max_message_bytes_in_flight / 2)
                .min(settings.startup_commit_every_message_bytes)
                .min(settings.steady_commit_every_message_bytes)
                .min(MAX_BATCH_BYTES)
                .max(1),
            messages: settings
                .startup_commit_every_messages
                .min(settings.steady_commit_every_messages)
                .min(MAX_BATCH_MESSAGES)
                .max(1),
            conversations: settings
                .startup_commit_every_conversations
                .min(settings.steady_commit_every_conversations)
                .max(1),
            page_size: settings
                .startup_batch_fetch_conversations
                .min(settings.steady_batch_fetch_conversations)
                .max(1)
                .min(super::LEXICAL_REBUILD_PAGE_SIZE as usize) as i64,
        }
    }
}

impl Replay<'_> {
    fn tick(&self) -> Result<()> {
        if let Some(progress) = &self.progress {
            progress.check_stop()?;
            progress.tick_activity();
        }
        super::bump_index_run_lock_progress_if_present(self.progress_bump.as_ref());
        Ok(())
    }

    fn scan(&mut self, storage: &FrankenStorage, page_size: i64) -> Result<()> {
        let (agents, workspaces) = storage.build_lexical_rebuild_lookups()?;
        let sources: HashMap<_, _> = storage
            .list_sources()?
            .into_iter()
            .map(|source| (source.id, (source.kind, source.host_label)))
            .collect();
        loop {
            self.tick()?;
            let rows = storage.list_conversations_for_lexical_rebuild_after_id(
                page_size,
                self.last_conversation_id.unwrap_or(0),
                &agents,
                &workspaces,
            )?;
            if rows.is_empty() {
                return Ok(());
            }
            for row in rows {
                let conversation_id = row
                    .id
                    .context("lexical recovery conversation has no identity")?;
                ensure!(
                    conversation_id > self.last_conversation_id.unwrap_or(0),
                    "canonical lexical recovery conversation order is not strictly increasing"
                );
                let mut messages = Vec::new();
                let mut bytes = 0usize;
                let mut previous_idx = None;
                let mut had_messages = false;
                let complete =
                    storage.for_each_lexical_rebuild_message(conversation_id, None, |message| {
                        self.tick()?;
                        ensure!(
                            message.idx >= 0 && previous_idx.is_none_or(|idx| message.idx > idx),
                            "canonical lexical recovery message indices are negative or repeated"
                        );
                        previous_idx = Some(message.idx);
                        had_messages = true;
                        (self.observer)(Event::CanonicalMessage {
                            conversation_id,
                            message_idx: message.idx,
                        })?;
                        let effective_bytes = super::responsiveness::effective_inflight_byte_limit(
                            self.commit_max_bytes,
                        )
                        .min(self.commit_max_bytes)
                        .max(1);
                        if !messages.is_empty()
                            && (messages.len() >= self.commit_max_messages
                                || message.content.len() > effective_bytes.saturating_sub(bytes))
                        {
                            self.consume(&row, &mut messages, &sources)?;
                            bytes = 0;
                        }
                        bytes = bytes
                            .checked_add(message.content.len())
                            .context("lexical recovery batch byte count overflow")?;
                        messages.push(message);
                        // A single oversized message remains bounded by the
                        // canonical per-message cap, like the chunked producer.
                        if bytes >= effective_bytes || messages.len() >= self.commit_max_messages {
                            self.consume(&row, &mut messages, &sources)?;
                            bytes = 0;
                        }
                        Ok(true)
                    })?;
                ensure!(complete, "canonical lexical recovery stream ended early");
                if !messages.is_empty() || !had_messages {
                    self.consume(&row, &mut messages, &sources)?;
                }
                self.last_conversation_id = Some(conversation_id);
                self.processed_conversations = self
                    .processed_conversations
                    .checked_add(1)
                    .context("lexical recovery conversation count overflow")?;
                if let Some(progress) = &self.progress {
                    progress
                        .current
                        .store(self.processed_conversations, Ordering::Relaxed);
                }
            }
        }
    }

    fn verify_coverage(&self, total_conversations: usize, total_messages: usize) -> Result<usize> {
        if self.retained_docs != self.original_docs {
            return Err(incompatible(format!(
                "{} of {} retained documents still exist in the canonical projection; a message, conversation or identity was removed",
                self.retained_docs, self.original_docs,
            )));
        }
        ensure!(
            self.processed_conversations == total_conversations
                && self.observed_messages == total_messages,
            "canonical lexical recovery coverage disagrees with the pinned source totals"
        );
        let indexed_docs = self
            .original_docs
            .checked_add(self.inserted_docs)
            .context("lexical recovery final document count overflow")?;
        ensure!(
            self.expected_docs == indexed_docs,
            "canonical lexical recovery document set is incomplete"
        );
        Ok(indexed_docs)
    }

    fn commit_pending(&mut self) -> Result<()> {
        if self.pending_docs == 0 {
            return Ok(());
        }
        self.tick()?;
        let indexed_docs = self
            .original_docs
            .checked_add(self.inserted_docs)
            .context("lexical recovery indexed document count overflow")?;
        // A commit may end inside a giant conversation. Audit mode never
        // uses this cursor to skip source rows, so every bounded batch can
        // become durable without waiting for the whole conversation.
        super::commit_lexical_rebuild_progress(
            self.live,
            self.candidate,
            &mut self.state,
            self.last_conversation_id,
            self.processed_conversations,
            indexed_docs,
            &self.runtime,
            self.index
                .as_mut()
                .context("read-only lexical audit cannot commit missing documents")?,
            true,
            None,
        )?;
        self.pending_docs = 0;
        self.pending_bytes = 0;
        self.pending_conversations = 0;
        self.pending_last_conversation_id = None;
        (self.observer)(Event::CommittedBatch { indexed_docs })
    }

    fn consume(
        &mut self,
        row: &LexicalRebuildConversationRow,
        messages: &mut Vec<Message>,
        sources: &HashMap<String, (SourceKind, Option<String>)>,
    ) -> Result<()> {
        self.tick()?;
        let packet = LexicalRebuildConversationPacket::from_canonical_replay_messages(
            row.clone(),
            std::mem::take(messages),
            sources,
        )?;
        self.observed_messages = self
            .observed_messages
            .checked_add(packet.message_count)
            .context("lexical recovery canonical message count overflow")?;
        self.equivalence.absorb_packet(&packet);
        let documents = packet.prebuilt_docs();
        self.expected_docs = self
            .expected_docs
            .checked_add(documents.len())
            .context("lexical recovery expected document count overflow")?;
        let mut missing = Vec::new();
        for document in documents {
            match self
                .published
                .classify_content_ref(document)
                .map_err(|error| candidate_failure(error, "reading candidate content witness"))?
            {
                ContentMatch::Matching => {
                    self.retained_docs = self
                        .retained_docs
                        .checked_add(1)
                        .context("lexical recovery retained document count overflow")?;
                }
                ContentMatch::Missing => missing.push(document),
                ContentMatch::Different => {
                    return Err(incompatible(format!(
                        "canonical projection changed for conversation {} message {}",
                        document.conversation_id.unwrap_or_default(),
                        document.msg_idx,
                    )));
                }
            }
        }
        if !missing.is_empty() {
            if self.index.is_none() {
                return Err(incompatible(
                    "published lexical generation is missing canonical documents",
                ));
            }
            let missing_bytes = missing.iter().try_fold(0usize, |bytes, document| {
                bytes
                    .checked_add(document.content.len())
                    .context("lexical recovery pending byte count overflow")
            })?;
            let effective_bytes =
                super::responsiveness::effective_inflight_byte_limit(self.commit_max_bytes)
                    .min(self.commit_max_bytes)
                    .max(1);
            // Aggregate small conversations without retaining their source
            // buffers. Engine writes remain within the existing commit caps;
            // only one canonical-capped oversized message may exceed bytes.
            if self.pending_docs > 0
                && (missing.len() > self.commit_max_messages.saturating_sub(self.pending_docs)
                    || missing_bytes > effective_bytes.saturating_sub(self.pending_bytes)
                    || (self.pending_last_conversation_id != row.id
                        && self.pending_conversations >= self.commit_max_conversations))
            {
                self.commit_pending()?;
            }
            super::ensure_lexical_rebuild_free_space(self.candidate)?;
            let inserted = self
                .index
                .as_mut()
                .context("lexical recovery writer disappeared")?
                .add_prebuilt_document_refs_slice(&missing)?;
            ensure!(
                inserted == missing.len(),
                "lexical recovery sink did not accept every missing identity"
            );
            self.inserted_docs = self
                .inserted_docs
                .checked_add(inserted)
                .context("lexical recovery inserted document count overflow")?;
            self.pending_docs = self
                .pending_docs
                .checked_add(inserted)
                .context("lexical recovery pending document count overflow")?;
            self.pending_bytes = self
                .pending_bytes
                .checked_add(missing_bytes)
                .context("lexical recovery pending byte count overflow")?;
            if self.pending_last_conversation_id != row.id {
                self.pending_conversations = self
                    .pending_conversations
                    .checked_add(1)
                    .context("lexical recovery pending conversation count overflow")?;
                self.pending_last_conversation_id = row.id;
            }
            if self.pending_docs >= self.commit_max_messages
                || self.pending_bytes >= effective_bytes
                || self.pending_conversations >= self.commit_max_conversations
            {
                self.commit_pending()?;
            }
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    db_path: &Path,
    _data_dir: &Path,
    index_path: &Path,
    state: LexicalRebuildState,
    progress: Option<Arc<IndexingProgress>>,
    progress_bump: Option<Arc<AtomicI64>>,
    observer: &mut dyn FnMut(Event) -> Result<()>,
) -> Result<LexicalRebuildOutcome> {
    let started = Instant::now();
    if !checked_eligible(&state, db_path)? {
        return Err(incompatible(
            "checkpoint archive identity or lexical contract changed",
        ));
    }
    let candidate_path = super::lexical_rebuild_resume_content_path(index_path, &state)
        .map_err(|error| candidate_failure(error, "staged generation unavailable"))?;
    if candidate_path == index_path {
        return Err(incompatible(
            "checkpoint does not name an off-live candidate",
        ));
    }
    crate::search::tantivy::validate_searchable_index_contract(&candidate_path)
        .map_err(|error| candidate_failure(error, "staged lexical contract is unreadable"))?;
    let mut state = super::reconcile_pending_lexical_commit(index_path, state)?;
    let published = PublishedSnapshot::open(&candidate_path)
        .map_err(|error| candidate_failure(error, "staged lexical generation is unreadable"))?;
    let original_docs = usize::try_from(published.doc_count())
        .context("lexical recovery candidate document count exceeds usize")?;
    if original_docs != state.indexed_docs {
        return Err(incompatible(format!(
            "candidate has {original_docs} documents, reconciled checkpoint requires {}",
            state.indexed_docs,
        )));
    }

    let epoch = ArchiveEpoch::capture(db_path)?;
    let current_binding = Binding::capture(db_path)?
        .context("stable archive identity is unavailable; candidate retained")?;
    let storage = FrankenStorage::open_strict_readonly(db_path)
        .context("opening canonical archive for verified lexical recovery")?;
    let snapshot = CanonicalSnapshot::begin(storage.raw())?;
    let total_conversations = super::count_total_conversations_exact(&storage)?;
    let total_messages = super::count_total_messages_exact(&storage)?;
    let db_state = super::lexical_rebuild_db_state_with_exact_totals(
        &storage,
        db_path,
        total_conversations,
        total_messages,
    )?;
    let descriptor_identity = storage.raw().file_identity()?;
    let path_identity = crate::franken_sync::FileIdentity::from_file(&File::open(db_path)?)?;
    ensure!(
        descriptor_identity.is_some() && descriptor_identity == path_identity,
        "canonical archive pathname no longer identifies the pinned reader; candidate retained"
    );
    epoch.verify(db_path)?;
    if !current_binding.matches(db_path)
        || state
            .resume_binding
            .as_ref()
            .is_some_and(|binding| binding != &current_binding)
    {
        return Err(SourceChanged.into());
    }
    let ReplayLimits {
        bytes: max_bytes,
        messages: max_messages,
        conversations: max_conversations,
        page_size,
    } = ReplayLimits::capture();
    let mut index = super::open_lexical_index_with_heartbeat(&candidate_path, progress.as_ref())?;
    index.configure_bulk_load_merge_policy();
    state.db = db_state;
    state.committed_offset = 0;
    state.committed_conversation_id = None;
    state.processed_conversations = 0;
    state.pending = None;
    state.completed = false;
    state.completed_source_epoch = None;
    state.clear_runtime();
    state.resume_binding = Some(current_binding);
    state.set_execution_mode(LexicalRebuildExecutionMode::StagedVerifiedReplay);
    super::persist_lexical_rebuild_state_for_active_run_start(index_path, &state)?;
    if let Some(progress) = &progress {
        progress.phase.store(2, Ordering::Relaxed);
        progress.is_rebuilding.store(true, Ordering::Relaxed);
        progress.current.store(0, Ordering::Relaxed);
        progress.total.store(total_conversations, Ordering::Relaxed);
        progress.total_is_final.store(true, Ordering::Relaxed);
    }
    tracing::info!(
        retained_candidate_docs = original_docs,
        total_conversations,
        total_messages,
        batch_messages = max_messages,
        batch_bytes = max_bytes,
        "auditing durable lexical candidate against canonical content before incremental reuse"
    );
    let mut replay = Replay {
        live: index_path,
        candidate: &candidate_path,
        state,
        index: Some(index),
        published,
        original_docs,
        retained_docs: 0,
        inserted_docs: 0,
        pending_docs: 0,
        pending_bytes: 0,
        pending_conversations: 0,
        pending_last_conversation_id: None,
        commit_max_messages: max_messages,
        commit_max_bytes: max_bytes,
        commit_max_conversations: max_conversations,
        expected_docs: 0,
        observed_messages: 0,
        processed_conversations: 0,
        last_conversation_id: None,
        equivalence: LexicalRebuildEquivalenceAccumulator::new(),
        progress,
        progress_bump,
        observer,
        runtime: LexicalRebuildPipelineRuntimeSnapshot {
            page_prep_workers: 1,
            max_message_bytes_in_flight: max_bytes,
            controller_mode: "verified_replay".to_string(),
            controller_reason: "bounded_single_reader_recovery".to_string(),
            ..LexicalRebuildPipelineRuntimeSnapshot::default()
        },
    };
    replay.scan(&storage, page_size)?;
    let indexed_docs = replay.verify_coverage(total_conversations, total_messages)?;
    epoch.verify(db_path)?;
    // Complete the same pending -> engine -> checkpoint protocol for the
    // aggregate tail before a merge can rewrite the candidate manifest.
    replay.commit_pending()?;
    let Replay {
        mut state,
        index,
        published,
        retained_docs,
        inserted_docs,
        observed_messages,
        processed_conversations,
        last_conversation_id,
        equivalence,
        progress,
        observer,
        ..
    } = replay;
    let mut index = index.context("incremental lexical recovery requires its staged writer")?;
    // Do not retain the old native reader while the bounded merge and final
    // validation reader run. Release the canonical transaction before closing
    // its handle; the physical epoch is checked again at publication.
    drop(published);
    snapshot.release()?;
    storage.close_without_checkpoint()?;
    let publication = super::lexical_publish::Finalization::new(
        index_path,
        &candidate_path,
        indexed_docs,
        processed_conversations,
    );
    if !super::lexical_post_run_maintenance_skipped_for_test()
        && super::lexical_rebuild_fold_shortfall(&candidate_path).is_none()
        && let Err(error) = index.force_merge()
    {
        tracing::warn!(error = %format!("{error:#}"),
            "bounded lexical recovery fold failed; preserving the unmerged candidate");
    }
    publication.run("checkpoint_recovered_candidate", || {
        super::commit_lexical_rebuild_progress(
            index_path,
            &candidate_path,
            &mut state,
            last_conversation_id,
            processed_conversations,
            indexed_docs,
            &LexicalRebuildPipelineRuntimeSnapshot::default(),
            &mut index,
            true,
            None,
        )
    })?;
    drop(index);
    publication.run("validate_recovered_candidate", || {
        crate::search::tantivy::validate_searchable_index_contract(&candidate_path)?;
        super::verify_published_lexical_doc_count(&candidate_path, indexed_docs, "verified_replay")
    })?;
    epoch.verify(db_path)?;
    let lexical_duration = started.elapsed();
    let publish_started = Instant::now();
    let evidence = equivalence.finalize();
    ensure!(
        evidence.document_count == indexed_docs as u64,
        "lexical recovery evidence omits retained documents"
    );
    let manifest = publication.run("persist_recovered_generation", || {
        super::persist_lexical_rebuild_generation_artifacts(
            &candidate_path,
            &state.db.storage_fingerprint,
            processed_conversations,
            total_conversations,
            observed_messages,
            indexed_docs,
            &evidence,
        )
    })?;
    state.db.total_conversations = total_conversations;
    state.db.total_messages = observed_messages;
    state.processed_conversations = processed_conversations;
    state.committed_offset = i64::try_from(processed_conversations)
        .context("lexical recovery completed cursor overflow")?;
    state.committed_conversation_id = last_conversation_id;
    state.indexed_docs = indexed_docs;
    state.completed_source_epoch = Some(epoch.clone());
    state.mark_completed(super::index_meta_fingerprint(&candidate_path)?);
    publication.run("persist_recovered_receipt", || {
        super::persist_lexical_rebuild_state(&candidate_path, &state)?;
        super::persist_lexical_publication_receipt(&candidate_path, &state)?;
        super::sync_parent_directory(&super::lexical_rebuild_state_path(&candidate_path))
    })?;
    observer(Event::BeforePublication)?;
    epoch.verify(db_path)?;
    if !state
        .resume_binding
        .as_ref()
        .is_some_and(|binding| binding.matches(db_path))
    {
        return Err(SourceChanged.into());
    }
    publication.run("publish_recovered_generation", || {
        super::publish_staged_lexical_index(&candidate_path, index_path)
    })?;
    publication.run("validate_recovered_publication", || {
        crate::search::tantivy::validate_searchable_index_contract(index_path)?;
        super::verify_published_lexical_doc_count(index_path, indexed_docs, "verified_replay")
    })?;
    super::log_lexical_generation_manifest_published(&manifest, &evidence);
    let ledger = super::build_authoritative_lexical_refresh_ledger(
        super::AuthoritativeLexicalRefreshLedgerInput {
            publish_mode: "atomic_verified_replay",
            lexical_duration,
            publish_duration: publish_started.elapsed(),
            processed_conversations,
            total_conversations,
            final_observed_messages: observed_messages,
            indexed_docs,
            equivalence_evidence: &evidence,
        },
    );
    publication.run("persist_recovered_ledger", || {
        super::persist_lexical_refresh_ledger(index_path, &ledger)
    })?;
    super::log_lexical_refresh_ledger_published(&ledger);
    tracing::info!(
        retained_docs,
        inserted_docs,
        indexed_docs,
        processed_conversations,
        "verified incremental lexical recovery published atomically"
    );
    if let Some(progress) = &progress {
        progress
            .current
            .store(total_conversations, Ordering::Relaxed);
        progress.total.store(total_conversations, Ordering::Relaxed);
        progress.total_is_final.store(true, Ordering::Relaxed);
        progress.phase.store(0, Ordering::Relaxed);
        progress.is_rebuilding.store(false, Ordering::Relaxed);
    }
    Ok(LexicalRebuildOutcome {
        observed_conversations: total_conversations,
        indexed_docs,
        observed_messages: Some(observed_messages),
        exact_checkpoint_persisted: true,
        equivalence: Some(evidence),
    })
}
