//! Durable semantic asset manifest, backlog ledger, and resumable checkpoints.
//!
//! This module is the authoritative state model for semantic assets.  It tells
//! cass exactly what semantic artifacts exist, how trustworthy they are, and
//! what work remains to converge the corpus — enabling partial readiness,
//! resumable builds, and truthful runtime degradation.
//!
//! # Storage
//!
//! The manifest is a single JSON file at:
//! ```text
//! {data_dir}/vector_index/semantic_manifest.json
//! ```
//!
//! It is written atomically (write-to-temp then rename) and is the only file
//! the backfill worker needs to consult to know what work remains.
//!
//! # Relationship to other modules
//!
//! - **[`policy`]**: Provides the contract (versions, budgets, tier names) that
//!   this manifest fingerprints against.
//! - **[`asset_state`]**: Evaluates coarse readiness from this manifest plus
//!   live file probes.
//! - **[`model_manager`]**: Detects model availability; this module records
//!   which model was used to build each artifact.

use std::fs;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::policy::{
    CHUNKING_STRATEGY_VERSION, InvalidationAction, SEMANTIC_SCHEMA_VERSION,
    SemanticAssetManifest as PolicyManifest, SemanticPolicy,
};

// ─── Constants ─────────────────────────────────────────────────────────────

/// Current manifest format version.  Bump when the JSON schema changes in a
/// backwards-incompatible way.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

/// Filename for the durable manifest.
pub const MANIFEST_FILENAME: &str = "semantic_manifest.json";

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

// ─── Build checkpoint ──────────────────────────────────────────────────────

/// Resumable position for an interrupted semantic build.
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
}

impl BuildCheckpoint {
    /// Progress as a percentage (0–100).
    pub fn progress_pct(&self) -> u8 {
        if self.total_conversations == 0 {
            return 0;
        }
        let pct = (self.conversations_processed as f64 / self.total_conversations as f64) * 100.0;
        (pct as u8).min(100)
    }

    /// Whether the build is complete (all conversations processed).
    pub fn is_complete(&self) -> bool {
        self.conversations_processed >= self.total_conversations
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
        let tmp_path = unique_manifest_temp_path(&path);
        let mut file = fs::File::create(&tmp_path).map_err(|e| ManifestError::Io {
            path: tmp_path.clone(),
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

fn unique_manifest_temp_path(path: &Path) -> PathBuf {
    static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(MANIFEST_FILENAME);
    let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}.{}",
        std::process::id(),
        now_ms(),
        nonce
    ))
}

#[cfg(windows)]
fn unique_manifest_backup_path(path: &Path) -> PathBuf {
    static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(MANIFEST_FILENAME);
    let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_file_name(format!(
        ".{file_name}.bak.{}.{}.{}",
        std::process::id(),
        now_ms(),
        nonce
    ))
}

fn replace_file_from_temp(temp_path: &Path, final_path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        match fs::rename(temp_path, final_path) {
            Ok(()) => sync_parent_directory(final_path),
            Err(first_err)
                if final_path.exists()
                    && matches!(
                        first_err.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                    ) =>
            {
                let backup_path = unique_manifest_backup_path(final_path);
                fs::rename(final_path, &backup_path).map_err(|backup_err| {
                    let _ = fs::remove_file(temp_path);
                    std::io::Error::other(format!(
                        "failed preparing backup {} before replacing {}: first error: {}; backup error: {}",
                        backup_path.display(),
                        final_path.display(),
                        first_err,
                        backup_err
                    ))
                })?;
                match fs::rename(temp_path, final_path) {
                    Ok(()) => {
                        let _ = fs::remove_file(&backup_path);
                        sync_parent_directory(final_path)
                    }
                    Err(second_err) => match fs::rename(&backup_path, final_path) {
                        Ok(()) => {
                            let _ = fs::remove_file(temp_path);
                            sync_parent_directory(final_path)?;
                            Err(std::io::Error::other(format!(
                                "failed replacing {} with {}: first error: {}; second error: {}; restored original file",
                                final_path.display(),
                                temp_path.display(),
                                first_err,
                                second_err
                            )))
                        }
                        Err(restore_err) => Err(std::io::Error::other(format!(
                            "failed replacing {} with {}: first error: {}; second error: {}; restore error: {}; temp file retained at {}",
                            final_path.display(),
                            temp_path.display(),
                            first_err,
                            second_err,
                            restore_err,
                            temp_path.display()
                        ))),
                    },
                }
            }
            Err(err) => Err(err),
        }
    }

    #[cfg(not(windows))]
    {
        fs::rename(temp_path, final_path)
    }
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let directory = fs::File::open(parent)?;
    directory.sync_all()
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::policy::SemanticPolicy;

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
        }
    }

    // ── Manifest load/save round-trip ──────────────────────────────────

    #[test]
    fn manifest_round_trip_via_disk() {
        let temp = tempfile::tempdir().unwrap();
        let mut manifest = SemanticManifest::default();
        manifest.fast_tier = Some(test_artifact(TierKind::Fast, true));
        manifest.quality_tier = Some(test_artifact(TierKind::Quality, true));
        manifest.hnsw = Some(test_hnsw());
        manifest.checkpoint = Some(test_checkpoint(TierKind::Quality));
        manifest.backlog = BacklogLedger {
            total_conversations: 2000,
            fast_tier_processed: 1000,
            quality_tier_processed: 500,
            db_fingerprint: "fp-1234".to_owned(),
            computed_at_ms: 1_700_000_000_000,
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
        let mut first = SemanticManifest::default();
        first.fast_tier = Some(test_artifact(TierKind::Fast, true));
        first.save(temp.path()).unwrap();

        let mut second = SemanticManifest::default();
        second.quality_tier = Some(test_artifact(TierKind::Quality, true));
        second.backlog = BacklogLedger {
            total_conversations: 99,
            fast_tier_processed: 0,
            quality_tier_processed: 99,
            db_fingerprint: "fp-overwrite".to_owned(),
            computed_at_ms: 1_700_000_000_123,
        };
        second.save(temp.path()).unwrap();

        let loaded = SemanticManifest::load(temp.path()).unwrap().unwrap();
        assert!(loaded.fast_tier.is_none());
        assert!(loaded.quality_tier.is_some());
        assert_eq!(loaded.backlog.total_conversations, 99);
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

        let mut manifest = SemanticManifest::default();
        manifest.manifest_version = MANIFEST_FORMAT_VERSION + 1;
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

        // Case 1: Ready artifact, matching fingerprint → Ready
        let artifact = test_artifact(TierKind::Fast, true);
        assert_eq!(
            artifact.readiness(&policy, db_fp, model_rev),
            TierReadiness::Ready,
        );

        // Case 2: Ready artifact, DB fingerprint changed → Stale
        let artifact = test_artifact(TierKind::Fast, true);
        assert!(matches!(
            artifact.readiness(&policy, "different-fp", model_rev),
            TierReadiness::Stale { .. },
        ));

        // Case 3: Ready artifact, model revision changed → Stale
        let artifact = test_artifact(TierKind::Quality, true);
        assert!(matches!(
            artifact.readiness(&policy, db_fp, "new-revision"),
            TierReadiness::Stale { .. },
        ));

        // Case 4: Schema version mismatch → Incompatible
        let mut artifact = test_artifact(TierKind::Quality, true);
        artifact.schema_version = 0;
        assert!(matches!(
            artifact.readiness(&policy, db_fp, model_rev),
            TierReadiness::Incompatible { .. },
        ));

        // Case 5: Not yet published (ready=false) → Building
        let artifact = test_artifact(TierKind::Fast, false);
        assert!(matches!(
            artifact.readiness(&policy, db_fp, model_rev),
            TierReadiness::Building { progress_pct: 100 },
        ));
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
        let mut manifest = SemanticManifest::default();
        manifest.checkpoint = Some(test_checkpoint(TierKind::Quality));

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
        let mut manifest = SemanticManifest::default();
        manifest.checkpoint = Some(test_checkpoint(TierKind::Quality));

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
        let mut manifest = SemanticManifest::default();
        manifest.fast_tier = Some(test_artifact(TierKind::Fast, true));
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
    fn checkpoint_progress_and_completion() {
        let cp = test_checkpoint(TierKind::Quality);
        assert_eq!(cp.progress_pct(), 50);
        assert!(!cp.is_complete());
        assert!(cp.is_valid("fp-1234"));
        assert!(!cp.is_valid("other-fp"));

        // Complete checkpoint
        let mut cp = test_checkpoint(TierKind::Quality);
        cp.conversations_processed = 1000;
        assert_eq!(cp.progress_pct(), 100);
        assert!(cp.is_complete());
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
        let mut manifest = SemanticManifest::default();
        manifest.checkpoint = Some(test_checkpoint(TierKind::Quality));

        manifest.publish_artifact(test_artifact(TierKind::Quality, true));
        assert!(manifest.checkpoint.is_none());
        assert!(manifest.quality_tier.is_some());
    }

    #[test]
    fn publish_artifact_keeps_non_matching_checkpoint() {
        let mut manifest = SemanticManifest::default();
        manifest.checkpoint = Some(test_checkpoint(TierKind::Quality));

        manifest.publish_artifact(test_artifact(TierKind::Fast, true));
        assert!(manifest.checkpoint.is_some()); // Quality checkpoint survives
        assert!(manifest.fast_tier.is_some());
    }

    // ── Refresh backlog ────────────────────────────────────────────────

    #[test]
    fn refresh_backlog_computes_from_ready_artifacts() {
        let mut manifest = SemanticManifest::default();
        manifest.fast_tier = Some(test_artifact(TierKind::Fast, true));
        manifest.quality_tier = Some(test_artifact(TierKind::Quality, true));

        manifest.refresh_backlog(2000, "fp-1234");
        assert_eq!(manifest.backlog.total_conversations, 2000);
        assert_eq!(manifest.backlog.fast_tier_processed, 250);
        assert_eq!(manifest.backlog.quality_tier_processed, 250);
    }

    #[test]
    fn refresh_backlog_ignores_stale_artifacts() {
        let mut manifest = SemanticManifest::default();
        manifest.fast_tier = Some(test_artifact(TierKind::Fast, true));

        // DB fingerprint doesn't match → artifact not counted
        manifest.refresh_backlog(2000, "different-fp");
        assert_eq!(manifest.backlog.fast_tier_processed, 0);
    }

    // ── Invalidation ───────────────────────────────────────────────────

    #[test]
    fn invalidate_incompatible_removes_schema_mismatch() {
        let mut manifest = SemanticManifest::default();
        let mut artifact = test_artifact(TierKind::Quality, true);
        artifact.schema_version = 0; // mismatch
        manifest.quality_tier = Some(artifact);
        manifest.hnsw = Some(test_hnsw()); // depends on quality tier

        let policy = test_policy();
        let count = manifest.invalidate_incompatible(&policy, "abc123");

        assert_eq!(count, 2); // quality + hnsw
        assert!(manifest.quality_tier.is_none());
        assert!(manifest.hnsw.is_none());
    }

    #[test]
    fn invalidate_incompatible_keeps_compatible() {
        let mut manifest = SemanticManifest::default();
        manifest.fast_tier = Some(test_artifact(TierKind::Fast, true));
        manifest.quality_tier = Some(test_artifact(TierKind::Quality, true));

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
        let mut manifest = SemanticManifest::default();
        manifest.fast_tier = Some(test_artifact(TierKind::Fast, true));
        manifest.quality_tier = Some(test_artifact(TierKind::Quality, true));
        manifest.hnsw = Some(test_hnsw());

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
        let mut manifest = SemanticManifest::default();
        manifest.fast_tier = Some(test_artifact(TierKind::Fast, true));
        manifest.quality_tier = Some(test_artifact(TierKind::Quality, true));
        manifest.hnsw = Some(test_hnsw());
        manifest.checkpoint = Some(test_checkpoint(TierKind::Quality));

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let deser: SemanticManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.fast_tier, manifest.fast_tier);
        assert_eq!(deser.quality_tier, manifest.quality_tier);
        assert_eq!(deser.hnsw, manifest.hnsw);
        assert_eq!(deser.checkpoint, manifest.checkpoint);
    }
}
