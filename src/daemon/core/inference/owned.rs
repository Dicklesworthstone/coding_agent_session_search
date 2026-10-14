//! Attestation checks the identity-bearing responses of the acquired producer.
//! A stable public slot before/after inference does not bind intermediate chunks.

use super::*;

pub(super) fn attested_batch(
    producer: &dyn Embedder,
    state: &DaemonAttestationState,
    operation: DaemonOperationV1,
    texts: &[&str],
    budget: &Budget<'_>,
) -> Result<Vec<Vec<f32>>, ErrorResponse> {
    budget.check()?;
    verify_live_identity(producer, state)?;
    // Honor producer overrides. Never infer raw vectors and then attach the
    // connection's expected identity to them at this boundary.
    let outcome = match operation {
        DaemonOperationV1::Embed if texts.len() == 1 => producer
            .embed_bound_sync(texts[0])
            .map(|embedding| vec![embedding]),
        DaemonOperationV1::EmbedBatch => producer.embed_batch_bound_sync(texts),
        _ => {
            return Err(error(
                ErrorCode::InvalidInput,
                "invalid attested embedding operation",
                false,
            ));
        }
    };
    let embeddings = outcome.map_err(|source| {
        tracing::warn!(error = %source, "Daemon bound embedding inference failed");
        error(
            ErrorCode::Internal,
            "native embedding inference failed",
            false,
        )
    })?;
    budget.check()?;
    verify_live_identity(producer, state)?;
    if embeddings.len() != texts.len() {
        return Err(error(
            ErrorCode::Internal,
            "native embedder returned the wrong batch length",
            false,
        ));
    }
    let mut vectors = Vec::with_capacity(embeddings.len());
    for embedding in embeddings {
        budget.check()?;
        embedding.validate().map_err(|_| {
            error(
                ErrorCode::Internal,
                "native embedder returned an invalid bound embedding",
                false,
            )
        })?;
        if embedding.identity != state.connection.embedding_identity {
            return Err(error(
                ErrorCode::ModelLoadFailed,
                "embedding response does not match the request's attested producer",
                false,
            ));
        }
        vectors.push(embedding.values);
    }
    Ok(vectors)
}
