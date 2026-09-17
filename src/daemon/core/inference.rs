//! Bounded foreground inference, shared by ordinary and attested requests.
//! No partial result is returned or signed. Deadlines and shutdown are checked
//! between model calls; an already-running loader/kernel is not forcibly killed.

mod budget;

use frankensearch::{DaemonChallengeV1, DaemonOperationV1};

use super::{DaemonAttestationState, ModelDaemon};
use crate::daemon::protocol::{EmbedResponse, ErrorCode, ErrorResponse, Request, Response, RerankResponse};
use crate::search::fastembed_embedder::FastEmbedder;
use crate::search::fastembed_reranker::FastEmbedReranker;

pub(super) use budget::InferenceGate;
use budget::{Budget, collect_batches, error, plan};

pub(super) fn handle(daemon: &ModelDaemon, request: Request) -> Response {
    let budget = Budget::new(daemon.config.request_timeout, &daemon.shutdown);
    let result = (|| {
        budget.check()?;
        let _permit = daemon.inference_gate.try_enter()?;
        match request {
            Request::Embed { texts, model, dims } => {
                embedding(daemon, &texts, &model, dims, None, &budget)
            }
            Request::EmbedAttested { texts, model, dims, challenge } => {
                embedding(daemon, &texts, &model, dims, Some(&challenge), &budget)
            }
            Request::Rerank { query, documents, model } => {
                reranking(daemon, &query, &documents, &model, None, &budget)
            }
            Request::RerankAttested { query, documents, model, challenge } => {
                reranking(daemon, &query, &documents, &model, Some(&challenge), &budget)
            }
            _ => Err(error(ErrorCode::Internal, "non-inference request reached inference dispatch", false)),
        }
    })();
    result.unwrap_or_else(Response::Error)
}

fn validate_embedding_request(model: &str, dims: Option<usize>) -> Result<(), ErrorResponse> {
    if model.len() > 256 {
        return Err(error(ErrorCode::InvalidInput, "embedding model selector exceeds 256 bytes", false));
    }
    let profile = if model.trim().eq_ignore_ascii_case("default") { "minilm" } else { model };
    let config = FastEmbedder::config_for(profile).ok_or_else(|| {
        error(ErrorCode::ModelNotFound, "requested embedding model is not a supported native profile", false)
    })?;
    if dims.is_some_and(|dimension| dimension != config.dimension) {
        return Err(error(ErrorCode::InvalidInput, "requested dimensions do not match the native embedding profile", false));
    }
    Ok(())
}

fn validate_served_embedding(
    requested: &str,
    served: &str,
    dimension: usize,
) -> Result<(), ErrorResponse> {
    let actual = FastEmbedder::config_for(served).ok_or_else(|| {
        error(ErrorCode::ModelNotFound, "daemon has no supported native embedding model", false)
    })?;
    if actual.embedder_id != served || actual.dimension != dimension {
        return Err(error(ErrorCode::Internal, "loaded embedder does not match its registered identity and dimensions", false));
    }
    if !requested.trim().eq_ignore_ascii_case("default") {
        let expected = FastEmbedder::config_for(requested).ok_or_else(|| {
            error(ErrorCode::ModelNotFound, "requested embedding model is not supported", false)
        })?;
        if expected.embedder_id != served {
            return Err(error(ErrorCode::ModelNotFound, "requested embedding model is not the model served by this daemon", false));
        }
    }
    Ok(())
}

fn validate_reranker(model: &str) -> Result<(), ErrorResponse> {
    if model.len() > 256 {
        return Err(error(ErrorCode::InvalidInput, "reranker model selector exceeds 256 bytes", false));
    }
    let model = model.trim();
    if model.eq_ignore_ascii_case("default")
        || model.eq_ignore_ascii_case(FastEmbedReranker::reranker_id_static())
        || model.eq_ignore_ascii_case("ms-marco-MiniLM-L-6-v2")
    {
        Ok(())
    } else {
        Err(error(ErrorCode::ModelNotFound, "requested reranker is not served by this daemon", false))
    }
}

/// EmbedBatch with one input remains EmbedBatch. Inferring the operation from
/// text count rewrites a valid singleton-batch challenge and breaks its digest.
fn embedding_operation(challenge: &DaemonChallengeV1, count: usize) -> Result<DaemonOperationV1, ErrorResponse> {
    match challenge.operation {
        DaemonOperationV1::Embed if count == 1 => Ok(DaemonOperationV1::Embed),
        DaemonOperationV1::EmbedBatch if count > 0 => Ok(DaemonOperationV1::EmbedBatch),
        _ => Err(error(ErrorCode::InvalidInput, "attested embedding operation does not match the request shape", false)),
    }
}

fn verify_challenge(
    state: Option<&DaemonAttestationState>,
    challenge: &DaemonChallengeV1,
    operation: DaemonOperationV1,
    inputs: &[&str],
) -> Result<(), ErrorResponse> {
    let state = state.ok_or_else(|| {
        error(ErrorCode::ModelLoadFailed, "producer-attested daemon channel is unavailable", false)
    })?;
    state.validate_challenge_for_inputs(challenge, operation, inputs).map_err(|_| {
        error(ErrorCode::InvalidInput, "daemon attestation request failed verification", false)
    })
}

fn verify_live_identity(daemon: &ModelDaemon, state: &DaemonAttestationState) -> Result<(), ErrorResponse> {
    let (identity, category) = daemon.models.embedder_attestation_identity().map_err(|_| {
        error(ErrorCode::ModelLoadFailed, "loaded embedding producer cannot be verified", false)
    })?;
    if identity != state.connection.embedding_identity || category != state.connection.model_category {
        return Err(error(ErrorCode::ModelLoadFailed, "loaded producer no longer matches this daemon's attested connection", false));
    }
    Ok(())
}

fn validate_vectors(vectors: &[Vec<f32>], dimension: usize) -> Result<(), ErrorResponse> {
    if vectors.iter().any(|vector| vector.len() != dimension || vector.iter().any(|value| !value.is_finite())) {
        return Err(error(ErrorCode::Internal, "native embedder returned invalid dimensions or non-finite values", false));
    }
    Ok(())
}

fn embedding(
    daemon: &ModelDaemon,
    texts: &[String],
    model: &str,
    dims: Option<usize>,
    challenge: Option<&DaemonChallengeV1>,
    budget: &Budget<'_>,
) -> Result<Response, ErrorResponse> {
    validate_embedding_request(model, dims)?;
    let ranges = plan(texts, None, budget)?;
    let inputs = texts.iter().map(String::as_str).collect::<Vec<_>>();
    let state = daemon.attestation.read();
    let operation = challenge.map(|challenge| embedding_operation(challenge, texts.len())).transpose()?;
    if let (Some(challenge), Some(operation)) = (challenge, operation) {
        verify_challenge(state.as_ref(), challenge, operation, &inputs)?;
    }
    budget.check()?;
    daemon.models.warm_embedder().map_err(|source| {
        tracing::warn!(error = %source, "Daemon embedder load failed");
        error(ErrorCode::ModelLoadFailed, "native embedding model could not be loaded; check installed model assets", true)
    })?;
    budget.check()?;
    let served = daemon.models.embedder_id();
    let dimension = daemon.models.embedder_dimension();
    validate_served_embedding(model, &served, dimension)?;
    if challenge.is_some() {
        verify_live_identity(daemon, state.as_ref().ok_or_else(|| error(ErrorCode::ModelLoadFailed, "attestation unavailable", false))?)?;
    }
    let vectors = collect_batches(texts, &ranges, budget, |batch| {
        let vectors = daemon.models.embed_batch(batch).map_err(|source| {
            tracing::warn!(error = %source, "Daemon embedding inference failed");
            error(ErrorCode::Internal, "native embedding inference failed", false)
        })?;
        validate_vectors(&vectors, dimension)?;
        Ok(vectors)
    })?;
    budget.check()?;
    if let (Some(challenge), Some(operation)) = (challenge, operation) {
        let state = state.as_ref().ok_or_else(|| error(ErrorCode::ModelLoadFailed, "attestation unavailable", false))?;
        verify_live_identity(daemon, state)?;
        budget.check()?;
        let response = state.sign_vectors(challenge, operation, &inputs, vectors).map_err(|_| {
            error(ErrorCode::Internal, "embedding response failed attestation validation", false)
        })?;
        budget.check()?;
        Ok(Response::AttestedEmbedding(response))
    } else {
        Ok(Response::Embed(EmbedResponse { embeddings: vectors, model: served, elapsed_ms: budget.elapsed_ms() }))
    }
}

fn reranking(
    daemon: &ModelDaemon,
    query: &str,
    documents: &[String],
    model: &str,
    challenge: Option<&DaemonChallengeV1>,
    budget: &Budget<'_>,
) -> Result<Response, ErrorResponse> {
    validate_reranker(model)?;
    let ranges = plan(documents, Some(query), budget)?;
    let mut inputs = Vec::with_capacity(documents.len() + 1);
    inputs.push(query);
    inputs.extend(documents.iter().map(String::as_str));
    let state = daemon.attestation.read();
    if let Some(challenge) = challenge {
        verify_challenge(state.as_ref(), challenge, DaemonOperationV1::Rerank, &inputs)?;
    }
    budget.check()?;
    daemon.models.warm_reranker().map_err(|source| {
        tracing::warn!(error = %source, "Daemon reranker load failed");
        error(ErrorCode::ModelLoadFailed, "native reranker could not be loaded; check installed model assets", true)
    })?;
    budget.check()?;
    let served = daemon.models.reranker_id();
    if served != FastEmbedReranker::reranker_id_static() {
        return Err(error(ErrorCode::ModelNotFound, "loaded reranker does not match the requested native profile", false));
    }
    let scores = collect_batches(documents, &ranges, budget, |batch| {
        let scores = daemon.models.rerank(query, batch).map_err(|source| {
            tracing::warn!(error = %source, "Daemon rerank inference failed");
            error(ErrorCode::Internal, "native reranking inference failed", false)
        })?;
        if scores.iter().any(|score| !score.is_finite()) {
            return Err(error(ErrorCode::Internal, "native reranker returned non-finite scores", false));
        }
        Ok(scores)
    })?;
    budget.check()?;
    if let Some(challenge) = challenge {
        let state = state.as_ref().ok_or_else(|| error(ErrorCode::ModelLoadFailed, "attestation unavailable", false))?;
        let response = state.sign_vectors(challenge, DaemonOperationV1::Rerank, &inputs, vec![scores]).map_err(|_| {
            error(ErrorCode::Internal, "rerank response failed attestation validation", false)
        })?;
        budget.check()?;
        Ok(Response::AttestedEmbedding(response))
    } else {
        Ok(Response::Rerank(RerankResponse { scores, model: served, elapsed_ms: budget.elapsed_ms() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::core::DaemonConfig;
    use crate::daemon::models::ModelManager;

    #[test]
    fn native_model_aliases_and_dimensions_are_enforced() {
        for model in ["default", "minilm", "all-minilm-l6-v2", "minilm-384", "multilingual", "multilingual-minilm-384"] {
            assert!(validate_embedding_request(model, None).is_ok());
            assert!(validate_embedding_request(model, Some(384)).is_ok());
            for dims in [0, 1, 383, 385, usize::MAX] {
                assert_eq!(validate_embedding_request(model, Some(dims)).unwrap_err().code, ErrorCode::InvalidInput);
            }
        }
        for model in ["", "hash", "nomic-embed", "unknown"] {
            assert_eq!(validate_embedding_request(model, None).unwrap_err().code, ErrorCode::ModelNotFound);
        }
        assert!(validate_served_embedding("multilingual", "multilingual-minilm-384", 384).is_ok());
        assert_eq!(validate_served_embedding("multilingual", "minilm-384", 384).unwrap_err().code, ErrorCode::ModelNotFound);
        assert_eq!(validate_served_embedding("minilm", "multilingual-minilm-384", 384).unwrap_err().code, ErrorCode::ModelNotFound);
        assert_eq!(validate_served_embedding("default", "minilm-384", 768).unwrap_err().code, ErrorCode::Internal);
        assert!(validate_served_embedding("default", "fnv1a-384", 384).is_err());
    }

    #[test]
    fn vector_validation_rejects_wrong_shapes_and_all_nonfinite_values() {
        assert!(validate_vectors(&[vec![0.0; 384]], 384).is_ok());
        for bad in [vec![], vec![0.0; 383], vec![0.0; 385], vec![f32::NAN; 384], vec![f32::INFINITY; 384], vec![f32::NEG_INFINITY; 384]] {
            assert_eq!(validate_vectors(&[bad], 384).unwrap_err().code, ErrorCode::Internal);
        }
    }

    #[test]
    fn singleton_batch_keeps_its_challenge_operation_and_signature() -> anyhow::Result<()> {
        use frankensearch::{Embedder as _, HashAlgorithm, HashEmbedder, ModelCategory, DaemonConnectionIdentityV1, DAEMON_CONNECTION_IDENTITY_SCHEMA_V1};
        use crate::daemon::{initialize_daemon_attestation_authority, DAEMON_ATTESTATION_PROTOCOL_REVISION};
        let temp = tempfile::tempdir()?;
        let (authority, generation) = initialize_daemon_attestation_authority(temp.path())?;
        let embedder = HashEmbedder::new(3, HashAlgorithm::FnvModular);
        let state = DaemonAttestationState {
            connection: DaemonConnectionIdentityV1 {
                schema_version: DAEMON_CONNECTION_IDENTITY_SCHEMA_V1,
                endpoint_fingerprint: "11".repeat(32), executable_fingerprint: "22".repeat(32),
                protocol_revision: DAEMON_ATTESTATION_PROTOCOL_REVISION.into(), key_id: authority.key_id().into(), generation,
                embedding_identity: embedder.identity()?.clone(), model_category: ModelCategory::HashEmbedder,
            },
            authority,
        };
        for operation in [DaemonOperationV1::Embed, DaemonOperationV1::EmbedBatch] {
            let challenge = DaemonChallengeV1::for_inputs("aa".repeat(32), operation, &["one"], &state.connection)?;
            let selected = embedding_operation(&challenge, 1).unwrap();
            assert_eq!(selected, operation);
            verify_challenge(Some(&state), &challenge, selected, &["one"]).unwrap();
            let signed = state.sign_vectors(&challenge, selected, &["one"], vec![vec![0.25, 0.5, 0.75]])?;
            signed.attestation.validate_against(&challenge, &state.connection, &signed.vectors)?;
            signed.attestation.authenticate_hmac_sha256(state.authority.secret())?;
            assert!(verify_challenge(Some(&state), &challenge, selected, &["changed"]).is_err());
            assert!(embedding_operation(&challenge, 0).is_err());
        }
        let health = DaemonChallengeV1::for_inputs("bb".repeat(32), DaemonOperationV1::Health, &[], &state.connection)?;
        assert!(embedding_operation(&health, 1).is_err());
        Ok(())
    }

    #[test]
    fn live_dispatch_rejects_invalid_requests_without_loading_models() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let daemon = ModelDaemon::new(DaemonConfig::default(), ModelManager::new(temp.path()));
        let requests = [
            Request::Embed { texts: vec!["hello".into()], model: "default".into(), dims: Some(768) },
            Request::Embed { texts: vec![String::new()], model: "default".into(), dims: None },
            Request::Embed { texts: vec!["x".into(); budget::MAX_ITEMS + 1], model: "default".into(), dims: None },
            Request::Rerank { query: "q".into(), documents: vec!["x".repeat(budget::MAX_TEXT_BYTES + 1)], model: "default".into() },
        ];
        for request in requests {
            assert!(matches!(daemon.handle_request("invalid".into(), request), Response::Error(error) if error.code == ErrorCode::InvalidInput && !error.retryable));
            assert!(!daemon.models.embedder_loaded());
            assert!(!daemon.models.reranker_loaded());
        }
        assert_eq!(std::fs::read_dir(temp.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn busy_foreground_inference_does_not_block_health_or_shutdown() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let daemon = ModelDaemon::new(DaemonConfig::default(), ModelManager::new(temp.path()));
        let permit = daemon.inference_gate.try_enter().unwrap();
        let request = Request::Embed { texts: vec!["hello".into()], model: "default".into(), dims: None };
        assert!(matches!(daemon.handle_request("busy".into(), request), Response::Error(error) if error.code == ErrorCode::Overloaded && error.retryable));
        assert!(matches!(daemon.handle_request("health".into(), Request::Health), Response::Health(_)));
        assert!(matches!(daemon.handle_request("shutdown".into(), Request::Shutdown), Response::Shutdown { .. }));
        drop(permit);
        assert!(!daemon.models.embedder_loaded());
        Ok(())
    }
}
