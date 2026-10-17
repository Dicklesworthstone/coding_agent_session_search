//! Canonical replacement assembly with cancellation and one-owner vector handoff.
//!
//! The canonical identity set remains caller-owned. Replacement vectors are
//! released as soon as the native writer copies them, before retained rows are
//! added. Native writer buffering and identity tables remain proportional to
//! the result set; this does not establish a process memory ceiling.

use super::*;
use frankensearch::index::VectorIndexWriter;

pub(super) fn run(
    indexer: &SemanticIndexer,
    embedded_messages: Vec<EmbeddedMessage>,
    data_dir: &Path,
    tier: TierKind,
    db_fingerprint: &str,
    current_doc_ids: &HashSet<String>,
    mut check_cancelled: impl FnMut() -> Result<()>,
) -> Result<VectorIndex> {
    check_cancelled()?;
    let destination = vector_index_path(data_dir, indexer.embedder_id());
    let wal_path = wal_path_for(&destination);
    let wal_before = ObservedSemanticFile::capture(&wal_path)?;
    // An empty embedding delta is not proof of unchanged coverage. Admit
    // a no-op only after comparing every current identity and checking
    // the stored vectors under a retained native reader. In that case no
    // generation or recovery sidecar is being replaced or retired.
    if wal_before.is_none() && embedded_messages.is_empty() {
        ensure!(
            !db_fingerprint.trim().is_empty(),
            "canonical semantic reconciliation requires a DB fingerprint"
        );
        if let Some(source) = indexer.retain_unchanged_canonical_index(
            &destination,
            current_doc_ids,
            &mut check_cancelled,
        )? {
            tracing::info!(
                tier = tier.as_str(),
                retained_docs = source.record_count(),
                "canonical semantic reconciliation retained unchanged generation"
            );
            return Ok(source);
        }
    }
    refuse_publication_sidecars(&destination, true)?;
    ensure!(
        !db_fingerprint.trim().is_empty(),
        "canonical semantic reconciliation requires a DB fingerprint"
    );
    let revision = expected_vector_space_revision(indexer.embedder_id())
        .context("canonical reconciliation has no registered vector-space revision")?;
    let mut replacements = prepare_replacements(
        indexer,
        embedded_messages,
        current_doc_ids,
        &mut check_cancelled,
    )?;

    // Unlike an unqualified full rebuild, this call has the exact canonical
    // set needed to retire deleted records and retain acknowledged WAL rows.
    refuse_publication_sidecars(&destination, true)?;
    check_cancelled()?;
    let previous = RebuildDestination::capture(&destination)?;
    let complete_replacement = replacements.len() == current_doc_ids.len();
    // The same reader-owned merge serves both WAL and log-free sources.
    // Even a complete replacement must respect the existing artifact's
    // reader/writer lock and header admission, rather than overwriting an
    // unreadable generation. An absent main is not authority over its WAL.
    let source = if previous.file.is_some() {
        Some(VectorIndex::open_read_only(&destination)?)
    } else {
        ensure!(
            wal_before.is_none(),
            "canonical reconciliation refuses an orphan WAL; recover its main generation first"
        );
        ensure!(
            complete_replacement,
            "initial canonical reconciliation lacks vectors for {} current documents",
            current_doc_ids.len().saturating_sub(replacements.len())
        );
        None
    };
    check_cancelled()?;
    ensure!(
        RebuildDestination::capture(&destination)? == previous
            && ObservedSemanticFile::capture(&wal_path)? == wal_before,
        "semantic source changed while opening canonical reconciliation"
    );
    ensure!(
        complete_replacement
            || source.as_ref().is_some_and(|source| {
                source.embedder_id() == indexer.embedder_id()
                    && source.embedder_revision() == revision
                    && source.dimension() == indexer.embedder_dimension()
            }),
        "incompatible vector space requires a complete canonical replacement"
    );

    let parent = destination
        .parent()
        .context("semantic index has no parent")?;
    check_cancelled()?;
    fs::create_dir_all(parent)?;
    let scratch = tempfile::Builder::new()
        .prefix(".semantic-reconcile-")
        .tempdir_in(parent)?;
    let candidate_path = scratch.path().join("candidate.fsvi");
    let mut writer = VectorIndex::create_with_revision(
        &candidate_path,
        indexer.embedder_id(),
        revision,
        indexer.embedder_dimension(),
        if complete_replacement {
            Quantization::F16
        } else {
            source
                .as_ref()
                .context("canonical partial reuse requires a source index")?
                .quantization()
        },
    )?
    .with_generation(previous.generation.map_or(1, next_generation));
    let mut remaining = HashSet::new();
    remaining.try_reserve(current_doc_ids.len())?;
    fill_remaining(&mut remaining, current_doc_ids, &mut check_cancelled)?;
    write_replacements(
        &mut replacements,
        &mut writer,
        &mut remaining,
        &mut check_cancelled,
    )?;

    if !complete_replacement {
        let source = source
            .as_ref()
            .context("canonical partial reuse requires a source index")?;
        let mut wal_ids = HashSet::new();
        wal_ids.try_reserve(source.wal_record_count())?;
        // The native reader has already applied last-write-wins within the
        // retained log. Admit those rows BEFORE main rows so a pre-cleanup
        // main image cannot override an acknowledged replacement.
        for (id, vector) in source.wal_records() {
            check_cancelled()?;
            ensure!(wal_ids.insert(id), "duplicate retained WAL identity");
            if remaining.remove(id) {
                writer.write_record(id, vector)?;
            }
        }
        for row in 0..source.record_count() {
            check_cancelled()?;
            if source.is_deleted(row) {
                continue;
            }
            let id = source.doc_id_at(row)?;
            if !current_doc_ids.contains(id)
                || replacements.contains_key(id)
                || wal_ids.contains(id)
            {
                continue;
            }
            ensure!(
                remaining.remove(id),
                "duplicate current document in the source index"
            );
            writer.write_record(id, &source.vector_at_f32(row)?)?;
        }
    }
    ensure!(
        remaining.is_empty(),
        "canonical reconciliation lacks vectors for {} current documents",
        remaining.len()
    );
    check_cancelled()?;
    writer
        .finish()
        .context("finish unpublished canonical generation")?;

    check_cancelled()?;
    let candidate = VectorIndex::open_read_only(&candidate_path)?;
    check_cancelled()?;
    ensure!(
        candidate.wal_record_count() == 0 && candidate.record_count() == current_doc_ids.len(),
        "canonical replacement has incomplete or non-live physical coverage"
    );
    fill_remaining(&mut remaining, current_doc_ids, &mut check_cancelled)?;
    for row in 0..candidate.record_count() {
        check_cancelled()?;
        ensure!(
            !candidate.is_deleted(row),
            "canonical replacement contains a tombstone"
        );
        ensure!(
            remaining.remove(candidate.doc_id_at(row)?),
            "persisted replacement has an unexpected or duplicate identity"
        );
        ensure!(
            candidate.is_vector_usable(row),
            "unusable persisted canonical vector at row {row}"
        );
    }
    ensure!(
        remaining.is_empty(),
        "persisted replacement lacks canonical identities"
    );
    drop(candidate);

    // Leave the acknowledged WAL with its original main until the native
    // generation-aware rename invalidates it atomically. Never park/delete
    // it first, and never infer identity from the wrapping generation alone.
    refuse_publication_sidecars(&destination, true)?;
    ensure!(
        RebuildDestination::capture(&destination)? == previous
            && ObservedSemanticFile::capture(&wal_path)? == wal_before,
        "semantic source changed during canonical reconciliation; retry under the maintenance lock"
    );
    check_cancelled()?;
    let published = VectorIndex::install_replacement(&destination, &candidate_path)
        .context("install complete canonical semantic generation")?;
    tracing::info!(
        tier = tier.as_str(),
        published_docs = published.record_count(),
        replaced_docs = replacements.len(),
        retained_wal_rows = source.as_ref().map_or(0, VectorIndex::wal_record_count),
        "published canonical semantic reconciliation"
    );
    Ok(published)
}

// Keep borrowed canonical keys even after a vector is handed off, so main/WAL
// precedence checks need no second replacement-ID inventory or string copies.
type ReplacementVectors<'a> = HashMap<&'a str, Option<Vec<f32>>>;

fn prepare_replacements<'a>(
    indexer: &SemanticIndexer,
    messages: Vec<EmbeddedMessage>,
    current_doc_ids: &'a HashSet<String>,
    check_cancelled: &mut impl FnMut() -> Result<()>,
) -> Result<ReplacementVectors<'a>> {
    check_cancelled()?;
    let mut replacements = HashMap::new();
    replacements.try_reserve(messages.len())?;
    // Validate the complete replacement set before opening a source or writer.
    for embedded in messages {
        check_cancelled()?;
        ensure!(
            embedded.embedding.len() == indexer.embedder_dimension(),
            "canonical replacement has the wrong vector dimension"
        );
        let norm = embedded
            .embedding
            .iter()
            .fold(0.0f32, |sum, value| sum + value * value);
        ensure!(
            norm.is_finite() && norm > 0.0,
            "canonical replacement has a non-finite or zero-norm vector"
        );
        let id = SemanticDocId {
            message_id: embedded.message_id,
            chunk_idx: embedded.chunk_idx,
            agent_id: embedded.agent_id,
            workspace_id: embedded.workspace_id,
            source_id: embedded.source_id,
            role: embedded.role,
            created_at_ms: embedded.created_at_ms,
            content_hash: Some(embedded.content_hash),
        }
        .to_doc_id_string();
        let canonical_id = current_doc_ids
            .get(id.as_str())
            .context("replacement document is not in the current canonical set")?;
        ensure!(
            replacements
                .insert(canonical_id.as_str(), Some(embedded.embedding))
                .is_none(),
            "duplicate canonical replacement document"
        );
    }
    check_cancelled()?;
    Ok(replacements)
}

fn write_replacements<'a>(
    replacements: &mut ReplacementVectors<'a>,
    writer: &mut VectorIndexWriter,
    remaining: &mut HashSet<&'a str>,
    check_cancelled: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for (&id, vector) in replacements {
        check_cancelled()?;
        let vector = vector
            .take()
            .context("canonical replacement was already consumed")?;
        writer.write_record(id, &vector)?;
        remaining.remove(id);
        // The native writer now owns its copy. Drop the original immediately,
        // rather than holding every replacement through finish and validation.
    }
    check_cancelled()?;
    Ok(())
}

fn fill_remaining<'a>(
    remaining: &mut HashSet<&'a str>,
    current_doc_ids: &'a HashSet<String>,
    check_cancelled: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    for id in current_doc_ids {
        check_cancelled()?;
        remaining.insert(id.as_str());
    }
    check_cancelled()?;
    Ok(())
}

#[cfg(test)]
mod tests;
