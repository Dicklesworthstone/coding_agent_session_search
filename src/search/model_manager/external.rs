//! Explicit external query selection. Initial admission precedes HTTP; archive
//! and publication gates are checked again after public-input preflight. No
//! user query is disclosed until both read-only admission phases succeed.
use super::*;
use crate::search::external_embedder::{CancelCheck, ExternalEmbedder, ExternalEmbeddingConfig};
use crate::search::policy::{CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION};
use crate::search::semantic_manifest::{ArtifactRecord, SemanticManifest};

fn failed(context: impl Into<String>) -> SemanticAvailability {
    SemanticAvailability::LoadFailed {
        context: context.into(),
    }
}

fn config_from_env() -> Result<ExternalEmbeddingConfig, SemanticAvailability> {
    ExternalEmbeddingConfig::from_env()
        .map_err(|error| failed(error.to_string()))?
        .ok_or_else(|| SemanticAvailability::Disabled {
            reason: "external_disabled: external queries require CASS_EXTERNAL_EMBEDDINGS=1; no text was sent".into(),
        })
}

/// The mutable ledger is only an additional refusal gate. It never replaces
/// the retained FSVI owner and its exact id/revision/dimension validation below.
fn recorded_artifact(
    data_dir: &Path,
    config: &ExternalEmbeddingConfig,
) -> Result<ArtifactRecord, SemanticAvailability> {
    let id = config.identity();
    let stale = |reason: &str| SemanticAvailability::IndexStale {
        embedder_id: id.clone(),
        reason: reason.to_owned(),
    };
    let manifest = SemanticManifest::load(data_dir)
        .map_err(|error| failed(format!("external semantic manifest: {error}")))?
        .ok_or_else(|| {
            stale("external vectors have no completed publication; run external backfill")
        })?;
    if manifest.checkpoint.as_ref().is_some_and(|checkpoint| {
        checkpoint.embedder_id == id && checkpoint.tier == TierKind::Quality
    }) {
        return Err(stale(
            "external backfill is incomplete; resume its durable checkpoint",
        ));
    }
    let artifact = manifest
        .quality_tier
        .ok_or_else(|| stale("no completed external quality artifact; run external backfill"))?;
    // The CLI records the retained endpoint identity as its model revision.
    // The FSVI header revision describes only the transport/input contract and
    // cannot stand in for the model/endpoint/dimension/revision identity.
    let expected_path = vector_index_path(data_dir, &id);
    if artifact.tier != TierKind::Quality
        || artifact.embedder_id != id
        || artifact.model_revision != id
        || artifact.dimension != config.dimension()
        || !artifact.ready
        || artifact.schema_version != SEMANTIC_SCHEMA_VERSION
        || artifact.chunking_version != CHUNKING_STRATEGY_VERSION
        || !semantic_shard_artifact_path_is_safe(&artifact.index_path)
        || data_dir.join(&artifact.index_path) != expected_path
    {
        return Err(stale(
            "external artifact identity or format changed; rebuild this external embedding space",
        ));
    }
    Ok(artifact)
}

fn open_recorded(
    data_dir: &Path,
    config: &ExternalEmbeddingConfig,
    record: &ArtifactRecord,
) -> Result<SemanticIndexArtifact, SemanticAvailability> {
    let path = vector_index_path(data_dir, &config.identity());
    if !semantic_artifact_candidate_exists(&path).map_err(failed)? {
        return Err(SemanticAvailability::IndexMissing { index_path: path });
    }
    // ANN qualification is independent of external-provider admission. Keep
    // this first external path on the existing exact retained-reader surface.
    let artifact =
        open_validated_semantic_artifact(&path, None, &config.identity(), config.dimension())
            .map_err(failed)?;
    let index = artifact.index();
    if index.wal_record_count() != 0
        || index.tombstone_count() != 0
        || u64::try_from(index.record_count()).ok() != Some(record.doc_count)
    {
        return Err(failed(
            "external artifact differs from its completed publication; resume/rebuild external backfill",
        ));
    }
    Ok(artifact)
}

fn require_current_archive(
    storage: &FrankenStorage,
    record: &ArtifactRecord,
) -> Result<(), SemanticAvailability> {
    let invalidated = storage
        .semantic_identity_rebuild_required(SemanticIdentityTier::Quality)
        .map_err(|error| failed(format!("external canonical identity check: {error}")))?;
    // Unlike the legacy manifest-less path, a dynamic endpoint requires
    // an authoritative fingerprint: inability to read it is a refusal.
    let fingerprint = crate::indexer::lexical_storage_fingerprint_for_storage(storage)
        .map_err(|error| failed(format!("external canonical fingerprint: {error}")))?;
    if invalidated || record.db_fingerprint != fingerprint {
        return Err(SemanticAvailability::IndexStale {
            embedder_id: record.embedder_id.clone(),
            reason: "canonical archive changed; resume/rebuild this external embedding space"
                .into(),
        });
    }
    Ok(())
}

/// HTTP preflight may outlive an archive update or the start of another
/// backfill. It is not a lease on the state admitted before the request.
/// Recheck with a fresh read-only connection, not a potentially retained DB
/// snapshot, and never replace the already-admitted FSVI reader on success.
/// This is admission revalidation, not a transaction or a post-load watcher.
fn revalidate_after_preflight(
    data_dir: &Path,
    db_path: &Path,
    config: &ExternalEmbeddingConfig,
    record: &ArtifactRecord,
) -> Result<(), SemanticAvailability> {
    let storage = FrankenStorage::open_strict_readonly(db_path).map_err(|error| {
        SemanticAvailability::DatabaseUnavailable {
            db_path: db_path.to_path_buf(),
            error: error.to_string(),
        }
    })?;
    require_current_archive(&storage, record)?;
    // An unrelated fast-tier checkpoint/backlog update must not invalidate
    // quality. Compare the exact admitted quality record, not ledger bytes.
    if recorded_artifact(data_dir, config)? != *record {
        return Err(SemanticAvailability::IndexStale {
            embedder_id: record.embedder_id.clone(),
            reason: "external_admission_changed: completed publication changed during preflight; retry against the current external index".into(),
        });
    }
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return Err(availability);
    }
    Ok(())
}

pub(super) fn probe(data_dir: &Path) -> SemanticAvailability {
    if let Some(availability) = selected_generation_owner_requirement(data_dir) {
        return availability;
    }
    let result = (|| {
        let config = config_from_env()?;
        let record = recorded_artifact(data_dir, &config)?;
        open_recorded(data_dir, &config, &record)?;
        Ok::<_, SemanticAvailability>(SemanticAvailability::Ready {
            embedder_id: config.identity(),
        })
    })();
    result.unwrap_or_else(|availability| availability)
}

pub(super) fn load(data_dir: &Path, db_path: &Path, strict_read_only: bool) -> SemanticSetup {
    match config_from_env() {
        Ok(config) => load_with_config(
            data_dir,
            db_path,
            strict_read_only,
            config,
            Arc::new(|| false),
        ),
        Err(availability) => SemanticSetup {
            availability,
            context: None,
        },
    }
}

pub(super) fn load_with_config(
    data_dir: &Path,
    db_path: &Path,
    strict_read_only: bool,
    config: ExternalEmbeddingConfig,
    cancelled: CancelCheck,
) -> SemanticSetup {
    let result = (|| {
        if let Some(availability) = selected_generation_owner_requirement(data_dir) {
            return Err(availability);
        }
        let record = recorded_artifact(data_dir, &config)?;
        let artifact = open_recorded(data_dir, &config, &record)?;
        let storage = if strict_read_only {
            FrankenStorage::open_strict_readonly(db_path)
        } else {
            FrankenStorage::open_readonly(db_path)
        }
        .map_err(|error| SemanticAvailability::DatabaseUnavailable {
            db_path: db_path.to_path_buf(),
            error: error.to_string(),
        })?;
        require_current_archive(&storage, &record)?;
        let filter_maps = SemanticFilterMaps::from_storage(&storage)
            .map_err(|error| failed(format!("external filter maps: {error}")))?;
        drop(storage);
        // No user query is passed to this constructor. Its only traffic is the
        // fixed public preflight. A failure never loads local MiniLM or hash.
        let embedder = ExternalEmbedder::connect(config.clone(), Arc::clone(&cancelled))
            .map_err(|error| failed(format!("external query preflight: {error}")))?;
        if cancelled() {
            return Err(failed("external_cancelled: query admission was cancelled"));
        }
        revalidate_after_preflight(data_dir, db_path, &config, &record)?;
        if cancelled() {
            return Err(failed("external_cancelled: query admission was cancelled"));
        }
        let id = embedder.id().to_owned();
        Ok::<_, SemanticAvailability>((
            id,
            SemanticContext {
                embedder: Arc::new(embedder),
                artifacts: vec![artifact],
                quality_artifact: None,
                filter_maps,
                roles: Some(HashSet::from([ROLE_USER, ROLE_ASSISTANT])),
            },
        ))
    })();
    match result {
        Ok((embedder_id, context)) => SemanticSetup {
            availability: SemanticAvailability::Ready { embedder_id },
            context: Some(context),
        },
        Err(availability) => SemanticSetup {
            availability,
            context: None,
        },
    }
}

#[cfg(test)]
mod tests;
