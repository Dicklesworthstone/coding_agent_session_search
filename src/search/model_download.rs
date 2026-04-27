//! Model download and management system.
//!
//! This module handles:
//! - Model manifest with SHA256 checksums
//! - State machine for download lifecycle
//! - Resumable downloads with progress reporting
//! - SHA256 verification
//! - Atomic installation (temp dir -> rename)
//! - Model version upgrade detection
//!
//! **Note**: The core types (`ModelState`, `ModelFile`, `ModelManifest`) are
//! structurally identical to those in `frankensearch_embed::model_manifest`.
//! They are kept locally for now due to build-system sync constraints.
//! See frankensearch-embed for the canonical definitions.
//!
//! **Network Policy**: No network calls occur without explicit user consent.
//! The download system is consent-gated via [`ModelState::NeedsConsent`].

use std::collections::HashSet;
use std::fs::{self, File};
use std::future::{Future, poll_fn};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use asupersync::bytes::Buf;
use asupersync::http::Body;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::search::policy::{ModelDownloadPolicy, SemanticPolicy};

/// Model state machine for download lifecycle.
///
/// State transitions:
/// ```text
/// NotInstalled ──> NeedsConsent ──> Downloading ──> Verifying ──> Ready
///                       │                │              │
///                       │                │              └──> VerificationFailed
///                       │                └──────────────────> Cancelled
///                       └────────────────────────────────────> Disabled
///
/// Ready ──> UpdateAvailable ──> Downloading (upgrade) ──> Verifying ──> Ready
/// ```
///
/// Structurally identical to `frankensearch_embed::ModelState`.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelState {
    /// Model not installed on disk.
    NotInstalled,
    /// User consent required before download.
    NeedsConsent,
    /// Download in progress.
    Downloading {
        /// Progress percentage (0-100).
        progress_pct: u8,
        /// Bytes downloaded so far.
        bytes_downloaded: u64,
        /// Total bytes to download.
        total_bytes: u64,
    },
    /// Verifying downloaded files.
    Verifying,
    /// Model ready for use.
    Ready,
    /// Model disabled by user or policy.
    Disabled { reason: String },
    /// Verification failed after download.
    VerificationFailed { reason: String },
    /// New model version available.
    UpdateAvailable {
        /// Currently installed revision.
        current_revision: String,
        /// Latest available revision.
        latest_revision: String,
    },
    /// Download was cancelled.
    Cancelled,
}

impl ModelState {
    /// Whether the model is ready for use.
    pub fn is_ready(&self) -> bool {
        matches!(self, ModelState::Ready)
    }

    /// Whether a download is in progress.
    pub fn is_downloading(&self) -> bool {
        matches!(self, ModelState::Downloading { .. })
    }

    /// Whether user consent is needed.
    pub fn needs_consent(&self) -> bool {
        matches!(self, ModelState::NeedsConsent)
    }

    /// Human-readable summary of the state.
    pub fn summary(&self) -> String {
        match self {
            ModelState::NotInstalled => "not installed".into(),
            ModelState::NeedsConsent => "needs consent".into(),
            ModelState::Downloading { progress_pct, .. } => {
                format!("downloading ({progress_pct}%)")
            }
            ModelState::Verifying => "verifying".into(),
            ModelState::Ready => "ready".into(),
            ModelState::Disabled { reason } => format!("disabled: {reason}"),
            ModelState::VerificationFailed { reason } => format!("verification failed: {reason}"),
            ModelState::UpdateAvailable {
                current_revision,
                latest_revision,
            } => {
                format!("update available: {current_revision} -> {latest_revision}")
            }
            ModelState::Cancelled => "cancelled".into(),
        }
    }
}

/// Policy inputs that constrain semantic model acquisition.
///
/// This is intentionally local and explicit: callers can construct it from the
/// resolved semantic policy, CLI flags, test fixtures, or future persisted
/// config without hiding why acquisition is blocked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAcquisitionPolicy {
    /// Whether semantic model acquisition is enabled at all.
    pub downloads_enabled: bool,
    /// Whether a missing model requires explicit user consent before fetching.
    pub requires_consent: bool,
    /// Whether network acquisition is unavailable because the host is offline.
    pub offline: bool,
    /// Whether the current network is metered.
    pub metered: bool,
    /// Whether acquisition may proceed on a metered network.
    pub allow_metered: bool,
    /// Maximum allowed size for this model download.
    pub max_model_bytes: Option<u64>,
    /// Optional mirror source selected for acquisition.
    pub mirror_base_url: Option<String>,
    /// Human-readable provenance for the active policy.
    pub config_source: String,
}

impl Default for ModelAcquisitionPolicy {
    fn default() -> Self {
        Self {
            downloads_enabled: true,
            requires_consent: true,
            offline: false,
            metered: false,
            allow_metered: false,
            max_model_bytes: None,
            mirror_base_url: None,
            config_source: "compiled_default".to_string(),
        }
    }
}

impl ModelAcquisitionPolicy {
    /// Build acquisition constraints from the resolved semantic policy.
    pub fn from_semantic_policy(policy: &SemanticPolicy) -> Self {
        const MIB: u64 = 1_048_576;

        Self {
            downloads_enabled: policy.mode.should_build_semantic(),
            requires_consent: matches!(policy.download_policy, ModelDownloadPolicy::OptIn),
            max_model_bytes: Some(policy.max_model_size_mb.saturating_mul(MIB)),
            config_source: "semantic_policy".to_string(),
            ..Self::default()
        }
    }
}

/// Precise on-disk semantic model cache state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ModelCacheState {
    /// Required files are absent or incomplete.
    NotAcquired {
        /// Missing manifest files, expressed as local filenames.
        missing_files: Vec<String>,
        /// Whether the next acquisition step is consent rather than download.
        needs_consent: bool,
    },
    /// A staged/resumable acquisition is present.
    Acquiring {
        staging_dir: PathBuf,
        bytes_present: u64,
        total_bytes: u64,
    },
    /// All files are installed, verified, and revision-compatible.
    Acquired { model_dir: PathBuf },
    /// At least one file exists but its checksum does not match the manifest.
    ChecksumMismatch {
        file: String,
        expected: String,
        actual: String,
    },
    /// Files are complete, but the installed revision does not match the manifest.
    IncompatibleVersion {
        current_revision: String,
        expected_revision: String,
    },
    /// Semantic model acquisition is disabled by user or policy.
    DisabledByPolicy { reason: String },
    /// The model exceeds the active byte budget.
    BudgetBlocked { required_bytes: u64, max_bytes: u64 },
    /// A previous corrupt cache has been quarantined.
    QuarantinedCorrupt {
        marker_path: PathBuf,
        reason: String,
    },
    /// Files were preseeded locally and verified without a cass marker.
    PreseededLocal { model_dir: PathBuf },
    /// Files were acquired from a configured mirror and verified.
    MirrorSourced {
        model_dir: PathBuf,
        mirror_base_url: String,
    },
    /// Acquisition is needed, but the host is offline.
    OfflineBlocked { missing_files: Vec<String> },
}

impl ModelCacheState {
    /// Stable machine-readable state code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAcquired { .. } => "not_acquired",
            Self::Acquiring { .. } => "acquiring",
            Self::Acquired { .. } => "acquired",
            Self::ChecksumMismatch { .. } => "checksum_mismatch",
            Self::IncompatibleVersion { .. } => "incompatible_version",
            Self::DisabledByPolicy { .. } => "disabled_by_policy",
            Self::BudgetBlocked { .. } => "budget_blocked",
            Self::QuarantinedCorrupt { .. } => "quarantined_corrupt",
            Self::PreseededLocal { .. } => "preseeded_local",
            Self::MirrorSourced { .. } => "mirror_sourced",
            Self::OfflineBlocked { .. } => "offline_blocked",
        }
    }

    /// Human-readable state detail for CLI and robot diagnostics.
    pub fn summary(&self) -> String {
        match self {
            Self::NotAcquired {
                missing_files,
                needs_consent,
            } => {
                let action = if *needs_consent {
                    "user consent required"
                } else {
                    "ready to acquire"
                };
                format!(
                    "model not acquired ({action}); missing {}",
                    missing_files.join(", ")
                )
            }
            Self::Acquiring {
                bytes_present,
                total_bytes,
                staging_dir,
            } => format!(
                "model acquisition in progress at {} ({bytes_present}/{total_bytes} bytes)",
                staging_dir.display()
            ),
            Self::Acquired { .. } => "model cache acquired and verified".to_string(),
            Self::ChecksumMismatch {
                file,
                expected,
                actual,
            } => format!("checksum mismatch for {file}: expected {expected}, got {actual}"),
            Self::IncompatibleVersion {
                current_revision,
                expected_revision,
            } => format!("model revision mismatch: {current_revision} != {expected_revision}"),
            Self::DisabledByPolicy { reason } => format!("model acquisition disabled: {reason}"),
            Self::BudgetBlocked {
                required_bytes,
                max_bytes,
            } => {
                format!("model requires {required_bytes} bytes but policy allows {max_bytes} bytes")
            }
            Self::QuarantinedCorrupt { reason, .. } => {
                format!("model cache quarantined: {reason}")
            }
            Self::PreseededLocal { .. } => "preseeded local model cache verified".to_string(),
            Self::MirrorSourced {
                mirror_base_url, ..
            } => {
                format!("model cache verified from mirror {mirror_base_url}")
            }
            Self::OfflineBlocked { missing_files } => {
                format!(
                    "offline and model is not acquired; missing {}",
                    missing_files.join(", ")
                )
            }
        }
    }

    /// Suggested next operator action.
    pub fn next_step(&self) -> Option<&'static str> {
        match self {
            Self::NotAcquired { .. } => {
                Some("Run `cass models install`, or keep using lexical search.")
            }
            Self::Acquiring { .. } => {
                Some("Wait for model acquisition to finish, or keep using lexical search.")
            }
            Self::Acquired { .. } | Self::PreseededLocal { .. } | Self::MirrorSourced { .. } => {
                None
            }
            Self::ChecksumMismatch { .. } | Self::QuarantinedCorrupt { .. } => Some(
                "Run `cass models verify --repair`, or reinstall with `cass models install -y`.",
            ),
            Self::IncompatibleVersion { .. } => {
                Some("Run `cass models install -y` to refresh the model cache.")
            }
            Self::DisabledByPolicy { .. } => {
                Some("Use lexical search or re-enable semantic model acquisition in policy.")
            }
            Self::BudgetBlocked { .. } => {
                Some("Increase the semantic model budget or keep using lexical search.")
            }
            Self::OfflineBlocked { .. } => Some(
                "Reconnect or install from local files with `cass models install --from-file`.",
            ),
        }
    }

    /// Whether the installed files can be used by the embedder immediately.
    pub fn is_usable(&self) -> bool {
        matches!(
            self,
            Self::Acquired { .. } | Self::PreseededLocal { .. } | Self::MirrorSourced { .. }
        )
    }
}

/// Machine-readable report for semantic model cache lifecycle decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCacheReport {
    pub model_id: String,
    pub model_dir: PathBuf,
    pub state: ModelCacheState,
    pub required_size_bytes: u64,
    pub installed_size_bytes: u64,
    pub policy_source: String,
}

impl ModelCacheReport {
    /// Stable machine-readable state code.
    pub fn state_code(&self) -> &'static str {
        self.state.code()
    }

    /// Whether the model cache can be used by semantic search.
    pub fn is_usable(&self) -> bool {
        self.state.is_usable()
    }
}

/// A file in the model manifest.
///
/// Structurally identical to `frankensearch_embed::ModelFile`.
#[derive(Debug, Clone)]
pub struct ModelFile {
    /// File path relative to repo root (e.g., "model.onnx" or "onnx/model.onnx").
    pub name: String,
    /// Expected SHA256 hash (hex string).
    pub sha256: String,
    /// Expected file size in bytes.
    pub size: u64,
}

impl ModelFile {
    /// Get the local filename (basename) for saving.
    ///
    /// For paths like "onnx/model.onnx", returns "model.onnx".
    /// This handles HuggingFace repos that restructure files into subdirectories.
    pub fn local_name(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or(&self.name)
    }
}

/// Model manifest describing a downloadable model.
///
/// Structurally compatible with `frankensearch_embed::ModelManifest`
/// (which has additional optional fields: version, display_name, description,
/// dimension, tier, download_size_bytes).
#[derive(Debug, Clone)]
pub struct ModelManifest {
    /// Model identifier (e.g., "all-minilm-l6-v2").
    pub id: String,
    /// HuggingFace repository.
    pub repo: String,
    /// Pinned revision (commit SHA).
    pub revision: String,
    /// Files to download.
    pub files: Vec<ModelFile>,
    /// License identifier.
    pub license: String,
}

/// Placeholder checksum value used for unverified manifests.
///
/// When a manifest file has this checksum, it means the model has not been
/// downloaded and verified yet. The download system will reject such files.
pub const PLACEHOLDER_CHECKSUM: &str = "PLACEHOLDER_VERIFY_AFTER_DOWNLOAD";

/// Validate and normalize a mirror base URL for model downloads.
///
/// The returned string never ends with a trailing slash and must be an
/// absolute HTTP(S) URL without query or fragment components.
pub fn normalize_mirror_base_url(base_url: &str) -> Result<String, DownloadError> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return Err(invalid_mirror_url(base_url, "mirror URL cannot be empty"));
    }

    let parsed = Url::parse(trimmed).map_err(|err| invalid_mirror_url(trimmed, err.to_string()))?;

    match parsed.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(invalid_mirror_url(
                trimmed,
                format!("unsupported URL scheme '{scheme}' (expected http or https)"),
            ));
        }
    }

    if parsed.host_str().is_none() {
        return Err(invalid_mirror_url(
            trimmed,
            "mirror URL must include a host",
        ));
    }

    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(invalid_mirror_url(
            trimmed,
            "mirror URL must not include query or fragment components",
        ));
    }

    Ok(parsed.to_string().trim_end_matches('/').to_string())
}

fn invalid_mirror_url(url: impl Into<String>, reason: impl Into<String>) -> DownloadError {
    DownloadError::InvalidMirrorUrl {
        url: url.into(),
        reason: reason.into(),
    }
}

impl ModelManifest {
    /// Check if this manifest has verified checksums for all files.
    ///
    /// Returns `false` if any file has the placeholder checksum, indicating
    /// the model has not been downloaded and verified yet.
    pub fn has_verified_checksums(&self) -> bool {
        self.files.iter().all(|f| f.sha256 != PLACEHOLDER_CHECKSUM)
    }

    /// Check if this manifest has a pinned revision (not "main").
    ///
    /// Unpinned revisions ("main") are not reproducible since the content
    /// can change at any time on HuggingFace.
    pub fn has_pinned_revision(&self) -> bool {
        self.revision != "main"
    }

    /// Check if this manifest is production-ready.
    ///
    /// A manifest is production-ready if it has:
    /// - All checksums verified (no placeholders)
    /// - A pinned revision (not "main")
    pub fn is_production_ready(&self) -> bool {
        self.has_verified_checksums() && self.has_pinned_revision()
    }

    /// Get the default MiniLM model manifest (baseline for bake-off).
    ///
    /// The revision and checksums are pinned for reproducibility.
    /// Updated 2026-01-13: HuggingFace restructured the repo - ONNX models moved to onnx/ subdir.
    pub fn minilm_v2() -> Self {
        Self {
            id: "all-minilm-l6-v2".into(),
            repo: "sentence-transformers/all-MiniLM-L6-v2".into(),
            // Pinned revision for reproducibility (updated 2026-01-13 for onnx/ restructuring)
            revision: "c9745ed1d9f207416be6d2e6f8de32d1f16199bf".into(),
            files: vec![
                ModelFile {
                    // Note: model moved from root to onnx/ subdirectory in repo restructuring
                    name: "onnx/model.onnx".into(),
                    sha256: "6fd5d72fe4589f189f8ebc006442dbb529bb7ce38f8082112682524616046452"
                        .into(),
                    size: 90405214,
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037"
                        .into(),
                    size: 466247,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41"
                        .into(),
                    size: 612,
                },
                // FastEmbed requires special_tokens_map.json and tokenizer_config.json
                // to construct a tokenizer with correct padding/truncation behavior.
                // We download and verify them alongside the core model files.
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "303df45a03609e4ead04bc3dc1536d0ab19b5358db685b6f3da123d05ec200e3"
                        .into(),
                    size: 112,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "acb92769e8195aabd29b7b2137a9e6d6e25c476a4f15aa4355c233426c61576b"
                        .into(),
                    size: 350,
                },
            ],
            license: "Apache-2.0".into(),
        }
    }

    // ==================== Bake-off Eligible Models ====================
    // These models were released after 2025-11-01 and are candidates for
    // the CPU-optimized embedding bake-off.
    //
    // Canonical definitions also available via `frankensearch_embed::ModelManifest`.

    /// Snowflake Arctic Embed S manifest.
    ///
    /// Released: 2025-11-10
    /// Dimension: 384
    /// Small, fast model with MiniLM-compatible dimension.
    ///
    /// Verified: 2026-02-02 - All checksums verified from HuggingFace.
    pub fn snowflake_arctic_s() -> Self {
        Self {
            id: "snowflake-arctic-embed-s".into(),
            repo: "Snowflake/snowflake-arctic-embed-s".into(),
            revision: "e596f507467533e48a2e17c007f0e1dacc837b33".into(),
            files: vec![
                ModelFile {
                    name: "onnx/model.onnx".into(),
                    sha256: "579c1f1778a0993eb0d2a1403340ffb491c769247fb46acc4f5cf8ac5b89c1e1"
                        .into(),
                    size: 133_093_492,
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "91f1def9b9391fdabe028cd3f3fcc4efd34e5d1f08c3bf2de513ebb5911a1854"
                        .into(),
                    size: 711_649,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "4e519aa92ec40943356032afe458c8829d70c5766b109e4a57490b82f72dcfb7"
                        .into(),
                    size: 703,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "5d5b662e421ea9fac075174bb0688ee0d9431699900b90662acd44b2a350503a"
                        .into(),
                    size: 695,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "9ca59277519f6e3692c8685e26b94d4afca2d5438deff66483db495e48735810"
                        .into(),
                    size: 1_433,
                },
            ],
            license: "Apache-2.0".into(),
        }
    }

    /// Nomic Embed Text v1.5 manifest.
    ///
    /// Released: 2025-11-05
    /// Dimension: 768
    /// Long context support with Matryoshka embedding capability.
    ///
    /// Verified: 2026-02-02 - All checksums verified from HuggingFace.
    pub fn nomic_embed() -> Self {
        Self {
            id: "nomic-embed-text-v1.5".into(),
            repo: "nomic-ai/nomic-embed-text-v1.5".into(),
            revision: "e5cf08aadaa33385f5990def41f7a23405aec398".into(),
            files: vec![
                ModelFile {
                    name: "onnx/model.onnx".into(),
                    sha256: "147d5aa88c2101237358e17796cf3a227cead1ec304ec34b465bb08e9d952965"
                        .into(),
                    size: 547_310_275,
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66"
                        .into(),
                    size: 711_396,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "0168e0883705b0bf8f2b381e10f45a9f3e1ef4b13869b43c160e4c8a70ddf442"
                        .into(),
                    size: 2_331,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "5d5b662e421ea9fac075174bb0688ee0d9431699900b90662acd44b2a350503a"
                        .into(),
                    size: 695,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "d7e0000bcc80134debd2222220427e6bf5fa20a669f40a0d0d1409cc18e0a9bc"
                        .into(),
                    size: 1_191,
                },
            ],
            license: "Apache-2.0".into(),
        }
    }

    // ==================== Reranker Models ====================

    /// MS MARCO MiniLM reranker manifest (baseline for bake-off).
    ///
    /// Verified: 2026-02-02 - All checksums verified from HuggingFace.
    /// Note: Repo is ms-marco-MiniLM-L6-v2 (no hyphen between L and 6).
    pub fn msmarco_reranker() -> Self {
        Self {
            id: "ms-marco-MiniLM-L6-v2".into(),
            repo: "cross-encoder/ms-marco-MiniLM-L6-v2".into(),
            revision: "c5ee24cb16019beea0893ab7796b1df96625c6b8".into(),
            files: vec![
                ModelFile {
                    name: "onnx/model.onnx".into(),
                    sha256: "5d3e70fd0c9ff14b9b5169a51e957b7a9c74897afd0a35ce4bd318150c1d4d4a"
                        .into(),
                    size: 91_011_230,
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66"
                        .into(),
                    size: 711_396,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "380e02c93f431831be65d99a4e7e5f67c133985bf2e77d9d4eba46847190bacc"
                        .into(),
                    size: 794,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "3c3507f36dff57bce437223db3b3081d1e2b52ec3e56ee55438193ecb2c94dd6"
                        .into(),
                    size: 132,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "a5c2e5a7b1a29a0702cd28c08a399b5ecc110c263009d17f7e3b415f25905fd8"
                        .into(),
                    size: 1_330,
                },
            ],
            license: "Apache-2.0".into(),
        }
    }

    /// Jina Reranker v1 Turbo EN manifest.
    ///
    /// Released: 2025-11-20
    /// Fast, optimized for English.
    ///
    /// Verified: 2026-02-02 - All checksums verified from HuggingFace.
    pub fn jina_reranker_turbo() -> Self {
        Self {
            id: "jina-reranker-v1-turbo-en".into(),
            repo: "jinaai/jina-reranker-v1-turbo-en".into(),
            revision: "b8c14f4e723d9e0aab4732a7b7b93741eeeb77c2".into(),
            files: vec![
                ModelFile {
                    name: "onnx/model.onnx".into(),
                    sha256: "c1296c66c119de645fa9cdee536d8637740efe85224cfa270281e50f213aa565"
                        .into(),
                    size: 151_296_975,
                },
                ModelFile {
                    name: "tokenizer.json".into(),
                    sha256: "0046da43cc8c424b317f56b092b0512aaaa65c4f925d2f16af9d9eeb4d0ef902"
                        .into(),
                    size: 2_030_772,
                },
                ModelFile {
                    name: "config.json".into(),
                    sha256: "e050ff6a15ae9295e84882fa0e98051bd8754856cd5201395ebf00ce9f2d609b"
                        .into(),
                    size: 1_206,
                },
                ModelFile {
                    name: "special_tokens_map.json".into(),
                    sha256: "06e405a36dfe4b9604f484f6a1e619af1a7f7d09e34a8555eb0b77b66318067f"
                        .into(),
                    size: 280,
                },
                ModelFile {
                    name: "tokenizer_config.json".into(),
                    sha256: "d291c6652d96d56ffdbcf1ea19d9bae5ed79003f7648c627e725a619227ce8fa"
                        .into(),
                    size: 1_215,
                },
            ],
            license: "Apache-2.0".into(),
        }
    }

    // ==================== Lookup Functions ====================

    /// Get manifest by embedder name.
    pub fn for_embedder(name: &str) -> Option<Self> {
        match name {
            "minilm" => Some(Self::minilm_v2()),
            "snowflake-arctic-s" => Some(Self::snowflake_arctic_s()),
            "nomic-embed" => Some(Self::nomic_embed()),
            _ => None,
        }
    }

    /// Get manifest by reranker name.
    pub fn for_reranker(name: &str) -> Option<Self> {
        match name {
            "ms-marco" => Some(Self::msmarco_reranker()),
            "jina-reranker-turbo" => Some(Self::jina_reranker_turbo()),
            _ => None,
        }
    }

    /// Get all bake-off eligible embedder manifests.
    ///
    /// All models are verified with pinned revisions and SHA256 checksums.
    pub fn bakeoff_embedder_candidates() -> Vec<Self> {
        vec![Self::snowflake_arctic_s(), Self::nomic_embed()]
    }

    /// Get all bake-off eligible reranker manifests.
    ///
    /// All models are verified with pinned revisions and SHA256 checksums.
    pub fn bakeoff_reranker_candidates() -> Vec<Self> {
        vec![Self::jina_reranker_turbo()]
    }

    /// Get all bake-off eligible model manifests (embedders + rerankers).
    ///
    /// All models are verified with pinned revisions and SHA256 checksums.
    pub fn bakeoff_candidates() -> Vec<Self> {
        let mut candidates = Self::bakeoff_embedder_candidates();
        candidates.extend(Self::bakeoff_reranker_candidates());
        candidates
    }

    /// Total size of all files in bytes.
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Download URL for a file, optionally via a validated mirror base URL.
    pub fn download_url_with_base(&self, file: &ModelFile, base_url: Option<&str>) -> String {
        let root = base_url.unwrap_or("https://huggingface.co");
        format!(
            "{}/{}/resolve/{}/{}",
            root.trim_end_matches('/'),
            self.repo.trim_start_matches('/'),
            self.revision,
            file.name.trim_start_matches('/')
        )
    }

    /// HuggingFace download URL for a file.
    pub fn download_url(&self, file: &ModelFile) -> String {
        self.download_url_with_base(file, None)
    }

    /// Generate a ready-to-paste bash script that downloads every file in the
    /// manifest via `curl` and then invokes `cass models install --from-file`.
    ///
    /// Use this when the in-process downloader fails (e.g. the known Windows
    /// rustls/TCP connect race — see GH#193 for context). The script uses the
    /// pinned repo revision so checksums match.
    pub fn air_gap_bash_script(&self, base_url: Option<&str>) -> String {
        // Single-quote URLs to avoid any shell interpretation. Model-download
        // URLs are HTTP(S) with an allow-listed base (`normalize_mirror_base_url`
        // rejects anything with query strings), and `ModelFile::name` is
        // repo-scoped and hash-verified post-download, so no caller-reachable
        // single quote can slip through. Even so, assert at debug-build time.
        fn quote_url(url: &str) -> String {
            debug_assert!(
                !url.contains('\''),
                "model download URL unexpectedly contains a single quote: {url}"
            );
            format!("'{url}'")
        }

        let mut out = String::new();
        out.push_str("# Air-gap model install (bash / Git Bash / MSYS2)\n");
        out.push_str(
            "# Run these commands, then re-run `cass models install --from-file \"$DIR\"`.\n",
        );
        out.push_str("set -euo pipefail\n");
        out.push_str(&format!("DIR=\"${{DIR:-./{}_files}}\"\n", self.id));
        out.push_str("mkdir -p \"$DIR\"\n");
        for file in &self.files {
            // Write with explicit `-o "$DIR/<local>"` rather than `-O` so the
            // output filename is decoupled from the last URL path component.
            // Manifest files can sit at any repo-internal path (`onnx/model.onnx`,
            // etc.) but `--from-file` resolves each file by `local_name()`.
            let url = self.download_url_with_base(file, base_url);
            out.push_str(&format!(
                "curl -fL --retry 3 {} -o \"$DIR/{}\"  # {} bytes\n",
                quote_url(&url),
                file.local_name(),
                file.size,
            ));
        }
        out.push_str(&format!(
            "cass models install {} --from-file \"$DIR\" -y\n",
            self.id
        ));
        out
    }

    /// Generate a ready-to-paste PowerShell script that downloads every file
    /// via `Invoke-WebRequest` and then invokes `cass models install --from-file`.
    pub fn air_gap_powershell_script(&self, base_url: Option<&str>) -> String {
        // Same single-quoting invariant as the bash path.
        fn quote_url_ps(url: &str) -> String {
            debug_assert!(
                !url.contains('\''),
                "model download URL unexpectedly contains a single quote: {url}"
            );
            format!("'{url}'")
        }

        let mut out = String::new();
        out.push_str("# Air-gap model install (PowerShell 5.1+ and 7+)\n");
        out.push_str("$ErrorActionPreference = 'Stop'\n");
        // Force TLS 1.2+ on Windows PowerShell 5.1 where default may be
        // TLS 1.0/1.1; HuggingFace requires 1.2+. No-op on PowerShell 7+.
        out.push_str(
            "[System.Net.ServicePointManager]::SecurityProtocol = \
             [System.Net.ServicePointManager]::SecurityProtocol -bor \
             [System.Net.SecurityProtocolType]::Tls12\n",
        );
        out.push_str(&format!("$dir = \"{}_files\"\n", self.id));
        out.push_str("New-Item -ItemType Directory -Force -Path $dir | Out-Null\n");
        for file in &self.files {
            let url = self.download_url_with_base(file, base_url);
            // `-UseBasicParsing` keeps this compatible with Windows PowerShell
            // 5.1 and avoids the IE engine dependency. Ignored on PS 7+.
            out.push_str(&format!(
                "Invoke-WebRequest -UseBasicParsing -Uri {} -OutFile (Join-Path $dir '{}')  # {} bytes\n",
                quote_url_ps(&url),
                file.local_name(),
                file.size,
            ));
        }
        out.push_str(&format!(
            "cass models install {} --from-file $dir -y\n",
            self.id
        ));
        out
    }
}

/// Progress callback for downloads.
pub type ProgressCallback = Arc<dyn Fn(DownloadProgress) + Send + Sync>;

/// Download progress information.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    /// Current file being downloaded.
    pub current_file: String,
    /// File index (1-based).
    pub file_index: usize,
    /// Total number of files.
    pub total_files: usize,
    /// Bytes downloaded for current file.
    pub file_bytes: u64,
    /// Total bytes for current file.
    pub file_total: u64,
    /// Total bytes downloaded across all files.
    pub total_bytes: u64,
    /// Total bytes to download across all files.
    pub grand_total: u64,
    /// Overall progress percentage (0-100).
    pub progress_pct: u8,
}

/// Download error types.
#[derive(Debug, Error)]
pub enum DownloadError {
    /// Network error during download.
    #[error("network error: {0}")]
    NetworkError(String),
    /// File I/O error.
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    /// SHA256 verification failed.
    #[error("verification failed for {file}: expected {expected}, got {actual}")]
    VerificationFailed {
        file: String,
        expected: String,
        actual: String,
    },
    /// Download was cancelled.
    #[error("download cancelled")]
    Cancelled,
    /// Timeout during download.
    #[error("download timed out")]
    Timeout,
    /// HTTP error response.
    #[error("HTTP error {status}: {message}")]
    HttpError { status: u16, message: String },
    /// Manifest has placeholder checksums and is not production-ready.
    ///
    /// This error is returned when attempting to download a bake-off candidate
    /// model that has not yet been verified. The model files need to be:
    /// 1. Downloaded manually to compute SHA256 checksums
    /// 2. Revision pinned to a specific commit (not "main")
    #[error(
        "model '{model_id}' is not production-ready: {} file(s) have placeholder checksums{}",
        unverified_files.len(),
        if *revision_unpinned {
            " and revision is not pinned"
        } else {
            ""
        }
    )]
    ManifestNotVerified {
        model_id: String,
        unverified_files: Vec<String>,
        revision_unpinned: bool,
    },
    /// Mirror URL failed validation.
    #[error("invalid mirror URL '{url}': {reason}")]
    InvalidMirrorUrl { url: String, reason: String },
}

impl DownloadError {
    fn is_retryable(&self) -> bool {
        match self {
            DownloadError::NetworkError(_) | DownloadError::IoError(_) | DownloadError::Timeout => {
                true
            }
            DownloadError::HttpError { status, .. } => {
                *status == 408 || *status == 429 || (500..=599).contains(status)
            }
            DownloadError::VerificationFailed { .. }
            | DownloadError::Cancelled
            | DownloadError::ManifestNotVerified { .. }
            | DownloadError::InvalidMirrorUrl { .. } => false,
        }
    }

    fn should_discard_temp(&self) -> bool {
        matches!(self, DownloadError::VerificationFailed { .. })
    }
}

fn run_download_with_cx<T, F, Fut>(f: F) -> Result<T, DownloadError>
where
    T: Send + 'static,
    F: FnOnce(asupersync::Cx) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T, DownloadError>> + Send + 'static,
{
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .map_err(|e| {
            DownloadError::NetworkError(format!("failed to build download runtime: {e}"))
        })?;

    runtime.block_on(async move {
        let handle = asupersync::runtime::Runtime::current_handle().ok_or_else(|| {
            DownloadError::NetworkError("download runtime handle unavailable".into())
        })?;
        let (tx, rx) = std::sync::mpsc::channel();
        handle
            .try_spawn_with_cx(move |cx| async move {
                let _ = tx.send(f(cx).await);
            })
            .map_err(|e| {
                DownloadError::NetworkError(format!("failed to spawn download task: {e}"))
            })?;

        loop {
            match rx.try_recv() {
                Ok(result) => return result,
                Err(TryRecvError::Empty) => asupersync::runtime::yield_now().await,
                Err(TryRecvError::Disconnected) => {
                    return Err(DownloadError::NetworkError(
                        "download task exited before returning a result".into(),
                    ));
                }
            }
        }
    })
}

/// Model downloader with resumption and verification.
pub struct ModelDownloader {
    /// Target directory for model files.
    target_dir: PathBuf,
    /// Temporary download directory.
    temp_dir: PathBuf,
    /// Cancellation flag.
    cancelled: Arc<AtomicBool>,
    /// Connection timeout.
    connect_timeout: Duration,
    /// Per-file timeout.
    file_timeout: Duration,
    /// Maximum retries per file.
    max_retries: u32,
}

impl ModelDownloader {
    /// Create a new model downloader.
    pub fn new(target_dir: PathBuf) -> Self {
        // Use parent + modified filename to avoid with_extension() replacing dots in dir names
        // e.g., "model.v2" should become "model.v2.downloading", not "model.downloading"
        let temp_dir = if let Some(parent) = target_dir.parent() {
            let dir_name = target_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("model");
            parent.join(format!("{}.downloading", dir_name))
        } else {
            // Fallback for root paths (unlikely)
            target_dir.with_extension("downloading")
        };
        Self {
            target_dir,
            temp_dir,
            cancelled: Arc::new(AtomicBool::new(false)),
            connect_timeout: Duration::from_secs(30),
            file_timeout: Duration::from_secs(300), // 5 minutes per file
            max_retries: 3,
        }
    }

    /// Get a cancellation handle.
    pub fn cancellation_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    /// Cancel the download.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Check if download was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Download and install a model.
    ///
    /// This function:
    /// 1. Creates a temporary download directory
    /// 2. Downloads each file with resume support
    /// 3. Verifies SHA256 checksums
    /// 4. Atomically moves to target directory
    ///
    /// # Arguments
    ///
    /// * `manifest` - Model manifest with file checksums
    /// * `on_progress` - Progress callback (called frequently)
    ///
    /// # Errors
    ///
    /// Returns `DownloadError` if download fails.
    pub fn download(
        &self,
        manifest: &ModelManifest,
        on_progress: Option<ProgressCallback>,
    ) -> Result<(), DownloadError> {
        self.download_with_mirror(manifest, None, on_progress)
    }

    /// Download and install a model, optionally via a validated mirror base URL.
    pub fn download_with_mirror(
        &self,
        manifest: &ModelManifest,
        mirror_base_url: Option<&str>,
        on_progress: Option<ProgressCallback>,
    ) -> Result<(), DownloadError> {
        // Validate manifest is production-ready before downloading
        // This prevents downloading models with placeholder checksums that can't be verified
        if !manifest.is_production_ready() {
            let unverified_files: Vec<String> = manifest
                .files
                .iter()
                .filter(|f| f.sha256 == PLACEHOLDER_CHECKSUM)
                .map(|f| f.name.clone())
                .collect();
            return Err(DownloadError::ManifestNotVerified {
                model_id: manifest.id.clone(),
                unverified_files,
                revision_unpinned: !manifest.has_pinned_revision(),
            });
        }

        // Reset cancellation flag
        self.cancelled.store(false, Ordering::SeqCst);

        // Prepare the temp directory for a safe resume. Keep partials for the
        // current manifest, but remove stale or unsafe entries from older runs.
        self.prepare_temp_dir(manifest)?;

        let grand_total = manifest.total_size();
        let total_files = manifest.files.len();
        let bytes_downloaded = Arc::new(AtomicU64::new(0));

        for (idx, file) in manifest.files.iter().enumerate() {
            self.fail_if_cancelled()?;

            // Use local_name() for local path (handles onnx/model.onnx -> model.onnx)
            let file_path = self.temp_dir.join(file.local_name());
            let url = manifest.download_url_with_base(file, mirror_base_url);

            // Track bytes_downloaded at start of this file to reset on retry
            let bytes_before_file = bytes_downloaded.load(Ordering::SeqCst);

            // Download with retries
            let mut last_error = None;
            for attempt in 0..self.max_retries {
                self.fail_if_cancelled()?;

                // Reset byte counter to before this file on retry (avoid double-counting)
                if attempt > 0 {
                    bytes_downloaded.store(bytes_before_file, Ordering::SeqCst);
                }

                // Exponential backoff delay (except first attempt)
                if attempt > 0 {
                    let delay = Duration::from_secs(5 * (1 << (attempt - 1)));
                    std::thread::sleep(delay);
                }

                match self.download_file(
                    &url,
                    &file_path,
                    file.size,
                    idx,
                    total_files,
                    &bytes_downloaded,
                    grand_total,
                    on_progress.as_ref(),
                ) {
                    Ok(()) => {
                        last_error = None;
                        break;
                    }
                    Err(DownloadError::Cancelled) => {
                        return Err(DownloadError::Cancelled);
                    }
                    Err(e) => {
                        if !e.is_retryable() {
                            self.cleanup_temp_for_error(&e);
                            return Err(e);
                        }
                        last_error = Some(e);
                    }
                }
            }

            if let Some(err) = last_error {
                self.cleanup_temp_for_error(&err);
                return Err(err);
            }

            // Verify SHA256
            self.fail_if_cancelled()?;

            let actual_hash = compute_sha256(&file_path)?;
            if actual_hash != file.sha256 {
                let err = DownloadError::VerificationFailed {
                    file: file.name.clone(),
                    expected: file.sha256.clone(),
                    actual: actual_hash,
                };
                self.cleanup_temp_for_error(&err);
                return Err(err);
            }
        }

        // Atomic install: rename temp -> target
        self.atomic_install()?;

        // Write verified marker
        self.write_verified_marker(manifest, mirror_base_url)?;

        Ok(())
    }

    fn prepare_temp_dir(&self, manifest: &ModelManifest) -> Result<(), DownloadError> {
        fs::create_dir_all(&self.temp_dir)?;

        let expected_files: HashSet<String> = manifest
            .files
            .iter()
            .map(|file| file.local_name().to_string())
            .collect();

        for entry in fs::read_dir(&self.temp_dir)? {
            let entry = entry?;
            let entry_type = entry.file_type()?;
            let entry_name = entry.file_name();
            let keep_entry = entry_type.is_file()
                && entry_name
                    .to_str()
                    .is_some_and(|name| expected_files.contains(name));

            if keep_entry {
                continue;
            }

            let entry_path = entry.path();
            if entry_type.is_dir() {
                fs::remove_dir_all(entry_path)?;
            } else {
                fs::remove_file(entry_path)?;
            }
        }

        Ok(())
    }

    /// Download a single file with resume support.
    #[allow(clippy::too_many_arguments)]
    fn download_file(
        &self,
        url: &str,
        path: &Path,
        expected_size: u64,
        file_idx: usize,
        total_files: usize,
        bytes_downloaded: &Arc<AtomicU64>,
        grand_total: u64,
        on_progress: Option<&ProgressCallback>,
    ) -> Result<(), DownloadError> {
        // Check for existing partial download
        let mut existing_size = if path.exists() {
            fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };

        // If the existing partial is larger than expected, discard it and start fresh.
        if existing_size > expected_size {
            let _ = fs::remove_file(path);
            existing_size = 0;
        }

        // If already complete, skip download
        if existing_size == expected_size {
            bytes_downloaded.fetch_add(expected_size, Ordering::SeqCst);
            return Ok(());
        }

        let url = url.to_string();
        let path = path.to_path_buf();
        let bytes_downloaded = Arc::clone(bytes_downloaded);
        let cancelled = Arc::clone(&self.cancelled);
        let progress_callback = on_progress.cloned();
        let connect_timeout = self.connect_timeout;
        let file_timeout = self.file_timeout;

        run_download_with_cx(move |cx| async move {
            // Allow up to 500 MB for model downloads. The default 16 MiB
            // limit in asupersync's HTTP client is too small for embedding
            // models (e.g., all-MiniLM-L6-v2 is ~86 MB).
            const MODEL_MAX_BODY_SIZE: usize = 500 * 1024 * 1024;

            let client = asupersync::http::h1::HttpClient::builder()
                .user_agent(concat!(
                    "cass/",
                    env!("CARGO_PKG_VERSION"),
                    " (model-download)"
                ))
                .max_body_size(MODEL_MAX_BODY_SIZE)
                .build();
            let mut headers = vec![("Accept".to_string(), "application/octet-stream".to_string())];

            if existing_size > 0 {
                headers.push(("Range".to_string(), format!("bytes={existing_size}-")));
                bytes_downloaded.fetch_add(existing_size, Ordering::SeqCst);
            }

            let mut response = asupersync::time::timeout(
                cx.now(),
                connect_timeout,
                client.request_streaming(
                    &cx,
                    asupersync::http::h1::Method::Get,
                    &url,
                    headers,
                    Vec::new(),
                ),
            )
            .await
            .map_err(|_| DownloadError::Timeout)?
            .map_err(|e| DownloadError::NetworkError(e.to_string()))?;

            let status = response.head.status;
            if status >= 400 {
                return Err(DownloadError::HttpError {
                    status,
                    message: if response.head.reason.is_empty() {
                        status.to_string()
                    } else {
                        format!("{} {}", status, response.head.reason)
                    },
                });
            }

            // 206 = Partial Content (resume works), 200 = Full file (server ignored Range)
            let actually_resuming = existing_size > 0 && status == 206;
            if existing_size > 0 && status == 200 {
                bytes_downloaded.fetch_sub(existing_size, Ordering::SeqCst);
                existing_size = 0;
            }

            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(actually_resuming)
                .write(true)
                .truncate(!actually_resuming)
                .open(&path)?;

            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            let start = Instant::now();
            let mut file_bytes = if actually_resuming { existing_size } else { 0 };

            loop {
                if cancelled.load(Ordering::SeqCst) {
                    return Err(DownloadError::Cancelled);
                }

                let remaining = file_timeout.saturating_sub(start.elapsed());
                if remaining.is_zero() {
                    return Err(DownloadError::Timeout);
                }

                let frame = asupersync::time::timeout(
                    cx.now(),
                    remaining,
                    poll_fn(|task_cx| Pin::new(&mut response.body).poll_frame(task_cx)),
                )
                .await
                .map_err(|_| DownloadError::Timeout)?;

                let Some(frame) = frame else {
                    break;
                };

                match frame.map_err(|e| DownloadError::NetworkError(e.to_string()))? {
                    asupersync::http::body::Frame::Data(mut buf) => {
                        while buf.has_remaining() {
                            let chunk = buf.chunk();
                            if chunk.is_empty() {
                                break;
                            }
                            file.write_all(chunk)?;
                            let chunk_len = chunk.len();
                            buf.advance(chunk_len);
                            file_bytes = file_bytes.saturating_add(chunk_len as u64);
                            bytes_downloaded.fetch_add(chunk_len as u64, Ordering::SeqCst);

                            if let Some(callback) = progress_callback.as_ref() {
                                let total_downloaded = bytes_downloaded.load(Ordering::SeqCst);
                                let progress_pct = if grand_total > 0 {
                                    ((total_downloaded as f64 / grand_total as f64) * 100.0)
                                        .min(100.0) as u8
                                } else {
                                    0
                                };

                                callback(DownloadProgress {
                                    current_file: file_name.clone(),
                                    file_index: file_idx + 1,
                                    total_files,
                                    file_bytes,
                                    file_total: expected_size,
                                    total_bytes: total_downloaded,
                                    grand_total,
                                    progress_pct,
                                });
                            }
                        }
                    }
                    asupersync::http::body::Frame::Trailers(_) => {}
                }
            }

            file.sync_all()?;
            Ok(())
        })
    }

    /// Atomically install downloaded files.
    ///
    /// Uses a backup-rename-cleanup pattern to minimize the window where no model exists:
    /// 1. Move existing target to backup (if present)
    /// 2. Rename temp to target
    /// 3. Remove backup on success, or restore on failure
    fn atomic_install(&self) -> Result<(), DownloadError> {
        let backup_dir = unique_model_backup_dir(&self.target_dir);
        sync_tree(&self.temp_dir)?;

        // Move existing target to backup (preserves it until new install succeeds)
        let had_existing = if self.target_dir.exists() {
            fs::rename(&self.target_dir, &backup_dir)?;
            true
        } else {
            false
        };

        // Rename temp to target
        match fs::rename(&self.temp_dir, &self.target_dir) {
            Ok(()) => {
                sync_parent_directory(&self.target_dir)?;
                // Success: remove backup
                if had_existing {
                    let _ = fs::remove_dir_all(&backup_dir);
                    sync_parent_directory(&self.target_dir)?;
                }
            }
            Err(e) => {
                // Failed: try to restore from backup
                if had_existing && backup_dir.exists() {
                    match fs::rename(&backup_dir, &self.target_dir) {
                        Ok(()) => {
                            sync_parent_directory(&self.target_dir)?;
                            return Err(std::io::Error::other(format!(
                                "failed installing {} from {}: {e}; restored original model",
                                self.target_dir.display(),
                                self.temp_dir.display()
                            ))
                            .into());
                        }
                        Err(restore_err) => {
                            return Err(std::io::Error::other(format!(
                                "failed installing {} from {}: {e}; restore error: {restore_err}; temp model retained at {}",
                                self.target_dir.display(),
                                self.temp_dir.display(),
                                self.temp_dir.display()
                            ))
                            .into());
                        }
                    }
                }
                return Err(e.into());
            }
        }

        Ok(())
    }

    /// Write .verified marker file.
    fn write_verified_marker(
        &self,
        manifest: &ModelManifest,
        mirror_base_url: Option<&str>,
    ) -> Result<(), DownloadError> {
        let marker_path = self.target_dir.join(".verified");
        let source = mirror_base_url
            .map(|url| format!("mirror:{url}"))
            .unwrap_or_else(|| "registry".to_string());
        let content = format!(
            "revision={}\nverified_at={}\nsource={}\n",
            manifest.revision,
            chrono::Utc::now().to_rfc3339(),
            source
        );
        let temp_path = unique_model_sidecar_path(&marker_path, "tmp", ".verified");
        let mut file = File::create(&temp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        replace_file_from_temp(&temp_path, &marker_path)?;
        sync_parent_directory(&marker_path)?;
        Ok(())
    }

    /// Clean up temporary download directory.
    fn cleanup_temp(&self) {
        let _ = fs::remove_dir_all(&self.temp_dir);
    }

    fn cleanup_temp_for_error(&self, err: &DownloadError) {
        if err.should_discard_temp() {
            self.cleanup_temp();
        }
    }

    fn fail_if_cancelled(&self) -> Result<(), DownloadError> {
        if self.is_cancelled() {
            Err(DownloadError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Compute SHA256 hash of a file.
pub fn compute_sha256(path: &Path) -> Result<String, DownloadError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();

    let mut buffer = [0u8; 8192];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

/// Classify the local semantic model cache without performing network I/O.
///
/// This is the central fail-open lifecycle gate for semantic quality assets:
/// it reports why quality semantic search is unavailable without changing the
/// lexical search path.
pub fn classify_model_cache(
    model_dir: &Path,
    manifest: &ModelManifest,
    policy: &ModelAcquisitionPolicy,
) -> ModelCacheReport {
    let required_size_bytes = manifest.total_size();
    let installed_size_bytes = installed_manifest_size(model_dir, manifest);
    let missing_files = missing_manifest_files(model_dir, manifest);
    let state = classify_model_cache_state(model_dir, manifest, policy, &missing_files);

    ModelCacheReport {
        model_id: manifest.id.clone(),
        model_dir: model_dir.to_path_buf(),
        state,
        required_size_bytes,
        installed_size_bytes,
        policy_source: policy.config_source.clone(),
    }
}

fn classify_model_cache_state(
    model_dir: &Path,
    manifest: &ModelManifest,
    policy: &ModelAcquisitionPolicy,
    missing_files: &[String],
) -> ModelCacheState {
    if !policy.downloads_enabled {
        return ModelCacheState::DisabledByPolicy {
            reason: "semantic model downloads disabled by policy".to_string(),
        };
    }

    let quarantine_marker = model_dir.join(".quarantined");
    if quarantine_marker.is_file() {
        let reason = fs::read_to_string(&quarantine_marker)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "model cache quarantined after integrity failure".to_string());
        return ModelCacheState::QuarantinedCorrupt {
            marker_path: quarantine_marker,
            reason,
        };
    }

    let staging_dir = model_download_temp_dir(model_dir);
    if staging_dir.is_dir() {
        return ModelCacheState::Acquiring {
            bytes_present: directory_size_bytes(&staging_dir),
            staging_dir,
            total_bytes: manifest.total_size(),
        };
    }

    if !missing_files.is_empty() {
        if policy.offline {
            return ModelCacheState::OfflineBlocked {
                missing_files: missing_files.to_vec(),
            };
        }

        if policy.metered && !policy.allow_metered {
            return ModelCacheState::DisabledByPolicy {
                reason: "metered network disallows model acquisition".to_string(),
            };
        }

        if let Some(max_bytes) = policy.max_model_bytes
            && manifest.total_size() > max_bytes
        {
            return ModelCacheState::BudgetBlocked {
                required_bytes: manifest.total_size(),
                max_bytes,
            };
        }

        return ModelCacheState::NotAcquired {
            missing_files: missing_files.to_vec(),
            needs_consent: policy.requires_consent,
        };
    }

    for file in &manifest.files {
        let Some(path) = model_file_path(model_dir, file) else {
            continue;
        };
        match compute_sha256(&path) {
            Ok(actual) if actual == file.sha256 => {}
            Ok(actual) => {
                return ModelCacheState::ChecksumMismatch {
                    file: file.local_name().to_string(),
                    expected: file.sha256.clone(),
                    actual,
                };
            }
            Err(err) => {
                return ModelCacheState::QuarantinedCorrupt {
                    marker_path: path,
                    reason: format!("unable to hash model file {}: {err}", file.local_name()),
                };
            }
        }
    }

    let verified_marker = model_dir.join(".verified");
    if !verified_marker.is_file() {
        return ModelCacheState::PreseededLocal {
            model_dir: model_dir.to_path_buf(),
        };
    }

    let marker = match fs::read_to_string(&verified_marker) {
        Ok(marker) => marker,
        Err(err) => {
            return ModelCacheState::QuarantinedCorrupt {
                marker_path: verified_marker,
                reason: format!("unable to read verified marker: {err}"),
            };
        }
    };

    let current_revision =
        marker_field(&marker, "revision").unwrap_or_else(|| "<unknown>".to_string());
    if current_revision != manifest.revision {
        return ModelCacheState::IncompatibleVersion {
            current_revision,
            expected_revision: manifest.revision.clone(),
        };
    }

    match marker_field(&marker, "source") {
        Some(source) if source == "preseeded_local" => ModelCacheState::PreseededLocal {
            model_dir: model_dir.to_path_buf(),
        },
        Some(source) if source.starts_with("mirror:") => ModelCacheState::MirrorSourced {
            model_dir: model_dir.to_path_buf(),
            mirror_base_url: source.trim_start_matches("mirror:").to_string(),
        },
        _ => ModelCacheState::Acquired {
            model_dir: model_dir.to_path_buf(),
        },
    }
}

/// Check if a model is installed and verified against the given manifest.
///
/// `coding_agent_session_search-odbnh`: pre-fix this function hardcoded
/// `ModelManifest::minilm_v2()` to enumerate required files, so on a
/// machine with a complete snowflake-arctic-s or nomic-embed install
/// it always returned `NotInstalled` (minilm's filenames aren't a
/// subset of those models' filenames). The caller passes the manifest
/// they already resolved via `ModelManifest::for_embedder(name)` so
/// the file-presence check aligns with the model that was installed.
pub fn check_model_installed(model_dir: &Path, manifest: &ModelManifest) -> ModelState {
    if !model_dir.is_dir() {
        return ModelState::NotInstalled;
    }

    let verified_marker = model_dir.join(".verified");
    if !verified_marker.is_file() {
        return ModelState::NotInstalled;
    }

    // Check if all required files exist. Accept either the canonical repo path
    // (for preseeded HuggingFace layouts) or the flat local name used by the
    // downloader and air-gap installer.
    for file in &manifest.files {
        if model_file_path(model_dir, file).is_none() {
            return ModelState::NotInstalled;
        }
    }

    ModelState::Ready
}

/// Check for model version mismatch.
pub fn check_version_mismatch(model_dir: &Path, manifest: &ModelManifest) -> Option<ModelState> {
    let verified_marker = model_dir.join(".verified");
    if !verified_marker.is_file() {
        return None;
    }

    // Read installed revision
    let content = fs::read_to_string(&verified_marker).ok()?;
    let installed_revision = content
        .lines()
        .find(|l| l.starts_with("revision="))
        .map(|l| l.trim_start_matches("revision=").to_string())?;

    if installed_revision != manifest.revision {
        Some(ModelState::UpdateAvailable {
            current_revision: installed_revision,
            latest_revision: manifest.revision.clone(),
        })
    } else {
        None
    }
}

fn model_download_temp_dir(target_dir: &Path) -> PathBuf {
    if let Some(parent) = target_dir.parent() {
        let dir_name = target_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("model");
        parent.join(format!("{dir_name}.downloading"))
    } else {
        target_dir.with_extension("downloading")
    }
}

/// Resolve a manifest file on disk.
///
/// The downloader stores HuggingFace paths by local basename, while preseeded
/// directories can preserve the canonical repo layout.
pub fn model_file_path(model_dir: &Path, file: &ModelFile) -> Option<PathBuf> {
    let canonical = model_dir.join(&file.name);
    if canonical.is_file() {
        return Some(canonical);
    }

    let local = model_dir.join(file.local_name());
    if local.is_file() {
        return Some(local);
    }

    None
}

fn missing_manifest_files(model_dir: &Path, manifest: &ModelManifest) -> Vec<String> {
    manifest
        .files
        .iter()
        .filter(|file| model_file_path(model_dir, file).is_none())
        .map(|file| file.local_name().to_string())
        .collect()
}

fn installed_manifest_size(model_dir: &Path, manifest: &ModelManifest) -> u64 {
    manifest
        .files
        .iter()
        .filter_map(|file| model_file_path(model_dir, file))
        .filter_map(|path| path.metadata().ok())
        .map(|metadata| metadata.len())
        .sum()
}

fn directory_size_bytes(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            match entry.file_type() {
                Ok(file_type) if file_type.is_file() => {
                    entry.metadata().map(|metadata| metadata.len()).unwrap_or(0)
                }
                Ok(file_type) if file_type.is_dir() => directory_size_bytes(&path),
                _ => 0,
            }
        })
        .sum()
}

fn marker_field(content: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}=");
    content
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn unique_model_backup_dir(path: &Path) -> PathBuf {
    unique_model_sidecar_path(path, "bak", "model")
}

fn unique_model_sidecar_path(path: &Path, suffix: &str, fallback_name: &str) -> PathBuf {
    static NEXT_NONCE: AtomicU64 = AtomicU64::new(0);

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = NEXT_NONCE.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback_name);

    path.with_file_name(format!(
        ".{file_name}.{suffix}.{}.{}.{}",
        std::process::id(),
        timestamp,
        nonce
    ))
}

fn replace_file_from_temp(temp_path: &Path, final_path: &Path) -> Result<(), DownloadError> {
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
                let backup_path = unique_model_backup_dir(final_path);
                fs::rename(final_path, &backup_path).map_err(|backup_err| {
                    let _ = fs::remove_file(temp_path);
                    DownloadError::IoError(std::io::Error::other(format!(
                        "failed preparing backup {} before replacing {}: first error: {first_err}; backup error: {backup_err}",
                        backup_path.display(),
                        final_path.display()
                    )))
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
                                "failed replacing {} with {}: first error: {first_err}; second error: {second_err}; restored original file",
                                final_path.display(),
                                temp_path.display()
                            ))
                            .into())
                        }
                        Err(restore_err) => Err(std::io::Error::other(format!(
                            "failed replacing {} with {}: first error: {first_err}; second error: {second_err}; restore error: {restore_err}; temp file retained at {}",
                            final_path.display(),
                            temp_path.display(),
                            temp_path.display()
                        ))
                        .into()),
                    },
                }
            }
            Err(rename_err) => Err(rename_err.into()),
        }
    }

    #[cfg(not(windows))]
    {
        fs::rename(temp_path, final_path)?;
        sync_parent_directory(final_path)
    }
}

#[cfg(not(windows))]
fn sync_tree(path: &Path) -> Result<(), DownloadError> {
    sync_tree_inner(path)?;
    sync_parent_directory(path)
}

#[cfg(not(windows))]
fn sync_tree_inner(path: &Path) -> Result<(), DownloadError> {
    let metadata = fs::metadata(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            sync_tree_inner(&entry.path())?;
        }
        File::open(path)?.sync_all()?;
    } else if metadata.is_file() {
        File::open(path)?.sync_all()?;
    }
    Ok(())
}

#[cfg(windows)]
fn sync_tree(_path: &Path) -> Result<(), DownloadError> {
    Ok(())
}

#[cfg(not(windows))]
fn sync_parent_directory(path: &Path) -> Result<(), DownloadError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> Result<(), DownloadError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::error::Error as _;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    /// Copy model fixtures from tests/fixtures/models/ to the target directory.
    /// Copies model.onnx plus config files.
    fn copy_model_fixtures(target_dir: &Path) -> std::io::Result<()> {
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models");
        fs::create_dir_all(target_dir)?;

        // Copy model.onnx fixture
        fs::copy(
            fixture_dir.join("model.onnx"),
            target_dir.join("model.onnx"),
        )?;

        // Copy config files
        for file in &[
            "tokenizer.json",
            "config.json",
            "special_tokens_map.json",
            "tokenizer_config.json",
        ] {
            fs::copy(fixture_dir.join(file), target_dir.join(file))?;
        }

        Ok(())
    }

    #[derive(Clone, Debug)]
    struct MirrorRequest {
        path: String,
        range_start: Option<u64>,
    }

    #[derive(Clone)]
    struct MirrorRoute {
        body: Vec<u8>,
        content_type: &'static str,
        chunk_size: usize,
        chunk_delay: Duration,
    }

    struct MirrorFixtureServer {
        base_url: String,
        stop: Arc<AtomicBool>,
        wake_addr: String,
        requests: Arc<Mutex<Vec<MirrorRequest>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl MirrorFixtureServer {
        fn requests(&self) -> Vec<MirrorRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for MirrorFixtureServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Ok(stream) = TcpStream::connect(&self.wake_addr) {
                let _ = stream.shutdown(Shutdown::Both);
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn start_mirror_fixture_server(routes: Vec<(String, MirrorRoute)>) -> MirrorFixtureServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test mirror server");
        listener
            .set_nonblocking(true)
            .expect("set test mirror server nonblocking");
        let addr = listener.local_addr().expect("read server address");
        let wake_addr = addr.to_string();
        let base_url = format!("http://{wake_addr}");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_log = Arc::clone(&requests);
        let route_map: BTreeMap<String, MirrorRoute> = routes.into_iter().collect();
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_mirror_request(stream, &route_map, &request_log);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        MirrorFixtureServer {
            base_url,
            stop,
            wake_addr,
            requests,
            handle: Some(handle),
        }
    }

    fn handle_mirror_request(
        mut stream: TcpStream,
        routes: &BTreeMap<String, MirrorRoute>,
        request_log: &Arc<Mutex<Vec<MirrorRequest>>>,
    ) {
        let mut buffer = [0_u8; 8192];
        let read = match stream.read(&mut buffer) {
            Ok(read) => read,
            Err(_) => return,
        };
        let request = String::from_utf8_lossy(&buffer[..read]);
        let mut lines = request.lines();
        let target = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        let path = target
            .split_once('?')
            .map(|(path, _)| path)
            .unwrap_or(target)
            .split_once('#')
            .map(|(path, _)| path)
            .unwrap_or(target)
            .to_string();
        let range_start = lines.find_map(parse_range_start_header);
        request_log.lock().unwrap().push(MirrorRequest {
            path: path.clone(),
            range_start,
        });

        let Some(route) = routes.get(&path) else {
            let response = concat!(
                "HTTP/1.1 404 Not Found\r\n",
                "Content-Length: 9\r\n",
                "Content-Type: text/plain\r\n",
                "Connection: close\r\n\r\n",
                "not found"
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            return;
        };

        let start = range_start.unwrap_or(0) as usize;
        let mut status = "200 OK";
        let mut content_range = None;
        let body = if start >= route.body.len() {
            status = "416 Range Not Satisfiable";
            &[][..]
        } else if start > 0 {
            status = "206 Partial Content";
            content_range = Some(format!(
                "bytes {start}-{}/{}",
                route.body.len().saturating_sub(1),
                route.body.len()
            ));
            &route.body[start..]
        } else {
            route.body.as_slice()
        };

        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n",
            body.len(),
            route.content_type
        );
        if let Some(content_range) = content_range {
            response.push_str(&format!("Content-Range: {content_range}\r\n"));
        }
        response.push_str("\r\n");
        let _ = stream.write_all(response.as_bytes());
        for chunk in body.chunks(route.chunk_size.max(1)) {
            if stream.write_all(chunk).is_err() {
                return;
            }
            let _ = stream.flush();
            if !route.chunk_delay.is_zero() {
                thread::sleep(route.chunk_delay);
            }
        }
    }

    fn parse_range_start_header(line: &str) -> Option<u64> {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("range") {
            return None;
        }
        let value = value.trim();
        let value = value.strip_prefix("bytes=")?;
        let (start, _) = value.split_once('-')?;
        start.parse().ok()
    }

    fn build_test_manifest(repo: &str, revision: &str, files: &[(&str, &[u8])]) -> ModelManifest {
        ModelManifest {
            id: "mirror-test-model".into(),
            repo: repo.into(),
            revision: revision.into(),
            files: files
                .iter()
                .map(|(name, body)| ModelFile {
                    name: (*name).into(),
                    sha256: hex::encode(Sha256::digest(body)),
                    size: body.len() as u64,
                })
                .collect(),
            license: "Apache-2.0".into(),
        }
    }

    fn mirror_route_path(prefix: &str, manifest: &ModelManifest, file: &ModelFile) -> String {
        format!(
            "{}/{}/resolve/{}/{}",
            prefix.trim_end_matches('/'),
            manifest.repo.trim_start_matches('/'),
            manifest.revision,
            file.name.trim_start_matches('/')
        )
    }

    #[test]
    fn test_model_state_summary() {
        assert_eq!(ModelState::NotInstalled.summary(), "not installed");
        assert_eq!(ModelState::NeedsConsent.summary(), "needs consent");
        assert_eq!(ModelState::Ready.summary(), "ready");
        assert_eq!(
            ModelState::Downloading {
                progress_pct: 50,
                bytes_downloaded: 1000,
                total_bytes: 2000
            }
            .summary(),
            "downloading (50%)"
        );
    }

    #[test]
    fn test_model_state_is_ready() {
        assert!(ModelState::Ready.is_ready());
        assert!(!ModelState::NotInstalled.is_ready());
        assert!(!ModelState::NeedsConsent.is_ready());
        assert!(
            !ModelState::Downloading {
                progress_pct: 0,
                bytes_downloaded: 0,
                total_bytes: 0
            }
            .is_ready()
        );
    }

    #[test]
    fn test_model_manifest_total_size() {
        let manifest = ModelManifest::minilm_v2();
        assert!(manifest.total_size() > 20_000_000); // > 20MB
    }

    #[test]
    fn test_model_manifest_download_url() {
        let manifest = ModelManifest::minilm_v2();
        let url = manifest.download_url(&manifest.files[0]);
        assert!(url.contains("huggingface.co"));
        assert!(url.contains("sentence-transformers/all-MiniLM-L6-v2"));
        assert!(url.contains("model.onnx"));
    }

    #[test]
    fn test_model_manifest_download_url_with_mirror_base() {
        let manifest = ModelManifest::minilm_v2();
        let url = manifest
            .download_url_with_base(&manifest.files[0], Some("https://mirror.example/cache/"));
        assert_eq!(
            url,
            format!(
                "https://mirror.example/cache/{}/resolve/{}/{}",
                manifest.repo, manifest.revision, manifest.files[0].name
            )
        );
    }

    #[test]
    fn air_gap_bash_script_uses_explicit_output_filenames() {
        // Regression for a subtle bug in the initial #193 fix: using `curl -O`
        // derives the output filename from the URL's last path component, which
        // happens to match `local_name()` for this manifest but fails for
        // files whose repo path has extra segments. `-o "$DIR/<local>"`
        // makes the mapping explicit and matches what --from-file resolves.
        let manifest = ModelManifest::minilm_v2();
        let script = manifest.air_gap_bash_script(None);
        assert!(script.contains("set -euo pipefail"));
        assert!(script.contains("DIR=\"${DIR:-./all-minilm-l6-v2_files}\""));
        for file in &manifest.files {
            let local = file.local_name();
            assert!(
                script.contains(&format!("-o \"$DIR/{local}\"")),
                "bash script must write {local} via explicit -o, got:\n{script}"
            );
        }
        assert!(
            script.contains("cass models install all-minilm-l6-v2 --from-file \"$DIR\" -y"),
            "bash script must invoke install with --from-file"
        );
    }

    #[test]
    fn air_gap_bash_script_quotes_urls_with_single_quotes() {
        // URLs must be single-quoted so the shell performs no interpolation.
        let manifest = ModelManifest::minilm_v2();
        let script = manifest.air_gap_bash_script(None);
        let sample_url = manifest.download_url(&manifest.files[0]);
        assert!(script.contains(&format!("'{sample_url}'")));
    }

    #[test]
    fn air_gap_powershell_script_forces_tls12_and_basic_parsing() {
        let manifest = ModelManifest::minilm_v2();
        let script = manifest.air_gap_powershell_script(None);
        assert!(
            script.contains("SecurityProtocolType]::Tls12"),
            "PowerShell script must opt into TLS 1.2 for Windows PowerShell 5.1 compat"
        );
        assert!(
            script.contains("Invoke-WebRequest -UseBasicParsing"),
            "PowerShell script must use -UseBasicParsing for PS 5.1 compat"
        );
        for file in &manifest.files {
            let local = file.local_name();
            assert!(
                script.contains(&format!("(Join-Path $dir '{local}')")),
                "PowerShell script must materialize output path for {local}, got:\n{script}"
            );
        }
        assert!(
            script.contains("cass models install all-minilm-l6-v2 --from-file $dir -y"),
            "PowerShell script must invoke install with --from-file"
        );
    }

    #[test]
    fn air_gap_scripts_honor_mirror_base_url() {
        let manifest = ModelManifest::minilm_v2();
        let mirror = Some("https://mirror.example/cache");
        let bash = manifest.air_gap_bash_script(mirror);
        let ps = manifest.air_gap_powershell_script(mirror);
        assert!(bash.contains("https://mirror.example/cache"));
        assert!(!bash.contains("huggingface.co"));
        assert!(ps.contains("https://mirror.example/cache"));
        assert!(!ps.contains("huggingface.co"));
    }

    #[test]
    fn test_normalize_mirror_base_url_trims_trailing_slash() {
        let normalized = normalize_mirror_base_url("https://mirror.example/cache/").unwrap();
        assert_eq!(normalized, "https://mirror.example/cache");
    }

    #[test]
    fn test_normalize_mirror_base_url_rejects_invalid_values() {
        let cases = [
            ("mirror.example", "invalid mirror URL"),
            ("file:///tmp/mirror", "unsupported URL scheme"),
            (
                "https://mirror.example/cache?trace=abc",
                "must not include query or fragment",
            ),
        ];

        for (input, expected_fragment) in cases {
            let err = normalize_mirror_base_url(input).unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains(expected_fragment),
                "expected error for {input:?} to contain {expected_fragment:?}, got {message:?}"
            );
        }
    }

    #[test]
    fn test_invalid_mirror_url_helper_shape() {
        let err = invalid_mirror_url("ftp://mirror.example/model.onnx", "unsupported scheme");

        assert!(matches!(
            &err,
            DownloadError::InvalidMirrorUrl { url, reason }
                if url == "ftp://mirror.example/model.onnx" && reason == "unsupported scheme"
        ));
        assert_eq!(
            err.to_string(),
            "invalid mirror URL 'ftp://mirror.example/model.onnx': unsupported scheme"
        );
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_check_model_installed_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("nonexistent");
        assert_eq!(
            check_model_installed(&model_dir, &ModelManifest::minilm_v2()),
            ModelState::NotInstalled
        );
    }

    #[test]
    fn test_check_model_installed_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        // Use fixture files instead of fake content - only copy model.onnx
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models");
        fs::create_dir_all(&model_dir).unwrap();
        fs::copy(fixture_dir.join("model.onnx"), model_dir.join("model.onnx")).unwrap();
        assert_eq!(
            check_model_installed(&model_dir, &ModelManifest::minilm_v2()),
            ModelState::NotInstalled
        );
    }

    #[test]
    fn test_check_model_installed_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        // Use fixture files instead of fake content
        copy_model_fixtures(&model_dir).unwrap();
        fs::write(model_dir.join(".verified"), "revision=test\n").unwrap();
        assert_eq!(
            check_model_installed(&model_dir, &ModelManifest::minilm_v2()),
            ModelState::Ready
        );
    }

    #[test]
    fn classify_cache_policy_disabled_takes_precedence_over_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = build_test_manifest("repo/model", "rev1", &[("model.onnx", b"model")]);
        let policy = ModelAcquisitionPolicy {
            downloads_enabled: false,
            offline: true,
            max_model_bytes: Some(1),
            ..ModelAcquisitionPolicy::default()
        };

        let report = classify_model_cache(tmp.path(), &manifest, &policy);
        assert_eq!(report.state_code(), "disabled_by_policy");
        assert!(matches!(
            report.state,
            ModelCacheState::DisabledByPolicy { .. }
        ));
    }

    #[test]
    fn classify_cache_detects_resume_stage_before_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        let staging_dir = tmp.path().join("model.downloading");
        fs::create_dir_all(&staging_dir).unwrap();
        fs::write(staging_dir.join("model.onnx"), b"partial").unwrap();
        let manifest = build_test_manifest("repo/model", "rev1", &[("model.onnx", b"model")]);

        let report =
            classify_model_cache(&model_dir, &manifest, &ModelAcquisitionPolicy::default());
        assert_eq!(report.state_code(), "acquiring");
        assert!(matches!(
            report.state,
            ModelCacheState::Acquiring {
                bytes_present: 7,
                total_bytes: 5,
                ..
            }
        ));
    }

    #[test]
    fn classify_cache_distinguishes_offline_and_budget_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = build_test_manifest("repo/model", "rev1", &[("model.onnx", b"model")]);

        let offline = ModelAcquisitionPolicy {
            offline: true,
            ..ModelAcquisitionPolicy::default()
        };
        let report = classify_model_cache(tmp.path(), &manifest, &offline);
        assert_eq!(report.state_code(), "offline_blocked");

        let budget = ModelAcquisitionPolicy {
            max_model_bytes: Some(1),
            ..ModelAcquisitionPolicy::default()
        };
        let report = classify_model_cache(tmp.path(), &manifest, &budget);
        assert_eq!(report.state_code(), "budget_blocked");
    }

    #[test]
    fn classify_cache_accepts_preseeded_local_manifest_files() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        fs::create_dir_all(model_dir.join("onnx")).unwrap();
        fs::write(model_dir.join("onnx/model.onnx"), b"model").unwrap();
        fs::write(model_dir.join("tokenizer.json"), b"tok").unwrap();
        let manifest = build_test_manifest(
            "repo/model",
            "rev1",
            &[("onnx/model.onnx", b"model"), ("tokenizer.json", b"tok")],
        );

        let report =
            classify_model_cache(&model_dir, &manifest, &ModelAcquisitionPolicy::default());
        assert_eq!(report.state_code(), "preseeded_local");
        assert!(report.is_usable());
    }

    #[test]
    fn classify_cache_detects_checksum_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join("model.onnx"), b"wrong").unwrap();
        let manifest = build_test_manifest("repo/model", "rev1", &[("model.onnx", b"model")]);

        let report =
            classify_model_cache(&model_dir, &manifest, &ModelAcquisitionPolicy::default());
        assert_eq!(report.state_code(), "checksum_mismatch");
        assert!(matches!(
            report.state,
            ModelCacheState::ChecksumMismatch { .. }
        ));
    }

    #[test]
    fn classify_cache_detects_incompatible_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join("model.onnx"), b"model").unwrap();
        fs::write(model_dir.join(".verified"), "revision=old\n").unwrap();
        let manifest = build_test_manifest("repo/model", "rev1", &[("model.onnx", b"model")]);

        let report =
            classify_model_cache(&model_dir, &manifest, &ModelAcquisitionPolicy::default());
        assert_eq!(report.state_code(), "incompatible_version");
        assert!(matches!(
            report.state,
            ModelCacheState::IncompatibleVersion {
                current_revision,
                expected_revision
            } if current_revision == "old" && expected_revision == "rev1"
        ));
    }

    #[test]
    fn classify_cache_reports_mirror_sourced_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join("model.onnx"), b"model").unwrap();
        fs::write(
            model_dir.join(".verified"),
            "revision=rev1\nsource=mirror:https://mirror.example/cache\n",
        )
        .unwrap();
        let manifest = build_test_manifest("repo/model", "rev1", &[("model.onnx", b"model")]);

        let report =
            classify_model_cache(&model_dir, &manifest, &ModelAcquisitionPolicy::default());
        assert_eq!(report.state_code(), "mirror_sourced");
        assert!(matches!(
            report.state,
            ModelCacheState::MirrorSourced {
                mirror_base_url,
                ..
            } if mirror_base_url == "https://mirror.example/cache"
        ));
    }

    #[test]
    fn classify_cache_reports_quarantine_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join(".quarantined"), "bad checksum\n").unwrap();
        let manifest = build_test_manifest("repo/model", "rev1", &[("model.onnx", b"model")]);

        let report =
            classify_model_cache(&model_dir, &manifest, &ModelAcquisitionPolicy::default());
        assert_eq!(report.state_code(), "quarantined_corrupt");
        assert!(matches!(
            report.state,
            ModelCacheState::QuarantinedCorrupt { reason, .. } if reason == "bad checksum"
        ));
    }

    #[test]
    fn test_compute_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("test.txt");
        fs::write(&file_path, b"hello world").unwrap();
        let hash = compute_sha256(&file_path).unwrap();
        // SHA256 of "hello world"
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_check_version_mismatch_none() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        fs::create_dir_all(&model_dir).unwrap();
        // Use the current pinned revision from the manifest
        let manifest = ModelManifest::minilm_v2();
        fs::write(
            model_dir.join(".verified"),
            format!("revision={}\n", manifest.revision),
        )
        .unwrap();

        let result = check_version_mismatch(&model_dir, &manifest);
        assert!(result.is_none());
    }

    #[test]
    fn test_model_file_local_name() {
        // Test that local_name() extracts basename from path with subdirectories
        let file = ModelFile {
            name: "onnx/model.onnx".into(),
            sha256: "abc123".into(),
            size: 1000,
        };
        assert_eq!(file.local_name(), "model.onnx");

        // Test that local_name() works for files without subdirectory
        let file2 = ModelFile {
            name: "tokenizer.json".into(),
            sha256: "def456".into(),
            size: 500,
        };
        assert_eq!(file2.local_name(), "tokenizer.json");

        // Test nested paths
        let file3 = ModelFile {
            name: "path/to/deep/model.bin".into(),
            sha256: "ghi789".into(),
            size: 2000,
        };
        assert_eq!(file3.local_name(), "model.bin");
    }

    #[test]
    fn test_check_version_mismatch_found() {
        let tmp = tempfile::tempdir().unwrap();
        let model_dir = tmp.path().join("model");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join(".verified"), "revision=old_version\n").unwrap();

        let manifest = ModelManifest::minilm_v2();
        let result = check_version_mismatch(&model_dir, &manifest);
        assert!(matches!(result, Some(ModelState::UpdateAvailable { .. })));
    }

    #[test]
    fn test_atomic_install_preserves_preexisting_legacy_backup_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let target_dir = tmp.path().join("model");
        copy_model_fixtures(&target_dir).unwrap();
        fs::write(target_dir.join(".verified"), "revision=old\n").unwrap();

        let legacy_backup_dir = tmp.path().join("model.bak");
        fs::create_dir_all(&legacy_backup_dir).unwrap();
        fs::write(legacy_backup_dir.join("sentinel.txt"), "keep me").unwrap();

        let downloader = ModelDownloader::new(target_dir.clone());
        copy_model_fixtures(&downloader.temp_dir).unwrap();
        fs::write(downloader.temp_dir.join(".verified"), "revision=new\n").unwrap();

        downloader.atomic_install().unwrap();

        assert_eq!(
            fs::read_to_string(legacy_backup_dir.join("sentinel.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(
            fs::read_to_string(target_dir.join(".verified")).unwrap(),
            "revision=new\n"
        );
    }

    #[test]
    fn test_write_verified_marker_overwrites_existing_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let target_dir = tmp.path().join("model");
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(target_dir.join(".verified"), "revision=old\n").unwrap();

        let downloader = ModelDownloader::new(target_dir.clone());
        let manifest = ModelManifest::minilm_v2();
        downloader.write_verified_marker(&manifest, None).unwrap();

        let marker = fs::read_to_string(target_dir.join(".verified")).unwrap();
        assert!(marker.contains(&format!("revision={}", manifest.revision)));
        assert!(marker.contains("verified_at="));
        assert!(marker.contains("source=registry"));
    }

    #[test]
    fn test_download_error_display() {
        let display_cases = [
            (
                DownloadError::NetworkError("connection refused".into()),
                "network error: connection refused",
            ),
            (
                DownloadError::VerificationFailed {
                    file: "test.onnx".into(),
                    expected: "abc".into(),
                    actual: "def".into(),
                },
                "verification failed for test.onnx: expected abc, got def",
            ),
            (DownloadError::Cancelled, "download cancelled"),
            (DownloadError::Timeout, "download timed out"),
            (
                DownloadError::HttpError {
                    status: 503,
                    message: "service unavailable".into(),
                },
                "HTTP error 503: service unavailable",
            ),
            (
                DownloadError::ManifestNotVerified {
                    model_id: "test-model".into(),
                    unverified_files: vec!["model.onnx".into(), "config.json".into()],
                    revision_unpinned: true,
                },
                "model 'test-model' is not production-ready: 2 file(s) have placeholder checksums and revision is not pinned",
            ),
            (
                DownloadError::ManifestNotVerified {
                    model_id: "test-model".into(),
                    unverified_files: vec!["model.onnx".into()],
                    revision_unpinned: false,
                },
                "model 'test-model' is not production-ready: 1 file(s) have placeholder checksums",
            ),
            (
                DownloadError::InvalidMirrorUrl {
                    url: "ftp://mirror.example/model.onnx".into(),
                    reason: "unsupported scheme".into(),
                },
                "invalid mirror URL 'ftp://mirror.example/model.onnx': unsupported scheme",
            ),
        ];

        for (err, expected) in display_cases {
            assert_eq!(err.to_string(), expected);
        }

        let err: DownloadError = std::io::Error::other("disk full").into();

        assert_eq!(err.to_string(), "I/O error: disk full");
        let source = err.source().expect("I/O errors expose their source");
        assert_eq!(source.to_string(), "disk full");

        assert!(
            DownloadError::NetworkError("connection refused".into())
                .source()
                .is_none(),
            "non-source variants must not gain an error source"
        );
    }

    #[test]
    fn test_manifest_production_ready_minilm() {
        // MiniLM should be production-ready (verified checksums + pinned revision)
        let manifest = ModelManifest::minilm_v2();
        assert!(manifest.has_verified_checksums());
        assert!(manifest.has_pinned_revision());
        assert!(manifest.is_production_ready());
    }

    #[test]
    fn test_all_bakeoff_candidates_production_ready() {
        // All bake-off candidates should be production-ready (verified checksums)
        let candidates = ModelManifest::bakeoff_candidates();

        // Should have 3 verified models: snowflake, nomic, jina-turbo
        assert_eq!(candidates.len(), 3, "Expected 3 bake-off candidates");

        // All should be production-ready
        for manifest in &candidates {
            assert!(
                manifest.is_production_ready(),
                "Model {} should be production-ready",
                manifest.id
            );
            assert!(
                manifest.has_verified_checksums(),
                "Model {} should have verified checksums",
                manifest.id
            );
            assert!(
                manifest.has_pinned_revision(),
                "Model {} should have pinned revision",
                manifest.id
            );
        }

        // Verify specific models are present
        assert!(
            candidates
                .iter()
                .any(|m| m.id == "snowflake-arctic-embed-s"),
            "Snowflake should be in candidates"
        );
        assert!(
            candidates.iter().any(|m| m.id == "nomic-embed-text-v1.5"),
            "Nomic should be in candidates"
        );
        assert!(
            candidates
                .iter()
                .any(|m| m.id == "jina-reranker-v1-turbo-en"),
            "Jina Turbo should be in candidates"
        );
    }

    #[test]
    fn test_downloader_cancellation() {
        let tmp = tempfile::tempdir().unwrap();
        let downloader = ModelDownloader::new(tmp.path().join("model"));

        assert!(!downloader.is_cancelled());
        downloader.cancel();
        assert!(downloader.is_cancelled());
    }

    #[test]
    fn test_prepare_temp_dir_prunes_stale_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let downloader = ModelDownloader::new(tmp.path().join("model"));
        fs::create_dir_all(&downloader.temp_dir).unwrap();
        fs::write(downloader.temp_dir.join("model.onnx"), b"partial").unwrap();
        fs::write(downloader.temp_dir.join("stale.bin"), b"stale").unwrap();
        fs::create_dir_all(downloader.temp_dir.join("nested")).unwrap();
        fs::write(
            downloader.temp_dir.join("nested").join("should-remove.txt"),
            b"stale",
        )
        .unwrap();

        downloader
            .prepare_temp_dir(&ModelManifest::minilm_v2())
            .unwrap();

        assert!(downloader.temp_dir.join("model.onnx").exists());
        assert!(!downloader.temp_dir.join("stale.bin").exists());
        assert!(!downloader.temp_dir.join("nested").exists());
    }

    #[test]
    #[cfg(unix)]
    fn test_prepare_temp_dir_removes_symlink_entries() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let downloader = ModelDownloader::new(tmp.path().join("model"));
        fs::create_dir_all(&downloader.temp_dir).unwrap();
        let outside = tmp.path().join("outside.bin");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, downloader.temp_dir.join("model.onnx")).unwrap();

        downloader
            .prepare_temp_dir(&ModelManifest::minilm_v2())
            .unwrap();

        let metadata = fs::symlink_metadata(downloader.temp_dir.join("model.onnx"));
        assert!(metadata.is_err(), "symlink should be removed before resume");
        assert!(
            outside.exists(),
            "cleanup must not touch the symlink target"
        );
    }

    #[test]
    fn test_retryable_error_classification() {
        let cases = [
            (DownloadError::NetworkError("boom".into()), true),
            (DownloadError::Timeout, true),
            (
                DownloadError::HttpError {
                    status: 503,
                    message: "unavailable".into(),
                },
                true,
            ),
            (
                DownloadError::HttpError {
                    status: 404,
                    message: "missing".into(),
                },
                false,
            ),
            (DownloadError::Cancelled, false),
            (
                DownloadError::VerificationFailed {
                    file: "model.onnx".into(),
                    expected: "a".into(),
                    actual: "b".into(),
                },
                false,
            ),
        ];

        for (err, expected) in cases {
            assert_eq!(
                err.is_retryable(),
                expected,
                "retryability mismatch for {err}"
            );
        }
    }

    #[test]
    fn test_cleanup_temp_for_error_preserves_partial_downloads_on_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let downloader = ModelDownloader::new(tmp.path().join("model"));
        fs::create_dir_all(&downloader.temp_dir).unwrap();
        let partial = downloader.temp_dir.join("model.onnx");
        fs::write(&partial, b"partial").unwrap();

        downloader.cleanup_temp_for_error(&DownloadError::Cancelled);

        assert!(
            partial.exists(),
            "cancelled downloads should keep partial files for a resumable retry"
        );
    }

    #[test]
    fn test_fail_if_cancelled_preserves_partial_downloads() {
        let tmp = tempfile::tempdir().unwrap();
        let downloader = ModelDownloader::new(tmp.path().join("model"));
        fs::create_dir_all(&downloader.temp_dir).unwrap();
        let partial = downloader.temp_dir.join("model.onnx");
        fs::write(&partial, b"partial").unwrap();
        downloader.cancel();

        let result = downloader.fail_if_cancelled();

        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert!(
            partial.exists(),
            "early cancellation checks should not discard resumable partial files"
        );
    }

    #[test]
    fn test_cleanup_temp_for_error_discards_temp_after_verification_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let downloader = ModelDownloader::new(tmp.path().join("model"));
        fs::create_dir_all(&downloader.temp_dir).unwrap();
        let partial = downloader.temp_dir.join("model.onnx");
        fs::write(&partial, b"partial").unwrap();

        downloader.cleanup_temp_for_error(&DownloadError::VerificationFailed {
            file: "model.onnx".into(),
            expected: "good".into(),
            actual: "bad".into(),
        });

        assert!(
            !downloader.temp_dir.exists(),
            "verification failures should discard the temp directory to avoid reusing corrupt data"
        );
    }

    #[test]
    fn test_download_with_mirror_installs_verified_model_from_http_mirror() {
        let files = [
            ("onnx/model.onnx", b"mirror-model".as_slice()),
            ("tokenizer.json", br#"{"tokenizer":"ok"}"#.as_slice()),
        ];
        let manifest = build_test_manifest("mirror/test-model", "rev123", &files);
        let route_prefix = "/cache";
        let routes: Vec<(String, MirrorRoute)> = manifest
            .files
            .iter()
            .zip(files.iter())
            .map(|(file, (_, body))| {
                (
                    mirror_route_path(route_prefix, &manifest, file),
                    MirrorRoute {
                        body: body.to_vec(),
                        content_type: "application/octet-stream",
                        chunk_size: 64,
                        chunk_delay: Duration::ZERO,
                    },
                )
            })
            .collect();
        let server = start_mirror_fixture_server(routes);
        let tmp = tempfile::tempdir().unwrap();
        let downloader = ModelDownloader::new(tmp.path().join("model"));
        let mirror_base = format!("{}/cache/", server.base_url);

        downloader
            .download_with_mirror(&manifest, Some(&mirror_base), None)
            .unwrap();

        for (name, body) in files {
            let installed = downloader.target_dir.join(
                Path::new(name)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .as_ref(),
            );
            assert_eq!(
                fs::read(installed).unwrap(),
                body,
                "mirror install should persist the downloaded payload"
            );
        }
        let marker = fs::read_to_string(downloader.target_dir.join(".verified")).unwrap();
        assert!(
            marker.contains("revision=rev123"),
            "verified marker should preserve manifest identity after mirror install"
        );
        assert!(
            marker.contains("source=mirror:"),
            "verified marker should record mirror source"
        );

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            manifest.files.len(),
            "expected one request per manifest file"
        );
        assert!(
            requests
                .iter()
                .all(|request| request.path.starts_with("/cache/")),
            "mirror requests should stay under the configured mirror prefix: {requests:?}"
        );
    }

    #[test]
    fn test_download_with_mirror_reports_missing_artifact_from_http_mirror() {
        let file_body = b"mirror-model".as_slice();
        let manifest = build_test_manifest(
            "mirror/test-model",
            "rev404",
            &[("onnx/model.onnx", file_body)],
        );
        let server = start_mirror_fixture_server(Vec::new());
        let tmp = tempfile::tempdir().unwrap();
        let downloader = ModelDownloader::new(tmp.path().join("model"));
        let mirror_base = format!("{}/cache", server.base_url);

        let err = downloader
            .download_with_mirror(&manifest, Some(&mirror_base), None)
            .unwrap_err();

        assert!(
            matches!(err, DownloadError::HttpError { status: 404, .. }),
            "missing mirror artifacts should surface as HTTP 404, got: {err}"
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].path.contains("/resolve/"),
            "mirror request should target the resolved artifact path: {requests:?}"
        );
    }

    #[test]
    fn test_download_with_mirror_discards_corrupt_payload_from_http_mirror() {
        let manifest = build_test_manifest(
            "mirror/test-model",
            "revbad",
            &[("onnx/model.onnx", b"expected-bytes".as_slice())],
        );
        let route_prefix = "/cache";
        let server = start_mirror_fixture_server(vec![(
            mirror_route_path(route_prefix, &manifest, &manifest.files[0]),
            MirrorRoute {
                body: b"corrupt-bytes".to_vec(),
                content_type: "application/octet-stream",
                chunk_size: 64,
                chunk_delay: Duration::ZERO,
            },
        )]);
        let tmp = tempfile::tempdir().unwrap();
        let downloader = ModelDownloader::new(tmp.path().join("model"));
        let mirror_base = format!("{server_base}/cache", server_base = server.base_url);

        let err = downloader
            .download_with_mirror(&manifest, Some(&mirror_base), None)
            .unwrap_err();

        assert!(
            matches!(err, DownloadError::VerificationFailed { .. }),
            "corrupt mirror payloads must fail checksum verification, got: {err}"
        );
        assert!(
            !downloader.temp_dir.exists(),
            "verification failures should discard the temp directory so corrupt payloads are not reused"
        );
        assert!(
            !downloader.target_dir.exists(),
            "corrupt mirror payloads must not be promoted into the installed model directory"
        );
    }

    #[test]
    fn test_download_with_mirror_resumes_after_cancelled_partial_download() {
        let large_payload = vec![b'x'; 128 * 1024];
        let manifest = build_test_manifest(
            "mirror/test-model",
            "revresume",
            &[("onnx/model.onnx", &large_payload)],
        );
        let route_prefix = "/cache";
        let server = start_mirror_fixture_server(vec![(
            mirror_route_path(route_prefix, &manifest, &manifest.files[0]),
            MirrorRoute {
                body: large_payload.clone(),
                content_type: "application/octet-stream",
                chunk_size: 1024,
                chunk_delay: Duration::from_millis(2),
            },
        )]);
        let tmp = tempfile::tempdir().unwrap();
        let downloader = Arc::new(ModelDownloader::new(tmp.path().join("model")));
        let mirror_base = format!("{server_base}/cache", server_base = server.base_url);
        let cancel_once = Arc::new(AtomicBool::new(false));
        let canceller = Arc::clone(&downloader);
        let cancel_flag = Arc::clone(&cancel_once);

        let cancelled = downloader.download_with_mirror(
            &manifest,
            Some(&mirror_base),
            Some(Arc::new(move |progress| {
                if progress.total_bytes >= 16 * 1024
                    && cancel_flag
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    canceller.cancel();
                }
            })),
        );

        assert!(
            matches!(cancelled, Err(DownloadError::Cancelled)),
            "first mirror attempt should stop with a cancellation so we can verify resumable recovery"
        );
        let partial_path = downloader.temp_dir.join("model.onnx");
        let partial_size = fs::metadata(&partial_path).unwrap().len();
        assert!(
            partial_size > 0 && partial_size < large_payload.len() as u64,
            "cancelled run should preserve a partial download for resume; got {partial_size} bytes"
        );

        downloader
            .download_with_mirror(&manifest, Some(&mirror_base), None)
            .unwrap();

        assert_eq!(
            fs::read(downloader.target_dir.join("model.onnx")).unwrap(),
            large_payload,
            "rerun after cancellation should finish the mirrored download and install the exact payload"
        );
        let requests = server.requests();
        assert!(
            requests
                .iter()
                .any(|request| request.range_start == Some(partial_size)),
            "rerun should resume from the preserved partial via Range requests; saw requests: {requests:?}"
        );
    }
}
