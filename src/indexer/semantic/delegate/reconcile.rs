//! Canonical replacement assembly with cancellation and one-owner vector handoff.
//!
//! The canonical identity set remains caller-owned. Eager inputs retain their
//! complete preflight; fallible producers validate and hand off one vector at a
//! time without collecting the replacement set. Retained source/WAL storage,
//! native writer buffering and identity tables still scale with the result set;
//! this is not a process memory ceiling.

use super::*;

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
    // GH #527: malformed input must not create directories or authorize the
    // rebuild admission sweep to remove an interrupted operation's scratch.
    // These checks consume only caller-owned vectors and canonical identities.
    ensure!(
        !db_fingerprint.trim().is_empty(),
        "canonical semantic reconciliation requires a DB fingerprint"
    );
    expected_vector_space_revision(indexer.embedder_id())
        .context("canonical reconciliation has no registered vector-space revision")?;
    let mut replacements = prepare_replacements(
        indexer,
        embedded_messages,
        current_doc_ids,
        &mut check_cancelled,
    )?;
    let plan = CanonicalReconciliationPlan {
        tier,
        db_fingerprint,
        current_doc_ids,
        replacement_count: replacements.len(),
    };
    publish(
        indexer,
        data_dir,
        plan,
        Admission::Prepared,
        |emit| {
            write_replacements(&mut replacements, emit)?;
            drop(replacements);
            Ok(())
        },
        check_cancelled,
    )
}

/// A fallible producer cannot preflight all vectors without retaining them.
/// Use the existing-writer lease, which never sweeps other recovery scratch.
/// The producer must yield exactly the planned delta, including for an empty
/// delta: its final source and cancellation fences must still execute.
pub(super) fn run_with_producer<P, C>(
    indexer: &SemanticIndexer,
    data_dir: &Path,
    plan: CanonicalReconciliationPlan<'_>,
    produce: P,
    mut check_cancelled: C,
) -> Result<VectorIndex>
where
    P: FnOnce(&mut dyn FnMut(EmbeddedMessage) -> Result<()>) -> Result<()>,
    C: FnMut() -> Result<()>,
{
    check_cancelled()?;
    ensure!(
        !plan.db_fingerprint.trim().is_empty(),
        "canonical semantic reconciliation requires a DB fingerprint"
    );
    ensure!(
        plan.replacement_count <= plan.current_doc_ids.len(),
        "planned replacement count exceeds the canonical document set"
    );
    let current_doc_ids = plan.current_doc_ids;
    publish(
        indexer,
        data_dir,
        plan,
        Admission::Streaming,
        |emit| {
            let mut rejected = false;
            produce(&mut |embedded| {
                ensure!(
                    !rejected,
                    "canonical producer continued after a rejected replacement"
                );
                let result = (|| {
                    let id = canonical_replacement_id(indexer, &embedded, current_doc_ids)?;
                    emit(id, embedded.embedding)
                })();
                rejected = result.is_err();
                result
            })?;
            ensure!(
                !rejected,
                "canonical producer ignored a rejected replacement"
            );
            Ok(())
        },
        check_cancelled,
    )
}

#[derive(Clone, Copy)]
enum Admission {
    Prepared,
    Streaming,
}

fn publish<P, C>(
    indexer: &SemanticIndexer,
    data_dir: &Path,
    plan: CanonicalReconciliationPlan<'_>,
    admission: Admission,
    produce: P,
    mut check_cancelled: C,
) -> Result<VectorIndex>
where
    P: FnOnce(&mut dyn FnMut(&str, Vec<f32>) -> Result<()>) -> Result<()>,
    C: FnMut() -> Result<()>,
{
    let current_doc_ids = plan.current_doc_ids;
    let tier = plan.tier;
    let replacement_count = plan.replacement_count;
    let complete_replacement = replacement_count == current_doc_ids.len();
    let revision = expected_vector_space_revision(indexer.embedder_id())
        .context("canonical reconciliation has no registered vector-space revision")?;
    let destination = vector_index_path(data_dir, indexer.embedder_id());
    if !complete_replacement {
        // This observation can only REFUSE early. It cannot admit a source or
        // publication: the complete source/WAL checks below run under the lease
        // again, so disappearance or replacement during admission stays closed.
        match fs::symlink_metadata(&destination) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!(
                    "initial canonical reconciliation lacks vectors for {} current documents",
                    current_doc_ids.len().saturating_sub(replacement_count)
                );
            }
            Err(error) => {
                return Err(error).context("inspect canonical reconciliation destination");
            }
            Ok(_) => {}
        }
    }
    check_cancelled()?;
    let mut produce = Some(produce);
    if replacement_count == 0 {
        let mut rejected = false;
        produce.take().context("missing canonical producer")?(&mut |_, _| {
            rejected = true;
            anyhow::bail!("canonical producer exceeded its planned replacement count")
        })?;
        ensure!(
            !rejected,
            "canonical producer ignored an unplanned replacement"
        );
        check_cancelled()?;
    }
    // Hold the lease through authoritative source observations and all writes.
    // Its scratch borrow also prevents releasing it before TempDir cleanup.
    let artifacts = match admission {
        Admission::Prepared => super::super::artifacts::BackfillArtifacts::for_rebuild(data_dir)?,
        Admission::Streaming => {
            super::super::artifacts::BackfillArtifacts::for_existing_writer(data_dir)?
        }
    };
    check_cancelled()?;
    let wal_path = wal_path_for(&destination);
    let wal_before = ObservedSemanticFile::capture(&wal_path)?;
    // An empty embedding delta is not proof of unchanged coverage. Admit
    // a no-op only after comparing every current identity and checking
    // the stored vectors under a retained native reader. In that case no
    // generation or recovery sidecar is being replaced or retired.
    if wal_before.is_none()
        && replacement_count == 0
        && let Some(source) = indexer.retain_unchanged_canonical_index(
            &destination,
            current_doc_ids,
            &mut check_cancelled,
        )?
    {
        tracing::info!(
            tier = tier.as_str(),
            retained_docs = source.record_count(),
            "canonical semantic reconciliation retained unchanged generation"
        );
        return Ok(source);
    }
    refuse_publication_sidecars(&destination, true)?;

    // Unlike an unqualified full rebuild, this call has the exact canonical
    // set needed to retire deleted records and retain acknowledged WAL rows.
    refuse_publication_sidecars(&destination, true)?;
    check_cancelled()?;
    let previous = RebuildDestination::capture(&destination)?;
    ensure!(
        matches!(admission, Admission::Prepared) || previous.file.is_some(),
        "streaming canonical reconciliation requires its existing generation"
    );
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
            current_doc_ids.len().saturating_sub(replacement_count)
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

    check_cancelled()?;
    let scratch = artifacts.scratch()?;
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
    let mut replacement_ids = HashSet::new();
    replacement_ids.try_reserve(replacement_count)?;
    if let Some(produce) = produce {
        let mut rejected = false;
        produce(&mut |id, vector| {
            ensure!(
                !rejected,
                "canonical producer continued after a rejected replacement"
            );
            let result = (|| {
                check_cancelled()?;
                ensure!(
                    replacement_ids.len() < replacement_count,
                    "canonical producer exceeded its planned replacement count"
                );
                let id = current_doc_ids
                    .get(id)
                    .context("replacement document is not in the current canonical set")?
                    .as_str();
                ensure!(
                    replacement_ids.insert(id),
                    "duplicate canonical replacement document"
                );
                writer.write_record(id, &vector)?;
                remaining.remove(id);
                check_cancelled()
            })();
            rejected = result.is_err();
            result
        })?;
        ensure!(
            !rejected,
            "canonical producer ignored a rejected replacement"
        );
    }
    check_cancelled()?;
    ensure!(
        replacement_ids.len() == replacement_count,
        "canonical producer did not yield every planned replacement"
    );

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
            if !current_doc_ids.contains(id) || replacement_ids.contains(id) || wal_ids.contains(id)
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
    // Close the source reader and both observations before the rename: each
    // holds the destination or its WAL open, and Windows refuses to rename
    // over a file while any handle to it is open ("Access is denied", if17v).
    let retained_wal_rows = source.as_ref().map_or(0, VectorIndex::wal_record_count);
    drop(source);
    drop(previous);
    drop(wal_before);
    check_cancelled()?;
    let published = VectorIndex::install_replacement(&destination, &candidate_path)
        .context("install complete canonical semantic generation")?;
    tracing::info!(
        tier = tier.as_str(),
        published_docs = published.record_count(),
        replaced_docs = replacement_ids.len(),
        retained_wal_rows,
        "published canonical semantic reconciliation"
    );
    Ok(published)
}

// Eager callers preflight before acquiring any lease. Borrow canonical keys
// without cloning their strings; release the table once its vectors are emitted.
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
        let canonical_id = canonical_replacement_id(indexer, &embedded, current_doc_ids)?;
        ensure!(
            replacements
                .insert(canonical_id, Some(embedded.embedding))
                .is_none(),
            "duplicate canonical replacement document"
        );
    }
    check_cancelled()?;
    Ok(replacements)
}

fn canonical_replacement_id<'a>(
    indexer: &SemanticIndexer,
    embedded: &EmbeddedMessage,
    current_doc_ids: &'a HashSet<String>,
) -> Result<&'a str> {
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
    current_doc_ids
        .get(id.as_str())
        .map(String::as_str)
        .context("replacement document is not in the current canonical set")
}

fn write_replacements(
    replacements: &mut ReplacementVectors<'_>,
    emit: &mut dyn FnMut(&str, Vec<f32>) -> Result<()>,
) -> Result<()> {
    for (&id, vector) in replacements {
        let vector = vector
            .take()
            .context("canonical replacement was already consumed")?;
        emit(id, vector)?;
        // The native writer now owns its copy. Drop the original immediately,
        // rather than holding every replacement through finish and validation.
    }
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
