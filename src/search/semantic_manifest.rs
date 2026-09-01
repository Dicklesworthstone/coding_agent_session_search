//! Durable semantic asset manifest, backlog ledger, and resumable checkpoints.
//!
//! This module contains two deliberately separate contracts:
//!
//! - the historical mutable backfill ledger used to describe work in progress;
//! - the authoritative immutable generation manifest plus compare-and-swap
//!   `current.json` pointer used for runtime selection.
//!
//! Runtime readiness must come only from a fully validated selected generation.
//! Mutable ledger state never implies that an artifact is searchable.
//!
//! # Storage
//!
//! The historical backfill ledger remains at:
//! ```text
//! {data_dir}/vector_index/semantic_manifest.json
//! ```
//!
//! Authoritative immutable generations live at:
//! ```text
//! {data_dir}/vector_index/generations/{generation_id}/manifest.json
//! {data_dir}/vector_index/current.json
//! ```
//!
//! Each generation is sealed only after artifact durability is established.
//! Publication is a serialized compare-and-swap of `current.json`.
//!
//! # Relationship to other modules
//!
//! - **[`policy`]**: Provides the contract (versions, budgets, tier names) that
//!   this manifest fingerprints against.
//! - **[`asset_state`]**: Evaluates coarse readiness from this manifest plus
//!   live file probes.
//! - **[`model_manager`]**: Detects model availability; this module records
//!   which model was used to build each artifact.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{Read as IoRead, Write as IoWrite};
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use frankensearch::core::{
    EMBEDDING_INPUT_CONTRACT_SCHEMA_V1, EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1,
    EMBEDDING_SPACE_IDENTITY_SCHEMA_V1, EmbeddingSpaceKindV1, FrozenEmbeddingIdentityBundleV1,
    VECTOR_STORAGE_IDENTITY_SCHEMA_V1,
};
use frankensearch::index::wal_path_for;
use fs2::FileExt;

use super::policy::{
    CHUNKING_STRATEGY_VERSION, InvalidationAction, SEMANTIC_SCHEMA_VERSION,
    SemanticAssetManifest as PolicyManifest, SemanticPolicy,
};

// ─── Constants ─────────────────────────────────────────────────────────────

/// Current manifest format version.  Bump when the JSON schema changes in a
/// backwards-incompatible way.
/// Manifest format version.
///
/// History:
/// - **v1** (pre-cass#257): `BuildCheckpoint` resume cursor is the
///   conversation offset only.
/// - **v2** (cass#257 sub-fix 2): `BuildCheckpoint` may additionally
///   carry `last_message_id`, an inclusive canonical message PK
///   advanced by every batch. Resume strictly skips messages ≤ this
///   cursor so an interrupted bounded run never re-embeds work it
///   already staged. Later v2 checkpoints may also carry
///   `cursor_exhausted`, which separates durable completion from
///   count-based progress. Both fields use `#[serde(default)]`; the
///   JSON-on-disk shape only differs when a batch persists the cursor
///   metadata, and older v2 readers ignore the extra field.
///
/// **Compatibility:**
/// - **Old binary reading v2 manifest:** clean `UnsupportedVersion`
///   error from `load()`; operator sees a clear "manifest version
///   $V is newer than max-supported $MAX" message and can upgrade.
/// - **New binary reading v1 manifest:** loads fine. `last_message_id`
///   defaults to `None`; resume falls back to the conversation offset
///   with a one-shot warning that resume granularity is coarser than
///   ideal until the next checkpoint save bumps the on-disk shape.
pub const MANIFEST_FORMAT_VERSION: u32 = 2;

/// Highest manifest-version emitted by pre-cass#257 binaries; loading
/// this is fully supported, but resume granularity will be coarser
/// until a fresh checkpoint is saved. Kept as a named constant so the
/// fallback warning quotes a stable number.
pub const MANIFEST_FORMAT_VERSION_PRE_LAST_MESSAGE_CURSOR: u32 = 1;

/// Filename for the durable manifest.
pub const MANIFEST_FILENAME: &str = "semantic_manifest.json";

/// Filename for the prototype per-shard semantic artifact manifest.
pub const SHARD_MANIFEST_FILENAME: &str = "semantic_shards.json";

pub(crate) fn semantic_shard_artifact_path_is_safe(recorded_path: &str) -> bool {
    let trimmed = recorded_path.trim();
    if trimmed.is_empty() || trimmed != recorded_path {
        return false;
    }
    let path = Path::new(recorded_path);
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

// ─── Tier kind ─────────────────────────────────────────────────────────────

/// Which semantic tier an artifact or checkpoint belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierKind {
    Fast,
    Quality,
}

impl TierKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Quality => "quality",
        }
    }
}

// ─── Tier readiness ────────────────────────────────────────────────────────

/// Readiness of a single semantic tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierReadiness {
    /// Artifact exists, verified, and current with the DB.
    Ready,
    /// Build is in progress (checkpoint present).
    Building { progress_pct: u8 },
    /// Artifact exists but DB or model changed since it was built.
    Stale { reason: String },
    /// No artifact at all for this tier.
    Missing,
    /// Schema or chunking version mismatch — must discard and rebuild.
    Incompatible { reason: String },
}

impl TierReadiness {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Ready | Self::Stale { .. })
    }
}

// ─── Artifact record ───────────────────────────────────────────────────────

/// Durable metadata for a single vector index artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    /// Which tier this artifact belongs to.
    pub tier: TierKind,
    /// Embedder ID that produced these vectors (e.g., "minilm-384", "fnv1a-384").
    pub embedder_id: String,
    /// Model revision hash (HuggingFace commit or "hash" for the hash embedder).
    pub model_revision: String,
    /// Semantic schema version at build time.
    pub schema_version: u32,
    /// Chunking strategy version at build time.
    pub chunking_version: u32,
    /// Output dimension of the embedder.
    pub dimension: usize,
    /// Number of documents (message chunks) embedded.
    pub doc_count: u64,
    /// Number of conversations processed to produce this artifact.
    pub conversation_count: u64,
    /// Storage fingerprint of the canonical DB when this artifact was built.
    pub db_fingerprint: String,
    /// Relative path to the index file (from data_dir).
    pub index_path: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Unix timestamp (ms) when the build started.
    pub started_at_ms: i64,
    /// Unix timestamp (ms) when the build completed.
    pub completed_at_ms: i64,
    /// Whether this artifact has been verified and published.
    pub ready: bool,
}

impl ArtifactRecord {
    /// Convert to the policy-level manifest for invalidation checks.
    pub fn to_policy_manifest(&self) -> PolicyManifest {
        PolicyManifest {
            embedder_id: self.embedder_id.clone(),
            model_revision: self.model_revision.clone(),
            schema_version: self.schema_version,
            chunking_version: self.chunking_version,
            doc_count: self.doc_count,
            built_at_ms: self.completed_at_ms,
        }
    }

    /// Evaluate this artifact's readiness against the current policy and DB
    /// fingerprint.
    ///
    /// **Note**: This checks schema/chunking versions, mode, model revision,
    /// and DB fingerprint.  It does NOT detect embedder changes because the
    /// expected embedder ID requires the embedder registry to resolve.
    /// Callers needing embedder-change detection should call
    /// [`SemanticAssetManifest::invalidation_action`] directly with the
    /// correct `expected_embedder_id` from the registry.
    pub fn readiness(
        &self,
        policy: &SemanticPolicy,
        current_db_fingerprint: &str,
        current_model_revision: &str,
    ) -> TierReadiness {
        let action = self.to_policy_manifest().invalidation_action(
            policy,
            current_model_revision,
            &self.embedder_id,
        );

        match action {
            InvalidationAction::UpToDate => {
                if self.db_fingerprint != current_db_fingerprint {
                    TierReadiness::Stale {
                        reason: "DB content changed since artifact was built".to_owned(),
                    }
                } else if !self.ready {
                    TierReadiness::Building { progress_pct: 100 }
                } else {
                    TierReadiness::Ready
                }
            }
            InvalidationAction::RebuildInBackground => TierReadiness::Stale {
                reason: "model revision changed; vectors usable until rebuild completes".to_owned(),
            },
            InvalidationAction::DiscardAndRebuild { reason } => {
                TierReadiness::Incompatible { reason }
            }
            InvalidationAction::Evict => TierReadiness::Incompatible {
                reason: "semantic mode set to lexical-only".to_owned(),
            },
        }
    }
}

// ─── HNSW accelerator record ──────────────────────────────────────────────

/// Durable metadata for an HNSW accelerator index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HnswRecord {
    /// Which base artifact this accelerates.
    pub base_tier: TierKind,
    /// Embedder ID of the base artifact.
    pub embedder_id: String,
    /// ef_search parameter used at build time.
    pub ef_search: usize,
    /// Relative path to the HNSW index file (from data_dir).
    pub index_path: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Unix timestamp (ms) when built.
    pub built_at_ms: i64,
    /// Whether this index is ready for use.
    pub ready: bool,
}

// ─── Sharded vector artifact sidecar ─────────────────────────────────────

/// Durable metadata for one mmap-friendly semantic vector shard.
///
/// Shards deliberately live in a sidecar manifest instead of
/// [`SemanticManifest`]. Runtime readiness continues to flow through the
/// existing tier artifact records, so partial shard generations cannot make a
/// semantic tier look ready before a promotion step has merged or selected a
/// complete generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticShardRecord {
    /// Which semantic tier this shard belongs to.
    pub tier: TierKind,
    /// Embedder ID that produced the vectors.
    pub embedder_id: String,
    /// Model revision hash or "hash" for the hash embedder.
    pub model_revision: String,
    /// Semantic schema version at build time.
    pub schema_version: u32,
    /// Chunking strategy version at build time.
    pub chunking_version: u32,
    /// Output dimension of the embedder.
    pub dimension: usize,
    /// Zero-based shard number within the generation.
    pub shard_index: u32,
    /// Total shards expected for this generation.
    pub shard_count: u32,
    /// Number of documents embedded in this shard.
    pub doc_count: u64,
    /// Total conversations represented by the full shard generation.
    pub total_conversations: u64,
    /// Storage fingerprint of the canonical DB when this shard was built.
    pub db_fingerprint: String,
    /// Relative path to the shard index file from data_dir.
    pub index_path: String,
    /// Vector quantization used by the shard file.
    pub quantization: String,
    /// Whether readers may mmap this artifact directly.
    pub mmap_ready: bool,
    /// Relative path to the shard-local ANN accelerator, when built.
    pub ann_index_path: Option<String>,
    /// File size of the ANN accelerator in bytes.
    pub ann_size_bytes: u64,
    /// Whether the shard-local ANN accelerator is ready for use.
    pub ann_ready: bool,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Unix timestamp (ms) when the shard build started.
    pub started_at_ms: i64,
    /// Unix timestamp (ms) when the shard build completed.
    pub completed_at_ms: i64,
    /// Whether this shard has been verified and published to the sidecar.
    pub ready: bool,
}

impl SemanticShardRecord {
    pub fn to_policy_manifest(&self) -> PolicyManifest {
        PolicyManifest {
            embedder_id: self.embedder_id.clone(),
            model_revision: self.model_revision.clone(),
            schema_version: self.schema_version,
            chunking_version: self.chunking_version,
            doc_count: self.doc_count,
            built_at_ms: self.completed_at_ms,
        }
    }

    pub fn matches_generation(
        &self,
        tier: TierKind,
        embedder_id: &str,
        db_fingerprint: &str,
    ) -> bool {
        self.tier == tier
            && self.embedder_id == embedder_id
            && self.db_fingerprint == db_fingerprint
    }
}

/// Aggregated readiness for a shard generation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticShardSummary {
    pub shard_count: u32,
    pub ready_shards: u32,
    pub ann_ready_shards: u32,
    pub doc_count: u64,
    pub total_conversations: u64,
    pub size_bytes: u64,
    pub ann_size_bytes: u64,
    pub complete: bool,
}

/// Sidecar manifest for prototype semantic shard generations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticShardManifest {
    pub manifest_version: u32,
    pub shards: Vec<SemanticShardRecord>,
    pub updated_at_ms: i64,
}

impl Default for SemanticShardManifest {
    fn default() -> Self {
        Self {
            manifest_version: MANIFEST_FORMAT_VERSION,
            shards: Vec::new(),
            updated_at_ms: 0,
        }
    }
}

impl SemanticShardManifest {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("vector_index").join(SHARD_MANIFEST_FILENAME)
    }

    pub fn load(data_dir: &Path) -> Result<Option<Self>, ManifestError> {
        let path = Self::path(data_dir);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(ManifestError::Io {
                    path,
                    source: e.to_string(),
                });
            }
        };

        let manifest: Self = serde_json::from_slice(&bytes).map_err(|e| ManifestError::Parse {
            path: path.clone(),
            source: e.to_string(),
        })?;

        if manifest.manifest_version > MANIFEST_FORMAT_VERSION {
            return Err(ManifestError::UnsupportedVersion {
                found: manifest.manifest_version,
                max_supported: MANIFEST_FORMAT_VERSION,
            });
        }

        Ok(Some(manifest))
    }

    pub fn load_or_default(data_dir: &Path) -> Result<Self, ManifestError> {
        match Self::load(data_dir) {
            Ok(Some(manifest)) => Ok(manifest),
            Ok(None) => Ok(Self::default()),
            Err(e @ ManifestError::Io { .. }) => Err(e),
            Err(ManifestError::Parse { .. } | ManifestError::UnsupportedVersion { .. }) => {
                Ok(Self::default())
            }
            Err(e) => Err(e),
        }
    }

    pub fn save(&mut self, data_dir: &Path) -> Result<(), ManifestError> {
        let path = Self::path(data_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| ManifestError::Io {
                path: parent.to_path_buf(),
                source: e.to_string(),
            })?;
        }

        self.updated_at_ms = now_ms();
        let json = serde_json::to_string_pretty(self).map_err(|e| ManifestError::Serialize {
            source: e.to_string(),
        })?;
        let (tmp_path, mut file) =
            create_unique_manifest_temp_file(&path).map_err(|e| ManifestError::Io {
                path: path.clone(),
                source: e.to_string(),
            })?;
        file.write_all(json.as_bytes())
            .map_err(|e| ManifestError::Io {
                path: tmp_path.clone(),
                source: e.to_string(),
            })?;
        file.sync_all().map_err(|e| ManifestError::Io {
            path: tmp_path.clone(),
            source: e.to_string(),
        })?;
        replace_file_from_temp(&tmp_path, &path).map_err(|e| ManifestError::Io {
            path: path.clone(),
            source: e.to_string(),
        })?;
        sync_parent_directory(&path).map_err(|e| ManifestError::Io {
            path: path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.clone()),
            source: e.to_string(),
        })?;

        Ok(())
    }

    pub fn replace_shards_for_generation(
        &mut self,
        tier: TierKind,
        embedder_id: &str,
        db_fingerprint: &str,
        mut shards: Vec<SemanticShardRecord>,
    ) {
        self.shards
            .retain(|shard| !shard.matches_generation(tier, embedder_id, db_fingerprint));
        self.shards.append(&mut shards);
        self.shards.sort_by(|a, b| {
            (
                a.tier.as_str(),
                &a.embedder_id,
                &a.db_fingerprint,
                a.shard_index,
            )
                .cmp(&(
                    b.tier.as_str(),
                    &b.embedder_id,
                    &b.db_fingerprint,
                    b.shard_index,
                ))
        });
    }

    /// Stop every shard generation for one embedder from shadowing a freshly
    /// rebuilt monolithic artifact while retaining its provenance record and
    /// files for explicit inspection/GC.
    pub fn mark_shards_stale_for_embedder(&mut self, embedder_id: &str) -> usize {
        let mut changed = 0usize;
        for shard in self
            .shards
            .iter_mut()
            .filter(|shard| shard.embedder_id == embedder_id)
        {
            if shard.ready || shard.ann_ready {
                shard.ready = false;
                shard.ann_ready = false;
                changed = changed.saturating_add(1);
            }
        }
        changed
    }

    pub fn summary(
        &self,
        tier: TierKind,
        embedder_id: &str,
        db_fingerprint: &str,
    ) -> SemanticShardSummary {
        let mut summary = SemanticShardSummary::default();
        let mut ready_indices = std::collections::BTreeSet::new();
        let mut ann_ready_indices = std::collections::BTreeSet::new();
        let mut seen_indices = std::collections::BTreeSet::new();
        let mut seen_index_paths = std::collections::BTreeSet::new();
        let mut seen_ann_index_paths = std::collections::BTreeSet::new();
        let mut expected_shard_count = None;
        let mut expected_generation_metadata: Option<(&str, u32, u32, usize, u64, &str)> = None;
        let mut generation_consistent = true;
        for shard in self
            .shards
            .iter()
            .filter(|shard| shard.matches_generation(tier, embedder_id, db_fingerprint))
        {
            if shard.shard_count == 0 || shard.shard_index >= shard.shard_count {
                generation_consistent = false;
            }
            if !seen_indices.insert(shard.shard_index) {
                generation_consistent = false;
            }
            if !semantic_shard_artifact_path_is_safe(&shard.index_path)
                || !seen_index_paths.insert(&shard.index_path)
            {
                generation_consistent = false;
            }
            match expected_shard_count {
                Some(expected) if expected != shard.shard_count => {
                    generation_consistent = false;
                }
                None => expected_shard_count = Some(shard.shard_count),
                _ => {}
            }
            let generation_metadata = (
                shard.model_revision.as_str(),
                shard.schema_version,
                shard.chunking_version,
                shard.dimension,
                shard.total_conversations,
                shard.quantization.as_str(),
            );
            match expected_generation_metadata {
                Some(expected) if expected != generation_metadata => {
                    generation_consistent = false;
                }
                None => expected_generation_metadata = Some(generation_metadata),
                _ => {}
            }
            if shard.schema_version != SEMANTIC_SCHEMA_VERSION
                || shard.chunking_version != CHUNKING_STRATEGY_VERSION
                || shard.dimension == 0
            {
                generation_consistent = false;
            }
            summary.shard_count = summary.shard_count.max(shard.shard_count);
            summary.doc_count = summary.doc_count.saturating_add(shard.doc_count);
            summary.total_conversations =
                summary.total_conversations.max(shard.total_conversations);
            summary.size_bytes = summary.size_bytes.saturating_add(shard.size_bytes);
            summary.ann_size_bytes = summary.ann_size_bytes.saturating_add(shard.ann_size_bytes);
            if shard.ready && shard.mmap_ready {
                ready_indices.insert(shard.shard_index);
            }
            if shard.ready
                && shard.mmap_ready
                && shard.ann_ready
                && shard.ann_size_bytes > 0
                && let Some(ann_index_path) = shard.ann_index_path.as_deref()
                && semantic_shard_artifact_path_is_safe(ann_index_path)
                && seen_ann_index_paths.insert(ann_index_path)
            {
                ann_ready_indices.insert(shard.shard_index);
            }
        }
        summary.ready_shards = u32::try_from(ready_indices.len()).unwrap_or(u32::MAX);
        summary.ann_ready_shards = u32::try_from(ann_ready_indices.len()).unwrap_or(u32::MAX);
        summary.complete = generation_consistent
            && summary.shard_count > 0
            && summary.ready_shards == summary.shard_count
            && (0..summary.shard_count).all(|index| ready_indices.contains(&index));
        summary
    }

    pub fn invalidate_incompatible(
        &mut self,
        policy: &SemanticPolicy,
        current_model_revision: &str,
    ) -> usize {
        let before = self.shards.len();
        self.shards.retain(|shard| {
            !matches!(
                shard.to_policy_manifest().invalidation_action(
                    policy,
                    current_model_revision,
                    &shard.embedder_id,
                ),
                InvalidationAction::DiscardAndRebuild { .. } | InvalidationAction::Evict
            )
        });
        before.saturating_sub(self.shards.len())
    }

    pub fn total_size_bytes(&self) -> u64 {
        self.shards
            .iter()
            .map(|shard| shard.size_bytes)
            .fold(0, u64::saturating_add)
    }
}

// ─── Build checkpoint ──────────────────────────────────────────────────────

/// Resumable position for an interrupted semantic build.
///
/// Sub-fix 2 for cass#257 added the optional `last_message_id` cursor.
/// Resume strictly advances past this cursor when present, so that a
/// rerun of an interrupted bounded backfill never re-embeds messages
/// that already made it into the staged index. Pre-#257 binaries wrote
/// checkpoints without the field (it deserializes to `None` via
/// `#[serde(default)]`), and modern binaries fall back to the
/// conversation offset when `last_message_id` is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildCheckpoint {
    /// Which tier is being built.
    pub tier: TierKind,
    /// Embedder ID for this build.
    pub embedder_id: String,
    /// Last conversation offset processed (for pagination).
    pub last_offset: i64,
    /// Total documents embedded so far in this build.
    pub docs_embedded: u64,
    /// Total conversations processed so far.
    pub conversations_processed: u64,
    /// Total conversations expected (from DB at start of build).
    pub total_conversations: u64,
    /// DB fingerprint when this build started.
    pub db_fingerprint: String,
    /// Schema version for this build.
    pub schema_version: u32,
    /// Chunking version for this build.
    pub chunking_version: u32,
    /// Unix timestamp (ms) when this checkpoint was saved.
    pub saved_at_ms: i64,
    /// Highest canonical message PK embedded in this run so far.
    ///
    /// Added in cass#257 (sub-fix 2). `None` for checkpoints written
    /// by pre-#257 binaries; new code falls back to `last_offset`
    /// (conversation-granularity) when this is absent. New code
    /// strictly resumes past this cursor when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_id: Option<i64>,
    /// Whether the canonical selection cursor proved there are no more
    /// eligible conversations after this checkpoint.
    ///
    /// This is distinct from count-based progress: checkpoint caps and
    /// cursor predicates can leave more work even when processed counts
    /// appear to have reached the DB total snapshot.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cursor_exhausted: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl BuildCheckpoint {
    /// Progress as a percentage (0–100).
    pub fn progress_pct(&self) -> u8 {
        if self.total_conversations == 0 {
            return 0;
        }
        let pct = (self.conversations_processed as f64 / self.total_conversations as f64) * 100.0;
        let pct = (pct as u8).min(100);
        if pct == 100 && !self.cursor_exhausted {
            99
        } else {
            pct
        }
    }

    /// Whether the build is complete (all conversations processed).
    pub fn is_complete(&self) -> bool {
        self.cursor_exhausted && self.conversations_processed >= self.total_conversations
    }

    /// Whether this checkpoint is still valid against the current DB and policy.
    pub fn is_valid(&self, current_db_fingerprint: &str) -> bool {
        self.db_fingerprint == current_db_fingerprint
            && self.schema_version == SEMANTIC_SCHEMA_VERSION
            && self.chunking_version == CHUNKING_STRATEGY_VERSION
    }
}

// ─── Backlog ledger ────────────────────────────────────────────────────────

/// Tracks what semantic build work remains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacklogLedger {
    /// Total conversations in the canonical DB at last check.
    pub total_conversations: u64,
    /// Conversations embedded in the fast tier.
    pub fast_tier_processed: u64,
    /// Conversations embedded in the quality tier.
    pub quality_tier_processed: u64,
    /// DB fingerprint when this ledger was computed.
    pub db_fingerprint: String,
    /// Unix timestamp (ms) when this ledger was computed.
    pub computed_at_ms: i64,
}

impl BacklogLedger {
    /// Remaining conversations for the fast tier.
    pub fn fast_tier_remaining(&self) -> u64 {
        self.total_conversations
            .saturating_sub(self.fast_tier_processed)
    }

    /// Remaining conversations for the quality tier.
    pub fn quality_tier_remaining(&self) -> u64 {
        self.total_conversations
            .saturating_sub(self.quality_tier_processed)
    }

    /// Whether either tier has outstanding work.
    pub fn has_pending_work(&self) -> bool {
        self.fast_tier_remaining() > 0 || self.quality_tier_remaining() > 0
    }

    /// Whether the ledger is current with the given DB fingerprint.
    pub fn is_current(&self, current_db_fingerprint: &str) -> bool {
        self.db_fingerprint == current_db_fingerprint
    }
}

// ─── The top-level manifest ────────────────────────────────────────────────

/// Durable, atomic semantic asset manifest.
///
/// This is the single source of truth for what semantic assets exist, their
/// provenance, and what work remains.  It is loaded/saved as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticManifest {
    /// Format version — for future migrations.
    pub manifest_version: u32,
    /// Fast-tier vector artifact (hash embedder).
    pub fast_tier: Option<ArtifactRecord>,
    /// Quality-tier vector artifact (ML embedder).
    pub quality_tier: Option<ArtifactRecord>,
    /// HNSW accelerator index.
    pub hnsw: Option<HnswRecord>,
    /// Backlog / progress tracker.
    pub backlog: BacklogLedger,
    /// Active build checkpoint (for resuming interrupted work).
    pub checkpoint: Option<BuildCheckpoint>,
    /// Unix timestamp (ms) when this manifest was last written.
    pub updated_at_ms: i64,
}

impl Default for SemanticManifest {
    fn default() -> Self {
        Self {
            manifest_version: MANIFEST_FORMAT_VERSION,
            fast_tier: None,
            quality_tier: None,
            hnsw: None,
            backlog: BacklogLedger {
                total_conversations: 0,
                fast_tier_processed: 0,
                quality_tier_processed: 0,
                db_fingerprint: String::new(),
                computed_at_ms: 0,
            },
            checkpoint: None,
            updated_at_ms: 0,
        }
    }
}

impl SemanticManifest {
    // ── Path helpers ───────────────────────────────────────────────────

    /// Path to the manifest file.
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("vector_index").join(MANIFEST_FILENAME)
    }

    // ── Load / Save ───────────────────────────────────────────────────

    /// Load the manifest from disk.  Returns `None` if the file doesn't
    /// exist, `Err` if it exists but is corrupt.
    ///
    /// **Migration notes (cass#257 sub-fix 2):** a v1 manifest written
    /// by a pre-#257 binary loads cleanly — `last_message_id` defaults
    /// to `None` and resume falls back to the conversation offset.
    /// A one-shot warning surfaces on first load so an operator sees
    /// that resume granularity is coarser than ideal until the next
    /// checkpoint save lands and the on-disk shape upgrades.
    pub fn load(data_dir: &Path) -> Result<Option<Self>, ManifestError> {
        let path = Self::path(data_dir);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(ManifestError::Io {
                    path,
                    source: e.to_string(),
                });
            }
        };

        let manifest: Self = serde_json::from_slice(&bytes).map_err(|e| ManifestError::Parse {
            path: path.clone(),
            source: e.to_string(),
        })?;

        // Forward-compatible: reject future manifest versions we can't read.
        if manifest.manifest_version > MANIFEST_FORMAT_VERSION {
            return Err(ManifestError::UnsupportedVersion {
                found: manifest.manifest_version,
                max_supported: MANIFEST_FORMAT_VERSION,
            });
        }

        // Backwards-compatible: a v1 manifest with an active checkpoint
        // means a previous interrupted backfill saved without the
        // `last_message_id` cursor. Warn so an operator monitoring an
        // overnight run knows resume granularity is conversation-coarse
        // until the next checkpoint save bumps the on-disk shape.
        if manifest.manifest_version <= MANIFEST_FORMAT_VERSION_PRE_LAST_MESSAGE_CURSOR
            && manifest
                .checkpoint
                .as_ref()
                .is_some_and(|cp| cp.last_message_id.is_none())
        {
            tracing::warn!(
                manifest_version = manifest.manifest_version,
                supported_version = MANIFEST_FORMAT_VERSION,
                path = %path.display(),
                "semantic checkpoint manifest predates last_message_id cursor (cass#257 sub-fix 2); resume will fall back to conversation offset until the next checkpoint save"
            );
        }

        Ok(Some(manifest))
    }

    /// Load the manifest, returning defaults if absent or corrupt.
    ///
    /// Unlike [`load`], this method treats parse errors and version mismatches
    /// as "manifest absent" — the caller gets a clean default rather than an
    /// error.  This is the right behaviour for runtime code that must always
    /// make progress.
    pub fn load_or_default(data_dir: &Path) -> Result<Self, ManifestError> {
        match Self::load(data_dir) {
            Ok(Some(manifest)) => Ok(manifest),
            Ok(None) => Ok(Self::default()),
            // I/O errors are real failures — propagate the original.
            Err(e @ ManifestError::Io { .. }) => Err(e),
            // Parse or version errors → treat as absent.
            Err(ManifestError::Parse { .. } | ManifestError::UnsupportedVersion { .. }) => {
                Ok(Self::default())
            }
            Err(e) => Err(e),
        }
    }

    /// Atomically save the manifest to disk (write-to-temp then rename).
    pub fn save(&mut self, data_dir: &Path) -> Result<(), ManifestError> {
        let path = Self::path(data_dir);

        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| ManifestError::Io {
                path: parent.to_path_buf(),
                source: e.to_string(),
            })?;
        }

        self.updated_at_ms = now_ms();

        let json = serde_json::to_string_pretty(self).map_err(|e| ManifestError::Serialize {
            source: e.to_string(),
        })?;

        // Atomic write: temp file → rename.
        let (tmp_path, mut file) =
            create_unique_manifest_temp_file(&path).map_err(|e| ManifestError::Io {
                path: path.clone(),
                source: e.to_string(),
            })?;
        file.write_all(json.as_bytes())
            .map_err(|e| ManifestError::Io {
                path: tmp_path.clone(),
                source: e.to_string(),
            })?;
        file.sync_all().map_err(|e| ManifestError::Io {
            path: tmp_path.clone(),
            source: e.to_string(),
        })?;
        replace_file_from_temp(&tmp_path, &path).map_err(|e| ManifestError::Io {
            path: path.clone(),
            source: e.to_string(),
        })?;
        sync_parent_directory(&path).map_err(|e| ManifestError::Io {
            path: path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.clone()),
            source: e.to_string(),
        })?;

        Ok(())
    }

    // ── Readiness evaluation ──────────────────────────────────────────

    /// Evaluate readiness of the fast tier.
    pub fn fast_tier_readiness(
        &self,
        policy: &SemanticPolicy,
        current_db_fingerprint: &str,
        current_model_revision: &str,
    ) -> TierReadiness {
        match &self.fast_tier {
            Some(artifact) => {
                artifact.readiness(policy, current_db_fingerprint, current_model_revision)
            }
            None => {
                // Check for an active build checkpoint for this tier.
                if let Some(cp) = &self.checkpoint
                    && cp.tier == TierKind::Fast
                    && cp.is_valid(current_db_fingerprint)
                {
                    TierReadiness::Building {
                        progress_pct: cp.progress_pct(),
                    }
                } else {
                    TierReadiness::Missing
                }
            }
        }
    }

    /// Evaluate readiness of the quality tier.
    pub fn quality_tier_readiness(
        &self,
        policy: &SemanticPolicy,
        current_db_fingerprint: &str,
        current_model_revision: &str,
    ) -> TierReadiness {
        match &self.quality_tier {
            Some(artifact) => {
                artifact.readiness(policy, current_db_fingerprint, current_model_revision)
            }
            None => {
                if let Some(cp) = &self.checkpoint
                    && cp.tier == TierKind::Quality
                    && cp.is_valid(current_db_fingerprint)
                {
                    TierReadiness::Building {
                        progress_pct: cp.progress_pct(),
                    }
                } else {
                    TierReadiness::Missing
                }
            }
        }
    }

    /// Whether hybrid refinement can run right now (fast tier usable).
    pub fn can_hybrid_search(
        &self,
        policy: &SemanticPolicy,
        current_db_fingerprint: &str,
        current_model_revision: &str,
    ) -> bool {
        self.fast_tier_readiness(policy, current_db_fingerprint, current_model_revision)
            .is_usable()
    }

    // ── Backlog / checkpoint management ───────────────────────────────

    /// Update the backlog from the canonical DB state.
    pub fn refresh_backlog(&mut self, total_conversations: u64, current_db_fingerprint: &str) {
        let fast_processed = self
            .fast_tier
            .as_ref()
            .filter(|a| a.ready && a.db_fingerprint == current_db_fingerprint)
            .map_or(0, |a| a.conversation_count);
        let quality_processed = self
            .quality_tier
            .as_ref()
            .filter(|a| a.ready && a.db_fingerprint == current_db_fingerprint)
            .map_or(0, |a| a.conversation_count);

        self.backlog = BacklogLedger {
            total_conversations,
            fast_tier_processed: fast_processed,
            quality_tier_processed: quality_processed,
            db_fingerprint: current_db_fingerprint.to_owned(),
            computed_at_ms: now_ms(),
        };
    }

    /// Save a build checkpoint (called periodically during backfill).
    pub fn save_checkpoint(&mut self, checkpoint: BuildCheckpoint) {
        self.checkpoint = Some(checkpoint);
    }

    /// Clear the build checkpoint (called when build finishes or is abandoned).
    pub fn clear_checkpoint(&mut self) {
        self.checkpoint = None;
    }

    /// Record a completed artifact and clear the matching checkpoint.
    pub fn publish_artifact(&mut self, artifact: ArtifactRecord) {
        // Clear checkpoint if it matches this tier.
        if self
            .checkpoint
            .as_ref()
            .is_some_and(|cp| cp.tier == artifact.tier)
        {
            self.checkpoint = None;
        }

        match artifact.tier {
            TierKind::Fast => self.fast_tier = Some(artifact),
            TierKind::Quality => self.quality_tier = Some(artifact),
        }
    }

    /// Record a completed HNSW accelerator.
    pub fn publish_hnsw(&mut self, hnsw: HnswRecord) {
        self.hnsw = Some(hnsw);
    }

    /// Adopt a legacy (pre-manifest) artifact if it is compatible with the
    /// current schema/chunking versions.  Returns `true` if adopted.
    #[allow(clippy::too_many_arguments)]
    pub fn adopt_legacy_artifact(
        &mut self,
        tier: TierKind,
        embedder_id: &str,
        model_revision: &str,
        dimension: usize,
        doc_count: u64,
        conversation_count: u64,
        db_fingerprint: &str,
        index_path: &str,
        size_bytes: u64,
    ) -> bool {
        let record = ArtifactRecord {
            tier,
            embedder_id: embedder_id.to_owned(),
            model_revision: model_revision.to_owned(),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_version: CHUNKING_STRATEGY_VERSION,
            dimension,
            doc_count,
            conversation_count,
            db_fingerprint: db_fingerprint.to_owned(),
            index_path: index_path.to_owned(),
            size_bytes,
            started_at_ms: 0,
            completed_at_ms: now_ms(),
            ready: true,
        };

        match tier {
            TierKind::Fast => self.fast_tier = Some(record),
            TierKind::Quality => self.quality_tier = Some(record),
        }
        true
    }

    /// Invalidate artifacts that are incompatible with the current policy.
    /// Returns the number of artifacts invalidated.
    ///
    /// **Note**: This detects schema version, chunking version, and mode
    /// incompatibilities.  It does NOT detect embedder changes (e.g., minilm →
    /// snowflake) because the policy stores short names while artifacts store
    /// full registry IDs.  Callers who need embedder-change detection should
    /// compare `artifact.embedder_id` against the expected ID from the
    /// embedder registry.
    pub fn invalidate_incompatible(
        &mut self,
        policy: &SemanticPolicy,
        current_model_revision: &str,
    ) -> usize {
        let mut count = 0;

        if let Some(ref artifact) = self.fast_tier {
            let pm = artifact.to_policy_manifest();
            if matches!(
                pm.invalidation_action(policy, current_model_revision, &artifact.embedder_id),
                InvalidationAction::DiscardAndRebuild { .. } | InvalidationAction::Evict
            ) {
                self.fast_tier = None;
                count += 1;
            }
        }

        if let Some(ref artifact) = self.quality_tier {
            let pm = artifact.to_policy_manifest();
            if matches!(
                pm.invalidation_action(policy, current_model_revision, &artifact.embedder_id),
                InvalidationAction::DiscardAndRebuild { .. } | InvalidationAction::Evict
            ) {
                self.quality_tier = None;
                count += 1;
            }
        }

        // HNSW depends on the base tier — invalidate if base is gone.
        if let Some(ref hnsw) = self.hnsw {
            let base_gone = match hnsw.base_tier {
                TierKind::Fast => self.fast_tier.is_none(),
                TierKind::Quality => self.quality_tier.is_none(),
            };
            if base_gone {
                self.hnsw = None;
                count += 1;
            }
        }

        // Invalidate checkpoint if its schema/chunking is wrong.
        if let Some(ref cp) = self.checkpoint
            && (cp.schema_version != policy.semantic_schema_version
                || cp.chunking_version != policy.chunking_strategy_version)
        {
            self.checkpoint = None;
        }

        count
    }

    /// Total disk usage of all semantic artifacts (bytes).
    pub fn total_size_bytes(&self) -> u64 {
        let fast = self.fast_tier.as_ref().map_or(0, |a| a.size_bytes);
        let quality = self.quality_tier.as_ref().map_or(0, |a| a.size_bytes);
        let hnsw = self.hnsw.as_ref().map_or(0, |h| h.size_bytes);
        fast + quality + hnsw
    }

    /// Total disk usage in megabytes (rounded up).
    pub fn total_size_mb(&self) -> u64 {
        self.total_size_bytes().div_ceil(1_048_576)
    }
}

// ─── Immutable semantic generation contract ──────────────────────────────

/// Schema version for immutable semantic generation manifests.
pub const SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Schema version for the atomic current-generation pointer.
pub const SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION: u32 = 1;

/// Filename of the atomic current-generation pointer.
pub const SEMANTIC_CURRENT_POINTER_FILENAME: &str = "current.json";

/// Advisory serialization lock for compare-and-swap pointer publication.
pub const SEMANTIC_CURRENT_POINTER_LOCK_FILENAME: &str = "current.lock";

const SEMANTIC_CURRENT_POINTER_TEMP_PREFIX: &str = ".current.publish.";

/// Directory containing immutable semantic generations.
pub const SEMANTIC_GENERATIONS_DIRNAME: &str = "generations";

/// Directory containing unpublished semantic build state.
pub const SEMANTIC_STAGING_DIRNAME: &str = "staging";

/// Filename of an immutable generation manifest.
pub const SEMANTIC_GENERATION_MANIFEST_FILENAME: &str = "manifest.json";

const SHA256_HEX_LENGTH: usize = 64;
const MAX_GENERATION_ID_LENGTH: usize = 128;
const MAX_SEMANTIC_POINTER_BYTES: usize = 16 * 1024;
const MAX_SEMANTIC_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_SEMANTIC_ARTIFACTS: usize = 4;
const MAX_SEMANTIC_ARTIFACT_PATH_BYTES: usize = 1024;
const MAX_PRODUCER_FEATURES: usize = 256;
const MAX_PRODUCER_FEATURE_BYTES: usize = 128;
const MAX_EMBEDDING_DIMENSION: u32 = 65_536;

/// Requested or realized semantic tier topology.
///
/// Topology records which tiers exist, not whether each tier covers the whole
/// selected corpus. Per-tier coverage lives on [`SemanticGenerationArtifact`],
/// so a sealed full-progressive generation can truthfully expose a complete
/// fast tier and a partially populated quality tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticGenerationTopology {
    FastOnly,
    QualityOnly,
    FullProgressive,
}

impl SemanticGenerationTopology {
    fn includes_fast(self) -> bool {
        matches!(self, Self::FastOnly | Self::FullProgressive)
    }

    fn includes_quality(self) -> bool {
        matches!(self, Self::QualityOnly | Self::FullProgressive)
    }

    fn is_valid_realization_of(self, requested: Self) -> bool {
        match requested {
            Self::FastOnly => self == Self::FastOnly,
            Self::QualityOnly => self == Self::QualityOnly,
            Self::FullProgressive => true,
        }
    }
}

/// Immutable generation seal state.
///
/// Mutable build progress belongs under `vector_index/staging/`; a generation
/// eligible for `current.json` is durably sealed by construction. `Sealed`
/// deliberately does not mean that every tier has full corpus coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticGenerationStatus {
    /// Staging-only state. Deserializable for a typed failure, never sealable
    /// or selectable as an immutable generation.
    Building,
    Sealed,
}

/// Role of one manifest-declared semantic artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticArtifactRole {
    FastVector,
    QualityVector,
    FastAnn,
    QualityAnn,
}

impl SemanticArtifactRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FastVector => "fast_vector",
            Self::QualityVector => "quality_vector",
            Self::FastAnn => "fast_ann",
            Self::QualityAnn => "quality_ann",
        }
    }

    fn is_vector(self) -> bool {
        matches!(self, Self::FastVector | Self::QualityVector)
    }

    fn base_vector_role(self) -> Option<Self> {
        match self {
            Self::FastAnn => Some(Self::FastVector),
            Self::QualityAnn => Some(Self::QualityVector),
            Self::FastVector | Self::QualityVector => None,
        }
    }
}

/// Exact binding from an ANN artifact to its vector base and build parameters.
///
/// The base artifact bytes, canonical Frankensearch storage contract, and
/// producer-supplied vector-layout parameters are all bound independently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticAnnBaseBindingV1 {
    pub base_role: SemanticArtifactRole,
    pub base_artifact_sha256: String,
    pub base_storage_fingerprint: String,
    pub base_artifact_parameters_sha256: String,
    pub algorithm: String,
    pub metric: String,
    pub parameters_sha256: String,
}

/// Strong identity for the canonical corpus snapshot used by a generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticCorpusSnapshotIdentity {
    pub database_uuid: String,
    pub source_change_sequence: u64,
    pub schema_contract_sha256: String,
    pub content_selection_sha256: String,
    pub canonicalization_sha256: String,
    pub chunking_sha256: String,
    pub document_id_contract_sha256: String,
    pub config_sha256: String,
    pub corpus_sha256: String,
    pub ordered_live_docset_sha256: String,
    pub content_sha256: String,
    pub document_count: u64,
}

/// Reproducible producer provenance for every artifact in a generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticProducerAttestation {
    pub cass_git_commit: String,
    pub frankensearch_git_commit: String,
    pub cargo_lock_sha256: String,
    pub rust_toolchain: String,
    pub enabled_features: Vec<String>,
    pub build_profile: String,
    pub binary_elf_sha256: String,
}

/// Evidence captured when the producer inspected an artifact before sealing
/// the immutable generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticArtifactValidationEvidence {
    pub validator_id: String,
    pub validated_at_ms: i64,
    pub artifact_sha256: String,
    pub size_bytes: u64,
    pub selected_document_count: u64,
    pub covered_document_count: u64,
    pub vector_slot_count: u64,
    pub live_vector_count: u64,
    pub tombstone_vector_count: u64,
    pub wal_entry_count: u64,
    pub generation_corpus_sha256: String,
    pub covered_live_docset_sha256: String,
    pub covered_content_sha256: String,
}

/// Exact description of one immutable vector or ANN artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticGenerationArtifact {
    pub role: SemanticArtifactRole,
    /// Manifest-relative path beneath the selected generation directory.
    pub relative_path: String,
    pub artifact_sha256: String,
    pub size_bytes: u64,
    /// Exact artifact-container format, for example `fsvi-v2` or `hnsw-v1`.
    pub artifact_format: String,
    /// Digest of all format/layout/build parameters not already represented by
    /// the canonical embedding identity.
    pub artifact_parameters_sha256: String,
    /// The accepted Frankensearch identity contract, stored losslessly. CASS
    /// never redefines or weakens embedding-space compatibility.
    pub embedding_identity: FrozenEmbeddingIdentityBundleV1,
    /// Required for ANN roles and forbidden for vector roles.
    pub ann_base: Option<SemanticAnnBaseBindingV1>,
    /// Number of live corpus documents selected for this generation.
    pub selected_document_count: u64,
    /// Number of selected documents represented by a live vector in this tier.
    pub covered_document_count: u64,
    /// Physical slots in the artifact, including tombstones.
    pub vector_slot_count: u64,
    pub live_vector_count: u64,
    pub tombstone_vector_count: u64,
    pub wal_entry_count: u64,
    pub generation_corpus_sha256: String,
    /// Exact ordered identity of the live documents covered by this tier.
    pub covered_live_docset_sha256: String,
    /// Exact content identity of the live documents covered by this tier.
    pub covered_content_sha256: String,
    pub validation: SemanticArtifactValidationEvidence,
}

/// One immutable, self-describing semantic generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticGenerationManifestV1 {
    pub schema_version: u32,
    pub generation_id: String,
    pub build_id: String,
    pub request_id: Option<String>,
    pub previous_generation_id: Option<String>,
    pub created_at_ms: i64,
    pub completed_at_ms: i64,
    /// Time immutable artifacts and this manifest were durably sealed.
    pub sealed_at_ms: i64,
    pub requested_topology: SemanticGenerationTopology,
    pub realized_topology: SemanticGenerationTopology,
    pub status: SemanticGenerationStatus,
    pub corpus: SemanticCorpusSnapshotIdentity,
    pub producer: SemanticProducerAttestation,
    /// Exact selected live-document count; equal to `corpus.document_count`.
    pub selected_document_count: u64,
    /// Sum of physical vector slots across vector tiers. A full two-tier
    /// generation therefore normally counts each covered document twice.
    pub total_vector_slot_count_across_tiers: u64,
    /// Sum of live vectors across vector tiers, not a unique-document count.
    pub total_live_vector_count_across_tiers: u64,
    /// Sum of tombstone slots across vector tiers.
    pub total_tombstone_vector_count_across_tiers: u64,
    /// Sum of WAL entries represented across vector tiers.
    pub total_wal_entry_count_across_tiers: u64,
    /// Sorted by role. Stable ordering is part of canonical serialization.
    pub artifacts: Vec<SemanticGenerationArtifact>,
}

/// Atomic pointer selecting exactly one immutable semantic generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticCurrentPointerV1 {
    pub schema_version: u32,
    pub generation_id: String,
    pub manifest_sha256: String,
    /// Monotone publication epoch allocated under the current-pointer CAS.
    /// Epoch 1 is the first selection; every successful replacement is exactly
    /// the previous epoch plus one.
    pub selection_epoch: u64,
    /// Time this pointer selection occurred. This is not part of the immutable
    /// generation and is informational; CAS ordering uses `selection_epoch`.
    pub selected_at_ms: i64,
}

/// Required compare-and-swap precondition for current-pointer publication.
///
/// There is intentionally no `Any` variant: every publisher must prove either
/// that no current generation exists or that it observed the entire exact
/// pointer state it intends to replace. Capturing the monotone
/// `selection_epoch` closes the A -> B -> A ABA hole that generation and
/// manifest identity alone cannot detect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticExpectedCurrentV1 {
    Absent,
    Exact(SemanticCurrentPointerV1),
}

impl SemanticExpectedCurrentV1 {
    pub fn exact(pointer: &SemanticCurrentPointerV1) -> Self {
        Self::Exact(pointer.clone())
    }
}

/// A current generation whose pointer, manifest, corpus identity, paths, and
/// artifact bytes all passed fail-closed validation.
#[derive(Debug, Clone)]
pub struct ValidatedSemanticGeneration {
    pub pointer: SemanticCurrentPointerV1,
    pub manifest: SemanticGenerationManifestV1,
    pub generation_dir: PathBuf,
    /// Diagnostic resolution only. Serving code must not reopen these paths:
    /// the retained immutable FSVI witness/search owner is gated by
    /// `coding_agent_session_search-cass-fsvi-semantic-witness-xbq0b`.
    pub artifact_paths: BTreeMap<SemanticArtifactRole, PathBuf>,
}

/// Minimal, serializable disk-resolution vocabulary shared with doctor/robot
/// surfaces. Full remediation planning remains downstream, but these states
/// must never collapse into an ambiguous "semantic missing" result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum SemanticGenerationDiskStateV1 {
    Absent,
    Building {
        build_count: u64,
        unselected_generation_directory_count: u64,
        legacy_artifact_count: u64,
    },
    InterruptedPublish {
        temporary_entry_count: u64,
        build_count: u64,
        unselected_generation_directory_count: u64,
        legacy_artifact_count: u64,
    },
    PointerMissingWithGenerationDirectories {
        generation_directory_count: u64,
        legacy_artifact_count: u64,
    },
    LegacyUnidentified {
        artifact_count: u64,
    },
    Ready {
        generation_id: String,
        manifest_sha256: String,
        selection_epoch: u64,
        selected_at_ms: i64,
        requested_topology: SemanticGenerationTopology,
        realized_topology: SemanticGenerationTopology,
    },
}

impl SemanticGenerationDiskStateV1 {
    pub fn reason_code(&self) -> Option<&'static str> {
        match self {
            Self::Absent => Some("semantic_generation_absent"),
            Self::Building { .. } => Some("semantic_generation_building"),
            Self::InterruptedPublish { .. } => Some("semantic_pointer_publish_interrupted"),
            Self::PointerMissingWithGenerationDirectories { .. } => {
                Some("semantic_pointer_missing_with_generation_directories")
            }
            Self::LegacyUnidentified { .. } => Some("semantic_generation_legacy_unidentified"),
            Self::Ready { .. } => None,
        }
    }

    pub fn recovery_action(&self) -> Option<SemanticGenerationRecoveryAction> {
        match self {
            Self::Absent => Some(SemanticGenerationRecoveryAction::ProvisionAndIndex),
            Self::Building { .. } => Some(SemanticGenerationRecoveryAction::WaitForBuild),
            Self::InterruptedPublish { .. } => Some(SemanticGenerationRecoveryAction::RetryPublish),
            Self::PointerMissingWithGenerationDirectories { .. } => {
                Some(SemanticGenerationRecoveryAction::RollbackOrReindex)
            }
            Self::LegacyUnidentified { .. } => {
                Some(SemanticGenerationRecoveryAction::ReindexRequired)
            }
            Self::Ready { .. } => None,
        }
    }

    pub fn robot_projection(&self) -> SemanticGenerationDiskStateRobotV1 {
        SemanticGenerationDiskStateRobotV1 {
            schema_version: 1,
            state: self.clone(),
            reason_code: self.reason_code().map(str::to_owned),
            recovery_action: self.recovery_action(),
        }
    }
}

impl std::fmt::Display for SemanticGenerationDiskStateV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => write!(
                formatter,
                "no semantic generation exists; provision an embedder and build the initial index"
            ),
            Self::Building {
                build_count,
                unselected_generation_directory_count,
                legacy_artifact_count,
            } => write!(
                formatter,
                "staged semantic build count: {build_count}; unselected generation candidate directory count: {unselected_generation_directory_count}; unselected legacy artifact count: {legacy_artifact_count}"
            ),
            Self::InterruptedPublish {
                temporary_entry_count,
                build_count,
                unselected_generation_directory_count,
                legacy_artifact_count,
            } => write!(
                formatter,
                "interrupted pointer publication temp count: {temporary_entry_count}; staged build count: {build_count}; unselected generation candidate directory count: {unselected_generation_directory_count}; legacy artifact count: {legacy_artifact_count}"
            ),
            Self::PointerMissingWithGenerationDirectories {
                generation_directory_count,
                legacy_artifact_count,
            } => write!(
                formatter,
                "current semantic pointer is missing; unselected generation candidate directory count: {generation_directory_count}; legacy artifact count: {legacy_artifact_count}; validate a candidate fully before rollback, otherwise reindex"
            ),
            Self::LegacyUnidentified { artifact_count } => write!(
                formatter,
                "legacy semantic artifacts without authoritative identity: {artifact_count}"
            ),
            Self::Ready {
                generation_id,
                requested_topology,
                realized_topology,
                ..
            } => write!(
                formatter,
                "semantic generation {} is ready ({requested_topology:?} requested, {realized_topology:?} realized)",
                fingerprint_prefix(generation_id)
            ),
        }
    }
}

/// Stable robot envelope for disk-state diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticGenerationDiskStateRobotV1 {
    pub schema_version: u32,
    pub state: SemanticGenerationDiskStateV1,
    pub reason_code: Option<String>,
    pub recovery_action: Option<SemanticGenerationRecoveryAction>,
}

/// Stable action class downstream readiness surfaces can turn into a richer
/// `RecoveryPlan` without guessing from error strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticGenerationRecoveryAction {
    ProvisionAndIndex,
    ReindexRequired,
    RollbackOrReindex,
    UpgradeRequired,
    RetryPublish,
    WaitForBuild,
    InspectCorruption,
}

/// Stable classification for a rejected manifest invariant.
///
/// Downstream diagnostics must branch on this enum/reason code, never parse a
/// human explanation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticManifestInvariantClass {
    Identity,
    Path,
    Topology,
    Count,
    SealState,
    HistoricalSchema,
    PartialCoverage,
    AnnBinding,
    Timestamp,
    Provenance,
}

/// Fail-closed current-generation resolution errors.
#[derive(Debug)]
pub enum SemanticGenerationError {
    MissingPointer,
    BuildingGeneration {
        generation_id: String,
        build_id: String,
    },
    LegacyUnidentified {
        artifact_count: u64,
    },
    PointerIo {
        source: String,
    },
    PointerParse {
        source: String,
    },
    UnsupportedPointerSchema {
        found: u64,
        supported: u64,
    },
    InvalidPointer {
        reason: String,
    },
    MissingManifest {
        generation_id: String,
    },
    ManifestIo {
        generation_id: String,
        source: String,
    },
    ManifestParse {
        generation_id: String,
        source: String,
    },
    UnsupportedManifestSchema {
        found: u64,
        supported: u64,
    },
    UnsupportedEmbeddingIdentitySchema {
        component: String,
        found: u64,
        supported: u64,
    },
    InvalidManifest {
        class: SemanticManifestInvariantClass,
        reason: String,
    },
    ManifestDigestMismatch {
        expected: String,
        actual: String,
    },
    StaleCorpus {
        expected: String,
        actual: String,
    },
    ArtifactIo {
        role: SemanticArtifactRole,
        source: String,
    },
    ArtifactSizeMismatch {
        role: SemanticArtifactRole,
        expected: u64,
        actual: u64,
    },
    ArtifactDigestMismatch {
        role: SemanticArtifactRole,
        expected: String,
        actual: String,
    },
    EmbeddingIdentityMismatch {
        role: SemanticArtifactRole,
        expected: String,
        actual: String,
        reason: String,
    },
    ImmutableManifestConflict {
        generation_id: String,
    },
    ConcurrentPublishConflict {
        expected: String,
        actual: String,
    },
    SelectionEpochNotAdvanced {
        prior_selection_epoch: u64,
        proposed_selection_epoch: u64,
    },
    PublishLockIo {
        source: String,
    },
    Serialize {
        source: String,
    },
}

impl SemanticGenerationError {
    pub fn reason_code(&self) -> &'static str {
        match self {
            Self::MissingPointer => "semantic_pointer_missing",
            Self::BuildingGeneration { .. } => "semantic_generation_building",
            Self::LegacyUnidentified { .. } => "semantic_generation_legacy_unidentified",
            Self::PointerIo { .. } => "semantic_pointer_io",
            Self::PointerParse { .. } => "semantic_pointer_corrupt",
            Self::UnsupportedPointerSchema { .. } => "semantic_pointer_upgrade_required",
            Self::InvalidPointer { .. } => "semantic_pointer_invalid",
            Self::MissingManifest { .. } => "semantic_manifest_missing",
            Self::ManifestIo { .. } => "semantic_manifest_io",
            Self::ManifestParse { .. } => "semantic_manifest_corrupt",
            Self::UnsupportedManifestSchema { .. } => "semantic_manifest_upgrade_required",
            Self::UnsupportedEmbeddingIdentitySchema { .. } => {
                "semantic_embedding_identity_upgrade_required"
            }
            Self::InvalidManifest { class, .. } => match class {
                SemanticManifestInvariantClass::Identity => "semantic_manifest_identity_invalid",
                SemanticManifestInvariantClass::Path => "semantic_manifest_path_invalid",
                SemanticManifestInvariantClass::Topology => "semantic_manifest_topology_invalid",
                SemanticManifestInvariantClass::Count => "semantic_manifest_count_invalid",
                SemanticManifestInvariantClass::SealState => "semantic_manifest_not_durably_sealed",
                SemanticManifestInvariantClass::HistoricalSchema => {
                    "semantic_manifest_historical_schema"
                }
                SemanticManifestInvariantClass::PartialCoverage => {
                    "semantic_manifest_partial_coverage_invalid"
                }
                SemanticManifestInvariantClass::AnnBinding => {
                    "semantic_manifest_ann_binding_invalid"
                }
                SemanticManifestInvariantClass::Timestamp => "semantic_manifest_timestamp_invalid",
                SemanticManifestInvariantClass::Provenance => {
                    "semantic_manifest_provenance_invalid"
                }
            },
            Self::ManifestDigestMismatch { .. } => "semantic_manifest_digest_mismatch",
            Self::StaleCorpus { .. } => "semantic_generation_stale_source",
            Self::ArtifactIo { .. } => "semantic_artifact_io",
            Self::ArtifactSizeMismatch { .. } => "semantic_artifact_size_mismatch",
            Self::ArtifactDigestMismatch { .. } => "semantic_artifact_digest_mismatch",
            Self::EmbeddingIdentityMismatch { .. } => "semantic_embedding_identity_mismatch",
            Self::ImmutableManifestConflict { .. } => "semantic_manifest_immutable_conflict",
            Self::ConcurrentPublishConflict { .. } => "semantic_pointer_cas_conflict",
            Self::SelectionEpochNotAdvanced { .. } => {
                "semantic_pointer_selection_epoch_not_advanced"
            }
            Self::PublishLockIo { .. } => "semantic_pointer_lock_io",
            Self::Serialize { .. } => "semantic_manifest_serialize",
        }
    }

    pub fn recovery_action(&self) -> SemanticGenerationRecoveryAction {
        match self {
            Self::UnsupportedPointerSchema { .. }
            | Self::UnsupportedManifestSchema { .. }
            | Self::UnsupportedEmbeddingIdentitySchema { .. } => {
                SemanticGenerationRecoveryAction::UpgradeRequired
            }
            Self::MissingPointer
            | Self::StaleCorpus { .. }
            | Self::LegacyUnidentified { .. }
            | Self::ImmutableManifestConflict { .. } => {
                SemanticGenerationRecoveryAction::ReindexRequired
            }
            Self::BuildingGeneration { .. } => SemanticGenerationRecoveryAction::WaitForBuild,
            Self::ConcurrentPublishConflict { .. }
            | Self::SelectionEpochNotAdvanced { .. }
            | Self::PublishLockIo { .. } => SemanticGenerationRecoveryAction::RetryPublish,
            Self::PointerParse { .. } | Self::InvalidPointer { .. } | Self::Serialize { .. } => {
                SemanticGenerationRecoveryAction::InspectCorruption
            }
            Self::PointerIo { .. }
            | Self::ManifestIo { .. }
            | Self::MissingManifest { .. }
            | Self::ManifestParse { .. }
            | Self::InvalidManifest { .. }
            | Self::ManifestDigestMismatch { .. }
            | Self::ArtifactIo { .. }
            | Self::ArtifactSizeMismatch { .. }
            | Self::ArtifactDigestMismatch { .. }
            | Self::EmbeddingIdentityMismatch { .. } => {
                SemanticGenerationRecoveryAction::RollbackOrReindex
            }
        }
    }
}

impl std::fmt::Display for SemanticGenerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPointer => write!(f, "semantic current pointer is missing"),
            Self::BuildingGeneration {
                generation_id,
                build_id,
            } => write!(
                f,
                "semantic generation {} from build {} is still building",
                fingerprint_prefix(generation_id),
                fingerprint_prefix(build_id)
            ),
            Self::LegacyUnidentified { artifact_count } => write!(
                f,
                "found {artifact_count} legacy semantic artifact entries without authoritative identity"
            ),
            Self::PointerIo { .. } => write!(f, "semantic pointer I/O failure"),
            Self::PointerParse { .. } => write!(f, "semantic pointer parse failure"),
            Self::UnsupportedPointerSchema { found, supported } => write!(
                f,
                "semantic pointer schema {found} is newer than supported schema {supported}"
            ),
            Self::InvalidPointer { .. } => write!(f, "semantic pointer failed validation"),
            Self::MissingManifest { generation_id } => {
                write!(
                    f,
                    "semantic manifest is missing for generation {}",
                    fingerprint_prefix(generation_id)
                )
            }
            Self::ManifestIo { generation_id, .. } => write!(
                f,
                "semantic manifest I/O failure for generation {}",
                fingerprint_prefix(generation_id)
            ),
            Self::ManifestParse { generation_id, .. } => write!(
                f,
                "semantic manifest parse failure for generation {}",
                fingerprint_prefix(generation_id)
            ),
            Self::UnsupportedManifestSchema { found, supported } => write!(
                f,
                "semantic generation schema {found} is newer than supported schema {supported}"
            ),
            Self::UnsupportedEmbeddingIdentitySchema {
                component,
                found,
                supported,
            } => write!(
                f,
                "semantic embedding identity component {component} schema {found} is newer than supported schema {supported}"
            ),
            Self::InvalidManifest { class, .. } => {
                write!(f, "semantic generation failed {class:?} validation")
            }
            Self::ManifestDigestMismatch { expected, actual } => write!(
                f,
                "semantic generation manifest digest mismatch: expected {}, got {}",
                fingerprint_prefix(expected),
                fingerprint_prefix(actual)
            ),
            Self::StaleCorpus { expected, actual } => write!(
                f,
                "semantic generation corpus is stale: expected {}, got {}",
                fingerprint_prefix(expected),
                fingerprint_prefix(actual)
            ),
            Self::ArtifactIo { role, .. } => {
                write!(f, "semantic {} artifact I/O failure", role.as_str())
            }
            Self::ArtifactSizeMismatch {
                role,
                expected,
                actual,
            } => write!(
                f,
                "semantic {} artifact size mismatch: expected {expected}, got {actual}",
                role.as_str()
            ),
            Self::ArtifactDigestMismatch {
                role,
                expected,
                actual,
            } => write!(
                f,
                "semantic {} artifact digest mismatch: expected {}, got {}",
                role.as_str(),
                fingerprint_prefix(expected),
                fingerprint_prefix(actual)
            ),
            Self::EmbeddingIdentityMismatch {
                role,
                expected,
                actual,
                ..
            } => write!(
                f,
                "semantic {} embedding identity mismatch: expected {}, got {}",
                role.as_str(),
                fingerprint_prefix(expected),
                fingerprint_prefix(actual)
            ),
            Self::ImmutableManifestConflict { generation_id } => write!(
                f,
                "immutable semantic manifest conflicts for generation {}",
                fingerprint_prefix(generation_id)
            ),
            Self::ConcurrentPublishConflict { expected, actual } => write!(
                f,
                "semantic pointer compare-and-swap conflict: expected {}, found {}",
                fingerprint_prefix(expected),
                fingerprint_prefix(actual)
            ),
            Self::SelectionEpochNotAdvanced {
                prior_selection_epoch,
                proposed_selection_epoch,
            } => write!(
                f,
                "semantic pointer selection epoch must advance exactly once: prior {prior_selection_epoch}, proposed {proposed_selection_epoch}"
            ),
            Self::PublishLockIo { .. } => {
                write!(f, "semantic pointer publication lock I/O failure")
            }
            Self::Serialize { .. } => {
                write!(f, "semantic generation serialization failure")
            }
        }
    }
}

impl std::error::Error for SemanticGenerationError {}

impl SemanticGenerationManifestV1 {
    /// Deterministic compact JSON used for storage and inspection.
    ///
    /// Integrity never depends on serde's field ordering. [`Self::sha256`]
    /// hashes the domain-separated binary transcript from
    /// [`Self::canonical_digest_bytes`] instead.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, SemanticGenerationError> {
        let mut bytes =
            serde_json::to_vec(self).map_err(|error| SemanticGenerationError::Serialize {
                source: error.to_string(),
            })?;
        bytes.push(b'\n');
        if bytes.len() > MAX_SEMANTIC_MANIFEST_BYTES {
            return Err(SemanticGenerationError::Serialize {
                source: "semantic generation manifest exceeds its size limit".to_owned(),
            });
        }
        Ok(bytes)
    }

    fn canonical_digest_bytes(&self) -> Vec<u8> {
        let mut encoder = SemanticCanonicalEncoder::new(b"cass.semantic-generation-manifest.v1");
        encoder.u32(self.schema_version);
        encoder.text(&self.generation_id);
        encoder.text(&self.build_id);
        encoder.optional_text(self.request_id.as_deref());
        encoder.optional_text(self.previous_generation_id.as_deref());
        encoder.i64(self.created_at_ms);
        encoder.i64(self.completed_at_ms);
        encoder.i64(self.sealed_at_ms);
        encoder.u8(topology_tag(self.requested_topology));
        encoder.u8(topology_tag(self.realized_topology));
        encoder.u8(match self.status {
            SemanticGenerationStatus::Building => 1,
            SemanticGenerationStatus::Sealed => 2,
        });
        encode_corpus_identity(&mut encoder, &self.corpus);
        encode_producer_attestation(&mut encoder, &self.producer);
        encoder.u64(self.selected_document_count);
        encoder.u64(self.total_vector_slot_count_across_tiers);
        encoder.u64(self.total_live_vector_count_across_tiers);
        encoder.u64(self.total_tombstone_vector_count_across_tiers);
        encoder.u64(self.total_wal_entry_count_across_tiers);
        encoder.len(self.artifacts.len());
        for artifact in &self.artifacts {
            encode_generation_artifact(&mut encoder, artifact);
        }
        encoder.finish()
    }

    pub fn sha256(&self) -> Result<String, SemanticGenerationError> {
        Ok(sha256_hex(&self.canonical_digest_bytes()))
    }

    pub fn generation_dir(&self, data_dir: &Path) -> Result<PathBuf, SemanticGenerationError> {
        validate_generation_id(&self.generation_id).map_err(|reason| {
            SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::Path,
                reason,
            }
        })?;
        Ok(semantic_generations_root(data_dir).join(&self.generation_id))
    }

    pub fn manifest_path(&self, data_dir: &Path) -> Result<PathBuf, SemanticGenerationError> {
        Ok(self
            .generation_dir(data_dir)?
            .join(SEMANTIC_GENERATION_MANIFEST_FILENAME))
    }

    /// Validate every authoritative field without consulting the filesystem.
    pub fn validate(&self) -> Result<(), SemanticGenerationError> {
        if self.schema_version != SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION {
            if self.schema_version > SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION {
                return Err(SemanticGenerationError::UnsupportedManifestSchema {
                    found: u64::from(self.schema_version),
                    supported: u64::from(SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION),
                });
            }
            return invalid_manifest(
                SemanticManifestInvariantClass::HistoricalSchema,
                &format!(
                    "unsupported historical schema {}; expected {}",
                    self.schema_version, SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION
                ),
            );
        }

        validate_generation_id(&self.generation_id).map_err(|reason| {
            SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::Path,
                reason,
            }
        })?;
        if let Some(previous) = self.previous_generation_id.as_deref() {
            validate_generation_id(previous).map_err(|reason| {
                SemanticGenerationError::InvalidManifest {
                    class: SemanticManifestInvariantClass::Path,
                    reason,
                }
            })?;
            if previous == self.generation_id {
                return invalid_manifest(
                    SemanticManifestInvariantClass::Topology,
                    "previous generation must differ from current generation",
                );
            }
        }
        validate_generation_id(&self.build_id).map_err(|reason| {
            SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::Provenance,
                reason: format!("invalid build_id: {reason}"),
            }
        })?;
        if let Some(request_id) = self.request_id.as_deref() {
            validate_identifier(
                SemanticManifestInvariantClass::Provenance,
                "request_id",
                request_id,
            )?;
        }
        if self.created_at_ms < 0
            || self.completed_at_ms < self.created_at_ms
            || self.sealed_at_ms < self.completed_at_ms
        {
            return invalid_manifest(
                SemanticManifestInvariantClass::Timestamp,
                "timestamps must satisfy 0 <= created_at <= completed_at <= sealed_at",
            );
        }
        if !self
            .realized_topology
            .is_valid_realization_of(self.requested_topology)
        {
            return invalid_manifest(
                SemanticManifestInvariantClass::Topology,
                "realized topology is not a subset of requested topology",
            );
        }
        if self.status != SemanticGenerationStatus::Sealed {
            return Err(SemanticGenerationError::BuildingGeneration {
                generation_id: self.generation_id.clone(),
                build_id: self.build_id.clone(),
            });
        }

        validate_corpus_identity(&self.corpus)?;
        validate_producer_attestation(&self.producer)?;
        if self.selected_document_count != self.corpus.document_count {
            return invalid_manifest(
                SemanticManifestInvariantClass::Count,
                "manifest and corpus selected-document counts differ",
            );
        }
        if self.selected_document_count == 0 {
            return invalid_manifest(
                SemanticManifestInvariantClass::Count,
                "sealed generations must select at least one document",
            );
        }
        if self.artifacts.is_empty() {
            return invalid_manifest(
                SemanticManifestInvariantClass::Topology,
                "sealed generations must declare artifacts",
            );
        }
        if self.artifacts.len() > MAX_SEMANTIC_ARTIFACTS {
            return invalid_manifest(
                SemanticManifestInvariantClass::Topology,
                "semantic generation declares too many artifact roles",
            );
        }

        let mut roles = BTreeSet::new();
        let mut paths = BTreeSet::new();
        let mut portable_paths = BTreeSet::new();
        let mut previous_role = None;
        for artifact in &self.artifacts {
            if previous_role.is_some_and(|previous| previous >= artifact.role) {
                return invalid_manifest(
                    SemanticManifestInvariantClass::Topology,
                    "artifacts must be strictly sorted by role with no duplicates",
                );
            }
            previous_role = Some(artifact.role);
            if !roles.insert(artifact.role) {
                return invalid_manifest(
                    SemanticManifestInvariantClass::Topology,
                    "duplicate artifact role",
                );
            }
            let normalized =
                normalized_manifest_relative_path(&artifact.relative_path).map_err(|reason| {
                    SemanticGenerationError::InvalidManifest {
                        class: SemanticManifestInvariantClass::Path,
                        reason,
                    }
                })?;
            if !paths.insert(normalized) {
                return invalid_manifest(
                    SemanticManifestInvariantClass::Path,
                    "duplicate or lexically aliased artifact path",
                );
            }
            if !portable_paths.insert(artifact.relative_path.to_ascii_lowercase()) {
                return invalid_manifest(
                    SemanticManifestInvariantClass::Path,
                    "artifact paths collide under portable case-insensitive lookup",
                );
            }
            validate_generation_artifact(
                artifact,
                &self.corpus,
                self.created_at_ms,
                self.completed_at_ms,
                self.sealed_at_ms,
            )?;
        }

        for required in required_vector_roles(self.realized_topology) {
            if !roles.contains(&required) {
                return invalid_manifest(
                    SemanticManifestInvariantClass::Topology,
                    &format!(
                        "realized topology is missing required role {}",
                        required.as_str(),
                    ),
                );
            }
        }
        if !self.realized_topology.includes_fast()
            && (roles.contains(&SemanticArtifactRole::FastVector)
                || roles.contains(&SemanticArtifactRole::FastAnn))
        {
            return invalid_manifest(
                SemanticManifestInvariantClass::Topology,
                "fast artifacts contradict realized topology",
            );
        }
        if !self.realized_topology.includes_quality()
            && (roles.contains(&SemanticArtifactRole::QualityVector)
                || roles.contains(&SemanticArtifactRole::QualityAnn))
        {
            return invalid_manifest(
                SemanticManifestInvariantClass::Topology,
                "quality artifacts contradict realized topology",
            );
        }

        for ann_role in [
            SemanticArtifactRole::FastAnn,
            SemanticArtifactRole::QualityAnn,
        ] {
            if let Some(ann) = self.artifact(ann_role) {
                let Some(base_role) = ann_role.base_vector_role() else {
                    return invalid_manifest(
                        SemanticManifestInvariantClass::AnnBinding,
                        "internal artifact-role contract is inconsistent",
                    );
                };
                let Some(base) = self.artifact(base_role) else {
                    return invalid_manifest(
                        SemanticManifestInvariantClass::AnnBinding,
                        "ANN artifact has no manifest-declared base vector",
                    );
                };
                let Some(binding) = ann.ann_base.as_ref() else {
                    return invalid_manifest(
                        SemanticManifestInvariantClass::AnnBinding,
                        "ANN artifact is missing its exact base binding",
                    );
                };
                if ann.embedding_identity != base.embedding_identity {
                    return Err(SemanticGenerationError::EmbeddingIdentityMismatch {
                        role: ann_role,
                        expected: base.embedding_identity.fingerprint.clone(),
                        actual: ann.embedding_identity.fingerprint.clone(),
                        reason: "ANN embedding identity differs from its vector base".to_owned(),
                    });
                }
                if binding.base_role != base_role
                    || binding.base_artifact_sha256 != base.artifact_sha256
                    || binding.base_storage_fingerprint
                        != base.embedding_identity.identity.storage.fingerprint()
                    || binding.base_artifact_parameters_sha256 != base.artifact_parameters_sha256
                    || binding.parameters_sha256 != ann.artifact_parameters_sha256
                    || ann.selected_document_count != base.selected_document_count
                    || ann.covered_document_count != base.covered_document_count
                    || ann.vector_slot_count != base.vector_slot_count
                    || ann.live_vector_count != base.live_vector_count
                    || ann.tombstone_vector_count != base.tombstone_vector_count
                    || ann.generation_corpus_sha256 != base.generation_corpus_sha256
                    || ann.covered_live_docset_sha256 != base.covered_live_docset_sha256
                    || ann.covered_content_sha256 != base.covered_content_sha256
                {
                    return invalid_manifest(
                        SemanticManifestInvariantClass::AnnBinding,
                        "ANN artifact does not bind the exact base bytes, storage, parameters, identity, counts, and covered corpus",
                    );
                }
            }
        }

        let mut vector_artifacts = self
            .artifacts
            .iter()
            .filter(|artifact| artifact.role.is_vector());
        let (vector_count, live_count, tombstone_count, wal_count) =
            vector_artifacts.try_fold((0_u64, 0_u64, 0_u64, 0_u64), |totals, artifact| {
                Ok::<_, SemanticGenerationError>((
                    checked_sum(totals.0, artifact.vector_slot_count, "vector slot count")?,
                    checked_sum(totals.1, artifact.live_vector_count, "live vector count")?,
                    checked_sum(
                        totals.2,
                        artifact.tombstone_vector_count,
                        "tombstone vector count",
                    )?,
                    checked_sum(totals.3, artifact.wal_entry_count, "WAL entry count")?,
                ))
            })?;
        if (
            self.total_vector_slot_count_across_tiers,
            self.total_live_vector_count_across_tiers,
            self.total_tombstone_vector_count_across_tiers,
            self.total_wal_entry_count_across_tiers,
        ) != (vector_count, live_count, tombstone_count, wal_count)
        {
            return invalid_manifest(
                SemanticManifestInvariantClass::Count,
                "manifest across-tier aggregate counts do not match vector artifacts",
            );
        }

        Ok(())
    }

    pub fn artifact(&self, role: SemanticArtifactRole) -> Option<&SemanticGenerationArtifact> {
        self.artifacts
            .binary_search_by_key(&role, |artifact| artifact.role)
            .ok()
            .and_then(|index| self.artifacts.get(index))
    }

    /// Durably seal an immutable generation manifest.
    ///
    /// Every artifact and its containing directory is synced first. The
    /// manifest is then written to a same-directory temporary file, synced,
    /// and atomically published without clobbering an existing manifest.
    /// Retrying the exact semantic manifest is idempotent; reusing a generation
    /// ID for different content fails closed.
    pub fn write_immutable(&self, data_dir: &Path) -> Result<String, SemanticGenerationError> {
        self.write_immutable_with_hook(data_dir, |_| Ok(()))
    }

    fn write_immutable_with_hook(
        &self,
        data_dir: &Path,
        mut after_checkpoint: impl FnMut(ManifestSealCheckpoint) -> Result<(), SemanticGenerationError>,
    ) -> Result<String, SemanticGenerationError> {
        let started = Instant::now();
        self.validate()?;
        let generation_dir = self.generation_dir(data_dir)?;
        prepare_generation_directory(data_dir, &generation_dir)?;
        self.validate_artifacts_on_disk(data_dir, true)?;
        after_checkpoint(ManifestSealCheckpoint::ArtifactDurabilityEstablished)?;
        let manifest_path = generation_dir.join(SEMANTIC_GENERATION_MANIFEST_FILENAME);
        reject_symlink_entry(&manifest_path, "generation manifest")?;
        let bytes = self.canonical_bytes()?;
        let digest = self.sha256()?;
        let mut temp = tempfile::Builder::new()
            .prefix(".manifest.seal.")
            .tempfile_in(&generation_dir)
            .map_err(|error| SemanticGenerationError::ManifestIo {
                generation_id: self.generation_id.clone(),
                source: error.to_string(),
            })?;
        temp.write_all(&bytes)
            .map_err(|error| SemanticGenerationError::ManifestIo {
                generation_id: self.generation_id.clone(),
                source: error.to_string(),
            })?;
        after_checkpoint(ManifestSealCheckpoint::TemporaryBytesWritten)?;
        temp.as_file()
            .sync_all()
            .map_err(|error| SemanticGenerationError::ManifestIo {
                generation_id: self.generation_id.clone(),
                source: error.to_string(),
            })?;
        after_checkpoint(ManifestSealCheckpoint::TemporaryFileSynced)?;

        match persist_named_temp(temp, &manifest_path, false) {
            Ok(file) => {
                file.sync_all()
                    .map_err(|error| SemanticGenerationError::ManifestIo {
                        generation_id: self.generation_id.clone(),
                        source: error.to_string(),
                    })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = load_manifest_file(&manifest_path, &self.generation_id)?;
                if existing != *self || existing.sha256()? != digest {
                    return Err(SemanticGenerationError::ImmutableManifestConflict {
                        generation_id: self.generation_id.clone(),
                    });
                }
            }
            Err(error) => {
                return Err(SemanticGenerationError::ManifestIo {
                    generation_id: self.generation_id.clone(),
                    source: error.to_string(),
                });
            }
        };
        after_checkpoint(ManifestSealCheckpoint::ManifestPublished)?;
        sync_parent_directory(&manifest_path).map_err(|error| {
            SemanticGenerationError::ManifestIo {
                generation_id: self.generation_id.clone(),
                source: error.to_string(),
            }
        })?;
        after_checkpoint(ManifestSealCheckpoint::ParentDirectorySynced)?;

        for artifact in &self.artifacts {
            tracing::info!(
                event = "semantic.manifest.artifact.validate",
                schema_version = self.schema_version,
                generation = %fingerprint_prefix(&self.generation_id),
                build = %fingerprint_prefix(&self.build_id),
                request = %optional_fingerprint_prefix(self.request_id.as_deref()),
                requested_topology = ?self.requested_topology,
                realized_topology = ?self.realized_topology,
                tier = artifact.role.as_str(),
                artifact = %artifact.relative_path,
                embedding_fingerprint =
                    %fingerprint_prefix(&artifact.embedding_identity.fingerprint),
                coverage_numerator = artifact.covered_document_count,
                coverage_denominator = artifact.selected_document_count,
                artifact_sha256 = %fingerprint_prefix(&artifact.artifact_sha256),
                "validated and synced manifest-declared semantic artifact"
            );
        }

        tracing::info!(
            event = "semantic.manifest.seal",
            schema_version = self.schema_version,
            generation = %fingerprint_prefix(&self.generation_id),
            build = %fingerprint_prefix(&self.build_id),
            request = %optional_fingerprint_prefix(self.request_id.as_deref()),
            requested_topology = ?self.requested_topology,
            realized_topology = ?self.realized_topology,
            manifest_sha256 = %fingerprint_prefix(&digest),
            selected_document_count = self.selected_document_count,
            total_vector_slot_count_across_tiers =
                self.total_vector_slot_count_across_tiers,
            artifact_count = self.artifacts.len(),
            elapsed_ms = elapsed_ms(started),
            "sealed immutable semantic generation manifest"
        );
        Ok(digest)
    }

    fn validate_artifacts_on_disk(
        &self,
        data_dir: &Path,
        sync_for_seal: bool,
    ) -> Result<BTreeMap<SemanticArtifactRole, PathBuf>, SemanticGenerationError> {
        let generation_dir = self.generation_dir(data_dir)?;
        validate_existing_generation_directory(data_dir, &generation_dir)?;
        let canonical_root =
            generation_dir
                .canonicalize()
                .map_err(|error| SemanticGenerationError::ManifestIo {
                    generation_id: self.generation_id.clone(),
                    source: error.to_string(),
                })?;
        let mut canonical_artifacts = BTreeSet::new();
        let mut physical_artifacts = BTreeSet::new();
        let mut synced_parent_directories = BTreeSet::new();
        let mut resolved = BTreeMap::new();

        for artifact in &self.artifacts {
            let relative =
                normalized_manifest_relative_path(&artifact.relative_path).map_err(|reason| {
                    SemanticGenerationError::InvalidManifest {
                        class: SemanticManifestInvariantClass::Path,
                        reason,
                    }
                })?;
            let joined = generation_dir.join(relative);
            reject_existing_path_links(&generation_dir, &joined).map_err(|error| {
                SemanticGenerationError::ArtifactIo {
                    role: artifact.role,
                    source: error.to_string(),
                }
            })?;
            if artifact.role.is_vector() {
                let wal_path = wal_path_for(&joined);
                match fs::symlink_metadata(&wal_path) {
                    Ok(_) => {
                        return Err(SemanticGenerationError::ArtifactIo {
                            role: artifact.role,
                            source: "immutable vector artifact has a WAL sidecar".to_owned(),
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(SemanticGenerationError::ArtifactIo {
                            role: artifact.role,
                            source: format!("failed inspecting immutable WAL sidecar: {error}"),
                        });
                    }
                }
            }
            let file = OpenOptions::new()
                .read(true)
                .write(sync_for_seal)
                .open(&joined)
                .map_err(|error| SemanticGenerationError::ArtifactIo {
                    role: artifact.role,
                    source: error.to_string(),
                })?;
            let metadata =
                file.metadata()
                    .map_err(|error| SemanticGenerationError::ArtifactIo {
                        role: artifact.role,
                        source: error.to_string(),
                    })?;
            if !metadata.is_file() {
                return Err(SemanticGenerationError::ArtifactIo {
                    role: artifact.role,
                    source: "manifest-declared artifact is not a regular file".to_owned(),
                });
            }
            let canonical =
                joined
                    .canonicalize()
                    .map_err(|error| SemanticGenerationError::ArtifactIo {
                        role: artifact.role,
                        source: error.to_string(),
                    })?;
            if !canonical.starts_with(&canonical_root) {
                return Err(SemanticGenerationError::ArtifactIo {
                    role: artifact.role,
                    source: "manifest-declared artifact escapes the generation root".to_owned(),
                });
            }
            let opened_identity = portable_file_identity(&file, &metadata).map_err(|error| {
                SemanticGenerationError::ArtifactIo {
                    role: artifact.role,
                    source: error.to_string(),
                }
            })?;
            if opened_identity.is_some_and(|identity| identity.link_count != 1) {
                return Err(SemanticGenerationError::InvalidManifest {
                    class: SemanticManifestInvariantClass::Path,
                    reason: "manifest-declared artifacts must not have external hard links"
                        .to_owned(),
                });
            }
            verify_path_still_names_open_file(&canonical, opened_identity).map_err(|error| {
                SemanticGenerationError::ArtifactIo {
                    role: artifact.role,
                    source: error.to_string(),
                }
            })?;
            if !canonical_artifacts.insert(canonical.clone()) {
                return Err(SemanticGenerationError::InvalidManifest {
                    class: SemanticManifestInvariantClass::Path,
                    reason: "multiple artifact roles resolve to the same file".to_owned(),
                });
            }
            if let Some(identity) = opened_identity
                && !physical_artifacts.insert(identity)
            {
                return Err(SemanticGenerationError::InvalidManifest {
                    class: SemanticManifestInvariantClass::Path,
                    reason: "multiple artifact roles are hard-link aliases of one file".to_owned(),
                });
            }
            if metadata.len() != artifact.size_bytes {
                return Err(SemanticGenerationError::ArtifactSizeMismatch {
                    role: artifact.role,
                    expected: artifact.size_bytes,
                    actual: metadata.len(),
                });
            }
            let actual_digest =
                sha256_reader(&file).map_err(|error| SemanticGenerationError::ArtifactIo {
                    role: artifact.role,
                    source: error.to_string(),
                })?;
            if actual_digest != artifact.artifact_sha256 {
                return Err(SemanticGenerationError::ArtifactDigestMismatch {
                    role: artifact.role,
                    expected: artifact.artifact_sha256.clone(),
                    actual: actual_digest,
                });
            }
            if sync_for_seal {
                file.sync_all()
                    .map_err(|error| SemanticGenerationError::ArtifactIo {
                        role: artifact.role,
                        source: error.to_string(),
                    })?;
                if let Some(parent) = canonical.parent() {
                    let relative_parent =
                        parent.strip_prefix(&canonical_root).map_err(|error| {
                            SemanticGenerationError::ArtifactIo {
                                role: artifact.role,
                                source: error.to_string(),
                            }
                        })?;
                    let mut directory = canonical_root.clone();
                    if synced_parent_directories.insert(directory.clone()) {
                        sync_directory(&directory).map_err(|error| {
                            SemanticGenerationError::ArtifactIo {
                                role: artifact.role,
                                source: error.to_string(),
                            }
                        })?;
                    }
                    for component in relative_parent.components() {
                        directory.push(component.as_os_str());
                        if synced_parent_directories.insert(directory.clone()) {
                            sync_directory(&directory).map_err(|error| {
                                SemanticGenerationError::ArtifactIo {
                                    role: artifact.role,
                                    source: error.to_string(),
                                }
                            })?;
                        }
                    }
                }
            }
            verify_path_still_names_open_file(&canonical, opened_identity).map_err(|error| {
                SemanticGenerationError::ArtifactIo {
                    role: artifact.role,
                    source: error.to_string(),
                }
            })?;
            resolved.insert(artifact.role, canonical);
        }
        Ok(resolved)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestSealCheckpoint {
    ArtifactDurabilityEstablished,
    TemporaryBytesWritten,
    TemporaryFileSynced,
    ManifestPublished,
    ParentDirectorySynced,
}

impl SemanticCurrentPointerV1 {
    pub fn path(data_dir: &Path) -> PathBuf {
        semantic_vector_root(data_dir).join(SEMANTIC_CURRENT_POINTER_FILENAME)
    }

    pub fn for_manifest(
        manifest: &SemanticGenerationManifestV1,
        selected_at_ms: i64,
        selection_epoch: u64,
    ) -> Result<Self, SemanticGenerationError> {
        manifest.validate()?;
        if selected_at_ms < manifest.sealed_at_ms {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: "selection timestamp predates immutable generation seal".to_owned(),
            });
        }
        if selection_epoch == 0 {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: "selection_epoch must be positive".to_owned(),
            });
        }
        Ok(Self {
            schema_version: SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION,
            generation_id: manifest.generation_id.clone(),
            manifest_sha256: manifest.sha256()?,
            selection_epoch,
            selected_at_ms,
        })
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, SemanticGenerationError> {
        let mut bytes =
            serde_json::to_vec(self).map_err(|error| SemanticGenerationError::Serialize {
                source: error.to_string(),
            })?;
        bytes.push(b'\n');
        if bytes.len() > MAX_SEMANTIC_POINTER_BYTES {
            return Err(SemanticGenerationError::Serialize {
                source: "semantic current pointer exceeds its size limit".to_owned(),
            });
        }
        Ok(bytes)
    }

    pub fn validate(&self) -> Result<(), SemanticGenerationError> {
        if self.schema_version != SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION {
            if self.schema_version > SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION {
                return Err(SemanticGenerationError::UnsupportedPointerSchema {
                    found: u64::from(self.schema_version),
                    supported: u64::from(SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION),
                });
            }
            return Err(SemanticGenerationError::InvalidPointer {
                reason: format!(
                    "unsupported historical pointer schema {}; expected {}",
                    self.schema_version, SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION
                ),
            });
        }
        validate_generation_id(&self.generation_id)
            .map_err(|reason| SemanticGenerationError::InvalidPointer { reason })?;
        validate_sha256("manifest_sha256", &self.manifest_sha256)
            .map_err(|reason| SemanticGenerationError::InvalidPointer { reason })?;
        if self.selection_epoch == 0 {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: "selection_epoch must be positive".to_owned(),
            });
        }
        if self.selected_at_ms < 0 {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: "selected_at_ms must be non-negative".to_owned(),
            });
        }
        Ok(())
    }

    /// Atomically select an already-sealed, fully verified generation with a
    /// serialized compare-and-swap precondition.
    ///
    /// `manifest.previous_generation_id` records immutable build lineage; it
    /// is deliberately not the publication precondition because rollback may
    /// select an older generation. `expected` is the sole current-state CAS.
    pub fn publish_atomic(
        &self,
        data_dir: &Path,
        expected: &SemanticExpectedCurrentV1,
    ) -> Result<(), SemanticGenerationError> {
        self.publish_atomic_with_hook(data_dir, expected, |_| Ok(()))
    }

    fn publish_atomic_with_hook(
        &self,
        data_dir: &Path,
        expected: &SemanticExpectedCurrentV1,
        mut after_checkpoint: impl FnMut(
            PointerPublishCheckpoint,
        ) -> Result<(), SemanticGenerationError>,
    ) -> Result<(), SemanticGenerationError> {
        let started = Instant::now();
        self.validate()?;
        let root = semantic_vector_root(data_dir);
        prepare_semantic_vector_root(data_dir, &root)?;
        let lock_path = root.join(SEMANTIC_CURRENT_POINTER_LOCK_FILENAME);
        let lock_file = open_semantic_publish_lock(&root, &lock_path)?;
        lock_file
            .lock_exclusive()
            .map_err(|error| SemanticGenerationError::PublishLockIo {
                source: error.to_string(),
            })?;
        let _lock = SemanticPublishLock(lock_file);

        let prior = read_current_pointer_for_cas(data_dir)?;
        verify_publish_precondition(expected, prior.as_ref())?;
        let required_selection_epoch = match prior.as_ref() {
            Some(prior) => prior.selection_epoch.checked_add(1).ok_or_else(|| {
                SemanticGenerationError::InvalidPointer {
                    reason: "selection epoch is exhausted".to_owned(),
                }
            })?,
            None => 1,
        };
        if self.selection_epoch != required_selection_epoch {
            return Err(SemanticGenerationError::SelectionEpochNotAdvanced {
                prior_selection_epoch: prior.as_ref().map_or(0, |pointer| pointer.selection_epoch),
                proposed_selection_epoch: self.selection_epoch,
            });
        }

        let loaded = load_manifest_selected_by_pointer(data_dir, self)?;
        if self.selected_at_ms < loaded.manifest.sealed_at_ms {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: "selection timestamp predates immutable generation seal".to_owned(),
            });
        }
        loaded
            .manifest
            .validate_artifacts_on_disk(data_dir, false)?;
        let pointer_path = Self::path(data_dir);
        reject_symlink_entry(&pointer_path, "semantic current pointer")?;
        let bytes = self.canonical_bytes()?;
        let mut temp = tempfile::Builder::new()
            .prefix(SEMANTIC_CURRENT_POINTER_TEMP_PREFIX)
            .tempfile_in(&root)
            .map_err(|error| SemanticGenerationError::PointerIo {
                source: error.to_string(),
            })?;
        temp.write_all(&bytes)
            .map_err(|error| SemanticGenerationError::PointerIo {
                source: error.to_string(),
            })?;
        after_checkpoint(PointerPublishCheckpoint::TemporaryBytesWritten)?;
        temp.as_file()
            .sync_all()
            .map_err(|error| SemanticGenerationError::PointerIo {
                source: error.to_string(),
            })?;
        after_checkpoint(PointerPublishCheckpoint::TemporaryFileSynced)?;
        let file = persist_named_temp(temp, &pointer_path, true).map_err(|error| {
            SemanticGenerationError::PointerIo {
                source: error.to_string(),
            }
        })?;
        file.sync_all()
            .map_err(|error| SemanticGenerationError::PointerIo {
                source: error.to_string(),
            })?;
        after_checkpoint(PointerPublishCheckpoint::PointerReplaced)?;
        sync_parent_directory(&pointer_path).map_err(|error| {
            SemanticGenerationError::PointerIo {
                source: error.to_string(),
            }
        })?;
        after_checkpoint(PointerPublishCheckpoint::ParentDirectorySynced)?;

        tracing::info!(
            event = "semantic.pointer.swap",
            schema_version = self.schema_version,
            generation = %fingerprint_prefix(&self.generation_id),
            build = %fingerprint_prefix(&loaded.manifest.build_id),
            request = %optional_fingerprint_prefix(loaded.manifest.request_id.as_deref()),
            requested_topology = ?loaded.manifest.requested_topology,
            realized_topology = ?loaded.manifest.realized_topology,
            manifest_sha256 = %fingerprint_prefix(&self.manifest_sha256),
            expected_prior = %expected_pointer_log_value(expected),
            actual_prior = %actual_pointer_log_value(prior.as_ref()),
            selection_epoch = self.selection_epoch,
            selected_at_ms = self.selected_at_ms,
            elapsed_ms = elapsed_ms(started),
            "compare-and-swap selected immutable semantic generation"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerPublishCheckpoint {
    TemporaryBytesWritten,
    TemporaryFileSynced,
    PointerReplaced,
    ParentDirectorySynced,
}

/// Load and verify the selected semantic generation. No conventional filename,
/// model-name, or fallback discovery is attempted.
pub fn load_current_semantic_generation(
    data_dir: &Path,
    expected_corpus: Option<&SemanticCorpusSnapshotIdentity>,
) -> Result<ValidatedSemanticGeneration, SemanticGenerationError> {
    let started = Instant::now();
    let mut pointer_for_log = None;
    let mut manifest_for_log = None;
    let result = (|| {
        let pointer_path = SemanticCurrentPointerV1::path(data_dir);
        match fs::symlink_metadata(&pointer_path) {
            Ok(metadata) if metadata_is_link_or_reparse(&metadata) => {
                return Err(SemanticGenerationError::InvalidPointer {
                    reason: "current pointer must not be a symlink or reparse point".to_owned(),
                });
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(SemanticGenerationError::InvalidPointer {
                    reason: "current pointer is not a regular file".to_owned(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SemanticGenerationError::MissingPointer);
            }
            Err(error) => {
                return Err(SemanticGenerationError::PointerIo {
                    source: error.to_string(),
                });
            }
        }
        let pointer_bytes =
            read_bounded_file(&pointer_path, MAX_SEMANTIC_POINTER_BYTES).map_err(|error| {
                if error.kind() == std::io::ErrorKind::InvalidData {
                    SemanticGenerationError::PointerParse {
                        source: error.to_string(),
                    }
                } else {
                    SemanticGenerationError::PointerIo {
                        source: error.to_string(),
                    }
                }
            })?;
        let pointer = parse_current_pointer_bytes(&pointer_bytes)?;
        pointer_for_log = Some(pointer.clone());
        pointer.validate()?;
        let loaded = load_manifest_selected_by_pointer(data_dir, &pointer)?;
        manifest_for_log = Some(loaded.manifest.clone());
        if let Some(expected) = expected_corpus
            && expected != &loaded.manifest.corpus
        {
            return Err(SemanticGenerationError::StaleCorpus {
                expected: corpus_identity_sha256(expected)?,
                actual: corpus_identity_sha256(&loaded.manifest.corpus)?,
            });
        }
        let artifact_paths = loaded
            .manifest
            .validate_artifacts_on_disk(data_dir, false)?;

        tracing::info!(
            event = "semantic.manifest.load",
            schema_version = loaded.manifest.schema_version,
            generation = %fingerprint_prefix(&loaded.manifest.generation_id),
            build = %fingerprint_prefix(&loaded.manifest.build_id),
            request = %optional_fingerprint_prefix(loaded.manifest.request_id.as_deref()),
            requested_topology = ?loaded.manifest.requested_topology,
            realized_topology = ?loaded.manifest.realized_topology,
            manifest_sha256 = %fingerprint_prefix(&pointer.manifest_sha256),
            selection_epoch = pointer.selection_epoch,
            selected_document_count = loaded.manifest.selected_document_count,
            total_vector_slot_count_across_tiers =
                loaded.manifest.total_vector_slot_count_across_tiers,
            artifact_count = loaded.manifest.artifacts.len(),
            elapsed_ms = elapsed_ms(started),
            "loaded and verified current semantic generation"
        );

        Ok(ValidatedSemanticGeneration {
            pointer,
            generation_dir: loaded.generation_dir,
            manifest: loaded.manifest,
            artifact_paths,
        })
    })();

    if let Err(error) = &result {
        log_generation_validation_failure(
            pointer_for_log.as_ref(),
            manifest_for_log.as_ref(),
            error,
            started,
        );
    }
    result
}

/// Resolve the coarse authoritative semantic state without guessing artifact
/// identity from legacy filenames.
///
/// A selected pointer receives the full fail-closed load path. Without a
/// pointer, orphaned private pointer-publication tempfiles are reported as
/// `InterruptedPublish`, staging directories as `Building`, unselected
/// generation directories as `PointerMissingWithGenerationDirectories`, and
/// unrecognized root entries as `LegacyUnidentified`. A generation directory
/// is only an unvalidated candidate at this stage; this coarse resolver never
/// calls it rollback-safe. Suspicious tempfile lookalikes remain
/// legacy/unidentified rather than receiving the private-file classification.
pub fn resolve_semantic_generation_disk_state(
    data_dir: &Path,
) -> Result<SemanticGenerationDiskStateV1, SemanticGenerationError> {
    let pointer_path = SemanticCurrentPointerV1::path(data_dir);
    match fs::symlink_metadata(&pointer_path) {
        Ok(_) => {
            let loaded = load_current_semantic_generation(data_dir, None)?;
            return Ok(SemanticGenerationDiskStateV1::Ready {
                generation_id: loaded.pointer.generation_id,
                manifest_sha256: loaded.pointer.manifest_sha256,
                selection_epoch: loaded.pointer.selection_epoch,
                selected_at_ms: loaded.pointer.selected_at_ms,
                requested_topology: loaded.manifest.requested_topology,
                realized_topology: loaded.manifest.realized_topology,
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(SemanticGenerationError::PointerIo {
                source: error.to_string(),
            });
        }
    }

    let root = semantic_vector_root(data_dir);
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SemanticGenerationDiskStateV1::Absent);
        }
        Err(error) => {
            return Err(SemanticGenerationError::PointerIo {
                source: error.to_string(),
            });
        }
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() => {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: "semantic vector root is not a safe directory".to_owned(),
            });
        }
        Ok(_) => {}
    }

    let staging = root.join(SEMANTIC_STAGING_DIRNAME);
    let build_count = count_safe_directory_entries(&staging, "semantic staging root", false)?;
    let generations = root.join(SEMANTIC_GENERATIONS_DIRNAME);
    let unselected_generation_directory_count =
        count_safe_directory_entries(&generations, "semantic generations root", true)?;
    let mut temporary_entry_count = 0_u64;
    let mut legacy_artifact_count = 0_u64;
    for entry in fs::read_dir(&root).map_err(|error| SemanticGenerationError::PointerIo {
        source: error.to_string(),
    })? {
        let entry = entry.map_err(|error| SemanticGenerationError::PointerIo {
            source: error.to_string(),
        })?;
        let name = entry.file_name();
        if name == OsStr::new(SEMANTIC_CURRENT_POINTER_LOCK_FILENAME)
            || name == OsStr::new(SEMANTIC_GENERATIONS_DIRNAME)
            || name == OsStr::new(SEMANTIC_STAGING_DIRNAME)
        {
            continue;
        }
        if is_private_pointer_publish_temp_name(&name) {
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
                SemanticGenerationError::PointerIo {
                    source: error.to_string(),
                }
            })?;
            if !metadata_is_link_or_reparse(&metadata) && metadata.is_file() {
                temporary_entry_count = temporary_entry_count.saturating_add(1);
                continue;
            }
        }
        legacy_artifact_count = legacy_artifact_count.saturating_add(1);
    }

    if temporary_entry_count > 0 {
        return Ok(SemanticGenerationDiskStateV1::InterruptedPublish {
            temporary_entry_count,
            build_count,
            unselected_generation_directory_count,
            legacy_artifact_count,
        });
    }
    if build_count > 0 {
        return Ok(SemanticGenerationDiskStateV1::Building {
            build_count,
            unselected_generation_directory_count,
            legacy_artifact_count,
        });
    }
    if unselected_generation_directory_count > 0 {
        return Ok(
            SemanticGenerationDiskStateV1::PointerMissingWithGenerationDirectories {
                generation_directory_count: unselected_generation_directory_count,
                legacy_artifact_count,
            },
        );
    }
    if legacy_artifact_count > 0 {
        return Ok(SemanticGenerationDiskStateV1::LegacyUnidentified {
            artifact_count: legacy_artifact_count,
        });
    }
    Ok(SemanticGenerationDiskStateV1::Absent)
}

fn is_private_pointer_publish_temp_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(suffix) = name.strip_prefix(SEMANTIC_CURRENT_POINTER_TEMP_PREFIX) else {
        return false;
    };
    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn count_safe_directory_entries(
    path: &Path,
    root_description: &'static str,
    entries_must_be_directories: bool,
) -> Result<u64, SemanticGenerationError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(SemanticGenerationError::PointerIo {
                source: error.to_string(),
            });
        }
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() => {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: format!("{root_description} is not a safe directory"),
            });
        }
        Ok(_) => {}
    }
    let mut count = 0_u64;
    for entry in fs::read_dir(path).map_err(|error| SemanticGenerationError::PointerIo {
        source: error.to_string(),
    })? {
        let entry = entry.map_err(|error| SemanticGenerationError::PointerIo {
            source: error.to_string(),
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            SemanticGenerationError::PointerIo {
                source: error.to_string(),
            }
        })?;
        if metadata_is_link_or_reparse(&metadata) {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: format!("{root_description} contains a symlink or reparse point"),
            });
        }
        if entries_must_be_directories && !metadata.is_dir() {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: format!("{root_description} contains a non-directory entry"),
            });
        }
        count = count.saturating_add(1);
    }
    Ok(count)
}

struct LoadedManifest {
    manifest: SemanticGenerationManifestV1,
    generation_dir: PathBuf,
}

fn load_manifest_selected_by_pointer(
    data_dir: &Path,
    pointer: &SemanticCurrentPointerV1,
) -> Result<LoadedManifest, SemanticGenerationError> {
    pointer.validate()?;
    let generation_dir = semantic_generations_root(data_dir).join(&pointer.generation_id);
    validate_existing_generation_directory(data_dir, &generation_dir)?;
    let manifest_path = generation_dir.join(SEMANTIC_GENERATION_MANIFEST_FILENAME);
    match fs::symlink_metadata(&manifest_path) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) => {
            return Err(SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::Path,
                reason: "generation manifest must not be a symlink or reparse point".to_owned(),
            });
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::Path,
                reason: "generation manifest is not a regular file".to_owned(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SemanticGenerationError::MissingManifest {
                generation_id: pointer.generation_id.clone(),
            });
        }
        Err(error) => {
            return Err(SemanticGenerationError::ManifestIo {
                generation_id: pointer.generation_id.clone(),
                source: error.to_string(),
            });
        }
    }
    let manifest = load_manifest_file(&manifest_path, &pointer.generation_id)?;
    manifest.validate()?;
    let actual_digest = manifest.sha256()?;
    if actual_digest != pointer.manifest_sha256 {
        return Err(SemanticGenerationError::ManifestDigestMismatch {
            expected: pointer.manifest_sha256.clone(),
            actual: actual_digest,
        });
    }
    if manifest.generation_id != pointer.generation_id {
        return invalid_manifest(
            SemanticManifestInvariantClass::Identity,
            "pointer generation ID and manifest generation ID differ",
        );
    }
    if pointer.selected_at_ms < manifest.sealed_at_ms {
        return invalid_manifest(
            SemanticManifestInvariantClass::Timestamp,
            "pointer selection predates immutable manifest seal",
        );
    }
    Ok(LoadedManifest {
        manifest,
        generation_dir,
    })
}

fn load_manifest_file(
    manifest_path: &Path,
    generation_id: &str,
) -> Result<SemanticGenerationManifestV1, SemanticGenerationError> {
    let bytes = read_bounded_file(manifest_path, MAX_SEMANTIC_MANIFEST_BYTES).map_err(|error| {
        if error.kind() == std::io::ErrorKind::InvalidData {
            SemanticGenerationError::ManifestParse {
                generation_id: generation_id.to_owned(),
                source: error.to_string(),
            }
        } else {
            SemanticGenerationError::ManifestIo {
                generation_id: generation_id.to_owned(),
                source: error.to_string(),
            }
        }
    })?;
    parse_generation_manifest_bytes(&bytes, generation_id)
}

fn parse_current_pointer_bytes(
    bytes: &[u8],
) -> Result<SemanticCurrentPointerV1, SemanticGenerationError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| SemanticGenerationError::PointerParse {
            source: error.to_string(),
        })?;
    let schema_version = json_schema_version(&value)
        .map_err(|source| SemanticGenerationError::PointerParse { source })?;
    let supported = u64::from(SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION);
    if schema_version > supported {
        return Err(SemanticGenerationError::UnsupportedPointerSchema {
            found: schema_version,
            supported,
        });
    }
    if schema_version < supported {
        return Err(SemanticGenerationError::InvalidPointer {
            reason: format!(
                "unsupported historical pointer schema {schema_version}; expected {supported}"
            ),
        });
    }
    serde_json::from_value(value).map_err(|error| SemanticGenerationError::PointerParse {
        source: error.to_string(),
    })
}

fn parse_generation_manifest_bytes(
    bytes: &[u8],
    generation_id: &str,
) -> Result<SemanticGenerationManifestV1, SemanticGenerationError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| SemanticGenerationError::ManifestParse {
            generation_id: generation_id.to_owned(),
            source: error.to_string(),
        })?;
    let schema_version =
        json_schema_version(&value).map_err(|source| SemanticGenerationError::ManifestParse {
            generation_id: generation_id.to_owned(),
            source,
        })?;
    let supported = u64::from(SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION);
    if schema_version > supported {
        return Err(SemanticGenerationError::UnsupportedManifestSchema {
            found: schema_version,
            supported,
        });
    }
    if schema_version < supported {
        return invalid_manifest(
            SemanticManifestInvariantClass::HistoricalSchema,
            &format!(
                "unsupported historical generation schema {schema_version}; expected {supported}"
            ),
        );
    }
    preflight_embedding_identity_schemas(&value)?;
    serde_json::from_value(value).map_err(|error| SemanticGenerationError::ManifestParse {
        generation_id: generation_id.to_owned(),
        source: error.to_string(),
    })
}

fn json_schema_version(value: &serde_json::Value) -> Result<u64, String> {
    value
        .as_object()
        .and_then(|object| object.get("schema_version"))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "schema_version must be an unsigned integer".to_owned())
}

fn preflight_embedding_identity_schemas(
    manifest: &serde_json::Value,
) -> Result<(), SemanticGenerationError> {
    let Some(artifacts) = manifest
        .as_object()
        .and_then(|object| object.get("artifacts"))
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(());
    };
    let components = [
        ("space", u64::from(EMBEDDING_SPACE_IDENTITY_SCHEMA_V1)),
        (
            "producer",
            u64::from(EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1),
        ),
        ("input", u64::from(EMBEDDING_INPUT_CONTRACT_SCHEMA_V1)),
        ("storage", u64::from(VECTOR_STORAGE_IDENTITY_SCHEMA_V1)),
    ];
    for artifact in artifacts {
        let Some(identity) = artifact
            .as_object()
            .and_then(|object| object.get("embedding_identity"))
            .and_then(serde_json::Value::as_object)
            .and_then(|object| object.get("identity"))
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        for &(component, supported) in &components {
            let Some(found) = identity
                .get(component)
                .and_then(serde_json::Value::as_object)
                .and_then(|object| object.get("schema_version"))
                .and_then(serde_json::Value::as_u64)
            else {
                continue;
            };
            if found > supported {
                return Err(
                    SemanticGenerationError::UnsupportedEmbeddingIdentitySchema {
                        component: component.to_owned(),
                        found,
                        supported,
                    },
                );
            }
        }
    }
    Ok(())
}

struct SemanticPublishLock(fs::File);

impl Drop for SemanticPublishLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn open_semantic_publish_lock(
    vector_root: &Path,
    path: &Path,
) -> Result<fs::File, SemanticGenerationError> {
    reject_existing_path_links(vector_root, path).map_err(|error| {
        SemanticGenerationError::PublishLockIo {
            source: error.to_string(),
        }
    })?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| SemanticGenerationError::PublishLockIo {
            source: error.to_string(),
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| SemanticGenerationError::PublishLockIo {
            source: error.to_string(),
        })?;
    if !metadata.is_file() {
        return Err(SemanticGenerationError::PublishLockIo {
            source: "publication lock is not a regular file".to_owned(),
        });
    }
    Ok(file)
}

fn read_current_pointer_for_cas(
    data_dir: &Path,
) -> Result<Option<SemanticCurrentPointerV1>, SemanticGenerationError> {
    let path = SemanticCurrentPointerV1::path(data_dir);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) => {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: "current pointer must not be a symlink or reparse point".to_owned(),
            });
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(SemanticGenerationError::InvalidPointer {
                reason: "current pointer is not a regular file".to_owned(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(SemanticGenerationError::PointerIo {
                source: error.to_string(),
            });
        }
    }
    let bytes = read_bounded_file(&path, MAX_SEMANTIC_POINTER_BYTES).map_err(|error| {
        SemanticGenerationError::PointerIo {
            source: error.to_string(),
        }
    })?;
    let pointer = parse_current_pointer_bytes(&bytes)?;
    pointer.validate()?;
    Ok(Some(pointer))
}

fn verify_publish_precondition(
    expected: &SemanticExpectedCurrentV1,
    actual: Option<&SemanticCurrentPointerV1>,
) -> Result<(), SemanticGenerationError> {
    if let SemanticExpectedCurrentV1::Exact(pointer) = expected {
        pointer.validate()?;
    }
    let matches = match (expected, actual) {
        (SemanticExpectedCurrentV1::Absent, None) => true,
        (SemanticExpectedCurrentV1::Exact(expected), Some(actual)) => expected == actual,
        _ => false,
    };
    if matches {
        return Ok(());
    }
    Err(SemanticGenerationError::ConcurrentPublishConflict {
        expected: expected_pointer_log_value(expected),
        actual: actual_pointer_log_value(actual),
    })
}

fn expected_pointer_log_value(expected: &SemanticExpectedCurrentV1) -> String {
    match expected {
        SemanticExpectedCurrentV1::Absent => "[absent]".to_owned(),
        SemanticExpectedCurrentV1::Exact(pointer) => format!(
            "{}:{}#{}",
            fingerprint_prefix(&pointer.generation_id),
            fingerprint_prefix(&pointer.manifest_sha256),
            pointer.selection_epoch
        ),
    }
}

fn actual_pointer_log_value(actual: Option<&SemanticCurrentPointerV1>) -> String {
    actual.map_or_else(
        || "[absent]".to_owned(),
        |pointer| {
            format!(
                "{}:{}#{}",
                fingerprint_prefix(&pointer.generation_id),
                fingerprint_prefix(&pointer.manifest_sha256),
                pointer.selection_epoch
            )
        },
    )
}

fn required_vector_roles(
    topology: SemanticGenerationTopology,
) -> impl Iterator<Item = SemanticArtifactRole> {
    let roles = match topology {
        SemanticGenerationTopology::FastOnly => [Some(SemanticArtifactRole::FastVector), None],
        SemanticGenerationTopology::QualityOnly => {
            [Some(SemanticArtifactRole::QualityVector), None]
        }
        SemanticGenerationTopology::FullProgressive => [
            Some(SemanticArtifactRole::FastVector),
            Some(SemanticArtifactRole::QualityVector),
        ],
    };
    roles.into_iter().flatten()
}

fn validate_corpus_identity(
    corpus: &SemanticCorpusSnapshotIdentity,
) -> Result<(), SemanticGenerationError> {
    validate_identifier(
        SemanticManifestInvariantClass::Identity,
        "database_uuid",
        &corpus.database_uuid,
    )?;
    for (name, value) in [
        (
            "schema_contract_sha256",
            corpus.schema_contract_sha256.as_str(),
        ),
        (
            "content_selection_sha256",
            corpus.content_selection_sha256.as_str(),
        ),
        (
            "canonicalization_sha256",
            corpus.canonicalization_sha256.as_str(),
        ),
        ("chunking_sha256", corpus.chunking_sha256.as_str()),
        (
            "document_id_contract_sha256",
            corpus.document_id_contract_sha256.as_str(),
        ),
        ("config_sha256", corpus.config_sha256.as_str()),
        ("corpus_sha256", corpus.corpus_sha256.as_str()),
        (
            "ordered_live_docset_sha256",
            corpus.ordered_live_docset_sha256.as_str(),
        ),
        ("content_sha256", corpus.content_sha256.as_str()),
    ] {
        validate_sha256(name, value).map_err(|reason| {
            SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::Identity,
                reason,
            }
        })?;
    }
    if corpus.document_count == 0 {
        return invalid_manifest(
            SemanticManifestInvariantClass::Count,
            "corpus document_count must be positive",
        );
    }
    Ok(())
}

fn corpus_identity_sha256(
    corpus: &SemanticCorpusSnapshotIdentity,
) -> Result<String, SemanticGenerationError> {
    let mut encoder = SemanticCanonicalEncoder::new(b"cass.semantic-corpus-snapshot.v1");
    encode_corpus_identity(&mut encoder, corpus);
    Ok(sha256_hex(&encoder.finish()))
}

fn validate_producer_attestation(
    producer: &SemanticProducerAttestation,
) -> Result<(), SemanticGenerationError> {
    validate_git_commit("cass_git_commit", &producer.cass_git_commit)?;
    validate_git_commit(
        "frankensearch_git_commit",
        &producer.frankensearch_git_commit,
    )?;
    for (name, value) in [
        ("rust_toolchain", producer.rust_toolchain.as_str()),
        ("build_profile", producer.build_profile.as_str()),
    ] {
        validate_identifier(SemanticManifestInvariantClass::Provenance, name, value)?;
    }
    for (name, value) in [
        ("cargo_lock_sha256", producer.cargo_lock_sha256.as_str()),
        ("binary_elf_sha256", producer.binary_elf_sha256.as_str()),
    ] {
        validate_sha256(name, value).map_err(|reason| {
            SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::Provenance,
                reason,
            }
        })?;
    }
    if producer.enabled_features.len() > MAX_PRODUCER_FEATURES
        || producer.enabled_features.iter().any(|feature| {
            feature.is_empty()
                || feature.len() > MAX_PRODUCER_FEATURE_BYTES
                || feature.trim() != feature
                || feature.chars().any(char::is_whitespace)
        })
    {
        return invalid_manifest(
            SemanticManifestInvariantClass::Provenance,
            "producer feature names must be non-empty normalized tokens",
        );
    }
    if !producer
        .enabled_features
        .windows(2)
        .all(|window| matches!(window, [left, right] if left < right))
    {
        return invalid_manifest(
            SemanticManifestInvariantClass::Provenance,
            "producer features must be sorted and unique",
        );
    }
    Ok(())
}

fn validate_git_commit(name: &str, value: &str) -> Result<(), SemanticGenerationError> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return invalid_manifest(
            SemanticManifestInvariantClass::Provenance,
            &format!("{name} must be a full 40- or 64-character lowercase hexadecimal commit ID"),
        );
    }
    Ok(())
}

fn validate_generation_artifact(
    artifact: &SemanticGenerationArtifact,
    corpus: &SemanticCorpusSnapshotIdentity,
    created_at_ms: i64,
    completed_at_ms: i64,
    sealed_at_ms: i64,
) -> Result<(), SemanticGenerationError> {
    for (name, value) in [
        ("artifact_sha256", artifact.artifact_sha256.as_str()),
        (
            "artifact_parameters_sha256",
            artifact.artifact_parameters_sha256.as_str(),
        ),
        (
            "generation_corpus_sha256",
            artifact.generation_corpus_sha256.as_str(),
        ),
        (
            "covered_live_docset_sha256",
            artifact.covered_live_docset_sha256.as_str(),
        ),
        (
            "covered_content_sha256",
            artifact.covered_content_sha256.as_str(),
        ),
    ] {
        validate_sha256(name, value).map_err(|reason| {
            SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::Identity,
                reason,
            }
        })?;
    }
    validate_identifier(
        SemanticManifestInvariantClass::Identity,
        "artifact_format",
        &artifact.artifact_format,
    )?;
    validate_identifier(
        SemanticManifestInvariantClass::SealState,
        "validator_id",
        &artifact.validation.validator_id,
    )?;
    artifact.embedding_identity.validate().map_err(|error| {
        SemanticGenerationError::InvalidManifest {
            class: SemanticManifestInvariantClass::Identity,
            reason: format!("canonical Frankensearch embedding identity rejected: {error}"),
        }
    })?;
    if artifact.embedding_identity.identity.space.kind != EmbeddingSpaceKindV1::Semantic {
        return invalid_manifest(
            SemanticManifestInvariantClass::Identity,
            "semantic generations reject explicit hash-control embedding identities",
        );
    }
    let dimension = artifact.embedding_identity.identity.space.dimension;
    if dimension == 0 || dimension > MAX_EMBEDDING_DIMENSION {
        return invalid_manifest(
            SemanticManifestInvariantClass::Identity,
            "canonical embedding dimension must be 1..=65536",
        );
    }
    if artifact.role.is_vector() {
        if artifact.ann_base.is_some() {
            return invalid_manifest(
                SemanticManifestInvariantClass::AnnBinding,
                "vector artifacts must not carry ANN base bindings",
            );
        }
        if artifact.artifact_format != artifact.embedding_identity.identity.storage.format {
            return invalid_manifest(
                SemanticManifestInvariantClass::Identity,
                "vector artifact format must equal canonical Frankensearch storage identity",
            );
        }
    } else if artifact.ann_base.is_none() {
        return invalid_manifest(
            SemanticManifestInvariantClass::AnnBinding,
            "ANN artifacts require an exact base binding",
        );
    }
    if artifact.size_bytes == 0
        || artifact.selected_document_count == 0
        || artifact.covered_document_count == 0
        || artifact.vector_slot_count == 0
    {
        return invalid_manifest(
            SemanticManifestInvariantClass::Count,
            "sealed artifacts must have positive size, selected/covered document, and vector-slot counts",
        );
    }
    if artifact.selected_document_count != corpus.document_count {
        return invalid_manifest(
            SemanticManifestInvariantClass::Count,
            "artifact selected-document count must equal the generation corpus",
        );
    }
    if artifact.covered_document_count > artifact.selected_document_count
        || artifact.live_vector_count != artifact.covered_document_count
    {
        return invalid_manifest(
            SemanticManifestInvariantClass::PartialCoverage,
            "tier coverage must be within the selected corpus with exactly one live vector per covered document",
        );
    }
    if artifact
        .live_vector_count
        .checked_add(artifact.tombstone_vector_count)
        != Some(artifact.vector_slot_count)
    {
        return invalid_manifest(
            SemanticManifestInvariantClass::Count,
            "artifact live plus tombstone counts must equal physical vector-slot count",
        );
    }
    if artifact.wal_entry_count != 0 {
        return invalid_manifest(
            SemanticManifestInvariantClass::SealState,
            "immutable generation artifacts must be compacted and carry zero live WAL entries",
        );
    }
    if artifact.generation_corpus_sha256 != corpus.corpus_sha256 {
        return invalid_manifest(
            SemanticManifestInvariantClass::Identity,
            "artifact generation-corpus identity does not match the selected generation",
        );
    }
    let full_coverage = artifact.covered_document_count == artifact.selected_document_count;
    if full_coverage
        && (artifact.covered_live_docset_sha256 != corpus.ordered_live_docset_sha256
            || artifact.covered_content_sha256 != corpus.content_sha256)
    {
        return invalid_manifest(
            SemanticManifestInvariantClass::PartialCoverage,
            "full tier coverage must bind the generation's exact live-docset and content identities",
        );
    }
    if !full_coverage
        && (artifact.covered_live_docset_sha256 == corpus.ordered_live_docset_sha256
            || artifact.covered_content_sha256 == corpus.content_sha256)
    {
        return invalid_manifest(
            SemanticManifestInvariantClass::PartialCoverage,
            "partial tier coverage must carry distinct covered-subset docset and content identities",
        );
    }
    if let Some(binding) = artifact.ann_base.as_ref() {
        if !matches!(
            binding.base_role,
            SemanticArtifactRole::FastVector | SemanticArtifactRole::QualityVector
        ) {
            return invalid_manifest(
                SemanticManifestInvariantClass::AnnBinding,
                "ANN base role must identify a vector artifact",
            );
        }
        for (name, value) in [
            (
                "ann_base.base_artifact_sha256",
                binding.base_artifact_sha256.as_str(),
            ),
            (
                "ann_base.base_storage_fingerprint",
                binding.base_storage_fingerprint.as_str(),
            ),
            (
                "ann_base.base_artifact_parameters_sha256",
                binding.base_artifact_parameters_sha256.as_str(),
            ),
            (
                "ann_base.parameters_sha256",
                binding.parameters_sha256.as_str(),
            ),
        ] {
            validate_sha256(name, value).map_err(|reason| {
                SemanticGenerationError::InvalidManifest {
                    class: SemanticManifestInvariantClass::AnnBinding,
                    reason,
                }
            })?;
        }
        validate_identifier(
            SemanticManifestInvariantClass::AnnBinding,
            "ann_base.algorithm",
            &binding.algorithm,
        )?;
        validate_identifier(
            SemanticManifestInvariantClass::AnnBinding,
            "ann_base.metric",
            &binding.metric,
        )?;
        if binding.parameters_sha256 != artifact.artifact_parameters_sha256 {
            return invalid_manifest(
                SemanticManifestInvariantClass::AnnBinding,
                "ANN binding parameters do not match artifact parameters",
            );
        }
    }
    let evidence = &artifact.validation;
    if evidence.validated_at_ms < completed_at_ms
        || evidence.validated_at_ms < created_at_ms
        || evidence.validated_at_ms > sealed_at_ms
        || evidence.artifact_sha256 != artifact.artifact_sha256
        || evidence.size_bytes != artifact.size_bytes
        || evidence.selected_document_count != artifact.selected_document_count
        || evidence.covered_document_count != artifact.covered_document_count
        || evidence.vector_slot_count != artifact.vector_slot_count
        || evidence.live_vector_count != artifact.live_vector_count
        || evidence.tombstone_vector_count != artifact.tombstone_vector_count
        || evidence.wal_entry_count != artifact.wal_entry_count
        || evidence.generation_corpus_sha256 != artifact.generation_corpus_sha256
        || evidence.covered_live_docset_sha256 != artifact.covered_live_docset_sha256
        || evidence.covered_content_sha256 != artifact.covered_content_sha256
    {
        return invalid_manifest(
            SemanticManifestInvariantClass::SealState,
            "artifact validation evidence must postdate completion, predate sealing, and attest the descriptor exactly",
        );
    }
    Ok(())
}

fn validate_generation_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_GENERATION_ID_LENGTH
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        || !value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !portable_path_segment_is_unambiguous(value)
    {
        return Err(
            "generation_id must be a portable 1-128 ASCII [A-Za-z0-9._-] segment, start alphanumeric, and avoid reserved/trailing-dot aliases"
                .to_owned(),
        );
    }
    Ok(())
}

fn validate_identifier(
    class: SemanticManifestInvariantClass,
    name: &str,
    value: &str,
) -> Result<(), SemanticGenerationError> {
    if value.is_empty()
        || value.trim() != value
        || value.chars().any(char::is_control)
        || value.len() > 512
    {
        return invalid_manifest(
            class,
            &format!("{name} must be a non-empty normalized value of at most 512 bytes"),
        );
    }
    Ok(())
}

fn validate_sha256(name: &str, value: &str) -> Result<(), String> {
    if value.len() != SHA256_HEX_LENGTH
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "{name} must be 64 lowercase hexadecimal characters"
        ));
    }
    Ok(())
}

fn invalid_manifest<T>(
    class: SemanticManifestInvariantClass,
    reason: &str,
) -> Result<T, SemanticGenerationError> {
    Err(SemanticGenerationError::InvalidManifest {
        class,
        reason: reason.to_owned(),
    })
}

fn checked_sum(left: u64, right: u64, label: &str) -> Result<u64, SemanticGenerationError> {
    left.checked_add(right)
        .ok_or_else(|| SemanticGenerationError::InvalidManifest {
            class: SemanticManifestInvariantClass::Count,
            reason: format!("{label} overflow"),
        })
}

#[derive(Debug)]
struct SemanticCanonicalEncoder {
    bytes: Vec<u8>,
}

impl SemanticCanonicalEncoder {
    fn new(domain: &[u8]) -> Self {
        let mut encoder = Self { bytes: Vec::new() };
        encoder.bytes(domain);
        encoder
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn bytes(&mut self, value: &[u8]) {
        self.u64(u64::try_from(value.len()).unwrap_or(u64::MAX));
        self.bytes.extend_from_slice(value);
    }

    fn text(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn optional_text(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.text(value);
            }
            None => self.u8(0),
        }
    }

    fn len(&mut self, value: usize) {
        self.u64(u64::try_from(value).unwrap_or(u64::MAX));
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }
}

fn topology_tag(topology: SemanticGenerationTopology) -> u8 {
    match topology {
        SemanticGenerationTopology::FastOnly => 1,
        SemanticGenerationTopology::QualityOnly => 2,
        SemanticGenerationTopology::FullProgressive => 3,
    }
}

fn artifact_role_tag(role: SemanticArtifactRole) -> u8 {
    match role {
        SemanticArtifactRole::FastVector => 1,
        SemanticArtifactRole::QualityVector => 2,
        SemanticArtifactRole::FastAnn => 3,
        SemanticArtifactRole::QualityAnn => 4,
    }
}

fn encode_corpus_identity(
    encoder: &mut SemanticCanonicalEncoder,
    corpus: &SemanticCorpusSnapshotIdentity,
) {
    encoder.text(&corpus.database_uuid);
    encoder.u64(corpus.source_change_sequence);
    encoder.text(&corpus.schema_contract_sha256);
    encoder.text(&corpus.content_selection_sha256);
    encoder.text(&corpus.canonicalization_sha256);
    encoder.text(&corpus.chunking_sha256);
    encoder.text(&corpus.document_id_contract_sha256);
    encoder.text(&corpus.config_sha256);
    encoder.text(&corpus.corpus_sha256);
    encoder.text(&corpus.ordered_live_docset_sha256);
    encoder.text(&corpus.content_sha256);
    encoder.u64(corpus.document_count);
}

fn encode_producer_attestation(
    encoder: &mut SemanticCanonicalEncoder,
    producer: &SemanticProducerAttestation,
) {
    encoder.text(&producer.cass_git_commit);
    encoder.text(&producer.frankensearch_git_commit);
    encoder.text(&producer.cargo_lock_sha256);
    encoder.text(&producer.rust_toolchain);
    encoder.len(producer.enabled_features.len());
    for feature in &producer.enabled_features {
        encoder.text(feature);
    }
    encoder.text(&producer.build_profile);
    encoder.text(&producer.binary_elf_sha256);
}

fn encode_generation_artifact(
    encoder: &mut SemanticCanonicalEncoder,
    artifact: &SemanticGenerationArtifact,
) {
    encoder.u8(artifact_role_tag(artifact.role));
    encoder.text(&artifact.relative_path);
    encoder.text(&artifact.artifact_sha256);
    encoder.u64(artifact.size_bytes);
    encoder.text(&artifact.artifact_format);
    encoder.text(&artifact.artifact_parameters_sha256);
    encoder.text(&artifact.embedding_identity.fingerprint);
    match artifact.ann_base.as_ref() {
        Some(binding) => {
            encoder.u8(1);
            encoder.u8(artifact_role_tag(binding.base_role));
            encoder.text(&binding.base_artifact_sha256);
            encoder.text(&binding.base_storage_fingerprint);
            encoder.text(&binding.base_artifact_parameters_sha256);
            encoder.text(&binding.algorithm);
            encoder.text(&binding.metric);
            encoder.text(&binding.parameters_sha256);
        }
        None => encoder.u8(0),
    }
    encoder.u64(artifact.selected_document_count);
    encoder.u64(artifact.covered_document_count);
    encoder.u64(artifact.vector_slot_count);
    encoder.u64(artifact.live_vector_count);
    encoder.u64(artifact.tombstone_vector_count);
    encoder.u64(artifact.wal_entry_count);
    encoder.text(&artifact.generation_corpus_sha256);
    encoder.text(&artifact.covered_live_docset_sha256);
    encoder.text(&artifact.covered_content_sha256);
    let evidence = &artifact.validation;
    encoder.text(&evidence.validator_id);
    encoder.i64(evidence.validated_at_ms);
    encoder.text(&evidence.artifact_sha256);
    encoder.u64(evidence.size_bytes);
    encoder.u64(evidence.selected_document_count);
    encoder.u64(evidence.covered_document_count);
    encoder.u64(evidence.vector_slot_count);
    encoder.u64(evidence.live_vector_count);
    encoder.u64(evidence.tombstone_vector_count);
    encoder.u64(evidence.wal_entry_count);
    encoder.text(&evidence.generation_corpus_sha256);
    encoder.text(&evidence.covered_live_docset_sha256);
    encoder.text(&evidence.covered_content_sha256);
}

fn normalized_manifest_relative_path(recorded: &str) -> Result<PathBuf, String> {
    if recorded.is_empty()
        || recorded.len() > MAX_SEMANTIC_ARTIFACT_PATH_BYTES
        || !recorded.is_ascii()
        || recorded.trim() != recorded
        || recorded.contains('\\')
        || recorded.contains(':')
        || recorded.chars().any(char::is_control)
        || recorded == SEMANTIC_GENERATION_MANIFEST_FILENAME
    {
        return Err(
            "artifact path must be normalized portable ASCII, manifest-relative, and may not name manifest.json"
                .to_owned(),
        );
    }
    let path = Path::new(recorded);
    if path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(
            "artifact path contains an absolute, parent, root, or alias component".to_owned(),
        );
    }
    let normalized = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => segment.to_str(),
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    if normalized != recorded {
        return Err("artifact path contains repeated or trailing separators".to_owned());
    }
    for segment in normalized.split('/') {
        if !portable_path_segment_is_unambiguous(segment) {
            return Err(
                "artifact path contains a Windows-reserved, ADS, trailing-dot/space, or alias-prone segment"
                    .to_owned(),
            );
        }
    }
    Ok(path.to_path_buf())
}

fn portable_path_segment_is_unambiguous(segment: &str) -> bool {
    if segment.is_empty()
        || segment.ends_with('.')
        || segment.ends_with(' ')
        || segment
            .bytes()
            .any(|byte| matches!(byte, b'<' | b'>' | b'"' | b'|' | b'?' | b'*' | b':'))
    {
        return false;
    }
    let basename = segment
        .split_once('.')
        .map_or(segment, |(basename, _)| basename);
    let upper = basename.to_ascii_uppercase();
    !matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !(upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'))
}

fn semantic_vector_root(data_dir: &Path) -> PathBuf {
    data_dir.join("vector_index")
}

fn semantic_generations_root(data_dir: &Path) -> PathBuf {
    semantic_vector_root(data_dir).join(SEMANTIC_GENERATIONS_DIRNAME)
}

fn prepare_semantic_vector_root(
    data_dir: &Path,
    vector_root: &Path,
) -> Result<(), SemanticGenerationError> {
    ensure_directory_chain_without_links(data_dir, data_dir).map_err(|error| {
        SemanticGenerationError::PointerIo {
            source: error.to_string(),
        }
    })?;
    ensure_directory_chain_without_links(data_dir, vector_root).map_err(|error| {
        SemanticGenerationError::PointerIo {
            source: error.to_string(),
        }
    })?;
    let canonical_data =
        data_dir
            .canonicalize()
            .map_err(|error| SemanticGenerationError::PointerIo {
                source: error.to_string(),
            })?;
    let canonical_vector =
        vector_root
            .canonicalize()
            .map_err(|error| SemanticGenerationError::PointerIo {
                source: error.to_string(),
            })?;
    if !canonical_vector.starts_with(canonical_data) {
        return Err(SemanticGenerationError::InvalidPointer {
            reason: "semantic vector root escapes data directory".to_owned(),
        });
    }
    Ok(())
}

fn prepare_generation_directory(
    data_dir: &Path,
    generation_dir: &Path,
) -> Result<(), SemanticGenerationError> {
    let vector_root = semantic_vector_root(data_dir);
    prepare_semantic_vector_root(data_dir, &vector_root)?;
    let generations_root = semantic_generations_root(data_dir);
    let generation_id = generation_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("[invalid]")
        .to_owned();
    ensure_directory_chain_without_links(data_dir, &generations_root).map_err(|error| {
        SemanticGenerationError::ManifestIo {
            generation_id: generation_id.clone(),
            source: error.to_string(),
        }
    })?;
    ensure_directory_chain_without_links(data_dir, generation_dir).map_err(|error| {
        SemanticGenerationError::ManifestIo {
            generation_id,
            source: error.to_string(),
        }
    })?;
    validate_existing_generation_directory(data_dir, generation_dir)
}

fn validate_existing_generation_directory(
    data_dir: &Path,
    generation_dir: &Path,
) -> Result<(), SemanticGenerationError> {
    let generation_id = generation_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("[invalid]")
        .to_owned();
    reject_existing_path_links(data_dir, generation_dir).map_err(|error| {
        SemanticGenerationError::ManifestIo {
            generation_id: generation_id.clone(),
            source: error.to_string(),
        }
    })?;
    let metadata = match fs::symlink_metadata(generation_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SemanticGenerationError::MissingManifest { generation_id });
        }
        Err(error) => {
            return Err(SemanticGenerationError::ManifestIo {
                generation_id,
                source: error.to_string(),
            });
        }
    };
    if !metadata.is_dir() {
        return invalid_manifest(
            SemanticManifestInvariantClass::Path,
            "generation root is not a directory",
        );
    }
    let canonical_data =
        data_dir
            .canonicalize()
            .map_err(|error| SemanticGenerationError::ManifestIo {
                generation_id: generation_id.clone(),
                source: error.to_string(),
            })?;
    let canonical_generation =
        generation_dir
            .canonicalize()
            .map_err(|error| SemanticGenerationError::ManifestIo {
                generation_id: generation_id.clone(),
                source: error.to_string(),
            })?;
    if !canonical_generation.starts_with(canonical_data) {
        return invalid_manifest(
            SemanticManifestInvariantClass::Path,
            "generation directory escapes data directory",
        );
    }
    Ok(())
}

fn reject_symlink_entry(path: &Path, label: &str) -> Result<(), SemanticGenerationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) => invalid_manifest(
            SemanticManifestInvariantClass::Path,
            &format!("{label} must not be a symlink or reparse point"),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => invalid_manifest(
            SemanticManifestInvariantClass::Path,
            &format!("failed inspecting {label}: {error}"),
        ),
    }
}

/// Splits `path` into the trusted `base` prefix plus the (possibly empty)
/// relative remainder of plain `Normal` components below it.
///
/// The link checks in this module anchor at `base` instead of walking from
/// the filesystem root: components ABOVE the controlled root are not ours to
/// police and cannot legally be link-free everywhere (macOS keeps every
/// standard temp and data path under `/var` and `/tmp`, both of which are
/// symlinks into `/private`). The controlled region — `base` itself and
/// everything below it — is still fully link-checked.
fn relative_below_base<'p>(base: &Path, path: &'p Path) -> std::io::Result<&'p Path> {
    path.strip_prefix(base).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path escapes its trusted base directory",
        )
    })
}

fn verify_or_create_dir_component(current: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(current) {
        Ok(metadata) => {
            if metadata_is_link_or_reparse(&metadata) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "directory chain contains a symlink or reparse point",
                ));
            }
            if !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "directory chain contains a non-directory component",
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(current)?;
            let metadata = fs::symlink_metadata(current)?;
            if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "new directory component was replaced by an unsafe path entry",
                ));
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn ensure_directory_chain_without_links(base: &Path, path: &Path) -> std::io::Result<()> {
    let relative = relative_below_base(base, path)?;
    let mut current = base.to_path_buf();
    verify_or_create_dir_component(&current)?;
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "directory chain contains a non-normal path component",
            ));
        };
        current.push(part);
        verify_or_create_dir_component(&current)?;
    }
    Ok(())
}

fn reject_existing_path_links(base: &Path, path: &Path) -> std::io::Result<()> {
    let relative = relative_below_base(base, path)?;
    let mut current = base.to_path_buf();
    match fs::symlink_metadata(&current) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains a symlink or reparse point",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains a non-normal path component",
            ));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata_is_link_or_reparse(&metadata) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path contains a symlink or reparse point",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn sha256_reader(file: &fs::File) -> std::io::Result<String> {
    let mut file = file.try_clone()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PortableFileIdentity {
    volume: u64,
    file: u64,
    link_count: u64,
}

#[cfg(unix)]
fn portable_file_identity(
    _file: &fs::File,
    metadata: &fs::Metadata,
) -> std::io::Result<Option<PortableFileIdentity>> {
    use std::os::unix::fs::MetadataExt;

    Ok(Some(PortableFileIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
        link_count: metadata.nlink(),
    }))
}

#[cfg(windows)]
fn portable_file_identity(
    file: &fs::File,
    _metadata: &fs::Metadata,
) -> std::io::Result<Option<PortableFileIdentity>> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time_low: u32,
        creation_time_high: u32,
        last_access_time_low: u32,
        last_access_time_high: u32,
        last_write_time_low: u32,
        last_write_time_high: u32,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            file: *mut std::ffi::c_void,
            information: *mut ByHandleFileInformation,
        ) -> i32;
    }

    let mut information = MaybeUninit::<ByHandleFileInformation>::uninit();
    // SAFETY: `file` owns a valid Windows file handle, and `information`
    // points to writable storage of exactly the documented C layout.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), information.as_mut_ptr()) }
        == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a successful GetFileInformationByHandle call initializes every
    // field of BY_HANDLE_FILE_INFORMATION.
    let information = unsafe { information.assume_init() };
    Ok(Some(PortableFileIdentity {
        volume: u64::from(information.volume_serial_number),
        file: (u64::from(information.file_index_high) << 32)
            | u64::from(information.file_index_low),
        link_count: u64::from(information.number_of_links),
    }))
}

#[cfg(not(any(unix, windows)))]
fn portable_file_identity(
    _file: &fs::File,
    _metadata: &fs::Metadata,
) -> std::io::Result<Option<PortableFileIdentity>> {
    Ok(None)
}

fn verify_path_still_names_open_file(
    path: &Path,
    expected: Option<PortableFileIdentity>,
) -> std::io::Result<()> {
    let path_file = OpenOptions::new().read(true).open(path)?;
    let path_metadata = path_file.metadata()?;
    if !path_metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "validated artifact path no longer names a regular file",
        ));
    }
    let actual = portable_file_identity(&path_file, &path_metadata)?;
    if expected.is_some() && actual != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "validated artifact path changed file identity during inspection",
        ));
    }
    if actual.is_some_and(|identity| identity.link_count != 1) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "validated artifact acquired an external hard link during inspection",
        ));
    }
    Ok(())
}

fn read_bounded_file(path: &Path, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    let max_bytes_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    if metadata.len() > max_bytes_u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {max_bytes}-byte limit"),
        ));
    }
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    file.take(max_bytes_u64.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {max_bytes}-byte limit"),
        ));
    }
    Ok(bytes)
}

fn fingerprint_prefix(value: &str) -> &str {
    value.get(..value.len().min(12)).unwrap_or("[invalid]")
}

fn optional_fingerprint_prefix(value: Option<&str>) -> &str {
    value.map_or("[none]", fingerprint_prefix)
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

fn log_generation_validation_failure(
    pointer: Option<&SemanticCurrentPointerV1>,
    manifest: Option<&SemanticGenerationManifestV1>,
    error: &SemanticGenerationError,
    started: Instant,
) {
    let schema_version = pointer.map_or(0, |pointer| pointer.schema_version);
    let generation = pointer.map_or("[unresolved]", |pointer| {
        fingerprint_prefix(&pointer.generation_id)
    });
    let manifest_sha256 = pointer.map_or("[unresolved]", |pointer| {
        fingerprint_prefix(&pointer.manifest_sha256)
    });
    let build = manifest.map_or("[unresolved]", |manifest| {
        fingerprint_prefix(&manifest.build_id)
    });
    let request = manifest.map_or("[unresolved]", |manifest| {
        optional_fingerprint_prefix(manifest.request_id.as_deref())
    });
    let requested_topology = manifest.map(|manifest| manifest.requested_topology);
    let realized_topology = manifest.map(|manifest| manifest.realized_topology);
    let role = match error {
        SemanticGenerationError::ArtifactIo { role, .. }
        | SemanticGenerationError::ArtifactSizeMismatch { role, .. }
        | SemanticGenerationError::ArtifactDigestMismatch { role, .. }
        | SemanticGenerationError::EmbeddingIdentityMismatch { role, .. } => Some(*role),
        _ => None,
    };
    let artifact = manifest
        .and_then(|manifest| role.and_then(|role| manifest.artifact(role)))
        .map_or("[unresolved]", |artifact| artifact.relative_path.as_str());
    let tier = role.map_or("[unresolved]", SemanticArtifactRole::as_str);
    let coverage = manifest.map_or_else(
        || "[unresolved]".to_owned(),
        |manifest| {
            manifest
                .artifacts
                .iter()
                .filter(|artifact| artifact.role.is_vector())
                .map(|artifact| {
                    format!(
                        "{}:{}/{}",
                        artifact.role.as_str(),
                        artifact.covered_document_count,
                        artifact.selected_document_count
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        },
    );
    let (expected_identity, actual_identity) = match error {
        SemanticGenerationError::ManifestDigestMismatch { expected, actual }
        | SemanticGenerationError::StaleCorpus { expected, actual }
        | SemanticGenerationError::ArtifactDigestMismatch {
            expected, actual, ..
        }
        | SemanticGenerationError::EmbeddingIdentityMismatch {
            expected, actual, ..
        }
        | SemanticGenerationError::ConcurrentPublishConflict { expected, actual } => {
            (fingerprint_prefix(expected), fingerprint_prefix(actual))
        }
        _ => ("[unavailable]", "[unavailable]"),
    };
    tracing::warn!(
        event = "semantic.manifest.validate",
        schema_version,
        generation,
        build,
        request,
        requested_topology = ?requested_topology,
        realized_topology = ?realized_topology,
        tier,
        coverage,
        artifact,
        manifest_sha256,
        expected_identity,
        actual_identity,
        reason_code = error.reason_code(),
        recovery_action = ?error.recovery_action(),
        elapsed_ms = elapsed_ms(started),
        "semantic generation validation failed closed"
    );
}

// ─── Errors ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ManifestError {
    Io { path: PathBuf, source: String },
    Parse { path: PathBuf, source: String },
    Serialize { source: String },
    UnsupportedVersion { found: u32, max_supported: u32 },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "manifest I/O error at {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(f, "manifest parse error at {}: {source}", path.display())
            }
            Self::Serialize { source } => write!(f, "manifest serialization error: {source}"),
            Self::UnsupportedVersion {
                found,
                max_supported,
            } => write!(
                f,
                "manifest version {found} is newer than supported version {max_supported}"
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn unique_manifest_temp_path(path: &Path, attempt: u32, random: u64) -> PathBuf {
    static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(MANIFEST_FILENAME);
    let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(
        ".{file_name}.tmp.{attempt}.{}.{}.{random:016x}",
        now_ms(),
        nonce
    ))
}

fn create_unique_manifest_temp_file(path: &Path) -> std::io::Result<(PathBuf, fs::File)> {
    for attempt in 0..100 {
        let random = random_manifest_path_nonce()?;
        let tmp_path = unique_manifest_temp_path(path, attempt, random);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(file) => return Ok((tmp_path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "could not create a unique temporary manifest file for {} after 100 attempts",
            path.display()
        ),
    ))
}

fn random_manifest_path_nonce() -> std::io::Result<u64> {
    let mut random_bytes = [0u8; 8];
    SystemRandom::new()
        .fill(&mut random_bytes)
        .map_err(|_| std::io::Error::other("failed to generate manifest temp path nonce"))?;
    Ok(u64::from_le_bytes(random_bytes))
}

fn replace_file_from_temp(temp_path: &Path, final_path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        windows_move_file_write_through(temp_path, final_path, true)?;
        return OpenOptions::new()
            .read(true)
            .write(true)
            .open(final_path)?
            .sync_all();
    }
    #[cfg(not(windows))]
    {
        let temp = tempfile::TempPath::try_from_path(temp_path.to_path_buf())?;
        temp.persist(final_path).map_err(|error| error.error)?;
        sync_parent_directory(final_path)
    }
}

#[cfg(not(windows))]
fn persist_named_temp(
    temp: tempfile::NamedTempFile,
    final_path: &Path,
    replace_existing: bool,
) -> std::io::Result<fs::File> {
    if replace_existing {
        temp.persist(final_path).map_err(|error| error.error)
    } else {
        temp.persist_noclobber(final_path)
            .map_err(|error| error.error)
    }
}

#[cfg(windows)]
fn persist_named_temp(
    temp: tempfile::NamedTempFile,
    final_path: &Path,
    replace_existing: bool,
) -> std::io::Result<fs::File> {
    windows_move_file_write_through(temp.path(), final_path, replace_existing)?;
    Ok(temp.into_file())
}

#[cfg(windows)]
fn windows_move_file_write_through(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
        fn SetFileAttributesW(file_name: *const u16, file_attributes: u32) -> i32;
    }

    fn wide_path(path: &Path) -> std::io::Result<Vec<u16>> {
        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Windows path contains an interior NUL",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    let source = wide_path(source)?;
    let destination = wide_path(destination)?;
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace_existing {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    // SAFETY: both pointers reference NUL-terminated UTF-16 buffers that live
    // through each call. The flags are documented MoveFileExW values. The
    // source is a private same-directory temporary file and the destination
    // was validated before this helper is invoked.
    unsafe {
        if SetFileAttributesW(source.as_ptr(), FILE_ATTRIBUTE_NORMAL) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn replacement_path_entry_exists(path: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => Ok(false),
        Err(err) => Err(std::io::Error::new(
            err.kind(),
            format!(
                "failed inspecting semantic manifest replacement target {}: {err}",
                path.display()
            ),
        )),
    }
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    let directory = fs::File::open(path)?;
    directory.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    // Windows does not document FlushFileBuffers for directory handles.
    // Artifact files themselves are opened writable and flushed; every
    // manifest/current namespace transition goes through MoveFileExW with
    // MOVEFILE_WRITE_THROUGH in `persist_named_temp`.
    Ok(())
}

fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    sync_directory(parent)
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::policy::SemanticPolicy;
    use proptest::prelude::*;

    fn test_policy() -> SemanticPolicy {
        SemanticPolicy::compiled_defaults()
    }

    fn test_artifact(tier: TierKind, ready: bool) -> ArtifactRecord {
        ArtifactRecord {
            tier,
            embedder_id: match tier {
                TierKind::Fast => "fnv1a-384".to_owned(),
                TierKind::Quality => "minilm-384".to_owned(),
            },
            model_revision: "abc123".to_owned(),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_version: CHUNKING_STRATEGY_VERSION,
            dimension: 384,
            doc_count: 1000,
            conversation_count: 250,
            db_fingerprint: "fp-1234".to_owned(),
            index_path: format!(
                "vector_index/index-{}.fsvi",
                match tier {
                    TierKind::Fast => "fnv1a-384",
                    TierKind::Quality => "minilm-384",
                }
            ),
            size_bytes: 150_000,
            started_at_ms: 1_700_000_000_000,
            completed_at_ms: 1_700_000_060_000,
            ready,
        }
    }

    fn test_hnsw() -> HnswRecord {
        HnswRecord {
            base_tier: TierKind::Quality,
            embedder_id: "minilm-384".to_owned(),
            ef_search: 128,
            index_path: "vector_index/hnsw-minilm-384.chsw".to_owned(),
            size_bytes: 50_000,
            built_at_ms: 1_700_000_070_000,
            ready: true,
        }
    }

    fn test_shard(shard_index: u32, shard_count: u32, ready: bool) -> SemanticShardRecord {
        SemanticShardRecord {
            tier: TierKind::Fast,
            embedder_id: "fnv1a-384".to_owned(),
            model_revision: "hash".to_owned(),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_version: CHUNKING_STRATEGY_VERSION,
            dimension: 384,
            shard_index,
            shard_count,
            doc_count: 25,
            total_conversations: 10,
            db_fingerprint: "fp-sharded".to_owned(),
            index_path: format!("vector_index/shards/fast-fnv1a-384/shard-{shard_index:05}.fsvi"),
            quantization: "f16".to_owned(),
            mmap_ready: true,
            ann_index_path: None,
            ann_size_bytes: 0,
            ann_ready: false,
            size_bytes: 4096,
            started_at_ms: 1_700_000_080_000,
            completed_at_ms: 1_700_000_081_000,
            ready,
        }
    }

    fn test_checkpoint(tier: TierKind) -> BuildCheckpoint {
        BuildCheckpoint {
            tier,
            embedder_id: "minilm-384".to_owned(),
            last_offset: 500,
            docs_embedded: 3000,
            conversations_processed: 500,
            total_conversations: 1000,
            db_fingerprint: "fp-1234".to_owned(),
            schema_version: SEMANTIC_SCHEMA_VERSION,
            chunking_version: CHUNKING_STRATEGY_VERSION,
            saved_at_ms: 1_700_000_030_000,
            last_message_id: None,
            cursor_exhausted: false,
        }
    }

    fn test_sha256(label: &str) -> String {
        sha256_hex(label.as_bytes())
    }

    fn test_semantic_identity(
        label: &str,
        dimension: u32,
        storage_format: &str,
    ) -> FrozenEmbeddingIdentityBundleV1 {
        use frankensearch::core::{
            EMBEDDING_INPUT_CONTRACT_SCHEMA_V1, EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1,
            EMBEDDING_SPACE_IDENTITY_SCHEMA_V1, EmbeddingArtifactIdentityV1,
            EmbeddingIdentityBundleV1, EmbeddingInputContractV1, EmbeddingProducerAttestationV1,
            EmbeddingSpaceIdentityV1, GoldenVectorCertificateV1, QuantizationFormat,
            VECTOR_STORAGE_IDENTITY_SCHEMA_V1, VectorStorageIdentityV1,
        };

        let input = EmbeddingInputContractV1 {
            schema_version: EMBEDDING_INPUT_CONTRACT_SCHEMA_V1,
            canonicalization: "cass-canonicalization-v1".to_owned(),
            content_selection: "cass-selected-message-content-v1".to_owned(),
            chunking: "cass-chunking-v1".to_owned(),
            query_instruction: "query:".to_owned(),
            document_instruction: "document:".to_owned(),
            doc_id_semantics: "cass-stable-document-id-v1".to_owned(),
        };
        let mut space = EmbeddingSpaceIdentityV1 {
            schema_version: EMBEDDING_SPACE_IDENTITY_SCHEMA_V1,
            logical_model_id: format!("{label}-semantic-model"),
            immutable_revision: format!("{label}-revision-v1"),
            kind: EmbeddingSpaceKindV1::Semantic,
            artifact_manifest_fingerprint: test_sha256(&format!("{label}-model-manifest")),
            artifacts: vec![
                EmbeddingArtifactIdentityV1 {
                    role: "tokenizer".to_owned(),
                    sha256: test_sha256(&format!("{label}-tokenizer")),
                    size: 101,
                },
                EmbeddingArtifactIdentityV1 {
                    role: "weights".to_owned(),
                    sha256: test_sha256(&format!("{label}-weights")),
                    size: 10_001,
                },
            ],
            tokenizer_fingerprint: test_sha256(&format!("{label}-tokenizer-contract")),
            vocabulary_fingerprint: test_sha256(&format!("{label}-vocabulary")),
            model_config_fingerprint: test_sha256(&format!("{label}-config")),
            model_preprocessing: "lowercase=false;unicode=nfc".to_owned(),
            sequence_policy: "truncate=tail;max_tokens=512;padding=none".to_owned(),
            query_instruction: "query:".to_owned(),
            document_instruction: "document:".to_owned(),
            pooling: "mean-mask-aware".to_owned(),
            output_normalization: "l2-f32-v1".to_owned(),
            dimension,
            input_contract_fingerprint: input.fingerprint(),
            hash_control: None,
            projection: None,
        };
        let producer = EmbeddingProducerAttestationV1 {
            schema_version: EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1,
            backend: "frankensearch-native".to_owned(),
            implementation_revision: "implementation-v1".to_owned(),
            protocol_revision: "in-process-v1".to_owned(),
            numeric_profile: "deterministic-f32-v1".to_owned(),
            provenance_manifest_fingerprint: test_sha256(&format!("{label}-provenance-manifest")),
            space_fingerprint: space.fingerprint(),
            golden_vectors: GoldenVectorCertificateV1 {
                corpus_sha256: test_sha256(&format!("{label}-golden-corpus")),
                vectors_sha256: test_sha256(&format!("{label}-golden-vectors")),
                vector_count: 2,
                dimension,
            },
        };
        space.artifact_manifest_fingerprint = test_sha256(&format!("{label}-model-manifest"));
        EmbeddingIdentityBundleV1 {
            space,
            producer,
            input,
            storage: VectorStorageIdentityV1 {
                schema_version: VECTOR_STORAGE_IDENTITY_SCHEMA_V1,
                format: storage_format.to_owned(),
                quantization: QuantizationFormat::F16,
                endianness: "little-endian".to_owned(),
                vector_normalization: "l2-f32-v1".to_owned(),
                dimension,
            },
        }
        .freeze()
        .expect("test semantic identity must satisfy Frankensearch")
    }

    fn test_corpus_identity() -> SemanticCorpusSnapshotIdentity {
        SemanticCorpusSnapshotIdentity {
            database_uuid: "cass-test-db-uuid".to_owned(),
            source_change_sequence: 42,
            schema_contract_sha256: test_sha256("schema-contract"),
            content_selection_sha256: test_sha256("content-selection"),
            canonicalization_sha256: test_sha256("canonicalization"),
            chunking_sha256: test_sha256("chunking"),
            document_id_contract_sha256: test_sha256("document-id-contract"),
            config_sha256: test_sha256("config"),
            corpus_sha256: test_sha256("generation"),
            ordered_live_docset_sha256: test_sha256("ordered-docset"),
            content_sha256: test_sha256("content"),
            document_count: 3,
        }
    }

    fn test_producer_attestation() -> SemanticProducerAttestation {
        SemanticProducerAttestation {
            cass_git_commit: "a".repeat(40),
            frankensearch_git_commit: "b".repeat(40),
            cargo_lock_sha256: test_sha256("cargo-lock"),
            rust_toolchain: "stable-1.90.0".to_owned(),
            enabled_features: vec!["native".to_owned(), "semantic".to_owned()],
            build_profile: "release".to_owned(),
            binary_elf_sha256: test_sha256("binary"),
        }
    }

    fn test_generation_artifact(
        role: SemanticArtifactRole,
        relative_path: &str,
        bytes: &[u8],
        corpus: &SemanticCorpusSnapshotIdentity,
        covered_document_count: u64,
    ) -> SemanticGenerationArtifact {
        let size_bytes = u64::try_from(bytes.len()).expect("test artifact length fits u64");
        let quality = matches!(
            role,
            SemanticArtifactRole::QualityVector | SemanticArtifactRole::QualityAnn
        );
        let artifact_sha256 = sha256_hex(bytes);
        let embedding_identity = test_semantic_identity(
            if quality { "quality" } else { "fast" },
            if quality { 384 } else { 256 },
            "fsvi-v1",
        );
        let full_coverage = covered_document_count == corpus.document_count;
        let covered_live_docset_sha256 = if full_coverage {
            corpus.ordered_live_docset_sha256.clone()
        } else {
            test_sha256(if quality {
                "partial-quality-docset"
            } else {
                "partial-fast-docset"
            })
        };
        let covered_content_sha256 = if full_coverage {
            corpus.content_sha256.clone()
        } else {
            test_sha256(if quality {
                "partial-quality-content"
            } else {
                "partial-fast-content"
            })
        };
        let artifact_format = if role.is_vector() {
            "fsvi-v1"
        } else {
            "hnsw-v1"
        };
        let artifact_parameters_sha256 = test_sha256(match role {
            SemanticArtifactRole::FastVector => "fast-fsvi-parameters",
            SemanticArtifactRole::QualityVector => "quality-fsvi-parameters",
            SemanticArtifactRole::FastAnn => "fast-ann-parameters",
            SemanticArtifactRole::QualityAnn => "quality-ann-parameters",
        });
        let validation = SemanticArtifactValidationEvidence {
            validator_id: "frankensearch-fsvi-v1".to_owned(),
            validated_at_ms: 1_700_000_000_250,
            artifact_sha256: artifact_sha256.clone(),
            size_bytes,
            selected_document_count: corpus.document_count,
            covered_document_count,
            vector_slot_count: covered_document_count,
            live_vector_count: covered_document_count,
            tombstone_vector_count: 0,
            wal_entry_count: 0,
            generation_corpus_sha256: corpus.corpus_sha256.clone(),
            covered_live_docset_sha256: covered_live_docset_sha256.clone(),
            covered_content_sha256: covered_content_sha256.clone(),
        };
        SemanticGenerationArtifact {
            role,
            relative_path: relative_path.to_owned(),
            artifact_sha256,
            size_bytes,
            artifact_format: artifact_format.to_owned(),
            artifact_parameters_sha256,
            embedding_identity,
            ann_base: None,
            selected_document_count: corpus.document_count,
            covered_document_count,
            vector_slot_count: covered_document_count,
            live_vector_count: covered_document_count,
            tombstone_vector_count: 0,
            wal_entry_count: 0,
            generation_corpus_sha256: corpus.corpus_sha256.clone(),
            covered_live_docset_sha256,
            covered_content_sha256,
            validation,
        }
    }

    fn bind_ann_to_base(
        mut ann: SemanticGenerationArtifact,
        base: &SemanticGenerationArtifact,
    ) -> SemanticGenerationArtifact {
        ann.embedding_identity = base.embedding_identity.clone();
        ann.selected_document_count = base.selected_document_count;
        ann.covered_document_count = base.covered_document_count;
        ann.vector_slot_count = base.vector_slot_count;
        ann.live_vector_count = base.live_vector_count;
        ann.tombstone_vector_count = base.tombstone_vector_count;
        ann.generation_corpus_sha256 = base.generation_corpus_sha256.clone();
        ann.covered_live_docset_sha256 = base.covered_live_docset_sha256.clone();
        ann.covered_content_sha256 = base.covered_content_sha256.clone();
        ann.validation.selected_document_count = ann.selected_document_count;
        ann.validation.covered_document_count = ann.covered_document_count;
        ann.validation.vector_slot_count = ann.vector_slot_count;
        ann.validation.live_vector_count = ann.live_vector_count;
        ann.validation.tombstone_vector_count = ann.tombstone_vector_count;
        ann.validation.generation_corpus_sha256 = ann.generation_corpus_sha256.clone();
        ann.validation.covered_live_docset_sha256 = ann.covered_live_docset_sha256.clone();
        ann.validation.covered_content_sha256 = ann.covered_content_sha256.clone();
        ann.ann_base = Some(SemanticAnnBaseBindingV1 {
            base_role: base.role,
            base_artifact_sha256: base.artifact_sha256.clone(),
            base_storage_fingerprint: base.embedding_identity.identity.storage.fingerprint(),
            base_artifact_parameters_sha256: base.artifact_parameters_sha256.clone(),
            algorithm: "hnsw".to_owned(),
            metric: "cosine".to_owned(),
            parameters_sha256: ann.artifact_parameters_sha256.clone(),
        });
        ann
    }

    fn test_generation_manifest(
        realized_topology: SemanticGenerationTopology,
        artifacts: Vec<SemanticGenerationArtifact>,
    ) -> SemanticGenerationManifestV1 {
        let vector_slot_count = artifacts
            .iter()
            .filter(|artifact| artifact.role.is_vector())
            .map(|artifact| artifact.vector_slot_count)
            .sum();
        let live_vector_count = artifacts
            .iter()
            .filter(|artifact| artifact.role.is_vector())
            .map(|artifact| artifact.live_vector_count)
            .sum();
        let tombstone_vector_count = artifacts
            .iter()
            .filter(|artifact| artifact.role.is_vector())
            .map(|artifact| artifact.tombstone_vector_count)
            .sum();
        let wal_entry_count = artifacts
            .iter()
            .filter(|artifact| artifact.role.is_vector())
            .map(|artifact| artifact.wal_entry_count)
            .sum();
        SemanticGenerationManifestV1 {
            schema_version: SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION,
            generation_id: "gen-20260726-0001".to_owned(),
            build_id: "build-0001".to_owned(),
            request_id: Some("request-0001".to_owned()),
            previous_generation_id: Some("gen-20260725-0001".to_owned()),
            created_at_ms: 1_700_000_000_000,
            completed_at_ms: 1_700_000_000_200,
            sealed_at_ms: 1_700_000_000_300,
            requested_topology: SemanticGenerationTopology::FullProgressive,
            realized_topology,
            status: SemanticGenerationStatus::Sealed,
            corpus: test_corpus_identity(),
            producer: test_producer_attestation(),
            selected_document_count: 3,
            total_vector_slot_count_across_tiers: vector_slot_count,
            total_live_vector_count_across_tiers: live_vector_count,
            total_tombstone_vector_count_across_tiers: tombstone_vector_count,
            total_wal_entry_count_across_tiers: wal_entry_count,
            artifacts,
        }
    }

    fn write_generation_artifacts(
        data_dir: &Path,
        manifest: &SemanticGenerationManifestV1,
        contents: &BTreeMap<SemanticArtifactRole, Vec<u8>>,
    ) {
        let generation_dir = manifest.generation_dir(data_dir).unwrap();
        fs::create_dir_all(&generation_dir).unwrap();
        for artifact in &manifest.artifacts {
            let path = generation_dir.join(&artifact.relative_path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents.get(&artifact.role).unwrap()).unwrap();
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum ExpectedTierReadiness {
        Ready,
        Stale,
        Incompatible,
        Building(u8),
    }

    fn no_artifact_mutation(_: &mut ArtifactRecord) {}

    type TierReadinessCase = (
        &'static str,
        TierKind,
        bool,
        &'static str,
        &'static str,
        fn(&mut ArtifactRecord),
        ExpectedTierReadiness,
    );

    fn set_schema_version_to_zero(artifact: &mut ArtifactRecord) {
        artifact.schema_version = 0;
    }

    fn assert_tier_readiness(actual: TierReadiness, expected: ExpectedTierReadiness, label: &str) {
        match expected {
            ExpectedTierReadiness::Ready => {
                assert_eq!(actual, TierReadiness::Ready, "{label}");
            }
            ExpectedTierReadiness::Stale => {
                assert!(
                    matches!(actual, TierReadiness::Stale { .. }),
                    "{label}: {actual:?}"
                );
            }
            ExpectedTierReadiness::Incompatible => {
                assert!(
                    matches!(actual, TierReadiness::Incompatible { .. }),
                    "{label}: {actual:?}"
                );
            }
            ExpectedTierReadiness::Building(progress_pct) => {
                assert_eq!(actual, TierReadiness::Building { progress_pct }, "{label}");
            }
        }
    }

    // ── Manifest load/save round-trip ──────────────────────────────────

    #[test]
    fn manifest_round_trip_via_disk() {
        let temp = tempfile::tempdir().unwrap();
        let mut manifest = SemanticManifest {
            fast_tier: Some(test_artifact(TierKind::Fast, true)),
            quality_tier: Some(test_artifact(TierKind::Quality, true)),
            hnsw: Some(test_hnsw()),
            checkpoint: Some(test_checkpoint(TierKind::Quality)),
            backlog: BacklogLedger {
                total_conversations: 2000,
                fast_tier_processed: 1000,
                quality_tier_processed: 500,
                db_fingerprint: "fp-1234".to_owned(),
                computed_at_ms: 1_700_000_000_000,
            },
            ..Default::default()
        };

        manifest.save(temp.path()).unwrap();
        let loaded = SemanticManifest::load(temp.path()).unwrap().unwrap();

        assert_eq!(loaded.manifest_version, MANIFEST_FORMAT_VERSION);
        assert!(loaded.fast_tier.is_some());
        assert!(loaded.quality_tier.is_some());
        assert!(loaded.hnsw.is_some());
        assert!(loaded.checkpoint.is_some());
        assert_eq!(loaded.backlog.total_conversations, 2000);
        assert!(loaded.updated_at_ms > 0);
    }

    #[test]
    fn manifest_save_overwrites_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let mut first = SemanticManifest {
            fast_tier: Some(test_artifact(TierKind::Fast, true)),
            ..Default::default()
        };
        first.save(temp.path()).unwrap();

        let mut second = SemanticManifest {
            quality_tier: Some(test_artifact(TierKind::Quality, true)),
            backlog: BacklogLedger {
                total_conversations: 99,
                fast_tier_processed: 0,
                quality_tier_processed: 99,
                db_fingerprint: "fp-overwrite".to_owned(),
                computed_at_ms: 1_700_000_000_123,
            },
            ..Default::default()
        };
        second.save(temp.path()).unwrap();

        let loaded = SemanticManifest::load(temp.path()).unwrap().unwrap();
        assert!(loaded.fast_tier.is_none());
        assert!(loaded.quality_tier.is_some());
        assert_eq!(loaded.backlog.total_conversations, 99);
    }

    #[cfg(unix)]
    #[test]
    fn manifest_replacement_path_entry_exists_detects_dangling_symlink() -> Result<(), String> {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let link_path = SemanticManifest::path(temp.path());
        let manifest_dir = link_path
            .parent()
            .ok_or_else(|| "semantic manifest path should have a parent directory".to_string())?;
        fs::create_dir_all(manifest_dir).map_err(|e| e.to_string())?;
        let missing_target = manifest_dir.join("missing-semantic-manifest.json");

        symlink(&missing_target, &link_path).map_err(|e| e.to_string())?;

        if link_path.exists() {
            return Err("dangling manifest symlink unexpectedly resolved".to_string());
        }
        if !replacement_path_entry_exists(&link_path).map_err(|e| e.to_string())? {
            return Err(format!(
                "semantic manifest replacement entry check missed dangling symlink {}",
                link_path.display()
            ));
        }

        Ok(())
    }

    #[test]
    fn manifest_temp_file_creation_is_exclusive_and_unique() -> Result<(), String> {
        let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let final_path = SemanticManifest::path(temp.path());
        let manifest_dir = final_path
            .parent()
            .ok_or_else(|| "semantic manifest path should have a parent directory".to_string())?;
        fs::create_dir_all(manifest_dir).map_err(|e| e.to_string())?;

        let (first_path, mut first_file) =
            create_unique_manifest_temp_file(&final_path).map_err(|e| e.to_string())?;
        first_file.write_all(b"first").map_err(|e| e.to_string())?;
        let (second_path, mut second_file) =
            create_unique_manifest_temp_file(&final_path).map_err(|e| e.to_string())?;
        second_file
            .write_all(b"second")
            .map_err(|e| e.to_string())?;

        if first_path == second_path {
            return Err("exclusive temp creation reused the same path".to_string());
        }
        if !first_path.exists() {
            return Err(format!(
                "first temp file is missing: {}",
                first_path.display()
            ));
        }
        if !second_path.exists() {
            return Err(format!(
                "second temp file is missing: {}",
                second_path.display()
            ));
        }
        if first_path.parent() != Some(manifest_dir) {
            return Err(format!(
                "first temp path escaped manifest directory: {}",
                first_path.display()
            ));
        }
        if second_path.parent() != Some(manifest_dir) {
            return Err(format!(
                "second temp path escaped manifest directory: {}",
                second_path.display()
            ));
        }

        Ok(())
    }

    #[test]
    fn manifest_load_missing_returns_none() {
        let temp = tempfile::tempdir().unwrap();
        let loaded = SemanticManifest::load(temp.path()).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn manifest_load_or_default_returns_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = SemanticManifest::load_or_default(temp.path()).unwrap();
        assert_eq!(manifest.manifest_version, MANIFEST_FORMAT_VERSION);
        assert!(manifest.fast_tier.is_none());
        assert!(manifest.quality_tier.is_none());
    }

    #[test]
    fn manifest_load_corrupt_returns_parse_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = SemanticManifest::path(temp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not json").unwrap();

        let result = SemanticManifest::load(temp.path());
        assert!(matches!(result, Err(ManifestError::Parse { .. })));
    }

    #[test]
    fn manifest_load_future_version_returns_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = SemanticManifest::path(temp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        let manifest = SemanticManifest {
            manifest_version: MANIFEST_FORMAT_VERSION + 1,
            ..Default::default()
        };
        let json = serde_json::to_string(&manifest).unwrap();
        fs::write(&path, json).unwrap();

        let result = SemanticManifest::load(temp.path());
        assert!(matches!(
            result,
            Err(ManifestError::UnsupportedVersion { .. })
        ));
    }

    // ── Tier readiness (table-driven) ──────────────────────────────────

    #[test]
    fn tier_readiness_cases() {
        let policy = test_policy();
        let db_fp = "fp-1234";
        let model_rev = "abc123";
        let cases: &[TierReadinessCase] = &[
            (
                "ready artifact with matching fingerprint",
                TierKind::Fast,
                true,
                db_fp,
                model_rev,
                no_artifact_mutation,
                ExpectedTierReadiness::Ready,
            ),
            (
                "ready artifact with changed DB fingerprint",
                TierKind::Fast,
                true,
                "different-fp",
                model_rev,
                no_artifact_mutation,
                ExpectedTierReadiness::Stale,
            ),
            (
                "ready artifact with changed model revision",
                TierKind::Quality,
                true,
                db_fp,
                "new-revision",
                no_artifact_mutation,
                ExpectedTierReadiness::Stale,
            ),
            (
                "schema version mismatch",
                TierKind::Quality,
                true,
                db_fp,
                model_rev,
                set_schema_version_to_zero,
                ExpectedTierReadiness::Incompatible,
            ),
            (
                "not yet published artifact",
                TierKind::Fast,
                false,
                db_fp,
                model_rev,
                no_artifact_mutation,
                ExpectedTierReadiness::Building(100),
            ),
        ];

        for (label, tier, ready, current_db_fp, current_model_rev, mutate, expected) in cases {
            let mut artifact = test_artifact(*tier, *ready);
            mutate(&mut artifact);
            assert_tier_readiness(
                artifact.readiness(&policy, current_db_fp, current_model_rev),
                *expected,
                label,
            );
        }
    }

    // ── Manifest-level readiness ───────────────────────────────────────

    #[test]
    fn manifest_tier_readiness_missing() {
        let manifest = SemanticManifest::default();
        let policy = test_policy();
        assert_eq!(
            manifest.fast_tier_readiness(&policy, "fp", "rev"),
            TierReadiness::Missing,
        );
        assert_eq!(
            manifest.quality_tier_readiness(&policy, "fp", "rev"),
            TierReadiness::Missing,
        );
    }

    #[test]
    fn manifest_tier_readiness_with_checkpoint() {
        let manifest = SemanticManifest {
            checkpoint: Some(test_checkpoint(TierKind::Quality)),
            ..Default::default()
        };

        let policy = test_policy();
        // Fast tier has no checkpoint → Missing
        assert_eq!(
            manifest.fast_tier_readiness(&policy, "fp-1234", "rev"),
            TierReadiness::Missing,
        );
        // Quality tier has a valid checkpoint → Building
        assert!(matches!(
            manifest.quality_tier_readiness(&policy, "fp-1234", "rev"),
            TierReadiness::Building { progress_pct: 50 },
        ));
    }

    #[test]
    fn manifest_tier_readiness_checkpoint_invalid_db() {
        let manifest = SemanticManifest {
            checkpoint: Some(test_checkpoint(TierKind::Quality)),
            ..Default::default()
        };

        let policy = test_policy();
        // Checkpoint DB doesn't match → Missing (checkpoint invalid)
        assert_eq!(
            manifest.quality_tier_readiness(&policy, "other-fp", "rev"),
            TierReadiness::Missing,
        );
    }

    // ── Hybrid search check ────────────────────────────────────────────

    #[test]
    fn can_hybrid_search_requires_usable_fast_tier() {
        let policy = test_policy();
        let db_fp = "fp-1234";
        let rev = "abc123";

        // No fast tier → can't hybrid
        let manifest = SemanticManifest::default();
        assert!(!manifest.can_hybrid_search(&policy, db_fp, rev));

        // Fast tier present → can hybrid
        let manifest = SemanticManifest {
            fast_tier: Some(test_artifact(TierKind::Fast, true)),
            ..Default::default()
        };
        assert!(manifest.can_hybrid_search(&policy, db_fp, rev));
    }

    // ── Backlog ledger ─────────────────────────────────────────────────

    #[test]
    fn backlog_remaining_and_pending() {
        let ledger = BacklogLedger {
            total_conversations: 1000,
            fast_tier_processed: 800,
            quality_tier_processed: 300,
            db_fingerprint: "fp".to_owned(),
            computed_at_ms: 0,
        };

        assert_eq!(ledger.fast_tier_remaining(), 200);
        assert_eq!(ledger.quality_tier_remaining(), 700);
        assert!(ledger.has_pending_work());
        assert!(ledger.is_current("fp"));
        assert!(!ledger.is_current("other"));
    }

    #[test]
    fn backlog_no_pending_when_fully_processed() {
        let ledger = BacklogLedger {
            total_conversations: 500,
            fast_tier_processed: 500,
            quality_tier_processed: 500,
            db_fingerprint: "fp".to_owned(),
            computed_at_ms: 0,
        };

        assert_eq!(ledger.fast_tier_remaining(), 0);
        assert_eq!(ledger.quality_tier_remaining(), 0);
        assert!(!ledger.has_pending_work());
    }

    // ── Build checkpoint ───────────────────────────────────────────────

    #[test]
    fn checkpoint_progress_and_completion() -> Result<(), String> {
        let cp = test_checkpoint(TierKind::Quality);
        assert_eq!(cp.progress_pct(), 50);
        assert!(!cp.is_complete());
        assert!(cp.is_valid("fp-1234"));
        assert!(!cp.is_valid("other-fp"));

        // Complete checkpoint
        let mut cp = test_checkpoint(TierKind::Quality);
        cp.conversations_processed = 1000;
        assert_eq!(cp.progress_pct(), 99);
        if cp.is_complete() {
            return Err("count parity alone should not complete a checkpoint".to_string());
        }
        cp.cursor_exhausted = true;
        assert_eq!(cp.progress_pct(), 100);
        assert!(cp.is_complete());
        Ok(())
    }

    #[test]
    fn checkpoint_zero_total_gives_zero_pct() {
        let mut cp = test_checkpoint(TierKind::Fast);
        cp.total_conversations = 0;
        cp.conversations_processed = 0;
        assert_eq!(cp.progress_pct(), 0);
    }

    // ── Publish and clear ──────────────────────────────────────────────

    #[test]
    fn publish_artifact_clears_matching_checkpoint() {
        let mut manifest = SemanticManifest {
            checkpoint: Some(test_checkpoint(TierKind::Quality)),
            ..Default::default()
        };

        manifest.publish_artifact(test_artifact(TierKind::Quality, true));
        assert!(manifest.checkpoint.is_none());
        assert!(manifest.quality_tier.is_some());
    }

    #[test]
    fn publish_artifact_keeps_non_matching_checkpoint() {
        let mut manifest = SemanticManifest {
            checkpoint: Some(test_checkpoint(TierKind::Quality)),
            ..Default::default()
        };

        manifest.publish_artifact(test_artifact(TierKind::Fast, true));
        assert!(manifest.checkpoint.is_some()); // Quality checkpoint survives
        assert!(manifest.fast_tier.is_some());
    }

    // ── Refresh backlog ────────────────────────────────────────────────

    #[test]
    fn refresh_backlog_computes_from_ready_artifacts() {
        let mut manifest = SemanticManifest {
            fast_tier: Some(test_artifact(TierKind::Fast, true)),
            quality_tier: Some(test_artifact(TierKind::Quality, true)),
            ..Default::default()
        };

        manifest.refresh_backlog(2000, "fp-1234");
        assert_eq!(manifest.backlog.total_conversations, 2000);
        assert_eq!(manifest.backlog.fast_tier_processed, 250);
        assert_eq!(manifest.backlog.quality_tier_processed, 250);
    }

    #[test]
    fn refresh_backlog_ignores_stale_artifacts() {
        let mut manifest = SemanticManifest {
            fast_tier: Some(test_artifact(TierKind::Fast, true)),
            ..Default::default()
        };

        // DB fingerprint doesn't match → artifact not counted
        manifest.refresh_backlog(2000, "different-fp");
        assert_eq!(manifest.backlog.fast_tier_processed, 0);
    }

    // ── Invalidation ───────────────────────────────────────────────────

    #[test]
    fn invalidate_incompatible_removes_schema_mismatch() {
        let mut artifact = test_artifact(TierKind::Quality, true);
        artifact.schema_version = 0; // mismatch
        let mut manifest = SemanticManifest {
            quality_tier: Some(artifact),
            hnsw: Some(test_hnsw()), // depends on quality tier
            ..Default::default()
        };

        let policy = test_policy();
        let count = manifest.invalidate_incompatible(&policy, "abc123");

        assert_eq!(count, 2); // quality + hnsw
        assert!(manifest.quality_tier.is_none());
        assert!(manifest.hnsw.is_none());
    }

    #[test]
    fn invalidate_incompatible_keeps_compatible() {
        let mut manifest = SemanticManifest {
            fast_tier: Some(test_artifact(TierKind::Fast, true)),
            quality_tier: Some(test_artifact(TierKind::Quality, true)),
            ..Default::default()
        };

        let policy = test_policy();
        let count = manifest.invalidate_incompatible(&policy, "abc123");

        assert_eq!(count, 0);
        assert!(manifest.fast_tier.is_some());
        assert!(manifest.quality_tier.is_some());
    }

    // ── Legacy adoption ────────────────────────────────────────────────

    #[test]
    fn adopt_legacy_artifact() {
        let mut manifest = SemanticManifest::default();
        let doc_count = 500;
        let conversation_count = 125;
        let index_path = "vector_index/index-fnv1a-384.fsvi";
        let db_fingerprint = "fp-old";
        let size_bytes = 75_000;
        let adopted = manifest.adopt_legacy_artifact(
            TierKind::Fast,
            "fnv1a-384",
            "hash",
            384,
            doc_count,
            conversation_count,
            db_fingerprint,
            index_path,
            size_bytes,
        );

        assert!(adopted);
        let fast = manifest.fast_tier.as_ref().unwrap();
        assert_eq!(fast.embedder_id, "fnv1a-384");
        assert!(fast.ready);
        assert_eq!(fast.schema_version, SEMANTIC_SCHEMA_VERSION);
    }

    // ── Size accounting ────────────────────────────────────────────────

    #[test]
    fn total_size_accounts_for_all_artifacts() {
        let manifest = SemanticManifest {
            fast_tier: Some(test_artifact(TierKind::Fast, true)),
            quality_tier: Some(test_artifact(TierKind::Quality, true)),
            hnsw: Some(test_hnsw()),
            ..Default::default()
        };

        assert_eq!(manifest.total_size_bytes(), 150_000 + 150_000 + 50_000);
        assert_eq!(manifest.total_size_mb(), 1); // 350KB rounds up to 1MB
    }

    #[test]
    fn total_size_empty_is_zero() {
        let manifest = SemanticManifest::default();
        assert_eq!(manifest.total_size_bytes(), 0);
        assert_eq!(manifest.total_size_mb(), 0);
    }

    // ── JSON round-trip ────────────────────────────────────────────────

    #[test]
    fn manifest_json_round_trip() {
        let manifest = SemanticManifest {
            fast_tier: Some(test_artifact(TierKind::Fast, true)),
            quality_tier: Some(test_artifact(TierKind::Quality, true)),
            hnsw: Some(test_hnsw()),
            checkpoint: Some(test_checkpoint(TierKind::Quality)),
            ..Default::default()
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let deser: SemanticManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.fast_tier, manifest.fast_tier);
        assert_eq!(deser.quality_tier, manifest.quality_tier);
        assert_eq!(deser.hnsw, manifest.hnsw);
        assert_eq!(deser.checkpoint, manifest.checkpoint);
    }

    // ── Shard sidecar manifest ─────────────────────────────────────────

    #[test]
    fn shard_manifest_round_trip_via_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let mut shards = SemanticShardManifest::default();
        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![test_shard(1, 2, true), test_shard(0, 2, true)],
        );

        shards.save(temp.path()).unwrap();
        let loaded = SemanticShardManifest::load(temp.path()).unwrap().unwrap();

        assert_eq!(loaded.manifest_version, MANIFEST_FORMAT_VERSION);
        assert_eq!(loaded.shards.len(), 2);
        assert_eq!(loaded.shards[0].shard_index, 0);
        assert_eq!(loaded.shards[1].shard_index, 1);
        assert!(loaded.updated_at_ms > 0);
    }

    #[test]
    fn invalidating_embedder_shards_preserves_records_and_other_generations() {
        let hash_fast = test_shard(0, 1, true);
        let mut hash_quality = test_shard(0, 1, true);
        hash_quality.tier = TierKind::Quality;
        hash_quality.db_fingerprint = "fp-quality-hash".to_string();
        let mut minilm_quality = test_shard(0, 1, true);
        minilm_quality.tier = TierKind::Quality;
        minilm_quality.embedder_id = "minilm-384".to_string();
        minilm_quality.db_fingerprint = "fp-quality-minilm".to_string();
        let mut shards = SemanticShardManifest {
            shards: vec![hash_fast, hash_quality, minilm_quality],
            ..Default::default()
        };

        assert_eq!(shards.mark_shards_stale_for_embedder("fnv1a-384"), 2);
        assert_eq!(shards.shards.len(), 3);
        assert!(
            shards
                .shards
                .iter()
                .filter(|shard| shard.embedder_id == "fnv1a-384")
                .all(|shard| !shard.ready && !shard.ann_ready)
        );
        assert!(
            shards
                .shards
                .iter()
                .find(|shard| shard.embedder_id == "minilm-384")
                .is_some_and(|shard| shard.ready)
        );
    }

    #[test]
    fn shard_summary_requires_every_ready_shard() {
        let mut shards = SemanticShardManifest::default();
        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![test_shard(0, 3, true), test_shard(2, 3, true)],
        );

        let partial = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
        assert_eq!(partial.shard_count, 3);
        assert_eq!(partial.ready_shards, 2);
        assert!(!partial.complete);

        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![
                test_shard(0, 3, true),
                test_shard(1, 3, true),
                test_shard(2, 3, true),
            ],
        );

        let complete = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
        assert_eq!(complete.ready_shards, 3);
        assert!(complete.complete);
        assert_eq!(complete.doc_count, 75);
        assert_eq!(complete.total_conversations, 10);
    }

    #[test]
    fn shard_summary_rejects_non_mmap_ready_or_inconsistent_shards() {
        let mut non_mmap = test_shard(0, 1, true);
        non_mmap.mmap_ready = false;
        let mut shards = SemanticShardManifest::default();
        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![non_mmap],
        );

        let non_mmap_summary = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
        assert_eq!(non_mmap_summary.ready_shards, 0);
        assert!(!non_mmap_summary.complete);

        let mut inconsistent = test_shard(1, 3, true);
        inconsistent.ann_ready = true;
        inconsistent.ann_index_path = None;
        inconsistent.ann_size_bytes = 4096;
        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![test_shard(0, 2, true), inconsistent],
        );

        let inconsistent_summary = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
        assert_eq!(inconsistent_summary.shard_count, 3);
        assert_eq!(inconsistent_summary.ready_shards, 2);
        assert_eq!(inconsistent_summary.ann_ready_shards, 0);
        assert!(!inconsistent_summary.complete);

        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![
                test_shard(0, 2, true),
                test_shard(1, 2, true),
                test_shard(1, 2, false),
            ],
        );
        let duplicate_summary = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
        assert_eq!(duplicate_summary.shard_count, 2);
        assert_eq!(duplicate_summary.ready_shards, 2);
        assert!(
            !duplicate_summary.complete,
            "duplicate shard indexes must not summarize as a complete generation"
        );

        let mut duplicate_path = test_shard(1, 2, true);
        duplicate_path.index_path = test_shard(0, 2, true).index_path;
        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![test_shard(0, 2, true), duplicate_path],
        );
        let duplicate_path_summary = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
        assert_eq!(duplicate_path_summary.shard_count, 2);
        assert_eq!(duplicate_path_summary.ready_shards, 2);
        assert!(
            !duplicate_path_summary.complete,
            "duplicate shard index paths must not summarize as a complete generation"
        );

        let mut blank_path = test_shard(0, 1, true);
        blank_path.index_path.clear();
        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![blank_path],
        );
        let blank_path_summary = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
        assert_eq!(blank_path_summary.shard_count, 1);
        assert_eq!(blank_path_summary.ready_shards, 1);
        assert!(
            !blank_path_summary.complete,
            "blank shard index paths must not summarize as complete"
        );

        for unsafe_path in [
            tempfile::tempdir()
                .unwrap()
                .path()
                .join("outside.fsvi")
                .to_string_lossy()
                .to_string(),
            "vector_index/shards/../outside.fsvi".to_string(),
            "./vector_index/shards/fast/hash.fsvi".to_string(),
            " vector_index/shards/fast/hash.fsvi".to_string(),
        ] {
            let mut unsafe_shard = test_shard(0, 1, true);
            unsafe_shard.index_path = unsafe_path;
            shards.replace_shards_for_generation(
                TierKind::Fast,
                "fnv1a-384",
                "fp-sharded",
                vec![unsafe_shard],
            );
            let unsafe_path_summary = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
            assert_eq!(unsafe_path_summary.shard_count, 1);
            assert_eq!(unsafe_path_summary.ready_shards, 1);
            assert!(
                !unsafe_path_summary.complete,
                "unsafe shard index paths must not summarize as complete"
            );
        }

        let outside_ann_dir = tempfile::tempdir().unwrap();
        for unsafe_ann_path in [
            outside_ann_dir
                .path()
                .join("outside.chsw")
                .to_string_lossy()
                .to_string(),
            "vector_index/shards/../outside.chsw".to_string(),
            "./vector_index/shards/fast/hash.chsw".to_string(),
            " vector_index/shards/fast/hash.chsw".to_string(),
        ] {
            let mut unsafe_ann = test_shard(0, 1, true);
            unsafe_ann.ann_ready = true;
            unsafe_ann.ann_index_path = Some(unsafe_ann_path);
            unsafe_ann.ann_size_bytes = 4096;
            shards.replace_shards_for_generation(
                TierKind::Fast,
                "fnv1a-384",
                "fp-sharded",
                vec![unsafe_ann],
            );
            let unsafe_ann_summary = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
            assert_eq!(unsafe_ann_summary.shard_count, 1);
            assert_eq!(unsafe_ann_summary.ready_shards, 1);
            assert_eq!(unsafe_ann_summary.ann_ready_shards, 0);
            assert!(
                unsafe_ann_summary.complete,
                "unsafe optional ANN paths must not invalidate the vector shard generation"
            );
        }

        let mut duplicate_ann_path = test_shard(1, 2, true);
        duplicate_ann_path.ann_ready = true;
        duplicate_ann_path.ann_index_path =
            Some("vector_index/shards/fast-fnv1a-384/shared-ann.chsw".to_owned());
        duplicate_ann_path.ann_size_bytes = 4096;
        let mut first_ann_path = test_shard(0, 2, true);
        first_ann_path.ann_ready = true;
        first_ann_path.ann_index_path = duplicate_ann_path.ann_index_path.clone();
        first_ann_path.ann_size_bytes = 4096;
        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![first_ann_path, duplicate_ann_path],
        );
        let duplicate_ann_summary = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
        assert_eq!(duplicate_ann_summary.shard_count, 2);
        assert_eq!(duplicate_ann_summary.ready_shards, 2);
        assert_eq!(duplicate_ann_summary.ann_ready_shards, 1);
        assert!(
            duplicate_ann_summary.complete,
            "duplicate optional ANN paths must not invalidate the vector shard generation"
        );

        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![test_shard(2, 2, true)],
        );
        let out_of_range_summary = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
        assert_eq!(out_of_range_summary.shard_count, 2);
        assert_eq!(out_of_range_summary.ready_shards, 1);
        assert!(
            !out_of_range_summary.complete,
            "shard indexes outside the declared shard count are malformed"
        );

        let mut mismatched_metadata = test_shard(1, 2, true);
        mismatched_metadata.dimension = 768;
        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![test_shard(0, 2, true), mismatched_metadata],
        );
        let metadata_summary = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
        assert_eq!(metadata_summary.shard_count, 2);
        assert_eq!(metadata_summary.ready_shards, 2);
        assert!(
            !metadata_summary.complete,
            "complete shard generations require consistent shard metadata"
        );

        let mut stale_schema = test_shard(0, 1, true);
        stale_schema.schema_version = SEMANTIC_SCHEMA_VERSION.saturating_sub(1);
        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![stale_schema],
        );
        let stale_schema_summary = shards.summary(TierKind::Fast, "fnv1a-384", "fp-sharded");
        assert_eq!(stale_schema_summary.shard_count, 1);
        assert_eq!(stale_schema_summary.ready_shards, 1);
        assert!(
            !stale_schema_summary.complete,
            "stale schema shards must not summarize as complete"
        );
    }

    #[test]
    fn shard_sidecar_does_not_make_main_manifest_ready() {
        let mut shards = SemanticShardManifest::default();
        shards.replace_shards_for_generation(
            TierKind::Fast,
            "fnv1a-384",
            "fp-sharded",
            vec![test_shard(0, 1, true)],
        );
        assert!(
            shards
                .summary(TierKind::Fast, "fnv1a-384", "fp-sharded")
                .complete
        );

        let manifest = SemanticManifest::default();
        let policy = test_policy();
        assert_eq!(
            manifest.fast_tier_readiness(&policy, "fp-sharded", "hash"),
            TierReadiness::Missing,
            "sidecar shards must not publish runtime semantic readiness"
        );
    }

    #[test]
    fn shard_manifest_invalidates_incompatible_shards() {
        let mut bad = test_shard(0, 1, true);
        bad.schema_version = 0;
        let mut shards = SemanticShardManifest {
            shards: vec![bad, test_shard(0, 1, true)],
            ..Default::default()
        };

        let invalidated = shards.invalidate_incompatible(&test_policy(), "hash");

        assert_eq!(invalidated, 1);
        assert_eq!(shards.shards.len(), 1);
        assert_eq!(shards.total_size_bytes(), 4096);
    }

    // The pre-review tests below are retained verbatim as schema archaeology:
    // they describe the rejected shadow-identity/full-coverage-only draft and
    // intentionally do not compile into the test target. The active tests
    // following the module exercise the accepted canonical identity and
    // compare-and-swap contract.
    #[rustfmt::skip]
    #[cfg(any())]
    mod superseded_pre_review_generation_contract {
        use super::*;

    // ── Superseded immutable generation manifest and pointer ───────────

    fn mutated_generation(
        base: &SemanticGenerationManifestV1,
        mutate: impl FnOnce(&mut SemanticGenerationManifestV1),
    ) -> SemanticGenerationManifestV1 {
        let mut candidate = base.clone();
        mutate(&mut candidate);
        candidate
    }

    #[test]
    fn immutable_generation_canonical_serialization_and_authoritative_fields_are_hashed() {
        let corpus = test_corpus_identity();
        let artifact = test_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/vector.fsvi",
            b"fast-vector",
            &corpus,
        );
        let base = test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);
        base.validate().unwrap();
        let canonical = base.canonical_bytes().unwrap();
        assert_eq!(
            canonical,
            base.canonical_bytes().unwrap(),
            "canonical serialization must be byte deterministic"
        );
        let canonical_text = std::str::from_utf8(&canonical).unwrap();
        let ordered_root_keys = [
            "\"schema_version\"",
            "\"generation_id\"",
            "\"build_id\"",
            "\"request_id\"",
            "\"previous_generation_id\"",
            "\"created_at_ms\"",
            "\"completed_at_ms\"",
            "\"published_at_ms\"",
            "\"requested_topology\"",
            "\"realized_topology\"",
            "\"status\"",
            "\"corpus\"",
            "\"producer\"",
            "\"artifacts\"",
        ];
        let positions =
            ordered_root_keys.map(|key| canonical_text.find(key).expect("canonical key"));
        assert!(
            positions.windows(2).all(|window| window[0] < window[1]),
            "canonical root fields must remain in schema order: {positions:?}"
        );
        let ordered_embedding_keys = [
            "\"embedder_id\"",
            "\"embedder_revision\"",
            "\"dimension\"",
            "\"model_manifest_sha256\"",
            "\"model_files_sha256\"",
            "\"input_contract_sha256\"",
        ];
        let positions =
            ordered_embedding_keys.map(|key| canonical_text.find(key).expect("embedding key"));
        assert!(
            positions.windows(2).all(|window| window[0] < window[1]),
            "canonical embedding identity fields must remain in schema order: {positions:?}"
        );
        let decoded: SemanticGenerationManifestV1 = serde_json::from_slice(&canonical).unwrap();
        assert_eq!(decoded, base);

        let mutations: Vec<(&str, SemanticGenerationManifestV1)> = vec![
            (
                "schema_version",
                mutated_generation(&base, |value| value.schema_version += 1),
            ),
            (
                "generation_id",
                mutated_generation(&base, |value| value.generation_id.push('x')),
            ),
            (
                "build_id",
                mutated_generation(&base, |value| value.build_id.push('x')),
            ),
            (
                "request_id",
                mutated_generation(&base, |value| value.request_id = None),
            ),
            (
                "previous_generation_id",
                mutated_generation(&base, |value| value.previous_generation_id = None),
            ),
            (
                "created_at_ms",
                mutated_generation(&base, |value| value.created_at_ms += 1),
            ),
            (
                "completed_at_ms",
                mutated_generation(&base, |value| value.completed_at_ms += 1),
            ),
            (
                "published_at_ms",
                mutated_generation(&base, |value| value.published_at_ms += 1),
            ),
            (
                "requested_topology",
                mutated_generation(&base, |value| {
                    value.requested_topology = SemanticGenerationTopology::FastOnly;
                }),
            ),
            (
                "realized_topology",
                mutated_generation(&base, |value| {
                    value.realized_topology = SemanticGenerationTopology::QualityOnly;
                }),
            ),
            (
                "database_uuid",
                mutated_generation(&base, |value| value.corpus.database_uuid.push('x')),
            ),
            (
                "source_change_sequence",
                mutated_generation(&base, |value| value.corpus.source_change_sequence += 1),
            ),
            (
                "schema_contract_sha256",
                mutated_generation(&base, |value| {
                    value.corpus.schema_contract_sha256 = test_sha256("changed-schema");
                }),
            ),
            (
                "content_selection_sha256",
                mutated_generation(&base, |value| {
                    value.corpus.content_selection_sha256 = test_sha256("changed-selection");
                }),
            ),
            (
                "canonicalization_sha256",
                mutated_generation(&base, |value| {
                    value.corpus.canonicalization_sha256 = test_sha256("changed-canonicalization");
                }),
            ),
            (
                "chunking_sha256",
                mutated_generation(&base, |value| {
                    value.corpus.chunking_sha256 = test_sha256("changed-chunking");
                }),
            ),
            (
                "document_id_contract_sha256",
                mutated_generation(&base, |value| {
                    value.corpus.document_id_contract_sha256 = test_sha256("changed-document-id");
                }),
            ),
            (
                "config_sha256",
                mutated_generation(&base, |value| {
                    value.corpus.config_sha256 = test_sha256("changed-config");
                }),
            ),
            (
                "corpus_sha256",
                mutated_generation(&base, |value| {
                    value.corpus.corpus_sha256 = test_sha256("changed-corpus");
                }),
            ),
            (
                "ordered_live_docset_sha256",
                mutated_generation(&base, |value| {
                    value.corpus.ordered_live_docset_sha256 = test_sha256("changed-docset");
                }),
            ),
            (
                "content_sha256",
                mutated_generation(&base, |value| {
                    value.corpus.content_sha256 = test_sha256("changed-content");
                }),
            ),
            (
                "corpus_document_count",
                mutated_generation(&base, |value| value.corpus.document_count += 1),
            ),
            (
                "cass_git_commit",
                mutated_generation(&base, |value| value.producer.cass_git_commit.push('x')),
            ),
            (
                "frankensearch_git_commit",
                mutated_generation(&base, |value| {
                    value.producer.frankensearch_git_commit.push('x');
                }),
            ),
            (
                "cargo_lock_sha256",
                mutated_generation(&base, |value| {
                    value.producer.cargo_lock_sha256 = test_sha256("changed-lock");
                }),
            ),
            (
                "rust_toolchain",
                mutated_generation(&base, |value| value.producer.rust_toolchain.push('x')),
            ),
            (
                "enabled_features",
                mutated_generation(&base, |value| {
                    value.producer.enabled_features.push("zstd".to_owned());
                }),
            ),
            (
                "build_profile",
                mutated_generation(&base, |value| value.producer.build_profile.push('x')),
            ),
            (
                "binary_elf_sha256",
                mutated_generation(&base, |value| {
                    value.producer.binary_elf_sha256 = test_sha256("changed-binary");
                }),
            ),
            (
                "document_count",
                mutated_generation(&base, |value| value.document_count += 1),
            ),
            (
                "vector_count",
                mutated_generation(&base, |value| value.vector_count += 1),
            ),
            (
                "live_vector_count",
                mutated_generation(&base, |value| value.live_vector_count -= 1),
            ),
            (
                "tombstone_count",
                mutated_generation(&base, |value| value.tombstone_count += 1),
            ),
            (
                "wal_entry_count",
                mutated_generation(&base, |value| value.wal_entry_count += 1),
            ),
            (
                "artifact_role",
                mutated_generation(&base, |value| {
                    value.artifacts[0].role = SemanticArtifactRole::QualityVector;
                }),
            ),
            (
                "artifact_path",
                mutated_generation(&base, |value| {
                    value.artifacts[0].relative_path.push('x');
                }),
            ),
            (
                "artifact_sha256",
                mutated_generation(&base, |value| {
                    value.artifacts[0].artifact_sha256 = test_sha256("changed-artifact");
                }),
            ),
            (
                "artifact_size",
                mutated_generation(&base, |value| value.artifacts[0].size_bytes += 1),
            ),
            (
                "storage_format",
                mutated_generation(&base, |value| {
                    value.artifacts[0].storage_format.push('x');
                }),
            ),
            (
                "storage_format_version",
                mutated_generation(&base, |value| {
                    value.artifacts[0].storage_format_version += 1;
                }),
            ),
            (
                "quantization",
                mutated_generation(&base, |value| {
                    value.artifacts[0].quantization.push('x');
                }),
            ),
            (
                "embedder_id",
                mutated_generation(&base, |value| {
                    value.artifacts[0].embedding.embedder_id.push('x');
                }),
            ),
            (
                "embedder_revision",
                mutated_generation(&base, |value| {
                    value.artifacts[0].embedding.embedder_revision.push('x');
                }),
            ),
            (
                "dimension",
                mutated_generation(&base, |value| {
                    value.artifacts[0].embedding.dimension += 1;
                }),
            ),
            (
                "model_manifest_sha256",
                mutated_generation(&base, |value| {
                    value.artifacts[0].embedding.model_manifest_sha256 =
                        test_sha256("changed-model-manifest");
                }),
            ),
            (
                "model_files_sha256",
                mutated_generation(&base, |value| {
                    value.artifacts[0].embedding.model_files_sha256 =
                        test_sha256("changed-model-files");
                }),
            ),
            (
                "input_contract_sha256",
                mutated_generation(&base, |value| {
                    value.artifacts[0].embedding.input_contract_sha256 =
                        test_sha256("changed-input");
                }),
            ),
            (
                "artifact_document_count",
                mutated_generation(&base, |value| value.artifacts[0].document_count += 1),
            ),
            (
                "artifact_vector_count",
                mutated_generation(&base, |value| value.artifacts[0].vector_count += 1),
            ),
            (
                "artifact_live_count",
                mutated_generation(&base, |value| value.artifacts[0].live_vector_count -= 1),
            ),
            (
                "artifact_tombstone_count",
                mutated_generation(&base, |value| value.artifacts[0].tombstone_count += 1),
            ),
            (
                "artifact_wal_count",
                mutated_generation(&base, |value| value.artifacts[0].wal_entry_count += 1),
            ),
            (
                "coverage_numerator",
                mutated_generation(&base, |value| value.artifacts[0].coverage_numerator -= 1),
            ),
            (
                "coverage_denominator",
                mutated_generation(&base, |value| value.artifacts[0].coverage_denominator += 1),
            ),
            (
                "artifact_generation_sha256",
                mutated_generation(&base, |value| {
                    value.artifacts[0].generation_sha256 = test_sha256("changed-generation");
                }),
            ),
            (
                "artifact_docset_sha256",
                mutated_generation(&base, |value| {
                    value.artifacts[0].ordered_live_docset_sha256 =
                        test_sha256("changed-artifact-docset");
                }),
            ),
            (
                "artifact_content_sha256",
                mutated_generation(&base, |value| {
                    value.artifacts[0].content_sha256 = test_sha256("changed-artifact-content");
                }),
            ),
            (
                "validator_id",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.validator_id.push('x');
                }),
            ),
            (
                "validated_at_ms",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.validated_at_ms += 1;
                }),
            ),
            (
                "validation_artifact_sha256",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.artifact_sha256 =
                        test_sha256("changed-validation-artifact");
                }),
            ),
            (
                "validation_size",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.size_bytes += 1;
                }),
            ),
            (
                "validation_document_count",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.document_count += 1;
                }),
            ),
            (
                "validation_vector_count",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.vector_count += 1;
                }),
            ),
            (
                "validation_live_count",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.live_vector_count -= 1;
                }),
            ),
            (
                "validation_tombstone_count",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.tombstone_count += 1;
                }),
            ),
            (
                "validation_wal_count",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.wal_entry_count += 1;
                }),
            ),
            (
                "validation_generation_sha256",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.generation_sha256 =
                        test_sha256("changed-validation-generation");
                }),
            ),
            (
                "validation_docset_sha256",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.ordered_live_docset_sha256 =
                        test_sha256("changed-validation-docset");
                }),
            ),
            (
                "validation_content_sha256",
                mutated_generation(&base, |value| {
                    value.artifacts[0].validation.content_sha256 =
                        test_sha256("changed-validation-content");
                }),
            ),
        ];
        let base_digest = base.sha256().unwrap();
        for (label, mutation) in mutations {
            assert_ne!(
                mutation.sha256().unwrap(),
                base_digest,
                "authoritative field {label} must affect canonical digest"
            );
        }

        let mut json = serde_json::to_value(&base).unwrap();
        json["status"] = serde_json::Value::String("building".to_owned());
        assert!(
            serde_json::from_value::<SemanticGenerationManifestV1>(json).is_err(),
            "mutable/building status must not decode as an immutable generation"
        );
    }

    #[test]
    fn immutable_generation_validates_fast_quality_full_and_partial_topologies() {
        let corpus = test_corpus_identity();
        let fast = test_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/vector.fsvi",
            b"fast-vector",
            &corpus,
        );
        let quality = test_generation_artifact(
            SemanticArtifactRole::QualityVector,
            "quality/vector.fsvi",
            b"quality-vector",
            &corpus,
        );

        test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![fast.clone()])
            .validate()
            .unwrap();
        test_generation_manifest(
            SemanticGenerationTopology::QualityOnly,
            vec![quality.clone()],
        )
        .validate()
        .unwrap();
        test_generation_manifest(
            SemanticGenerationTopology::FullProgressive,
            vec![fast.clone(), quality.clone()],
        )
        .validate()
        .unwrap();

        let partial =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![fast.clone()]);
        assert_eq!(
            partial.requested_topology,
            SemanticGenerationTopology::FullProgressive
        );
        partial.validate().unwrap();

        let mut contradictory = partial.clone();
        contradictory.requested_topology = SemanticGenerationTopology::QualityOnly;
        assert!(matches!(
            contradictory.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));

        let mut missing = test_generation_manifest(
            SemanticGenerationTopology::FullProgressive,
            vec![fast.clone(), quality.clone()],
        );
        missing.artifacts.pop();
        missing.vector_count = fast.vector_count;
        missing.live_vector_count = fast.live_vector_count;
        assert!(matches!(
            missing.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));

        let duplicate = test_generation_manifest(
            SemanticGenerationTopology::FastOnly,
            vec![fast.clone(), fast],
        );
        assert!(matches!(
            duplicate.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));
    }

    #[test]
    fn immutable_generation_rejects_count_evidence_ann_and_path_inconsistency() {
        let corpus = test_corpus_identity();
        let fast_bytes = b"fast-vector";
        let fast = test_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/vector.fsvi",
            fast_bytes,
            &corpus,
        );
        let mut ann = test_generation_artifact(
            SemanticArtifactRole::FastAnn,
            "fast/vector.hnsw",
            b"fast-ann",
            &corpus,
        );
        let valid = test_generation_manifest(
            SemanticGenerationTopology::FastOnly,
            vec![fast.clone(), ann.clone()],
        );
        valid.validate().unwrap();

        let mut tombstoned = fast.clone();
        tombstoned.vector_count += 2;
        tombstoned.tombstone_count = 2;
        tombstoned.validation.vector_count = tombstoned.vector_count;
        tombstoned.validation.tombstone_count = tombstoned.tombstone_count;
        test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![tombstoned])
            .validate()
            .unwrap();

        ann.embedding.dimension += 1;
        ann.validation.artifact_sha256 = ann.artifact_sha256.clone();
        let bad_ann = test_generation_manifest(
            SemanticGenerationTopology::FastOnly,
            vec![fast.clone(), ann],
        );
        assert!(matches!(
            bad_ann.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));

        let mut bad_evidence = valid.clone();
        bad_evidence.artifacts[0].validation.vector_count += 1;
        assert!(matches!(
            bad_evidence.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));

        let mut bad_count = valid.clone();
        bad_count.vector_count += 1;
        assert!(matches!(
            bad_count.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));

        for unsafe_path in [
            "/absolute/vector.fsvi",
            "../escape.fsvi",
            "fast/../escape.fsvi",
            "./fast/vector.fsvi",
            "fast\\vector.fsvi",
            "fast//vector.fsvi",
            "fast/vector.fsvi/",
            "fast/\0vector.fsvi",
            " manifest.fsvi",
            "manifest.json",
        ] {
            let mut candidate = test_generation_manifest(
                SemanticGenerationTopology::FastOnly,
                vec![test_generation_artifact(
                    SemanticArtifactRole::FastVector,
                    unsafe_path,
                    fast_bytes,
                    &corpus,
                )],
            );
            candidate.artifacts[0].relative_path = unsafe_path.to_owned();
            assert!(
                matches!(
                    candidate.validate(),
                    Err(SemanticGenerationError::InvalidManifest { .. })
                ),
                "unsafe path must fail closed: {unsafe_path}"
            );
        }
    }

    #[test]
    fn immutable_generation_rejects_weak_provenance_and_embedding_identity() {
        let corpus = test_corpus_identity();
        let artifact = test_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/vector.fsvi",
            b"fast-vector",
            &corpus,
        );
        let base =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact.clone()]);

        let mut weak_commit = base.clone();
        weak_commit.producer.cass_git_commit = "main".to_owned();
        assert!(matches!(
            weak_commit.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));

        let mut unsorted_features = base.clone();
        unsorted_features.producer.enabled_features =
            vec!["semantic".to_owned(), "native".to_owned()];
        assert!(matches!(
            unsorted_features.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));

        let mut missing_model_identity = base.clone();
        missing_model_identity.artifacts[0]
            .embedding
            .model_files_sha256 = "unverified".to_owned();
        assert!(matches!(
            missing_model_identity.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));

        let mut impossible_dimension = base.clone();
        impossible_dimension.artifacts[0].embedding.dimension =
            MAX_EMBEDDING_DIMENSION.saturating_add(1);
        assert!(matches!(
            impossible_dimension.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));

        let mut wrong_format = base.clone();
        wrong_format.artifacts[0].storage_format = "hnsw".to_owned();
        assert!(matches!(
            wrong_format.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));

        let mut incomplete_live_set = artifact;
        incomplete_live_set.live_vector_count -= 1;
        incomplete_live_set.tombstone_count += 1;
        incomplete_live_set.validation.live_vector_count = incomplete_live_set.live_vector_count;
        incomplete_live_set.validation.tombstone_count = incomplete_live_set.tombstone_count;
        let incomplete = test_generation_manifest(
            SemanticGenerationTopology::FastOnly,
            vec![incomplete_live_set],
        );
        assert!(matches!(
            incomplete.validate(),
            Err(SemanticGenerationError::InvalidManifest { .. })
        ));
    }

    #[test]
    fn immutable_generation_publish_and_restart_load_use_only_pointer_and_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let corpus = test_corpus_identity();
        let fast_bytes = b"fast-vector".to_vec();
        let quality_bytes = b"quality-vector".to_vec();
        let fast = test_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/vector.fsvi",
            &fast_bytes,
            &corpus,
        );
        let quality = test_generation_artifact(
            SemanticArtifactRole::QualityVector,
            "quality/vector.fsvi",
            &quality_bytes,
            &corpus,
        );
        let manifest = test_generation_manifest(
            SemanticGenerationTopology::FullProgressive,
            vec![fast, quality],
        );
        let contents = BTreeMap::from([
            (SemanticArtifactRole::FastVector, fast_bytes),
            (SemanticArtifactRole::QualityVector, quality_bytes),
        ]);
        write_generation_artifacts(temp.path(), &manifest, &contents);

        let digest = manifest.write_immutable(temp.path()).unwrap();
        assert_eq!(
            manifest.write_immutable(temp.path()).unwrap(),
            digest,
            "identical immutable write retry must be idempotent"
        );
        let pointer = SemanticCurrentPointerV1::for_manifest(&manifest).unwrap();
        assert_eq!(pointer.manifest_sha256, digest);
        pointer.publish_atomic(temp.path()).unwrap();

        let loaded = load_current_semantic_generation(temp.path(), Some(&manifest.corpus)).unwrap();
        assert_eq!(loaded.pointer, pointer);
        assert_eq!(loaded.manifest, manifest);
        assert_eq!(loaded.artifact_paths.len(), 2);
        assert!(
            !temp.path().join("vector_index/vector.fast.idx").exists(),
            "resolution must not depend on a conventional filename"
        );

        let mut conflicting = manifest.clone();
        conflicting.build_id.push_str("-different");
        assert!(matches!(
            conflicting.write_immutable(temp.path()),
            Err(SemanticGenerationError::ImmutableManifestConflict { .. })
        ));
    }

    #[test]
    fn immutable_generation_resolution_distinguishes_missing_corrupt_future_and_stale() {
        let missing = tempfile::tempdir().unwrap();
        let error = load_current_semantic_generation(missing.path(), None).unwrap_err();
        assert!(matches!(error, SemanticGenerationError::MissingPointer));
        assert_eq!(
            error.recovery_action(),
            SemanticGenerationRecoveryAction::ReindexRequired
        );

        let corrupt = tempfile::tempdir().unwrap();
        let pointer_path = SemanticCurrentPointerV1::path(corrupt.path());
        fs::create_dir_all(pointer_path.parent().unwrap()).unwrap();
        fs::write(&pointer_path, b"not-json").unwrap();
        let error = load_current_semantic_generation(corrupt.path(), None).unwrap_err();
        assert!(matches!(
            error,
            SemanticGenerationError::PointerParse { .. }
        ));

        let future = tempfile::tempdir().unwrap();
        let pointer_path = SemanticCurrentPointerV1::path(future.path());
        fs::create_dir_all(pointer_path.parent().unwrap()).unwrap();
        let future_pointer = SemanticCurrentPointerV1 {
            schema_version: SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION + 1,
            generation_id: "gen-future".to_owned(),
            manifest_sha256: test_sha256("future"),
            published_at_ms: 1,
        };
        fs::write(&pointer_path, serde_json::to_vec(&future_pointer).unwrap()).unwrap();
        let error = load_current_semantic_generation(future.path(), None).unwrap_err();
        assert!(matches!(
            error,
            SemanticGenerationError::UnsupportedPointerSchema { .. }
        ));
        assert_eq!(
            error.recovery_action(),
            SemanticGenerationRecoveryAction::UpgradeRequired
        );

        let stale = tempfile::tempdir().unwrap();
        let corpus = test_corpus_identity();
        let bytes = b"fast-vector".to_vec();
        let artifact = test_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/vector.fsvi",
            &bytes,
            &corpus,
        );
        let manifest =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);
        write_generation_artifacts(
            stale.path(),
            &manifest,
            &BTreeMap::from([(SemanticArtifactRole::FastVector, bytes)]),
        );
        manifest.write_immutable(stale.path()).unwrap();
        SemanticCurrentPointerV1::for_manifest(&manifest)
            .unwrap()
            .publish_atomic(stale.path())
            .unwrap();
        let mut expected = manifest.corpus.clone();
        expected.source_change_sequence += 1;
        expected.corpus_sha256 = test_sha256("newer-corpus");
        let error = load_current_semantic_generation(stale.path(), Some(&expected)).unwrap_err();
        assert!(matches!(error, SemanticGenerationError::StaleCorpus { .. }));
    }

    #[test]
    fn immutable_generation_detects_manifest_artifact_hash_and_size_substitution() {
        let temp = tempfile::tempdir().unwrap();
        let corpus = test_corpus_identity();
        let bytes = b"fast-vector".to_vec();
        let artifact = test_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/vector.fsvi",
            &bytes,
            &corpus,
        );
        let manifest =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);
        write_generation_artifacts(
            temp.path(),
            &manifest,
            &BTreeMap::from([(SemanticArtifactRole::FastVector, bytes.clone())]),
        );
        manifest.write_immutable(temp.path()).unwrap();
        let pointer = SemanticCurrentPointerV1::for_manifest(&manifest).unwrap();
        pointer.publish_atomic(temp.path()).unwrap();

        let artifact_path = manifest
            .generation_dir(temp.path())
            .unwrap()
            .join("fast/vector.fsvi");
        fs::write(&artifact_path, b"same-length!").unwrap();
        let error = load_current_semantic_generation(temp.path(), None).unwrap_err();
        assert!(matches!(
            &error,
            SemanticGenerationError::ArtifactDigestMismatch { .. }
                | SemanticGenerationError::ArtifactSizeMismatch { .. }
        ));
        assert_eq!(
            error.recovery_action(),
            SemanticGenerationRecoveryAction::RollbackOrReindex
        );

        fs::write(&artifact_path, &bytes).unwrap();
        let manifest_path = manifest.manifest_path(temp.path()).unwrap();
        let mut manifest_bytes = fs::read(&manifest_path).unwrap();
        manifest_bytes.push(b' ');
        fs::write(&manifest_path, manifest_bytes).unwrap();
        let error = load_current_semantic_generation(temp.path(), None).unwrap_err();
        assert!(matches!(
            &error,
            SemanticGenerationError::ManifestDigestMismatch { .. }
        ));
        assert_eq!(
            error.recovery_action(),
            SemanticGenerationRecoveryAction::RollbackOrReindex
        );
    }

    #[test]
    fn immutable_pointer_publish_faults_never_expose_a_mixed_generation() {
        let checkpoints = [
            (
                PointerPublishCheckpoint::TemporaryBytesWritten,
                "gen-20260726-old",
            ),
            (
                PointerPublishCheckpoint::TemporaryFileSynced,
                "gen-20260726-old",
            ),
            (
                PointerPublishCheckpoint::PointerReplaced,
                "gen-20260726-new",
            ),
            (
                PointerPublishCheckpoint::ParentDirectorySynced,
                "gen-20260726-new",
            ),
        ];

        for (fault, expected_generation_id) in checkpoints {
            let temp = tempfile::tempdir().unwrap();
            let corpus = test_corpus_identity();
            let old_bytes = b"old-vector".to_vec();
            let new_bytes = b"new-vector".to_vec();
            let old_artifact = test_generation_artifact(
                SemanticArtifactRole::FastVector,
                "fast/vector.fsvi",
                &old_bytes,
                &corpus,
            );
            let new_artifact = test_generation_artifact(
                SemanticArtifactRole::FastVector,
                "fast/vector.fsvi",
                &new_bytes,
                &corpus,
            );
            let mut old_manifest =
                test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![old_artifact]);
            old_manifest.generation_id = "gen-20260726-old".to_owned();
            old_manifest.build_id = "build-old".to_owned();
            old_manifest.previous_generation_id = None;
            let mut new_manifest =
                test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![new_artifact]);
            new_manifest.generation_id = "gen-20260726-new".to_owned();
            new_manifest.build_id = "build-new".to_owned();
            new_manifest.previous_generation_id = Some(old_manifest.generation_id.clone());
            new_manifest.created_at_ms += 1_000;
            new_manifest.completed_at_ms += 1_000;
            new_manifest.published_at_ms += 1_000;

            write_generation_artifacts(
                temp.path(),
                &old_manifest,
                &BTreeMap::from([(SemanticArtifactRole::FastVector, old_bytes.clone())]),
            );
            write_generation_artifacts(
                temp.path(),
                &new_manifest,
                &BTreeMap::from([(SemanticArtifactRole::FastVector, new_bytes.clone())]),
            );
            old_manifest.write_immutable(temp.path()).unwrap();
            new_manifest.write_immutable(temp.path()).unwrap();
            SemanticCurrentPointerV1::for_manifest(&old_manifest)
                .unwrap()
                .publish_atomic(temp.path())
                .unwrap();

            let new_pointer = SemanticCurrentPointerV1::for_manifest(&new_manifest).unwrap();
            let mut injected = false;
            let error = new_pointer
                .publish_atomic_with_hook(temp.path(), |checkpoint| {
                    if checkpoint == fault {
                        injected = true;
                        return Err(SemanticGenerationError::PointerIo {
                            source: format!("injected failure after {fault:?}"),
                        });
                    }
                    Ok(())
                })
                .unwrap_err();
            assert!(injected, "fault checkpoint {fault:?} must be reached");
            assert!(matches!(error, SemanticGenerationError::PointerIo { .. }));

            let loaded = load_current_semantic_generation(temp.path(), Some(&corpus)).unwrap();
            assert_eq!(
                loaded.manifest.generation_id, expected_generation_id,
                "failure after {fault:?} must expose exactly the old or new sealed generation"
            );
            let expected_bytes = if expected_generation_id == old_manifest.generation_id {
                &old_bytes
            } else {
                &new_bytes
            };
            let selected_artifact = loaded
                .artifact_paths
                .get(&SemanticArtifactRole::FastVector)
                .unwrap();
            assert_eq!(
                fs::read(selected_artifact).unwrap(),
                *expected_bytes,
                "pointer and artifact bytes must always come from the same generation"
            );
        }
    }

    #[test]
    fn immutable_generation_rejects_unknown_fields_missing_manifest_and_future_manifest() {
        let corpus = test_corpus_identity();
        let bytes = b"fast-vector".to_vec();
        let artifact = test_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/vector.fsvi",
            &bytes,
            &corpus,
        );
        let manifest =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);

        let mut manifest_json: serde_json::Value =
            serde_json::from_slice(&manifest.canonical_bytes().unwrap()).unwrap();
        manifest_json
            .as_object_mut()
            .unwrap()
            .insert("guessed_ready".to_owned(), serde_json::Value::Bool(true));
        assert!(
            serde_json::from_value::<SemanticGenerationManifestV1>(manifest_json).is_err(),
            "unknown manifest fields must fail closed"
        );

        let missing = tempfile::tempdir().unwrap();
        let pointer = SemanticCurrentPointerV1 {
            schema_version: SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION,
            generation_id: manifest.generation_id.clone(),
            manifest_sha256: manifest.sha256().unwrap(),
            published_at_ms: manifest.published_at_ms,
        };
        let pointer_path = SemanticCurrentPointerV1::path(missing.path());
        fs::create_dir_all(pointer_path.parent().unwrap()).unwrap();
        fs::write(&pointer_path, pointer.canonical_bytes().unwrap()).unwrap();
        assert!(matches!(
            load_current_semantic_generation(missing.path(), None),
            Err(SemanticGenerationError::MissingManifest { .. })
        ));

        let future = tempfile::tempdir().unwrap();
        let mut future_manifest = manifest;
        future_manifest.schema_version = SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION + 1;
        write_generation_artifacts(
            future.path(),
            &future_manifest,
            &BTreeMap::from([(SemanticArtifactRole::FastVector, bytes)]),
        );
        let manifest_path = future_manifest.manifest_path(future.path()).unwrap();
        let manifest_bytes = future_manifest.canonical_bytes().unwrap();
        fs::write(&manifest_path, &manifest_bytes).unwrap();
        let future_pointer = SemanticCurrentPointerV1 {
            schema_version: SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION,
            generation_id: future_manifest.generation_id.clone(),
            manifest_sha256: sha256_hex(&manifest_bytes),
            published_at_ms: future_manifest.published_at_ms,
        };
        let pointer_path = SemanticCurrentPointerV1::path(future.path());
        fs::write(&pointer_path, future_pointer.canonical_bytes().unwrap()).unwrap();
        let error = load_current_semantic_generation(future.path(), None).unwrap_err();
        assert!(matches!(
            error,
            SemanticGenerationError::UnsupportedManifestSchema { .. }
        ));
        assert_eq!(
            error.recovery_action(),
            SemanticGenerationRecoveryAction::UpgradeRequired
        );
    }

    proptest! {
        #[test]
        fn immutable_generation_property_rejects_parent_alias_paths(
            directory in "[A-Za-z0-9_-]{1,64}",
            filename in "[A-Za-z0-9_-]{1,64}\\.fsvi",
        ) {
            let corpus = test_corpus_identity();
            let bytes = b"property-vector";
            let traversal = format!("{directory}/../{filename}");
            let artifact = test_generation_artifact(
                SemanticArtifactRole::FastVector,
                &traversal,
                bytes,
                &corpus,
            );
            let candidate =
                test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);
            prop_assert!(matches!(
                candidate.validate(),
                Err(SemanticGenerationError::InvalidManifest { .. })
            ), "parent traversal path unexpectedly validated");
        }

        #[test]
        fn immutable_generation_property_digest_is_stable_and_generation_sensitive(
            suffix in "[A-Za-z0-9_-]{1,32}",
        ) {
            let corpus = test_corpus_identity();
            let artifact = test_generation_artifact(
                SemanticArtifactRole::FastVector,
                "fast/vector.fsvi",
                b"property-vector",
                &corpus,
            );
            let base =
                test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);
            let mut changed = base.clone();
            changed.generation_id = format!("gen-{suffix}");
            prop_assert_eq!(base.sha256().unwrap(), base.sha256().unwrap());
            prop_assert_ne!(base.sha256().unwrap(), changed.sha256().unwrap());
        }
    }

    #[cfg(unix)]
    #[test]
    fn immutable_generation_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_artifact = outside.path().join("outside.fsvi");
        fs::write(&outside_artifact, b"fast-vector").unwrap();
        let corpus = test_corpus_identity();
        let artifact = test_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/vector.fsvi",
            b"fast-vector",
            &corpus,
        );
        let manifest =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);
        let generation_dir = manifest.generation_dir(temp.path()).unwrap();
        fs::create_dir_all(generation_dir.join("fast")).unwrap();
        symlink(&outside_artifact, generation_dir.join("fast/vector.fsvi")).unwrap();
        manifest.write_immutable(temp.path()).unwrap();
        let error = SemanticCurrentPointerV1::for_manifest(&manifest)
            .unwrap()
            .publish_atomic(temp.path())
            .unwrap_err();
        assert!(matches!(error, SemanticGenerationError::ArtifactIo { .. }));

        let pointer_escape = tempfile::tempdir().unwrap();
        let outside_pointer = outside.path().join("outside-current.json");
        fs::write(
            &outside_pointer,
            SemanticCurrentPointerV1::for_manifest(&manifest)
                .unwrap()
                .canonical_bytes()
                .unwrap(),
        )
        .unwrap();
        let pointer_path = SemanticCurrentPointerV1::path(pointer_escape.path());
        fs::create_dir_all(pointer_path.parent().unwrap()).unwrap();
        symlink(&outside_pointer, &pointer_path).unwrap();
        assert!(matches!(
            load_current_semantic_generation(pointer_escape.path(), None),
            Err(SemanticGenerationError::InvalidPointer { .. })
        ));
    }
    }

    // ── Accepted immutable generation contract ────────────────────────

    fn accepted_mutated_generation(
        base: &SemanticGenerationManifestV1,
        mutate: impl FnOnce(&mut SemanticGenerationManifestV1),
    ) -> SemanticGenerationManifestV1 {
        let mut candidate = base.clone();
        mutate(&mut candidate);
        candidate
    }

    fn complete_generation_artifact(
        role: SemanticArtifactRole,
        relative_path: &str,
        bytes: &[u8],
        corpus: &SemanticCorpusSnapshotIdentity,
    ) -> SemanticGenerationArtifact {
        test_generation_artifact(role, relative_path, bytes, corpus, corpus.document_count)
    }

    fn generation_with_id(
        generation_id: &str,
        realized_topology: SemanticGenerationTopology,
        artifacts: Vec<SemanticGenerationArtifact>,
    ) -> SemanticGenerationManifestV1 {
        let mut manifest = test_generation_manifest(realized_topology, artifacts);
        manifest.generation_id = generation_id.to_owned();
        manifest.build_id = format!("build-{generation_id}");
        manifest.previous_generation_id = None;
        manifest
    }

    fn seal_generation(
        data_dir: &Path,
        manifest: &SemanticGenerationManifestV1,
        contents: &BTreeMap<SemanticArtifactRole, Vec<u8>>,
    ) -> String {
        write_generation_artifacts(data_dir, manifest, contents);
        manifest.write_immutable(data_dir).expect("seal generation")
    }

    #[test]
    fn accepted_manifest_uses_frankensearch_identity_and_rejects_hash_controls() {
        use frankensearch::core::EmbeddingIdentityBundleV1;

        let corpus = test_corpus_identity();
        let mut artifact = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            b"fast-vector",
            &corpus,
        );
        let manifest =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact.clone()]);
        manifest.validate().expect("canonical semantic identity");

        artifact.embedding_identity =
            EmbeddingIdentityBundleV1::explicit_test_model("hash-control", 256)
                .freeze()
                .expect("explicit hash control");
        artifact.artifact_format = artifact.embedding_identity.identity.storage.format.clone();
        let hash_control =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);
        let error = hash_control
            .validate()
            .expect_err("hash control must fail closed");
        assert_eq!(error.reason_code(), "semantic_manifest_identity_invalid");

        let mut noncanonical = manifest;
        noncanonical.artifacts[0]
            .embedding_identity
            .identity
            .space
            .pooling
            .push_str("-drift");
        assert!(matches!(
            noncanonical.validate(),
            Err(SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::Identity,
                ..
            })
        ));
    }

    #[test]
    fn accepted_manifest_represents_partial_quality_coverage_and_across_tier_totals() {
        let corpus = test_corpus_identity();
        let fast = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            b"fast-vector",
            &corpus,
        );
        let quality = test_generation_artifact(
            SemanticArtifactRole::QualityVector,
            "quality/partial.fsvi",
            b"quality-vector",
            &corpus,
            1,
        );
        let manifest = test_generation_manifest(
            SemanticGenerationTopology::FullProgressive,
            vec![fast.clone(), quality.clone()],
        );
        manifest.validate().expect("partial quality is truthful");
        assert_eq!(manifest.selected_document_count, 3);
        assert_eq!(manifest.total_live_vector_count_across_tiers, 4);
        assert_eq!(manifest.total_vector_slot_count_across_tiers, 4);
        assert_eq!(quality.covered_document_count, 1);
        assert_eq!(quality.selected_document_count, 3);
        assert_ne!(
            quality.covered_live_docset_sha256,
            corpus.ordered_live_docset_sha256
        );

        let quality_only = test_generation_manifest(
            SemanticGenerationTopology::QualityOnly,
            vec![complete_generation_artifact(
                SemanticArtifactRole::QualityVector,
                "quality/primary.fsvi",
                b"quality-only",
                &corpus,
            )],
        );
        quality_only.validate().expect("quality-only topology");

        let mut dishonest = manifest.clone();
        dishonest.artifacts[1].covered_live_docset_sha256 =
            corpus.ordered_live_docset_sha256.clone();
        dishonest.artifacts[1].validation.covered_live_docset_sha256 =
            corpus.ordered_live_docset_sha256.clone();
        let error = dishonest
            .validate()
            .expect_err("partial tier cannot claim full docset identity");
        assert_eq!(
            error.reason_code(),
            "semantic_manifest_partial_coverage_invalid"
        );

        let mut wrong_total = manifest;
        wrong_total.total_live_vector_count_across_tiers += 1;
        assert_eq!(
            wrong_total.validate().unwrap_err().reason_code(),
            "semantic_manifest_count_invalid"
        );
    }

    #[test]
    fn accepted_immutable_generation_rejects_declared_or_on_disk_wal_state() {
        let corpus = test_corpus_identity();
        let bytes = b"wal-free-vector".to_vec();
        let artifact = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            &bytes,
            &corpus,
        );
        let manifest = generation_with_id(
            "gen-wal-free",
            SemanticGenerationTopology::FastOnly,
            vec![artifact],
        );

        let mut declared_wal = manifest.clone();
        declared_wal.artifacts[0].wal_entry_count = 1;
        declared_wal.artifacts[0].validation.wal_entry_count = 1;
        let error = declared_wal
            .validate()
            .expect_err("sealed generation cannot declare live WAL entries");
        assert_eq!(error.reason_code(), "semantic_manifest_not_durably_sealed");

        let seal_temp = tempfile::tempdir().unwrap();
        write_generation_artifacts(
            seal_temp.path(),
            &manifest,
            &BTreeMap::from([(SemanticArtifactRole::FastVector, bytes.clone())]),
        );
        let artifact_path = manifest
            .generation_dir(seal_temp.path())
            .unwrap()
            .join(&manifest.artifacts[0].relative_path);
        fs::write(wal_path_for(&artifact_path), b"injected-sidecar").unwrap();
        assert!(matches!(
            manifest.write_immutable(seal_temp.path()),
            Err(SemanticGenerationError::ArtifactIo { .. })
        ));

        let load_temp = tempfile::tempdir().unwrap();
        seal_generation(
            load_temp.path(),
            &manifest,
            &BTreeMap::from([(SemanticArtifactRole::FastVector, bytes)]),
        );
        let pointer =
            SemanticCurrentPointerV1::for_manifest(&manifest, manifest.sealed_at_ms + 1, 1)
                .unwrap();
        pointer
            .publish_atomic(load_temp.path(), &SemanticExpectedCurrentV1::Absent)
            .unwrap();
        let artifact_path = manifest
            .generation_dir(load_temp.path())
            .unwrap()
            .join(&manifest.artifacts[0].relative_path);
        fs::write(wal_path_for(&artifact_path), b"post-seal-sidecar").unwrap();
        assert!(matches!(
            load_current_semantic_generation(load_temp.path(), Some(&corpus)),
            Err(SemanticGenerationError::ArtifactIo { .. })
        ));
    }

    #[test]
    fn accepted_ann_binding_covers_exact_base_bytes_storage_and_parameters() {
        let corpus = test_corpus_identity();
        let base = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            b"base-vector",
            &corpus,
        );
        let ann_template = complete_generation_artifact(
            SemanticArtifactRole::FastAnn,
            "fast/search.hnsw",
            b"ann-index",
            &corpus,
        );
        let mut base_with_wal = base.clone();
        base_with_wal.wal_entry_count = 7;
        base_with_wal.validation.wal_entry_count = 7;
        let ann_from_live_base = bind_ann_to_base(ann_template.clone(), &base_with_wal);
        assert_eq!(ann_from_live_base.wal_entry_count, 0);
        assert_eq!(ann_from_live_base.validation.wal_entry_count, 0);

        let ann = bind_ann_to_base(ann_template, &base);
        let manifest = test_generation_manifest(
            SemanticGenerationTopology::FastOnly,
            vec![base.clone(), ann],
        );
        manifest.validate().expect("exact ANN base binding");

        for mutation in 0..3 {
            let mut invalid = manifest.clone();
            let binding = invalid.artifacts[1].ann_base.as_mut().expect("ANN binding");
            match mutation {
                0 => binding.base_artifact_sha256 = test_sha256("wrong-base-bytes"),
                1 => binding.base_storage_fingerprint = test_sha256("wrong-base-storage"),
                _ => binding.base_artifact_parameters_sha256 = test_sha256("wrong-base-parameters"),
            }
            let error = invalid.validate().expect_err("ANN drift must fail");
            assert_eq!(error.reason_code(), "semantic_manifest_ann_binding_invalid");
        }

        let mut live_ann_wal = manifest.clone();
        live_ann_wal.artifacts[1].wal_entry_count = 1;
        live_ann_wal.artifacts[1].validation.wal_entry_count = 1;
        let error = live_ann_wal
            .validate()
            .expect_err("immutable ANN artifact must reject a live WAL");
        assert_eq!(error.reason_code(), "semantic_manifest_not_durably_sealed");

        let mut wrong_space = manifest;
        wrong_space.artifacts[1].embedding_identity =
            test_semantic_identity("different-fast-space", 256, "fsvi-v1");
        let error = wrong_space
            .validate()
            .expect_err("ANN identity mismatch must be typed");
        assert!(matches!(
            &error,
            SemanticGenerationError::EmbeddingIdentityMismatch { .. }
        ));
        assert_eq!(error.reason_code(), "semantic_embedding_identity_mismatch");
    }

    #[test]
    fn accepted_manifest_storage_order_is_stable_but_digest_is_json_independent() {
        let corpus = test_corpus_identity();
        let base_vector = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            b"fast-vector",
            &corpus,
        );
        let ann = bind_ann_to_base(
            complete_generation_artifact(
                SemanticArtifactRole::FastAnn,
                "fast/search.hnsw",
                b"fast-ann",
                &corpus,
            ),
            &base_vector,
        );
        let base =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![base_vector, ann]);
        base.validate().unwrap();
        let canonical = base.canonical_bytes().unwrap();
        assert_eq!(canonical, base.canonical_bytes().unwrap());
        let canonical_text = std::str::from_utf8(&canonical).unwrap();

        let root_keys = [
            "\"schema_version\"",
            "\"generation_id\"",
            "\"build_id\"",
            "\"request_id\"",
            "\"previous_generation_id\"",
            "\"created_at_ms\"",
            "\"completed_at_ms\"",
            "\"sealed_at_ms\"",
            "\"requested_topology\"",
            "\"realized_topology\"",
            "\"status\"",
            "\"corpus\"",
            "\"producer\"",
            "\"selected_document_count\"",
            "\"total_vector_slot_count_across_tiers\"",
            "\"total_live_vector_count_across_tiers\"",
            "\"total_tombstone_vector_count_across_tiers\"",
            "\"total_wal_entry_count_across_tiers\"",
            "\"artifacts\"",
        ];
        let root_positions =
            root_keys.map(|key| canonical_text.find(key).expect("root manifest key"));
        assert!(
            root_positions
                .windows(2)
                .all(|window| window[0] < window[1]),
            "stored root fields must remain in schema order: {root_positions:?}"
        );

        let corpus_json = canonical_text
            .split_once("\"corpus\":")
            .expect("corpus JSON")
            .1;
        let corpus_keys = [
            "\"database_uuid\"",
            "\"source_change_sequence\"",
            "\"schema_contract_sha256\"",
            "\"content_selection_sha256\"",
            "\"canonicalization_sha256\"",
            "\"chunking_sha256\"",
            "\"document_id_contract_sha256\"",
            "\"config_sha256\"",
            "\"corpus_sha256\"",
            "\"ordered_live_docset_sha256\"",
            "\"content_sha256\"",
            "\"document_count\"",
        ];
        let corpus_positions = corpus_keys.map(|key| corpus_json.find(key).expect("corpus key"));
        assert!(
            corpus_positions
                .windows(2)
                .all(|window| window[0] < window[1]),
            "stored corpus fields must remain in schema order: {corpus_positions:?}"
        );

        let producer_json = canonical_text
            .split_once("\"producer\":")
            .expect("producer JSON")
            .1;
        let producer_keys = [
            "\"cass_git_commit\"",
            "\"frankensearch_git_commit\"",
            "\"cargo_lock_sha256\"",
            "\"rust_toolchain\"",
            "\"enabled_features\"",
            "\"build_profile\"",
            "\"binary_elf_sha256\"",
        ];
        let producer_positions =
            producer_keys.map(|key| producer_json.find(key).expect("producer key"));
        assert!(
            producer_positions
                .windows(2)
                .all(|window| window[0] < window[1]),
            "stored producer fields must remain in schema order: {producer_positions:?}"
        );

        let artifacts_json = canonical_text
            .split_once("\"artifacts\":")
            .expect("artifact JSON")
            .1;
        let artifact_keys = [
            "\"role\"",
            "\"relative_path\"",
            "\"artifact_sha256\"",
            "\"size_bytes\"",
            "\"artifact_format\"",
            "\"artifact_parameters_sha256\"",
            "\"embedding_identity\"",
            "\"ann_base\"",
            "\"selected_document_count\"",
            "\"covered_document_count\"",
            "\"vector_slot_count\"",
            "\"live_vector_count\"",
            "\"tombstone_vector_count\"",
            "\"wal_entry_count\"",
            "\"generation_corpus_sha256\"",
            "\"covered_live_docset_sha256\"",
            "\"covered_content_sha256\"",
            "\"validation\"",
        ];
        let artifact_positions =
            artifact_keys.map(|key| artifacts_json.find(key).expect("artifact key"));
        assert!(
            artifact_positions
                .windows(2)
                .all(|window| window[0] < window[1]),
            "stored artifact fields must remain in schema order: {artifact_positions:?}"
        );
        let embedding_json = artifacts_json
            .split_once("\"embedding_identity\":")
            .expect("frozen embedding identity JSON")
            .1;
        let frozen_keys = ["\"identity\"", "\"canonical_bytes\"", "\"fingerprint\""];
        let frozen_positions =
            frozen_keys.map(|key| embedding_json.find(key).expect("frozen identity key"));
        assert!(
            frozen_positions
                .windows(2)
                .all(|window| window[0] < window[1]),
            "stored frozen identity fields must remain in schema order: {frozen_positions:?}"
        );
        let identity_json = embedding_json
            .split_once("\"identity\":")
            .expect("embedding bundle JSON")
            .1;
        let identity_keys = ["\"space\"", "\"producer\"", "\"input\"", "\"storage\""];
        let identity_positions =
            identity_keys.map(|key| identity_json.find(key).expect("identity component key"));
        assert!(
            identity_positions
                .windows(2)
                .all(|window| window[0] < window[1]),
            "stored identity components must remain in schema order: {identity_positions:?}"
        );
        let validation_json = artifacts_json
            .split_once("\"validation\":")
            .expect("validation JSON")
            .1;
        let validation_keys = [
            "\"validator_id\"",
            "\"validated_at_ms\"",
            "\"artifact_sha256\"",
            "\"size_bytes\"",
            "\"selected_document_count\"",
            "\"covered_document_count\"",
            "\"vector_slot_count\"",
            "\"live_vector_count\"",
            "\"tombstone_vector_count\"",
            "\"wal_entry_count\"",
            "\"generation_corpus_sha256\"",
            "\"covered_live_docset_sha256\"",
            "\"covered_content_sha256\"",
        ];
        let validation_positions =
            validation_keys.map(|key| validation_json.find(key).expect("validation key"));
        assert!(
            validation_positions
                .windows(2)
                .all(|window| window[0] < window[1]),
            "stored validation fields must remain in schema order: {validation_positions:?}"
        );
        for key in [
            "\"base_role\"",
            "\"base_artifact_sha256\"",
            "\"base_storage_fingerprint\"",
            "\"base_artifact_parameters_sha256\"",
            "\"algorithm\"",
            "\"metric\"",
            "\"parameters_sha256\"",
        ] {
            assert!(
                artifacts_json.contains(key),
                "missing ANN binding key {key}"
            );
        }

        let digest = base.sha256().unwrap();
        let pretty = serde_json::to_vec_pretty(&base).unwrap();
        let decoded: SemanticGenerationManifestV1 = serde_json::from_slice(&pretty).unwrap();
        assert_eq!(decoded.sha256().unwrap(), digest);
        assert_ne!(
            digest,
            sha256_hex(&canonical),
            "manifest fingerprint must be a domain-separated semantic transcript, not raw JSON"
        );
    }

    #[test]
    fn accepted_manifest_digest_covers_every_authoritative_field() {
        let corpus = test_corpus_identity();
        let artifact = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            b"fast-vector",
            &corpus,
        );
        let base = test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);
        base.validate().unwrap();
        let base_digest = base.sha256().unwrap();

        macro_rules! assert_digest_field {
            ($label:literal, $mutate:expr) => {{
                let candidate = accepted_mutated_generation(&base, $mutate);
                assert_ne!(
                    candidate.sha256().unwrap(),
                    base_digest,
                    "authoritative field {} must affect the semantic digest",
                    $label
                );
            }};
        }

        assert_digest_field!("schema_version", |value| value.schema_version += 1);
        assert_digest_field!("generation_id", |value| value.generation_id.push('x'));
        assert_digest_field!("build_id", |value| value.build_id.push('x'));
        assert_digest_field!("request_id", |value| value.request_id = None);
        assert_digest_field!("previous_generation_id", |value| {
            value.previous_generation_id = None;
        });
        assert_digest_field!("created_at_ms", |value| value.created_at_ms += 1);
        assert_digest_field!("completed_at_ms", |value| value.completed_at_ms += 1);
        assert_digest_field!("sealed_at_ms", |value| value.sealed_at_ms += 1);
        assert_digest_field!("requested_topology", |value| {
            value.requested_topology = SemanticGenerationTopology::FastOnly;
        });
        assert_digest_field!("realized_topology", |value| {
            value.realized_topology = SemanticGenerationTopology::QualityOnly;
        });
        assert_digest_field!("status", |value| {
            value.status = SemanticGenerationStatus::Building;
        });

        assert_digest_field!("corpus.database_uuid", |value| {
            value.corpus.database_uuid.push('x');
        });
        assert_digest_field!("corpus.source_change_sequence", |value| {
            value.corpus.source_change_sequence += 1;
        });
        assert_digest_field!("corpus.schema_contract_sha256", |value| {
            value.corpus.schema_contract_sha256 = test_sha256("changed-schema");
        });
        assert_digest_field!("corpus.content_selection_sha256", |value| {
            value.corpus.content_selection_sha256 = test_sha256("changed-selection");
        });
        assert_digest_field!("corpus.canonicalization_sha256", |value| {
            value.corpus.canonicalization_sha256 = test_sha256("changed-canonicalization");
        });
        assert_digest_field!("corpus.chunking_sha256", |value| {
            value.corpus.chunking_sha256 = test_sha256("changed-chunking");
        });
        assert_digest_field!("corpus.document_id_contract_sha256", |value| {
            value.corpus.document_id_contract_sha256 = test_sha256("changed-document-id");
        });
        assert_digest_field!("corpus.config_sha256", |value| {
            value.corpus.config_sha256 = test_sha256("changed-config");
        });
        assert_digest_field!("corpus.corpus_sha256", |value| {
            value.corpus.corpus_sha256 = test_sha256("changed-corpus");
        });
        assert_digest_field!("corpus.ordered_live_docset_sha256", |value| {
            value.corpus.ordered_live_docset_sha256 = test_sha256("changed-docset");
        });
        assert_digest_field!("corpus.content_sha256", |value| {
            value.corpus.content_sha256 = test_sha256("changed-content");
        });
        assert_digest_field!("corpus.document_count", |value| {
            value.corpus.document_count += 1;
        });

        assert_digest_field!("producer.cass_git_commit", |value| {
            value.producer.cass_git_commit.push('c');
        });
        assert_digest_field!("producer.frankensearch_git_commit", |value| {
            value.producer.frankensearch_git_commit.push('c');
        });
        assert_digest_field!("producer.cargo_lock_sha256", |value| {
            value.producer.cargo_lock_sha256 = test_sha256("changed-lock");
        });
        assert_digest_field!("producer.rust_toolchain", |value| {
            value.producer.rust_toolchain.push('x');
        });
        assert_digest_field!("producer.enabled_features", |value| {
            value.producer.enabled_features.push("zstd".to_owned());
        });
        assert_digest_field!("producer.build_profile", |value| {
            value.producer.build_profile.push('x');
        });
        assert_digest_field!("producer.binary_elf_sha256", |value| {
            value.producer.binary_elf_sha256 = test_sha256("changed-binary");
        });

        assert_digest_field!("selected_document_count", |value| {
            value.selected_document_count += 1;
        });
        assert_digest_field!("total_vector_slot_count_across_tiers", |value| {
            value.total_vector_slot_count_across_tiers += 1;
        });
        assert_digest_field!("total_live_vector_count_across_tiers", |value| {
            value.total_live_vector_count_across_tiers += 1;
        });
        assert_digest_field!("total_tombstone_vector_count_across_tiers", |value| {
            value.total_tombstone_vector_count_across_tiers += 1;
        });
        assert_digest_field!("total_wal_entry_count_across_tiers", |value| {
            value.total_wal_entry_count_across_tiers += 1;
        });
        assert_digest_field!("artifacts length", |value| value.artifacts.clear());

        assert_digest_field!("artifact.role", |value| {
            value.artifacts[0].role = SemanticArtifactRole::QualityVector;
        });
        assert_digest_field!("artifact.relative_path", |value| {
            value.artifacts[0].relative_path.push('x');
        });
        assert_digest_field!("artifact.artifact_sha256", |value| {
            value.artifacts[0].artifact_sha256 = test_sha256("changed-artifact");
        });
        assert_digest_field!("artifact.size_bytes", |value| {
            value.artifacts[0].size_bytes += 1;
        });
        assert_digest_field!("artifact.artifact_format", |value| {
            value.artifacts[0].artifact_format.push('x');
        });
        assert_digest_field!("artifact.artifact_parameters_sha256", |value| {
            value.artifacts[0].artifact_parameters_sha256 = test_sha256("changed-parameters");
        });
        assert_digest_field!("artifact.embedding_identity", |value| {
            value.artifacts[0].embedding_identity =
                test_semantic_identity("changed-fast", 256, "fsvi-v1");
        });
        assert_digest_field!("artifact.selected_document_count", |value| {
            value.artifacts[0].selected_document_count += 1;
        });
        assert_digest_field!("artifact.covered_document_count", |value| {
            value.artifacts[0].covered_document_count -= 1;
        });
        assert_digest_field!("artifact.vector_slot_count", |value| {
            value.artifacts[0].vector_slot_count += 1;
        });
        assert_digest_field!("artifact.live_vector_count", |value| {
            value.artifacts[0].live_vector_count -= 1;
        });
        assert_digest_field!("artifact.tombstone_vector_count", |value| {
            value.artifacts[0].tombstone_vector_count += 1;
        });
        assert_digest_field!("artifact.wal_entry_count", |value| {
            value.artifacts[0].wal_entry_count += 1;
        });
        assert_digest_field!("artifact.generation_corpus_sha256", |value| {
            value.artifacts[0].generation_corpus_sha256 = test_sha256("changed-generation");
        });
        assert_digest_field!("artifact.covered_live_docset_sha256", |value| {
            value.artifacts[0].covered_live_docset_sha256 = test_sha256("changed-artifact-docset");
        });
        assert_digest_field!("artifact.covered_content_sha256", |value| {
            value.artifacts[0].covered_content_sha256 = test_sha256("changed-artifact-content");
        });

        assert_digest_field!("validation.validator_id", |value| {
            value.artifacts[0].validation.validator_id.push('x');
        });
        assert_digest_field!("validation.validated_at_ms", |value| {
            value.artifacts[0].validation.validated_at_ms += 1;
        });
        assert_digest_field!("validation.artifact_sha256", |value| {
            value.artifacts[0].validation.artifact_sha256 =
                test_sha256("changed-validation-artifact");
        });
        assert_digest_field!("validation.size_bytes", |value| {
            value.artifacts[0].validation.size_bytes += 1;
        });
        assert_digest_field!("validation.selected_document_count", |value| {
            value.artifacts[0].validation.selected_document_count += 1;
        });
        assert_digest_field!("validation.covered_document_count", |value| {
            value.artifacts[0].validation.covered_document_count -= 1;
        });
        assert_digest_field!("validation.vector_slot_count", |value| {
            value.artifacts[0].validation.vector_slot_count += 1;
        });
        assert_digest_field!("validation.live_vector_count", |value| {
            value.artifacts[0].validation.live_vector_count -= 1;
        });
        assert_digest_field!("validation.tombstone_vector_count", |value| {
            value.artifacts[0].validation.tombstone_vector_count += 1;
        });
        assert_digest_field!("validation.wal_entry_count", |value| {
            value.artifacts[0].validation.wal_entry_count += 1;
        });
        assert_digest_field!("validation.generation_corpus_sha256", |value| {
            value.artifacts[0].validation.generation_corpus_sha256 =
                test_sha256("changed-validation-generation");
        });
        assert_digest_field!("validation.covered_live_docset_sha256", |value| {
            value.artifacts[0].validation.covered_live_docset_sha256 =
                test_sha256("changed-validation-docset");
        });
        assert_digest_field!("validation.covered_content_sha256", |value| {
            value.artifacts[0].validation.covered_content_sha256 =
                test_sha256("changed-validation-content");
        });

        let base_vector = base.artifacts[0].clone();
        let ann = bind_ann_to_base(
            complete_generation_artifact(
                SemanticArtifactRole::FastAnn,
                "fast/search.hnsw",
                b"fast-ann",
                &corpus,
            ),
            &base_vector,
        );
        let ann_base =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![base_vector, ann]);
        let ann_digest = ann_base.sha256().unwrap();
        macro_rules! assert_ann_binding_field {
            ($label:literal, $mutate:expr) => {{
                let candidate = accepted_mutated_generation(&ann_base, $mutate);
                assert_ne!(
                    candidate.sha256().unwrap(),
                    ann_digest,
                    "authoritative ANN field {} must affect the semantic digest",
                    $label
                );
            }};
        }
        assert_ann_binding_field!("ann_base presence", |value| {
            value.artifacts[1].ann_base = None;
        });
        assert_ann_binding_field!("ann_base.base_role", |value| {
            value.artifacts[1].ann_base.as_mut().unwrap().base_role =
                SemanticArtifactRole::QualityVector;
        });
        assert_ann_binding_field!("ann_base.base_artifact_sha256", |value| {
            value.artifacts[1]
                .ann_base
                .as_mut()
                .unwrap()
                .base_artifact_sha256 = test_sha256("changed-base");
        });
        assert_ann_binding_field!("ann_base.base_storage_fingerprint", |value| {
            value.artifacts[1]
                .ann_base
                .as_mut()
                .unwrap()
                .base_storage_fingerprint = test_sha256("changed-storage");
        });
        assert_ann_binding_field!("ann_base.base_artifact_parameters_sha256", |value| {
            value.artifacts[1]
                .ann_base
                .as_mut()
                .unwrap()
                .base_artifact_parameters_sha256 = test_sha256("changed-base-parameters");
        });
        assert_ann_binding_field!("ann_base.algorithm", |value| {
            value.artifacts[1]
                .ann_base
                .as_mut()
                .unwrap()
                .algorithm
                .push('x');
        });
        assert_ann_binding_field!("ann_base.metric", |value| {
            value.artifacts[1]
                .ann_base
                .as_mut()
                .unwrap()
                .metric
                .push('x');
        });
        assert_ann_binding_field!("ann_base.parameters_sha256", |value| {
            value.artifacts[1]
                .ann_base
                .as_mut()
                .unwrap()
                .parameters_sha256 = test_sha256("changed-ann-parameters");
        });
    }

    #[test]
    fn accepted_manifest_seal_faults_never_leave_torn_final_bytes() {
        let checkpoints = [
            ManifestSealCheckpoint::ArtifactDurabilityEstablished,
            ManifestSealCheckpoint::TemporaryBytesWritten,
            ManifestSealCheckpoint::TemporaryFileSynced,
            ManifestSealCheckpoint::ManifestPublished,
            ManifestSealCheckpoint::ParentDirectorySynced,
        ];
        for checkpoint in checkpoints {
            let temp = tempfile::tempdir().unwrap();
            let corpus = test_corpus_identity();
            let bytes = b"durable-vector".to_vec();
            let artifact = complete_generation_artifact(
                SemanticArtifactRole::FastVector,
                "fast/primary.fsvi",
                &bytes,
                &corpus,
            );
            let manifest = generation_with_id(
                &format!("gen-seal-{checkpoint:?}"),
                SemanticGenerationTopology::FastOnly,
                vec![artifact],
            );
            write_generation_artifacts(
                temp.path(),
                &manifest,
                &BTreeMap::from([(SemanticArtifactRole::FastVector, bytes)]),
            );
            let error = manifest
                .write_immutable_with_hook(temp.path(), |observed| {
                    if observed == checkpoint {
                        return Err(SemanticGenerationError::ManifestIo {
                            generation_id: manifest.generation_id.clone(),
                            source: "injected seal fault".to_owned(),
                        });
                    }
                    Ok(())
                })
                .expect_err("injected checkpoint must fail");
            assert!(matches!(error, SemanticGenerationError::ManifestIo { .. }));

            let manifest_path = manifest.manifest_path(temp.path()).unwrap();
            if matches!(
                checkpoint,
                ManifestSealCheckpoint::ManifestPublished
                    | ManifestSealCheckpoint::ParentDirectorySynced
            ) {
                let loaded = load_manifest_file(&manifest_path, &manifest.generation_id).unwrap();
                assert_eq!(loaded, manifest);
                assert_eq!(loaded.sha256().unwrap(), manifest.sha256().unwrap());
            } else {
                assert!(
                    !manifest_path.exists(),
                    "pre-publication fault exposed a final manifest"
                );
            }
            let generation_dir = manifest.generation_dir(temp.path()).unwrap();
            assert!(
                fs::read_dir(generation_dir)
                    .unwrap()
                    .filter_map(Result::ok)
                    .all(|entry| !entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".manifest.seal.")),
                "temporary seal file survived error return"
            );
        }
    }

    #[test]
    fn accepted_pointer_publish_is_locked_compare_and_swap_under_concurrency() {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().unwrap();
        let corpus = test_corpus_identity();
        let mut manifests = Vec::new();
        for (suffix, bytes) in [("a", b"vector-a".to_vec()), ("b", b"vector-b".to_vec())] {
            let artifact = complete_generation_artifact(
                SemanticArtifactRole::FastVector,
                "fast/primary.fsvi",
                &bytes,
                &corpus,
            );
            let manifest = generation_with_id(
                &format!("gen-concurrent-{suffix}"),
                SemanticGenerationTopology::FastOnly,
                vec![artifact],
            );
            seal_generation(
                temp.path(),
                &manifest,
                &BTreeMap::from([(SemanticArtifactRole::FastVector, bytes)]),
            );
            manifests.push(manifest);
        }

        let barrier = Arc::new(Barrier::new(3));
        let data_dir = Arc::new(temp.path().to_path_buf());
        let handles = manifests
            .iter()
            .cloned()
            .map(|manifest| {
                let barrier = Arc::clone(&barrier);
                let data_dir = Arc::clone(&data_dir);
                std::thread::spawn(move || {
                    let pointer = SemanticCurrentPointerV1::for_manifest(
                        &manifest,
                        manifest.sealed_at_ms + 1,
                        1,
                    )
                    .unwrap();
                    barrier.wait();
                    pointer.publish_atomic(&data_dir, &SemanticExpectedCurrentV1::Absent)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(
                    result,
                    Err(SemanticGenerationError::ConcurrentPublishConflict { .. })
                ))
                .count(),
            1
        );
        let loaded = load_current_semantic_generation(temp.path(), Some(&corpus)).unwrap();
        assert!(
            manifests
                .iter()
                .any(|manifest| manifest.generation_id == loaded.manifest.generation_id)
        );
    }

    #[test]
    fn accepted_pointer_faults_select_exactly_old_or_new_generation() {
        let checkpoints = [
            (PointerPublishCheckpoint::TemporaryBytesWritten, false),
            (PointerPublishCheckpoint::TemporaryFileSynced, false),
            (PointerPublishCheckpoint::PointerReplaced, true),
            (PointerPublishCheckpoint::ParentDirectorySynced, true),
        ];
        for (fault, selects_new) in checkpoints {
            let temp = tempfile::tempdir().unwrap();
            let corpus = test_corpus_identity();
            let mut manifests = Vec::new();
            for (id, bytes) in [
                ("gen-pointer-old", b"old-vector".to_vec()),
                ("gen-pointer-new", b"new-vector".to_vec()),
            ] {
                let artifact = complete_generation_artifact(
                    SemanticArtifactRole::FastVector,
                    "fast/primary.fsvi",
                    &bytes,
                    &corpus,
                );
                let manifest =
                    generation_with_id(id, SemanticGenerationTopology::FastOnly, vec![artifact]);
                seal_generation(
                    temp.path(),
                    &manifest,
                    &BTreeMap::from([(SemanticArtifactRole::FastVector, bytes)]),
                );
                manifests.push(manifest);
            }
            let old_pointer =
                SemanticCurrentPointerV1::for_manifest(&manifests[0], 1_700_000_000_400, 1)
                    .unwrap();
            old_pointer
                .publish_atomic(temp.path(), &SemanticExpectedCurrentV1::Absent)
                .unwrap();
            let new_pointer =
                SemanticCurrentPointerV1::for_manifest(&manifests[1], 1_700_000_000_500, 2)
                    .unwrap();
            let error = new_pointer
                .publish_atomic_with_hook(
                    temp.path(),
                    &SemanticExpectedCurrentV1::exact(&old_pointer),
                    |observed| {
                        if observed == fault {
                            return Err(SemanticGenerationError::PointerIo {
                                source: "injected pointer fault".to_owned(),
                            });
                        }
                        Ok(())
                    },
                )
                .expect_err("injected pointer checkpoint");
            assert!(matches!(error, SemanticGenerationError::PointerIo { .. }));
            let loaded = load_current_semantic_generation(temp.path(), Some(&corpus)).unwrap();
            assert_eq!(
                loaded.manifest.generation_id,
                manifests[usize::from(selects_new)].generation_id
            );
        }
    }

    #[test]
    fn accepted_pointer_selection_timestamp_is_independent_and_supports_republication() {
        let temp = tempfile::tempdir().unwrap();
        let corpus = test_corpus_identity();
        let bytes = b"republish-vector".to_vec();
        let artifact = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            &bytes,
            &corpus,
        );
        let manifest = generation_with_id(
            "gen-republish",
            SemanticGenerationTopology::FastOnly,
            vec![artifact],
        );
        seal_generation(
            temp.path(),
            &manifest,
            &BTreeMap::from([(SemanticArtifactRole::FastVector, bytes)]),
        );
        assert!(
            SemanticCurrentPointerV1::for_manifest(&manifest, manifest.sealed_at_ms - 1, 1)
                .is_err()
        );
        assert!(matches!(
            SemanticCurrentPointerV1::for_manifest(&manifest, manifest.sealed_at_ms + 1, 0),
            Err(SemanticGenerationError::InvalidPointer { reason })
                if reason == "selection_epoch must be positive"
        ));

        let first =
            SemanticCurrentPointerV1::for_manifest(&manifest, manifest.sealed_at_ms + 100, 1)
                .unwrap();
        first
            .publish_atomic(temp.path(), &SemanticExpectedCurrentV1::Absent)
            .unwrap();
        let second =
            SemanticCurrentPointerV1::for_manifest(&manifest, manifest.sealed_at_ms + 200, 2)
                .unwrap();
        second
            .publish_atomic(temp.path(), &SemanticExpectedCurrentV1::exact(&first))
            .unwrap();
        let loaded = load_current_semantic_generation(temp.path(), None).unwrap();
        assert_eq!(
            loaded.manifest.sha256().unwrap(),
            manifest.sha256().unwrap()
        );
        assert_eq!(loaded.pointer.selected_at_ms, manifest.sealed_at_ms + 200);
        assert_eq!(loaded.manifest.sealed_at_ms, manifest.sealed_at_ms);
    }

    #[test]
    fn accepted_pointer_cas_rejects_aba_and_requires_a_strictly_newer_selection_epoch() {
        let temp = tempfile::tempdir().unwrap();
        let corpus = test_corpus_identity();
        let mut manifests = Vec::new();
        for (generation_id, bytes) in [
            ("gen-aba-a", b"aba-vector-a".to_vec()),
            ("gen-aba-b", b"aba-vector-b".to_vec()),
        ] {
            let artifact = complete_generation_artifact(
                SemanticArtifactRole::FastVector,
                "fast/primary.fsvi",
                &bytes,
                &corpus,
            );
            let manifest = generation_with_id(
                generation_id,
                SemanticGenerationTopology::FastOnly,
                vec![artifact],
            );
            seal_generation(
                temp.path(),
                &manifest,
                &BTreeMap::from([(SemanticArtifactRole::FastVector, bytes)]),
            );
            manifests.push(manifest);
        }

        let selected_a_first =
            SemanticCurrentPointerV1::for_manifest(&manifests[0], 1_700_000_000_400, 1).unwrap();
        selected_a_first
            .publish_atomic(temp.path(), &SemanticExpectedCurrentV1::Absent)
            .unwrap();
        let stale_a_expectation = SemanticExpectedCurrentV1::exact(&selected_a_first);

        let selected_b =
            SemanticCurrentPointerV1::for_manifest(&manifests[1], 1_700_000_000_500, 2).unwrap();
        selected_b
            .publish_atomic(
                temp.path(),
                &SemanticExpectedCurrentV1::exact(&selected_a_first),
            )
            .unwrap();
        let selected_a_again =
            SemanticCurrentPointerV1::for_manifest(&manifests[0], 1_700_000_000_600, 3).unwrap();
        selected_a_again
            .publish_atomic(temp.path(), &SemanticExpectedCurrentV1::exact(&selected_b))
            .unwrap();

        let stale_publisher_target =
            SemanticCurrentPointerV1::for_manifest(&manifests[1], 1_700_000_000_700, 4).unwrap();
        assert!(matches!(
            stale_publisher_target.publish_atomic(temp.path(), &stale_a_expectation),
            Err(SemanticGenerationError::ConcurrentPublishConflict { .. })
        ));
        let after_stale_attempt = load_current_semantic_generation(temp.path(), None).unwrap();
        assert_eq!(after_stale_attempt.pointer, selected_a_again);

        let equal_epoch = SemanticCurrentPointerV1::for_manifest(
            &manifests[1],
            selected_a_again.selected_at_ms,
            selected_a_again.selection_epoch,
        )
        .unwrap();
        let error = equal_epoch
            .publish_atomic(
                temp.path(),
                &SemanticExpectedCurrentV1::exact(&selected_a_again),
            )
            .expect_err("selection epochs must advance strictly");
        assert!(matches!(
            &error,
            SemanticGenerationError::SelectionEpochNotAdvanced { .. }
        ));
        assert_eq!(
            error.recovery_action(),
            SemanticGenerationRecoveryAction::RetryPublish
        );
    }

    #[cfg(unix)]
    #[test]
    fn accepted_path_checks_reject_symlinks_hardlink_aliases_and_portable_aliases() {
        use std::os::unix::fs::symlink;

        let corpus = test_corpus_identity();
        for path in [
            "../escape.fsvi",
            "CON/index.fsvi",
            "fast/trailing.",
            "fast/stream:payload.fsvi",
            "fast//duplicate.fsvi",
            "fast/naïve.fsvi",
        ] {
            let artifact = complete_generation_artifact(
                SemanticArtifactRole::FastVector,
                path,
                b"path-vector",
                &corpus,
            );
            let error =
                test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact])
                    .validate()
                    .expect_err("portable alias path");
            assert_eq!(error.reason_code(), "semantic_manifest_path_invalid");
        }

        let base = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            b"case-base",
            &corpus,
        );
        let mut ann = bind_ann_to_base(
            complete_generation_artifact(
                SemanticArtifactRole::FastAnn,
                "FAST/PRIMARY.FSVI",
                b"case-ann",
                &corpus,
            ),
            &base,
        );
        ann.ann_base.as_mut().unwrap().parameters_sha256 = ann.artifact_parameters_sha256.clone();
        let collision =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![base, ann]);
        assert_eq!(
            collision.validate().unwrap_err().reason_code(),
            "semantic_manifest_path_invalid"
        );

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let artifact = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            b"symlink-vector",
            &corpus,
        );
        let manifest = generation_with_id(
            "gen-symlink",
            SemanticGenerationTopology::FastOnly,
            vec![artifact],
        );
        let generation_dir = manifest.generation_dir(temp.path()).unwrap();
        fs::create_dir_all(generation_dir.join("fast")).unwrap();
        let outside_file = outside.path().join("outside.fsvi");
        fs::write(&outside_file, b"symlink-vector").unwrap();
        symlink(&outside_file, generation_dir.join("fast/primary.fsvi")).unwrap();
        assert!(matches!(
            manifest.write_immutable(temp.path()),
            Err(SemanticGenerationError::ArtifactIo { .. })
                | Err(SemanticGenerationError::ManifestIo { .. })
        ));
        assert!(!manifest.manifest_path(temp.path()).unwrap().exists());

        let alias_temp = tempfile::tempdir().unwrap();
        let base = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            b"same-physical-file",
            &corpus,
        );
        let ann = bind_ann_to_base(
            complete_generation_artifact(
                SemanticArtifactRole::FastAnn,
                "fast/search.hnsw",
                b"same-physical-file",
                &corpus,
            ),
            &base,
        );
        let alias_manifest = generation_with_id(
            "gen-hardlink",
            SemanticGenerationTopology::FastOnly,
            vec![base, ann],
        );
        let generation_dir = alias_manifest.generation_dir(alias_temp.path()).unwrap();
        fs::create_dir_all(generation_dir.join("fast")).unwrap();
        let vector_path = generation_dir.join("fast/primary.fsvi");
        fs::write(&vector_path, b"same-physical-file").unwrap();
        fs::hard_link(&vector_path, generation_dir.join("fast/search.hnsw")).unwrap();
        assert!(matches!(
            alias_manifest.write_immutable(alias_temp.path()),
            Err(SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::Path,
                ..
            })
        ));

        let external_link_temp = tempfile::tempdir().unwrap();
        let external_link_target = tempfile::tempdir().unwrap();
        let external_artifact = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            b"externally-linked",
            &corpus,
        );
        let external_manifest = generation_with_id(
            "gen-external-hardlink",
            SemanticGenerationTopology::FastOnly,
            vec![external_artifact],
        );
        write_generation_artifacts(
            external_link_temp.path(),
            &external_manifest,
            &BTreeMap::from([(
                SemanticArtifactRole::FastVector,
                b"externally-linked".to_vec(),
            )]),
        );
        let external_artifact_path = external_manifest
            .generation_dir(external_link_temp.path())
            .unwrap()
            .join(&external_manifest.artifacts[0].relative_path);
        fs::hard_link(
            &external_artifact_path,
            external_link_target.path().join("alias.fsvi"),
        )
        .unwrap();
        assert!(matches!(
            external_manifest.write_immutable(external_link_temp.path()),
            Err(SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::Path,
                ..
            })
        ));

        let swap_temp = tempfile::tempdir().unwrap();
        let selected_path = swap_temp.path().join("selected.fsvi");
        fs::write(&selected_path, b"owner-a").unwrap();
        let selected_file = fs::File::open(&selected_path).unwrap();
        let selected_metadata = selected_file.metadata().unwrap();
        let selected_identity = portable_file_identity(&selected_file, &selected_metadata).unwrap();
        verify_path_still_names_open_file(&selected_path, selected_identity).unwrap();
        fs::rename(&selected_path, swap_temp.path().join("displaced.fsvi")).unwrap();
        fs::write(&selected_path, b"owner-b").unwrap();
        assert!(
            verify_path_still_names_open_file(&selected_path, selected_identity).is_err(),
            "post-open path replacement must not retain the prior verified identity"
        );
    }

    #[cfg(windows)]
    #[test]
    fn accepted_windows_publication_uses_write_through_replace_and_no_clobber() {
        let temp = tempfile::tempdir().unwrap();
        let final_path = temp.path().join("current.json");
        fs::write(&final_path, b"old-pointer").unwrap();

        let mut replacement = tempfile::Builder::new()
            .prefix(".current.write-through.")
            .tempfile_in(temp.path())
            .unwrap();
        replacement.write_all(b"new-pointer").unwrap();
        replacement.as_file().sync_all().unwrap();
        let persisted = persist_named_temp(replacement, &final_path, true).unwrap();
        persisted.sync_all().unwrap();
        assert_eq!(fs::read(&final_path).unwrap(), b"new-pointer");

        let mut conflicting = tempfile::Builder::new()
            .prefix(".manifest.no-clobber.")
            .tempfile_in(temp.path())
            .unwrap();
        conflicting.write_all(b"conflicting-manifest").unwrap();
        conflicting.as_file().sync_all().unwrap();
        let error = persist_named_temp(conflicting, &final_path, false)
            .expect_err("no-clobber publication must preserve an existing final");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&final_path).unwrap(), b"new-pointer");
    }

    #[test]
    fn accepted_two_stage_schema_dispatch_classifies_future_envelopes_as_upgrade_required() {
        let pointer = serde_json::json!({
            "schema_version": u64::from(SEMANTIC_CURRENT_POINTER_SCHEMA_VERSION) + 1,
            "generation_id": "gen-future-pointer",
            "manifest_sha256": test_sha256("future-pointer"),
            "selection_epoch": 1,
            "selected_at_ms": 1_700_000_000_400_i64,
            "future_selection_epoch": 7,
        });
        let error = parse_current_pointer_bytes(&serde_json::to_vec(&pointer).unwrap())
            .expect_err("future pointer with added fields must dispatch before strict V1 parsing");
        assert!(matches!(
            &error,
            SemanticGenerationError::UnsupportedPointerSchema { .. }
        ));
        assert_eq!(
            error.recovery_action(),
            SemanticGenerationRecoveryAction::UpgradeRequired
        );

        let corpus = test_corpus_identity();
        let artifact = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            b"future-schema-vector",
            &corpus,
        );
        let manifest =
            test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);
        let mut future_manifest = serde_json::to_value(&manifest).unwrap();
        future_manifest["schema_version"] =
            serde_json::json!(u64::from(SEMANTIC_GENERATION_MANIFEST_SCHEMA_VERSION) + 1);
        future_manifest.as_object_mut().unwrap().insert(
            "future_provenance".to_owned(),
            serde_json::json!({"epoch": 2}),
        );
        let error = parse_generation_manifest_bytes(
            &serde_json::to_vec(&future_manifest).unwrap(),
            &manifest.generation_id,
        )
        .expect_err("future manifest with added fields must dispatch before strict V1 parsing");
        assert!(matches!(
            &error,
            SemanticGenerationError::UnsupportedManifestSchema { .. }
        ));
        assert_eq!(
            error.recovery_action(),
            SemanticGenerationRecoveryAction::UpgradeRequired
        );

        for (component, supported) in [
            ("space", u64::from(EMBEDDING_SPACE_IDENTITY_SCHEMA_V1)),
            (
                "producer",
                u64::from(EMBEDDING_PRODUCER_ATTESTATION_SCHEMA_V1),
            ),
            ("input", u64::from(EMBEDDING_INPUT_CONTRACT_SCHEMA_V1)),
            ("storage", u64::from(VECTOR_STORAGE_IDENTITY_SCHEMA_V1)),
        ] {
            let mut future_identity = serde_json::to_value(&manifest).unwrap();
            future_identity["artifacts"][0]["embedding_identity"]["identity"][component]["schema_version"] =
                serde_json::json!(supported + 1);
            future_identity["artifacts"][0]["embedding_identity"]["identity"][component]
                .as_object_mut()
                .unwrap()
                .insert(
                    "future_component_contract".to_owned(),
                    serde_json::json!({"epoch": 2}),
                );
            let error = parse_generation_manifest_bytes(
                &serde_json::to_vec(&future_identity).unwrap(),
                &manifest.generation_id,
            )
            .expect_err("future embedding identity must classify as upgrade-required");
            assert!(matches!(
                &error,
                SemanticGenerationError::UnsupportedEmbeddingIdentitySchema {
                    component: actual,
                    ..
                } if actual == component
            ));
            assert_eq!(
                error.recovery_action(),
                SemanticGenerationRecoveryAction::UpgradeRequired
            );
        }

        let mut current_unknown = serde_json::to_value(&manifest).unwrap();
        current_unknown
            .as_object_mut()
            .unwrap()
            .insert("unversioned_unknown".to_owned(), serde_json::json!(true));
        assert!(matches!(
            parse_generation_manifest_bytes(
                &serde_json::to_vec(&current_unknown).unwrap(),
                &manifest.generation_id,
            ),
            Err(SemanticGenerationError::ManifestParse { .. })
        ));
    }

    #[test]
    fn accepted_disk_resolution_is_exhaustive_with_robot_and_human_goldens() {
        // Rich step-by-step remediation plans remain the ds7uy.4 surface.
        // This minimal contract still has a distinct, stable state/reason/action
        // for every authoritative on-disk condition so callers never infer
        // recovery from free-form text.
        let absent = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_semantic_generation_disk_state(absent.path()).unwrap(),
            SemanticGenerationDiskStateV1::Absent
        );
        assert_eq!(
            serde_json::to_string_pretty(&SemanticGenerationDiskStateV1::Absent.robot_projection())
                .unwrap(),
            r#"{
  "schema_version": 1,
  "state": {
    "state": "absent"
  },
  "reason_code": "semantic_generation_absent",
  "recovery_action": "provision_and_index"
}"#
        );

        let legacy_root = tempfile::tempdir().unwrap();
        let legacy_vector_root = semantic_vector_root(legacy_root.path());
        fs::create_dir_all(&legacy_vector_root).unwrap();
        fs::write(
            legacy_vector_root.join("semantic_manifest.json"),
            b"legacy-ledger",
        )
        .unwrap();
        fs::write(legacy_vector_root.join("vector.fast.idx"), b"legacy-vector").unwrap();
        let legacy = resolve_semantic_generation_disk_state(legacy_root.path()).unwrap();
        assert_eq!(
            legacy,
            SemanticGenerationDiskStateV1::LegacyUnidentified { artifact_count: 2 }
        );
        assert_eq!(
            serde_json::to_string_pretty(&legacy.robot_projection()).unwrap(),
            r#"{
  "schema_version": 1,
  "state": {
    "state": "legacy_unidentified",
    "artifact_count": 2
  },
  "reason_code": "semantic_generation_legacy_unidentified",
  "recovery_action": "reindex_required"
}"#
        );

        let pointer_missing_root = tempfile::tempdir().unwrap();
        let pointer_missing_vector_root = semantic_vector_root(pointer_missing_root.path());
        fs::create_dir_all(
            pointer_missing_vector_root
                .join(SEMANTIC_GENERATIONS_DIRNAME)
                .join("gen-unselected"),
        )
        .unwrap();
        fs::write(
            pointer_missing_vector_root.join("vector.fast.idx"),
            b"legacy-vector",
        )
        .unwrap();
        let pointer_missing =
            resolve_semantic_generation_disk_state(pointer_missing_root.path()).unwrap();
        assert_eq!(
            pointer_missing,
            SemanticGenerationDiskStateV1::PointerMissingWithGenerationDirectories {
                generation_directory_count: 1,
                legacy_artifact_count: 1,
            }
        );
        assert_eq!(
            serde_json::to_string_pretty(&pointer_missing.robot_projection()).unwrap(),
            r#"{
  "schema_version": 1,
  "state": {
    "state": "pointer_missing_with_generation_directories",
    "generation_directory_count": 1,
    "legacy_artifact_count": 1
  },
  "reason_code": "semantic_pointer_missing_with_generation_directories",
  "recovery_action": "rollback_or_reindex"
}"#
        );

        let building_root = tempfile::tempdir().unwrap();
        let building_vector_root = semantic_vector_root(building_root.path());
        fs::create_dir_all(
            building_vector_root
                .join(SEMANTIC_STAGING_DIRNAME)
                .join("build-in-progress"),
        )
        .unwrap();
        fs::create_dir_all(
            building_vector_root
                .join(SEMANTIC_GENERATIONS_DIRNAME)
                .join("gen-unselected"),
        )
        .unwrap();
        fs::write(
            building_vector_root.join("vector.fast.idx"),
            b"legacy-vector",
        )
        .unwrap();
        let building = resolve_semantic_generation_disk_state(building_root.path()).unwrap();
        assert_eq!(
            building,
            SemanticGenerationDiskStateV1::Building {
                build_count: 1,
                unselected_generation_directory_count: 1,
                legacy_artifact_count: 1,
            }
        );
        assert_eq!(
            serde_json::to_string_pretty(&building.robot_projection()).unwrap(),
            r#"{
  "schema_version": 1,
  "state": {
    "state": "building",
    "build_count": 1,
    "unselected_generation_directory_count": 1,
    "legacy_artifact_count": 1
  },
  "reason_code": "semantic_generation_building",
  "recovery_action": "wait_for_build"
}"#
        );

        let interrupted_root = tempfile::tempdir().unwrap();
        let interrupted_vector_root = semantic_vector_root(interrupted_root.path());
        fs::create_dir_all(
            interrupted_vector_root
                .join(SEMANTIC_STAGING_DIRNAME)
                .join("build-in-progress"),
        )
        .unwrap();
        fs::create_dir_all(
            interrupted_vector_root
                .join(SEMANTIC_GENERATIONS_DIRNAME)
                .join("gen-unselected"),
        )
        .unwrap();
        fs::write(
            interrupted_vector_root.join("vector.fast.idx"),
            b"legacy-vector",
        )
        .unwrap();
        fs::write(
            interrupted_vector_root.join(".current.publish.A1b2C3"),
            b"torn-pointer-publication",
        )
        .unwrap();
        let interrupted = resolve_semantic_generation_disk_state(interrupted_root.path()).unwrap();
        assert_eq!(
            interrupted,
            SemanticGenerationDiskStateV1::InterruptedPublish {
                temporary_entry_count: 1,
                build_count: 1,
                unselected_generation_directory_count: 1,
                legacy_artifact_count: 1,
            }
        );
        assert_eq!(
            serde_json::to_string_pretty(&interrupted.robot_projection()).unwrap(),
            r#"{
  "schema_version": 1,
  "state": {
    "state": "interrupted_publish",
    "temporary_entry_count": 1,
    "build_count": 1,
    "unselected_generation_directory_count": 1,
    "legacy_artifact_count": 1
  },
  "reason_code": "semantic_pointer_publish_interrupted",
  "recovery_action": "retry_publish"
}"#
        );

        assert!(is_private_pointer_publish_temp_name(OsStr::new(
            ".current.publish.A1b2C3"
        )));
        for lookalike in [
            ".current.publish.",
            ".current.publish.contains-dash",
            ".current.publish.contains.dot",
            ".current.publish.non_ascii_é",
            "current.publish.A1b2C3",
        ] {
            assert!(!is_private_pointer_publish_temp_name(OsStr::new(lookalike)));
        }
        let directory_lookalike_root = tempfile::tempdir().unwrap();
        fs::create_dir_all(
            semantic_vector_root(directory_lookalike_root.path()).join(".current.publish.A1b2C3"),
        )
        .unwrap();
        assert_eq!(
            resolve_semantic_generation_disk_state(directory_lookalike_root.path()).unwrap(),
            SemanticGenerationDiskStateV1::LegacyUnidentified { artifact_count: 1 }
        );
        let malformed_generation_root = tempfile::tempdir().unwrap();
        let malformed_generations = semantic_generations_root(malformed_generation_root.path());
        fs::create_dir_all(&malformed_generations).unwrap();
        fs::write(
            malformed_generations.join("not-a-generation-directory"),
            b"untrusted-entry",
        )
        .unwrap();
        assert!(matches!(
            resolve_semantic_generation_disk_state(malformed_generation_root.path()),
            Err(SemanticGenerationError::InvalidPointer { reason })
                if reason == "semantic generations root contains a non-directory entry"
        ));

        let ready_digest = test_sha256("ready-manifest");
        let ready = SemanticGenerationDiskStateV1::Ready {
            generation_id: "gen-ready-1234567890abcdef".to_owned(),
            manifest_sha256: ready_digest.clone(),
            selection_epoch: 7,
            selected_at_ms: 1_700_000_000_700,
            requested_topology: SemanticGenerationTopology::FullProgressive,
            realized_topology: SemanticGenerationTopology::FastOnly,
        };
        assert_eq!(
            serde_json::to_string_pretty(&ready.robot_projection()).unwrap(),
            format!(
                r#"{{
  "schema_version": 1,
  "state": {{
    "state": "ready",
    "generation_id": "gen-ready-1234567890abcdef",
    "manifest_sha256": "{ready_digest}",
    "selection_epoch": 7,
    "selected_at_ms": 1700000000700,
    "requested_topology": "full_progressive",
    "realized_topology": "fast_only"
  }},
  "reason_code": null,
  "recovery_action": null
}}"#
            )
        );
        assert_eq!(
            SemanticGenerationDiskStateV1::Absent.to_string(),
            "no semantic generation exists; provision an embedder and build the initial index"
        );
        assert_eq!(
            legacy.to_string(),
            "legacy semantic artifacts without authoritative identity: 2"
        );
        assert_eq!(
            pointer_missing.to_string(),
            "current semantic pointer is missing; unselected generation candidate directory count: 1; legacy artifact count: 1; validate a candidate fully before rollback, otherwise reindex"
        );
        assert_eq!(
            building.to_string(),
            "staged semantic build count: 1; unselected generation candidate directory count: 1; unselected legacy artifact count: 1"
        );
        assert_eq!(
            interrupted.to_string(),
            "interrupted pointer publication temp count: 1; staged build count: 1; unselected generation candidate directory count: 1; legacy artifact count: 1"
        );
        let ready_summary = ready.to_string();
        assert!(ready_summary.contains("gen-ready-12"));
        assert!(!ready_summary.contains(&ready_digest));
    }

    #[test]
    fn accepted_errors_are_typed_redacted_and_do_not_misclassify_read_io_as_publish_retry() {
        let expected = "a".repeat(64);
        let actual = "b".repeat(64);
        let digest_error = SemanticGenerationError::ManifestDigestMismatch {
            expected: expected.clone(),
            actual: actual.clone(),
        };
        let rendered = digest_error.to_string();
        assert!(!rendered.contains(&expected));
        assert!(!rendered.contains(&actual));
        assert!(rendered.contains(&expected[..12]));
        assert_eq!(
            SemanticGenerationError::ArtifactIo {
                role: SemanticArtifactRole::FastVector,
                source: "/private/path".to_owned(),
            }
            .recovery_action(),
            SemanticGenerationRecoveryAction::RollbackOrReindex
        );

        let corpus = test_corpus_identity();
        let mut partial = test_generation_artifact(
            SemanticArtifactRole::QualityVector,
            "quality/partial.fsvi",
            b"partial",
            &corpus,
            1,
        );
        partial.covered_content_sha256 = corpus.content_sha256.clone();
        partial.validation.covered_content_sha256 = corpus.content_sha256.clone();
        let error =
            test_generation_manifest(SemanticGenerationTopology::QualityOnly, vec![partial])
                .validate()
                .unwrap_err();
        assert!(matches!(
            error,
            SemanticGenerationError::InvalidManifest {
                class: SemanticManifestInvariantClass::PartialCoverage,
                ..
            }
        ));

        let mut building = test_generation_manifest(
            SemanticGenerationTopology::FastOnly,
            vec![complete_generation_artifact(
                SemanticArtifactRole::FastVector,
                "fast/primary.fsvi",
                b"building",
                &corpus,
            )],
        );
        building.status = SemanticGenerationStatus::Building;
        let error = building.validate().unwrap_err();
        assert_eq!(error.reason_code(), "semantic_generation_building");
        assert_eq!(
            error.recovery_action(),
            SemanticGenerationRecoveryAction::WaitForBuild
        );
    }

    #[test]
    fn accepted_structured_events_include_context_and_redact_paths_and_full_hashes() {
        use std::process::{Command, Stdio};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use tracing_subscriber::prelude::*;
        use wait_timeout::ChildExt;

        const TRACE_CAPTURE_CHILD_ENV: &str = "CASS_SEMANTIC_MANIFEST_TRACE_CAPTURE_ISOLATED_V1";
        const TRACE_CAPTURE_TEST_NAME: &str = "search::semantic_manifest::tests::\
            accepted_structured_events_include_context_and_redact_paths_and_full_hashes";
        const TRACE_CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

        if std::env::var_os(TRACE_CAPTURE_CHILD_ENV).is_none() {
            let mut child =
                Command::new(std::env::current_exe().expect("current library test binary")) // ubs:ignore — fixed current test binary.
                    .arg("--exact")
                    .arg(TRACE_CAPTURE_TEST_NAME)
                    .arg("--nocapture")
                    .env(TRACE_CAPTURE_CHILD_ENV, "1")
                    .stdin(Stdio::null())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .expect("spawn isolated semantic-manifest tracing test");
            let status = child
                .wait_timeout(TRACE_CAPTURE_TIMEOUT)
                .expect("wait for isolated semantic-manifest tracing test");
            if status.is_none() {
                child
                    .kill()
                    .expect("kill timed-out semantic-manifest tracing test");
                let _ = child.wait();
            }
            let status =
                status.expect("isolated semantic-manifest tracing test exceeded 30-second timeout");
            assert!(
                status.success(),
                "isolated semantic-manifest tracing test failed with {status}"
            );
            return;
        }

        #[derive(Clone)]
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for SharedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("log buffer").extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedWriter {
            type Writer = Self;

            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let corpus = test_corpus_identity();
        let bytes = b"logged-vector".to_vec();
        let artifact = complete_generation_artifact(
            SemanticArtifactRole::FastVector,
            "fast/primary.fsvi",
            &bytes,
            &corpus,
        );
        let manifest = generation_with_id(
            "gen-structured-log",
            SemanticGenerationTopology::FastOnly,
            vec![artifact],
        );
        write_generation_artifacts(
            temp.path(),
            &manifest,
            &BTreeMap::from([(SemanticArtifactRole::FastVector, bytes)]),
        );
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let filter = tracing_subscriber::filter::dynamic_filter_fn(|_, _| true)
            .with_max_level_hint(tracing_subscriber::filter::LevelFilter::TRACE);
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .without_time()
            .with_writer(SharedWriter(Arc::clone(&buffer)))
            .with_filter(filter);
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::callsite::rebuild_interest_cache();
            manifest.write_immutable(temp.path()).unwrap();
            let pointer =
                SemanticCurrentPointerV1::for_manifest(&manifest, manifest.sealed_at_ms + 1, 1)
                    .unwrap();
            pointer
                .publish_atomic(temp.path(), &SemanticExpectedCurrentV1::Absent)
                .unwrap();
            load_current_semantic_generation(temp.path(), Some(&corpus)).unwrap();
            let artifact_path = manifest
                .generation_dir(temp.path())
                .unwrap()
                .join("fast/primary.fsvi");
            fs::write(artifact_path, b"tamper-vector").unwrap();
            assert!(load_current_semantic_generation(temp.path(), Some(&corpus)).is_err());
        });
        let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        for field in [
            "semantic.manifest.artifact.validate",
            "semantic.manifest.seal",
            "semantic.pointer.swap",
            "semantic.manifest.load",
            "semantic.manifest.validate",
            "build",
            "request",
            "requested_topology",
            "realized_topology",
            "tier",
            "coverage",
            "artifact",
            "expected_identity",
            "actual_identity",
            "reason_code",
        ] {
            assert!(
                logs.contains(field),
                "missing structured field/event {field}; captured logs:\n{logs}"
            );
        }
        assert!(logs.contains("fast/primary.fsvi"));
        assert!(!logs.contains(&temp.path().display().to_string()));
        assert!(!logs.contains(&manifest.artifacts[0].artifact_sha256));
        assert!(!logs.contains(&manifest.producer.binary_elf_sha256));
        assert!(logs.contains(fingerprint_prefix(&manifest.artifacts[0].artifact_sha256)));
        assert!(logs.contains(fingerprint_prefix(&sha256_hex(b"tamper-vector"))));
    }

    proptest! {
        #[test]
        fn accepted_property_rejects_parent_alias_paths(
            directory in "[A-Za-z0-9_-]{1,64}",
            filename in "[A-Za-z0-9_-]{1,64}\\.fsvi",
        ) {
            let corpus = test_corpus_identity();
            let traversal = format!("{directory}/../{filename}");
            let artifact = complete_generation_artifact(
                SemanticArtifactRole::FastVector,
                &traversal,
                b"property-vector",
                &corpus,
            );
            let candidate =
                test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);
            let rejected_as_path_invariant = matches!(
                candidate.validate(),
                Err(SemanticGenerationError::InvalidManifest {
                    class: SemanticManifestInvariantClass::Path,
                    ..
                })
            );
            prop_assert!(
                rejected_as_path_invariant,
                "parent-alias paths must fail the semantic manifest path invariant"
            );
        }

        #[test]
        fn accepted_property_digest_is_stable_and_generation_sensitive(
            suffix in "[A-Za-z0-9_-]{1,32}",
        ) {
            let corpus = test_corpus_identity();
            let artifact = complete_generation_artifact(
                SemanticArtifactRole::FastVector,
                "fast/primary.fsvi",
                b"property-vector",
                &corpus,
            );
            let base =
                test_generation_manifest(SemanticGenerationTopology::FastOnly, vec![artifact]);
            let mut changed = base.clone();
            changed.generation_id = format!("gen-{suffix}");
            prop_assert_eq!(base.sha256().unwrap(), base.sha256().unwrap());
            prop_assert_ne!(base.sha256().unwrap(), changed.sha256().unwrap());
        }
    }
}
