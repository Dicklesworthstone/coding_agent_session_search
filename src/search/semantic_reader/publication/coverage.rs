//! Cross-tier membership over retained, already-admitted v2 images.
//!
//! A complete tier authenticates the selected corpus's ordered live-docset.
//! Any partial companion must be its subset, including content hashes inside
//! canonical passage IDs. FSVI-order merge walks retain only a head per shard,
//! without another corpus-sized ID set or file opens. Two partial tiers still
//! rely on publication commitments; neither becomes a full-corpus witness.

use super::live_docset::LiveDocuments;
use super::{AdmittedTier, SemanticArtifactRole, SemanticSelectionError, SemanticSelectionResult};

pub(super) fn validate(
    fast: &AdmittedTier,
    quality: &AdmittedTier,
    selected_count: u64,
) -> SemanticSelectionResult<()> {
    if fast.live_count == selected_count && quality.live_count < selected_count {
        validate_subset(fast, quality, SemanticArtifactRole::QualityVector)?;
    } else if quality.live_count == selected_count && fast.live_count < selected_count {
        validate_subset(quality, fast, SemanticArtifactRole::FastVector)?;
    }
    Ok(())
}

fn validate_subset(
    complete: &AdmittedTier,
    partial: &AdmittedTier,
    role: SemanticArtifactRole,
) -> SemanticSelectionResult<()> {
    let mut complete_ids = LiveDocuments::new(&complete.shards)?;
    let mut partial_ids = LiveDocuments::new(&partial.shards)?;
    let mut reference = complete_ids.next()?;
    while let Some(document) = partial_ids.next()? {
        while reference.is_some_and(|candidate| candidate < document) {
            reference = complete_ids.next()?;
        }
        if reference != Some(document) {
            return Err(SemanticSelectionError::ArtifactMismatch {
                role,
                field: "partial_live_docset",
            });
        }
    }
    Ok(())
}
