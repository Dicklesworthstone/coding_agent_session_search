//! Schema-v2 partition metadata for immutable semantic publications.
//!
//! Schema v1 remains byte/digest compatible. V2 explicitly orders every tier's
//! nonempty FSVI shards by canonical document ID; ANN entries bind the matching
//! ordinal, never just the tier name. Range/count checks are structural, not a
//! substitute for the selected reader's whole-image and live-docset validation.

use super::*;

pub const SEMANTIC_SHARDED_GENERATION_MANIFEST_SCHEMA_VERSION: u32 = 2;
pub(super) const MAX_SHARDS_PER_TIER: usize = 256;
const MAX_DOCUMENT_ID_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticArtifactShardV2 {
    /// Zero-based ordinal in this tier's complete vector partition.
    pub ordinal: u32,
    /// Total vector shards in this tier, including shards without ANN.
    pub shard_count: u32,
    /// Inclusive bounds in canonical document-ID byte order. The reader checks
    /// the actual live-row extrema against both bounds. FSVI physical rows use
    /// hash-key order; they need not be lexical inside an individual shard.
    pub first_document_id: String,
    pub last_document_id: String,
}

pub(super) fn encode_shard(
    encoder: &mut SemanticCanonicalEncoder,
    shard: Option<&SemanticArtifactShardV2>,
) {
    match shard {
        None => encoder.u8(0),
        Some(shard) => {
            encoder.u8(1);
            encoder.u32(shard.ordinal);
            encoder.u32(shard.shard_count);
            encoder.text(&shard.first_document_id);
            encoder.text(&shard.last_document_id);
        }
    }
}

fn invalid(reason: &str) -> Result<(), SemanticGenerationError> {
    invalid_manifest(SemanticManifestInvariantClass::Topology, reason)
}

pub(super) fn validate(
    manifest: &SemanticGenerationManifestV1,
) -> Result<(), SemanticGenerationError> {
    if manifest.schema_version == SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION {
        if manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.shard.is_some())
        {
            return invalid("schema v1 cannot carry shard metadata; publish schema v2 explicitly");
        }
        return Ok(());
    }
    for artifact in &manifest.artifacts {
        let Some(shard) = artifact.shard.as_ref() else {
            return invalid("every schema-v2 artifact requires explicit shard metadata");
        };
        if shard.shard_count == 0
            || shard.shard_count as usize > MAX_SHARDS_PER_TIER
            || shard.ordinal >= shard.shard_count
            || shard.first_document_id.is_empty()
            || shard.first_document_id.len() > MAX_DOCUMENT_ID_BYTES
            || shard.last_document_id.len() > MAX_DOCUMENT_ID_BYTES
            || shard.first_document_id > shard.last_document_id
            || ((shard.first_document_id == shard.last_document_id)
                != (artifact.covered_document_count == 1))
        {
            return invalid("invalid semantic shard ordinal, extent, or document range");
        }
    }
    for role in [
        SemanticArtifactRole::FastVector,
        SemanticArtifactRole::QualityVector,
    ] {
        let artifacts: Vec<_> = manifest.artifacts_for(role).collect();
        let Some(first) = artifacts.first() else {
            continue;
        };
        let mut covered = 0_u64;
        let mut previous_last: Option<&str> = None;
        for (ordinal, artifact) in artifacts.iter().enumerate() {
            let shard = artifact.shard.as_ref().expect("metadata checked above");
            if shard.ordinal as usize != ordinal || shard.shard_count as usize != artifacts.len() {
                return invalid("vector shard ordinals must form one complete ordered partition");
            }
            if previous_last.is_some_and(|last| last >= shard.first_document_id.as_str()) {
                return invalid("semantic shard document ranges overlap or are reordered");
            }
            if artifact.embedding_identity != first.embedding_identity {
                return invalid(
                    "all vector shards in a tier must bind one complete embedding identity",
                );
            }
            previous_last = Some(&shard.last_document_id);
            covered = covered
                .checked_add(artifact.covered_document_count)
                .ok_or_else(|| SemanticGenerationError::InvalidManifest {
                    class: SemanticManifestInvariantClass::Count,
                    reason: "semantic shard coverage sum overflow".to_owned(),
                })?;
        }
        if covered > manifest.selected_document_count {
            return invalid("aggregate shard coverage exceeds the selected canonical corpus");
        }
    }
    Ok(())
}
