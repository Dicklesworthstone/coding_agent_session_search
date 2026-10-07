//! Cancellable full rebuilds never expose an incomplete candidate.
//!
//! Cancellation is cooperative around caller code, row work, and native I/O.
//! A native finish/open cannot be preempted, but cancellation observed after it
//! still prevents installation. There is deliberately no post-install check:
//! an acknowledged publication must not be reported as an uncommitted cancel.

use super::*;
use frankensearch::index::VectorIndexWriter;

pub(super) fn run<I, F, C>(
    indexer: &SemanticIndexer,
    messages: I,
    data_dir: &Path,
    mut on_progress: Option<F>,
    mut check_cancelled: C,
) -> Result<VectorIndex>
where
    I: IntoIterator<Item = EmbeddedMessage>,
    F: FnMut(usize),
    C: FnMut() -> Result<()>,
{
    check_cancelled()?;
    let destination = vector_index_path(data_dir, indexer.embedder_id());
    refuse_publication_sidecars(&destination, false)?;
    let previous = RebuildDestination::capture(&destination)?;
    let revision = expected_vector_space_revision(indexer.embedder_id())
        .context("full rebuild has no registered vector-space revision")?;
    let parent = destination
        .parent()
        .context("semantic index has no parent")?;
    check_cancelled()?;
    fs::create_dir_all(parent)?;
    let scratch = tempfile::Builder::new()
        .prefix(".semantic-full-rebuild-")
        .tempdir_in(parent)?;
    let candidate_path = vector_index_path(scratch.path(), indexer.embedder_id());
    fs::create_dir_all(candidate_path.parent().context("candidate has no parent")?)?;
    if previous.generation.is_none() {
        // Keep the engine's existing first-generation/progress interface live.
        // It accepts an infallible iterator, so a cancelled adapter stops input
        // and retains the exact error. Any finished prefix remains private and
        // is rejected before candidate admission or installation.
        build_initial(
            indexer,
            messages,
            scratch.path(),
            &mut on_progress,
            &mut check_cancelled,
        )?;
    } else {
        let mut writer = VectorIndex::create_with_revision(
            &candidate_path,
            indexer.embedder_id(),
            revision,
            indexer.embedder_dimension(),
            Quantization::F16,
        )?
        .with_generation(previous.generation.map_or(1, next_generation));

        // into_iter(), next(), and the progress callback are arbitrary caller code.
        // In particular, final None must not hide a cancellation from publication.
        check_cancelled()?;
        let mut messages = messages.into_iter();
        let mut accepted = 0usize;
        loop {
            check_cancelled()?;
            let embedded = messages.next();
            check_cancelled()?;
            let Some(embedded) = embedded else {
                break;
            };
            write_record(&mut writer, &embedded, indexer.embedder_dimension())?;
            accepted = accepted.saturating_add(1);
            if let Some(progress) = on_progress.as_mut() {
                progress(accepted);
            }
            check_cancelled()?;
        }
        // Drop arbitrary iterator state before the final cancellation fence too.
        drop(messages);
        check_cancelled()?;
        writer
            .finish()
            .context("finish unpublished semantic replacement")?;
        check_cancelled()?;
    }

    let candidate = VectorIndex::open_read_only(&candidate_path)?;
    validate_candidate(&candidate, &mut check_cancelled)?;
    drop(candidate);
    refuse_publication_sidecars(&destination, false)?;
    ensure!(
        RebuildDestination::capture(&destination)? == previous,
        "semantic destination changed during full rebuild; retry under the maintenance lock"
    );
    check_cancelled()?;
    VectorIndex::install_replacement(&destination, &candidate_path)
        .context("install complete semantic rebuild")
}

fn build_initial<I, F>(
    indexer: &SemanticIndexer,
    messages: I,
    scratch: &Path,
    on_progress: &mut Option<F>,
    check_cancelled: &mut impl FnMut() -> Result<()>,
) -> Result<()>
where
    I: IntoIterator<Item = EmbeddedMessage>,
    F: FnMut(usize),
{
    check_cancelled()?;
    let mut messages = messages.into_iter();
    let mut cancellation = None;
    let checked = std::iter::from_fn(|| {
        if cancellation.is_some() {
            return None;
        }
        if let Err(error) = check_cancelled() {
            cancellation = Some(error);
            return None;
        }
        let message = messages.next();
        if let Err(error) = check_cancelled() {
            cancellation = Some(error);
            return None;
        }
        message
    });
    let result = match on_progress.as_mut() {
        Some(progress) => indexer
            .inner
            .build_and_save_index_with_progress(checked, scratch, progress),
        None => indexer.inner.build_and_save_index(checked, scratch),
    };
    drop(messages);
    if let Some(error) = cancellation {
        return Err(error);
    }
    check_cancelled()?;
    result.map(drop)
}

fn write_record(
    writer: &mut VectorIndexWriter,
    embedded: &EmbeddedMessage,
    dimension: usize,
) -> Result<()> {
    ensure!(
        embedded.embedding.len() == dimension,
        "embedding dimension mismatch: expected {}, got {}",
        dimension,
        embedded.embedding.len()
    );
    ensure!(
        embedded.embedding.iter().all(|value| value.is_finite()),
        "embedding for message {} contains a non-finite value",
        embedded.message_id
    );
    let doc_id = SemanticDocId {
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
    writer
        .write_record(&doc_id, &embedded.embedding)
        .map_err(|error| {
            let message = format!("write fsvi record failed: {error}");
            anyhow::Error::new(error).context(message)
        })
}

fn validate_candidate(
    candidate: &VectorIndex,
    check_cancelled: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    check_cancelled()?;
    ensure!(
        candidate.wal_record_count() == 0,
        "unpublished rebuild has a WAL"
    );
    // The native writer sorts by (document hash, exact ID), so duplicate exact
    // IDs are adjacent even if separated in input. This needs no corpus-sized
    // seen set and never conflates different chunks/provenance of one message.
    let mut previous_id = None;
    for row in 0..candidate.record_count() {
        check_cancelled()?;
        let id = candidate.doc_id_at(row)?;
        ensure!(
            previous_id != Some(id),
            "full rebuild contains duplicate semantic document identity at row {row}"
        );
        previous_id = Some(id);
        ensure!(
            candidate.is_vector_usable(row),
            "unusable persisted semantic vector at row {row}"
        );
    }
    check_cancelled()?;
    Ok(())
}
