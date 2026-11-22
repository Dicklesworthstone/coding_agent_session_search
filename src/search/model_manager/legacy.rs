//! Immutable canonical identity admission for legacy semantic owners. These
//! artifacts have no candidate-level coverage proof, so any later destructive
//! identity mutation invalidates the whole retained generation.

use super::*;
use crate::storage::sqlite::SemanticArchiveState;

pub(super) fn capture(storage: &FrankenStorage) -> Result<SemanticArchiveState, String> {
    storage
        .semantic_archive_state()
        .map_err(|error| {
            format!("legacy semantic authority unavailable; rebuild semantic artifacts: {error:#}")
        })?
        .ok_or_else(|| {
            "legacy semantic authority unavailable; rebuild semantic artifacts".to_string()
        })
}

pub(super) fn validate(
    storage: &FrankenStorage,
    authority: &SemanticArchiveState,
) -> Result<(), String> {
    let invalidated = storage
        .semantic_invalidations_after(authority)
        .map_err(|error| format!("legacy semantic authority validation failed: {error:#}"))?;
    if invalidated {
        return Err("legacy semantic authority changed; rebuild semantic artifacts".to_string());
    }
    Ok(())
}

pub(super) fn seal(
    storage: &FrankenStorage,
    authority: &SemanticArchiveState,
    artifacts: Vec<SemanticIndexArtifact>,
) -> Result<Vec<SemanticIndexArtifact>, String> {
    validate(storage, authority)?;
    artifacts
        .into_iter()
        .map(|artifact| {
            if artifact.coverage_certificate().is_some() {
                Ok(artifact)
            } else {
                artifact
                    .with_legacy_authority(authority)
                    .map_err(|error| format!("legacy semantic authority binding failed: {error:#}"))
            }
        })
        .collect()
}
