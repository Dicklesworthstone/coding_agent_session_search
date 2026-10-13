//! Source-bound hydration of retained passage results in one archive snapshot.
//!
//! A retained vector owner authenticates a scored passage, not whichever text
//! currently occupies its message primary key. This adapter checks the encoded
//! metadata and SHA-256 of the selected canonical passage before returning any
//! text. It never opens a database, installs a model, repairs an index, or writes
//! archive rows. The caller supplies its existing FrankenSQLite connection.
//!
//! This is deliberately separate from ordinary SearchClient hydration. It is
//! callable on both retained and publication-selected batches, including each
//! progressive phase. It is not a whole-archive generation certificate.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::franken_sync::Connection;
use crate::franken_sync::compat::{RowExt, Transaction, TransactionExt};
use crate::search::canonicalize::{canonicalize_for_embedding, content_hash, embedding_passages};
use crate::search::semantic_manifest::TierKind;
use crate::search::semantic_reader::publication::SelectedSemanticBatch;
use crate::search::semantic_reader::{SemanticPassageHit, SemanticSearchBatch};
use crate::search::vector_index::{ROLE_USER, SemanticDocId, role_code_from_str};
use crate::sources::provenance::LOCAL_SOURCE_ID;

pub mod query;

#[cfg(test)]
mod tests;

const MAX_PASSAGES: usize = 1_024;
const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 16 * 1024;

/// The caller must explicitly name the projection used to build the artifact.
/// There is no default or automatic reinterpretation of an old input contract.
/// In either case a result is accepted only if its exact passage digest matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassageProjection {
    /// Historical whole-message canonicalization, capped by MAX_EMBED_CHARS.
    /// Only chunk zero exists under this contract.
    CanonicalPrefixV1,
    /// Current raw-first bounded passages, then per-passage canonicalization.
    RawPassagesV2,
}

/// Bounds returned candidates and source/output text retained by this adapter.
/// SQL execution and canonicalization are cooperative boundaries: neither the
/// timeout nor the byte probe is a hard engine-memory or native-call time bound.
#[derive(Debug, Clone, Copy)]
pub struct HydrationBudget {
    pub max_passages: usize,
    pub max_message_bytes: usize,
    pub max_total_source_bytes: usize,
    pub max_total_output_bytes: usize,
    pub timeout: Duration,
}

impl Default for HydrationBudget {
    fn default() -> Self {
        Self {
            max_passages: 256,
            max_message_bytes: 4 * 1024 * 1024,
            max_total_source_bytes: 16 * 1024 * 1024,
            max_total_output_bytes: 2 * 1024 * 1024,
            timeout: Duration::from_secs(5),
        }
    }
}

impl HydrationBudget {
    fn validate(self, passages: usize) -> Result<(), HydrationError> {
        if self.max_passages > MAX_PASSAGES
            || self.max_message_bytes > MAX_MESSAGE_BYTES
            || self.max_total_source_bytes > MAX_TOTAL_BYTES
            || self.max_total_output_bytes > MAX_TOTAL_BYTES
            || self.timeout > Duration::from_secs(60)
        {
            return Err(HydrationError::InvalidBudget);
        }
        if passages > self.max_passages {
            return Err(HydrationError::BudgetExceeded);
        }
        Ok(())
    }
}

/// Diagnostics are intentionally fixed and contain no paths, bodies or SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HydrationError {
    #[error("semantic hydration budget exceeds the supported limits")]
    InvalidBudget,
    #[error("semantic hydration exceeded its source, output or candidate budget")]
    BudgetExceeded,
    #[error("semantic hydration request deadline expired")]
    DeadlineExceeded,
    #[error("semantic result does not match its retained passage owner")]
    InvalidCandidate,
    #[error("semantic passage lacks a verifiable content fingerprint")]
    MissingFingerprint,
    #[error("semantic passage references a missing canonical message or conversation")]
    MissingSource,
    #[error("semantic passage no longer matches canonical source text or metadata; rebuild semantic assets before retrying")]
    StaleSource,
    #[error("canonical source row has an invalid identity, type or size")]
    InvalidSource,
    #[error("canonical source read failed during semantic hydration")]
    Database,
    #[error("semantic hydration could not roll back its read transaction")]
    Rollback,
}

/// A current snapshot locator, not a claim that the vector authenticated its
/// pathname. The vector binds the encoded source ID, not the physical path.
#[derive(Clone)]
pub struct HydratedPassageLocation {
    pub message_id: u64,
    pub conversation_id: i64,
    pub source_id: String,
    pub source_path: String,
    pub line_number: Option<usize>,
}

impl std::fmt::Debug for HydratedPassageLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HydratedPassageLocation")
            .field("message_id", &self.message_id)
            .field("conversation_id", &self.conversation_id)
            .field("line_number", &self.line_number)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct HydratedPassage {
    hit: SemanticPassageHit,
    canonical_text: String,
    location: Arc<HydratedPassageLocation>,
}

impl HydratedPassage {
    pub fn hit(&self) -> &SemanticPassageHit { &self.hit }
    pub fn canonical_text(&self) -> &str { &self.canonical_text }
    pub fn location(&self) -> &HydratedPassageLocation { &self.location }
}

impl std::fmt::Debug for HydratedPassage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HydratedPassage")
            .field("message_id", &self.hit.document.message_id)
            .field("chunk_idx", &self.hit.document.chunk_idx)
            .field("canonical_bytes", &self.canonical_text.len())
            .finish_non_exhaustive()
    }
}

/// Keeps the original result batch, its scores and all vector/graph owners.
/// For a SelectedSemanticBatch the exact publication identity is retained too.
/// No successfully returned batch contains an unverified or missing passage.
#[derive(Clone)]
pub struct HydratedSemanticBatch<B> {
    source: B,
    passages: Vec<HydratedPassage>,
    projection: PassageProjection,
}

impl<B> HydratedSemanticBatch<B> {
    pub fn source(&self) -> &B { &self.source }
    pub fn passages(&self) -> &[HydratedPassage] { &self.passages }
    pub fn projection(&self) -> PassageProjection { self.projection }
}

impl<B> std::fmt::Debug for HydratedSemanticBatch<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HydratedSemanticBatch")
            .field("passages", &self.passages.len())
            .field("projection", &self.projection)
            .finish_non_exhaustive()
    }
}

impl SemanticSearchBatch {
    /// Read and verify the entire batch using the caller's already-pinned
    /// transaction. This method does not commit, roll back or replace it.
    /// Projection must match the selected input contract, never an assumed
    /// newest writer policy. Only the selected canonical passage is verified;
    /// edits outside it can legitimately leave its digest unchanged.
    pub fn hydrate_in_transaction(
        &self,
        transaction: &Transaction<'_>,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> Result<HydratedSemanticBatch<Self>, HydrationError> {
        let passages = hydrate(self, transaction, projection, budget, Instant::now())?;
        Ok(HydratedSemanticBatch { source: self.clone(), passages, projection })
    }

    /// Own a read transaction on an existing connection and roll it back before
    /// returning either results or a failure. An existing caller transaction is
    /// not adopted. No new database is opened or migrated.
    pub fn hydrate_from_connection(
        &self,
        connection: &mut Connection,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> Result<HydratedSemanticBatch<Self>, HydrationError> {
        let started = Instant::now();
        budget.validate(self.hits().len())?;
        if budget.timeout.is_zero() { return Err(HydrationError::DeadlineExceeded); }
        let mut transaction = connection.transaction().map_err(|_| HydrationError::Database)?;
        let result = hydrate(self, &transaction, projection, budget, started);
        transaction.rollback().map_err(|_| HydrationError::Rollback)?;
        let passages = result?;
        if started.elapsed() >= budget.timeout { return Err(HydrationError::DeadlineExceeded); }
        Ok(HydratedSemanticBatch { source: self.clone(), passages, projection })
    }
}

impl SelectedSemanticBatch {
    pub fn hydrate_in_transaction(
        &self,
        transaction: &Transaction<'_>,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> Result<HydratedSemanticBatch<Self>, HydrationError> {
        let passages = hydrate(&self.batch, transaction, projection, budget, Instant::now())?;
        Ok(HydratedSemanticBatch {
            source: SelectedSemanticBatch { batch: self.batch.clone(), identity: Arc::clone(&self.identity) },
            passages, projection,
        })
    }

    pub fn hydrate_from_connection(
        &self,
        connection: &mut Connection,
        projection: PassageProjection,
        budget: HydrationBudget,
    ) -> Result<HydratedSemanticBatch<Self>, HydrationError> {
        let started = Instant::now();
        budget.validate(self.batch.hits().len())?;
        if budget.timeout.is_zero() { return Err(HydrationError::DeadlineExceeded); }
        let mut transaction = connection.transaction().map_err(|_| HydrationError::Database)?;
        let result = hydrate(&self.batch, &transaction, projection, budget, started);
        transaction.rollback().map_err(|_| HydrationError::Rollback)?;
        let passages = result?;
        if started.elapsed() >= budget.timeout { return Err(HydrationError::DeadlineExceeded); }
        Ok(HydratedSemanticBatch {
            source: SelectedSemanticBatch { batch: self.batch.clone(), identity: Arc::clone(&self.identity) },
            passages, projection,
        })
    }
}

struct Work {
    start: Instant,
    budget: HydrationBudget,
    source_bytes: usize,
    output_bytes: usize,
}

impl Work {
    fn new(budget: HydrationBudget, start: Instant) -> Result<Self, HydrationError> {
        budget.validate(0)?;
        let work = Self { start, budget, source_bytes: 0, output_bytes: 0 };
        work.check()?;
        Ok(work)
    }

    fn passages(&mut self, count: usize) -> Result<(), HydrationError> {
        self.budget.max_passages = self.budget.max_passages.checked_sub(count)
            .ok_or(HydrationError::BudgetExceeded)?;
        Ok(())
    }

    fn check(&self) -> Result<(), HydrationError> {
        if self.start.elapsed() >= self.budget.timeout {
            Err(HydrationError::DeadlineExceeded)
        } else {
            Ok(())
        }
    }
    fn source(&mut self, bytes: usize) -> Result<(), HydrationError> {
        self.source_bytes = self.source_bytes.checked_add(bytes)
            .filter(|&n| n <= self.budget.max_total_source_bytes)
            .ok_or(HydrationError::BudgetExceeded)?;
        Ok(())
    }
    fn output(&mut self, bytes: usize) -> Result<(), HydrationError> {
        self.output_bytes = self.output_bytes.checked_add(bytes)
            .filter(|&n| n <= self.budget.max_total_output_bytes)
            .ok_or(HydrationError::BudgetExceeded)?;
        Ok(())
    }
}

fn hydrate(
    batch: &SemanticSearchBatch,
    transaction: &Transaction<'_>,
    projection: PassageProjection,
    budget: HydrationBudget,
    started: Instant,
) -> Result<Vec<HydratedPassage>, HydrationError> {
    let mut work = Work::new(budget, started)?;
    hydrate_with_work(batch, transaction, projection, &mut work)
}

// One work account may span initial/refined phases. Only successfully verified
// passages are returned; an exhausted account is never reset by phase progress.
fn hydrate_with_work(
    batch: &SemanticSearchBatch,
    transaction: &Transaction<'_>,
    projection: PassageProjection,
    work: &mut Work,
) -> Result<Vec<HydratedPassage>, HydrationError> {
    work.check()?;
    work.passages(batch.hits().len())?;
    // Finish candidate/owner checks before reading any source bodies.
    let mut by_message = BTreeMap::<u64, Vec<usize>>::new();
    for (rank, hit) in batch.hits().iter().enumerate() {
        work.check()?;
        validate_candidate(batch, hit)?;
        by_message.entry(hit.document.message_id).or_default().push(rank);
    }
    let mut verified = vec![None; batch.hits().len()];
    for (message_id, ranks) in by_message {
        work.check()?;
        let source = read_message(transaction, message_id, work)?;
        let location = Arc::new(source.location());
        // Count a shared locator once, not once per chunk from the message.
        work.output(location.source_id.len().saturating_add(location.source_path.len()))?;
        for rank in ranks {
            work.check()?;
            let hit = &batch.hits()[rank];
            let canonical_text = verify_message(&hit.document, &source, projection)?;
            work.check()?;
            work.output(canonical_text.len())?;
            verified[rank] = Some(HydratedPassage {
                hit: hit.clone(), canonical_text, location: Arc::clone(&location),
            });
        }
        // The full source message is released here, not retained for the batch.
    }
    work.check()?;
    verified.into_iter().map(|hit| hit.ok_or(HydrationError::InvalidCandidate)).collect()
}

fn validate_candidate(batch: &SemanticSearchBatch, hit: &SemanticPassageHit) -> Result<(), HydrationError> {
    if hit.document.content_hash.is_none() { return Err(HydrationError::MissingFingerprint); }
    if hit.document.message_id > i64::MAX as u64
        || hit.document.to_doc_id_string() != hit.doc_id
        || !hit.ranking_score.is_finite()
        || (hit.fast.is_none() && hit.quality.is_none())
    {
        return Err(HydrationError::InvalidCandidate);
    }
    for (kind, contribution) in [(TierKind::Fast, &hit.fast), (TierKind::Quality, &hit.quality)] {
        if let Some(contribution) = contribution {
            let owner = batch.reader.tier(kind)
                .and_then(|tier| tier.shards.get(contribution.shard))
                .ok_or(HydrationError::InvalidCandidate)?;
            let row = owner.row(contribution.physical_index).map_err(|_| HydrationError::InvalidCandidate)?;
            if !contribution.score.is_finite() || contribution.tier_rank == 0
                || !row.flags().is_live() || row.doc_id() != hit.doc_id.as_str()
            {
                return Err(HydrationError::InvalidCandidate);
            }
        }
    }
    Ok(())
}

struct SourceMessage {
    message_id: u64,
    conversation_id: i64,
    content: String,
    role: String,
    created_at: Option<i64>,
    agent_id: Option<i64>,
    workspace_id: Option<i64>,
    source_id: String,
    source_path: String,
    idx: Option<i64>,
}

impl SourceMessage {
    fn location(&self) -> HydratedPassageLocation {
        HydratedPassageLocation {
            message_id: self.message_id,
            conversation_id: self.conversation_id,
            source_id: self.source_id.clone(),
            source_path: self.source_path.clone(),
            line_number: self.idx.and_then(|n| usize::try_from(n).ok()).and_then(|n| n.checked_add(1)),
        }
    }
}

fn read_message(transaction: &Transaction<'_>, id: u64, work: &mut Work) -> Result<SourceMessage, HydrationError> {
    let id = i64::try_from(id).map_err(|_| HydrationError::InvalidCandidate)?;
    // Probe UTF-8 bytes, not SQLite character counts (embedded NUL included).
    // This bounds transferred source bodies, not the engine's internal work.
    let probe = transaction.query_with_params(
        "SELECT m.conversation_id, length(CAST(m.content AS BLOB)),
           length(CAST(COALESCE(m.role, '') AS BLOB))
           + length(CAST(COALESCE(c.source_id, '') AS BLOB))
           + length(CAST(COALESCE(c.origin_host, '') AS BLOB))
           + length(CAST(COALESCE(c.source_path, '') AS BLOB))
         FROM messages m JOIN conversations c ON c.id = m.conversation_id
         WHERE m.id = ?1 LIMIT 2",
        &[crate::franken_sync::SqliteValue::Integer(id)],
    ).map_err(|_| HydrationError::Database)?;
    work.check()?;
    if probe.is_empty() { return Err(HydrationError::MissingSource); }
    if probe.len() != 1 { return Err(HydrationError::InvalidSource); }
    let conversation_id: i64 = probe[0].get_typed(0).map_err(|_| HydrationError::InvalidSource)?;
    let bytes: i64 = probe[0].get_typed(1).map_err(|_| HydrationError::InvalidSource)?;
    let metadata: i64 = probe[0].get_typed(2).map_err(|_| HydrationError::InvalidSource)?;
    let bytes = usize::try_from(bytes).map_err(|_| HydrationError::InvalidSource)?;
    let metadata = usize::try_from(metadata).map_err(|_| HydrationError::InvalidSource)?;
    if bytes > work.budget.max_message_bytes || metadata > MAX_METADATA_BYTES {
        return Err(HydrationError::BudgetExceeded);
    }
    work.source(bytes.checked_add(metadata).ok_or(HydrationError::BudgetExceeded)?)?;
    work.check()?;
    let rows = transaction.query_with_params(
        "SELECT m.conversation_id, m.content, m.role, m.created_at,
                c.agent_id, c.workspace_id, c.source_id, c.origin_host, c.source_path, m.idx
         FROM messages m JOIN conversations c ON c.id = m.conversation_id
         WHERE m.id = ?1 LIMIT 2",
        &[crate::franken_sync::SqliteValue::Integer(id)],
    ).map_err(|_| HydrationError::Database)?;
    work.check()?;
    if rows.len() != 1 { return Err(HydrationError::MissingSource); }
    let row = &rows[0];
    let actual_conversation: i64 = row.get_typed(0).map_err(|_| HydrationError::InvalidSource)?;
    let content: String = row.get_typed(1).map_err(|_| HydrationError::InvalidSource)?;
    let role: String = row.get_typed(2).map_err(|_| HydrationError::InvalidSource)?;
    let raw_source: Option<String> = row.get_typed(6).map_err(|_| HydrationError::InvalidSource)?;
    let host: Option<String> = row.get_typed(7).map_err(|_| HydrationError::InvalidSource)?;
    let source_path: String = row.get_typed(8).map_err(|_| HydrationError::InvalidSource)?;
    let actual_metadata = role.len().checked_add(raw_source.as_ref().map_or(0, String::len))
        .and_then(|n| n.checked_add(host.as_ref().map_or(0, String::len)))
        .and_then(|n| n.checked_add(source_path.len())).ok_or(HydrationError::InvalidSource)?;
    if actual_conversation != conversation_id || content.len() != bytes || actual_metadata != metadata {
        return Err(HydrationError::InvalidSource);
    }
    let source_id = canonical_source_id(raw_source.as_deref(), host.as_deref());
    Ok(SourceMessage {
        message_id: id as u64, conversation_id, content, role, source_id, source_path,
        created_at: row.get_typed(3).map_err(|_| HydrationError::InvalidSource)?,
        agent_id: row.get_typed(4).map_err(|_| HydrationError::InvalidSource)?,
        workspace_id: row.get_typed(5).map_err(|_| HydrationError::InvalidSource)?,
        idx: row.get_typed(9).map_err(|_| HydrationError::InvalidSource)?,
    })
}

// Match the canonical embedding packet's source/host fallback. Explicit source
// identity wins, including local; host fallback is used only for absent IDs.
fn canonical_source_id(source: Option<&str>, host: Option<&str>) -> String {
    match source.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) if id.eq_ignore_ascii_case(LOCAL_SOURCE_ID) => LOCAL_SOURCE_ID.to_owned(),
        Some(id) => id.to_owned(),
        None => host.map(str::trim).filter(|id| !id.is_empty()).unwrap_or(LOCAL_SOURCE_ID).to_owned(),
    }
}

fn component_id(value: Option<i64>) -> u32 {
    value.map(|n| u32::try_from(n.max(0)).unwrap_or(u32::MAX)).unwrap_or(0)
}

fn verify_message(expected: &SemanticDocId, actual: &SourceMessage, projection: PassageProjection) -> Result<String, HydrationError> {
    let expected_hash = expected.content_hash.ok_or(HydrationError::MissingFingerprint)?;
    if expected.message_id != actual.message_id
        || expected.agent_id != component_id(actual.agent_id)
        || expected.workspace_id != component_id(actual.workspace_id)
        || expected.source_id != crc32fast::hash(actual.source_id.as_bytes())
        || expected.role != role_code_from_str(&actual.role).unwrap_or(ROLE_USER)
        || expected.created_at_ms != actual.created_at.unwrap_or(0)
    {
        return Err(HydrationError::StaleSource);
    }
    let canonical = match projection {
        PassageProjection::CanonicalPrefixV1 if expected.chunk_idx == 0 => canonicalize_for_embedding(&actual.content),
        PassageProjection::CanonicalPrefixV1 => return Err(HydrationError::StaleSource),
        PassageProjection::RawPassagesV2 => {
            let passages = embedding_passages(&actual.content);
            let passage = passages.get(usize::from(expected.chunk_idx)).ok_or(HydrationError::StaleSource)?;
            canonicalize_for_embedding(passage)
        }
    };
    if canonical.is_empty() || content_hash(&canonical) != expected_hash {
        return Err(HydrationError::StaleSource);
    }
    Ok(canonical)
}
