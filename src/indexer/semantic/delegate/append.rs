//! Bounded, last-write-wins preparation before acquiring a live FSVI writer.
//!
//! The byte budget covers retained owned vector/key capacities and entry values,
//! not the caller's iterator, hash-table slack, existing index/WAL, or the native
//! writer's copies. It is an admission bound, not a process RSS ceiling. A rejected
//! request is never silently split into independently committed WAL batches.

use super::*;
use frankensearch::SearchError;
use std::mem::size_of;

const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_BYTES_ENV: &str = "CASS_SEMANTIC_APPEND_MAX_BYTES";

#[derive(Debug)]
struct Pending {
    last_position: usize,
    message: EmbeddedMessage,
    owned_bytes: usize,
}

#[derive(Debug, Default)]
struct Prepared {
    entries: HashMap<String, Pending>,
    input_count: usize,
    owned_bytes: usize,
}

fn budget_error(limit: usize) -> anyhow::Error {
    SearchError::InvalidConfig {
        field: "semantic_append.max_bytes".into(),
        value: limit.to_string(),
        reason: "append preparation exceeds its byte budget; no batch was committed; submit smaller explicit batches".into(),
    }
    .into()
}

fn configured_max_bytes() -> Result<usize> {
    match dotenvy::var(MAX_BYTES_ENV) {
        Ok(value) => {
            let bytes = value.parse::<usize>().ok().filter(|bytes| *bytes > 0);
            let bytes = bytes
                .with_context(|| format!("{MAX_BYTES_ENV} must be a positive byte count"))?;
            Ok(bytes.min(DEFAULT_MAX_BYTES))
        }
        Err(dotenvy::Error::EnvVar(std::env::VarError::NotPresent)) => Ok(DEFAULT_MAX_BYTES),
        Err(error) => Err(error).context("read semantic append byte budget"),
    }
}

fn validate_vector(vector: &[f32], dimension: usize, quantization: Quantization) -> Result<()> {
    if vector.len() != dimension {
        return Err(SearchError::DimensionMismatch {
            expected: dimension,
            found: vector.len(),
        }
        .into());
    }
    let mut input_norm = 0.0f32;
    let mut stored_norm = 0.0f32;
    for &value in vector {
        let stored = match quantization {
            Quantization::F16 => half::f16::from_f32(value).to_f32(),
            Quantization::F32 => value,
        };
        if !value.is_finite() || !stored.is_finite() {
            return Err(invalid_vector());
        }
        input_norm += value * value;
        stored_norm += stored * stored;
    }
    if !input_norm.is_finite()
        || input_norm <= 0.0
        || !stored_norm.is_finite()
        || stored_norm <= 0.0
    {
        return Err(invalid_vector());
    }
    Ok(())
}

fn invalid_vector() -> anyhow::Error {
    SearchError::InvalidConfig {
        field: "semantic_append.embedding".into(),
        value: "<invalid input or stored vector>".into(),
        reason: "embedding must have finite components and a finite nonzero norm both before and after storage quantization".into(),
    }
    .into()
}

impl Prepared {
    fn collect<I, C>(
        messages: I,
        dimension: usize,
        quantization: Quantization,
        max_bytes: usize,
        mut check_cancelled: C,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = EmbeddedMessage>,
        C: FnMut() -> Result<()>,
    {
        let mut prepared = Self::default();
        let mut messages = messages.into_iter();
        loop {
            check_cancelled()?;
            let message = messages.next();
            // next() is arbitrary caller code. It may set cancellation even
            // while returning the final None; do not commit on that boundary.
            check_cancelled()?;
            let Some(message) = message else {
                return Ok(prepared);
            };
            // Validate every occurrence, including an invalid earlier value
            // later superseded by a good value. Native append validates first.
            validate_vector(&message.embedding, dimension, quantization)?;
            let id = SemanticDocId {
                message_id: message.message_id,
                chunk_idx: message.chunk_idx,
                agent_id: message.agent_id,
                workspace_id: message.workspace_id,
                source_id: message.source_id,
                role: message.role,
                created_at_ms: message.created_at_ms,
                content_hash: Some(message.content_hash),
            }
            .to_doc_id_string();
            let previous = prepared.entries.get_key_value(&id);
            let key_capacity = previous.map_or(id.capacity(), |(key, _)| key.capacity());
            let owned_bytes = message
                .embedding
                .capacity()
                .checked_mul(size_of::<f32>())
                .and_then(|bytes| bytes.checked_add(key_capacity))
                .and_then(|bytes| bytes.checked_add(size_of::<(String, Pending)>()))
                .ok_or_else(|| budget_error(max_bytes))?;
            let prior_bytes = previous.map_or(0, |(_, value)| value.owned_bytes);
            let total_bytes = prepared
                .owned_bytes
                .checked_sub(prior_bytes)
                .and_then(|bytes| bytes.checked_add(owned_bytes))
                .filter(|bytes| *bytes <= max_bytes)
                .ok_or_else(|| budget_error(max_bytes))?;
            let input_count = prepared
                .input_count
                .checked_add(1)
                .context("semantic append input count overflow")?;
            let pending = Pending {
                last_position: prepared.input_count,
                message,
                owned_bytes,
            };
            if let Some(previous) = prepared.entries.get_mut(&id) {
                *previous = pending;
            } else {
                // Never reserve from an untrusted iterator size_hint().
                prepared.entries.try_reserve(1)?;
                prepared.entries.insert(id, pending);
            }
            prepared.input_count = input_count;
            prepared.owned_bytes = total_bytes;
        }
    }

    fn into_ordered_messages(self) -> Result<Vec<EmbeddedMessage>> {
        let mut ordered = Vec::new();
        ordered.try_reserve_exact(self.entries.len())?;
        ordered.extend(self.entries);
        // Match the native reverse/dedup/reverse algorithm exactly. Hash-map
        // iteration order must not determine equal-score WAL tie behavior.
        ordered.sort_unstable_by_key(|(_, pending)| pending.last_position);
        Ok(ordered
            .into_iter()
            .map(|(_, pending)| pending.message)
            .collect())
    }
}

pub(super) fn run(
    indexer: &SemanticIndexer,
    messages: impl IntoIterator<Item = EmbeddedMessage>,
    data_dir: &Path,
) -> Result<usize> {
    run_with_limit(indexer, messages, data_dir, configured_max_bytes()?, || {
        indexer.inner.check_external_cancelled()
    })
}

fn run_with_limit(
    indexer: &SemanticIndexer,
    messages: impl IntoIterator<Item = EmbeddedMessage>,
    data_dir: &Path,
    max_bytes: usize,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<usize> {
    check_cancelled()?;
    let path = vector_index_path(data_dir, indexer.embedder_id());
    let wal = wal_path_for(&path);
    let before = RebuildDestination::capture(&path)?;
    let wal_before = ObservedSemanticFile::capture(&wal)?;
    let source =
        VectorIndex::open_read_only(&path).context("open semantic append source read-only")?;
    check_contract(indexer, &source)?;
    ensure!(
        RebuildDestination::capture(&path)? == before
            && ObservedSemanticFile::capture(&wal)? == wal_before,
        "semantic append source changed during read-only admission"
    );
    let quantization = source.quantization();
    let prepared = Prepared::collect(
        messages,
        indexer.embedder_dimension(),
        quantization,
        max_bytes,
        &mut check_cancelled,
    )?;
    let count = prepared.input_count;
    let messages = prepared.into_ordered_messages()?;
    ensure!(
        RebuildDestination::capture(&path)? == before
            && ObservedSemanticFile::capture(&wal)? == wal_before,
        "semantic append source changed during preparation; retry under the maintenance lock"
    );
    check_cancelled()?;
    if count == 0 {
        return Ok(0);
    }
    drop(source);

    // Keep one authoritative persistence implementation. Its native writer
    // validates the contract again and writes one atomic WAL batch, then runs
    // the established compaction/error path. The caller's maintenance lock
    // must span source selection through this call; the retained observations
    // are not an atomic upgrade from the shared lock dropped above.
    check_cancelled()?;
    indexer.inner.append_to_index(messages, data_dir)?;
    // Preserve the API's input-row count even when repeated identities were
    // reduced to their last values. Never report cancellation after commit.
    Ok(count)
}

fn check_contract(indexer: &SemanticIndexer, source: &VectorIndex) -> Result<()> {
    let revision = expected_vector_space_revision(indexer.embedder_id())
        .context("semantic append has no registered vector-space revision")?;
    ensure!(
        source.embedder_id() == indexer.embedder_id()
            && source.embedder_revision() == revision
            && source.dimension() == indexer.embedder_dimension(),
        "semantic index is incompatible with current vector space; rebuild semantic vectors before appending"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
